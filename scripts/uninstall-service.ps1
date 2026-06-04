#Requires -Version 5.1
[CmdletBinding()]
param(
    [string]$ServiceName = "dbgflow-mcp",
    [string]$InstallRoot = (Join-Path $env:LOCALAPPDATA "dbgflow"),
    [switch]$RemoveInstallFiles
)

$ErrorActionPreference = "Stop"

function Test-Administrator {
    $identity = [Security.Principal.WindowsIdentity]::GetCurrent()
    $principal = [Security.Principal.WindowsPrincipal]::new($identity)
    $principal.IsInRole([Security.Principal.WindowsBuiltInRole]::Administrator)
}

function ConvertTo-PowerShellStringLiteral {
    param([string]$Value)

    "'" + $Value.Replace("'", "''") + "'"
}

function Invoke-SelfElevated {
    param([string]$Command)

    $powershell = Join-Path $PSHOME "powershell.exe"
    if (-not (Test-Path $powershell)) {
        $powershell = "powershell.exe"
    }

    $encodedCommand = [Convert]::ToBase64String([Text.Encoding]::Unicode.GetBytes($Command))
    Write-Host "Administrator privileges are required. Requesting elevation with UAC..."

    try {
        $process = Start-Process `
            -FilePath $powershell `
            -ArgumentList @("-NoProfile", "-ExecutionPolicy", "Bypass", "-EncodedCommand", $encodedCommand) `
            -Verb RunAs `
            -Wait `
            -PassThru
    }
    catch {
        throw "Elevation was cancelled or failed: $($_.Exception.Message)"
    }

    exit $process.ExitCode
}

function Assert-Administrator {
    if (Test-Administrator) {
        return
    }

    $scriptPath = ConvertTo-PowerShellStringLiteral -Value $PSCommandPath
    $serviceNameArgument = ConvertTo-PowerShellStringLiteral -Value $ServiceName
    $installRootArgument = ConvertTo-PowerShellStringLiteral -Value $InstallRoot
    $removeInstallFilesArgument = if ($RemoveInstallFiles) { " -RemoveInstallFiles" } else { "" }
    $command = @(
        '$ErrorActionPreference = "Stop"'
        'try {'
        "    & $scriptPath -ServiceName $serviceNameArgument -InstallRoot $installRootArgument$removeInstallFilesArgument"
        '    exit 0'
        '}'
        'catch {'
        '    Write-Error $_'
        '    exit 1'
        '}'
    ) -join [Environment]::NewLine

    Invoke-SelfElevated -Command $command
}

function Wait-ServiceDeleted {
    param([string]$Name)

    for ($i = 0; $i -lt 60; $i++) {
        $service = Get-Service -Name $Name -ErrorAction SilentlyContinue
        if ($null -eq $service) {
            return
        }
        Start-Sleep -Seconds 1
    }

    throw "Timed out waiting for service '$Name' to be deleted."
}

function Stop-And-DeleteService {
    param([string]$Name)

    $service = Get-Service -Name $Name -ErrorAction SilentlyContinue
    if ($null -eq $service) {
        Write-Host "Service '$Name' is not installed."
        return
    }

    if ($service.Status -ne "Stopped") {
        Write-Host "Stopping service '$Name'..."
        Stop-Service -Name $Name -Force
        $service.WaitForStatus("Stopped", [TimeSpan]::FromSeconds(30))
    }

    Write-Host "Deleting service '$Name'..."
    $delete = & sc.exe delete $Name
    if ($LASTEXITCODE -ne 0) {
        throw "sc.exe delete failed: $delete"
    }

    Wait-ServiceDeleted -Name $Name
}

function Remove-BinDirectory {
    param([string]$Root)

    $resolvedRoot = [IO.Path]::GetFullPath($Root)
    $binDir = [IO.Path]::GetFullPath((Join-Path $resolvedRoot "bin"))
    if (-not $binDir.StartsWith($resolvedRoot, [StringComparison]::OrdinalIgnoreCase)) {
        throw "Refusing to remove path outside install root: $binDir"
    }

    if (Test-Path $binDir) {
        Write-Host "Removing installed binaries at '$binDir'..."
        Remove-Item -LiteralPath $binDir -Recurse -Force
    }
}

Assert-Administrator
Stop-And-DeleteService -Name $ServiceName

if ($RemoveInstallFiles) {
    Remove-BinDirectory -Root $InstallRoot
}

Write-Host "Service '$ServiceName' has been uninstalled. Artifacts and logs were left under '$(Join-Path $InstallRoot "var")'."

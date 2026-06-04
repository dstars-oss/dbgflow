#Requires -Version 5.1
[CmdletBinding()]
param(
    [string]$ServiceName = "dbgflow-mcp",
    [string]$InstallRoot = (Join-Path $env:LOCALAPPDATA "dbgflow"),
    [switch]$RemoveInstallFiles
)

$ErrorActionPreference = "Stop"

function Assert-Administrator {
    $identity = [Security.Principal.WindowsIdentity]::GetCurrent()
    $principal = [Security.Principal.WindowsPrincipal]::new($identity)
    if (-not $principal.IsInRole([Security.Principal.WindowsBuiltInRole]::Administrator)) {
        throw "This script must be run from an elevated PowerShell session."
    }
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

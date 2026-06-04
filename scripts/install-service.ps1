#Requires -Version 5.1
[CmdletBinding()]
param(
    [string]$ServiceName = "dbgflow-mcp",
    [string]$DisplayName = "dbgflow MCP Server",
    [string]$Bind = "127.0.0.1:7331",
    [string]$InstallRoot = (Join-Path $env:LOCALAPPDATA "dbgflow")
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
        return
    }

    if ($service.Status -ne "Stopped") {
        Write-Host "Stopping existing service '$Name'..."
        Stop-Service -Name $Name -Force
        $service.WaitForStatus("Stopped", [TimeSpan]::FromSeconds(30))
    }

    Write-Host "Deleting existing service '$Name'..."
    $delete = & sc.exe delete $Name
    if ($LASTEXITCODE -ne 0) {
        throw "sc.exe delete failed: $delete"
    }

    Wait-ServiceDeleted -Name $Name
}

function Assert-PortAvailable {
    param([string]$BindAddress)

    $endpoint = Parse-BindAddress -BindAddress $BindAddress
    if (-not [Net.IPAddress]::IsLoopback($endpoint.Address)) {
        throw "Bind address must be loopback because HTTP transport has no authentication: $BindAddress"
    }

    $listener = $null
    try {
        $listener = [Net.Sockets.TcpListener]::new($endpoint.Address, $endpoint.Port)
        $listener.Start()
    }
    catch {
        throw "Bind address $BindAddress is not available: $($_.Exception.Message)"
    }
    finally {
        if ($null -ne $listener) {
            $listener.Stop()
        }
    }
}

function Parse-BindAddress {
    param([string]$BindAddress)

    if ($BindAddress.StartsWith("[")) {
        $end = $BindAddress.IndexOf("]")
        if ($end -lt 0 -or $BindAddress.Length -le ($end + 2) -or $BindAddress[$end + 1] -ne ":") {
            throw "Bind address must use '<ip>:<port>' or '[ipv6]:<port>' format. Current value: $BindAddress"
        }
        $hostPart = $BindAddress.Substring(1, $end - 1)
        $portPart = $BindAddress.Substring($end + 2)
    }
    else {
        $lastColon = $BindAddress.LastIndexOf(":")
        if ($lastColon -le 0 -or $lastColon -eq ($BindAddress.Length - 1)) {
            throw "Bind address must use '<ip>:<port>' or '[ipv6]:<port>' format. Current value: $BindAddress"
        }
        $hostPart = $BindAddress.Substring(0, $lastColon)
        $portPart = $BindAddress.Substring($lastColon + 1)
        if ($hostPart.Contains(":")) {
            throw "IPv6 bind addresses must use '[ipv6]:<port>' format. Current value: $BindAddress"
        }
    }

    try {
        $ip = [Net.IPAddress]::Parse($hostPart)
        $port = [int]$portPart
    }
    catch {
        throw "Bind address must use '<ip>:<port>' format. Current value: $BindAddress"
    }

    if ($port -lt 1 -or $port -gt 65535) {
        throw "Bind port must be between 1 and 65535. Current value: $BindAddress"
    }

    [Net.IPEndPoint]::new($ip, $port)
}

function Invoke-CargoBuild {
    param([string]$RepoRoot)

    Write-Host "Building dbgflow-mcp release binary..."
    Push-Location $RepoRoot
    try {
        & cargo build -p dbgflow-mcp --release
        if ($LASTEXITCODE -ne 0) {
            throw "cargo build failed with exit code $LASTEXITCODE."
        }
    }
    finally {
        Pop-Location
    }
}

function Wait-Healthz {
    param([string]$BindAddress)

    $uri = "http://$BindAddress/healthz"
    for ($i = 0; $i -lt 60; $i++) {
        try {
            $response = Invoke-RestMethod -Uri $uri -Method Get -TimeoutSec 2
            if ($response.status -eq "ok") {
                return
            }
        }
        catch {
            Start-Sleep -Seconds 1
        }
    }

    throw "Service did not pass health check at $uri."
}

Assert-Administrator

$ScriptDir = Split-Path -Parent $PSCommandPath
$RepoRoot = Split-Path -Parent $ScriptDir
$BinDir = Join-Path $InstallRoot "bin"
$VarDir = Join-Path $InstallRoot "var"
$LogDir = Join-Path $VarDir "logs"
$ArtifactRoot = Join-Path $VarDir "artifacts"
$SourceExe = Join-Path $RepoRoot "target\release\dbgflow-mcp.exe"
$InstalledExe = Join-Path $BinDir "dbgflow-mcp.exe"

Stop-And-DeleteService -Name $ServiceName
Assert-PortAvailable -BindAddress $Bind
Invoke-CargoBuild -RepoRoot $RepoRoot

if (-not (Test-Path $SourceExe)) {
    throw "Expected release binary was not found: $SourceExe"
}

New-Item -ItemType Directory -Force -Path $BinDir, $LogDir, $ArtifactRoot | Out-Null
Copy-Item -Path $SourceExe -Destination $InstalledExe -Force

& icacls.exe $InstallRoot /grant "SYSTEM:(OI)(CI)F" "Administrators:(OI)(CI)F" /T | Out-Null
if ($LASTEXITCODE -ne 0) {
    throw "icacls failed for $InstallRoot."
}

$binPath = "`"$InstalledExe`" service --bind $Bind --artifact-root `"$ArtifactRoot`" --log-dir `"$LogDir`""

Write-Host "Creating service '$ServiceName'..."
$create = & sc.exe create $ServiceName binPath= $binPath DisplayName= $DisplayName start= auto obj= LocalSystem
if ($LASTEXITCODE -ne 0) {
    throw "sc.exe create failed: $create"
}

& sc.exe description $ServiceName "dbgflow Streamable HTTP MCP server" | Out-Null

Write-Host "Starting service '$ServiceName'..."
Start-Service -Name $ServiceName
Wait-Healthz -BindAddress $Bind

Write-Host "Service '$ServiceName' is running at http://$Bind/mcp"

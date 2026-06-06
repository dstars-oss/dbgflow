#Requires -Version 5.1
[CmdletBinding()]
param(
    [string]$ServiceName = "dbgflow-mcp",
    [string]$DisplayName = "dbgflow MCP Server",
    [string]$Bind = "127.0.0.1:7331",
    [string]$InstallRoot = (Join-Path $env:LOCALAPPDATA "dbgflow")
)

$ErrorActionPreference = "Stop"

$ScriptDir = Split-Path -Parent $PSCommandPath
$RepoRoot = Split-Path -Parent $ScriptDir
$Exe = Join-Path $RepoRoot "target\release\dbgflow-mcp.exe"

$arguments = @(
    "service"
    "install"
    "--service-name"
    $ServiceName
    "--display-name"
    $DisplayName
    "--bind"
    $Bind
    "--install-root"
    $InstallRoot
)

Push-Location $RepoRoot
try {
    & cargo build -p dbgflow-mcp --release
    if ($LASTEXITCODE -ne 0) {
        exit $LASTEXITCODE
    }
    if (-not (Test-Path $Exe)) {
        throw "Expected release binary was not found: $Exe"
    }

    & $Exe @arguments
    exit $LASTEXITCODE
}
finally {
    Pop-Location
}

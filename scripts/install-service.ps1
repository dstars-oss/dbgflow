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

$arguments = @(
    "run"
    "-p"
    "dbgflow-mcp"
    "--"
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
    "--repo-root"
    $RepoRoot
)

Push-Location $RepoRoot
try {
    & cargo @arguments
    exit $LASTEXITCODE
}
finally {
    Pop-Location
}

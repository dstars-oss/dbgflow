#Requires -Version 5.1
[CmdletBinding()]
param(
    [string]$ServiceName = "dbgflow-mcp",
    [string]$InstallRoot = (Join-Path $env:LOCALAPPDATA "dbgflow"),
    [switch]$RemoveInstallFiles
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
    "uninstall"
    "--service-name"
    $ServiceName
    "--install-root"
    $InstallRoot
)

if ($RemoveInstallFiles) {
    $arguments += "--remove-install-files"
}

Push-Location $RepoRoot
try {
    & cargo @arguments
    exit $LASTEXITCODE
}
finally {
    Pop-Location
}

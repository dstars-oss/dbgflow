#Requires -Version 5.1
[CmdletBinding()]
param(
    [string]$ServiceName = "dbgflow-mcp",
    [string]$ConfigPath
)

$ErrorActionPreference = "Stop"

$ScriptDir = Split-Path -Parent $PSCommandPath
$RepoRoot = Split-Path -Parent $ScriptDir
$Exe = Join-Path $RepoRoot "target\release\dbgflow-mcp.exe"

$arguments = @(
    "service"
    "uninstall"
    "--service-name"
    $ServiceName
)

if ($ConfigPath) {
    $arguments += @("--config", ([System.IO.Path]::GetFullPath($ConfigPath)))
}

Push-Location $RepoRoot
try {
    & cargo build -p dbgflow-mcp --release
    if ($LASTEXITCODE -ne 0) {
        exit $LASTEXITCODE
    }
    if (-not (Test-Path -LiteralPath $Exe -PathType Leaf)) {
        throw "Expected release binary was not found: $Exe"
    }

    & $Exe @arguments
    exit $LASTEXITCODE
}
finally {
    Pop-Location
}

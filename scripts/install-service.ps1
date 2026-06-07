#Requires -Version 5.1
[CmdletBinding()]
param(
    [string]$ServiceName,
    [string]$DisplayName,
    [string]$Bind,
    [string]$InstallRoot,
    [string]$ProxyUrl,
    [string]$DbgEngDir,
    [string]$SysinternalsDir,
    [switch]$NoProxy,
    [switch]$NonInteractive
)

$ErrorActionPreference = "Stop"

$ScriptDir = Split-Path -Parent $PSCommandPath
$RepoRoot = Split-Path -Parent $ScriptDir
$Exe = Join-Path $RepoRoot "target\release\dbgflow-mcp.exe"

$arguments = @("service", "install")
if ($PSBoundParameters.ContainsKey("ServiceName")) {
    $arguments += @("--service-name", $ServiceName)
}
if ($PSBoundParameters.ContainsKey("DisplayName")) {
    $arguments += @("--display-name", $DisplayName)
}
if ($PSBoundParameters.ContainsKey("Bind")) {
    $arguments += @("--bind", $Bind)
}
if ($PSBoundParameters.ContainsKey("InstallRoot")) {
    $arguments += @("--install-root", $InstallRoot)
}
if ($PSBoundParameters.ContainsKey("ProxyUrl")) {
    $arguments += @("--proxy-url", $ProxyUrl)
}
if ($PSBoundParameters.ContainsKey("DbgEngDir")) {
    $arguments += @("--dbgeng-dir", $DbgEngDir)
}
if ($PSBoundParameters.ContainsKey("SysinternalsDir")) {
    $arguments += @("--sysinternals-dir", $SysinternalsDir)
}
if ($NoProxy) {
    $arguments += "--no-proxy"
}
if ($NonInteractive) {
    $arguments += "--non-interactive"
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

#Requires -Version 5.1
[CmdletBinding()]
param(
    [string]$ServiceName = "dbgflow-mcp",
    [string]$DisplayName = "dbgflow MCP Server",
    [string]$Bind = "127.0.0.1:7331",
    [string]$InstallRoot = (Join-Path $env:LOCALAPPDATA "dbgflow"),
    [string]$ProxyUrl = "http://127.0.0.1:7897",
    [switch]$NoProxy
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

function Convert-ToSymbolProxy {
    param([Parameter(Mandatory = $true)][string]$Url)
    $uri = [Uri]$Url
    if (($uri.Scheme -ne "http") -and ($uri.Scheme -ne "https")) {
        throw "ProxyUrl must use http:// or https://"
    }
    if (-not $uri.Host -or $uri.Port -le 0) {
        throw "ProxyUrl must include host and port"
    }
    if ($uri.UserInfo) {
        throw "ProxyUrl credentials are not supported for _NT_SYMBOL_PROXY"
    }
    return "$($uri.Host):$($uri.Port)"
}

function Set-ServiceProxyEnvironment {
    param(
        [Parameter(Mandatory = $true)][string]$ServiceName,
        [Parameter(Mandatory = $true)][string]$ProxyUrl
    )
    $symbolProxy = Convert-ToSymbolProxy -Url $ProxyUrl
    $key = "HKLM:\SYSTEM\CurrentControlSet\Services\$ServiceName"
    $environment = @(
        "HTTP_PROXY=$ProxyUrl"
        "HTTPS_PROXY=$ProxyUrl"
        "http_proxy=$ProxyUrl"
        "https_proxy=$ProxyUrl"
        "_NT_SYMBOL_PROXY=$symbolProxy"
    )
    New-ItemProperty -Path $key -Name Environment -PropertyType MultiString -Value $environment -Force | Out-Null
}

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
    $installExitCode = $LASTEXITCODE
    if ($installExitCode -ne 0) {
        exit $installExitCode
    }
    if (-not $NoProxy) {
        if ([string]::IsNullOrWhiteSpace($ProxyUrl)) {
            throw "ProxyUrl must not be empty. Use -NoProxy to skip service proxy configuration."
        }
        Set-ServiceProxyEnvironment -ServiceName $ServiceName -ProxyUrl $ProxyUrl
        Restart-Service -Name $ServiceName -Force
    }
    exit 0
}
finally {
    Pop-Location
}

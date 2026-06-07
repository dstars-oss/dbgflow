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

function Convert-ToPowerShellLiteral {
    param([AllowNull()][string]$Value)
    if ($null -eq $Value) {
        return "''"
    }
    return "'" + ($Value -replace "'", "''") + "'"
}

function Test-IsAdministrator {
    $identity = [Security.Principal.WindowsIdentity]::GetCurrent()
    $principal = New-Object Security.Principal.WindowsPrincipal($identity)
    return $principal.IsInRole([Security.Principal.WindowsBuiltInRole]::Administrator)
}

if (-not (Test-IsAdministrator)) {
    $command = @(
        "&"
        (Convert-ToPowerShellLiteral -Value $PSCommandPath)
        "-ServiceName"
        (Convert-ToPowerShellLiteral -Value $ServiceName)
        "-DisplayName"
        (Convert-ToPowerShellLiteral -Value $DisplayName)
        "-Bind"
        (Convert-ToPowerShellLiteral -Value $Bind)
        "-InstallRoot"
        (Convert-ToPowerShellLiteral -Value $InstallRoot)
        "-ProxyUrl"
        (Convert-ToPowerShellLiteral -Value $ProxyUrl)
    )
    if ($NoProxy) {
        $command += "-NoProxy"
    }
    $encodedCommand = [Convert]::ToBase64String([Text.Encoding]::Unicode.GetBytes(($command -join " ")))
    $process = Start-Process -FilePath "powershell.exe" -Verb RunAs -Wait -PassThru -ArgumentList @(
        "-NoProfile"
        "-ExecutionPolicy"
        "Bypass"
        "-EncodedCommand"
        $encodedCommand
    )
    exit $process.ExitCode
}

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

function Assert-ServiceName {
    param([Parameter(Mandatory = $true)][string]$ServiceName)
    if ([string]::IsNullOrWhiteSpace($ServiceName)) {
        throw "ServiceName must not be empty"
    }
    if ($ServiceName -match "[\\/]") {
        throw "ServiceName must not contain registry path separators"
    }
}

function Convert-ToSymbolProxy {
    param([Parameter(Mandatory = $true)][string]$Url)
    if ([string]::IsNullOrWhiteSpace($Url)) {
        throw "ProxyUrl must not be empty. Use -NoProxy to skip service proxy configuration."
    }
    if ($Url -match "\s") {
        throw "ProxyUrl must not contain whitespace"
    }
    if ($Url -cnotmatch "^(?<scheme>https?)://(?<authority>[^/?#]+)$") {
        throw "ProxyUrl must use http:// or https://"
    }

    $authority = $Matches["authority"]
    if ($authority -match "@") {
        throw "ProxyUrl credentials are not supported for _NT_SYMBOL_PROXY"
    }

    $host = $null
    $portText = $null
    $symbolHost = $null
    if ($authority -match "^\[(?<host>[^\]]+)\]:(?<port>[0-9]+)$") {
        $host = $Matches["host"]
        $portText = $Matches["port"]
        $symbolHost = "[$host]"
    }
    elseif ($authority -match "^(?<host>[^:]+):(?<port>[0-9]+)$") {
        $host = $Matches["host"]
        $portText = $Matches["port"]
        $symbolHost = $host
    }
    else {
        throw "ProxyUrl must include host and numeric port"
    }

    if ([string]::IsNullOrWhiteSpace($host)) {
        throw "ProxyUrl must include host and port"
    }

    $port = 0
    if (-not [int]::TryParse($portText, [ref]$port) -or $port -le 0 -or $port -gt 65535) {
        throw "ProxyUrl port must be between 1 and 65535"
    }

    return "${symbolHost}:$port"
}

function Wait-ServiceHealth {
    param(
        [Parameter(Mandatory = $true)][string]$Bind,
        [int]$TimeoutSeconds = 60
    )
    $uri = "http://$Bind/healthz"
    $deadline = (Get-Date).AddSeconds($TimeoutSeconds)
    do {
        try {
            $response = Invoke-WebRequest -Uri $uri -UseBasicParsing -TimeoutSec 5
            if ($response.Content -match '"status"\s*:\s*"ok"') {
                return
            }
        }
        catch {
        }
        Start-Sleep -Milliseconds 500
    } while ((Get-Date) -lt $deadline)

    throw "Service health check did not report status ok at $uri within $TimeoutSeconds seconds"
}

function Set-ServiceProxyEnvironment {
    param(
        [Parameter(Mandatory = $true)][string]$ServiceName,
        [Parameter(Mandatory = $true)][string]$ProxyUrl
    )
    Assert-ServiceName -ServiceName $ServiceName
    $symbolProxy = Convert-ToSymbolProxy -Url $ProxyUrl
    $key = "HKLM:\SYSTEM\CurrentControlSet\Services\$ServiceName"
    $environment = @(
        "HTTP_PROXY=$ProxyUrl"
        "HTTPS_PROXY=$ProxyUrl"
        "http_proxy=$ProxyUrl"
        "https_proxy=$ProxyUrl"
        "_NT_SYMBOL_PROXY=$symbolProxy"
    )
    New-ItemProperty -LiteralPath $key -Name Environment -PropertyType MultiString -Value $environment -Force | Out-Null
}

Assert-ServiceName -ServiceName $ServiceName

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
        Set-ServiceProxyEnvironment -ServiceName $ServiceName -ProxyUrl $ProxyUrl
        Restart-Service -Name $ServiceName -Force
        Wait-ServiceHealth -Bind $Bind
    }
    exit 0
}
finally {
    Pop-Location
}

#Requires -Version 5.1
[CmdletBinding()]
param(
    [string]$ServiceName = "dbgflow-mcp",
    [string]$DisplayName = "dbgflow MCP Server",
    [string]$Bind = "127.0.0.1:7331",
    [string]$InstallRoot = (Join-Path $env:LOCALAPPDATA "dbgflow"),
    [string]$ProxyUrl = "http://127.0.0.1:7897",
    [string]$SysinternalsDir,
    [switch]$NoProxy
)

$ErrorActionPreference = "Stop"
$proxyUrlWasBound = $PSBoundParameters.ContainsKey("ProxyUrl")
$sysinternalsDirWasBound = $PSBoundParameters.ContainsKey("SysinternalsDir")

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
    )
    if ($proxyUrlWasBound) {
        $command += @(
            "-ProxyUrl"
            (Convert-ToPowerShellLiteral -Value $ProxyUrl)
        )
    }
    if ($sysinternalsDirWasBound) {
        $command += @(
            "-SysinternalsDir"
            (Convert-ToPowerShellLiteral -Value $SysinternalsDir)
        )
    }
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

if ($NoProxy -and $proxyUrlWasBound) {
    throw "-NoProxy and -ProxyUrl cannot be used together."
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

function Test-SysinternalsDir {
    param([AllowNull()][string]$Path)
    if ([string]::IsNullOrWhiteSpace($Path)) {
        return $false
    }
    if (-not (Test-Path -LiteralPath $Path -PathType Container)) {
        return $false
    }
    return (
        (Test-Path -LiteralPath (Join-Path $Path "Procmon64.exe") -PathType Leaf) -or
        (Test-Path -LiteralPath (Join-Path $Path "Procmon.exe") -PathType Leaf)
    )
}

function Find-SysinternalsDir {
    param([Parameter(Mandatory = $true)][string]$RepoRoot)
    $candidates = @(
        (Join-Path (Split-Path -Parent $RepoRoot) "Sysinternals"),
        "C:\Tools\Sysinternals",
        "C:\Sysinternals",
        "C:\Program Files\Sysinternals"
    )
    foreach ($candidate in $candidates) {
        if (Test-SysinternalsDir -Path $candidate) {
            return (Resolve-Path -LiteralPath $candidate).Path
        }
    }
    return $null
}

function Confirm-SysinternalsDir {
    param([Parameter(Mandatory = $true)][string]$Path)
    $answer = Read-Host "Use Sysinternals directory '$Path' for optional Procmon features? [Y/n]"
    return [string]::IsNullOrWhiteSpace($answer) -or $answer -match '^(y|yes)$'
}

function Assert-NoControlCharacters {
    param(
        [Parameter(Mandatory = $true)][string]$Value,
        [Parameter(Mandatory = $true)][string]$Name
    )
    foreach ($ch in $Value.ToCharArray()) {
        if ([char]::IsControl($ch)) {
            throw "$Name must not contain control characters"
        }
    }
}

function Assert-ServiceName {
    param([Parameter(Mandatory = $true)][string]$ServiceName)
    if ([string]::IsNullOrWhiteSpace($ServiceName)) {
        throw "ServiceName must not be empty"
    }
    if (
        $ServiceName.Contains("\") -or
        $ServiceName.Contains("/") -or
        $ServiceName.Contains("*") -or
        $ServiceName.Contains("?") -or
        $ServiceName.Contains("[") -or
        $ServiceName.Contains("]")
    ) {
        throw "ServiceName must not contain path separators, wildcards, or control characters"
    }
    Assert-NoControlCharacters -Value $ServiceName -Name "ServiceName"
}

function Convert-ToSymbolProxy {
    param([Parameter(Mandatory = $true)][string]$Url)
    if ([string]::IsNullOrWhiteSpace($Url)) {
        throw "ProxyUrl must not be empty. Use -NoProxy to skip service proxy configuration."
    }
    Assert-NoControlCharacters -Value $Url -Name "ProxyUrl"
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

    $proxyHost = $null
    $portText = $null
    $symbolHost = $null
    if ($authority -match "^\[(?<host>[^\]]+)\]:(?<port>[0-9]+)$") {
        $proxyHost = $Matches["host"]
        $portText = $Matches["port"]
        $symbolHost = "[$proxyHost]"
    }
    elseif ($authority -match "^(?<host>[^:]+):(?<port>[0-9]+)$") {
        $proxyHost = $Matches["host"]
        $portText = $Matches["port"]
        $symbolHost = $proxyHost
    }
    else {
        throw "ProxyUrl must include host and numeric port"
    }

    if ([string]::IsNullOrWhiteSpace($proxyHost)) {
        throw "ProxyUrl must include host and port"
    }

    $port = 0
    if (-not [int]::TryParse($portText, [ref]$port) -or $port -le 0 -or $port -gt 65535) {
        throw "ProxyUrl port must be between 1 and 65535"
    }

    return "${symbolHost}:$port"
}

function Get-ExactService {
    param([Parameter(Mandatory = $true)][string]$ServiceName)
    Assert-ServiceName -ServiceName $ServiceName
    $services = @(Get-Service -Name $ServiceName -ErrorAction Stop)
    if (($services.Count -ne 1) -or ($services[0].Name -ne $ServiceName)) {
        throw "Service lookup for '$ServiceName' did not return an exact service match"
    }
    return $services[0]
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

function New-ServiceProxyEnvironment {
    param(
        [Parameter(Mandatory = $true)][string]$ProxyUrl,
        [Parameter(Mandatory = $true)][string]$SymbolProxy
    )
    return @(
        "HTTP_PROXY=$ProxyUrl"
        "HTTPS_PROXY=$ProxyUrl"
        "http_proxy=$ProxyUrl"
        "https_proxy=$ProxyUrl"
        "_NT_SYMBOL_PROXY=$SymbolProxy"
        "ALL_PROXY="
        "NO_PROXY="
        "all_proxy="
        "no_proxy="
    )
}

function New-ServiceNoProxyEnvironment {
    return @(
        "_NT_SYMBOL_PROXY="
        "HTTP_PROXY="
        "HTTPS_PROXY="
        "ALL_PROXY="
        "NO_PROXY="
        "http_proxy="
        "https_proxy="
        "all_proxy="
        "no_proxy="
    )
}

function Set-ServiceEnvironment {
    param(
        [Parameter(Mandatory = $true)][string]$ServiceName,
        [Parameter(Mandatory = $true)][string[]]$Environment
    )
    Assert-ServiceName -ServiceName $ServiceName
    $key = "HKLM:\SYSTEM\CurrentControlSet\Services\$ServiceName"
    New-ItemProperty -LiteralPath $key -Name Environment -PropertyType MultiString -Value $Environment -Force | Out-Null
}

Assert-ServiceName -ServiceName $ServiceName
$resolvedSysinternalsDir = $null
if ($sysinternalsDirWasBound) {
    if (-not (Test-SysinternalsDir -Path $SysinternalsDir)) {
        throw "SysinternalsDir must contain Procmon64.exe or Procmon.exe: $SysinternalsDir"
    }
    $resolvedSysinternalsDir = (Resolve-Path -LiteralPath $SysinternalsDir).Path
}
else {
    $candidateSysinternalsDir = Find-SysinternalsDir -RepoRoot $RepoRoot
    if ($candidateSysinternalsDir -and (Confirm-SysinternalsDir -Path $candidateSysinternalsDir)) {
        $resolvedSysinternalsDir = $candidateSysinternalsDir
    }
}
if ($resolvedSysinternalsDir) {
    $arguments += @("--sysinternals-dir", $resolvedSysinternalsDir)
}

$serviceEnvironment = $null
if ($NoProxy) {
    $serviceEnvironment = New-ServiceNoProxyEnvironment
}
else {
    $symbolProxy = Convert-ToSymbolProxy -Url $ProxyUrl
    $serviceEnvironment = New-ServiceProxyEnvironment -ProxyUrl $ProxyUrl -SymbolProxy $symbolProxy
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
    Set-ServiceEnvironment -ServiceName $ServiceName -Environment $serviceEnvironment
    $service = Get-ExactService -ServiceName $ServiceName
    Restart-Service -InputObject $service -Force
    Wait-ServiceHealth -Bind $Bind
    exit 0
}
finally {
    Pop-Location
}

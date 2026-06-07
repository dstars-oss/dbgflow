#Requires -Version 5.1
[CmdletBinding()]
param(
    [string]$ServiceName = "dbgflow-mcp",
    [string]$DisplayName = "dbgflow MCP Server",
    [string]$Bind = "127.0.0.1:7331",
    [string]$InstallRoot = (Join-Path $env:LOCALAPPDATA "dbgflow"),
    [string]$ConfigPath,
    [string]$ProxyUrl,
    [string]$DbgEngDir,
    [string]$SymbolPath,
    [string]$SysinternalsDir,
    [switch]$NoProxy,
    [switch]$NonInteractive
)

$ErrorActionPreference = "Stop"
$SymbolPathWasProvided = $PSBoundParameters.ContainsKey("SymbolPath")

$ScriptDir = Split-Path -Parent $PSCommandPath
$RepoRoot = Split-Path -Parent $ScriptDir
$Exe = Join-Path $RepoRoot "target\release\dbgflow-mcp.exe"
$KnownProxyKeys = @(
    "_NT_SYMBOL_PROXY",
    "HTTP_PROXY",
    "HTTPS_PROXY",
    "ALL_PROXY",
    "NO_PROXY",
    "http_proxy",
    "https_proxy",
    "all_proxy",
    "no_proxy"
)

function ConvertTo-TomlString {
    param([AllowEmptyString()][string]$Value)

    $escaped = $Value.Replace("\", "\\").Replace('"', '\"').Replace("`r", "\r").Replace("`n", "\n").Replace("`t", "\t")
    return '"' + $escaped + '"'
}

function Get-FullPath {
    param([Parameter(Mandatory = $true)][string]$Path)

    return [System.IO.Path]::GetFullPath($Path)
}

function Test-PathUnderRoot {
    param(
        [Parameter(Mandatory = $true)][string]$Path,
        [Parameter(Mandatory = $true)][string]$Root
    )

    $fullPath = Get-FullPath -Path $Path
    $fullRoot = (Get-FullPath -Path $Root).TrimEnd('\', '/')
    return $fullPath.Equals($fullRoot, [System.StringComparison]::OrdinalIgnoreCase) -or
        $fullPath.StartsWith($fullRoot + [System.IO.Path]::DirectorySeparatorChar, [System.StringComparison]::OrdinalIgnoreCase) -or
        $fullPath.StartsWith($fullRoot + [System.IO.Path]::AltDirectorySeparatorChar, [System.StringComparison]::OrdinalIgnoreCase)
}

function Assert-SafeInstallRoot {
    param([Parameter(Mandatory = $true)][string]$Path)

    $fullPath = (Get-FullPath -Path $Path).TrimEnd('\', '/')
    $leaf = Split-Path -Leaf $fullPath
    if (-not $leaf.Equals("dbgflow", [System.StringComparison]::OrdinalIgnoreCase)) {
        throw "Install root must be a dedicated 'dbgflow' directory: $fullPath"
    }

    $dangerousRoots = New-Object System.Collections.Generic.List[string]
    foreach ($value in @(
        $env:USERPROFILE,
        $env:LOCALAPPDATA,
        $env:APPDATA,
        $env:ProgramData,
        $env:ProgramFiles,
        [System.Environment]::GetEnvironmentVariable("ProgramFiles(x86)"),
        "C:\Users",
        "C:\ProgramData"
    )) {
        if ($value) {
            $dangerousRoots.Add((Get-FullPath -Path $value).TrimEnd('\', '/'))
        }
    }

    foreach ($root in $dangerousRoots) {
        if ($fullPath.Equals($root, [System.StringComparison]::OrdinalIgnoreCase)) {
            throw "Install root must not be a high-level directory: $fullPath"
        }
    }
}

function Test-DbgEngDirectory {
    param([Parameter(Mandatory = $true)][string]$Path)

    return (Test-Path -LiteralPath (Join-Path $Path "dbgeng.dll") -PathType Leaf)
}

function Test-SysinternalsDirectory {
    param([Parameter(Mandatory = $true)][string]$Path)

    return (Test-Path -LiteralPath (Join-Path $Path "Procmon64.exe") -PathType Leaf) -or
        (Test-Path -LiteralPath (Join-Path $Path "Procmon.exe") -PathType Leaf)
}

function Get-ExeArchitectureInfo {
    param([Parameter(Mandatory = $true)][string]$Path)

    $stream = [System.IO.File]::Open($Path, [System.IO.FileMode]::Open, [System.IO.FileAccess]::Read, [System.IO.FileShare]::ReadWrite)
    try {
        $reader = New-Object System.IO.BinaryReader($stream)
        $stream.Seek(0x3c, [System.IO.SeekOrigin]::Begin) | Out-Null
        $peHeaderOffset = $reader.ReadInt32()
        $stream.Seek($peHeaderOffset, [System.IO.SeekOrigin]::Begin) | Out-Null
        $signature = $reader.ReadUInt32()
        if ($signature -ne 0x00004550) {
            throw "Expected PE signature was not found in $Path"
        }
        $machine = $reader.ReadUInt16()
        switch ($machine) {
            0x014c { return @{ StoreArch = "x86"; SdkArch = "x86" } }
            0x8664 { return @{ StoreArch = "amd64"; SdkArch = "x64" } }
            0xaa64 { return @{ StoreArch = "arm64"; SdkArch = "arm64" } }
            default { throw ("Unsupported PE machine type 0x{0:x4} in {1}" -f $machine, $Path) }
        }
    }
    finally {
        $stream.Dispose()
    }
}

function Resolve-DbgEngFromDependencyRoot {
    param(
        [Parameter(Mandatory = $true)][string]$Path,
        [Parameter(Mandatory = $true)][string]$SdkArch
    )

    $candidates = @(
        $Path,
        (Join-Path (Join-Path $Path "Debuggers") $SdkArch),
        (Join-Path (Join-Path (Join-Path $Path "Windows Kits") "10") (Join-Path "Debuggers" $SdkArch))
    )

    foreach ($candidate in $candidates) {
        if (Test-DbgEngDirectory -Path $candidate) {
            return (Get-FullPath -Path $candidate)
        }
    }
    return $null
}

function Find-StoreWinDbgDbgEngDir {
    param(
        [Parameter(Mandatory = $true)][string]$StoreArch
    )

    if (-not (Get-Command Get-AppxPackage -ErrorAction SilentlyContinue)) {
        return $null
    }

    $packages = @(Get-AppxPackage -Name "Microsoft.WinDbg" -ErrorAction SilentlyContinue |
        Where-Object { $_.InstallLocation } |
        Sort-Object Version -Descending)

    foreach ($package in $packages) {
        $candidate = Join-Path $package.InstallLocation $StoreArch
        if (Test-DbgEngDirectory -Path $candidate) {
            return (Get-FullPath -Path $candidate)
        }
    }
    return $null
}

function Find-DbgEngDir {
    param(
        [Parameter(Mandatory = $true)][string]$StoreArch,
        [Parameter(Mandatory = $true)][string]$SdkArch
    )

    $store = Find-StoreWinDbgDbgEngDir -StoreArch $StoreArch
    if ($store) {
        return $store
    }

    $roots = New-Object System.Collections.Generic.List[string]
    foreach ($key in @("WindowsSdkDir", "WDKContentRoot", "WindowsSDK_ExecutablePath_x64")) {
        $value = [System.Environment]::GetEnvironmentVariable($key)
        if ($value) {
            $roots.Add($value)
        }
    }
    if ($env:ProgramFiles) {
        $roots.Add((Join-Path (Join-Path $env:ProgramFiles "Windows Kits") "10"))
    }
    $programFilesX86 = [System.Environment]::GetEnvironmentVariable("ProgramFiles(x86)")
    if ($programFilesX86) {
        $roots.Add((Join-Path (Join-Path $programFilesX86 "Windows Kits") "10"))
    }
    $roots.Add("C:\Program Files (x86)\Windows Kits\10")
    $roots.Add("C:\Program Files\Windows Kits\10")

    $seen = New-Object System.Collections.Generic.HashSet[string] ([System.StringComparer]::OrdinalIgnoreCase)
    foreach ($root in $roots) {
        if (-not $seen.Add($root)) {
            continue
        }
        $resolved = Resolve-DbgEngFromDependencyRoot -Path $root -SdkArch $SdkArch
        if ($resolved) {
            return $resolved
        }
    }

    $systemRoot = if ($env:SystemRoot) { $env:SystemRoot } else { "C:\Windows" }
    $system32 = Join-Path $systemRoot "System32"
    if (Test-DbgEngDirectory -Path $system32) {
        return (Get-FullPath -Path $system32)
    }

    return $null
}

function Resolve-SysinternalsFromDependencyRoot {
    param([Parameter(Mandatory = $true)][string]$Path)

    if (Test-SysinternalsDirectory -Path $Path) {
        return (Get-FullPath -Path $Path)
    }
    foreach ($child in @("SysinternalsSuite", "Sysinternals")) {
        $candidate = Join-Path $Path $child
        if (Test-SysinternalsDirectory -Path $candidate) {
            return (Get-FullPath -Path $candidate)
        }
    }
    return $null
}

function Find-SysinternalsDir {
    $candidates = New-Object System.Collections.Generic.List[string]
    foreach ($key in @("DBGFLOW_SYSINTERNALS_DIR", "SysinternalsDir")) {
        $value = [System.Environment]::GetEnvironmentVariable($key)
        if ($value) {
            $candidates.Add($value)
        }
    }
    if ($env:PATH) {
        foreach ($entry in $env:PATH.Split([System.IO.Path]::PathSeparator)) {
            if ($entry) {
                $candidates.Add($entry)
            }
        }
    }
    if ($env:USERPROFILE) {
        $candidates.Add((Join-Path $env:USERPROFILE "Bin"))
    }
    $candidates.Add((Join-Path $RepoRoot "Sysinternals"))
    $parent = Split-Path -Parent $RepoRoot
    if ($parent) {
        $candidates.Add((Join-Path $parent "Sysinternals"))
    }
    $candidates.Add("C:\Tools\Sysinternals")
    $candidates.Add("C:\Sysinternals")
    $candidates.Add("C:\Program Files\Sysinternals")

    $seen = New-Object System.Collections.Generic.HashSet[string] ([System.StringComparer]::OrdinalIgnoreCase)
    foreach ($candidate in $candidates) {
        if (-not $seen.Add($candidate)) {
            continue
        }
        $resolved = Resolve-SysinternalsFromDependencyRoot -Path $candidate
        if ($resolved) {
            return $resolved
        }
    }
    return $null
}

function Get-ProxyConfig {
    if ($NoProxy -and $ProxyUrl) {
        throw "-ProxyUrl and -NoProxy cannot be used together"
    }
    if ($NoProxy) {
        return @{ Mode = "disabled"; Url = $null; Env = @{} }
    }
    if ($ProxyUrl) {
        return @{ Mode = "url"; Url = $ProxyUrl; Env = @{} }
    }

    $proxyEnv = [ordered]@{}
    foreach ($key in $KnownProxyKeys) {
        $value = [System.Environment]::GetEnvironmentVariable($key)
        if ($value) {
            $proxyEnv[$key] = $value
        }
    }
    if ($proxyEnv.Count -gt 0) {
        return @{ Mode = "env"; Url = $null; Env = $proxyEnv }
    }
    return @{ Mode = "none"; Url = $null; Env = @{} }
}

function Assert-SymbolPathValue {
    param(
        [Parameter(Mandatory = $true)][AllowEmptyString()][string]$Value,
        [Parameter(Mandatory = $true)][string]$Label
    )

    if ([string]::IsNullOrWhiteSpace($Value)) {
        throw "$Label must not be empty"
    }
    foreach ($ch in $Value.ToCharArray()) {
        $code = [int][char]$ch
        if ($code -eq 0 -or $code -eq 10 -or $code -eq 13 -or $code -eq 0x2028 -or $code -eq 0x2029 -or [char]::IsControl($ch)) {
            throw "$Label contains unsupported control characters"
        }
    }
}

function Get-SymbolPathConfig {
    if ($SymbolPathWasProvided) {
        Assert-SymbolPathValue -Value $SymbolPath -Label "Symbol path"
        return $SymbolPath
    }

    $parts = New-Object System.Collections.Generic.List[string]
    foreach ($key in @("_NT_ALT_SYMBOL_PATH", "_NT_SYMBOL_PATH")) {
        $value = [System.Environment]::GetEnvironmentVariable($key)
        if ([string]::IsNullOrWhiteSpace($value)) {
            continue
        }
        Assert-SymbolPathValue -Value $value -Label $key
        $parts.Add($value)
    }
    if ($parts.Count -eq 0) {
        return $null
    }
    return ($parts -join ";")
}

function Write-DbgFlowConfig {
    param(
        [Parameter(Mandatory = $true)][string]$Path,
        [Parameter(Mandatory = $true)][string]$InstallRoot,
        [Parameter(Mandatory = $true)][string]$DataDir,
        [string]$ResolvedDbgEngDir,
        [string]$ResolvedSymbolPath,
        [string]$ResolvedSysinternalsDir,
        [Parameter(Mandatory = $true)]$ProxyConfig
    )

    $lines = New-Object System.Collections.Generic.List[string]
    $lines.Add("version = 1")
    $lines.Add("")
    $lines.Add("[service]")
    $lines.Add("name = $(ConvertTo-TomlString $ServiceName)")
    $lines.Add("display_name = $(ConvertTo-TomlString $DisplayName)")
    $lines.Add("install_root = $(ConvertTo-TomlString $InstallRoot)")
    $lines.Add("")
    $lines.Add("[server]")
    $lines.Add("bind = $(ConvertTo-TomlString $Bind)")
    $lines.Add("data_dir = $(ConvertTo-TomlString $DataDir)")

    if ($ResolvedDbgEngDir -or $ResolvedSymbolPath) {
        $lines.Add("")
        $lines.Add("[debugger]")
        if ($ResolvedDbgEngDir) {
            $lines.Add("dbgeng_dir = $(ConvertTo-TomlString $ResolvedDbgEngDir)")
        }
        if ($ResolvedSymbolPath) {
            $lines.Add("symbol_path = $(ConvertTo-TomlString $ResolvedSymbolPath)")
        }
    }
    if ($ResolvedSysinternalsDir) {
        $lines.Add("")
        $lines.Add("[tools]")
        $lines.Add("sysinternals_dir = $(ConvertTo-TomlString $ResolvedSysinternalsDir)")
    }

    $lines.Add("")
    $lines.Add("[proxy]")
    $lines.Add("mode = $(ConvertTo-TomlString $ProxyConfig.Mode)")
    if ($ProxyConfig.Url) {
        $lines.Add("url = $(ConvertTo-TomlString $ProxyConfig.Url)")
    }
    if ($ProxyConfig.Mode -eq "env") {
        $lines.Add("")
        $lines.Add("[proxy.env]")
        foreach ($key in $ProxyConfig.Env.Keys) {
            $lines.Add("$key = $(ConvertTo-TomlString $ProxyConfig.Env[$key])")
        }
    }

    $parent = Split-Path -Parent $Path
    if ($parent) {
        New-Item -ItemType Directory -Force -Path $parent | Out-Null
    }
    $utf8NoBom = New-Object System.Text.UTF8Encoding $false
    [System.IO.File]::WriteAllLines($Path, [string[]]$lines, $utf8NoBom)
}

function Read-YesNo {
    param(
        [Parameter(Mandatory = $true)][string]$Prompt,
        [bool]$DefaultYes = $true
    )

    $suffix = if ($DefaultYes) { "[Y/n]" } else { "[y/N]" }
    while ($true) {
        $answer = Read-Host "$Prompt $suffix"
        if ([string]::IsNullOrWhiteSpace($answer)) {
            return $DefaultYes
        }
        if ($answer -match '^(y|yes)$') {
            return $true
        }
        if ($answer -match '^(n|no)$') {
            return $false
        }
        Write-Host "Please answer y or n."
    }
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

    $InstallRoot = Get-FullPath -Path $InstallRoot
    Assert-SafeInstallRoot -Path $InstallRoot
    if (-not $ConfigPath) {
        $ConfigPath = Join-Path $InstallRoot "config.toml"
    }
    $ConfigPath = Get-FullPath -Path $ConfigPath
    $DataDir = Join-Path $InstallRoot "var"

    if (-not (Test-PathUnderRoot -Path $ConfigPath -Root $InstallRoot)) {
        throw "Config path must be under install root: $ConfigPath"
    }

    $arch = Get-ExeArchitectureInfo -Path $Exe
    $resolvedDbgEngDir = $DbgEngDir
    if ($resolvedDbgEngDir) {
        $resolvedDbgEngDir = Get-FullPath -Path $resolvedDbgEngDir
        if (-not (Test-DbgEngDirectory -Path $resolvedDbgEngDir)) {
            throw "DbgEng directory must contain dbgeng.dll: $resolvedDbgEngDir"
        }
    } else {
        $resolvedDbgEngDir = Find-DbgEngDir -StoreArch $arch.StoreArch -SdkArch $arch.SdkArch
    }

    $resolvedSysinternalsDir = $SysinternalsDir
    if ($resolvedSysinternalsDir) {
        $resolvedSysinternalsDir = Get-FullPath -Path $resolvedSysinternalsDir
        if (-not (Test-SysinternalsDirectory -Path $resolvedSysinternalsDir)) {
            throw "Sysinternals directory must contain Procmon64.exe or Procmon.exe: $resolvedSysinternalsDir"
        }
    } else {
        $resolvedSysinternalsDir = Find-SysinternalsDir
    }

    $proxyConfig = Get-ProxyConfig
    $resolvedSymbolPath = Get-SymbolPathConfig

    Write-Host "dbgflow Windows service install"
    Write-Host ""
    Write-Host "Service name: $ServiceName"
    Write-Host "Display name: $DisplayName"
    Write-Host "Bind: $Bind"
    Write-Host "Install root: $InstallRoot"
    Write-Host "Config: $ConfigPath"
    Write-Host "Data dir: $DataDir"
    Write-Host "DbgEng dir: $(if ($resolvedDbgEngDir) { $resolvedDbgEngDir } else { 'not configured' })"
    Write-Host "Symbol path: $(if ($resolvedSymbolPath) { 'configured' } else { 'not configured' })"
    Write-Host "Sysinternals dir: $(if ($resolvedSysinternalsDir) { $resolvedSysinternalsDir } else { 'not configured' })"
    Write-Host "Proxy mode: $($proxyConfig.Mode)"
    if ($proxyConfig.Url) {
        Write-Host "Proxy URL: $($proxyConfig.Url)"
    }
    if ($proxyConfig.Mode -eq "env") {
        Write-Host "Proxy keys: $($proxyConfig.Env.Keys -join ', ')"
    }
    Write-Host "Service command: $InstallRoot\bin\dbgflow-mcp.exe service run --config $ConfigPath"
    Write-Host ""

    if (-not $NonInteractive) {
        if (-not (Read-YesNo -Prompt "Write config and install service?" -DefaultYes $true)) {
            throw "service install cancelled"
        }
    }

    Write-DbgFlowConfig -Path $ConfigPath -InstallRoot $InstallRoot -DataDir $DataDir -ResolvedDbgEngDir $resolvedDbgEngDir -ResolvedSymbolPath $resolvedSymbolPath -ResolvedSysinternalsDir $resolvedSysinternalsDir -ProxyConfig $proxyConfig

    & $Exe service install --config $ConfigPath
    exit $LASTEXITCODE
}
finally {
    Pop-Location
}

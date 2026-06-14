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
    [string]$TtdDir,
    [string]$IdaInstallDir,
    [string]$IdaPythonExecutable,
    [ValidateRange(1, 1024)]
    [int]$IdaMaxWorkers = 4,
    [switch]$NoProxy,
    [switch]$NonInteractive
)

$ErrorActionPreference = "Stop"
$SymbolPathWasProvided = $PSBoundParameters.ContainsKey("SymbolPath")

$ScriptDir = Split-Path -Parent $PSCommandPath
$RepoRoot = Split-Path -Parent $ScriptDir
$Exe = Join-Path $RepoRoot "target\release\dbgflow-mcp.exe"
$VendoredIdaProMcpRoot = Join-Path $RepoRoot "vendor\ida-pro-mcp"
$IdaPythonPackages = @("idapro>=0.0.9", "tomli-w>=1.0.0")
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

function Test-TtdDirectory {
    param([Parameter(Mandatory = $true)][string]$Path)

    return (Test-Path -LiteralPath (Join-Path $Path "TTD.exe") -PathType Leaf)
}

function Test-IdaInstallDirectory {
    param([Parameter(Mandatory = $true)][string]$Path)

    foreach ($fileName in @("ida.exe", "ida.dll", "idalib.dll", "ida.hlp")) {
        if (-not (Test-Path -LiteralPath (Join-Path $Path $fileName) -PathType Leaf)) {
            return $false
        }
    }
    return $true
}

function Test-IdaProMcpVendorRoot {
    param([Parameter(Mandatory = $true)][string]$Path)

    return (Test-Path -LiteralPath (Join-Path $Path "LICENSE") -PathType Leaf) -and
        (Test-Path -LiteralPath (Join-Path $Path "pyproject.toml") -PathType Leaf) -and
        (Test-Path -LiteralPath (Join-Path (Join-Path $Path "src") "ida_pro_mcp\idalib_supervisor.py") -PathType Leaf)
}

function Get-InstalledIdaVendorRoot {
    param([Parameter(Mandatory = $true)][string]$InstallRoot)

    return (Join-Path (Join-Path (Join-Path $InstallRoot "bin") "vendor") "ida-pro-mcp")
}

function Get-IdaVenvDir {
    param([Parameter(Mandatory = $true)][string]$InstallRoot)

    return (Join-Path (Join-Path $InstallRoot "python") "ida-venv")
}

function Get-IdaVenvPythonPath {
    param([Parameter(Mandatory = $true)][string]$VenvDir)

    return (Join-Path (Join-Path $VenvDir "Scripts") "python.exe")
}

function Invoke-CheckedProcess {
    param(
        [Parameter(Mandatory = $true)][string]$FilePath,
        [string[]]$Arguments = @(),
        [Parameter(Mandatory = $true)][string]$ErrorMessage
    )

    $output = & $FilePath @Arguments 2>&1
    $exitCode = $LASTEXITCODE
    foreach ($line in $output) {
        Write-Host $line
    }
    if ($exitCode -ne 0) {
        throw "$ErrorMessage (exit code $exitCode)"
    }
}

function Invoke-CheckedPythonBootstrap {
    param(
        [Parameter(Mandatory = $true)]$Bootstrap,
        [string[]]$Arguments = @(),
        [Parameter(Mandatory = $true)][string]$ErrorMessage
    )

    $output = & $Bootstrap.Command @($Bootstrap.Arguments) @Arguments 2>&1
    $exitCode = $LASTEXITCODE
    foreach ($line in $output) {
        Write-Host $line
    }
    if ($exitCode -ne 0) {
        throw "$ErrorMessage (exit code $exitCode)"
    }
}

function Test-PythonBootstrap {
    param([Parameter(Mandatory = $true)]$Bootstrap)

    try {
        $code = "import sys; raise SystemExit(0 if sys.version_info >= (3, 11) else 1)"
        & $Bootstrap.Command @($Bootstrap.Arguments) -c $code *> $null
        return $LASTEXITCODE -eq 0
    }
    catch {
        return $false
    }
}

function Resolve-IdaPythonBootstrap {
    $candidates = New-Object System.Collections.Generic.List[object]

    if ($IdaPythonExecutable) {
        $path = Get-FullPath -Path $IdaPythonExecutable
        if (-not (Test-Path -LiteralPath $path -PathType Leaf)) {
            throw "IDA Python executable was not found: $path"
        }
        $candidates.Add([pscustomobject]@{ Command = $path; Arguments = @(); Display = $path })
    }

    $envPython = [System.Environment]::GetEnvironmentVariable("DBGFLOW_IDA_PYTHON")
    if ($envPython) {
        try {
            $path = Get-FullPath -Path $envPython
            if (Test-Path -LiteralPath $path -PathType Leaf) {
                $candidates.Add([pscustomobject]@{ Command = $path; Arguments = @(); Display = $path })
            }
        }
        catch {
        }
    }

    $py = Get-Command "py.exe" -ErrorAction SilentlyContinue
    if ($py -and $py.Source) {
        $candidates.Add([pscustomobject]@{ Command = $py.Source; Arguments = @("-3.11"); Display = "$($py.Source) -3.11" })
    }

    foreach ($name in @("python.exe", "python3.exe")) {
        $command = Get-Command $name -ErrorAction SilentlyContinue
        if ($command -and $command.Source) {
            $candidates.Add([pscustomobject]@{ Command = $command.Source; Arguments = @(); Display = $command.Source })
        }
    }

    foreach ($candidate in $candidates) {
        if (Test-PythonBootstrap -Bootstrap $candidate) {
            return $candidate
        }
    }

    throw "Python 3.11 or newer was not found. Install Python 3.11+ or pass -IdaPythonExecutable <python.exe>."
}

function Copy-IdaProMcpVendor {
    param(
        [Parameter(Mandatory = $true)][string]$SourceRoot,
        [Parameter(Mandatory = $true)][string]$InstallRoot
    )

    if (-not (Test-IdaProMcpVendorRoot -Path $SourceRoot)) {
        throw "Vendored ida-pro-mcp runtime is incomplete: $SourceRoot"
    }

    $destination = Get-InstalledIdaVendorRoot -InstallRoot $InstallRoot
    if (-not (Test-PathUnderRoot -Path $destination -Root $InstallRoot)) {
        throw "IDA vendor destination must be under install root: $destination"
    }

    if (Test-Path -LiteralPath $destination) {
        Remove-Item -LiteralPath $destination -Recurse -Force
    }
    New-Item -ItemType Directory -Force -Path (Split-Path -Parent $destination) | Out-Null
    Copy-Item -LiteralPath $SourceRoot -Destination $destination -Recurse -Force

    $vendorSrc = Join-Path $destination "src"
    if (-not (Test-Path -LiteralPath (Join-Path $vendorSrc "ida_pro_mcp\idalib_supervisor.py") -PathType Leaf)) {
        throw "Copied ida-pro-mcp runtime is incomplete: $vendorSrc"
    }
    return (Get-FullPath -Path $vendorSrc)
}

function Invoke-IdaPythonRuntimeCheck {
    param(
        [Parameter(Mandatory = $true)][string]$PythonExecutable,
        [Parameter(Mandatory = $true)][string]$IdaInstallDir,
        [Parameter(Mandatory = $true)][string]$VendorSrcDir
    )

    $oldIdaDir = [System.Environment]::GetEnvironmentVariable("IDADIR", "Process")
    $oldPath = [System.Environment]::GetEnvironmentVariable("PATH", "Process")
    $oldPythonPath = [System.Environment]::GetEnvironmentVariable("PYTHONPATH", "Process")
    try {
        [System.Environment]::SetEnvironmentVariable("IDADIR", $IdaInstallDir, "Process")
        [System.Environment]::SetEnvironmentVariable("PATH", "$IdaInstallDir;$oldPath", "Process")
        $pythonPath = if ($oldPythonPath) { "$VendorSrcDir;$oldPythonPath" } else { $VendorSrcDir }
        [System.Environment]::SetEnvironmentVariable("PYTHONPATH", $pythonPath, "Process")

        $code = "import sys; assert sys.version_info >= (3, 11); import tomli_w; import idapro; import ida_pro_mcp.idalib_supervisor; import ida_pro_mcp.idalib_server; print('IDA Python runtime OK')"
        Invoke-CheckedProcess -FilePath $PythonExecutable -Arguments @("-c", $code) -ErrorMessage "IDA Python runtime verification failed"
    }
    finally {
        [System.Environment]::SetEnvironmentVariable("IDADIR", $oldIdaDir, "Process")
        [System.Environment]::SetEnvironmentVariable("PATH", $oldPath, "Process")
        [System.Environment]::SetEnvironmentVariable("PYTHONPATH", $oldPythonPath, "Process")
    }
}

function Initialize-IdaPythonVirtualEnvironment {
    param(
        [Parameter(Mandatory = $true)][string]$InstallRoot,
        [Parameter(Mandatory = $true)][string]$IdaInstallDir,
        [Parameter(Mandatory = $true)][string]$VendorSrcDir,
        [Parameter(Mandatory = $true)]$ProxyConfig
    )

    $venvDir = Get-IdaVenvDir -InstallRoot $InstallRoot
    $venvPython = Get-IdaVenvPythonPath -VenvDir $venvDir
    if (-not (Test-Path -LiteralPath $venvPython -PathType Leaf)) {
        $bootstrap = Resolve-IdaPythonBootstrap
        Write-Host "Creating IDA Python venv with $($bootstrap.Display): $venvDir"
        New-Item -ItemType Directory -Force -Path (Split-Path -Parent $venvDir) | Out-Null
        Invoke-CheckedPythonBootstrap -Bootstrap $bootstrap -Arguments @("-m", "venv", $venvDir) -ErrorMessage "Create IDA Python virtual environment failed"
    } else {
        Write-Host "Reusing IDA Python venv: $venvDir"
    }

    if (-not (Test-Path -LiteralPath $venvPython -PathType Leaf)) {
        throw "IDA Python venv did not create python.exe: $venvPython"
    }

    $activateIdalib = Join-Path $IdaInstallDir "idalib\python\py-activate-idalib.py"
    if (Test-Path -LiteralPath $activateIdalib -PathType Leaf) {
        Write-Host "Activating idalib for venv using: $activateIdalib"
        Invoke-CheckedProcess -FilePath $venvPython -Arguments @($activateIdalib, "--ida-install-dir", $IdaInstallDir) -ErrorMessage "Activate IDA idalib Python config failed"
    }

    Write-Host "Installing IDA Python runtime packages: $($IdaPythonPackages -join ', ')"
    Invoke-CheckedProcess -FilePath $venvPython -Arguments @("-m", "ensurepip", "--upgrade") -ErrorMessage "Initialize pip in IDA Python venv failed"
    $pipArgs = @("-m", "pip", "install")
    if ($ProxyConfig.Url) {
        $pipArgs += @("--proxy", $ProxyConfig.Url)
    }
    Invoke-CheckedProcess -FilePath $venvPython -Arguments ($pipArgs + @("--upgrade") + $IdaPythonPackages) -ErrorMessage "Install IDA Python runtime packages failed"

    Invoke-IdaPythonRuntimeCheck -PythonExecutable $venvPython -IdaInstallDir $IdaInstallDir -VendorSrcDir $VendorSrcDir
    return (Get-FullPath -Path $venvPython)
}

function Add-PathCandidate {
    param(
        [Parameter(Mandatory = $true)]$Candidates,
        [Parameter(Mandatory = $true)]$Seen,
        [string]$Path
    )

    if ([string]::IsNullOrWhiteSpace($Path)) {
        return
    }

    try {
        $fullPath = Get-FullPath -Path $Path
    }
    catch {
        return
    }

    if ($Seen.Add($fullPath)) {
        $Candidates.Add($fullPath)
    }
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

function Find-StoreTtdDir {
    if (-not (Get-Command Get-AppxPackage -ErrorAction SilentlyContinue)) {
        return $null
    }

    $packages = @(Get-AppxPackage -Name "Microsoft.TimeTravelDebugging" -ErrorAction SilentlyContinue |
        Where-Object { $_.InstallLocation } |
        Sort-Object Version -Descending)

    foreach ($package in $packages) {
        if (Test-TtdDirectory -Path $package.InstallLocation) {
            return (Get-FullPath -Path $package.InstallLocation)
        }
    }
    return $null
}

function Resolve-TtdFromDbgEngDir {
    param([string]$DbgEngDir)

    if (-not $DbgEngDir) {
        return $null
    }

    $candidate = Join-Path $DbgEngDir "ttd"
    if (Test-TtdDirectory -Path $candidate) {
        return (Get-FullPath -Path $candidate)
    }
    return $null
}

function Find-TtdDir {
    param([string]$DbgEngDir)

    $derived = Resolve-TtdFromDbgEngDir -DbgEngDir $DbgEngDir
    if ($derived) {
        return $derived
    }

    $store = Find-StoreTtdDir
    if ($store) {
        return $store
    }

    $command = Get-Command "TTD.exe" -ErrorAction SilentlyContinue
    if ($command -and $command.Source) {
        $dir = Split-Path -Parent $command.Source
        if ($dir -and (Test-TtdDirectory -Path $dir)) {
            return (Get-FullPath -Path $dir)
        }
    }
    return $null
}

function Add-IdaRegistryCandidates {
    param(
        [Parameter(Mandatory = $true)]$Candidates,
        [Parameter(Mandatory = $true)]$Seen
    )

    $registryRoots = @(
        "HKLM:\SOFTWARE\Microsoft\Windows\CurrentVersion\Uninstall",
        "HKLM:\SOFTWARE\WOW6432Node\Microsoft\Windows\CurrentVersion\Uninstall",
        "HKCU:\SOFTWARE\Microsoft\Windows\CurrentVersion\Uninstall"
    )

    foreach ($root in $registryRoots) {
        if (-not (Test-Path -LiteralPath $root)) {
            continue
        }

        $subKeys = @(Get-ChildItem -LiteralPath $root -ErrorAction SilentlyContinue)
        foreach ($subKey in $subKeys) {
            try {
                $item = Get-ItemProperty -LiteralPath $subKey.PSPath -ErrorAction Stop
            }
            catch {
                continue
            }

            $displayName = [string]$item.DisplayName
            if ([string]::IsNullOrWhiteSpace($displayName) -or $displayName -notmatch '^(IDA(\s|$)|Hex-Rays.*IDA)') {
                continue
            }

            Add-PathCandidate -Candidates $Candidates -Seen $Seen -Path ([string]$item.InstallLocation)
            if ($item.DisplayIcon) {
                $displayIcon = ([string]$item.DisplayIcon).Trim()
                if ($displayIcon.StartsWith('"')) {
                    $displayIconPath = ($displayIcon -replace '^\s*"', '') -replace '"\s*(,.*)?$', ''
                } else {
                    $displayIconPath = ($displayIcon -split ',', 2)[0].Trim()
                }
                if ($displayIconPath -match '\.exe$' -and (Test-Path -LiteralPath $displayIconPath -PathType Leaf)) {
                    Add-PathCandidate -Candidates $Candidates -Seen $Seen -Path (Split-Path -Parent $displayIconPath)
                }
            }
        }
    }
}

function Find-IdaInstallDir {
    $candidates = New-Object System.Collections.Generic.List[string]
    $seen = New-Object System.Collections.Generic.HashSet[string] ([System.StringComparer]::OrdinalIgnoreCase)

    Add-PathCandidate -Candidates $candidates -Seen $seen -Path ([System.Environment]::GetEnvironmentVariable("DBGFLOW_IDA_DIR"))
    Add-IdaRegistryCandidates -Candidates $candidates -Seen $seen

    $roots = New-Object System.Collections.Generic.List[string]
    foreach ($root in @(
        $env:ProgramFiles,
        [System.Environment]::GetEnvironmentVariable("ProgramFiles(x86)"),
        $env:LOCALAPPDATA,
        $env:ProgramData,
        "C:\Program Files",
        "C:\Program Files (x86)"
    )) {
        if (-not [string]::IsNullOrWhiteSpace($root)) {
            $roots.Add($root)
        }
    }

    $versions = @("9.4", "9.3", "9.2", "9.1", "9.0", "8.5", "8.4", "8.3", "8.2", "8.1", "8.0")
    foreach ($root in $roots) {
        foreach ($version in $versions) {
            foreach ($name in @(
                "IDA Professional $version",
                "IDA Pro $version",
                "IDA Freeware $version"
            )) {
                Add-PathCandidate -Candidates $candidates -Seen $seen -Path (Join-Path $root $name)
            }
        }
    }

    foreach ($root in $roots) {
        if (-not (Test-Path -LiteralPath $root -PathType Container)) {
            continue
        }
        Get-ChildItem -LiteralPath $root -Directory -Filter "IDA*" -ErrorAction SilentlyContinue |
            Sort-Object Name -Descending |
            ForEach-Object {
                Add-PathCandidate -Candidates $candidates -Seen $seen -Path $_.FullName
            }
    }

    foreach ($candidate in $candidates) {
        if (Test-IdaInstallDirectory -Path $candidate) {
            return (Get-FullPath -Path $candidate)
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
        [string]$ResolvedTtdDir,
        [string]$ResolvedIdaInstallDir,
        [string]$ResolvedIdaPythonExecutable,
        [string]$ResolvedIdaVendorSrcDir,
        [int]$ResolvedIdaMaxWorkers = 0,
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
    if ($ResolvedTtdDir) {
        $lines.Add("")
        $lines.Add("[tools]")
        $lines.Add("ttd_dir = $(ConvertTo-TomlString $ResolvedTtdDir)")
    }
    if ($ResolvedIdaInstallDir) {
        $lines.Add("")
        $lines.Add("[reverse.ida]")
        $lines.Add("install_dir = $(ConvertTo-TomlString $ResolvedIdaInstallDir)")
        if ($ResolvedIdaPythonExecutable) {
            $lines.Add("python_executable = $(ConvertTo-TomlString $ResolvedIdaPythonExecutable)")
        }
        if ($ResolvedIdaVendorSrcDir) {
            $lines.Add("vendor_src_dir = $(ConvertTo-TomlString $ResolvedIdaVendorSrcDir)")
        }
        if ($ResolvedIdaMaxWorkers -gt 0) {
            $lines.Add("max_workers = $ResolvedIdaMaxWorkers")
        }
    }

    $lines.Add("")
    $lines.Add("[process]")
    $lines.Add('child_identity = "mcp_peer_session"')
    $lines.Add('fallback_child_identity = "active_interactive_session"')
    $lines.Add("elevate_if_admin = true")

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

    $resolvedTtdDir = $TtdDir
    if ($resolvedTtdDir) {
        $resolvedTtdDir = Get-FullPath -Path $resolvedTtdDir
        if (-not (Test-TtdDirectory -Path $resolvedTtdDir)) {
            throw "TTD directory must contain TTD.exe: $resolvedTtdDir"
        }
    } else {
        $resolvedTtdDir = Find-TtdDir -DbgEngDir $resolvedDbgEngDir
    }

    $resolvedIdaInstallDir = $IdaInstallDir
    if ($resolvedIdaInstallDir) {
        $resolvedIdaInstallDir = Get-FullPath -Path $resolvedIdaInstallDir
        if (-not (Test-IdaInstallDirectory -Path $resolvedIdaInstallDir)) {
            throw "IDA install directory must contain ida.exe, ida.dll, idalib.dll, and ida.hlp: $resolvedIdaInstallDir"
        }
    } else {
        $resolvedIdaInstallDir = Find-IdaInstallDir
    }

    $proxyConfig = Get-ProxyConfig
    $resolvedSymbolPath = Get-SymbolPathConfig
    $plannedIdaVendorSrcDir = $null
    $plannedIdaPythonExecutable = $null
    $resolvedIdaVendorSrcDir = $null
    $resolvedIdaPythonExecutable = $null
    $resolvedIdaMaxWorkers = 0
    if ($resolvedIdaInstallDir) {
        $plannedIdaVendorSrcDir = Join-Path (Get-InstalledIdaVendorRoot -InstallRoot $InstallRoot) "src"
        $plannedIdaPythonExecutable = Get-IdaVenvPythonPath -VenvDir (Get-IdaVenvDir -InstallRoot $InstallRoot)
        $resolvedIdaMaxWorkers = $IdaMaxWorkers
    }

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
    Write-Host "TTD dir: $(if ($resolvedTtdDir) { $resolvedTtdDir } else { 'not configured' })"
    Write-Host "IDA dir: $(if ($resolvedIdaInstallDir) { $resolvedIdaInstallDir } else { 'not configured' })"
    Write-Host "IDA Python venv: $(if ($plannedIdaPythonExecutable) { $plannedIdaPythonExecutable } else { 'not configured' })"
    Write-Host "IDA vendor src: $(if ($plannedIdaVendorSrcDir) { $plannedIdaVendorSrcDir } else { 'not configured' })"
    Write-Host "IDA max workers: $(if ($resolvedIdaMaxWorkers -gt 0) { $resolvedIdaMaxWorkers } else { 'not configured' })"
    Write-Host "Child identity: mcp_peer_session (fallback active_interactive_session, elevate_if_admin true)"
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

    if ($resolvedIdaInstallDir) {
        Write-Host "Copying vendored ida-pro-mcp runtime..."
        $resolvedIdaVendorSrcDir = Copy-IdaProMcpVendor -SourceRoot $VendoredIdaProMcpRoot -InstallRoot $InstallRoot
        $resolvedIdaPythonExecutable = Initialize-IdaPythonVirtualEnvironment -InstallRoot $InstallRoot -IdaInstallDir $resolvedIdaInstallDir -VendorSrcDir $resolvedIdaVendorSrcDir -ProxyConfig $proxyConfig
    }

    Write-DbgFlowConfig -Path $ConfigPath -InstallRoot $InstallRoot -DataDir $DataDir -ResolvedDbgEngDir $resolvedDbgEngDir -ResolvedSymbolPath $resolvedSymbolPath -ResolvedTtdDir $resolvedTtdDir -ResolvedIdaInstallDir $resolvedIdaInstallDir -ResolvedIdaPythonExecutable $resolvedIdaPythonExecutable -ResolvedIdaVendorSrcDir $resolvedIdaVendorSrcDir -ResolvedIdaMaxWorkers $resolvedIdaMaxWorkers -ProxyConfig $proxyConfig

    & $Exe service install --config $ConfigPath
    exit $LASTEXITCODE
}
finally {
    Pop-Location
}

#Requires -Version 5.1
[CmdletBinding(SupportsShouldProcess = $true, ConfirmImpact = 'Medium')]
param(
    [string]$ServerName = "dbgflow-mcp",
    [string]$Endpoint = "http://127.0.0.1:7331/mcp",
    [string]$ProjectRoot = (Get-Location).Path,
    [switch]$Global,
    [switch]$Force,
    [switch]$NonInteractive,
    [switch]$SkipHealthCheck,
    [switch]$RequireHealth
)

$ErrorActionPreference = "Stop"

function ConvertTo-TomlString {
    param(
        [Parameter(Mandatory = $true)]
        [string]$Value
    )

    $escaped = $Value.Replace("\", "\\").Replace('"', '\"')
    return '"' + $escaped + '"'
}

function ConvertTo-TomlKey {
    param(
        [Parameter(Mandatory = $true)]
        [string]$Value
    )

    if ($Value -match '^[A-Za-z0-9_-]+$') {
        return $Value
    }

    return ConvertTo-TomlString -Value $Value
}

function Resolve-CodexCli {
    $command = Get-Command codex -ErrorAction SilentlyContinue
    if ($command) {
        if ($command.Path) {
            return $command.Path
        }

        return $command.Source
    }

    throw "codex CLI was not found on PATH. Open a Codex-enabled shell or add codex.exe to PATH, then run this script again."
}

function Resolve-CodexHome {
    $codexHome = $env:CODEX_HOME
    if ([string]::IsNullOrWhiteSpace($codexHome)) {
        $codexHome = Join-Path $HOME ".codex"
    }

    return $codexHome
}

function Resolve-GlobalCodexConfigPath {
    return Join-Path (Resolve-CodexHome) "config.toml"
}

function Resolve-ProjectCodexConfigPath {
    param(
        [Parameter(Mandatory = $true)]
        [string]$Root
    )

    $resolvedRoot = (Resolve-Path -LiteralPath $Root).Path
    return Join-Path (Join-Path $resolvedRoot ".codex") "config.toml"
}

function Backup-CodexConfig {
    param(
        [Parameter(Mandatory = $true)]
        [string]$ConfigPath
    )

    if ($script:CodexConfigBackupPath) {
        return $script:CodexConfigBackupPath
    }

    if (-not (Test-Path -LiteralPath $ConfigPath)) {
        return $null
    }

    $timestamp = Get-Date -Format "yyyyMMdd-HHmmss"
    $backupPath = "$ConfigPath.bak.$timestamp"
    Copy-Item -LiteralPath $ConfigPath -Destination $backupPath -Force
    $script:CodexConfigBackupPath = $backupPath
    Write-Host "Backed up Codex config: $backupPath"
    return $backupPath
}

function Restore-CodexConfig {
    param(
        [Parameter(Mandatory = $true)]
        [string]$ConfigPath,
        [Parameter(Mandatory = $true)]
        [string]$BackupPath
    )

    Copy-Item -LiteralPath $BackupPath -Destination $ConfigPath -Force
    Write-Warning "Restored Codex config from backup: $BackupPath"
}

function Invoke-CodexCommand {
    param(
        [Parameter(Mandatory = $true)]
        [string]$CodexCli,
        [Parameter(Mandatory = $true)]
        [string[]]$Arguments
    )

    $previousErrorActionPreference = $ErrorActionPreference
    $ErrorActionPreference = "Continue"
    try {
        $output = & $CodexCli @Arguments 2>&1 | ForEach-Object { $_.ToString() }
        $exitCode = $LASTEXITCODE
    }
    finally {
        $ErrorActionPreference = $previousErrorActionPreference
    }

    [pscustomobject]@{
        ExitCode = $exitCode
        Output   = ($output -join [Environment]::NewLine)
    }
}

function Resolve-HealthzUri {
    param(
        [Parameter(Mandatory = $true)]
        [uri]$McpEndpoint
    )

    $builder = [System.UriBuilder]::new($McpEndpoint)
    $builder.Path = "/healthz"
    $builder.Query = ""
    $builder.Fragment = ""
    return $builder.Uri.AbsoluteUri
}

function Test-LoopbackEndpoint {
    param(
        [Parameter(Mandatory = $true)]
        [uri]$McpEndpoint
    )

    if ($McpEndpoint.Scheme -ne "http" -and $McpEndpoint.Scheme -ne "https") {
        throw "Endpoint must be an HTTP URL: $McpEndpoint"
    }

    $hostName = $McpEndpoint.Host.ToLowerInvariant()
    if ($hostName -ne "localhost" -and $hostName -ne "127.0.0.1" -and $hostName -ne "::1" -and $hostName -ne "[::1]") {
        throw "dbgflow HTTP MCP must use a loopback endpoint, got: $McpEndpoint"
    }
}

function Test-DbgflowHealth {
    param(
        [Parameter(Mandatory = $true)]
        [uri]$McpEndpoint
    )

    $healthz = Resolve-HealthzUri -McpEndpoint $McpEndpoint
    try {
        $response = Invoke-WebRequest -UseBasicParsing -Method Get -Uri $healthz -TimeoutSec 5
    }
    catch {
        throw "dbgflow service did not respond at $healthz. Start or reinstall the dbgflow-mcp service, or rerun with -SkipHealthCheck. $($_.Exception.Message)"
    }

    if ($response.StatusCode -ne 200) {
        throw "dbgflow service health check failed at $healthz with HTTP $($response.StatusCode)."
    }
}

function Test-McpServerExists {
    param(
        [Parameter(Mandatory = $true)]
        [string]$CodexCli,
        [Parameter(Mandatory = $true)]
        [string]$Name
    )

    $result = Invoke-CodexCommand -CodexCli $CodexCli -Arguments @("mcp", "get", $Name)
    if ($result.ExitCode -eq 0) {
        return $true
    }

    if ($result.Output -match "No MCP server named .* found") {
        return $false
    }

    throw "Failed to inspect Codex MCP server '$Name'. codex mcp get exited with $($result.ExitCode).$([Environment]::NewLine)$($result.Output)"
}

function Test-ProjectMcpServerExists {
    param(
        [Parameter(Mandatory = $true)]
        [string]$ConfigPath,
        [Parameter(Mandatory = $true)]
        [string]$Name
    )

    if (-not (Test-Path -LiteralPath $ConfigPath)) {
        return $false
    }

    $content = Get-Content -Raw -LiteralPath $ConfigPath
    foreach ($line in ($content -split "\r?\n")) {
        if (Test-IsTargetMcpTable -Line $line -Name $Name) {
            return $true
        }
    }

    return $false
}

function Test-IsTargetMcpTable {
    param(
        [Parameter(Mandatory = $true)]
        [AllowEmptyString()]
        [string]$Line,
        [Parameter(Mandatory = $true)]
        [string]$Name
    )

    if ($Line -notmatch '^\s*\[([^\]]+)\]\s*(?:#.*)?$') {
        return $false
    }

    $table = $matches[1].Trim()
    $quotedName = ConvertTo-TomlString -Value $Name
    $candidates = @(
        "mcp_servers.$Name",
        "mcp_servers.$quotedName"
    )
    if ($Name -notmatch "'") {
        $candidates += "mcp_servers.'$Name'"
    }

    foreach ($candidate in $candidates) {
        if ($table -eq $candidate -or $table.StartsWith("$candidate.", [System.StringComparison]::Ordinal)) {
            return $true
        }
    }

    return $false
}

function Set-ProjectMcpServer {
    param(
        [Parameter(Mandatory = $true)]
        [string]$ConfigPath,
        [Parameter(Mandatory = $true)]
        [string]$Name,
        [Parameter(Mandatory = $true)]
        [string]$Url
    )

    $configDir = Split-Path -Parent $ConfigPath
    if (-not (Test-Path -LiteralPath $configDir)) {
        New-Item -ItemType Directory -Path $configDir -Force | Out-Null
    }

    $rawLines = @()
    if (Test-Path -LiteralPath $ConfigPath) {
        $rawContent = Get-Content -Raw -LiteralPath $ConfigPath
        if ($rawContent.Length -gt 0) {
            $rawLines = $rawContent -split "\r?\n"
        }
    }

    $lines = [System.Collections.Generic.List[string]]::new()
    $skipTargetTable = $false
    foreach ($line in $rawLines) {
        $isTableHeader = $line -match '^\s*\[[^\]]+\]\s*(?:#.*)?$'
        if ($isTableHeader) {
            if (Test-IsTargetMcpTable -Line $line -Name $Name) {
                $skipTargetTable = $true
                continue
            }

            $skipTargetTable = $false
        }

        if (-not $skipTargetTable) {
            $lines.Add($line)
        }
    }

    while ($lines.Count -gt 0 -and [string]::IsNullOrWhiteSpace($lines[$lines.Count - 1])) {
        $lines.RemoveAt($lines.Count - 1)
    }

    if ($lines.Count -gt 0) {
        $lines.Add("")
    }

    $serverKey = ConvertTo-TomlKey -Value $Name
    $lines.Add("[mcp_servers.$serverKey]")
    $lines.Add("url = $(ConvertTo-TomlString -Value $Url)")
    $lines.Add("enabled = true")

    $output = ($lines -join [Environment]::NewLine) + [Environment]::NewLine
    $utf8NoBom = [System.Text.UTF8Encoding]::new($false)
    [System.IO.File]::WriteAllText($ConfigPath, $output, $utf8NoBom)
}

function Install-ProjectMcpServer {
    param(
        [Parameter(Mandatory = $true)]
        [string]$ConfigPath,
        [Parameter(Mandatory = $true)]
        [string]$Name,
        [Parameter(Mandatory = $true)]
        [string]$Url
    )

    $exists = Test-ProjectMcpServerExists -ConfigPath $ConfigPath -Name $Name
    if ($exists -and -not $Force) {
        if ($NonInteractive) {
            throw "Project Codex MCP server '$Name' already exists in '$ConfigPath'. Rerun with -Force to replace it."
        }

        $answer = Read-Host "Project Codex MCP server '$Name' already exists. Replace it? [Y/n]"
        if ($answer -match '^(n|no)$') {
            Write-Host "Canceled. Existing project Codex MCP server '$Name' was left unchanged."
            exit 0
        }
    }

    $action = if ($exists) {
        "replace project-scoped streamable HTTP endpoint $Url"
    }
    else {
        "add project-scoped streamable HTTP endpoint $Url"
    }

    if ($PSCmdlet.ShouldProcess("Project Codex config '$ConfigPath'", $action)) {
        $hadConfig = Test-Path -LiteralPath $ConfigPath
        $backupPath = Backup-CodexConfig -ConfigPath $ConfigPath
        try {
            Set-ProjectMcpServer -ConfigPath $ConfigPath -Name $Name -Url $Url
        }
        catch {
            $operationError = $_
            if ($backupPath) {
                try {
                    Restore-CodexConfig -ConfigPath $ConfigPath -BackupPath $backupPath
                }
                catch {
                    throw "Failed to restore Codex config from '$backupPath' after install failure. Original error: $operationError Restore error: $($_.Exception.Message)"
                }
            }
            elseif (-not $hadConfig -and (Test-Path -LiteralPath $ConfigPath)) {
                Remove-Item -LiteralPath $ConfigPath -Force
            }

            throw $operationError
        }

        Write-Host ""
        Write-Host "Installed project-scoped Codex MCP server:"
        Write-Host "  config: $ConfigPath"
        Write-Host "  name:   $Name"
        Write-Host "  url:    $Url"
        Write-Host ""
        Write-Host "Codex loads project .codex/config.toml only for trusted projects."
    }
}

function Install-GlobalMcpServer {
    param(
        [Parameter(Mandatory = $true)]
        [string]$Name,
        [Parameter(Mandatory = $true)]
        [string]$Url
    )

    $codexCli = Resolve-CodexCli
    $codexConfigPath = Resolve-GlobalCodexConfigPath
    $exists = Test-McpServerExists -CodexCli $codexCli -Name $Name

    if ($exists -and -not $Force) {
        if ($NonInteractive) {
            throw "Global Codex MCP server '$Name' already exists. Rerun with -Force to replace it."
        }

        $answer = Read-Host "Global Codex MCP server '$Name' already exists. Replace it? [Y/n]"
        if ($answer -match '^(n|no)$') {
            Write-Host "Canceled. Existing global Codex MCP server '$Name' was left unchanged."
            exit 0
        }
    }

    $action = if ($exists) {
        "replace global configuration with streamable HTTP endpoint $Url"
    }
    else {
        "add global streamable HTTP endpoint $Url"
    }

    if ($PSCmdlet.ShouldProcess("Global Codex MCP server '$Name'", $action)) {
        $backupPath = Backup-CodexConfig -ConfigPath $codexConfigPath
        try {
            if ($exists) {
                & $codexCli mcp remove $Name
                if ($LASTEXITCODE -ne 0) {
                    throw "Failed to remove existing global Codex MCP server '$Name'."
                }
            }

            & $codexCli mcp add $Name --url $Url
            if ($LASTEXITCODE -ne 0) {
                throw "Failed to add global Codex MCP server '$Name'."
            }
        }
        catch {
            $operationError = $_
            if ($backupPath) {
                try {
                    Restore-CodexConfig -ConfigPath $codexConfigPath -BackupPath $backupPath
                }
                catch {
                    throw "Failed to restore Codex config from '$backupPath' after install failure. Original error: $operationError Restore error: $($_.Exception.Message)"
                }
            }

            throw $operationError
        }
    }

    if (-not $WhatIfPreference) {
        Write-Host ""
        Write-Host "Installed global Codex MCP server:"
        & $codexCli mcp get $Name
        if ($LASTEXITCODE -ne 0) {
            throw "Global Codex MCP server '$Name' was added, but verification with 'codex mcp get' failed."
        }
    }
}

if ([string]::IsNullOrWhiteSpace($ServerName)) {
    throw "ServerName cannot be empty."
}

$mcpUri = [uri]$Endpoint
if (-not $mcpUri.IsAbsoluteUri) {
    throw "Endpoint must be an absolute URL: $Endpoint"
}

Test-LoopbackEndpoint -McpEndpoint $mcpUri

if (-not $SkipHealthCheck) {
    try {
        Test-DbgflowHealth -McpEndpoint $mcpUri
    }
    catch {
        if ($RequireHealth) {
            throw
        }

        Write-Warning $_.Exception.Message
    }
}

if ($Global) {
    Install-GlobalMcpServer -Name $ServerName -Url $Endpoint
}
else {
    $projectConfigPath = Resolve-ProjectCodexConfigPath -Root $ProjectRoot
    Install-ProjectMcpServer -ConfigPath $projectConfigPath -Name $ServerName -Url $Endpoint
}

#requires -Version 5.1
[CmdletBinding()]
param(
    [string]$Version = 'latest',
    [string]$InstallDir = (Join-Path $env:LOCALAPPDATA 'Programs\Kodrik'),
    [switch]$AddToPath,
    [switch]$SkipConfigure
)
$ErrorActionPreference = 'Stop'

function ConvertTo-TomlString {
    param([string]$Value)
    # JSON string escapes are valid TOML basic-string escapes for these inputs.
    if ($Value -match '[\x00-\x1f\x7f]') { throw 'Control characters are not allowed in configuration values.' }
    return '"' + $Value.Replace('\', '\\').Replace('"', '\"') + '"'
}

function Save-KodrikConfig {
    param([string]$Path, [string]$Model, [string]$ApiKey, [string]$ApiBase, [string]$TelegramToken, [string]$OwnerId)
    if ([string]::IsNullOrWhiteSpace($Model) -or [string]::IsNullOrWhiteSpace($ApiKey)) { throw 'Model and API key are required.' }
    if (Test-Path -LiteralPath $Path) { throw 'Existing config will not be overwritten.' }
    if ($ApiBase) {
        $uri = $null
        if (![Uri]::TryCreate($ApiBase, [UriKind]::Absolute, [ref]$uri) -or $uri.Scheme -notin @('https', 'http') -or $uri.UserInfo) {
            throw 'API base must be an HTTP(S) URL without embedded credentials.'
        }
    }
    $text = "[gateway]`n`n[provider.primary]`ntype = `"openai`"`nmodel = $(ConvertTo-TomlString $Model)`napi_key = $(ConvertTo-TomlString $ApiKey)`n"
    if ($ApiBase) { $text += "api_base = $(ConvertTo-TomlString $ApiBase)`n" }
    if ($TelegramToken) {
        $id = [System.UInt64]0
        if (![System.UInt64]::TryParse($OwnerId, [ref]$id) -or $id -eq 0) { throw 'Telegram owner ID must be a positive integer.' }
        $text += "`n[gateway.telegram]`ntype = `"telegram`"`ntoken = $(ConvertTo-TomlString $TelegramToken)`n`n[owner]`ninterface = `"telegram`"`nuser_id = $id`n"
    }
    $parent = Split-Path -Parent $Path
    New-Item -ItemType Directory -Force -Path $parent | Out-Null
    $stage = Join-Path $parent ('.configure-' + [guid]::NewGuid().ToString())
    New-Item -ItemType Directory -Path $stage | Out-Null
    try {
        $acl = New-Object System.Security.AccessControl.DirectorySecurity
        $acl.SetAccessRuleProtection($true, $false)
        $sid = [Security.Principal.WindowsIdentity]::GetCurrent().User
        $acl.AddAccessRule((New-Object System.Security.AccessControl.FileSystemAccessRule(
            $sid, 'FullControl', 'ContainerInherit,ObjectInherit', 'None', 'Allow')))
        Set-Acl -LiteralPath $stage -AclObject $acl
        $file = Join-Path $stage 'config.toml'
        [IO.File]::WriteAllText($file, $text, (New-Object Text.UTF8Encoding($false)))
        $fileAcl = New-Object System.Security.AccessControl.FileSecurity
        $fileAcl.SetAccessRuleProtection($true, $false)
        $fileAcl.AddAccessRule((New-Object System.Security.AccessControl.FileSystemAccessRule($sid, 'FullControl', 'Allow')))
        Set-Acl -LiteralPath $file -AclObject $fileAcl
        # Move without overwrite: a concurrently created config must survive.
        [IO.File]::Move($file, $Path)
    } finally {
        Remove-Item -LiteralPath $stage -Recurse -Force -ErrorAction SilentlyContinue
    }
}

function Read-KodrikRequired {
    param([string]$Prompt, [switch]$Secret)
    do {
        if ($Secret) {
            $secure = Read-Host $Prompt -AsSecureString
            $pointer = [Runtime.InteropServices.Marshal]::SecureStringToBSTR($secure)
            try { $value = [Runtime.InteropServices.Marshal]::PtrToStringBSTR($pointer) }
            finally { [Runtime.InteropServices.Marshal]::ZeroFreeBSTR($pointer); $secure.Dispose() }
        } else { $value = (Read-Host $Prompt).Trim() }
        if ([string]::IsNullOrWhiteSpace($value)) { Write-Host 'A value is required.' }
    } while ([string]::IsNullOrWhiteSpace($value))
    return $value
}

function Initialize-KodrikConfig {
    $path = Join-Path $HOME '.kodrik\config.toml'
    if (Test-Path -LiteralPath $path) {
        Write-Host "Keeping existing configuration: $path"
        return
    }
    Write-Host 'Configure a provider supporting OpenAI Responses API and tool calling.'
    $model = Read-KodrikRequired 'Model identifier'
    $key = Read-KodrikRequired 'API key (hidden)' -Secret
    $apiBase = (Read-Host 'API base URL (Enter for OpenAI default)').Trim()
    if ($apiBase -match '^http://') { Write-Warning 'HTTP sends API credentials without TLS; use only a trusted local endpoint.' }
    $token = ''; $owner = ''
    if ((Read-Host 'Configure Telegram? [y/N]') -match '^(y|yes)$') {
        $token = Read-KodrikRequired 'Telegram bot token (hidden)' -Secret
        do { $owner = Read-KodrikRequired 'Your numeric Telegram user ID (not username)' }
        while ($owner -notmatch '^[1-9][0-9]*$')
    }
    try { Save-KodrikConfig $path $model $key $apiBase $token $owner }
    finally { $key = $null; $token = $null }
    Write-Host "Configuration saved privately: $path"
    Write-Host 'Credentials were saved locally, not checked against the provider.'
}

function Install-KodrikPackage {
    param([string]$Archive, [string]$ExpectedHash, [string]$Version, [string]$Destination)
    $Archive = [IO.Path]::GetFullPath($Archive)
    $Destination = [IO.Path]::GetFullPath($Destination)
    if ((Get-FileHash -LiteralPath $Archive -Algorithm SHA256).Hash -ine $ExpectedHash) {
        throw 'SHA256 mismatch; installation has not been changed.'
    }
    New-Item -ItemType Directory -Force -Path $Destination | Out-Null
    $stage = Join-Path $Destination ('.install-' + [guid]::NewGuid().ToString())
    New-Item -ItemType Directory -Path $stage | Out-Null
    try {
        # Only the current user can alter executable staging files before use.
        $acl = New-Object System.Security.AccessControl.DirectorySecurity
        $acl.SetAccessRuleProtection($true, $false)
        $sid = [Security.Principal.WindowsIdentity]::GetCurrent().User
        $acl.AddAccessRule((New-Object System.Security.AccessControl.FileSystemAccessRule(
            $sid, 'FullControl', 'ContainerInherit,ObjectInherit', 'None', 'Allow')))
        Set-Acl -LiteralPath $stage -AclObject $acl
        Add-Type -AssemblyName System.IO.Compression.FileSystem
        $zip = [IO.Compression.ZipFile]::OpenRead($Archive)
        try {
            $entries = @($zip.Entries | Where-Object { $_.FullName -ceq 'kodrik.exe' })
            if ($entries.Count -ne 1 -or $entries[0].Length -gt 512MB) { throw 'Invalid release archive' }
            $candidate = Join-Path $stage 'kodrik.exe'
            # Extract only the known binary, never arbitrary ZIP paths.
            [IO.Compression.ZipFileExtensions]::ExtractToFile($entries[0], $candidate)
        } finally { $zip.Dispose() }
        $reported = & $candidate --version
        if ($LASTEXITCODE -ne 0 -or "$reported".Trim() -ne "kodrik $Version") {
            throw 'Downloaded executable failed its version check; installation has not been changed.'
        }
        $target = Join-Path $Destination 'kodrik.exe'
        if (Test-Path -LiteralPath $target) {
            # A running/locked executable causes a safe failure, not process termination.
            # Windows PowerShell 5.1 marshals $null to an invalid empty string here.
            $backup = Join-Path $stage 'previous-kodrik.exe'
            [IO.File]::Replace($candidate, $target, $backup)
        } else {
            [IO.File]::Move($candidate, $target)
        }
    } finally {
        Remove-Item -LiteralPath $stage -Recurse -Force -ErrorAction SilentlyContinue
    }
}

function Install-Kodrik {
    if ($env:OS -ne 'Windows_NT') { throw 'This installer requires Windows.' }
    $arch = $env:PROCESSOR_ARCHITEW6432
    if (!$arch) { $arch = $env:PROCESSOR_ARCHITECTURE }
    $target = switch ($arch.ToUpperInvariant()) {
        'ARM64' { 'aarch64-pc-windows-msvc' }
        'AMD64' { 'x86_64-pc-windows-msvc' }
        default { throw "Unsupported Windows architecture: $arch" }
    }
    if ($Version -ne 'latest' -and $Version -notmatch '^v?\d+\.\d+\.\d+$') {
        throw 'Version must be latest or a stable version such as 0.5.0.'
    }
    [Net.ServicePointManager]::SecurityProtocol = [Net.ServicePointManager]::SecurityProtocol -bor [Net.SecurityProtocolType]::Tls12
    $api = 'https://api.github.com/repos/suinly/kodrik/releases/'
    $endpoint = if ($Version -eq 'latest') { 'latest' } else { 'tags/v' + $Version.TrimStart('v') }
    $release = Invoke-RestMethod -Uri ($api + $endpoint) -Headers @{ 'User-Agent' = 'Kodrik-Windows-Installer' }
    if ($release.draft -or $release.prerelease -or $release.tag_name -notmatch '^v\d+\.\d+\.\d+$') {
        throw 'A published stable release is required.'
    }
    $resolvedVersion = $release.tag_name.Substring(1)
    $name = "kodrik-$resolvedVersion-$target.zip"
    foreach ($asset in @($name, 'SHA256SUMS')) {
        if (@($release.assets | Where-Object { $_.name -ceq $asset }).Count -ne 1) {
            throw "Release is missing $asset"
        }
    }
    $temp = Join-Path ([IO.Path]::GetTempPath()) ('kodrik-download-' + [guid]::NewGuid().ToString())
    New-Item -ItemType Directory -Path $temp | Out-Null
    try {
        $base = "https://github.com/suinly/kodrik/releases/download/$($release.tag_name)"
        Invoke-WebRequest -UseBasicParsing -Uri "$base/SHA256SUMS" -OutFile "$temp/SHA256SUMS"
        $matches = @(Get-Content "$temp/SHA256SUMS" | Where-Object { $_ -match ('^[a-fA-F0-9]{64}\s+\*?' + [regex]::Escape($name) + '$') })
        if ($matches.Count -ne 1) { throw 'Missing or ambiguous archive checksum' }
        $expected = ($matches[0] -split '\s+')[0]
        Invoke-WebRequest -UseBasicParsing -Uri "$base/$name" -OutFile "$temp/package.zip"
        Install-KodrikPackage "$temp/package.zip" $expected $resolvedVersion $InstallDir
    } finally {
        Remove-Item -LiteralPath $temp -Recurse -Force -ErrorAction SilentlyContinue
    }
    if ($AddToPath) {
        $path = [string][Environment]::GetEnvironmentVariable('Path', 'User')
        if (@($path -split ';') -notcontains $InstallDir) {
            [Environment]::SetEnvironmentVariable('Path', ($path.TrimEnd(';') + ';' + $InstallDir).TrimStart(';'), 'User')
        }
        if (@($env:Path -split ';') -notcontains $InstallDir) { $env:Path += ";$InstallDir" }
    }
    Write-Host "Installed Kodrik $resolvedVersion ($target) in $InstallDir"
    if (!$SkipConfigure) { Initialize-KodrikConfig }
    Write-Host "Run terminal UI: & '$InstallDir\kodrik.exe'"
    Write-Host "Run gateway (Telegram/web): & '$InstallDir\kodrik.exe' gateway"
    Write-Host 'Existing configuration and data were not overwritten. No service or autostart was installed.'
}

if ($MyInvocation.InvocationName -ne '.') { Install-Kodrik }

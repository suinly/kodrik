#requires -Version 5.1
[CmdletBinding()]
param(
    [string]$Version = 'latest',
    [string]$InstallDir = (Join-Path $env:LOCALAPPDATA 'Programs\Kodrik'),
    [switch]$AddToPath
)
$ErrorActionPreference = 'Stop'

function Install-KodrikPackage {
    param([string]$Archive, [string]$ExpectedHash, [string]$Version, [string]$Destination)
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
            [IO.File]::Replace($candidate, $target, $null)
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
    Write-Host "Run: & '$InstallDir\kodrik.exe' --help"
    Write-Host 'Configuration and data in $HOME\.kodrik were not changed. No service or autostart was installed.'
}

if ($MyInvocation.InvocationName -ne '.') { Install-Kodrik }

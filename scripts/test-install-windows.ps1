param([Parameter(Mandatory = $true)][string]$Binary)
$ErrorActionPreference = 'Stop'
. "$PSScriptRoot/install-windows.ps1"
$root = Join-Path ([IO.Path]::GetTempPath()) ([guid]::NewGuid().ToString())
try {
    New-Item -ItemType Directory "$root/package" -Force | Out-Null
    Copy-Item -LiteralPath $Binary -Destination "$root/package/kodrik.exe"
    $version = (& $Binary --version) -replace '^kodrik ', ''
    $archive = "$root/package.zip"
    Compress-Archive -Path "$root/package/kodrik.exe" -DestinationPath $archive
    $hash = (Get-FileHash -LiteralPath $archive -Algorithm SHA256).Hash
    $destination = "$root/installation with spaces"
    Install-KodrikPackage $archive $hash $version $destination
    $installed = Join-Path $destination 'kodrik.exe'
    if ((Get-FileHash $installed).Hash -ne (Get-FileHash $Binary).Hash) { throw 'Wrong installed binary' }
    Set-Content "$destination/config.toml" 'preserve existing settings'
    Install-KodrikPackage $archive $hash $version $destination
    if ((Get-Content "$destination/config.toml") -ne 'preserve existing settings') { throw 'Settings overwritten' }
    $rejected = $false
    try { Install-KodrikPackage $archive ('0' * 64) $version $destination } catch { $rejected = $true }
    if (!$rejected) { throw 'Invalid checksum accepted' }
    if ((Get-FileHash $installed).Hash -ne (Get-FileHash $Binary).Hash) { throw 'Failed update changed installation' }
    $rejected = $false
    try { Install-KodrikPackage $archive $hash '999.0.0' $destination } catch { $rejected = $true }
    if (!$rejected) { throw 'Wrong binary version accepted' }
    if ((Get-FileHash $installed).Hash -ne (Get-FileHash $Binary).Hash) { throw 'Wrong version replaced installation' }
    Write-Host 'PASS: install, upgrade, paths with spaces, settings preservation, checksum and version rejection'
} finally {
    Remove-Item -LiteralPath $root -Recurse -Force -ErrorAction SilentlyContinue
}

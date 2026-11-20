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
    if (@(Get-ChildItem -LiteralPath $destination -Force | Where-Object { $_.Name -like '.install-*' }).Count -ne 0) { throw 'Update backup was not cleaned up' }
    $rejected = $false
    try { Install-KodrikPackage $archive ('0' * 64) $version $destination } catch { $rejected = $true }
    if (!$rejected) { throw 'Invalid checksum accepted' }
    if ((Get-FileHash $installed).Hash -ne (Get-FileHash $Binary).Hash) { throw 'Failed update changed installation' }
    $rejected = $false
    try { Install-KodrikPackage $archive $hash '999.0.0' $destination } catch { $rejected = $true }
    if (!$rejected) { throw 'Wrong binary version accepted' }
    if ((Get-FileHash $installed).Hash -ne (Get-FileHash $Binary).Hash) { throw 'Wrong version replaced installation' }
    $config = "$root/state/config.toml"
    $secret = 'test"key\with-special-characters'
    Save-KodrikConfig $config 'test-model' $secret 'https://example.com/v1' '' ''
    $text = [IO.File]::ReadAllText($config)
    if (!$text.Contains('api_key = "test\"key\\with-special-characters"')) { throw 'TOML escaping failed' }
    if (!$text.Contains('[gateway]') -or !$text.Contains('[provider.primary]')) { throw 'Required config tables missing' }
    $acl = Get-Acl -LiteralPath $config
    if (!$acl.AreAccessRulesProtected) { throw 'Config inherits public access' }
    if (@($acl.Access).Count -ne 1 -or $acl.Access[0].IdentityReference.Translate([Security.Principal.SecurityIdentifier]).Value -ne [Security.Principal.WindowsIdentity]::GetCurrent().User.Value) { throw 'Config has unexpected trustee' }
    $rejected = $false
    try { Save-KodrikConfig $config 'other' 'other' '' '' '' } catch { $rejected = $true }
    if (!$rejected -or [IO.File]::ReadAllText($config) -ne $text) { throw 'Existing config overwritten' }
    Save-KodrikConfig "$root/telegram.toml" 'test' 'key' '' 'bot-token' '123456789'
    if (![IO.File]::ReadAllText("$root/telegram.toml").Contains('user_id = 123456789')) { throw 'Telegram owner missing' }
    Write-Host 'PASS: install, upgrade, paths with spaces, settings preservation, checksum and version rejection'
} finally {
    Remove-Item -LiteralPath $root -Recurse -Force -ErrorAction SilentlyContinue
}

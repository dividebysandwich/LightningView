$ErrorActionPreference = 'Stop'

$packageName  = 'lightningview'
$version      = '@@VERSION@@'
$url64        = "https://github.com/dividebysandwich/LightningView/releases/download/v$version/lightningview-x86_64-pc-windows-msvc-v$version.zip"
$toolsDir     = "$(Split-Path -parent $MyInvocation.MyCommand.Definition)"

$packageArgs = @{
  packageName    = $packageName
  unzipLocation  = $toolsDir
  url64bit       = $url64
  # softwareName matches the display name used in Add/Remove Programs.
  softwareName   = 'LightningView*'
  # Checksum is verified by Chocolatey at install time. The CI release job
  # will replace this placeholder with the real SHA-256 of the published zip.
  checksum64     = '@@CHECKSUM64@@'
  checksumType64 = 'sha256'
}

Install-ChocolateyZipPackage @packageArgs

# Drop a shim so `lightningview` is on PATH after install.
$exePath = Join-Path $toolsDir 'lightningview.exe'
if (Test-Path $exePath) {
  Install-BinFile -Name 'lightningview' -Path $exePath
}

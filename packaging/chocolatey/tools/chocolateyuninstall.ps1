$ErrorActionPreference = 'Stop'

$toolsDir = "$(Split-Path -parent $MyInvocation.MyCommand.Definition)"
$exePath  = Join-Path $toolsDir 'lightningview.exe'

if (Test-Path $exePath) {
  Uninstall-BinFile -Name 'lightningview' -Path $exePath
}

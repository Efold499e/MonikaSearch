# Create a Start Menu shortcut for this (portable) copy of MonikaSearch.
# Run from the folder that contains monikasearch.exe:
#   powershell -NoProfile -ExecutionPolicy Bypass -File add-startmenu-shortcut.ps1
$ErrorActionPreference = 'Stop'

$exe = Join-Path $PSScriptRoot 'monikasearch.exe'
if (-not (Test-Path $exe)) {
  Write-Error 'monikasearch.exe not found next to this script'
  exit 1
}

$ws = New-Object -ComObject WScript.Shell
$dir = [Environment]::GetFolderPath('StartMenu') + '\Programs'
$lnk = $ws.CreateShortcut($dir + '\MonikaSearch.lnk')
$lnk.TargetPath = $exe
$lnk.WorkingDirectory = $PSScriptRoot
$lnk.IconLocation = "$exe,0"
$lnk.Description = 'MonikaSearch - Alt+E / Alt+Space'
$lnk.Save()
Write-Output "created: $dir\MonikaSearch.lnk"

# shell context-menu extension helper
# Usage:
#   shellex.ps1 list              - list third-party context menu handlers
#   shellex.ps1 block-all         - block every third-party handler (Backup registry export first)
#   shellex.ps1 block <CLSID>     - block one handler
#   shellex.ps1 unblock-all       - remove every block we added
#   shellex.ps1 status            - show current blocked list
param(
  [Parameter(Position = 0)][string]$Action = 'list',
  [Parameter(Position = 1)][string]$Arg
)
$ErrorActionPreference = 'SilentlyContinue'
$blockedKey = 'HKLM:\SOFTWARE\Microsoft\Windows\CurrentVersion\Shell Extensions\Blocked'
$approvedKey = 'HKLM:\SOFTWARE\Microsoft\Windows\CurrentVersion\Shell Extensions\Approved'

function Get-HandlerClsids {
  $keys = @(
    'Registry::HKEY_CLASSES_ROOT\*\shellex\ContextMenuHandlers',
    'Registry::HKEY_CLASSES_ROOT\Directory\shellex\ContextMenuHandlers',
    'Registry::HKEY_CLASSES_ROOT\Folder\shellex\ContextMenuHandlers',
    'Registry::HKEY_CLASSES_ROOT\Drive\shellex\ContextMenuHandlers',
    'Registry::HKEY_CLASSES_ROOT\AllFilesystemObjects\shellex\ContextMenuHandlers',
    'Registry::HKEY_CLASSES_ROOT\Directory\Background\shellex\ContextMenuHandlers'
  )
  $set = @{}
  foreach ($k in $keys) {
    Get-ChildItem $k | ForEach-Object {
      $v = (Get-ItemProperty $_.PSPath).'(default)'
      if ($v -match '^\{[0-9A-Fa-f\-]+\}$') { $set[$v.ToUpper()] = $_.PSChildName }
    }
  }
  return $set
}

function Get-Dll([string]$clsid) {
  $k = "Registry::HKEY_CLASSES_ROOT\CLSID\$clsid\InprocServer32"
  if (Test-Path $k) { return (Get-ItemProperty $k).'(default)' }
  return ''
}

if ($Action -eq 'list') {
  $approved = Get-ItemProperty $approvedKey
  $set = Get-HandlerClsids
  foreach ($clsid in $set.Keys) {
    $dll = Get-Dll $clsid
    if ($dll -and $dll.ToLower().Contains('\windows\')) { continue }
    $desc = ''
    $p = $approved.PSObject.Properties[$clsid]
    if ($p) { $desc = $p.Value }
    Write-Output ("{0}  {1}  {2}" -f $clsid, $desc, $dll)
  }
  exit 0
}

if ($Action -eq 'block-all') {
  if (-not (Test-Path $blockedKey)) { New-Item $blockedKey -Force | Out-Null }
  $set = Get-HandlerClsids
  foreach ($clsid in $set.Keys) {
    $dll = Get-Dll $clsid
    if ($dll -and $dll.ToLower().Contains('\windows\')) { continue }
    New-ItemProperty -Path $blockedKey -Name $clsid -Value 'MonikaSearch bisect' -PropertyType String -Force | Out-Null
    Write-Output "blocked $clsid"
  }
  exit 0
}

if ($Action -eq 'block') {
  if (-not (Test-Path $blockedKey)) { New-Item $blockedKey -Force | Out-Null }
  New-ItemProperty -Path $blockedKey -Name $Arg.ToUpper() -Value 'MonikaSearch bisect' -PropertyType String -Force | Out-Null
  Write-Output "blocked $Arg"
  exit 0
}

if ($Action -eq 'unblock-all') {
  if (Test-Path $blockedKey) {
    Get-ItemProperty $blockedKey | Out-Null
    $props = (Get-Item $blockedKey).Property
    foreach ($p in $props) {
      Remove-ItemProperty -Path $blockedKey -Name $p -Force
      Write-Output "unblocked $p"
    }
  }
  exit 0
}

if ($Action -eq 'status') {
  if (Test-Path $blockedKey) {
    foreach ($p in (Get-Item $blockedKey).Property) { Write-Output $p }
  }
  exit 0
}

Write-Output "unknown action"
exit 1

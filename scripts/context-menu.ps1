# Register MonikaSearch in the Windows Explorer context menu
# ("用 MonikaSearch 搜索..." on folders and folder backgrounds).
# Usage:
#   powershell -NoProfile -ExecutionPolicy Bypass -File context-menu.ps1          # install
#   powershell -NoProfile -ExecutionPolicy Bypass -File context-menu.ps1 -Remove  # remove
# The script locates monikasearch.exe next to itself (portable/installed layout)
# or in <repo>\target\release (repo checkout). Elevated sessions write HKLM,
# otherwise HKCU (per-user, no admin needed). Keep as UTF-8 with BOM (PS 5.1).
param([switch]$Remove = $false)
$ErrorActionPreference = 'Stop'

$candidates = @(
  (Join-Path $PSScriptRoot 'monikasearch.exe'),
  (Join-Path (Split-Path -Parent $PSScriptRoot) 'target\release\monikasearch.exe')
)
$exe = $candidates | Where-Object { Test-Path $_ } | Select-Object -First 1
if (-not $exe -and -not $Remove) {
  throw 'monikasearch.exe not found next to this script or in target\release'
}

$hives = @('HKLM:\SOFTWARE\Classes', 'HKCU:\Software\Classes')
if (-not $Remove) {
  $elevated = ([Security.Principal.WindowsPrincipal][Security.Principal.WindowsIdentity]::GetCurrent()
  ).IsInRole([Security.Principal.WindowsBuiltInRole]::Administrator)
  $hives = @($(if ($elevated) { 'HKLM:\SOFTWARE\Classes' } else { 'HKCU:\Software\Classes' }))
}

$entries = @(
  @{ key = 'Directory\Background\shell\用 MonikaSearch 搜索...'; arg = '%V' },
  @{ key = 'Folder\shell\用 MonikaSearch 搜索...';               arg = '%1' }
)

foreach ($hive in $hives) {
  foreach ($e in $entries) {
    $path = Join-Path $hive $e.key
    if ($Remove) {
      if (Test-Path -LiteralPath $path) {
        Remove-Item -LiteralPath $path -Recurse -Force
        Write-Output "removed: $path"
      }
      continue
    }
    New-Item -Path (Join-Path $path 'command') -Force | Out-Null
    Set-Item -LiteralPath $path -Value '用 MonikaSearch 搜索...'
    Set-ItemProperty -LiteralPath $path -Name 'Icon' -Value "$exe,0"
    Set-Item -LiteralPath (Join-Path $path 'command') -Value ('"' + $exe + '" "' + $e.arg + '"')
    Write-Output "installed: $path"
  }
}
if (-not $Remove) { Write-Output "exe: $exe" }

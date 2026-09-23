# 列出本机第三方 shell 右键菜单扩展（CLSID + DLL 路径 + 描述）
# 非微软路径（不含 \windows\）视为第三方
$roots = @(
  'Registry::HKEY_CLASSES_ROOT\*\shellex\ContextMenuHandlers',
  'Registry::HKEY_CLASSES_ROOT\Directory\shellex\ContextMenuHandlers',
  'Registry::HKEY_CLASSES_ROOT\Folder\shellex\ContextMenuHandlers',
  'Registry::HKEY_CLASSES_ROOT\Directory\Background\shellex\ContextMenuHandlers',
  'Registry::HKEY_CLASSES_ROOT\AllFilesystemObjects\shellex\ContextMenuHandlers',
  'Registry::HKEY_CLASSES_ROOT\Drive\shellex\ContextMenuHandlers'
)
$clsids = New-Object 'System.Collections.Generic.HashSet[string]'
foreach ($r in $roots) {
  if (-not (Test-Path $r)) { continue }
  foreach ($h in (Get-ChildItem $r -ErrorAction SilentlyContinue)) {
    $v = (Get-ItemProperty $h.PSPath -ErrorAction SilentlyContinue).'(default)'
    if ($v -and $v -match '^\{[0-9A-Fa-f\-]+\}$') { [void]$clsids.Add($v.ToUpper()) }
  }
}
$approved = Get-ItemProperty 'HKLM:\SOFTWARE\Microsoft\Windows\CurrentVersion\Shell Extensions\Approved' -ErrorAction SilentlyContinue
foreach ($p in $clsids) {
  $dll = ''
  $k = "Registry::HKEY_CLASSES_ROOT\CLSID\$p\InprocServer32"
  if (Test-Path $k) { $dll = (Get-ItemProperty $k -ErrorAction SilentlyContinue).'(default)' }
  $desc = ''
  if ($approved) { $prop = $approved.PSObject.Properties[$p]; if ($prop) { $desc = $prop.Value } }
  $isMs = $dll -and ($dll.ToLower() -match '\\windows\\')
  $tag = 'MS'
  if (-not $isMs) { $tag = 'THIRD' }
  Write-Output ("{0}`t{1}`t{2}`t{3}" -f $tag, $p, $desc, $dll)
}

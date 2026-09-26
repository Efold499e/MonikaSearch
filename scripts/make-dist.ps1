# Build distributable packages for MonikaSearch.
# Usage (from anywhere):
#   powershell -NoProfile -ExecutionPolicy Bypass -File scripts\make-dist.ps1 [-SkipNsis]
# Outputs:
#   dist\MonikaSearch-<ver>-portable.zip     portable zip (exe + docs + shortcut script)
#   target\release\bundle\nsis\*.exe         NSIS installer (needs node/npm + network)
# Notes:
#   - ctxhost.exe MUST ship next to monikasearch.exe (native context menu host).
#   - Keep this file pure ASCII (PS 5.1 misreads UTF-8 without BOM).
param([switch]$SkipNsis = $false)
$ErrorActionPreference = 'Stop'

$root = Split-Path -Parent $PSScriptRoot
Set-Location $root

# 0. satisfy tauri-build's resources existence check before the first build
New-Item -ItemType Directory -Force app\binaries | Out-Null
if (-not (Test-Path app\binaries\ctxhost.exe) -and (Test-Path target\release\ctxhost.exe)) {
  Copy-Item target\release\ctxhost.exe app\binaries\ctxhost.exe
}

# 1. build. ep-mcp.exe is usually locked while an MCP client (e.g. ZCode) is
#    connected, so fall back to an app-only build and reuse the existing ep-mcp.
cargo build --release --workspace
if ($LASTEXITCODE -ne 0) {
  Write-Warning 'workspace build failed (ep-mcp.exe may be locked by a running MCP client); falling back to app-only build'
  cargo build --release -p monikasearch
  if ($LASTEXITCODE -ne 0) { throw 'cargo build failed' }
}

$ver = (Get-Content app\tauri.conf.json -Raw | ConvertFrom-Json).version
Write-Output "version: $ver"

# 2. refresh the staged ctxhost used by the tauri bundle (placeholder -> real one)
Copy-Item target\release\ctxhost.exe app\binaries\ctxhost.exe -Force

# 3. portable zip
$stage = "dist\MonikaSearch-$ver-portable"
if (Test-Path $stage) { Remove-Item $stage -Recurse -Force }
New-Item -ItemType Directory -Force $stage | Out-Null
Copy-Item target\release\monikasearch.exe $stage
Copy-Item target\release\ctxhost.exe $stage
if (Test-Path target\release\ep-mcp.exe) { Copy-Item target\release\ep-mcp.exe $stage }
Copy-Item LICENSE $stage
Copy-Item PRIVACY.md $stage
Copy-Item README.md $stage
Copy-Item scripts\add-startmenu-shortcut.ps1 $stage
Copy-Item scripts\context-menu.ps1 $stage
Copy-Item scripts\install-context-menu.cmd $stage
Copy-Item scripts\remove-context-menu.cmd $stage
Compress-Archive -Path $stage -DestinationPath "dist\MonikaSearch-$ver-portable.zip" -Force
Write-Output "portable zip: dist\MonikaSearch-$ver-portable.zip"

# 4. NSIS installer (best effort; the portable zip is the guaranteed deliverable)
if (-not $SkipNsis) {
  Push-Location app
  npx --yes "@tauri-apps/cli@^2" build
  Pop-Location
  if ($LASTEXITCODE -eq 0) {
    Write-Output "nsis installer: target\release\bundle\nsis\"
  } else {
    Write-Warning 'NSIS build failed; the portable zip is still valid'
  }
}

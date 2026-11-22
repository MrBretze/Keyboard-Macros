# Copies the plugin into OpenDeck's plugin folder on Windows.
# Usage:  powershell -ExecutionPolicy Bypass -File install.ps1 [-StreamDeck]
#   -StreamDeck  installs into the Elgato Stream Deck software instead of OpenDeck.
param([switch]$StreamDeck)
$ErrorActionPreference = "Stop"

$name = "com.mrbretzel.keymacro.sdPlugin"
$src = Join-Path $PSScriptRoot $name
if (-not (Test-Path (Join-Path $src "plugin.exe"))) {
    Write-Error "plugin.exe is missing: run build_windows.bat first."
}

if ($StreamDeck) {
    $dest = Join-Path $env:APPDATA "Elgato\StreamDeck\Plugins"
} else {
    $dest = Join-Path $env:APPDATA "opendeck\plugins"
}
New-Item -ItemType Directory -Force -Path $dest | Out-Null
$target = Join-Path $dest $name

if (Test-Path $target) { Remove-Item -Recurse -Force $target }
Copy-Item -Recurse $src $target
Get-ChildItem -Path $target -Recurse -Include "__pycache__", "plugin.log" | Remove-Item -Recurse -Force
if ($StreamDeck) {
    # The Elgato software only reads localization files (en.json, fr.json...) from the plugin root.
    Copy-Item (Join-Path $target "lang\*.json") $target
}

Write-Host "Installed to: $target"
Write-Host "Restart OpenDeck (or the Stream Deck software) to load the plugin."

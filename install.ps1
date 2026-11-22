# Builds the plugin from source and installs it on Windows.
# Requires Rust (https://rustup.rs) with the MSVC toolchain. Users who just want the
# plugin can install the release zip from OpenDeck instead.
# Usage:  powershell -ExecutionPolicy Bypass -File install.ps1 [-StreamDeck]
#   -StreamDeck  installs into the Elgato Stream Deck software instead of OpenDeck.
param([switch]$StreamDeck)
$ErrorActionPreference = "Stop"
Set-Location $PSScriptRoot

$name = "com.mrbretzel.keymacro.sdPlugin"
cargo build --release --locked
if ($LASTEXITCODE -ne 0) { Write-Error "Build failed." }
Copy-Item "target\release\keymacro.exe" (Join-Path $name "keymacro-windows-x86_64.exe") -Force

if ($StreamDeck) {
    $dest = Join-Path $env:APPDATA "Elgato\StreamDeck\Plugins"
} else {
    $dest = Join-Path $env:APPDATA "opendeck\plugins"
}
New-Item -ItemType Directory -Force -Path $dest | Out-Null
$target = Join-Path $dest $name

if (Test-Path $target) { Remove-Item -Recurse -Force $target }
Copy-Item -Recurse $name $target
Get-ChildItem -Path $target -Recurse -Include "plugin.log", "keymacro-linux-*" | Remove-Item -Force
if ($StreamDeck) {
    # The Elgato software only reads localization files (en.json, fr.json...) from the plugin root.
    Copy-Item (Join-Path $target "lang\*.json") $target
}

Write-Host "Installed to: $target"
Write-Host "Restart OpenDeck (or the Stream Deck software) to load the plugin."

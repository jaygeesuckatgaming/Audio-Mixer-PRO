# Installs the plugin into the local Stream Deck app for development.
#
#   powershell -ExecutionPolicy Bypass -File streamdeck\install-dev.ps1
#   powershell -ExecutionPolicy Bypass -File streamdeck\install-dev.ps1 -Restart
#
# This copies the .sdPlugin folder into %APPDATA%\Elgato\StreamDeck\Plugins.
# Stream Deck loads unpacked plugins from there, but only picks up changes when
# the app (or just the plugin) restarts: use -Restart, or the app's
# Settings > Plugins page.

param(
    [switch]$Restart
)

$ErrorActionPreference = "Stop"

$root = Split-Path -Parent $MyInvocation.MyCommand.Path
$plugin = Join-Path $root "com.jaygee.audiomixerpro.sdPlugin"
$target = Join-Path $env:APPDATA "Elgato\StreamDeck\Plugins\com.jaygee.audiomixerpro.sdPlugin"

if (-not (Test-Path (Join-Path $plugin "manifest.json"))) {
    throw "manifest.json missing - run this from the repository"
}

# Stream Deck keeps the plugin's Node process alive, so the folder cannot be
# replaced while it runs. Stop the plugin processes first (the app respawns
# them on restart).
Get-Process node -ErrorAction SilentlyContinue |
    Where-Object { $_.Path -and $_.Path -like "*Elgato*" } |
    Stop-Process -Force -ErrorAction SilentlyContinue

Remove-Item $target -Recurse -Force -ErrorAction SilentlyContinue
Copy-Item $plugin $target -Recurse -Force
Write-Host "installed to $target"

if ($Restart) {
    Get-Process StreamDeck -ErrorAction SilentlyContinue | Stop-Process -Force
    Start-Sleep -Milliseconds 700
    $exe = Join-Path $env:ProgramFiles "Elgato\StreamDeck\StreamDeck.exe"
    if (Test-Path $exe) {
        Start-Process $exe
        Write-Host "Stream Deck restarted"
    } else {
        Write-Host "Stream Deck.exe not found; start it yourself"
    }
} else {
    Write-Host "restart the Stream Deck app to load it (or rerun with -Restart)"
}

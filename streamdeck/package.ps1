# Builds the installer file: com.jaygee.audiomixerpro.streamDeckPlugin
#
#   powershell -ExecutionPolicy Bypass -File streamdeck\package.ps1
#
# The result lands in the repository's dist\ folder. Double-clicking it installs
# the plugin through the Stream Deck app; nothing else is required (the app
# ships its own Node runtime, so end users do not install Node).

$ErrorActionPreference = "Stop"

$root = Split-Path -Parent $MyInvocation.MyCommand.Path
$repo = Split-Path -Parent $root
$plugin = Join-Path $root "com.jaygee.audiomixerpro.sdPlugin"
$dist = Join-Path $repo "dist"
$zip = Join-Path $dist "com.jaygee.audiomixerpro.zip"
$out = Join-Path $dist "com.jaygee.audiomixerpro.streamDeckPlugin"

if (-not (Test-Path (Join-Path $plugin "manifest.json"))) {
    throw "manifest.json missing - run this from the repository"
}
if (-not (Test-Path $dist)) {
    New-Item -ItemType Directory -Force -Path $dist | Out-Null
}

Remove-Item $zip, $out -Force -ErrorAction SilentlyContinue
# The archive must contain the .sdPlugin folder itself, which is what the
# Stream Deck installer expects.
Compress-Archive -Path $plugin -DestinationPath $zip -CompressionLevel Optimal
Move-Item $zip $out

$size = (Get-Item $out).Length
Write-Host "built $out ($size bytes)"

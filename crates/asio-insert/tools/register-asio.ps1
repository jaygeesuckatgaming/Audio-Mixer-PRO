#Requires -Version 5.1
<#
.SYNOPSIS
    Registers the Audio Mixer PRO ASIO insert device so a DAW can select it.

.DESCRIPTION
    Copies asio_insert.dll to %LOCALAPPDATA%\Audio Mixer PRO\asio and writes the
    two registry entries an ASIO host looks for. Needs administrator once; it
    self-elevates. Use -Unregister to remove both entries again.

    Waveform 14 and other 64-bit hosts only: this driver is x64 for now.

.PARAMETER Dll
    Path to asio_insert.dll. Defaults to the copy next to this script (the
    distribution layout), then the crate's release build
    (crates\asio-insert\target\release\asio_insert.dll).

.PARAMETER Name
    The name the host shows and the key written under HKLM\SOFTWARE\ASIO.

.PARAMETER Unregister
    Remove the device instead of installing it.

.EXAMPLE
    .\register-asio.ps1
.EXAMPLE
    .\register-asio.ps1 -Unregister
#>
[CmdletBinding()]
param(
    [string]$Dll,
    [string]$Name = "Audio Mixer PRO Insert",
    [switch]$Unregister,
    [switch]$NoElevate
)

$ErrorActionPreference = "Stop"
$clsid = "{B7F3C2A1-5D4E-4F0B-9C7A-3E1D2B6A8F90}"
$asioKey = "HKLM:\SOFTWARE\ASIO\$Name"
$clsidKey = "HKLM:\SOFTWARE\Classes\CLSID\$clsid"

function Test-Admin {
    $identity = [Security.Principal.WindowsIdentity]::GetCurrent()
    $principal = New-Object Security.Principal.WindowsPrincipal($identity)
    return $principal.IsInRole([Security.Principal.WindowsBuiltInRole]::Administrator)
}

if (-not (Test-Admin) -and -not $NoElevate) {
    $psArgs = @(
        "-NoProfile",
        "-ExecutionPolicy", "Bypass",
        "-File", ('"{0}"' -f $PSCommandPath)
    )
    if ($Dll) { $psArgs += @("-Dll", ('"{0}"' -f $Dll)) }
    if ($Unregister) { $psArgs += "-Unregister" }
    if ($Name -ne "Audio Mixer PRO Insert") { $psArgs += @("-Name", ('"{0}"' -f $Name)) }
    Write-Host "Asking for administrator rights (one prompt)..." -ForegroundColor Cyan
    Start-Process -FilePath "powershell.exe" -ArgumentList $psArgs -Verb RunAs | Out-Null
    return
}

if (-not (Test-Admin)) {
    throw "This script must run as administrator to write HKLM\SOFTWARE\ASIO."
}

if ($Unregister) {
    if (Test-Path $asioKey) {
        Remove-Item -Path $asioKey -Recurse -Force
        Write-Host "Removed $asioKey"
    } else {
        Write-Host "Not registered: $asioKey"
    }
    if (Test-Path $clsidKey) {
        Remove-Item -Path $clsidKey -Recurse -Force
        Write-Host "Removed $clsidKey"
    }
    Write-Host "Done. Restart the DAW if it was open." -ForegroundColor Green
    return
}

# ---- locate and copy the driver ------------------------------------------
if (-not $Dll) {
    # Distribution layout: the DLL lives next to this script. A source tree
    # layout is also supported so the same script works from the repo.
    $repoRoot = Resolve-Path (Join-Path $PSScriptRoot "..\..\..") -ErrorAction SilentlyContinue
    $candidates = @(
        (Join-Path $PSScriptRoot "asio_insert.dll"),
        (Join-Path $PSScriptRoot "..\crates\asio-insert\target\release\asio_insert.dll"),
        (Join-Path $PSScriptRoot "..\crates\asio-insert\target\diag\release\asio_insert.dll")
    )
    if ($repoRoot) {
        $candidates += @(
            (Join-Path $repoRoot "crates\asio-insert\target\release\asio_insert.dll"),
            (Join-Path $repoRoot "crates\asio-insert\target\diag\release\asio_insert.dll")
        )
    }
    $Dll = $candidates | Where-Object { Test-Path $_ } | Select-Object -First 1
    if (-not $Dll) {
        Write-Host "asio_insert.dll was not found. Build it first:" -ForegroundColor Yellow
        Write-Host "  cd crates\asio-insert"
        Write-Host "  cargo build --release"
        Write-Host "or pass -Dll <path to asio_insert.dll>."
        exit 1
    }
}
if (-not (Test-Path $Dll)) {
    throw "No such file: $Dll"
}

$targetDir = Join-Path $env:LOCALAPPDATA "Audio Mixer PRO\asio"
New-Item -ItemType Directory -Path $targetDir -Force | Out-Null
$target = Join-Path $targetDir "asio_insert.dll"
try {
    Copy-Item -Path $Dll -Destination $target -Force
} catch {
    throw "Could not copy the DLL (is the DAW still running with it loaded?): $($_.Exception.Message)"
}
$info = Get-Item $target
Write-Host ("Driver  : {0} ({1:N0} bytes, {2:yyyy-MM-dd HH:mm})" -f $target, $info.Length, $info.LastWriteTime)

# ---- registry ------------------------------------------------------------
New-Item -Path $asioKey -Force | Out-Null
Set-Item -Path $asioKey -Value $Name
New-ItemProperty -Path $asioKey -Name "CLSID" -Value $clsid -PropertyType String -Force | Out-Null
New-ItemProperty -Path $asioKey -Name "Description" -Value "Audio Mixer PRO insert (64-bit)" -PropertyType String -Force | Out-Null

New-Item -Path $clsidKey -Force | Out-Null
Set-Item -Path $clsidKey -Value $Name
New-Item -Path (Join-Path $clsidKey "InprocServer32") -Force | Out-Null
Set-Item -Path (Join-Path $clsidKey "InprocServer32") -Value $target
New-ItemProperty -Path (Join-Path $clsidKey "InprocServer32") -Name "ThreadingModel" -Value "Both" -PropertyType String -Force | Out-Null

# ---- verify --------------------------------------------------------------
$check = Get-ItemProperty -Path $asioKey
$server = (Get-ItemProperty -Path (Join-Path $clsidKey "InprocServer32")).'(default)'
Write-Host ""
Write-Host "Registered:" -ForegroundColor Green
Write-Host ("  {0}" -f $asioKey)
Write-Host ("    CLSID       : {0}" -f $check.CLSID)
Write-Host ("    Description : {0}" -f $check.Description)
Write-Host ("  {0}\InprocServer32" -f $clsidKey)
Write-Host ("    (default)   : {0}" -f $server)
Write-Host ""
Write-Host "Next: close the DAW, choose '$Name' as its ASIO device (48000 Hz, 512 samples)." -ForegroundColor Cyan
Write-Host "In the mixer, assign a strip to EXTERNAL INSERT (ASIO) pair 1 or 2."
Write-Host "Pair 1 is ASIO channels 1/2; pair 2 is channels 3/4. The DAW track must"
Write-Host "monitor (record-arm) its input, or it returns silence."
Write-Host "To remove it: .\register-asio.ps1 -Unregister"

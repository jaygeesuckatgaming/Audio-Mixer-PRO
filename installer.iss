; Audio Mixer PRO - Inno Setup script
; ---------------------------------------------------------------------------
; Build:
;   1. Stage the package first (exe + README/INSTALL/VERSION/CHANGELOG/
;      LICENSE/THIRD-PARTY-NOTICES + ASIO Insert + Virtual Cable Driver) in
;      PkgDir below.
;   2. Install Inno Setup 7 (https://jrsoftware.org/isdl.php; 6.3+ also works).
;   3. From this folder:
;        "C:\Program Files\Inno Setup 7\ISCC.exe" installer.iss
;      or override the version / source folder:
;        ISCC.exe /DMyAppVersion=1.0.0 /DPkgDir="..\distribution\Audio Mixer PRO 1.0" installer.iss
;
; Signing (optional but recommended for a public release):
;     signtool sign /fd SHA256 /tr http://timestamp.digicert.com /td SHA256 ^
;         /a "..\distribution\Audio Mixer PRO 1.0 Setup.exe"
;
; The kernel driver is deliberately NOT installed by this script. It is
; experimental and test-signed, needs `bcdedit /set testsigning on` and a
; reboot, so the user must follow INSTALL.txt and run the driver's own
; install.ps1 with elevation.
; ---------------------------------------------------------------------------

#define MyAppName "Audio Mixer PRO"
#define MyAppPublisher "JayGee"
#define MyAppExeName "audio_mixer_pro.exe"
#define MyAppURL "https://github.com/jaygeesuckatgaming/Audio-Mixer-PRO"

#ifndef MyAppVersion
  #define MyAppVersion "1.0.0"
#endif

; Folder holding the staged package (must contain "Audio Mixer PRO\audio_mixer_pro.exe").
#ifndef PkgDir
  #define PkgDir "..\distribution\Audio Mixer PRO Beta"
#endif

[Setup]
AppId={{7C1E4B2A-9F3D-4B77-8E51-2A6D9C0F5B31}
AppName={#MyAppName}
AppVersion={#MyAppVersion}
AppVerName={#MyAppName} {#MyAppVersion}
AppPublisher={#MyAppPublisher}
AppPublisherURL={#MyAppURL}
AppSupportURL={#MyAppURL}
AppUpdatesURL={#MyAppURL}/releases
VersionInfoVersion={#MyAppVersion}.0
VersionInfoCompany={#MyAppPublisher}
VersionInfoDescription={#MyAppName} setup
VersionInfoProductName={#MyAppName}
VersionInfoProductVersion={#MyAppVersion}
VersionInfoCopyright=Copyright (c) 2026 JayGee
DefaultDirName={autopf}\{#MyAppName}
DefaultGroupName={#MyAppName}
DisableProgramGroupPage=yes
LicenseFile={#PkgDir}\LICENSE.txt
OutputDir=..\distribution
OutputBaseFilename={#MyAppName} {#MyAppVersion} Setup
SetupIconFile=icons\Icon_2_48_48_1.ico
UninstallDisplayIcon={app}\{#MyAppExeName}
UninstallDisplayName={#MyAppName}
WizardStyle=modern
Compression=lzma2/max
SolidCompression=yes
PrivilegesRequired=admin
PrivilegesRequiredOverridesAllowed=dialog
ArchitecturesAllowed=x64compatible
ArchitecturesInstallIn64BitMode=x64compatible
MinVersion=10.0
CloseApplications=yes
RestartApplications=no
AllowNoIcons=yes
ChangesAssociations=no
; Unsigned builds trip SmartScreen; sign the setup afterwards if a certificate
; is available.

[Languages]
Name: "english"; MessagesFile: "compiler:Default.isl"

[Tasks]
Name: "desktopicon"; Description: "Create a &desktop shortcut"; GroupDescription: "Shortcuts:"
Name: "asio"; Description: "Register the ASIO Insert driver (needed only if a DAW should process a strip)"; GroupDescription: "Optional components:"; Flags: unchecked
Name: "driverdocs"; Description: "Add Start Menu shortcuts for the virtual cable driver instructions"; GroupDescription: "Optional components:"; Flags: checkedonce

[Files]
; Mixer and documentation at the application root.
Source: "{#PkgDir}\Audio Mixer PRO\{#MyAppExeName}"; DestDir: "{app}"; Flags: ignoreversion
Source: "{#PkgDir}\README.txt"; DestDir: "{app}"; Flags: ignoreversion
Source: "{#PkgDir}\INSTALL.txt"; DestDir: "{app}"; Flags: ignoreversion
Source: "{#PkgDir}\VERSION.txt"; DestDir: "{app}"; Flags: ignoreversion
Source: "{#PkgDir}\CHANGELOG.md"; DestDir: "{app}"; Flags: ignoreversion
Source: "{#PkgDir}\LICENSE.txt"; DestDir: "{app}"; Flags: ignoreversion
Source: "{#PkgDir}\THIRD-PARTY-NOTICES.txt"; DestDir: "{app}"; Flags: ignoreversion

; Optional ASIO insert (64-bit DLL, installer script, notes).
Source: "{#PkgDir}\ASIO Insert\*"; DestDir: "{app}\ASIO Insert"; Flags: ignoreversion recursesubdirs createallsubdirs

; Kernel driver: shipped, not installed. The user follows INSTALL.txt.
Source: "{#PkgDir}\Virtual Cable Driver\*"; DestDir: "{app}\Virtual Cable Driver"; Flags: ignoreversion recursesubdirs createallsubdirs

[Icons]
Name: "{group}\{#MyAppName}"; Filename: "{app}\{#MyAppExeName}"
Name: "{group}\README"; Filename: "{app}\README.txt"
Name: "{group}\Installation guide"; Filename: "{app}\INSTALL.txt"; Tasks: driverdocs
Name: "{group}\Virtual cable driver folder"; Filename: "{app}\Virtual Cable Driver"; Tasks: driverdocs
Name: "{group}\ASIO Insert notes"; Filename: "{app}\ASIO Insert\README.txt"; Tasks: asio
Name: "{group}\Uninstall {#MyAppName}"; Filename: "{uninstallexe}"
Name: "{autodesktop}\{#MyAppName}"; Filename: "{app}\{#MyAppExeName}"; Tasks: desktopicon

[Run]
; ASIO registration writes HKLM\SOFTWARE\ASIO, so it needs elevation - the
; installer already runs elevated and runs the bundled script visibly so any
; message from it is seen.
Filename: "powershell.exe"; Parameters: "-NoProfile -ExecutionPolicy Bypass -File ""{app}\ASIO Insert\register-asio.ps1"""; StatusMsg: "Registering the ASIO Insert driver..."; Flags: waituntilterminated; Tasks: asio

Filename: "{app}\{#MyAppExeName}"; Description: "Start {#MyAppName}"; Flags: nowait postinstall skipifsilent
Filename: "{app}\INSTALL.txt"; Description: "Open the installation guide (virtual cable driver is optional)"; Flags: postinstall shellexec skipifsilent unchecked

[UninstallRun]
; The ASIO insert copies its DLL to %LOCALAPPDATA% and writes
; HKLM\SOFTWARE\ASIO\Audio Mixer PRO Insert plus a CLSID key. The bundled
; script's -Unregister mode removes both; running it when nothing was
; registered is harmless. -NoElevate because the uninstaller is already
; elevated.
Filename: "powershell.exe"; Parameters: "-NoProfile -ExecutionPolicy Bypass -File ""{app}\ASIO Insert\register-asio.ps1"" -Unregister -NoElevate"; Flags: waituntilterminated runhidden; RunOnceId: "UnregisterAsio"

; The kernel driver, if the user installed it, is removed with its own
; uninstall.ps1; the uninstaller only points at it.

[Code]
// Warn (do not block) when the mixer is running: the installer replaces its
// executable, and Windows cannot overwrite a running image.
function InitializeSetup(): Boolean;
var
  ResultCode: Integer;
begin
  Result := True;
  if Exec('cmd.exe', '/c tasklist /FI "IMAGENAME eq audio_mixer_pro.exe" | find /I "audio_mixer_pro.exe" >nul', '',
          SW_HIDE, ewWaitUntilTerminated, ResultCode) then
  begin
    if ResultCode = 0 then
    begin
      if MsgBox('Audio Mixer PRO appears to be running. Close it before continuing, so the executable can be replaced.' + #13#10 + #13#10 + 'Continue anyway?',
                mbConfirmation, MB_YESNO) = IDNO then
        Result := False;
    end;
  end;
end;

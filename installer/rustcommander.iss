; Inno Setup script for RustCommander (https://jrsoftware.org/isinfo.php).
; Built by scripts\package.ps1, which passes:
;   /DAppVersion=<version from Cargo.toml>
;   /DSourceDir=<folder with rustcommander.exe, README.md, LICENSE, THIRD-PARTY-NOTICES.txt>
;   /DOutputDir=<dist folder>
;   /DIconFile=<icon.ico written by crates\ui\build.rs> (optional)
;
; Settings live in %APPDATA%\RustCommander. They are never touched unless the
; user explicitly asks: installing over a previous installation asks whether
; to keep them (default: keep), uninstalling asks whether to remove them
; (default: keep). Silent runs always keep them unless /DELETESETTINGS is
; given. /SETTINGSDIR=<path> points that logic at another folder (testing).
;
; /RESTARTAPP (with /SILENT, used by the app's update command) starts
; RustCommander when the installation is done.

#ifndef AppVersion
  #define AppVersion "0.0.0"
#endif
#ifndef SourceDir
  #define SourceDir "..\target\release"
#endif
#ifndef OutputDir
  #define OutputDir "..\dist"
#endif

#define AppName "RustCommander"
#define AppExe "rustcommander.exe"
#define AppIdGuid "{EDB432ED-6FF7-4C9B-8FCF-BA8560B5A6BE}"

[Setup]
AppId={{#AppIdGuid}
AppName={#AppName}
AppVersion={#AppVersion}
AppVerName={#AppName} {#AppVersion}
AppPublisher=mongo56
AppPublisherURL=https://github.com/mongo56/RustCommander
AppSupportURL=https://github.com/mongo56/RustCommander/issues
AppUpdatesURL=https://github.com/mongo56/RustCommander/releases
DefaultDirName={autopf}\{#AppName}
DefaultGroupName={#AppName}
DisableProgramGroupPage=yes
; Program Files by default; the user may choose a per-user install instead
; (dialog, or /CURRENTUSER on the command line).
PrivilegesRequired=admin
PrivilegesRequiredOverridesAllowed=dialog commandline
ArchitecturesAllowed=x64compatible
ArchitecturesInstallIn64BitMode=x64compatible
OutputDir={#OutputDir}
OutputBaseFilename={#AppName}-{#AppVersion}-setup
#ifdef IconFile
SetupIconFile={#IconFile}
#endif
UninstallDisplayIcon={app}\{#AppExe}
UninstallDisplayName={#AppName}
Compression=lzma2/max
SolidCompression=yes
WizardStyle=modern
; Restart Manager: offers to close a running RustCommander that uses the
; files being replaced.
CloseApplications=yes
RestartApplications=no

[Languages]
Name: "english"; MessagesFile: "compiler:Default.isl"

[Tasks]
Name: "desktopicon"; Description: "{cm:CreateDesktopIcon}"; GroupDescription: "{cm:AdditionalIcons}"; Flags: unchecked

[Files]
Source: "{#SourceDir}\{#AppExe}"; DestDir: "{app}"; Flags: ignoreversion
Source: "{#SourceDir}\README.md"; DestDir: "{app}"; Flags: ignoreversion skipifsourcedoesntexist
Source: "{#SourceDir}\LICENSE*"; DestDir: "{app}"; Flags: ignoreversion skipifsourcedoesntexist
Source: "{#SourceDir}\THIRD-PARTY-NOTICES.txt"; DestDir: "{app}"; Flags: ignoreversion skipifsourcedoesntexist

[Icons]
Name: "{autoprograms}\{#AppName}"; Filename: "{app}\{#AppExe}"
Name: "{autodesktop}\{#AppName}"; Filename: "{app}\{#AppExe}"; Tasks: desktopicon

[Run]
Filename: "{app}\{#AppExe}"; Description: "{cm:LaunchProgram,{#AppName}}"; Flags: nowait postinstall skipifsilent
; The app's update command runs the installer with /SP- /SILENT /RESTARTAPP:
; start the new version afterwards, as the user (not elevated).
Filename: "{app}\{#AppExe}"; Flags: nowait runasoriginaluser skipifnotsilent; Check: HasSwitch('/RESTARTAPP')

[Code]
var
  SettingsPage: TInputOptionWizardPage;

{ The settings folder: %APPDATA%\RustCommander, or /SETTINGSDIR=<path>. }
function SettingsDir: String;
begin
  Result := ExpandConstant('{param:SETTINGSDIR|}');
  if Result = '' then
    Result := ExpandConstant('{userappdata}\{#AppName}');
end;

{ True if the command line has the switch (e.g. /DELETESETTINGS). }
function HasSwitch(const Name: String): Boolean;
var
  I: Integer;
begin
  Result := False;
  for I := 1 to ParamCount do
    if CompareText(ParamStr(I), Name) = 0 then
      Result := True;
end;

function PreviousInstallExists: Boolean;
var
  Key: String;
begin
  Key := 'Software\Microsoft\Windows\CurrentVersion\Uninstall\{#AppIdGuid}_is1';
  Result := RegKeyExists(HKLM64, Key) or RegKeyExists(HKLM32, Key) or RegKeyExists(HKCU, Key);
end;

{ True if a rustcommander.exe started from ExePath is running (WMI). }
function IsRunningFrom(const ExePath: String): Boolean;
var
  Locator, Service, Procs: Variant;
  Path: String;
begin
  Result := False;
  Path := ExePath;
  StringChangeEx(Path, '\', '\\', True);
  StringChangeEx(Path, '''', '\''', True);
  try
    Locator := CreateOleObject('WbemScripting.SWbemLocator');
    Service := Locator.ConnectServer('.', 'root\CIMV2');
    Procs := Service.ExecQuery('SELECT ProcessId FROM Win32_Process WHERE ExecutablePath = ''' + Path + '''');
    Result := Procs.Count > 0;
  except
    Result := False;
  end;
end;

procedure DeleteSettings;
begin
  if DirExists(SettingsDir) then
  begin
    Log('Deleting settings folder ' + SettingsDir);
    DelTree(SettingsDir, True, True, True);
  end;
end;

{ ---- Install ---- }

procedure InitializeWizard;
begin
  SettingsPage := CreateInputOptionPage(wpSelectTasks,
    'Your settings',
    'RustCommander is already installed and has saved settings.',
    'Your settings (panels, tabs, hotlist, keys, colors, server connections) are in:' + #13#10 +
      SettingsDir + #13#10#13#10 +
      'Keeping them is recommended. What should Setup do with them?',
    True, False);
  SettingsPage.Add('Keep my settings (recommended)');
  SettingsPage.Add('Delete my settings and start with the defaults');
  SettingsPage.SelectedValueIndex := 0;
end;

function ShouldSkipPage(PageID: Integer): Boolean;
begin
  Result := False;
  if PageID = SettingsPage.ID then
    Result := not (PreviousInstallExists and DirExists(SettingsDir));
end;

function NextButtonClick(CurPageID: Integer): Boolean;
begin
  Result := True;
  if (CurPageID = SettingsPage.ID) and (SettingsPage.SelectedValueIndex = 1) then
    if MsgBox('Delete all RustCommander settings in' + #13#10 + SettingsDir + '?' + #13#10#13#10 +
        'This cannot be undone.', mbConfirmation, MB_YESNO or MB_DEFBUTTON2) <> IDYES then
    begin
      SettingsPage.SelectedValueIndex := 0;
      Result := False;
    end;
end;

procedure CurStepChanged(CurStep: TSetupStep);
var
  DoDelete: Boolean;
begin
  if CurStep = ssPostInstall then
  begin
    { Only on an explicit choice: the confirmed page option in an
      interactive run, or /DELETESETTINGS on the command line. }
    if HasSwitch('/DELETESETTINGS') then
      DoDelete := True
    else
      DoDelete := (not WizardSilent) and (not ShouldSkipPage(SettingsPage.ID))
        and (SettingsPage.SelectedValueIndex = 1);
    if DoDelete then
      DeleteSettings
    else
      Log('Keeping settings folder ' + SettingsDir);
  end;
end;

{ ---- Uninstall ---- }

function InitializeUninstall: Boolean;
var
  Exe: String;
begin
  Result := True;
  Exe := ExpandConstant('{app}\{#AppExe}');
  while Result and IsRunningFrom(Exe) do
  begin
    if UninstallSilent then
      Result := False
    else
      Result := MsgBox('RustCommander is running. Please close it, then click OK to continue uninstalling.',
        mbInformation, MB_OKCANCEL) = IDOK;
  end;
  if not Result then
    Log('Uninstall cancelled: RustCommander is running.');
end;

procedure CurUninstallStepChanged(CurUninstallStep: TUninstallStep);
begin
  if (CurUninstallStep = usPostUninstall) and DirExists(SettingsDir) then
  begin
    if HasSwitch('/DELETESETTINGS') then
      DeleteSettings
    else if not UninstallSilent then
    begin
      if MsgBox('Also remove your RustCommander settings?' + #13#10 + SettingsDir + #13#10#13#10 +
          'Choose No to keep them for a later installation (recommended).',
          mbConfirmation, MB_YESNO or MB_DEFBUTTON2) = IDYES then
        DeleteSettings;
    end;
  end;
end;

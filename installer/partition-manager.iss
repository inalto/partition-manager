; Inno Setup script of the Windows installer.
;
;   ISCC.exe /DAppVersion=0.4.0 installer\partition-manager.iss
;
; reads pm.exe and partition-manager.exe from dist\ (scripts\build-release.ps1 puts them
; there) and writes dist\partition-manager-<version>-windows-x86_64-setup.exe.
;
; The application updates an installed copy by running the new installer with
;   /VERYSILENT /SUPPRESSMSGBOXES /NORESTART /CLOSEAPPLICATIONS /RELAUNCH=1
; (pm_update::INSTALLER_ARGS). /RELAUNCH=1 is ours: see the [Run] section.
; The uninstaller's name, unins000.exe, is how the application recognises an installed
; copy (pm_update::UNINSTALLER): do not change UninstallFilesDir.

#ifndef AppVersion
  #error Pass the version: ISCC.exe /DAppVersion=x.y.z installer\partition-manager.iss
#endif

#define AppName "Partition Manager"
#define AppExe "partition-manager.exe"
#define Root ".."

[Setup]
; identifies the application across versions: never change it
AppId={{6C0B2C0E-6F0B-4F5B-9E1F-3D6B7A4C2E91}
AppName={#AppName}
AppVersion={#AppVersion}
AppVerName={#AppName} {#AppVersion}
AppPublisher=inalto
AppPublisherURL=https://github.com/inalto/partition-manager
AppSupportURL=https://github.com/inalto/partition-manager/issues
AppUpdatesURL=https://github.com/inalto/partition-manager/releases
DefaultDirName={autopf}\{#AppName}
DefaultGroupName={#AppName}
DisableProgramGroupPage=yes
; the application manages disks: it needs administrator rights, so does its installer
PrivilegesRequired=admin
ArchitecturesAllowed=x64compatible
ArchitecturesInstallIn64BitMode=x64compatible
LicenseFile={#Root}\LICENSE
SetupIconFile={#Root}\crates\pm-gui\assets\partition-manager.ico
UninstallDisplayIcon={app}\{#AppExe}
UninstallDisplayName={#AppName}
OutputDir={#Root}\dist
OutputBaseFilename=partition-manager-{#AppVersion}-windows-x86_64-setup
Compression=lzma2
SolidCompression=yes
WizardStyle=modern
; an update replaces the running executables
CloseApplications=yes
RestartApplications=no
; the PATH task
ChangesEnvironment=yes

[Languages]
Name: "english"; MessagesFile: "compiler:Default.isl"

[Tasks]
Name: "desktopicon"; Description: "Create a &desktop shortcut"; Flags: unchecked
Name: "addtopath"; Description: "Add the command-line tool (pm) to the system &PATH"; Flags: unchecked

[Files]
Source: "{#Root}\dist\{#AppExe}"; DestDir: "{app}"; Flags: ignoreversion
Source: "{#Root}\dist\pm.exe"; DestDir: "{app}"; Flags: ignoreversion
Source: "{#Root}\README.md"; DestDir: "{app}"; Flags: ignoreversion
Source: "{#Root}\LICENSE"; DestDir: "{app}"; Flags: ignoreversion
Source: "{#Root}\CHANGELOG.md"; DestDir: "{app}"; Flags: ignoreversion

[Icons]
Name: "{group}\{#AppName}"; Filename: "{app}\{#AppExe}"
Name: "{autodesktop}\{#AppName}"; Filename: "{app}\{#AppExe}"; Tasks: desktopicon

[Run]
; interactive install: the usual "Launch" checkbox on the last page
Filename: "{app}\{#AppExe}"; Description: "Start {#AppName}"; Flags: nowait postinstall skipifsilent runascurrentuser
; update started by the application (silent, /RELAUNCH=1): start it again
Filename: "{app}\{#AppExe}"; Flags: nowait runascurrentuser; Check: Relaunch

[UninstallDelete]
; what the application's own updater can leave behind
Type: files; Name: "{app}\*.old"
Type: filesandordirs; Name: "{app}\.pm-update"

[Code]
const
  EnvKey = 'SYSTEM\CurrentControlSet\Control\Session Manager\Environment';

{ True when the installer was started with /RELAUNCH=1. }
function Relaunch: Boolean;
begin
  Result := ExpandConstant('{param:RELAUNCH|0}') = '1';
end;

{ Position of Dir in the ;-separated list Paths (1-based, 0 when absent), ignoring case. }
function PathPos(Dir, Paths: String): Integer;
begin
  Result := Pos(';' + Uppercase(Dir) + ';', ';' + Uppercase(Paths) + ';');
end;

procedure PathAdd(Dir: String);
var
  Paths: String;
begin
  if not RegQueryStringValue(HKEY_LOCAL_MACHINE, EnvKey, 'Path', Paths) then
    Paths := '';
  if PathPos(Dir, Paths) > 0 then
    exit;
  if (Paths <> '') and (Paths[Length(Paths)] <> ';') then
    Paths := Paths + ';';
  RegWriteExpandStringValue(HKEY_LOCAL_MACHINE, EnvKey, 'Path', Paths + Dir);
end;

procedure PathRemove(Dir: String);
var
  Paths: String;
  P: Integer;
begin
  if not RegQueryStringValue(HKEY_LOCAL_MACHINE, EnvKey, 'Path', Paths) then
    exit;
  P := PathPos(Dir, Paths);
  if P = 0 then
    exit;
  { Dir and the separator after it }
  Delete(Paths, P, Length(Dir) + 1);
  if (Paths <> '') and (Paths[Length(Paths)] = ';') then
    Delete(Paths, Length(Paths), 1);
  RegWriteExpandStringValue(HKEY_LOCAL_MACHINE, EnvKey, 'Path', Paths);
end;

procedure CurStepChanged(CurStep: TSetupStep);
begin
  if (CurStep = ssPostInstall) and WizardIsTaskSelected('addtopath') then
    PathAdd(ExpandConstant('{app}'));
end;

procedure CurUninstallStepChanged(CurUninstallStep: TUninstallStep);
begin
  if CurUninstallStep = usPostUninstall then
    PathRemove(ExpandConstant('{app}'));
end;

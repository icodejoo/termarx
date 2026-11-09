#ifndef MyAppVersion
  #define MyAppVersion "0.1.0"
#endif

#ifndef MyArch
  #define MyArch "x64"
#endif

#ifndef MyTarget
  #define MyTarget "x86_64-pc-windows-msvc"
#endif

#ifndef MyExeName
  #define MyExeName "termarx.exe"
#endif

#ifndef MyCliExeName
  #define MyCliExeName "termarx-cli.exe"
#endif

#if MyArch == "x64"
  #define MyArchAllowed "x64compatible"
  #define MyArchInstallMode "x64compatible"
#elif MyArch == "arm64"
  #define MyArchAllowed "arm64"
  #define MyArchInstallMode "arm64"
#else
  #error Unsupported MyArch value. Use x64 or arm64.
#endif

[Setup]
AppId={{7D3DD34B-5F8F-4D7B-BBC9-0F54B4C89142}
AppName=Termarx
AppVersion={#MyAppVersion}
AppPublisher=Termarx
AppPublisherURL=https://github.com/icodejoo/termarx
AppSupportURL=https://github.com/icodejoo/termarx/issues
AppUpdatesURL=https://github.com/icodejoo/termarx/releases
DefaultDirName={autopf}\Termarx
DefaultGroupName=Termarx
OutputDir=..\..\target\dist
OutputBaseFilename=Termarx-{#MyAppVersion}-windows-{#MyArch}-Setup
SetupIconFile=..\..\assets\termy.ico
Compression=lzma
SolidCompression=yes
WizardStyle=modern
ArchitecturesAllowed={#MyArchAllowed}
ArchitecturesInstallIn64BitMode={#MyArchInstallMode}
UninstallDisplayIcon={app}\{#MyExeName}
PrivilegesRequired=admin
CloseApplications=yes
RestartApplications=no

[Files]
Source: "..\..\target\{#MyTarget}\release\{#MyExeName}"; DestDir: "{app}"; Flags: ignoreversion
Source: "..\..\target\{#MyTarget}\release\{#MyCliExeName}"; DestDir: "{app}"; Flags: ignoreversion
; 内置新版 ConPTY：应用不再读 PATH 找 DLL，必须放在 exe 同目录
Source: "..\..\vendor\conpty\{#MyArch}\conpty.dll"; DestDir: "{app}"; Flags: ignoreversion
Source: "..\..\vendor\conpty\{#MyArch}\OpenConsole.exe"; DestDir: "{app}"; Flags: ignoreversion

[Icons]
Name: "{group}\Termarx"; Filename: "{app}\{#MyExeName}"
Name: "{autodesktop}\Termarx"; Filename: "{app}\{#MyExeName}"

[Registry]
Root: HKCR; Subkey: "termy"; ValueType: string; ValueName: ""; ValueData: "URL:Termarx Protocol"; Flags: uninsdeletekey
Root: HKCR; Subkey: "termy"; ValueType: string; ValueName: "URL Protocol"; ValueData: ""
Root: HKCR; Subkey: "termy\DefaultIcon"; ValueType: string; ValueName: ""; ValueData: "{app}\{#MyExeName},0"
Root: HKCR; Subkey: "termy\shell\open\command"; ValueType: string; ValueName: ""; ValueData: """{app}\{#MyExeName}"" ""%1"""
Root: HKCR; Subkey: "Directory\shell\TermyOpenTab"; ValueType: string; ValueName: ""; ValueData: "Open new Termarx tab here"; Flags: uninsdeletekey
Root: HKCR; Subkey: "Directory\shell\TermyOpenTab"; ValueType: string; ValueName: "Icon"; ValueData: "{app}\{#MyExeName}"
Root: HKCR; Subkey: "Directory\shell\TermyOpenTab\command"; ValueType: string; ValueName: ""; ValueData: """{app}\{#MyExeName}"" --working-directory ""%V"""
Root: HKCR; Subkey: "Directory\Background\shell\TermyOpenTab"; ValueType: string; ValueName: ""; ValueData: "Open new Termarx tab here"; Flags: uninsdeletekey
Root: HKCR; Subkey: "Directory\Background\shell\TermyOpenTab"; ValueType: string; ValueName: "Icon"; ValueData: "{app}\{#MyExeName}"
Root: HKCR; Subkey: "Directory\Background\shell\TermyOpenTab\command"; ValueType: string; ValueName: ""; ValueData: """{app}\{#MyExeName}"" --working-directory ""%V"""
Root: HKCR; Subkey: "Drive\shell\TermyOpenTab"; ValueType: string; ValueName: ""; ValueData: "Open new Termarx tab here"; Flags: uninsdeletekey
Root: HKCR; Subkey: "Drive\shell\TermyOpenTab"; ValueType: string; ValueName: "Icon"; ValueData: "{app}\{#MyExeName}"
Root: HKCR; Subkey: "Drive\shell\TermyOpenTab\command"; ValueType: string; ValueName: ""; ValueData: """{app}\{#MyExeName}"" --working-directory ""%V"""

[Code]
{ 优雅停止常驻的会话宿主（termarx.exe --multiplexer-host）。
  它在应用退出后仍活着，持有全部 shell 与 OpenConsole.exe，
  会让“正在使用的应用程序”检测列出一长串占用。旧版本未安装或宿主未运行时静默忽略失败。 }
procedure StopSessionHost();
var
  CliPath: String;
  ResultCode: Integer;
begin
  CliPath := ExpandConstant('{app}\{#MyCliExeName}');
  if FileExists(CliPath) then
    Exec(CliPath, 'mux shutdown', '', SW_HIDE, ewWaitUntilTerminated, ResultCode);
end;

{ 点击“安装”时、占用检测之前停掉宿主（向导模式）。 }
function NextButtonClick(CurPageID: Integer): Boolean;
begin
  if CurPageID = wpReady then
    StopSessionHost();
  Result := True;
end;

{ 静默安装不经过向导页，这里再兜底停一次；应用自动更新也会先在应用侧停宿主。 }
function PrepareToInstall(var NeedsRestart: Boolean): String;
begin
  StopSessionHost();
  Result := '';
end;

[Run]
Filename: "{app}\{#MyExeName}"; Description: "Launch Termarx"; Flags: nowait postinstall skipifsilent
; Silent auto-updates quit Termarx before setup finishes, so relaunch after install.
Filename: "{app}\{#MyExeName}"; Flags: nowait runasoriginaluser skipifnotsilent

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

[Run]
Filename: "{app}\{#MyExeName}"; Description: "Launch Termarx"; Flags: nowait postinstall skipifsilent
; Silent auto-updates quit Termarx before setup finishes, so relaunch after install.
Filename: "{app}\{#MyExeName}"; Flags: nowait runasoriginaluser skipifnotsilent

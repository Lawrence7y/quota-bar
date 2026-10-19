; QuotaBar 安装脚本（Inno Setup 6.7.x）
; 构建：D:\Inno Setup 6\ISCC.exe installer\quota-bar.iss
; 产物：installer\QuotaBar-Setup-0.1.0.exe
;
; 设计取舍：
; - PrivilegesRequired=lowest：额度条是个人工具，纯按用户安装（{localappdata}\Programs），
;   装载/卸载全程不弹 UAC；HKCU Run 自启任务与程序内菜单自启写的是同一个值名 QuotaBar。
; - 卸载默认保留 %LOCALAPPDATA%\QuotaBar（config.json / 凭据管理器条目不动）——
;   凭据在 Windows 凭据管理器里，卸载程序本就不应碰用户密钥。

#define MyAppName "QuotaBar"
#define MyAppNameZH "QuotaBar 额度条"
#define MyAppVersion "0.1.0"
#define MyAppExeName "quota-bar.exe"
; 应用 ID（GUID 固定，升级覆盖安装的关键）：由 {5B8E2C7A-9F3D-4E61-A7C4-8D2F5B1E0A93} 派生书写
[Setup]
AppId={{B8E2C7A3-3D5F-4B91-9C64-2F8D1E5A07B3}
AppName={#MyAppNameZH}
AppVersion={#MyAppVersion}
AppPublisher=quota-bar
DefaultDirName={localappdata}\Programs\{#MyAppName}
DefaultGroupName={#MyAppNameZH}
DisableProgramGroupPage=yes
PrivilegesRequired=lowest
ArchitecturesAllowed=x64compatible
ArchitecturesInstallIn64BitMode=x64compatible
OutputDir=.
OutputBaseFilename=QuotaBar-Setup-{#MyAppVersion}
Compression=lzma2/max
SolidCompression=yes
WizardStyle=modern
UninstallDisplayIcon={app}\{#MyAppExeName}
UninstallDisplayName={#MyAppNameZH}
CloseApplications=yes
RestartApplications=no
VersionInfoVersion={#MyAppVersion}
VersionInfoProductVersion={#MyAppVersion}
VersionInfoProductName={#MyAppName}

[Languages]
Name: "chs"; MessagesFile: "compiler:Languages\ChineseSimplified.isl"
Name: "en";  MessagesFile: "compiler:Default.isl"

[Tasks]
Name: "desktopicon"; Description: "创建桌面快捷方式(&D)"; Flags: unchecked
Name: "autostart";   Description: "开机自动启动（写入 HKCU Run，与程序内菜单同值）"; Flags: unchecked

[Files]
; 单文件交付：logo/png 均已 include_bytes 内嵌，exe 自包含
Source: "..\target\release\{#MyAppExeName}"; DestDir: "{app}"; Flags: ignoreversion

[Icons]
Name: "{group}\{#MyAppNameZH}"; Filename: "{app}\{#MyAppExeName}"
Name: "{autodesktop}\{#MyAppNameZH}"; Filename: "{app}\{#MyAppExeName}"; Tasks: desktopicon

[Registry]
Root: HKCU; Subkey: "Software\Microsoft\Windows\CurrentVersion\Run"; \
  ValueType: string; ValueName: "QuotaBar"; ValueData: """{app}\{#MyAppExeName}"""; \
  Flags: uninsdeletevalue; Tasks: autostart

[Run]
Filename: "{app}\{#MyAppExeName}"; \
  Description: "立即启动 {#MyAppNameZH}"; Flags: nowait postinstall skipifsilent

[Code]
// 覆盖安装/卸载前先结束运行中的实例（单实例互斥体会挡住文件替换）
procedure KillApp();
var
  ResultCode: Integer;
begin
  Exec('taskkill.exe', '/IM {#MyAppExeName} /F', '', SW_HIDE, ewWaitUntilTerminated, ResultCode);
end;

function PrepareToInstall(var NeedsRestart: Boolean): String;
begin
  KillApp();
  Result := '';
end;

procedure CurUninstallStepChanged(CurUninstallStep: TUninstallStep);
begin
  if CurUninstallStep = usUninstall then
    KillApp();
end;

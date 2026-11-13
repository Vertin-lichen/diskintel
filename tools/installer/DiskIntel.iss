; ============================================================================
;  磁盘情报系统 (DiskIntel) · Inno Setup 6 安装脚本
;
;  构建（推荐）：
;    pwsh -File tools\build-installer.ps1 [-Version 0.3.2] [-DistDir <发行目录>] [签名参数见脚本]
;  手工编译：
;    ISCC.exe /DAppVersion=0.3.2 /DDistDir=<发行目录>\DiskIntel-0.3.2 tools\installer\DiskIntel.iss
;
;  设计要点：
;   - 纯用户级安装（PrivilegesRequired=lowest），默认 {localappdata}\Programs\DiskIntel，不触发 UAC
;   - 数据目录默认 D:\disk-intel-data，安装向导可改；写入 HKCU\Software\DiskIntel\DataRoot
;   - 快照目录：程序把自定义快照目录写入 HKCU\Software\DiskIntel\SnapshotDir（安装包不写）；
;     卸载时实时读取 SnapshotDir 与 DataRoot\snapshots，去重后清理
;   - 卸载默认保留快照；自定义卸载页勾选「同时删除快照数据」或 unins000.exe /DELSNAPSHOTS=1 才删除
;   - 卸载会尝试调用 tools\uninstall.ps1 清理引擎桥计划任务（无管理员权限时失败但不阻断）
;   - 删除范围严格限定：程序文件、%APPDATA%\DiskIntel 设置、勾选时的快照文件；目录本身不删
; ============================================================================

#define MyAppName "磁盘情报系统"
#define MyAppShortName "DiskIntel"
#ifndef AppVersion
  #define AppVersion "0.3.2"
#endif
#ifdef DistDir
  #define MyDistDir DistDir
#else
  #define MyDistDir "..\..\dist\DiskIntel-" + AppVersion
#endif

[Setup]
AppId={{B71F2A3C-5D8E-4F62-9A1B-3C7D5E9F0A24}
AppName={#MyAppName}
AppVersion={#AppVersion}
AppVerName={#MyAppName} {#AppVersion}
AppPublisher=DiskIntel
DefaultDirName={localappdata}\Programs\DiskIntel
DefaultGroupName={#MyAppName}
DisableProgramGroupPage=yes
PrivilegesRequired=lowest
UsePreviousAppDir=no
UsePreviousGroup=no
UsePreviousTasks=no
ArchitecturesAllowed=x64compatible
ArchitecturesInstallIn64BitMode=x64compatible
MinVersion=10.0
OutputDir=..\..\dist
OutputBaseFilename=DiskIntel-{#AppVersion}-Setup
SetupIconFile=icon.ico
UninstallDisplayIcon={app}\DiskIntel.exe
UninstallDisplayName={#MyAppName}（DiskIntel）
Compression=lzma2/ultra64
SolidCompression=yes
WizardStyle=modern
WizardSizePercent=100
ChangesAssociations=no
ChangesEnvironment=no
RestartIfNeededByRun=no
CloseApplications=no
Uninstallable=yes
CreateUninstallRegKey=yes

[Languages]
Name: "chinesesimplified"; MessagesFile: "ChineseSimplified.isl"
Name: "english"; MessagesFile: "compiler:Default.isl"

[Tasks]
Name: "desktopicon"; Description: "{cm:CreateDesktopIcon}"; GroupDescription: "{cm:AdditionalIcons}"; Flags: unchecked

[Files]
; 发行包内容（dist\DiskIntel-<版本>）：
Source: "{#MyDistDir}\DiskIntel.exe"; DestDir: "{app}"; Flags: ignoreversion
Source: "{#MyDistDir}\DiskIntelCLI.exe"; DestDir: "{app}"; Flags: ignoreversion
Source: "{#MyDistDir}\README.txt"; DestDir: "{app}"; Flags: ignoreversion
Source: "{#MyDistDir}\goz\*"; DestDir: "{app}\goz"; Flags: ignoreversion recursesubdirs createallsubdirs
Source: "{#MyDistDir}\tools\*"; DestDir: "{app}\tools"; Flags: ignoreversion recursesubdirs createallsubdirs
; 引擎桥卸载脚本（仓库 tools\ 下，发行包里没有，安装时补进 {app}\tools）：
Source: "..\..\tools\uninstall.ps1"; DestDir: "{app}\tools"; Flags: ignoreversion
Source: "..\..\tools\uninstall-elevated.cmd"; DestDir: "{app}\tools"; Flags: ignoreversion
; LICENSE 仅在存在时打包（由 build-installer.ps1 通过 /DLicenseFile 传入绝对路径）：
#ifdef LicenseFile
Source: "{#LicenseFile}"; DestDir: "{app}"; Flags: ignoreversion
#endif

[Icons]
Name: "{group}\{#MyAppName}"; Filename: "{app}\DiskIntel.exe"; Parameters: "--data-root ""{code:GetDataRoot}"""; WorkingDir: "{app}"; Comment: "启动{#MyAppName}"
Name: "{group}\打开数据目录"; Filename: "{sys}\explorer.exe"; Parameters: """{code:GetDataRoot}"""; WorkingDir: "{app}"; Comment: "打开快照数据目录"
Name: "{group}\卸载 {#MyAppName}"; Filename: "{uninstallexe}"; Comment: "卸载{#MyAppName}"
Name: "{autodesktop}\{#MyAppName}"; Filename: "{app}\DiskIntel.exe"; Parameters: "--data-root ""{code:GetDataRoot}"""; WorkingDir: "{app}"; Comment: "启动{#MyAppName}"; Tasks: desktopicon

[Registry]
; 数据目录从此处读回：用户级 HKCU，卸载时由 uninsdeletevalue 清理
Root: HKCU; Subkey: "Software\DiskIntel"; ValueType: string; ValueName: "DataRoot"; ValueData: "{code:GetDataRoot}"; Flags: uninsdeletevalue uninsdeletekeyifempty

[UninstallDelete]
; 程序自己写的设置/历史/列宽（%APPDATA%\DiskIntel），卸载时删除；数据目录不在此列
Type: filesandordirs; Name: "{userappdata}\DiskIntel"

[Code]
const
  DefaultDataRoot = 'D:\disk-intel-data';
  MyAppTitle = '磁盘情报系统';

var
  DataRootPage: TInputDirWizardPage;
  DataRootValue: String;
  DeleteSnapshots: Boolean;
  DeletedSnapshotCount: Integer;
  BridgeCleanupFailed: Boolean;
  { 卸载时清理的快照目录：默认 DataRoot\snapshots + 注册表 SnapshotDir（去重） }
  SnapshotDefaultDir: String;
  SnapshotCustomDir: String;
  SnapshotTargets: TStringList;
  SnapshotSummary: String;

{ ---------------------------------------------------------------------------
  通用小工具
  --------------------------------------------------------------------------- }

{ 读取命令行 /名称=值；支持 /名称="带 空格 的值"。GetCmdTail 安装/卸载都可用。 }
function GetArgValue(const ParamName: String): String;
var
  Tail, Rest, Key: String;
  P, Q: Integer;
begin
  Result := '';
  Tail := ' ' + GetCmdTail + ' ';
  Key := '/' + UpperCase(ParamName) + '=';
  P := Pos(Key, UpperCase(Tail));
  if P = 0 then
    Exit;
  Rest := Copy(Tail, P + Length(Key), Length(Tail) - (P + Length(Key)) + 1);
  if (Length(Rest) > 0) and (Rest[1] = '"') then
  begin
    Delete(Rest, 1, 1);
    Q := Pos('"', Rest);
    if Q > 0 then
      Result := Copy(Rest, 1, Q - 1)
    else
      Result := Rest;
  end
  else
  begin
    Q := Pos(' ', Rest);
    if Q > 0 then
      Result := Copy(Rest, 1, Q - 1)
    else
      Result := Rest;
  end;
end;

function SwitchPresent(const ParamName: String): Boolean;
var
  Tail: String;
begin
  Tail := ' ' + UpperCase(GetCmdTail) + ' ';
  Result := Pos(' /' + UpperCase(ParamName) + ' ', Tail) > 0;
end;

function StripTrailingBackslash(const S: String): String;
begin
  Result := Trim(S);
  while (Length(Result) > 3) and (Result[Length(Result)] = '\') do
    Delete(Result, Length(Result), 1);
  if (Length(Result) = 2) and (Result[2] = ':') then
    Result := Result + '\';
end;

function IsAbsoluteDirPath(const Path: String): Boolean;
begin
  Result := (Length(Path) >= 3) and (Path[2] = ':') and (Path[3] = '\');
  if not Result then
    Result := (Length(Path) >= 3) and (Copy(Path, 1, 2) = '\\');
end;

function SnapshotDirOf(const Root: String): String;
begin
  Result := AddBackslash(Root) + 'snapshots';
end;

function SamePath(const A, B: String): Boolean;
begin
  Result := LowerCase(A) = LowerCase(B);
end;

{ 程序写入的自定义快照目录；空串表示未设置（只清理 DataRoot\snapshots）。 }
function ReadInstalledSnapshotDir: String;
begin
  Result := '';
  if RegQueryStringValue(HKCU, 'Software\DiskIntel', 'SnapshotDir', Result) then
    Result := StripTrailingBackslash(Result);
end;

{ 只认快照目录下这几类程序生成的文件；其它文件（用户放的任何东西）一律不动。 }
function IsSnapshotFileName(const FileName: String): Boolean;
var
  L: String;
begin
  L := LowerCase(FileName);
  Result := (Length(L) >= 7) and (Copy(L, Length(L) - 6, 7) = '.sqlite');
  if Result then
    Exit;
  Result := (L = 'index.json') or (L = '.create.lock') or (L = 'notes.json');
end;

{ 收集指定快照目录下的待删文件（仅当前层，不进子目录）。 }
function CollectSnapshotFilesInDir(const Dir: String; Names: TStringList): Integer;
var
  Mask: String;
  FindRec: TFindRec;
begin
  Result := 0;
  if not DirExists(Dir) then
    Exit;
  Mask := AddBackslash(Dir) + '*';
  if FindFirst(Mask, FindRec) then
  begin
    try
      repeat
        if (FindRec.Attributes and FILE_ATTRIBUTE_DIRECTORY) = 0 then
          if IsSnapshotFileName(FindRec.Name) then
          begin
            Names.Add(FindRec.Name);
            Result := Result + 1;
          end;
      until not FindNext(FindRec);
    finally
      FindClose(FindRec);
    end;
  end;
end;

function CountSnapshotFilesInDir(const Dir: String): Integer;
var
  Names: TStringList;
begin
  Names := TStringList.Create;
  try
    Result := CollectSnapshotFilesInDir(Dir, Names);
  finally
    Names.Free;
  end;
end;

function DeleteSnapshotFilesInDir(const Dir: String): Integer;
var
  Names: TStringList;
  FullName: String;
  I: Integer;
begin
  Result := 0;
  Names := TStringList.Create;
  try
    CollectSnapshotFilesInDir(Dir, Names);
    for I := 0 to Names.Count - 1 do
    begin
      FullName := AddBackslash(Dir) + Names.Strings[I];
      if DeleteFile(FullName) then
      begin
        Result := Result + 1;
        Log('已删除快照文件：' + FullName);
      end
      else
        Log('删除失败（可能被占用）：' + FullName);
    end;
  finally
    Names.Free;
  end;
end;

{ ---------------------------------------------------------------------------
  安装阶段
  --------------------------------------------------------------------------- }

// [Registry] / [Icons] 的 code 常量会调用本函数。
function GetDataRoot(Param: String): String;
begin
  Result := DataRootValue;
  if Result = '' then
    Result := DefaultDataRoot;
end;

procedure InitializeWizard;
begin
  DataRootPage := CreateInputDirPage(wpSelectDir,
    '数据目录',
    '程序数据（快照）存放在哪个目录？',
    '程序会把快照库放在该目录的 snapshots 子目录中。卸载时快照默认保留，仅当你勾选删除时才会清理。',
    False, '');
  DataRootPage.Add('数据目录（默认 D:\disk-intel-data）：');

  DataRootValue := StripTrailingBackslash(GetArgValue('DATAROOT'));
  if DataRootValue = '' then
    DataRootValue := DefaultDataRoot;

  DataRootPage.Values[0] := DataRootValue;
end;

function NextButtonClick(CurPageID: Integer): Boolean;
var
  V: String;
begin
  Result := True;
  if CurPageID = DataRootPage.ID then
  begin
    V := StripTrailingBackslash(DataRootPage.Values[0]);
    if (V = '') or (not IsAbsoluteDirPath(V)) then
    begin
      MsgBox('请选择完整的数据目录，例如 D:\disk-intel-data（不支持相对路径）。', mbError, MB_OK);
      Result := False;
      Exit;
    end;
    DataRootValue := V;
    DataRootPage.Values[0] := V;
  end;
end;

function UpdateReadyMemo(Space, NewLine, MemoUserInfoInfo, MemoDirInfo, MemoTypeInfo,
  MemoComponentsInfo, MemoGroupInfo, MemoTasksInfo: String): String;
begin
  Result := MemoDirInfo + NewLine + NewLine +
    '数据目录：' + NewLine + Space + DataRootValue + NewLine + NewLine +
    MemoGroupInfo + MemoTasksInfo;
end;

procedure CurStepChanged(CurStep: TSetupStep);
begin
  if CurStep = ssPostInstall then
  begin
    if ForceDirectories(DataRootValue) then
      Log('数据目录已就绪：' + DataRootValue)
    else
      Log('警告：无法创建数据目录：' + DataRootValue);
  end;
end;

{ ---------------------------------------------------------------------------
  卸载阶段
  --------------------------------------------------------------------------- }

function ReadInstalledDataRoot: String;
begin
  Result := '';
  if RegQueryStringValue(HKCU, 'Software\DiskIntel', 'DataRoot', Result) then
    Result := StripTrailingBackslash(Result);
  if Result = '' then
    Result := DefaultDataRoot;
end;

procedure KillImage(const ImageName: String);
var
  Exe: String;
  ResultCode: Integer;
begin
  Exe := ExpandConstant('{sys}\taskkill.exe');
  if not FileExists(Exe) then
    Exe := 'taskkill.exe';
  Exec(Exe, '/F /T /IM ' + ImageName, '', SW_HIDE, ewWaitUntilTerminated, ResultCode);
  Log('taskkill ' + ImageName + ' 返回：' + IntToStr(ResultCode));
end;

procedure KillDiskIntelProcesses;
begin
  KillImage('DiskIntel.exe');
  KillImage('DiskIntelCLI.exe');
end;

{ 尝试调用发行包里的引擎桥卸载脚本；没有管理员权限时脚本会 exit 1，这里只记录、不阻断。 }
procedure TryCleanupEngineBridge;
var
  ScriptPath, PSExe, Params: String;
  ResultCode: Integer;
begin
  BridgeCleanupFailed := False;
  ScriptPath := ExpandConstant('{app}\tools\uninstall.ps1');
  if not FileExists(ScriptPath) then
  begin
    Log('未找到引擎桥卸载脚本，跳过：' + ScriptPath);
    Exit;
  end;
  PSExe := ExpandConstant('{sys}\WindowsPowerShell\v1.0\powershell.exe');
  if not FileExists(PSExe) then
    PSExe := 'powershell.exe';
  Params := '-NoProfile -ExecutionPolicy Bypass -WindowStyle Hidden -File "' + ScriptPath + '"';
  BridgeCleanupFailed := True;
  if Exec(PSExe, Params, '', SW_HIDE, ewWaitUntilTerminated, ResultCode) then
  begin
    Log('引擎桥卸载脚本退出码：' + IntToStr(ResultCode));
    BridgeCleanupFailed := (ResultCode <> 0);
  end
  else
    Log('无法启动引擎桥卸载脚本：' + PSExe);
end;

{ 卸载自定义页：Inno 的卸载器不支持 CreateCustomPage/CreateInputOptionPage，
  这里用 CreateCustomForm 自绘一个等价页面。返回 False 表示用户取消卸载。 }
function ShowUninstallOptionsForm: Boolean;
var
  Form: TSetupForm;
  LIntro1, LIntro2, LPath, LDefault, LCustom, LHint: TNewStaticText;
  Chk: TNewCheckBox;
  OKButton, CancelButton: TNewButton;
  DefaultCount, CustomCount, W: Integer;
begin
  Result := False;
  DefaultCount := CountSnapshotFilesInDir(SnapshotTargets.Strings[0]);
  CustomCount := 0;
  if SnapshotTargets.Count > 1 then
    CustomCount := CountSnapshotFilesInDir(SnapshotTargets.Strings[1]);

  { 注意：Inno 6.7 的 CreateCustomForm 会自己按 DPI/字体缩放参数，这里传未缩放的 96-DPI 设计值；
    传 ScaleX 值会导致二次放大（WizardSizePercent 另算，故本脚本固定 WizardSizePercent=100）。 }
  Form := CreateCustomForm(580, 360, False, False);
  try
    Form.Caption := '卸载 ' + MyAppTitle;
    Form.Position := poScreenCenter;

    LIntro1 := TNewStaticText.Create(Form);
    LIntro1.Parent := Form;
    LIntro1.Left := ScaleX(16);
    LIntro1.Top := ScaleY(16);
    LIntro1.Caption := '将卸载磁盘情报系统，并删除程序文件与设置。';

    LIntro2 := TNewStaticText.Create(Form);
    LIntro2.Parent := Form;
    LIntro2.Left := ScaleX(16);
    LIntro2.Top := ScaleY(44);
    LIntro2.Caption := '%APPDATA%\DiskIntel（检索历史、列宽等）也会一并删除。';

    LPath := TNewStaticText.Create(Form);
    LPath.Parent := Form;
    LPath.Left := ScaleX(16);
    LPath.Top := ScaleY(84);
    LPath.Caption := '数据目录：' + DataRootValue;

    LDefault := TNewStaticText.Create(Form);
    LDefault.Parent := Form;
    LDefault.Left := ScaleX(16);
    LDefault.Top := ScaleY(112);
    LDefault.AutoSize := False;
    LDefault.Width := ScaleX(548);
    LDefault.Caption := '快照目录（默认）：' + SnapshotTargets.Strings[0] + '（' + IntToStr(DefaultCount) + ' 个文件）';

    LCustom := TNewStaticText.Create(Form);
    LCustom.Parent := Form;
    LCustom.Left := ScaleX(16);
    LCustom.Top := ScaleY(140);
    LCustom.AutoSize := False;
    LCustom.Width := ScaleX(548);
    if SnapshotCustomDir = '' then
      LCustom.Caption := '自定义快照目录：注册表未设置 SnapshotDir（只清理默认目录）'
    else if SnapshotTargets.Count > 1 then
      LCustom.Caption := '自定义快照目录：' + SnapshotCustomDir + '（' + IntToStr(CustomCount) + ' 个文件）'
    else
      LCustom.Caption := '自定义快照目录：与默认目录相同，已去重';

    LHint := TNewStaticText.Create(Form);
    LHint.Parent := Form;
    LHint.Left := ScaleX(16);
    LHint.Top := ScaleY(172);
    LHint.Caption := '勾选后删除上述目录中的 *.sqlite / index.json / notes.json / .create.lock；目录本身与其它文件保留。';

    Chk := TNewCheckBox.Create(Form);
    Chk.Parent := Form;
    Chk.Left := ScaleX(16);
    Chk.Top := ScaleY(208);
    Chk.Width := ScaleX(548);
    Chk.Caption := '同时删除快照数据（不可恢复）';
    Chk.Checked := False;

    OKButton := TNewButton.Create(Form);
    OKButton.Parent := Form;
    OKButton.Caption := '继续卸载';
    OKButton.ModalResult := mrOk;
    OKButton.Default := True;

    CancelButton := TNewButton.Create(Form);
    CancelButton.Parent := Form;
    CancelButton.Caption := '取消';
    CancelButton.ModalResult := mrCancel;
    CancelButton.Cancel := True;

    W := Form.CalculateButtonWidth([OKButton.Caption, CancelButton.Caption]);
    if W < ScaleX(90) then
      W := ScaleX(90);
    OKButton.Width := W;
    CancelButton.Width := W;
    OKButton.Height := ScaleY(25);
    CancelButton.Height := ScaleY(25);
    OKButton.Top := Form.ClientHeight - ScaleY(25 + 12);
    CancelButton.Top := OKButton.Top;
    CancelButton.Left := Form.ClientWidth - ScaleX(12) - W;
    OKButton.Left := CancelButton.Left - ScaleX(8) - W;

    Form.ActiveControl := OKButton;

    Result := Form.ShowModal() = mrOk;
    if Result then
      DeleteSnapshots := Chk.Checked;
  finally
    Form.Free();
  end;
end;

function InitializeUninstall(): Boolean;
begin
  DeleteSnapshots := False;
  DeletedSnapshotCount := 0;
  BridgeCleanupFailed := False;
  SnapshotSummary := '';
  DataRootValue := ReadInstalledDataRoot;
  SnapshotDefaultDir := SnapshotDirOf(DataRootValue);
  SnapshotCustomDir := ReadInstalledSnapshotDir;

  SnapshotTargets := TStringList.Create;
  SnapshotTargets.Add(SnapshotDefaultDir);
  if SnapshotCustomDir = '' then
    Log('注册表未设置 SnapshotDir，只清理默认快照目录：' + SnapshotDefaultDir)
  else if SamePath(SnapshotCustomDir, SnapshotDefaultDir) then
  begin
    Log('SnapshotDir 与 DataRoot\snapshots 相同，已去重：' + SnapshotDefaultDir);
    SnapshotCustomDir := SnapshotDefaultDir;
  end
  else
    SnapshotTargets.Add(SnapshotCustomDir);
  Log('卸载快照目标目录数：' + IntToStr(SnapshotTargets.Count));

  { 脚本化开关：unins000.exe /DELSNAPSHOTS=1 等价于勾选；默认不删除。 }
  if (UpperCase(Trim(GetArgValue('DELSNAPSHOTS'))) = '1') or SwitchPresent('DELSNAPSHOTS') then
    DeleteSnapshots := True;

  if UninstallSilent then
  begin
    Result := True;
    Exit;
  end;

  Result := ShowUninstallOptionsForm;
  if not Result then
    Log('用户取消了卸载（自定义卸载页）');
end;

procedure DeinitializeUninstall();
begin
  if SnapshotTargets <> nil then
  begin
    SnapshotTargets.Free;
    SnapshotTargets := nil;
  end;
end;

{ 依次清理所有目标快照目录（默认 + 自定义，已去重），返回删除文件总数。 }
function DeleteAllSnapshotFiles: Integer;
var
  I, N: Integer;
  Dir: String;
begin
  Result := 0;
  SnapshotSummary := '';
  for I := 0 to SnapshotTargets.Count - 1 do
  begin
    Dir := SnapshotTargets.Strings[I];
    if DirExists(Dir) then
    begin
      N := DeleteSnapshotFilesInDir(Dir);
      Result := Result + N;
      SnapshotSummary := SnapshotSummary + #13#10 + '  · ' + Dir + '：删除 ' + IntToStr(N) + ' 个文件';
      Log('快照目录清理：' + Dir + ' -> ' + IntToStr(N) + ' 个文件');
    end
    else
    begin
      SnapshotSummary := SnapshotSummary + #13#10 + '  · ' + Dir + '：目录不存在，跳过';
      Log('快照目录不存在，跳过：' + Dir);
    end;
  end;
end;

procedure CurUninstallStepChanged(CurUninstallStep: TUninstallStep);
var
  Msg: String;
  I: Integer;
begin
  if CurUninstallStep = usUninstall then
  begin
    KillDiskIntelProcesses;
    if DeleteSnapshots then
    begin
      DeletedSnapshotCount := DeleteAllSnapshotFiles;
      Log('快照删除文件总数：' + IntToStr(DeletedSnapshotCount));
    end;
    { 程序写入的 SnapshotDir 配置随程序卸载一并清掉；DataRoot 由 [Registry] uninsdeletevalue 处理 }
    if RegDeleteValue(HKCU, 'Software\DiskIntel', 'SnapshotDir') then
      Log('已删除 HKCU\Software\DiskIntel\SnapshotDir')
    else
      Log('HKCU SnapshotDir 不存在或删除失败');
    TryCleanupEngineBridge;
  end
  else if CurUninstallStep = usDone then
  begin
    if UninstallSilent then
      Exit;
    if DeleteSnapshots then
    begin
      if DeletedSnapshotCount > 0 then
        Msg := '已清理快照数据（共 ' + IntToStr(DeletedSnapshotCount) + ' 个文件）：' + SnapshotSummary
      else
        Msg := '未删除任何快照文件；检查过的目录：' + SnapshotSummary;
      Msg := Msg + #13#10 + #13#10 +
        '数据根目录已保留：' + DataRootValue + #13#10 +
        '快照目录本身与其它文件（非 *.sqlite / index.json / notes.json / .create.lock）均保留。';
    end
    else
    begin
      Msg := '快照数据已保留（仅卸载程序，不动快照）：';
      for I := 0 to SnapshotTargets.Count - 1 do
        Msg := Msg + #13#10 + '  · ' + SnapshotTargets.Strings[I];
      Msg := Msg + #13#10 + #13#10 + '数据根目录已保留：' + DataRootValue;
    end;
    if BridgeCleanupFailed then
      Msg := Msg + #13#10#13#10 +
        '注意：引擎桥计划任务（DiskIntel gozd / DiskIntel gozd stop）需要管理员权限，未能自动清理。' + #13#10 +
        '如需清理，请从发行包中取出 tools\uninstall-elevated.cmd（或 tools\uninstall.ps1），以管理员身份运行一次。';
    MsgBox(Msg, mbInformation, MB_OK);
  end;
end;

# tools\installer — Windows 安装包（Inno Setup 6）

本目录是「磁盘情报系统 / DiskIntel」的安装包工程，配套构建、签名与验收脚本。

## 文件

| 文件 | 说明 |
| --- | --- |
| `DiskIntel.iss` | Inno Setup 6 脚本（安装向导数据目录页、快捷方式、卸载自定义页、双快照目录清理、引擎桥清理） |
| `build-installer.ps1` | 构建脚本：System.Drawing 生成多尺寸 `icon.ico` → 可选代码签名 → ISCC 编译 → 签 Setup/verify → 输出 SHA256 |
| `Test-Installer.ps1` | 自动化验收：6 个阶段、92 项检查（单目录/双目录/去重/保留/删除分支，默认不碰真实数据根） |
| `ChineseSimplified.isl` | 安装向导简体中文翻译（取自 Inno Setup 官方 issrc `Files/Languages/ChineseSimplified.isl`，保留原文件头） |
| `icon.ico` | 构建时自动生成（多尺寸 16/32/48/64/128/256），已加入 `.gitignore`，不入库 |

## 构建

```powershell
# 默认版本 0.3.2；发行目录取 dist\DiskIntel-0.3.2
pwsh -File tools\build-installer.ps1

# 指定版本 / 发行目录 / 输出目录 / ISCC
pwsh -File tools\build-installer.ps1 -Version 0.3.2
pwsh -File tools\build-installer.ps1 -Version 0.3.2 -DistDir "D:\build\DiskIntel-0.3.2"
```

产物：

- `dist\DiskIntel-<版本>-Setup.exe`
- `dist\DiskIntel-<版本>-Setup.exe.sha256`

构建前需要已有的发行目录（`tools\package.ps1` 组装，含 `DiskIntel.exe`、`DiskIntelCLI.exe`、`goz\`、`tools\`、`README.txt`）。
ISCC 默认按用户范围安装在 `%LOCALAPPDATA%\Programs\Inno Setup 6\ISCC.exe`；若装在别处用 `-IsccPath` 指定。

## 代码签名（可选）

无证书时脚本照常构建，只在结尾打印「未签名」警告 + SHA256 + 分发建议；有证书时按顺序签名并校验：

| 参数 | 说明 |
| --- | --- |
| `-PfxPath <证书.pfx>` | PFX 证书文件；相对路径按仓库根解析 |
| `-PfxPassword <字符串>` | PFX 口令（可空；会出现在命令行/进程列表中，建议本机构建使用） |
| `-CertificateThumbprint <40位SHA1指纹>` | 从本机证书库取证书（先 CurrentUser\My，再 LocalMachine\My） |
| `-TimestampUrl <URL>` | 时间戳服务，默认 `http://timestamp.digicert.com`；传空串则不打时间戳 |
| `-SigntoolPath <signtool.exe>` | 可选，手工指定 signtool（默认自动探测 Windows SDK 10/8.1 或 PATH） |
| 环境变量 `SIGN_CERT_THUMBPRINT` | 未显式给证书时的指纹兜底 |

签名流程：

1. 用 `signtool sign /fd SHA256 /tr <时间戳> /td SHA256` 依次签名发行目录里的 `DiskIntel.exe`、`DiskIntelCLI.exe`（会修改 `-DistDir` 内的这两个文件）；
2. ISCC 编译 Setup；
3. 签名 `dist\DiskIntel-<版本>-Setup.exe`；
4. `signtool verify /pa /v` 校验 Setup，失败即构建失败；
5. 最后再计算 SHA256，确保 `.sha256` 对应最终（已签名）文件。

注意：签名动作会直接改写 `-DistDir` 里的两个 exe；若后续 ISCC 编译或 verify 失败，发行目录中的 exe 可能已带测试签名，重新打包前请用 `tools\package.ps1` 重新生成发行目录。

证书来源互斥：`-PfxPath` 与 `-CertificateThumbprint / SIGN_CERT_THUMBPRINT` 不能同时给。
分发建议：在 GitHub Release 附上 `.sha256`；详细流程见 `docs\签名与分发.md`。

## 安装 / 卸载行为

- 纯用户级安装：默认 `{localappdata}\Programs\DiskIntel`，`PrivilegesRequired=lowest`，不弹 UAC。
- 安装向导「数据目录」页：默认 `D:\disk-intel-data`，可改；写入 `HKCU\Software\DiskIntel\DataRoot`，并给程序快捷方式加 `--data-root "<目录>"`（0.3.x 程序仍以环境变量/命令行参数为准，靠快捷方式参数保证所选目录生效）。
- 快照目录：程序把自定义快照目录写入 `HKCU\Software\DiskIntel\SnapshotDir`（安装包不写）；卸载时实时读取：
  - 目标 1：`<DataRoot>\snapshots`；
  - 目标 2：`SnapshotDir`（存在且与目标 1 不同才加入，大小写不敏感去重）；
  - 勾选删除时依次清理目标目录当前层的 `*.sqlite`、`index.json`、`notes.json`、`.create.lock`；目录本身与其它文件保留；两个目录都不存在时只记录“跳过”。
- 卸载页「同时删除快照数据（不可恢复）」默认不勾；勾选或 `unins000.exe /DELSNAPSHOTS=1` 才删除。卸载结束提示会列出清理了哪些目录、各删几个文件、数据根保留在哪；静默卸载把同样信息写进 `/LOG`（`卸载快照目标目录数`、`快照目录清理`、`快照删除文件总数`）。
- 其它卸载动作：结束运行中的 `DiskIntel.exe / DiskIntelCLI.exe`；删除程序文件、开始菜单/桌面快捷方式、卸载登记、`HKCU\Software\DiskIntel`（DataRoot 由 Inno 清、SnapshotDir 由代码清）；删除 `%APPDATA%\DiskIntel`；尝试调 `{app}\tools\uninstall.ps1` 清引擎桥计划任务（无管理员失败不阻断）。

## 测试

```powershell
pwsh -File tools\installer\Test-Installer.ps1
```

- 必须以普通权限运行（管理员模式下卸载器会真的清计划任务，脚本会拒绝）；运行中的 `DiskIntel` 进程也会让脚本中止。
- 测试目录：`D:\_installtest\run-<时间戳>`；双目录场景固定用 `D:\_installtest\snapA\snapshots` 与 `E:\_installtest\snapB`（E 盘不可用时退回 `D:\_installtest\snapB`），测试结束保留不删。
- 若本机已安装磁盘情报系统：自动备份卸载登记/开始菜单/桌面快捷方式/`HKCU\Software\DiskIntel` 值/`%APPDATA%\DiskIntel`，测试结束原样恢复；真实程序目录不会被卸载。
- 6 个阶段：①安装校验 ②默认卸载保留 ③单目录 `/DELSNAPSHOTS=1` ④双目录默认保留 ⑤双目录同时清理（含带尾斜杠的 SnapshotDir） ⑥SnapshotDir 与默认目录相同时去重。共 92 项检查。

## Inno Setup 6.7 实测注意

1. **卸载器不能创建向导页**：`InitializeUninstall` 里调用 `CreateCustomPage` / `CreateInputOptionPage` 会直接
   `Internal error: Cannot call "CreateInputOptionPage" function during Uninstall`。本卸载页改用
   `CreateCustomForm` + `TNewCheckBox` 自绘（已实测）。
2. **`CreateCustomForm` 的宽高在 6.7 会自动按 DPI/字体缩放**：参数要传未缩放的 96-DPI 设计值（如 `580, 360`），
   再对子控件用 `ScaleX/ScaleY`；传 `ScaleX(580)` 会二次放大。脚本里另用 `WizardSizePercent=100` 定死基准。
3. 静默卸载脚本化删除快照：`unins000.exe /VERYSILENT /SUPPRESSMSGBOXES /NORESTART /DELSNAPSHOTS=1`。
4. 卸载日志 `/LOG=...` 可看到 `taskkill`、每个快照目录的清理记录、`tools\uninstall.ps1` 退出码。
5. 安装包不写 `SnapshotDir`；当前 src（HEAD f99c4d6）尚未写该值，卸载器已按新协议就绪，等程序侧写入后即生效（没有时只清 `DataRoot\snapshots`）。

## 0.3.2 实测记录（2026-11-13）

- 构建：`dist\DiskIntel-0.3.2-Setup.exe`，16.22 MB（17,003,387 字节），
  SHA256 `88a79e69ff0c3f69dd927d449103488cae9fe65a3fdd49ade2d618138b5a06fe`（未签名构建）。
- 自动化验收：**92 / 92 通过**，证据目录 `D:\_installtest\run-20261113-182651`（含 10 份安装/卸载日志）。
- 双目录实测：`D:\_installtest\snapA\snapshots` 与 `E:\_installtest\snapB` 各放 `a.sqlite/index.json/.create.lock/notes.json/keep.txt`；
  默认卸载全部保留；`/DELSNAPSHOTS=1` 两目录共删 8 个快照文件、`keep.txt` 与目录保留；去重场景只清一次、另一目录不动。
- 本机已有 0.3.2 安装：测试期间备份并恢复卸载登记/快捷方式/注册表/`%APPDATA%`，真实程序目录未动。

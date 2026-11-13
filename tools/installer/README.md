# tools\installer — Windows 安装包（Inno Setup 6）

本目录是「磁盘情报系统 / DiskIntel」的安装包工程，配套构建与验收脚本。

## 文件

| 文件 | 说明 |
| --- | --- |
| `DiskIntel.iss` | Inno Setup 6 脚本（安装向导数据目录页、快捷方式、卸载自定义页、快照删除选项、引擎桥清理） |
| `build-installer.ps1` | 构建脚本：从 `assets/icon.png` 生成多尺寸 `icon.ico` → 调 ISCC 编译 → 输出 Setup.exe 与 SHA256 |
| `Test-Installer.ps1` | 自动化验收：静默安装/卸载/删除快照分支，共 49 项检查（默认不碰真实数据根） |
| `ChineseSimplified.isl` | 安装向导简体中文翻译（取自 Inno Setup 官方 issrc `Files/Languages/ChineseSimplified.isl`，保留原文件头） |
| `icon.ico` | 构建时自动生成（多尺寸 16/32/48/64/128/256），已加入 `.gitignore`，不入库 |

## 构建

```powershell
# 默认版本 0.3.1；发行目录取 dist\DiskIntel-0.3.1
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

## 安装 / 卸载行为

- 纯用户级安装：默认 `{localappdata}\Programs\DiskIntel`，`PrivilegesRequired=lowest`，不弹 UAC。
- 安装向导新增「数据目录」页：默认 `D:\disk-intel-data`，可改；写入 `HKCU\Software\DiskIntel\DataRoot`，并给程序快捷方式加 `--data-root "<目录>"`（当前 0.3.x 程序只认环境变量/命令行参数，不读注册表，靠快捷方式参数保证所选目录生效）。
- 开始菜单组「磁盘情报系统」：程序 / 打开数据目录 / 卸载；桌面图标为可选任务，默认不勾。
- 卸载：
  - 结束运行中的 `DiskIntel.exe` / `DiskIntelCLI.exe`；
  - 删除程序文件、开始菜单/桌面快捷方式、卸载登记、`HKCU\Software\DiskIntel`；
  - 删除 `%APPDATA%\DiskIntel`（检索历史、列宽等设置）；
  - 尝试调用 `{app}\tools\uninstall.ps1` 清理引擎桥计划任务（普通权限会失败，不阻断，弹提示）；
  - **快照默认保留**；自定义卸载页勾选「同时删除快照数据（不可恢复）」或传 `/DELSNAPSHOTS=1` 时，只删除 `<数据根>\snapshots` 下的 `*.sqlite`、`index.json`、`.create.lock`、`notes.json`（当前层），**不删除数据根目录本身**，其它文件一律不动。

## 测试

```powershell
pwsh -File tools\installer\Test-Installer.ps1
```

- 必须以普通权限运行（管理员模式下卸载器会真的清计划任务，脚本会拒绝）。
- 每次新建 `D:\_installtest\run-<时间戳>`，证据与日志保留不删。
- 测试前备份 `%APPDATA%\DiskIntel`，结束后恢复。
- 检查项：文件齐全、开始菜单/桌面快捷方式、快捷方式参数、HKCU 注册表、卸载登记、静默卸载清理、默认保留快照、`/DELSNAPSHOTS=1` 删除分支、非快照文件与数据根保留、引擎桥脚本调用记录。

## Inno Setup 6.7 实测注意

1. **卸载器不能创建向导页**：`InitializeUninstall` 里调用 `CreateCustomPage` / `CreateInputOptionPage` 会直接
   `Internal error: Cannot call "CreateInputOptionPage" function during Uninstall`。本卸载页改用
   `CreateCustomForm` + `TNewCheckBox` 自绘（已实测）。
2. **`CreateCustomForm` 的宽高在 6.7 会自动按 DPI/字体缩放**：参数要传未缩放的 96-DPI 设计值（如 `580, 330`），
   再对子控件用 `ScaleX/ScaleY`；传 `ScaleX(580)` 会二次放大。脚本里另用 `WizardSizePercent=100` 定死基准。
3. 静默卸载脚本化删除快照：`unins000.exe /VERYSILENT /SUPPRESSMSGBOXES /NORESTART /DELSNAPSHOTS=1`。
4. 卸载日志 `/LOG=...` 可看到 `taskkill`、快照删除、`tools\uninstall.ps1` 退出码。

## 0.3.1 实测记录（2026-11-13）

- 构建：`DiskIntel-0.3.1-Setup.exe`，16.20 MB（16,985,364 字节），
  SHA256 `8383d6a6affd9e82a8ee6e52bdb38db57c325931cea3c65b0ba0d3e6676a4317`。
- 自动化验收：49 / 49 通过（最终构建复跑证据目录 `D:\_installtest\run-20261113-164007`；早前一轮 `run-20261113-163436` 同结果）。
- 交互式安装向导：数据目录页默认 `D:\disk-intel-data`（`shot-wizard-dataroot.png`）；传 `/DATAROOT=` 时页面回显覆盖值
  并写入注册表（`shot-wizard-dataroot-override.png`）。
- 交互式卸载页：UIA 实测复选框默认 `Off`，勾选后卸载删除 4 个快照文件，`keep.txt` 与数据根保留；
  卸载完成提示「已删除快照文件：4 个。数据根目录已保留：…」并提示引擎桥需管理员权限
  （`shot-uninstall-checkbox-off.png` / `shot-uninstall-checkbox-on.png` / `shot-uninstall-result-msg.png`）。
- 运行中进程：先启动已安装的 `DiskIntel.exe`（窗口已出现），再静默卸载，进程被 `taskkill` 结束、程序目录删除。

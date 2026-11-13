# DiskIntel · 磁盘情报系统

[中文](#中文) | [English](#english)

一个 Windows 本地磁盘情报工具：**像 Everything 一样检索、像 WizTree 一样看空间、像 WhoShitsOnMyC 一样对比快照**，
并同时向人和 AI 提供数据（MCP / JSON 通道）。纯 Rust + egui 原生窗口，单进程约 100–160 MB，冷启动约 0.3 s。

![检索](docs/shots/01-检索-Hero.png)

## 特性

- **检索**：输入即搜（防抖 200 ms），支持 `ext:` / `size:` / `after:` / `kind:` / `path:` 语法；虚拟滚动、列宽可拖拽、搜索历史、Ctrl+F 聚焦。
- **低价值结果折叠**：临时文件、缓存、Office 备份、系统/冷门扩展名（.dll/.mui/.d/.map…）默认隐藏到列表末尾，
  底部显示分类计数，一键展开；**仅影响界面，MCP / JSON 输出不受影响**。
- **空间**：按物理磁盘分行展示（C/D/E、F、I/J），卷环形图 + 目录树图（面积 = 字节数）+ 分层列表；
  区分「实际占用」（实时 API）与「快照逻辑大小」（含硬链接与系统文件）。
- **对比**：任意两份快照，新增 / 消失 / 增长 / 缩小与净变化；真实 446 万行数据约 3 秒。
- **快照**：只读 SQLite 证据库；删除（移入待删除区）、备注；v4 字典化格式可把单份从 1.5 GB 压到 290 MB。
- **双引擎**：常驻 goz 实时索引（毫秒级）或快照离线查询，界面可切换、不可用时自动降级。
- **AI 友好**：内置 MCP stdio 服务端（9 个工具）与 `--json` 动词，与窗口同口径。

![空间](docs/shots/05-空间-按盘分组.png)
![折叠](docs/shots/03-低价值折叠.png)

## 系统要求

- Windows 10 1809+ / Windows 11，x64
- 约 20 MB 程序体积；快照数据另计（每份约 1.5 GB，转 v4 后约 290 MB）
- 普通用户权限即可运行（实时引擎的建库需要一次性管理员授权，可选）

## 安装

**安装包（推荐）**：下载 `DiskIntel-<version>-Setup.exe`，双击安装（按用户安装，免管理员权限）。
卸载在「设置 → 应用」或开始菜单；卸载时可勾选是否一并删除快照数据（默认不删）。

**绿色包**：下载 `DiskIntel-<version>-win64.zip`，解压任意目录，双击 `DiskIntel.exe`。

## 快速上手

| 操作 | 说明 |
|---|---|
| Ctrl+F / Ctrl+L | 聚焦搜索框 |
| Esc | 清空并失焦 |
| Ctrl+1..4 | 检索 / 空间 / 对比 / 快照 |
| F5 | 刷新当前视图 |
| ↑ / ↓ | 选择结果行或搜索历史 |
| Ctrl+C | 复制选中项路径 |

检索语法：`报告 ext:docx`、`size:>10m`、`after:2026-01-01`、`kind:file`、`path:D:\资料`，空格即 AND。

## 数据与隐私

- 全部数据留在本机，**无遥测**；「检查更新」只在你点击时访问 GitHub API。
- 快照默认存放在 `D:\disk-intel-data\snapshots`（安装时或 `--data-root` 可指定），程序只读打开。
- 搜索历史、列宽、设置存放在 `%APPDATA%\DiskIntel\`，可随时删除。
- 快照什么时候建？默认**手动点「扫描」**；需要开机自动建可在「工具」菜单勾选（默认关闭，同一小时内不会重复建）。

## 性能参考

| 指标 | 数值 |
|---|---:|
| 冷启动到首帧 | 约 0.2–0.3 s |
| 常驻内存 | 100–170 MB |
| 实时索引检索（134k 命中） | 7–15 ms |
| 快照全表检索 | 10–800 ms（随命中量） |
| 空间视图目录加载 | 6–11 ms |
| 对比两份 446 万行快照 | 约 3 s |
| 单份快照（v3 → v4 字典化） | 1.5 GB → 290 MB |

## 更新

- 软件内：帮助 → 检查更新…（读取 GitHub Releases 最新版本号，给出下载链接，不自动安装）。
- 手动：下载新版本安装包直接覆盖安装，或替换 `DiskIntel.exe`；**数据目录与设置不受影响**。
- 发布页：https://github.com/Vertin-lichen/diskintel/releases

## 从源码构建

```bash
cargo build --release
# 打包 zip：      pwsh -File tools/package.ps1 -Version 0.3.2
# 构建安装包：    pwsh -File tools/build-installer.ps1 -Version 0.3.2
```

可选组件：`goz`（MIT）提供 MFT/USN 实时索引；`DiskIntelCLI.exe` 负责建快照（发行包里已包含）。

## 许可证

MIT。第三方组件见 [THIRD_PARTY_NOTICES.md](THIRD_PARTY_NOTICES.md)。

---

## English

DiskIntel is a local disk intelligence tool for Windows: Everything-style file search, WizTree-style space
visualization, and WhoShitsOnMyC-style snapshot diff — in one native Rust/egui app (single process, ~100–160 MB,
~0.3 s cold start). It also exposes the same data to agents via MCP stdio and a JSON CLI.

Highlights: instant search with full syntax, low-value result folding (temp/cache/system/rare extensions,
UI-only), per-disk space overview with treemap drill-down, ~3-second diff over 4.4M rows, read-only snapshot
evidence store with optional dictionary-encoded (v4) format, dual engine (live goz index / offline snapshot),
and an MCP server.

Requirements: Windows 10 1809+ x64. Install with the per-user Setup, or unzip the portable package.
Data stays local; no telemetry. Licensed under MIT.
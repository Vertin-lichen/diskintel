# DiskIntel · 磁盘情报系统

[中文](#中文) | [English](#english)

**一个软件，三种能力，外加一层给 AI 的接口。**

DiskIntel 把三款各自独立的经典 Windows 工具整合进同一个原生窗口：

| 能力 | 对标工具 | 在本项目里 |
|---|---|---|
| 全盘毫秒级检索 | Everything | 输入即搜、完整查询语法、虚拟滚动、低价值结果折叠 |
| 磁盘空间可视化 | WizTree | 按物理盘分行、卷环形图、目录树图（面积=字节数）、分层列表 |
| 快照差异对比 | WhoShitsOnMyC | 任意两份快照的新增/消失/增长/缩小与净变化 |
| **AI 直接调用** | — | **内置 MCP 服务端（9 个工具）+ `--json` 动词，和界面同口径** |

纯 Rust + egui 原生窗口，单进程约 100–170 MB，冷启动约 0.3 s；数据全部留在本机。

![检索](docs/shots/01-检索-Hero.png)

## 为什么是「合三为一 + AI 接口」

- 三个工具各装一份、各建一份索引，磁盘和内存都翻倍；这里共用同一份快照/索引。
- 三者的数据口径天然冲突（Everything 看实时、WizTree 看占用、快照工具看历史），
  本项目把它们拆成不同视图，并在界面里显式标注「实时/快照」来源，避免拿逻辑大小当磁盘占用。
- **AI 接口是核心卖点**：同一个数据源，人点界面、AI 调 MCP/JSON，不需要另建一套索引或导出文件。

## AI 接口

MCP（stdio）内置 9 个工具：`diskintel_status`、`diskintel_search`、`diskintel_snapshots`、
`diskintel_du`、`diskintel_top`、`diskintel_diff`、`diskintel_files`、`diskintel_volumes`、`diskintel_scan`。

```bash
# 任意 MCP 客户端（Claude Desktop / Cursor / 自研 agent）里配置：
#   command: D:\DiskIntel\DiskIntel.exe
#   args:    ["--mcp"]

# 或者直接调 JSON 通道（脚本/CI 友好）：
diskintel.exe --json search "报告 ext:docx" --limit 20
diskintel.exe --json du --path C:\Users --depth 2
diskintel.exe --json diff --old 20261112-100008 --new 20261113-090000
```

界面里的「低价值折叠」**只影响 UI**，MCP/JSON 输出始终是完整结果集。

![快照对比](docs/shots/07-快照对比.png)

## 特性

- **检索**：输入即搜（防抖 200 ms），支持 `ext:` / `size:` / `after:` / `kind:` / `path:`；搜索历史、列宽拖拽、Ctrl+F 聚焦。
- **低价值结果折叠**：临时文件、缓存、系统/冷门扩展名默认隐藏，底部显示分类计数，一键展开；规则透明可配置，见 [折叠规则](docs/说明-折叠规则-20261113.md)。
- **空间**：按物理磁盘分行，卷环形图 + 树图 + 分层列表；区分「实际占用」（实时 API）与「快照逻辑大小」。
- **对比**：446 万行数据约 3 秒。
- **快照**：只读 SQLite 证据库；v4 字典化格式可把单份从 1.5 GB 压到约 290 MB。
- **双引擎**：常驻 goz 实时索引或快照离线查询，自动降级。
- **托盘常驻**：关窗口不退出，留在托盘（可关闭），方便随时被调用。
- **自动快照**：首次运行必建；默认每 7 天至少一份；可选每次启动建；旧快照按数量自动清理（移入待删除，不硬删）。

![空间](docs/shots/05-空间-按盘分组.png)
![折叠](docs/shots/03-低价值折叠.png)

## 系统要求

- Windows 10 1809+ / Windows 11，x64
- 约 20 MB 程序体积；快照数据另计（v3 约 1.5 GB/份，v4 约 290 MB/份）
- 普通用户权限即可运行（实时引擎建库需一次性管理员授权，可选）

## 安装

**安装包（推荐）**：下载 `DiskIntel-<version>-Setup.exe`，双击安装（按用户安装，免管理员）。
安装向导可指定**数据目录**；卸载时可勾选是否删除快照（默认不删，且会同时清理你改到其它盘上的快照目录）。

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

## 数据与隐私

- 全部数据留在本机，**无遥测**；「检查更新」只在你点击时访问 GitHub API。
- 快照默认存放在 `D:\disk-intel-data\snapshots`，**可在设置里改到其它盘**（安装时或「工具 → 设置」）。
- 搜索历史、列宽、设置存放在 `%APPDATA%\DiskIntel\`，可随时删除。

## 快照策略

| 场景 | 行为 |
|---|---|
| 首次运行 | 自动创建第一份快照（否则软件没有可用数据） |
| 日常 | 默认每 7 天至少自动建一份；可在设置里改天数 |
| 可选 | 「每次启动都建一份」，默认关闭 |
| 清理 | 超过保留份数（默认 8）的旧快照自动移入 `待删除/`，不永久删除 |
| 手动 | 工具栏「扫描」随时建 |

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
- 手动：下载新版本安装包直接覆盖安装，或替换 `DiskIntel.exe`；数据目录与设置不受影响。
- 发布页：https://github.com/Vertin-lichen/diskintel/releases

## 关于签名与安全提示

当前发布包**未做代码签名**：首次运行可能出现 SmartScreen「未知发布者」提示，选择「更多信息 → 仍要运行」即可。
每次 Release 都公布 SHA256，可用 `Get-FileHash` 校验；后续计划接入开源免费签名（见 [签名与分发](docs/签名与分发-20261113.md)）。

## 从源码构建

```bash
cargo build --release
# 打包 zip：      pwsh -File tools/package.ps1 -Version 0.3.3
# 构建安装包：    pwsh -File tools/build-installer.ps1 -Version 0.3.3
# 采集运行数据：  pwsh -File tools/bench.ps1 -Exe target\release\diskintel-rs.exe
```

## 关于这个项目（作者的话）

> 本人技术确实有限，**没有完成核心技术的自研**：MFT/USN 索引来自 goz 项目，界面框架来自 egui/eframe，
> 存储基于 SQLite，其余是常见的 Rust 生态组件。
> 我能做的是把它们拼成一个「检索 + 空间 + 快照 + AI 接口」的完整产品，并把口径、边界和取舍写清楚。
> 在性能和效率上，这个项目可能仍有不足（例如 v4 检索的排序开销、超大命中量时的折叠统计口径）。
> 如果它对你有用，欢迎基于本项目进行二次开发和创作；也欢迎直接提 issue 或 PR 指出问题。

## 许可证

MIT。第三方组件见 [THIRD_PARTY_NOTICES.md](THIRD_PARTY_NOTICES.md)。

---

## English

DiskIntel merges three classic Windows tools — Everything-style instant search, WizTree-style space
visualization, and WhoShitsOnMyC-style snapshot diff — into one native Rust/egui app, and adds an
**AI interface** (MCP stdio server with 9 tools + a JSON CLI) over the same data the UI shows.

Highlights: instant search with full syntax; low-value result folding (temp/cache/system/rare extensions,
UI-only and configurable); per-disk space overview with treemap drill-down; ~3-second diff over 4.4M rows;
read-only snapshot store with optional dictionary-encoded (v4) format; dual engine (live goz index / offline
snapshot); tray residency; automatic snapshots (first run, then at least every 7 days, retention-aware).

Requirements: Windows 10 1809+ x64. Per-user Setup (no admin) or portable zip. Data stays local; no telemetry.

Built on the shoulders of open-source projects (goz, egui/eframe, rusqlite/SQLite). The author makes no claim
of original core technology; contributions and forks are welcome. Licensed under MIT.
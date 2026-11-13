# 第三方组件与许可证

本项目以 MIT 许可证发布，但依赖以下第三方组件，它们各自的许可证同样适用：

| 组件 | 用途 | 许可证 |
|---|---|---|
| [goz](https://github.com/mustafaahci/goz) | MFT / USN 实时索引（g.oz 引擎，可选组件） | MIT |
| [egui / eframe](https://github.com/emilk/egui) | 原生 UI 框架 | MIT 或 Apache-2.0 |
| [rusqlite](https://github.com/rusqlite/rusqlite) | SQLite 绑定 | MIT |
| [SQLite](https://sqlite.org) | 快照存储 | Public Domain |
| 其它 Rust crate | 见 Cargo.lock | 各自 MIT / Apache-2.0 |

发行包内 `goz\goz.exe`、`goz\gozd.exe` 来自 goz 项目，保留其原始许可证与版权声明。
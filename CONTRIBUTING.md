# 贡献指南

欢迎 issue 与 PR。提交前请确认：

1. `cargo test --release` 全绿；
2. `cargo build --release` 无错误；
3. 只改与主题相关的文件，提交信息用 `type(scope): 描述` 格式；
4. UI 改动请附截图（可用 `--shot` 无头截图：`diskintel-rs.exe --shot out.png --frames 40 --view space`）；
5. 不提交构建产物（target/、dist/）与快照数据（*.sqlite）。

开发环境：Windows 10+ x64、Rust stable（edition 2024）。
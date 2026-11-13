# 发布到 GitHub 操作手册

> 目标仓库：https://github.com/Vertin-lichen/diskintel （公开）
> 许可证：MIT　｜　默认分支：master

## 1. 首次创建仓库

```bash
# 在仓库根目录执行（已登录 gh）
gh repo create Vertin-lichen/diskintel --public --source=. --remote=origin --push
```

> 推送前请确认：工作树干净、无密钥、无个人路径（见 docs/OPENSOURCE_READY.md）。

## 2. 仓库设置建议

- Description：`Everything-style search + WizTree-style space + snapshot diff, in one native Rust app for Windows.`
- Topics：`windows`, `disk-usage`, `file-search`, `rust`, `egui`, `snapshot`, `treemap`, `mcp`, `everything`, `wiztree`
- 关闭 Wiki / Projects（可选）；开启 Issues。

## 3. 发布 Release

```bash
git tag -a v0.3.2 -m "DiskIntel 0.3.2"
git push origin v0.3.2

gh release create v0.3.2 \
  dist/DiskIntel-0.3.2-Setup.exe \
  dist/DiskIntel-0.3.2-win64.zip \
  --title "DiskIntel 0.3.2" \
  --notes-file docs/RELEASE_NOTES-0.3.2.md
```

- 资产命名固定：`DiskIntel-<version>-Setup.exe`、`DiskIntel-<version>-win64.zip`（软件内检查更新依赖该命名）。

## 4. 热更新链路

1. 用户在 帮助 → 检查更新… 时，程序请求 `https://api.github.com/repos/Vertin-lichen/diskintel/releases/latest`；
2. 比较 `tag_name` 与当前版本，提示新版本并给出 `html_url`；
3. 用户下载 Setup.exe 覆盖安装（或替换 exe），数据目录与 `%APPDATA%\DiskIntel` 保持不变；
4. 卸载时可选是否删除快照数据。

## 5. CI

`.github/workflows/ci.yml` 在每次 push / PR 上跑 `cargo test --release` 与 `cargo build --release`，
并上传 exe 作为 artifact；打 tag 后可手动下载产物做发行包。
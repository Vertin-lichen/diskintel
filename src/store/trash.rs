//! 待删除区：把文件 / 目录移入 <data_root>\待删除\ 并登记 MANIFEST.md。
//!
//! 项目公约：**不永久删除**。程序里的「删除」只有两步：
//!
//! 1. 把目标移到数据根下的「待删除」目录（重名自动追加 -2 / -3，不覆盖任何已有文件）；
//! 2. 在 <待删除>\MANIFEST.md 追加一行（时间 / 文件 / 原路径 / 新路径 / 字节 / 原因 / 操作者）。
//!
//! 永久删除由使用者本人执行，程序不提供，也不调用回收站。
//!
//! 移动优先用 rename（同盘瞬间完成）；跨盘时 Windows 的 rename 会报 ERROR_NOT_SAME_DEVICE，
//! 此时回退到「先完整复制，复制成功后才删原文件」。任何一步失败都保证原文件仍在：
//! - 复制失败：原文件未动，目标可能留半成品（错误信息里写明路径）；
//! - 登记 MANIFEST 失败：把已移动的文件搬回原路径再报错，不留「删了却没登记」的悬案。

use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};

/// 数据根下的暂存目录名。
pub const TRASH_DIR: &str = "待删除";
/// 登记文件名。
pub const MANIFEST_FILE: &str = "MANIFEST.md";
/// 表格头：文件不存在（或为空）时先写这两行，已有表格则只追加数据行。
const MANIFEST_HEADER: &str =
    "| 时间 | 文件 | 原路径 | 新路径 | 字节 | 原因 | 操作者 |\n|---|---|---|---|---:|---|---|\n";

/// 把 path 移入 <data_root>\待删除\ 并登记 MANIFEST，返回移动后的新路径。
///
/// - reason / operator 原样写进 MANIFEST，便于日后追溯（例如「快照管理页手动删除」/「UI」）；
/// - 目标目录不存在会自动创建；同名文件已存在时在扩展名前追加 -2 / -3…；
/// - 校验失败（源文件不存在、目录建不出来、磁盘满）返回可读的中文错误，绝不半途丢文件。
pub fn move_to_trash(
    path: &Path,
    data_root: &Path,
    reason: &str,
    operator: &str,
) -> Result<PathBuf, String> {
    move_to_trash_in(&data_root.join(TRASH_DIR), path, reason, operator)
}

/// 移入**指定**的待删除目录（数据根之外的自定义快照目录用它：待删除就近放，移动才是 rename）。
pub fn move_to_trash_in(
    trash: &Path,
    path: &Path,
    reason: &str,
    operator: &str,
) -> Result<PathBuf, String> {
    let meta = fs::metadata(path)
        .map_err(|error| format!("找不到要移动的 {}：{error}", path.display()))?;
    let bytes = if meta.is_dir() {
        dir_size(path)
    } else {
        meta.len()
    };

    fs::create_dir_all(trash)
        .map_err(|error| format!("创建待删除目录 {} 失败：{error}", trash.display()))?;

    let target = next_target(trash, path);
    move_path(path, &target)?;

    let name = path
        .file_name()
        .map(|name| name.to_string_lossy().into_owned())
        .unwrap_or_else(|| path.display().to_string());
    if let Err(error) = append_manifest(&trash, &name, path, &target, bytes, reason, operator) {
        // MANIFEST 是这次删除的唯一凭据：写不进去就不算删成功，把文件搬回原处。
        return Err(match move_path(&target, path) {
            Ok(()) => format!("已移入待删除目录，但登记 MANIFEST 失败，文件已还原：{error}"),
            Err(rollback) => format!(
                "已移入待删除目录，但登记 MANIFEST 失败且还原失败：{error}；{rollback}（文件当前在两处：{} 与 {}，请手动确认）",
                target.display(),
                path.display()
            ),
        });
    }
    Ok(target)
}

/// 待删除目录里下一个可用的目标路径：<YYYYMMDD-HHMMSS>_<原名>，重名追加 -2 / -3…
fn next_target(trash: &Path, source: &Path) -> PathBuf {
    let stamp = chrono::Local::now().format("%Y%m%d-%H%M%S").to_string();
    let name = source
        .file_name()
        .map(|name| name.to_string_lossy().into_owned())
        .unwrap_or_else(|| "unnamed".to_owned());
    next_name_in(trash, &stamp, &name)
}

/// next_target 的可注入版本：时间戳由调用方给，测试才能确定性地验证重名后缀。
fn next_name_in(trash: &Path, stamp: &str, name: &str) -> PathBuf {
    let mut index = 1;
    loop {
        let candidate = trash.join(trash_name(stamp, name, index));
        if !candidate.exists() {
            return candidate;
        }
        index += 1;
    }
}

/// 拼一个不覆盖已有文件的待删除文件名；index=1 是首名，2 起追加 -N。
fn trash_name(stamp: &str, name: &str, index: usize) -> String {
    let path = Path::new(name);
    let stem = path.file_stem().and_then(|stem| stem.to_str()).unwrap_or(name);
    let extension = path.extension().and_then(|ext| ext.to_str());
    let suffix = if index <= 1 {
        String::new()
    } else {
        format!("-{index}")
    };
    match extension {
        Some(extension) if !extension.is_empty() => {
            format!("{stamp}_{stem}{suffix}.{extension}")
        }
        _ => format!("{stamp}_{stem}{suffix}"),
    }
}

/// 移动一个文件或目录树：先 rename，跨盘失败再复制 + 删源。
fn move_path(from: &Path, to: &Path) -> Result<(), String> {
    let rename_error = match fs::rename(from, to) {
        Ok(()) => return Ok(()),
        Err(error) => error,
    };
    copy_path(from, to).map_err(|error| {
        format!(
            "移动 {} → {} 失败：rename {rename_error}，复制 {error}",
            from.display(),
            to.display()
        )
    })?;
    remove_path(from).map_err(|error| {
        format!(
            "已复制到 {}，但删除原路径 {} 失败：{error}（原文件仍在，数据不会丢）",
            to.display(),
            from.display()
        )
    })
}

/// 递归复制（文件直接 copy，目录建好再逐个复制子项）。
fn copy_path(from: &Path, to: &Path) -> std::io::Result<()> {
    if from.is_dir() {
        fs::create_dir_all(to)?;
        for entry in fs::read_dir(from)? {
            let entry = entry?;
            copy_path(&entry.path(), &to.join(entry.file_name()))?;
        }
        Ok(())
    } else {
        fs::copy(from, to).map(|_| ())
    }
}

/// 删除已经成功复制过去的源路径（文件 remove_file / 目录 remove_dir_all）。
fn remove_path(path: &Path) -> std::io::Result<()> {
    if path.is_dir() {
        fs::remove_dir_all(path)
    } else {
        fs::remove_file(path)
    }
}

/// 目录体积：逐文件累加字节数（读不到的项按 0 计，不因为一个坏链接让登记失败）。
fn dir_size(path: &Path) -> u64 {
    let Ok(meta) = fs::metadata(path) else {
        return 0;
    };
    if !meta.is_dir() {
        return meta.len();
    }
    let mut total = 0;
    if let Ok(entries) = fs::read_dir(path) {
        for entry in entries.flatten() {
            total += dir_size(&entry.path());
        }
    }
    total
}

/// 追加一行到 MANIFEST；文件不存在时先写表头。
fn append_manifest(
    trash: &Path,
    name: &str,
    from: &Path,
    to: &Path,
    bytes: u64,
    reason: &str,
    operator: &str,
) -> Result<(), String> {
    let manifest = trash.join(MANIFEST_FILE);
    let has_header = fs::metadata(&manifest)
        .map(|meta| meta.len() > 0)
        .unwrap_or(false);
    let mut file = fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&manifest)
        .map_err(|error| format!("打开 {} 失败：{error}", manifest.display()))?;
    if !has_header {
        file.write_all(MANIFEST_HEADER.as_bytes())
            .map_err(|error| format!("写 MANIFEST 表头失败：{error}"))?;
    }
    let row = format!(
        "| {} | {} | {} | {} | {} | {} | {} |\n",
        chrono::Local::now().format("%Y-%m-%dT%H:%M:%S"),
        cell(name),
        cell(&from.display().to_string()),
        cell(&to.display().to_string()),
        bytes,
        cell(reason),
        cell(operator),
    );
    file.write_all(row.as_bytes())
        .map_err(|error| format!("写 MANIFEST 失败：{error}"))?;
    file.flush()
        .map_err(|error| format!("刷新 {} 失败：{error}", manifest.display()))
}

/// 表格单元格转义：竖线、换行会破坏 Markdown 表格，替换掉。
fn cell(text: &str) -> String {
    text.replace('|', "\\|").replace(['\n', '\r'], " ")
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 唯一临时目录：%TEMP%\diskintel-trash-tests\<纳秒>-<序号>。
    ///
    /// 按项目公约测试夹具不做永久删除（只写自己的临时目录，跑完留在 %TEMP% 里，
    /// 每次运行目录名唯一，互不影响）。
    fn unique_dir(tag: &str) -> PathBuf {
        use std::sync::atomic::{AtomicU64, Ordering};
        static SEQ: AtomicU64 = AtomicU64::new(0);
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0);
        let dir = std::env::temp_dir()
            .join("diskintel-trash-tests")
            .join(format!("{tag}-{nanos}-{}", SEQ.fetch_add(1, Ordering::Relaxed)));
        fs::create_dir_all(&dir).expect("创建临时目录");
        dir
    }

    fn write_file(path: &Path, bytes: &[u8]) {
        fs::create_dir_all(path.parent().expect("有父目录")).expect("建父目录");
        fs::write(path, bytes).expect("写测试文件");
    }

    /// 需求里的核心用例：移走之后原路径不存在、目标存在、MANIFEST 有这一行。
    #[test]
    fn move_file_leaves_source_and_logs_manifest() {
        let root = unique_dir("move");
        let data_root = root.join("data");
        let source = root.join("snapshots").join("20260101-000000.sqlite");
        write_file(&source, b"snapshot-bytes");

        let moved = move_to_trash(&source, &data_root, "快照管理页手动删除", "UI").expect("应当移动成功");

        assert!(!source.exists(), "原路径必须不存在");
        assert!(moved.exists(), "待删除目录里要有这个文件");
        assert_eq!(fs::read(&moved).unwrap(), b"snapshot-bytes", "内容不能变形");
        assert_eq!(moved.parent(), Some(data_root.join(TRASH_DIR).as_path()));

        let manifest = fs::read_to_string(data_root.join(TRASH_DIR).join(MANIFEST_FILE))
            .expect("MANIFEST 应当已创建");
        assert!(manifest.starts_with(MANIFEST_HEADER), "首行是表头：{manifest}");
        let rows: Vec<&str> = manifest
            .lines()
            .filter(|line| line.starts_with("| 2"))
            .collect();
        assert_eq!(rows.len(), 1, "只该有一行记录：{manifest}");
        let row = rows[0];
        assert!(row.contains("20260101-000000.sqlite"), "行里要有文件名：{row}");
        assert!(row.contains(&source.display().to_string()), "行里要有原路径：{row}");
        assert!(row.contains(&moved.display().to_string()), "行里要有新路径：{row}");
        assert!(row.contains(" 14 "), "行里要有字节数：{row}");
        assert!(row.contains("快照管理页手动删除"), "行里要有原因：{row}");
        assert!(row.ends_with("| UI |"), "行尾是操作者：{row}");
    }

    /// 同一秒删两份同名文件：第二份追加 -2，绝不覆盖第一份。
    #[test]
    fn same_name_gets_numeric_suffix() {
        let root = unique_dir("suffix");
        let trash = root.join(TRASH_DIR);
        fs::create_dir_all(&trash).expect("建待删除目录");
        let stamp = "20261113-120000";
        assert_eq!(
            trash_name(stamp, "20260101-000000.sqlite", 1),
            "20261113-120000_20260101-000000.sqlite"
        );
        assert_eq!(
            trash_name(stamp, "20260101-000000.sqlite", 2),
            "20261113-120000_20260101-000000-2.sqlite"
        );
        assert_eq!(trash_name(stamp, "a", 3), "20261113-120000_a-3");

        // 已存在的名字会被跳过（时间戳注入，避免跨秒抖动）
        fs::write(trash.join(trash_name(stamp, "x.sqlite", 1)), b"1").unwrap();
        fs::write(trash.join(trash_name(stamp, "x.sqlite", 2)), b"2").unwrap();
        let moved = next_name_in(&trash, stamp, "x.sqlite");
        assert!(
            moved.ends_with(format!("{stamp}_x-3.sqlite")),
            "前两个名字都占用了要给 -3：{}",
            moved.display()
        );
    }

    /// 表头只写一次，第二行直接追加。
    #[test]
    fn manifest_header_is_written_once() {
        let root = unique_dir("header");
        let data_root = root.join("data");
        for name in ["a.sqlite", "b.sqlite"] {
            let source = root.join(name);
            write_file(&source, b"bytes");
            move_to_trash(&source, &data_root, "快照管理页手动删除", "UI").expect("移动成功");
        }
        let manifest = fs::read_to_string(data_root.join(TRASH_DIR).join(MANIFEST_FILE)).unwrap();
        assert_eq!(
            manifest.matches("|---|---|---|---|---:|---|---|").count(),
            1,
            "{manifest}"
        );
        assert_eq!(
            manifest.lines().filter(|line| line.starts_with('|')).count(),
            4,
            "表头 2 行 + 2 条记录"
        );
    }

    /// 目录（含子文件）同样能整体移入待删除，字节数是子树之和。
    #[test]
    fn move_directory_tree() {
        let root = unique_dir("dir");
        let data_root = root.join("data");
        let source = root.join("staging").join("build-1");
        write_file(&source.join("part.bin"), b"0123456789");
        write_file(&source.join("nested").join("other.bin"), b"01234");

        let moved = move_to_trash(&source, &data_root, "测试目录", "测试").expect("目录移动成功");

        assert!(!source.exists());
        assert!(moved.join("nested").join("other.bin").exists());
        let manifest = fs::read_to_string(data_root.join(TRASH_DIR).join(MANIFEST_FILE)).unwrap();
        assert!(manifest.contains(" 15 "), "目录字节数应是 15：{manifest}");
    }

    /// 源路径不存在时明确报错，不建 MANIFEST。
    #[test]
    fn missing_source_reports_error() {
        let root = unique_dir("missing");
        let data_root = root.join("data");
        let error = move_to_trash(&root.join("nope.sqlite"), &data_root, "x", "y").unwrap_err();
        assert!(error.contains("找不到要移动的"), "{error}");
        assert!(!data_root.join(TRASH_DIR).join(MANIFEST_FILE).exists());
    }
}

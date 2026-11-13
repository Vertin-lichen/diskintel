//! 快照读取层：只做「按视口取一页」，不在内存里堆全量条目。
//!
//! mtime 一律是 Unix 秒（Python 侧 EFU 导入时已换算），不是 FILETIME；
//! 历史原型把它当 FILETIME 处理，导致时间列与日期过滤全错，此处修正。

use std::path::{Path, PathBuf};
use std::time::Instant;

use rusqlite::{Connection, OpenFlags};

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum SortBy {
    PathAsc,
    PathDesc,
    SizeDesc,
    SizeAsc,
    MtimeDesc,
    MtimeAsc,
    NameAsc,
    NameDesc,
}

impl SortBy {
    pub fn order_sql(self) -> &'static str {
        match self {
            SortBy::PathAsc => "path ASC",
            SortBy::PathDesc => "path DESC",
            SortBy::SizeDesc => "size DESC",
            SortBy::SizeAsc => "size ASC",
            SortBy::MtimeDesc => "mtime DESC",
            SortBy::MtimeAsc => "mtime ASC",
            // WITHOUT ROWID 主键即 path；按名排序先按路径近似（同名文件才需要真名排序）
            SortBy::NameAsc => "path ASC",
            SortBy::NameDesc => "path DESC",
        }
    }

    /// 该排序是否能走现有索引（避免全表扫描把 1.5 GB 页全驻留）。
    pub fn is_indexed(self) -> bool {
        matches!(self, SortBy::PathAsc | SortBy::PathDesc | SortBy::NameAsc | SortBy::NameDesc)
    }

    pub fn label(self) -> &'static str {
        match self {
            SortBy::PathAsc | SortBy::PathDesc => "路径",
            SortBy::SizeDesc | SortBy::SizeAsc => "大小",
            SortBy::MtimeDesc | SortBy::MtimeAsc => "修改时间",
            SortBy::NameAsc | SortBy::NameDesc => "名称",
        }
    }

    pub fn ascending(self) -> bool {
        matches!(
            self,
            SortBy::PathAsc | SortBy::SizeAsc | SortBy::MtimeAsc | SortBy::NameAsc
        )
    }

    pub fn reversed(self) -> Self {
        match self {
            SortBy::PathAsc => SortBy::PathDesc,
            SortBy::PathDesc => SortBy::PathAsc,
            SortBy::SizeDesc => SortBy::SizeAsc,
            SortBy::SizeAsc => SortBy::SizeDesc,
            SortBy::MtimeDesc => SortBy::MtimeAsc,
            SortBy::MtimeAsc => SortBy::MtimeDesc,
            SortBy::NameAsc => SortBy::NameDesc,
            SortBy::NameDesc => SortBy::NameAsc,
        }
    }
}

#[derive(Clone)]
pub struct Row {
    pub path: String,
    pub size: i64,
    pub mtime: i64,
    pub is_dir: bool,
}

/// 目录聚合行：来自 folder_sizes 预聚合表，不是逐文件扫描。
#[derive(Clone)]
pub struct FolderRow {
    pub path: String,
    pub size: i64,
    pub file_count: i64,
}

/// 快照目录下的一份快照文件。
#[derive(Clone)]
pub struct SnapshotFile {
    pub path: PathBuf,
    pub name: String,
    pub bytes: u64,
    pub entries: u64,
    pub created_at: i64,
    /// 快照的来源 EFU 指纹：两份相同 = 同一份扫描数据，diff 必为零。
    pub source_sha256: String,
}

pub struct Snapshot {
    conn: Option<Connection>,
    /// v4 字典化快照（冷存储）；与 conn 互斥，二者最多一个为 Some。
    v4: Option<crate::store::v4::V4Snapshot>,
    pub path: PathBuf,
    pub name: String,
    pub total_entries: u64,
    pub corpus_bytes: u64,
    pub file_bytes: u64,
    pub open_ms: f64,
    pub format_version: i64,
    pub roots: Vec<String>,
    pub created_at: i64,
    pub note: String,
    pub source_efu: String,
}

impl Snapshot {
    /// 打开快照目录下最新的一份；没有快照时返回空快照（窗口照常打开）。
    pub fn open_latest(root: &Path) -> Result<Self, String> {
        let files = list_snapshots(root);
        match files.first() {
            Some(newest) => Self::open_path(&newest.path),
            None => Self::open_empty(),
        }
    }

    /// 空快照：没有连接，所有查询返回空，用于「第一次运行还没扫描」的空态。
    pub fn open_empty() -> Result<Self, String> {
        Ok(Self {
            conn: None,
            v4: None,
            path: PathBuf::new(),
            name: "-".to_owned(),
            total_entries: 0,
            corpus_bytes: 0,
            file_bytes: 0,
            open_ms: 0.0,
            format_version: 0,
            roots: Vec::new(),
            created_at: 0,
            note: String::new(),
            source_efu: String::new(),
        })
    }

    pub fn is_empty(&self) -> bool {
        self.conn.is_none() && self.v4.is_none()
    }

    pub fn is_v4(&self) -> bool {
        self.v4.is_some()
    }

    /// 打开指定快照文件，只读、不做任何写入。
    ///
    /// 先看 meta.format_version：4 走字典化冷存储读层，1–3 走原有 v3 读层。
    pub fn open_path(path: &Path) -> Result<Self, String> {
        if let Some(version) = peek_format_version(path) {
            if version >= 4 {
                let v4 = crate::store::v4::V4Snapshot::open(path)?;
                let note = v4.meta.get("note").cloned().unwrap_or_default();
                let source_efu = v4.meta.get("source_efu").cloned().unwrap_or_default();
                return Ok(Self {
                    conn: None,
                    v4: Some(crate::store::v4::V4Snapshot::open(path)?),
                    path: path.to_path_buf(),
                    name: path.file_stem().and_then(|s| s.to_str()).unwrap_or("-").to_owned(),
                    total_entries: v4.total_entries,
                    corpus_bytes: v4.corpus_bytes,
                    file_bytes: v4.file_bytes,
                    open_ms: v4.open_ms,
                    format_version: v4.format_version,
                    roots: Vec::new(),
                    created_at: v4.created_at,
                    note,
                    source_efu,
                });
            }
        }
        let started = Instant::now();
        let conn = Connection::open_with_flags(path, OpenFlags::SQLITE_OPEN_READ_ONLY)
            .map_err(|e| format!("打开 {} 失败：{e}", path.display()))?;
        // 与 Python 侧同一套只读参数，但 mmap 上限收到 256 MiB：整库映射会让
        // 全表扫描把 1.5 GB 页全部驻留进工作集（实测 RSS 达 1.1 GB）。
        let _ = conn.execute_batch(
            "PRAGMA query_only=ON; PRAGMA mmap_size=67108864; PRAGMA cache_size=-8192; PRAGMA temp_store=FILE;",
        );

        let meta_text = |key: &str| -> Option<String> {
            conn.query_row("SELECT value FROM meta WHERE key = ?1", [key], |r| {
                r.get::<_, String>(0)
            })
            .ok()
        };
        let meta_int = |key: &str| -> i64 {
            meta_text(key)
                .and_then(|v| v.trim().parse::<i64>().ok())
                .unwrap_or(0)
        };

        let mut total_entries = (meta_int("file_count") + meta_int("dir_count")).max(0) as u64;
        let mut corpus_bytes = meta_int("total_size").max(0) as u64;
        if total_entries == 0 {
            total_entries = conn
                .query_row("SELECT COUNT(*) FROM entries", [], |r| r.get::<_, i64>(0))
                .unwrap_or(0)
                .max(0) as u64;
        }
        if corpus_bytes == 0 {
            corpus_bytes = conn
                .query_row("SELECT SUM(size) FROM entries WHERE is_dir=0", [], |r| {
                    r.get::<_, i64>(0)
                })
                .unwrap_or(0)
                .max(0) as u64;
        }
        let roots: Vec<String> = meta_text("volume_roots")
            .and_then(|raw| serde_roots(&raw))
            .unwrap_or_default();
        let note = meta_text("note").unwrap_or_default();
        let source_efu = meta_text("source_efu").unwrap_or_default();
        let created_at = meta_int("created_at");
        let format_version = meta_int("format_version");
        let file_bytes = std::fs::metadata(path).map(|m| m.len()).unwrap_or(0);
        let name = path
            .file_stem()
            .and_then(|s| s.to_str())
            .unwrap_or("-")
            .to_owned();

        Ok(Self {
            conn: Some(conn),
            v4: None,
            path: path.to_path_buf(),
            name,
            total_entries,
            corpus_bytes,
            file_bytes,
            open_ms: started.elapsed().as_secs_f64() * 1000.0,
            format_version,
            roots,
            created_at,
            note,
            source_efu,
        })
    }

    /// 全表扫描后把页缓存还给系统，压住 RSS 峰值。
    pub fn shrink(&self) {
        if self.v4.is_some() {
            return;
        }
        if let Some(conn) = self.conn.as_ref() {
            let _ = conn.execute_batch("PRAGMA shrink_memory;");
        }
    }

    pub fn conn(&self) -> Option<&Connection> {
        self.conn.as_ref()
    }

    /// 无条件命中数直接用 meta 计数，避免对 446 万行做 COUNT。
    pub fn count(&self, filter: &crate::filter::Filter) -> u64 {
        if let Some(v4) = self.v4.as_ref() {
            return v4.count_filter(filter);
        }
        if self.conn.is_none() {
            return 0;
        }
        if filter.is_none() {
            return self.total_entries;
        }
        let (where_sql, params) = filter.where_clause();
        let sql = format!("SELECT COUNT(*) FROM entries WHERE {where_sql}");
        let conn = self.conn.as_ref().unwrap();
        let mut stmt = match conn.prepare_cached(&sql) {
            Ok(s) => s,
            Err(_) => return 0,
        };
        stmt.query_row(rusqlite::params_from_iter(params.iter()), |r| r.get::<_, i64>(0))
            .unwrap_or(0)
            .max(0) as u64
    }

    /// 只取 [offset, offset+limit) 这一段；返回 (行, 本页 SQL 耗时毫秒)。
    pub fn page(
        &self,
        filter: &crate::filter::Filter,
        sort: SortBy,
        offset: u64,
        limit: u64,
    ) -> (Vec<Row>, f64) {
        if let Some(v4) = self.v4.as_ref() {
            return v4.page_filter(filter, sort, offset, limit);
        }
        let started = Instant::now();
        let Some(conn) = self.conn.as_ref() else {
            return (Vec::new(), 0.0);
        };
        let (where_sql, mut params) = filter.where_clause();
        let sql = format!(
            "SELECT path, size, mtime, is_dir FROM entries \
             WHERE {where_sql} ORDER BY {} LIMIT ? OFFSET ?",
            sort.order_sql()
        );
        let mut stmt = match conn.prepare_cached(&sql) {
            Ok(s) => s,
            Err(_) => return (Vec::new(), 0.0),
        };
        params.push(limit.to_string());
        params.push(offset.to_string());
        let rows = stmt
            .query_map(rusqlite::params_from_iter(params.iter()), |r| {
                Ok(Row {
                    path: r.get(0)?,
                    size: r.get(1)?,
                    mtime: r.get(2)?,
                    is_dir: r.get::<_, i64>(3)? != 0,
                })
            })
            .map(|iter| iter.flatten().collect::<Vec<_>>())
            .unwrap_or_default();
        (rows, started.elapsed().as_secs_f64() * 1000.0)
    }

    /// 根层目录（各卷根）。
    pub fn roots_rows(&self) -> Vec<FolderRow> {
        if let Some(v4) = self.v4.as_ref() {
            let mut out = v4.roots_rows();
            out.sort_by(|a, b| b.size.cmp(&a.size));
            return out;
        }
        let Some(conn) = self.conn.as_ref() else {
            return Vec::new();
        };
        let Ok(mut stmt) = conn.prepare_cached(
            "SELECT path, size, file_count FROM folder_sizes WHERE parent_path = '' ORDER BY size DESC",
        ) else {
            return Vec::new();
        };
        stmt.query_map([], |r| {
            Ok(FolderRow {
                path: r.get(0)?,
                size: r.get(1)?,
                file_count: r.get(2)?,
            })
        })
        .map(|iter| iter.flatten().collect())
        .unwrap_or_default()
    }

    /// 目录的直接子目录，按大小降序。
    pub fn child_dirs(&self, parent: &str, limit: u64) -> Vec<FolderRow> {
        if let Some(v4) = self.v4.as_ref() {
            return v4.child_dirs_rows(parent, limit);
        }
        let Some(conn) = self.conn.as_ref() else {
            return Vec::new();
        };
        let Ok(mut stmt) = conn.prepare_cached(
            "SELECT path, size, file_count FROM folder_sizes \
             WHERE parent_path = ?1 ORDER BY size DESC LIMIT ?2",
        ) else {
            return Vec::new();
        };
        stmt.query_map([parent, &limit.to_string()], |r| {
            Ok(FolderRow {
                path: r.get(0)?,
                size: r.get(1)?,
                file_count: r.get(2)?,
            })
        })
        .map(|iter| iter.flatten().collect())
        .unwrap_or_default()
    }

    /// 目录的直接文件，按大小降序。
    pub fn child_files(&self, parent: &str, limit: u64) -> Vec<Row> {
        if let Some(v4) = self.v4.as_ref() {
            return v4.child_files_rows(parent, limit);
        }
        let Some(conn) = self.conn.as_ref() else {
            return Vec::new();
        };
        let Ok(mut stmt) = conn.prepare_cached(
            "SELECT path, size, mtime, is_dir FROM entries \
             WHERE parent_path = ?1 AND is_dir = 0 ORDER BY size DESC LIMIT ?2",
        ) else {
            return Vec::new();
        };
        stmt.query_map([parent, &limit.to_string()], |r| {
            Ok(Row {
                path: r.get(0)?,
                size: r.get(1)?,
                mtime: r.get(2)?,
                is_dir: r.get::<_, i64>(3)? != 0,
            })
        })
        .map(|iter| iter.flatten().collect())
        .unwrap_or_default()
    }

    /// 目录自身的聚合（大小 / 文件数）。
    pub fn dir_info(&self, path: &str) -> Option<FolderRow> {
        if let Some(v4) = self.v4.as_ref() {
            return v4
                .dir_info(path)
                .map(|(path, size, file_count)| FolderRow { path, size, file_count });
        }
        let conn = self.conn.as_ref()?;
        conn.query_row(
            "SELECT path, size, file_count FROM folder_sizes WHERE path = ?1",
            [path],
            |r| {
                Ok(FolderRow {
                    path: r.get(0)?,
                    size: r.get(1)?,
                    file_count: r.get(2)?,
                })
            },
        )
        .ok()
    }

    /// 某个路径的祖先链（从根到自身），用于层级导航。
    pub fn ancestors(&self, path: &str) -> Vec<String> {
        if let Some(v4) = self.v4.as_ref() {
            return v4.ancestors(path);
        }
        let mut out = Vec::new();
        let mut current = path.to_owned();
        for _ in 0..64 {
            out.push(current.clone());
            match parent_dir(&current) {
                Some(parent) if !parent.is_empty() => current = parent,
                _ => break,
            }
        }
        out.reverse();
        out
    }
}

/// 从 "["C:\\", "D:\\"]" 这种 meta 值里取出卷根，避免为了一个字段引入 serde。
fn serde_roots(raw: &str) -> Option<Vec<String>> {
    let trimmed = raw.trim().trim_start_matches('[').trim_end_matches(']');
    let mut out = Vec::new();
    for piece in trimmed.split(',') {
        let value = piece.trim().trim_matches('"').replace("\\\\", "\\");
        if !value.is_empty() {
            out.push(value);
        }
    }
    if out.is_empty() { None } else { Some(out) }
}

/// Unix 秒 → 本地时间字符串（yyyy-MM-dd HH:mm）。
pub fn format_unix(secs: i64) -> String {
    if secs <= 0 {
        return "-".to_owned();
    }
    match chrono::DateTime::from_timestamp(secs, 0) {
        Some(utc) => utc
            .with_timezone(&chrono::Local)
            .format("%Y-%m-%d %H:%M")
            .to_string(),
        None => "-".to_owned(),
    }
}

pub fn human_size(bytes: i64) -> String {
    const UNITS: [&str; 6] = ["B", "KB", "MB", "GB", "TB", "PB"];
    let mut value = bytes.max(0) as f64;
    let mut unit = 0;
    while value >= 1024.0 && unit < UNITS.len() - 1 {
        value /= 1024.0;
        unit += 1;
    }
    if unit == 0 {
        format!("{value:.0} {}", UNITS[unit])
    } else {
        format!("{value:.1} {}", UNITS[unit])
    }
}

/// 取路径最后一段作为显示名，避免表格被长路径撑开。
pub fn file_name(path: &str) -> &str {
    let trimmed = path.trim_end_matches(['\\', '/']);
    trimmed.rsplit(['\\', '/']).next().unwrap_or(trimmed)
}

/// 取父目录（保留末尾反斜杠的根路径形态）。
pub fn parent_dir(path: &str) -> Option<String> {
    let trimmed = path.trim_end_matches(['\\', '/']);
    if trimmed.is_empty() {
        return None;
    }
    let index = trimmed.rfind(['\\', '/'])?;
    if index == 0 {
        return Some(trimmed[..=0].to_owned());
    }
    let parent = &trimmed[..index];
    if parent.ends_with(':') || parent.is_empty() {
        Some(format!("{parent}\\"))
    } else {
        Some(parent.to_owned())
    }
}

/// Rust 的 format 不支持千分位分隔符，这里手写一个。
pub fn thousands(value: u64) -> String {
    let digits = value.to_string();
    let mut out = String::with_capacity(digits.len() + digits.len() / 3);
    for (index, ch) in digits.chars().enumerate() {
        if index > 0 && (digits.len() - index) % 3 == 0 {
            out.push(',');
        }
        out.push(ch);
    }
    out
}

/// 列出快照目录下所有 .sqlite，按名字倒序（快照名是时间戳，字典序即时间序）。
pub fn list_snapshots(root: &Path) -> Vec<SnapshotFile> {
    let mut out = Vec::new();
    let Ok(entries) = std::fs::read_dir(root) else {
        return out;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.extension().and_then(|s| s.to_str()) != Some("sqlite") {
            continue;
        }
        let name = path
            .file_stem()
            .and_then(|s| s.to_str())
            .unwrap_or("-")
            .to_owned();
        let bytes = entry.metadata().map(|m| m.len()).unwrap_or(0);
        let (entries, meta_created, source_sha256) = read_meta(&path);
        // 创建时间优先取文件 mtime（快照名、meta.created_at、文件时间三者一致时最可信；
        // 手工拷贝 / 改名后 mtime 仍能反映这份文件的真实生成时刻），读不到才退 meta.created_at。
        let mtime = file_mtime_unix(&path);
        let created_at = if mtime > 0 { mtime } else { meta_created };
        out.push(SnapshotFile {
            path,
            name,
            bytes,
            entries,
            created_at,
            source_sha256,
        });
    }
    out.sort_by(|a, b| b.name.cmp(&a.name));
    out
}

fn read_meta(path: &Path) -> (u64, i64, String) {
    let Ok(conn) = Connection::open_with_flags(path, OpenFlags::SQLITE_OPEN_READ_ONLY) else {
        return (0, 0, String::new());
    };
    let value = |key: &str| -> i64 {
        conn.query_row("SELECT CAST(COALESCE((SELECT value FROM meta WHERE key=?1),'0') AS INTEGER)", [key], |r| r.get::<_, i64>(0))
            .unwrap_or(0)
    };
    let text = |key: &str| -> String {
        conn.query_row("SELECT value FROM meta WHERE key=?1", [key], |r| r.get::<_, String>(0))
            .unwrap_or_default()
    };
    let entries = (value("file_count") + value("dir_count")).max(0) as u64;
    (entries, value("created_at"), text("source_sha256"))
}
/// 一份快照的创建时间（Unix 秒）：优先文件 mtime，读不到退 meta.created_at。
///
/// 保留策略（每 7 天、保留 N 份）以它为准；文件被手工拷贝后 mtime 仍是拷贝前的时刻不算准，
/// 但比 meta 缺失 / 被改写更可靠。
pub fn snapshot_created_unix(path: &Path) -> i64 {
    let mtime = file_mtime_unix(path);
    if mtime > 0 {
        return mtime;
    }
    read_meta(path).1.max(0)
}

/// 文件 mtime（Unix 秒）；读不到返回 0。
fn file_mtime_unix(path: &Path) -> i64 {
    let Ok(modified) = std::fs::metadata(path).and_then(|meta| meta.modified()) else {
        return 0;
    };
    modified
        .duration_since(std::time::UNIX_EPOCH)
        .map(|elapsed| elapsed.as_secs() as i64)
        .unwrap_or(0)
}

/// 只读 peek 一个 sqlite 的 meta.format_version，用于选择读层（也用于判断 v3 / v4）。
pub fn peek_format_version(path: &Path) -> Option<i64> {
    let conn = Connection::open_with_flags(path, OpenFlags::SQLITE_OPEN_READ_ONLY).ok()?;
    conn.query_row(
        "SELECT CAST(value AS INTEGER) FROM meta WHERE key = 'format_version'",
        [],
        |r| r.get::<_, i64>(0),
    )
    .ok()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::{Duration, SystemTime};

    /// 唯一临时目录（同仓库其它单测口径：只写自己的 %TEMP% 子目录，不做永久删除）。
    fn unique_dir(tag: &str) -> PathBuf {
        use std::sync::atomic::{AtomicU64, Ordering};
        static SEQ: AtomicU64 = AtomicU64::new(0);
        let nanos = SystemTime::now()
            .duration_since(SystemTime::UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0);
        let dir = std::env::temp_dir()
            .join("diskintel-db-tests")
            .join(format!("{tag}-{nanos}-{}", SEQ.fetch_add(1, Ordering::Relaxed)));
        std::fs::create_dir_all(&dir).expect("建临时目录");
        dir
    }

    /// 造一个文件并把 mtime 设成指定时刻。
    fn touched(path: &Path, secs: u64) {
        std::fs::write(path, b"not-a-real-sqlite").expect("写文件");
        let file = std::fs::OpenOptions::new()
            .write(true)
            .open(path)
            .expect("打开文件");
        file.set_modified(SystemTime::UNIX_EPOCH + Duration::from_secs(secs))
            .expect("设置 mtime");
    }

    /// 创建时间优先取 mtime：内容不是 sqlite、meta 读不到时也能给出真实生成时刻。
    #[test]
    fn created_unix_prefers_mtime() {
        let dir = unique_dir("created");
        let path = dir.join("20260101-000000.sqlite");
        let stamp = 1_700_000_000u64;
        touched(&path, stamp);
        assert_eq!(snapshot_created_unix(&path), stamp as i64);
        // 读不到的路径返回 0，而不是 panic 或负数
        assert_eq!(snapshot_created_unix(&dir.join("missing.sqlite")), 0);
    }

    /// 列表按名字倒序，且用 mtime 当创建时间（伪快照也能排对）。
    #[test]
    fn list_snapshots_uses_mtime_and_name_order() {
        let dir = unique_dir("list");
        touched(&dir.join("20260101-000000.sqlite"), 1_700_000_100);
        touched(&dir.join("20260102-000000.sqlite"), 1_700_000_200);
        std::fs::write(dir.join("index.json"), b"{}").expect("写无关文件");
        let files = list_snapshots(&dir);
        assert_eq!(files.len(), 2, "只认 .sqlite：{:?}", files.iter().map(|f| &f.name).collect::<Vec<_>>());
        assert_eq!(files[0].name, "20260102-000000");
        assert_eq!(files[0].created_at, 1_700_000_200);
        assert_eq!(files[1].created_at, 1_700_000_100);
    }

    /// 版本探测：不是 sqlite 的文件返回 None（保留策略据此判断 v3 / v4）。
    #[test]
    fn format_version_of_garbage_is_none() {
        let dir = unique_dir("version");
        let path = dir.join("20260101-000000.sqlite");
        std::fs::write(&path, b"garbage").expect("写文件");
        assert_eq!(peek_format_version(&path), None);
        assert_eq!(peek_format_version(&dir.join("missing.sqlite")), None);
    }
}

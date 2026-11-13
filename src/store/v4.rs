//! v4 字典化快照：v3 → v4 转换器（写侧）与只读查询层（读侧）。
//!
//! 规范：磁盘情报系统/docs/研究-快照存储优化分析-20261113.md 第 11 节。
//! 关键约束（全部按规范实现）：
//! - 建库前先设 page_size=8192 并读回断言（SQLite 对已写过的库改页大小会静默无效）；
//! - names 的 id 按全库出现频次降序分配，dirs 的 id 按 path 字典序分配；
//! - entries 里没有目录行，目录只在 dirs + rollup；
//! - 卷根的 name 存整条前缀（C:\），重建时靠 join_path 规则补分隔符；
//! - 转换只读打开 v3，产物校验通过后才 rename 落位。
//!
//! 读侧没有 entries.path，所以检索策略是：先用 names.name / dir_paths.path 命中 id，
//! 再按 id 从 entries / dirs 取行；路径在返回时现拼。

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::time::Instant;

use rusqlite::types::Value;
use rusqlite::{Connection, OpenFlags, params, params_from_iter};

use crate::db::{FolderRow, Row, SortBy};
use crate::filter::{Filter, Kind};
use crate::store::{LIKE_SEP_SUFFIX, escape_like, join_path, name_of};

/// 规范规定的页大小；8 KiB 下 v4 与 64 KiB 只差 0.35%，但叶页浪费更少。
const PAGE_SIZE: i64 = 8192;

/// 建库期 cache_size：256 MiB（负值 = KiB）。
const WRITE_CACHE_KIB: i64 = -262144;

const DDL: &str = r#"
CREATE TABLE meta (
    key   TEXT PRIMARY KEY,
    value TEXT NOT NULL
) WITHOUT ROWID;

-- 全库名字字典：id 按全库出现频次降序分配（高频名字落在 1 字节序列类型）
CREATE TABLE names (
    id   INTEGER PRIMARY KEY,
    name TEXT NOT NULL
) WITHOUT ROWID;

-- 目录：id 按 path 字典序分配；parent_id = -1 表示卷根
CREATE TABLE dirs (
    id        INTEGER PRIMARY KEY,
    parent_id INTEGER NOT NULL,
    name_id   INTEGER NOT NULL
) WITHOUT ROWID;

-- 文件行：主键 (parent_id, name_id) 即「某目录下的某个名字」
CREATE TABLE entries (
    parent_id INTEGER NOT NULL,
    name_id   INTEGER NOT NULL,
    size      INTEGER NOT NULL,
    mtime     INTEGER NOT NULL,
    flags     INTEGER NOT NULL,
    PRIMARY KEY (parent_id, name_id)
) WITHOUT ROWID;

-- 目录子树聚合，取代 v3 的 folder_sizes
CREATE TABLE rollup (
    dir_id     INTEGER PRIMARY KEY,
    size       INTEGER NOT NULL,
    file_count INTEGER NOT NULL
) WITHOUT ROWID;

-- 目录完整路径：只为前缀检索 / 范围检索，占约 90 MB
CREATE TABLE dir_paths (
    dir_id INTEGER PRIMARY KEY,
    path   TEXT NOT NULL
) WITHOUT ROWID;
"#;

/// 全部数据落库后再建的索引（与 v3 的 POST_INDEXES 同策略）。
const POST_INDEXES: &str = r#"
CREATE INDEX idx_rollup_size ON rollup(size DESC);
-- 名字检索：count/page 的 IN (SELECT id FROM names ...) 靠它落回主键
CREATE INDEX idx_entries_name ON entries(name_id);
-- 子目录枚举：dirs 按 parent_id 找孩子，没有它就只能扫 63.5 万行
CREATE INDEX idx_dirs_parent ON dirs(parent_id);
"#;

/// 文件行取路径的 SQL 表达式（父路径以分隔符结尾就直接接）。
const FILE_PATH_SQL: &str = r"p.path || CASE WHEN substr(p.path,-1,1) IN ('\','/') THEN '' ELSE '\' END || n.name";

/// 一次转换的产出统计，main.rs 直接拿去打印。
#[derive(Clone, Debug, Default)]
pub struct V4Stats {
    pub v3_path: PathBuf,
    pub out_path: PathBuf,
    pub v3_bytes: u64,
    pub out_bytes: u64,
    pub seconds: f64,
    /// 压缩比 = v3 字节 / v4 字节。
    pub ratio: f64,
    pub rows: u64,
    pub dirs: u64,
    pub names: u64,
    pub total_size: u64,
    pub page_size: i64,
    pub expect_rows: u64,
    pub expect_dirs: u64,
    pub expect_total_size: u64,
    pub verified: bool,
    pub notes: Vec<String>,
}

struct DirRec {
    path: String,
    parent_path: String,
    id: i64,
    parent_id: i64,
    name_dense: u32,
    name_id: i64,
}

struct RowRec {
    parent: u32,
    name: u32,
    size: i64,
    mtime: i64,
    flags: u32,
}

/// 名字池：先把名字压成稠密下标，统计完成后才按频次分配正式 id。
struct Interner {
    map: HashMap<String, u32>,
    freq: Vec<u32>,
}

impl Interner {
    fn new() -> Self {
        Self {
            map: HashMap::new(),
            freq: Vec::new(),
        }
    }

    fn intern(&mut self, name: &str) -> u32 {
        if let Some(&dense) = self.map.get(name) {
            self.freq[dense as usize] += 1;
            return dense;
        }
        let dense = self.freq.len() as u32;
        self.map.insert(name.to_owned(), dense);
        self.freq.push(1);
        dense
    }
}

macro_rules! try_ctx {
    ($ctx:expr, $expr:expr) => {
        $expr.map_err(|e| format!("{}：{}", $ctx, e))?
    };
}

fn find_dir(dirs: &[DirRec], path: &str) -> Option<i64> {
    dirs.binary_search_by(|d| d.path.as_str().cmp(path))
        .ok()
        .map(|i| dirs[i].id)
}

fn read_meta(conn: &Connection) -> Result<HashMap<String, String>, String> {
    let mut out = HashMap::new();
    let mut stmt = try_ctx!("读取 v3 meta 失败", conn.prepare("SELECT key, value FROM meta"));
    let mut rows = try_ctx!("读取 v3 meta 失败", stmt.query([]));
    while let Some(row) = try_ctx!("读取 v3 meta 失败", rows.next()) {
        let key: String = try_ctx!("读取 v3 meta 失败", row.get(0));
        let value: String = try_ctx!("读取 v3 meta 失败", row.get(1));
        out.insert(key, value);
    }
    Ok(out)
}

fn int_meta(meta: &HashMap<String, String>, key: &str) -> i64 {
    meta.get(key)
        .and_then(|v| v.trim().parse::<i64>().ok())
        .unwrap_or(0)
}

fn temp_path(out: &Path) -> PathBuf {
    let stamp = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis())
        .unwrap_or(0);
    let name = out
        .file_name()
        .and_then(|s| s.to_str())
        .unwrap_or("snapshot.sqlite");
    let temp = format!(".{}.tmp-{}-{}", name, std::process::id(), stamp);
    let path = match out.parent() {
        Some(parent) if !parent.as_os_str().is_empty() => parent.join(temp),
        _ => PathBuf::from(temp),
    };
    // 错误信息里要能直接看懂「到底想往哪写」，相对路径一律摊成绝对路径。
    absolutize(path)
}

/// 转成绝对路径（失败时原样返回，不因为取不到工作目录就提前报错）。
fn absolutize(path: PathBuf) -> PathBuf {
    if path.is_absolute() {
        return path;
    }
    match std::env::current_dir() {
        Ok(dir) => dir.join(path),
        Err(_) => path,
    }
}

/// 临时库清理卫士：除非显式收兵，离开作用域时删掉半成品。
///
/// 只清理**本次转换自己刚建的**临时文件；生产失败时不留 .tmp，符合「转换失败不得留下半成品」。
struct TempGuard {
    path: PathBuf,
    armed: bool,
}

impl TempGuard {
    fn new(path: PathBuf) -> Self {
        Self { path, armed: true }
    }

    /// 转换成功、文件已改名落位后调用。
    fn disarm(&mut self) {
        self.armed = false;
    }
}

impl Drop for TempGuard {
    fn drop(&mut self) {
        if !self.armed {
            return;
        }
        // 主文件 + SQLite 可能留下的 journal / wal 边角，一并清掉，别留半成品。
        for suffix in ["", "-journal", "-wal", "-shm"] {
            let path = if suffix.is_empty() {
                self.path.clone()
            } else {
                PathBuf::from(format!("{}{suffix}", self.path.display()))
            };
            match std::fs::remove_file(&path) {
                Ok(()) => {}
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
                Err(error) => eprintln!("[v4] 清理临时库失败 {}：{error}", path.display()),
            }
        }
    }
}

/// 打开临时库：先建空文件，再让 SQLite 打开。
///
/// 为什么不直接 Connection::open：路径不存在 / 目录只读 / 被杀软占用时，
/// sqlite3_open_v2 统一只回一句 unable to open database file，
/// 看不出是「目录不存在」还是「没权限」。先 File::create 能拿到精确的 OS 错误。
fn open_temp_db(temp: &Path) -> Result<Connection, String> {
    if let Some(parent) = temp.parent() {
        if !parent.as_os_str().is_empty() && !parent.exists() {
            std::fs::create_dir_all(parent)
                .map_err(|error| format!("创建输出目录 {} 失败：{error}", parent.display()))?;
        }
    }
    match std::fs::File::options()
        .create(true)
        .write(true)
        .truncate(false)
        .open(temp)
    {
        Ok(_) => {}
        Err(error) => {
            let parent = temp
                .parent()
                .map(|dir| dir.display().to_string())
                .unwrap_or_else(|| "(未知)".to_owned());
            let state = if temp.parent().is_some_and(Path::exists) {
                "目录存在"
            } else {
                "目录不存在"
            };
            return Err(format!(
                "创建临时库失败 {}：{error}（输出目录 {parent}，{state}）",
                temp.display()
            ));
        }
    }
    Connection::open_with_flags(
        temp,
        OpenFlags::SQLITE_OPEN_READ_WRITE | OpenFlags::SQLITE_OPEN_NO_MUTEX,
    )
    .map_err(|error| format!("打开临时库失败 {}：{error}", temp.display()))
}

/// 表行数；查不到返回 0（校验时会被当成不一致报出来）。
fn count_of(conn: &Connection, table: &str) -> i64 {
    conn.query_row(&format!("SELECT COUNT(*) FROM {table}"), [], |r| {
        r.get::<_, i64>(0)
    })
    .unwrap_or(0)
}

/// entries.size 合计（= v3 meta.total_size 的口径）。
fn sum_size(conn: &Connection) -> i64 {
    conn.query_row("SELECT COALESCE(SUM(size), 0) FROM entries", [], |r| {
        r.get::<_, i64>(0)
    })
    .unwrap_or(0)
}

/// v3 → v4 单向迁移：只读打开 v3，流式构建字典与表，单事务写入，
/// 校验 rows / file_count / dir_count / total_size 与 v3 meta 完全一致后原子落位。
pub fn convert_v3_to_v4(v3_path: &Path, out_path: &Path) -> Result<V4Stats, String> {
    let started = Instant::now();
    // 绝对化输出路径：--convert-v4 常被脚本 / 计划任务调用，错误信息里必须能直接看懂落点。
    let out_path = &absolutize(out_path.to_path_buf());
    let mut stats = V4Stats {
        v3_path: v3_path.to_path_buf(),
        out_path: out_path.to_path_buf(),
        ..Default::default()
    };
    // 输出目录不存在就先建（用户/脚本只负责给路径）；建不出来在 open_temp_db 里报精确错误。
    if let Some(parent) = out_path.parent() {
        if !parent.as_os_str().is_empty() && !parent.exists() {
            std::fs::create_dir_all(parent)
                .map_err(|error| format!("创建输出目录 {} 失败：{error}", parent.display()))?;
        }
    }

    let v3 = Connection::open_with_flags(v3_path, OpenFlags::SQLITE_OPEN_READ_ONLY)
        .map_err(|e| format!("打开 v3 失败 {}：{e}", v3_path.display()))?;
    // 转换期间只需要顺序读，缓存与 mmap 都收着用，别把 1.5 GB 全拉进工作集。
    let _ = v3.execute_batch(
        "PRAGMA query_only=ON; PRAGMA cache_size=-131072; PRAGMA temp_store=MEMORY; PRAGMA mmap_size=134217728;",
    );

    let meta = read_meta(&v3)?;
    let expect_rows = int_meta(&meta, "file_count");
    let expect_dirs = int_meta(&meta, "dir_count");
    let expect_total = int_meta(&meta, "total_size");
    if expect_rows <= 0 || expect_dirs <= 0 {
        return Err(format!(
            "v3 meta 计数异常：file_count={expect_rows} dir_count={expect_dirs}"
        ));
    }
    stats.expect_rows = expect_rows.max(0) as u64;
    stats.expect_dirs = expect_dirs.max(0) as u64;
    stats.expect_total_size = expect_total.max(0) as u64;
    stats.v3_bytes = std::fs::metadata(v3_path).map(|m| m.len()).unwrap_or(0);

    // ---- 1. 目录字典：按 path 字典序分配 id（这是 dir_paths 能二分查找的前提）----
    let mut dirs: Vec<DirRec> = Vec::with_capacity(expect_dirs as usize);
    {
        let mut stmt = try_ctx!(
            "扫描 v3 目录行失败",
            v3.prepare("SELECT path, parent_path FROM entries WHERE is_dir = 1")
        );
        let mut rows = try_ctx!("扫描 v3 目录行失败", stmt.query([]));
        while let Some(row) = try_ctx!("扫描 v3 目录行失败", rows.next()) {
            dirs.push(DirRec {
                path: try_ctx!("读目录 path 失败", row.get(0)),
                parent_path: try_ctx!("读目录 parent_path 失败", row.get(1)),
                id: 0,
                parent_id: -1,
                name_dense: 0,
                name_id: 0,
            });
        }
    }
    if dirs.len() as i64 != expect_dirs {
        return Err(format!(
            "v3 目录行数 {} 与 meta.dir_count {} 不一致",
            dirs.len(),
            expect_dirs
        ));
    }
    dirs.sort_unstable_by(|a, b| a.path.cmp(&b.path));
    for index in 1..dirs.len() {
        if dirs[index].path == dirs[index - 1].path {
            return Err(format!("v3 目录 path 重复：{}", dirs[index].path));
        }
    }
    for (index, dir) in dirs.iter_mut().enumerate() {
        dir.id = index as i64 + 1;
    }
    let parent_ids: Vec<i64> = {
        let mut out = Vec::with_capacity(dirs.len());
        for dir in &dirs {
            if dir.parent_path.is_empty() {
                out.push(-1);
            } else {
                match find_dir(&dirs, &dir.parent_path) {
                    Some(id) => out.push(id),
                    None => {
                        return Err(format!(
                            "目录的父路径不在目录集里：{}（父 {}）",
                            dir.path, dir.parent_path
                        ));
                    }
                }
            }
        }
        out
    };
    for (index, dir) in dirs.iter_mut().enumerate() {
        dir.parent_id = parent_ids[index];
    }

    // ---- 2. 名字字典：先统计频次（目录名 + 文件名一起算）----
    let mut interner = Interner::new();
    for dir in dirs.iter_mut() {
        let dense = {
            let name = name_of(&dir.path, &dir.parent_path);
            interner.intern(name)
        };
        dir.name_dense = dense;
    }

    // ---- 3. 文件行：一次扫描，父目录 id 用「上一行」缓存加速 ----
    let mut rows: Vec<RowRec> = Vec::with_capacity(expect_rows as usize);
    let mut replaced = 0u64;
    {
        let mut stmt = try_ctx!(
            "扫描 v3 文件行失败",
            v3.prepare(
                "SELECT path, parent_path, size, mtime, attributes FROM entries WHERE is_dir = 0 ORDER BY path"
            )
        );
        let mut cursor = try_ctx!("扫描 v3 文件行失败", stmt.query([]));
        let mut last_parent = String::new();
        let mut last_id: i64 = -1;
        while let Some(row) = try_ctx!("扫描 v3 文件行失败", cursor.next()) {
            let path: String = try_ctx!("读文件 path 失败", row.get(0));
            let parent: String = try_ctx!("读文件 parent_path 失败", row.get(1));
            let size: i64 = try_ctx!("读文件 size 失败", row.get(2));
            let mtime: i64 = try_ctx!("读文件 mtime 失败", row.get(3));
            let attrs: i64 = try_ctx!("读文件 attributes 失败", row.get(4));
            if path.contains('\u{FFFD}') {
                replaced += 1;
            }
            let parent_id = if last_id >= 0 && parent == last_parent {
                last_id
            } else {
                let id = match find_dir(&dirs, &parent) {
                    Some(id) => id,
                    None => {
                        return Err(format!("文件的父目录不在目录集里：{parent}（{path}）"));
                    }
                };
                last_id = id;
                last_parent.clear();
                last_parent.push_str(&parent);
                id
            };
            let dense = {
                let name = name_of(&path, &parent);
                interner.intern(name)
            };
            rows.push(RowRec {
                parent: parent_id as u32,
                name: dense,
                size,
                mtime,
                flags: flags_of(attrs),
            });
        }
    }
    if rows.len() as i64 != expect_rows {
        return Err(format!(
            "v3 文件行数 {} 与 meta.file_count {} 不一致",
            rows.len(),
            expect_rows
        ));
    }

    // ---- 4. 频次降序分配名字 id ----
    let name_count = interner.freq.len();
    let mut names_dense: Vec<Option<String>> = vec![None; name_count];
    for (name, dense) in interner.map.drain() {
        names_dense[dense as usize] = Some(name);
    }
    let mut order: Vec<u32> = (0..name_count as u32).collect();
    order.sort_unstable_by(|a, b| {
        interner.freq[*b as usize]
            .cmp(&interner.freq[*a as usize])
            .then_with(|| {
                let left = names_dense[*a as usize].as_deref().unwrap_or("");
                let right = names_dense[*b as usize].as_deref().unwrap_or("");
                left.cmp(right)
            })
    });
    let mut id_of_dense: Vec<u32> = vec![0; name_count];
    for (position, dense) in order.iter().enumerate() {
        id_of_dense[*dense as usize] = position as u32 + 1;
    }
    for row in rows.iter_mut() {
        row.name = id_of_dense[row.name as usize];
    }
    for dir in dirs.iter_mut() {
        dir.name_id = id_of_dense[dir.name_dense as usize] as i64;
    }
    if replaced > 0 {
        stats
            .notes
            .push(format!("{replaced} 行路径含替换字符（v3 已 decode，不影响行数）"));
    }

    // ---- 5. 建 v4：页大小断言 → 单事务写入 → 事后建索引 ----
    let temp = temp_path(out_path);
    // 失败时由卫士删除半成品（转换失败不得留下 .tmp）
    let mut guard = TempGuard::new(temp.clone());
    let conn = open_temp_db(&temp)?;
    conn.pragma_update(None, "page_size", PAGE_SIZE)
        .map_err(|e| format!("设置 page_size 失败：{e}"))?;
    conn.execute_batch(
        "PRAGMA journal_mode=OFF; PRAGMA synchronous=OFF; PRAGMA temp_store=MEMORY; \
         PRAGMA locking_mode=EXCLUSIVE;",
    )
    .map_err(|e| format!("设置建库 PRAGMA 失败：{e}"))?;
    let actual_page_size: i64 = conn
        .pragma_query_value(None, "page_size", |r| r.get(0))
        .map_err(|e| format!("读回 page_size 失败：{e}"))?;
    if actual_page_size != PAGE_SIZE {
        return Err(format!(
            "page_size 断言失败：期望 {PAGE_SIZE}，实际 {actual_page_size}（SQLite 会静默忽略已写库的页大小）"
        ));
    }
    stats.page_size = actual_page_size;
    conn.pragma_update(None, "cache_size", WRITE_CACHE_KIB)
        .map_err(|e| format!("设置 cache_size 失败：{e}"))?;
    conn.execute_batch(DDL)
        .map_err(|e| format!("建 v4 表失败：{e}"))?;
    conn.execute_batch("BEGIN")
        .map_err(|e| format!("开启写事务失败：{e}"))?;

    {
        // 名字按 id 升序写（B 树顺序追加）
        let mut stmt = conn
            .prepare("INSERT INTO names (id, name) VALUES (?1, ?2)")
            .map_err(|e| format!("准备 names 写入失败：{e}"))?;
        for (position, dense) in order.iter().enumerate() {
            let name = names_dense[*dense as usize]
                .as_deref()
                .ok_or_else(|| "名字字典缺项".to_owned())?;
            stmt.execute(params![position as i64 + 1, name])
                .map_err(|e| format!("写 names 失败：{e}"))?;
        }
    }
    {
        let mut stmt = conn
            .prepare("INSERT INTO dirs (id, parent_id, name_id) VALUES (?1, ?2, ?3)")
            .map_err(|e| format!("准备 dirs 写入失败：{e}"))?;
        for dir in dirs.iter() {
            stmt.execute(params![dir.id, dir.parent_id, dir.name_id])
                .map_err(|e| format!("写 dirs 失败：{e}"))?;
        }
    }
    {
        let mut stmt = conn
            .prepare("INSERT INTO dir_paths (dir_id, path) VALUES (?1, ?2)")
            .map_err(|e| format!("准备 dir_paths 写入失败：{e}"))?;
        for dir in dirs.iter() {
            stmt.execute(params![dir.id, dir.path])
                .map_err(|e| format!("写 dir_paths 失败：{e}"))?;
        }
    }

    // entries 按 (parent_id, name_id) 升序写，保证 B 树顺序追加
    rows.sort_unstable_by(|a, b| a.parent.cmp(&b.parent).then(a.name.cmp(&b.name)));
    {
        let mut stmt = conn
            .prepare("INSERT INTO entries (parent_id, name_id, size, mtime, flags) VALUES (?1, ?2, ?3, ?4, ?5)")
            .map_err(|e| format!("准备 entries 写入失败：{e}"))?;
        for row in rows.iter() {
            stmt.execute(params![
                row.parent as i64,
                row.name as i64,
                row.size,
                row.mtime,
                row.flags as i64
            ])
            .map_err(|e| format!("写 entries 失败：{e}"))?;
        }
    }

    // rollup 直接抄 v3 的 folder_sizes，保证 dir_info / child_dirs 与 v3 逐字节同值
    {
        let mut source = try_ctx!(
            "读取 v3 folder_sizes 失败",
            v3.prepare("SELECT path, size, file_count FROM folder_sizes")
        );
        let mut cursor = try_ctx!("读取 v3 folder_sizes 失败", source.query([]));
        let mut stmt = conn
            .prepare("INSERT INTO rollup (dir_id, size, file_count) VALUES (?1, ?2, ?3)")
            .map_err(|e| format!("准备 rollup 写入失败：{e}"))?;
        while let Some(row) = try_ctx!("读取 v3 folder_sizes 失败", cursor.next()) {
            let path: String = try_ctx!("读 folder_sizes.path 失败", row.get(0));
            let size: i64 = try_ctx!("读 folder_sizes.size 失败", row.get(1));
            let file_count: i64 = try_ctx!("读 folder_sizes.file_count 失败", row.get(2));
            let id = match find_dir(&dirs, &path) {
                Some(id) => id,
                None => return Err(format!("folder_sizes 里的目录不在 entries 里：{path}")),
            };
            stmt.execute(params![id, size, file_count])
                .map_err(|e| format!("写 rollup 失败：{e}"))?;
        }
    }

    // ---- 6. meta：继承 v3 全部键，再补 v4 必写键 ----
    let mut meta_out = meta.clone();
    meta_out.insert("format_version".to_owned(), "4".to_owned());
    meta_out.insert("layout".to_owned(), "dict-v4".to_owned());
    meta_out.insert("row_count".to_owned(), expect_rows.to_string());
    meta_out.insert("file_count".to_owned(), expect_rows.to_string());
    meta_out.insert("dir_count".to_owned(), expect_dirs.to_string());
    meta_out.insert("name_count".to_owned(), name_count.to_string());
    meta_out.insert("total_size".to_owned(), expect_total.to_string());
    meta_out.insert("chain_depth".to_owned(), "0".to_owned());
    meta_out.insert("converter".to_owned(), "diskintel-rs store::v4".to_owned());
    meta_out.insert("converted_at".to_owned(), crate::filter::now_unix().to_string());
    meta_out.insert(
        "flags_mask".to_owned(),
        "keep_attrs_except(0x10|0x20|0x80)".to_owned(),
    );
    if !meta_out.contains_key("created_at") {
        meta_out.insert("created_at".to_owned(), crate::filter::now_unix().to_string());
    }
    if !meta_out.contains_key("engine") {
        meta_out.insert("engine".to_owned(), "diskintel-rs".to_owned());
    }
    if !meta_out.contains_key("generator_version") {
        meta_out.insert(
            "generator_version".to_owned(),
            env!("CARGO_PKG_VERSION").to_owned(),
        );
    }
    {
        let mut stmt = conn
            .prepare("INSERT INTO meta (key, value) VALUES (?1, ?2)")
            .map_err(|e| format!("准备 meta 写入失败：{e}"))?;
        let mut keys: Vec<&String> = meta_out.keys().collect();
        keys.sort();
        for key in keys {
            stmt.execute(params![key, &meta_out[key]])
                .map_err(|e| format!("写 meta 失败：{e}"))?;
        }
    }

    conn.execute_batch("COMMIT")
        .map_err(|e| format!("提交写事务失败：{e}"))?;
    conn.execute_batch(POST_INDEXES)
        .map_err(|e| format!("建索引失败：{e}"))?;
    // 规范说 freelist_count=0 就不需要 VACUUM；实测顺序建库后仍有约 10% 的
    // 半空叶页，VACUUM 能把 321.1 MB 收到 289.9 MB（多花约 5 s），所以这里做一次。
    conn.execute_batch("VACUUM")
        .map_err(|e| format!("VACUUM 失败：{e}"))?;
    let page_after_vacuum: i64 = conn
        .pragma_query_value(None, "page_size", |r| r.get(0))
        .map_err(|e| format!("VACUUM 后读回 page_size 失败：{e}"))?;
    if page_after_vacuum != PAGE_SIZE {
        return Err(format!(
            "VACUUM 后 page_size 变成 {page_after_vacuum}（期望 {PAGE_SIZE}）"
        ));
    }

    // ---- 7. 校验：rows / file_count / dir_count / total_size 必须与 v3 完全一致 ----
    let entry_rows = count_of(&conn, "entries");
    let entry_bytes = sum_size(&conn);
    let dir_rows = count_of(&conn, "dirs");
    let rollup_rows = count_of(&conn, "rollup");
    let name_rows = count_of(&conn, "names");
    let dir_path_rows = count_of(&conn, "dir_paths");
    let root_total: i64 = conn
        .query_row(
            "SELECT COALESCE(SUM(r.size), 0) FROM rollup r JOIN dirs d ON d.id = r.dir_id WHERE d.parent_id = -1",
            [],
            |r| r.get(0),
        )
        .unwrap_or(0);
    let root_files: i64 = conn
        .query_row(
            "SELECT COALESCE(SUM(r.file_count), 0) FROM rollup r JOIN dirs d ON d.id = r.dir_id WHERE d.parent_id = -1",
            [],
            |r| r.get(0),
        )
        .unwrap_or(0);

    let mut problems: Vec<String> = Vec::new();
    if entry_rows != expect_rows {
        problems.push(format!("entries 行数 {entry_rows} != meta.file_count {expect_rows}"));
    }
    if dir_rows != expect_dirs {
        problems.push(format!("dirs 行数 {dir_rows} != meta.dir_count {expect_dirs}"));
    }
    if entry_bytes != expect_total {
        problems.push(format!(
            "entries.size 合计 {entry_bytes} != meta.total_size {expect_total}"
        ));
    }
    if name_rows != name_count as i64 {
        problems.push(format!("names 行数 {name_rows} != {name_count}"));
    }
    if rollup_rows != expect_dirs {
        problems.push(format!("rollup 行数 {rollup_rows} != meta.dir_count {expect_dirs}"));
    }
    if dir_path_rows != expect_dirs {
        problems.push(format!("dir_paths 行数 {dir_path_rows} != meta.dir_count {expect_dirs}"));
    }
    if rollup_rows != expect_dirs || entry_rows != expect_rows {
        problems.push("表行数与 v3 不符".to_owned());
    }
    // 归并口径也一起校验：rollup 根合计必须回到 v3 的 total_size / file_count
    if root_total != expect_total {
        problems.push(format!(
            "rollup 根合计 {root_total} != meta.total_size {expect_total}"
        ));
    }
    if root_files != expect_rows {
        problems.push(format!(
            "rollup 根 file_count 合计 {root_files} != meta.file_count {expect_rows}"
        ));
    }
    if !problems.is_empty() {
        // 校验失败：不落位，半成品由 TempGuard 清理（失败不留在磁盘上）
        let _ = conn.execute_batch("PRAGMA journal_mode=DELETE;");
        drop(conn);
        return Err(format!(
            "v4 校验失败（未落位，v3 原文件未改动）：{}",
            problems.join("；")
        ));
    }

    let integrity: String = conn
        .query_row("PRAGMA integrity_check", [], |r| r.get(0))
        .unwrap_or_else(|e| format!("integrity_check 失败：{e}"));
    let freelist: i64 = conn
        .query_row("PRAGMA freelist_count", [], |r| r.get(0))
        .unwrap_or(0);
    drop(conn);

    // ---- 8. 原子落位 ----
    if out_path.exists() {
        stats
            .notes
            .push("输出文件已存在，按转换请求覆盖（rename 原子替换）".to_owned());
    }
    std::fs::rename(&temp, out_path).map_err(|e| {
        format!(
            "落位失败（{} → {}）：{e}（v3 原文件未改动，半成品随临时库清理）",
            temp.display(),
            out_path.display()
        )
    })?;
    guard.disarm();

    stats.out_bytes = std::fs::metadata(out_path).map(|m| m.len()).unwrap_or(0);
    stats.rows = entry_rows.max(0) as u64;
    stats.dirs = dir_rows.max(0) as u64;
    stats.names = name_rows.max(0) as u64;
    stats.total_size = entry_bytes.max(0) as u64;
    stats.seconds = started.elapsed().as_secs_f64();
    stats.ratio = if stats.out_bytes > 0 {
        stats.v3_bytes as f64 / stats.out_bytes as f64
    } else {
        0.0
    };
    stats.verified = true;
    stats.notes.push(format!(
        "integrity_check={integrity} freelist_count={freelist} 单事务 + 预处理语句（50k 行一批等价实现）"
    ));
    stats.notes.push(format!(
        "峰值 RSS {:.0} MB",
        crate::store::peak_rss_bytes() as f64 / 1_048_576.0
    ));
    Ok(stats)
}

/// 只保留非常规属性位：常规目录 / 文件 / 归档位不再存（v3 实测 files 全 0、dirs 全 16）。
const FLAGS_MASK: i64 = !(0x10 | 0x20 | 0x80);

fn flags_of(attributes: i64) -> u32 {
    (attributes & FLAGS_MASK) as u32
}

/// 与 crate::filter::Filter 同字段的检索条件，读侧自己构建 SQL。
#[derive(Default, Clone, Debug)]
pub struct Query {
    pub terms: Vec<String>,
    pub exts: Vec<String>,
    pub min_size: Option<i64>,
    pub max_size: Option<i64>,
    /// Unix 秒（v4 的 mtime 是 Unix 秒；v3 的 filter 侧换算成 FILETIME 是历史缺陷）
    pub after: Option<i64>,
    pub before: Option<i64>,
    pub kind: Option<Kind>,
    /// 路径前缀，末尾分隔符已去掉（与 v3 的 normalize_scope 同口径）
    pub scope: Option<String>,
}

impl Query {
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        terms: &[String],
        exts: &[String],
        min_size: Option<i64>,
        max_size: Option<i64>,
        after: Option<i64>,
        before: Option<i64>,
        kind: Option<Kind>,
        scope: Option<String>,
    ) -> Self {
        Self {
            terms: terms.to_vec(),
            exts: exts.to_vec(),
            min_size,
            max_size,
            after,
            before,
            kind,
            scope,
        }
    }

    pub fn from_filter(filter: &Filter) -> Self {
        Self {
            terms: filter.terms.clone(),
            exts: filter.exts.clone(),
            min_size: filter.min_size,
            max_size: filter.max_size,
            after: filter.after,
            before: filter.before,
            kind: filter.kind,
            scope: filter.scope.clone(),
        }
    }

    pub fn is_none(&self) -> bool {
        self.terms.is_empty()
            && self.exts.is_empty()
            && self.min_size.is_none()
            && self.max_size.is_none()
            && self.after.is_none()
            && self.before.is_none()
            && self.kind.is_none()
            && self.scope.is_none()
    }

    fn wants_files(&self) -> bool {
        matches!(self.kind, None | Some(Kind::All) | Some(Kind::File))
    }

    /// 目录行能不能留在结果里：v4 的 dirs 表没有 size / mtime / ext，
    /// 所以大小、时间、扩展名条件一旦无法用 0 或空串满足，目录就整体不参与。
    fn wants_dirs(&self) -> bool {
        if !self.exts.is_empty() {
            return false;
        }
        if self.min_size.is_some_and(|min| min > 0) {
            return false;
        }
        if self.max_size.is_some_and(|max| max < 0) {
            return false;
        }
        if self.after.is_some() || self.before.is_some() {
            return false;
        }
        matches!(self.kind, None | Some(Kind::All) | Some(Kind::Dir))
    }
}

fn term_pattern(term: &str) -> String {
    format!("%{}%", escape_like(term))
}

/// 文件行的条件。只用到 e.name_id / e.parent_id，**不 join**，
/// 这样 count 走纯 entries 扫描 + 两个字典子查询，不碰 1.5 GB 的路径文本。
fn file_predicate(query: &Query) -> (String, Vec<Value>) {
    let mut parts: Vec<String> = Vec::new();
    let mut params: Vec<Value> = Vec::new();

    for term in &query.terms {
        let pattern = term_pattern(term);
        let mut alternatives: Vec<String> = vec![
            "e.name_id IN (SELECT id FROM names WHERE name LIKE ? ESCAPE '\\')".to_owned(),
            "e.parent_id IN (SELECT dir_id FROM dir_paths WHERE path LIKE ? ESCAPE '\\')".to_owned(),
        ];
        params.push(Value::Text(pattern.clone()));
        params.push(Value::Text(pattern));
        // 跨越「父路径 / 名字」边界的命中：拼路径时中间会插一个分隔符，
        // 所以只有当条件里带分隔符时才可能出现跨界命中，按分隔符位置枚举拆分。
        for (index, ch) in term.char_indices() {
            if ch != '\\' && ch != '/' {
                continue;
            }
            let head = &term[..index];
            let tail = &term[index + 1..];
            if tail.is_empty() {
                continue;
            }
            if head.is_empty() {
                alternatives.push(
                    "e.name_id IN (SELECT id FROM names WHERE name LIKE ? ESCAPE '\\')".to_owned(),
                );
                params.push(Value::Text(format!("{}%", escape_like(tail))));
            } else {
                alternatives.push(
                    "(e.parent_id IN (SELECT dir_id FROM dir_paths WHERE path LIKE ? ESCAPE '\\') \
                     AND e.name_id IN (SELECT id FROM names WHERE name LIKE ? ESCAPE '\\'))"
                        .to_owned(),
                );
                params.push(Value::Text(format!("%{}", escape_like(head))));
                params.push(Value::Text(format!("{}%", escape_like(tail))));
            }
        }
        parts.push(format!("({})", alternatives.join(" OR ")));
    }

    if !query.exts.is_empty() {
        let mut alternatives: Vec<&str> = Vec::new();
        for ext in &query.exts {
            alternatives.push("name LIKE ? ESCAPE '\\'");
            params.push(Value::Text(format!("%.{}", escape_like(ext))));
        }
        parts.push(format!(
            "e.name_id IN (SELECT id FROM names WHERE {})",
            alternatives.join(" OR ")
        ));
    }

    if let Some(min) = query.min_size {
        parts.push("e.size >= ?".to_owned());
        params.push(Value::Integer(min));
    }
    if let Some(max) = query.max_size {
        parts.push("e.size <= ?".to_owned());
        params.push(Value::Integer(max));
    }
    if query.after.is_some() || query.before.is_some() {
        let mut time_parts: Vec<&str> = vec!["e.mtime > 0"];
        if let Some(after) = query.after {
            time_parts.push("e.mtime >= ?");
            params.push(Value::Integer(after));
        }
        if let Some(before) = query.before {
            time_parts.push("e.mtime <= ?");
            params.push(Value::Integer(before));
        }
        parts.push(format!("({})", time_parts.join(" AND ")));
    }

    if let Some(scope) = &query.scope {
        // 文件在 scope 内 ⇔ 父目录等于 scope 或在其下（v3 的 path = scope OR path LIKE scope\%）
        let mut alternatives: Vec<String> = vec![
            "e.parent_id IN (SELECT dir_id FROM dir_paths WHERE path = ? OR path LIKE ? ESCAPE '\\')"
                .to_owned(),
        ];
        params.push(Value::Text(scope.clone()));
        params.push(Value::Text(format!("{}{}", escape_like(scope), LIKE_SEP_SUFFIX)));
        // 边界情况：scope 本身就是一个文件路径（v3 的 path = ?）
        if let Some(parent) = crate::db::parent_dir(scope) {
            let base = scope.rsplit(['\\', '/']).next().unwrap_or("");
            if !base.is_empty() {
                alternatives.push(
                    "(e.parent_id IN (SELECT dir_id FROM dir_paths WHERE path = ?) \
                     AND e.name_id IN (SELECT id FROM names WHERE name = ?))"
                        .to_owned(),
                );
                params.push(Value::Text(parent));
                params.push(Value::Text(base.to_owned()));
            }
        }
        parts.push(format!("({})", alternatives.join(" OR ")));
    }

    if parts.is_empty() {
        ("1=1".to_owned(), params)
    } else {
        (parts.join(" AND "), params)
    }
}

/// 目录行的条件：目录的完整路径就存在 dir_paths.path 里，条件直接落在它上面。
fn dir_predicate(query: &Query) -> (String, Vec<Value>) {
    let mut parts: Vec<String> = Vec::new();
    let mut params: Vec<Value> = Vec::new();
    for term in &query.terms {
        parts.push("d.id IN (SELECT dir_id FROM dir_paths WHERE path LIKE ? ESCAPE '\\')".to_owned());
        params.push(Value::Text(term_pattern(term)));
    }
    if let Some(scope) = &query.scope {
        parts.push(
            "(d.id IN (SELECT dir_id FROM dir_paths WHERE path = ?) \
             OR d.id IN (SELECT dir_id FROM dir_paths WHERE path LIKE ? ESCAPE '\\'))"
                .to_owned(),
        );
        params.push(Value::Text(scope.clone()));
        params.push(Value::Text(format!("{}{}", escape_like(scope), LIKE_SEP_SUFFIX)));
    }
    if parts.is_empty() {
        ("1=1".to_owned(), params)
    } else {
        (parts.join(" AND "), params)
    }
}

/// v4 只读快照。字段名与 crate::db::Snapshot 对齐，便于整层替换。
pub struct V4Snapshot {
    conn: Option<Connection>,
    /// 路径 → 目录 id 的小缓存：界面一次导航会连着问 child_dirs / child_files /
    /// dir_info 三个问题，第二次起就不必再走二分（约 20 次主键查）。
    dir_cache: std::cell::RefCell<HashMap<String, i64>>,
    pub path: PathBuf,
    pub name: String,
    pub meta: HashMap<String, String>,
    pub total_entries: u64,
    pub corpus_bytes: u64,
    pub file_bytes: u64,
    pub open_ms: f64,
    pub format_version: i64,
    pub file_count: u64,
    pub dir_count: u64,
    pub name_count: u64,
    pub layout: String,
    pub created_at: i64,
}

impl V4Snapshot {
    /// 空快照：没有连接，所有查询返回空，用于空态界面。
    pub fn open_empty() -> Result<Self, String> {
        Ok(Self {
            conn: None,
            dir_cache: std::cell::RefCell::new(HashMap::new()),
            path: PathBuf::new(),
            name: "-".to_owned(),
            meta: HashMap::new(),
            total_entries: 0,
            corpus_bytes: 0,
            file_bytes: 0,
            open_ms: 0.0,
            format_version: 0,
            file_count: 0,
            dir_count: 0,
            name_count: 0,
            layout: String::new(),
            created_at: 0,
        })
    }

    /// 打开第 1 个 format_version=4 的快照（按文件名倒序 = 时间倒序）。
    pub fn open_latest(root: &Path) -> Result<Self, String> {
        let mut candidates: Vec<PathBuf> = Vec::new();
        if let Ok(entries) = std::fs::read_dir(root) {
            for entry in entries.flatten() {
                let path = entry.path();
                if path.extension().and_then(|s| s.to_str()) == Some("sqlite") {
                    candidates.push(path);
                }
            }
        }
        candidates.sort_by(|a, b| b.file_name().cmp(&a.file_name()));
        for path in candidates {
            if let Ok(snapshot) = Self::open(&path) {
                return Ok(snapshot);
            }
        }
        Err(format!(
            "{} 下没有可读的 v4 快照（format_version=4）",
            root.display()
        ))
    }

    pub fn open(path: &Path) -> Result<Self, String> {
        let started = Instant::now();
        let conn = Connection::open_with_flags(
            path,
            OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_NO_MUTEX,
        )
        .map_err(|e| format!("打开 {} 失败：{e}", path.display()))?;
        // 只读 + 只读 PRAGMA；mmap 上限 256 MiB（v4 单份约 300 MB，接近全映射但不放大 RSS）
        let _ = conn.execute_batch(
            "PRAGMA query_only=ON; PRAGMA mmap_size=268435456; PRAGMA cache_size=-65536; \
             PRAGMA temp_store=MEMORY;",
        );

        let mut meta: HashMap<String, String> = HashMap::new();
        {
            let mut stmt = conn
                .prepare("SELECT key, value FROM meta")
                .map_err(|e| format!("不是 v4 快照（缺 meta 表）：{e}"))?;
            let mut rows = stmt
                .query([])
                .map_err(|e| format!("读取 meta 失败：{e}"))?;
            while let Some(row) = rows.next().map_err(|e| format!("读取 meta 失败：{e}"))? {
                let key: String = row.get(0).map_err(|e| format!("读取 meta 失败：{e}"))?;
                let value: String = row.get(1).map_err(|e| format!("读取 meta 失败：{e}"))?;
                meta.insert(key, value);
            }
        }
        let format_version = meta
            .get("format_version")
            .and_then(|v| v.trim().parse::<i64>().ok())
            .unwrap_or(0);
        if format_version != 4 {
            return Err(format!(
                "不是 v4 快照：{} 的 meta.format_version={format_version}（v4 读取层只接受 4）",
                path.display()
            ));
        }
        let meta_int = |key: &str| -> i64 {
            meta.get(key)
                .and_then(|v| v.trim().parse::<i64>().ok())
                .unwrap_or(0)
        };
        let file_count = meta_int("file_count").max(0) as u64;
        let dir_count = meta_int("dir_count").max(0) as u64;
        let mut total_entries = file_count + dir_count;
        let mut corpus_bytes = meta_int("total_size").max(0) as u64;
        if total_entries == 0 {
            total_entries = conn
                .query_row("SELECT COUNT(*) FROM entries", [], |r| r.get::<_, i64>(0))
                .unwrap_or(0)
                .max(0) as u64
                + conn
                    .query_row("SELECT COUNT(*) FROM dirs", [], |r| r.get::<_, i64>(0))
                    .unwrap_or(0)
                    .max(0) as u64;
        }
        if corpus_bytes == 0 {
            corpus_bytes = conn
                .query_row("SELECT COALESCE(SUM(size), 0) FROM entries", [], |r| {
                    r.get::<_, i64>(0)
                })
                .unwrap_or(0)
                .max(0) as u64;
        }
        let file_bytes = std::fs::metadata(path).map(|m| m.len()).unwrap_or(0);
        let name = path
            .file_stem()
            .and_then(|s| s.to_str())
            .unwrap_or("-")
            .to_owned();
        Ok(Self {
            conn: Some(conn),
            dir_cache: std::cell::RefCell::new(HashMap::new()),
            path: path.to_path_buf(),
            name,
            total_entries,
            corpus_bytes,
            file_bytes,
            open_ms: started.elapsed().as_secs_f64() * 1000.0,
            format_version,
            file_count,
            dir_count,
            name_count: meta_int("name_count").max(0) as u64,
            layout: meta.get("layout").cloned().unwrap_or_default(),
            created_at: meta_int("created_at"),
            meta,
        })
    }

    pub fn is_empty(&self) -> bool {
        self.conn.is_none()
    }

    pub fn meta_value(&self, key: &str) -> Option<&str> {
        self.meta.get(key).map(|s| s.as_str())
    }

    pub fn conn(&self) -> Option<&Connection> {
        self.conn.as_ref()
    }

    /// 目录 id → 完整路径（dir_paths 的主键查）。
    fn dir_path_of(&self, dir_id: i64) -> Option<String> {
        let conn = self.conn.as_ref()?;
        conn.query_row(
            "SELECT path FROM dir_paths WHERE dir_id = ?1",
            [dir_id],
            |r| r.get::<_, String>(0),
        )
        .ok()
    }

    /// 完整路径 → 目录 id。
    ///
    /// dirs 的 id 按 path 字典序分配，所以 dir_paths 天然按 path 有序，
    /// 直接在 dir_id 上二分（约 20 次主键查，0.2 ms），不需要给 90 MB 的
    /// path 列再建一份索引。取不到再退回一次线性查，兼容将来 delta 追尾的 id。
    pub fn dir_id_of(&self, path: &str) -> Option<i64> {
        if let Some(&hit) = self.dir_cache.borrow().get(path) {
            return Some(hit);
        }
        let id = self.dir_id_of_uncached(path)?;
        let mut cache = self.dir_cache.borrow_mut();
        if cache.len() >= 1024 {
            cache.clear();
        }
        cache.insert(path.to_owned(), id);
        Some(id)
    }

    fn dir_id_of_uncached(&self, path: &str) -> Option<i64> {
        let conn = self.conn.as_ref()?;
        let total = self.dir_count.max(1) as i64;
        let Ok(mut probe) = conn.prepare_cached("SELECT path FROM dir_paths WHERE dir_id = ?1")
        else {
            return None;
        };
        let mut low = 1i64;
        let mut high = total;
        while low <= high {
            let middle = low + (high - low) / 2;
            let Ok(middle_path) = probe.query_row([middle], |r| r.get::<_, String>(0)) else {
                break;
            };
            match middle_path.as_str().cmp(path) {
                std::cmp::Ordering::Equal => return Some(middle),
                std::cmp::Ordering::Less => low = middle + 1,
                std::cmp::Ordering::Greater => high = middle - 1,
            }
        }
        conn.query_row(
            "SELECT dir_id FROM dir_paths WHERE path = ?1 LIMIT 1",
            [path],
            |r| r.get::<_, i64>(0),
        )
        .ok()
    }

    /// 根层目录（各卷根），按大小降序。
    pub fn roots(&self) -> Vec<(String, i64, i64)> {
        self.child_dirs_of(-1, u64::MAX)
    }

    /// 与 v3 同形的根层行。
    pub fn roots_rows(&self) -> Vec<FolderRow> {
        self.roots()
            .into_iter()
            .map(|(path, size, file_count)| FolderRow {
                path,
                size,
                file_count,
            })
            .collect()
    }

    /// 某个目录的直接子目录，按大小降序（走 idx_dirs_parent + rollup）。
    pub fn child_dirs(&self, parent: &str, limit: u64) -> Vec<(String, i64, i64)> {
        match self.dir_id_of(parent) {
            Some(dir_id) => self.child_dirs_of(dir_id, limit),
            None => Vec::new(),
        }
    }

    pub fn child_dirs_rows(&self, parent: &str, limit: u64) -> Vec<FolderRow> {
        self.child_dirs(parent, limit)
            .into_iter()
            .map(|(path, size, file_count)| FolderRow {
                path,
                size,
                file_count,
            })
            .collect()
    }

    fn child_dirs_of(&self, parent_id: i64, limit: u64) -> Vec<(String, i64, i64)> {
        let Some(conn) = self.conn.as_ref() else {
            return Vec::new();
        };
        let sql = "SELECT dp.path, r.size, r.file_count FROM dirs c \
                   JOIN rollup r ON r.dir_id = c.id \
                   JOIN dir_paths dp ON dp.dir_id = c.id \
                   WHERE c.parent_id = ?1 ORDER BY r.size DESC LIMIT ?2";
        let Ok(mut stmt) = conn.prepare_cached(sql) else {
            return Vec::new();
        };
        // u64::MAX 直接转 i64 会变成 -1（SQLite 里恰好等价「不限」，但别靠巧合）
        let limit_sql = if limit > i64::MAX as u64 {
            -1
        } else {
            limit as i64
        };
        stmt.query_map(params![parent_id, limit_sql], |r| {
            Ok((r.get(0)?, r.get(1)?, r.get(2)?))
        })
        .map(|iter| iter.flatten().collect())
        .unwrap_or_default()
    }

    /// 某个目录的直接文件，按大小降序；路径在返回时用 join_path 现拼。
    pub fn child_files(&self, parent: &str, limit: u64) -> Vec<(String, i64, i64, bool)> {
        let Some(dir_id) = self.dir_id_of(parent) else {
            return Vec::new();
        };
        let Some(conn) = self.conn.as_ref() else {
            return Vec::new();
        };
        let sql = "SELECT n.name, e.size, e.mtime FROM entries e \
                   JOIN names n ON n.id = e.name_id \
                   WHERE e.parent_id = ?1 ORDER BY e.size DESC LIMIT ?2";
        let Ok(mut stmt) = conn.prepare_cached(sql) else {
            return Vec::new();
        };
        stmt.query_map(params![dir_id, limit as i64], |r| {
            let name: String = r.get(0)?;
            Ok((name, r.get::<_, i64>(1)?, r.get::<_, i64>(2)?))
        })
        .map(|iter| {
            iter.flatten()
                .map(|(name, size, mtime)| (join_path(parent, &name), size, mtime, false))
                .collect()
        })
        .unwrap_or_default()
    }

    pub fn child_files_rows(&self, parent: &str, limit: u64) -> Vec<Row> {
        self.child_files(parent, limit)
            .into_iter()
            .map(|(path, size, mtime, is_dir)| Row {
                path,
                size,
                mtime,
                is_dir,
            })
            .collect()
    }

    /// 目录自身的聚合（来自 rollup，等价 v3 的 folder_sizes）。
    pub fn dir_info(&self, path: &str) -> Option<(String, i64, i64)> {
        let dir_id = self.dir_id_of(path)?;
        let conn = self.conn.as_ref()?;
        conn.query_row(
            "SELECT size, file_count FROM rollup WHERE dir_id = ?1",
            [dir_id],
            |r| Ok((r.get::<_, i64>(0)?, r.get::<_, i64>(1)?)),
        )
        .ok()
        .map(|(size, file_count)| (path.to_owned(), size, file_count))
    }

    /// 命中数。无条件时直接用 meta 计数，避免对 442 万行做 COUNT。
    pub fn count(&self, query: &Query) -> u64 {
        let Some(conn) = self.conn.as_ref() else {
            return 0;
        };
        if query.is_none() {
            return self.total_entries;
        }
        let mut total = 0u64;
        if query.wants_files() {
            let (where_sql, params) = file_predicate(query);
            let sql = format!("SELECT COUNT(*) FROM entries e WHERE {where_sql}");
            if let Ok(mut stmt) = conn.prepare_cached(&sql) {
                if let Ok(value) = stmt
                    .query_row(params_from_iter(params.iter()), |r| r.get::<_, i64>(0))
                {
                    total += value.max(0) as u64;
                }
            }
        }
        if query.wants_dirs() {
            let (where_sql, params) = dir_predicate(query);
            let sql = format!("SELECT COUNT(*) FROM dirs d WHERE {where_sql}");
            if let Ok(mut stmt) = conn.prepare_cached(&sql) {
                if let Ok(value) = stmt
                    .query_row(params_from_iter(params.iter()), |r| r.get::<_, i64>(0))
                {
                    total += value.max(0) as u64;
                }
            }
        }
        total
    }

    pub fn count_filter(&self, filter: &Filter) -> u64 {
        self.count(&Query::from_filter(filter))
    }

    /// 取 [offset, offset+limit) 一段；路径在 SQL 里现拼（父目录名 + 名字）。
    pub fn page(
        &self,
        query: &Query,
        sort: SortBy,
        offset: u64,
        limit: u64,
    ) -> (Vec<(String, i64, i64, bool)>, f64) {
        let started = Instant::now();
        let Some(conn) = self.conn.as_ref() else {
            return (Vec::new(), 0.0);
        };
        if limit == 0 {
            return (Vec::new(), started.elapsed().as_secs_f64() * 1000.0);
        }
        let mut selects: Vec<String> = Vec::new();
        let mut params: Vec<Value> = Vec::new();
        if query.wants_files() {
            let (where_sql, mut where_params) = file_predicate(query);
            selects.push(format!(
                "SELECT {FILE_PATH_SQL} AS path, e.size AS size, e.mtime AS mtime, 0 AS is_dir \
                 FROM entries e JOIN names n ON n.id = e.name_id \
                 JOIN dir_paths p ON p.dir_id = e.parent_id WHERE {where_sql}"
            ));
            params.append(&mut where_params);
        }
        if query.wants_dirs() {
            let (where_sql, mut where_params) = dir_predicate(query);
            selects.push(format!(
                "SELECT p.path AS path, 0 AS size, 0 AS mtime, 1 AS is_dir \
                 FROM dirs d JOIN dir_paths p ON p.dir_id = d.id WHERE {where_sql}"
            ));
            params.append(&mut where_params);
        }
        if selects.is_empty() {
            return (Vec::new(), started.elapsed().as_secs_f64() * 1000.0);
        }
        // NameAsc/NameDesc 与 v3 一样按路径近似（同目录下才需要真名序）
        let order = match sort {
            SortBy::PathAsc | SortBy::NameAsc => "1 ASC",
            SortBy::PathDesc | SortBy::NameDesc => "1 DESC",
            SortBy::SizeDesc => "2 DESC",
            SortBy::SizeAsc => "2 ASC",
            SortBy::MtimeDesc => "3 DESC",
            SortBy::MtimeAsc => "3 ASC",
        };
        let sql = format!(
            "SELECT path, size, mtime, is_dir FROM ({}) ORDER BY {order} LIMIT ? OFFSET ?",
            selects.join(" UNION ALL ")
        );
        params.push(Value::Integer(limit as i64));
        params.push(Value::Integer(offset as i64));
        let mut stmt = match conn.prepare_cached(&sql) {
            Ok(stmt) => stmt,
            Err(_) => return (Vec::new(), started.elapsed().as_secs_f64() * 1000.0),
        };
        let rows = stmt
            .query_map(params_from_iter(params.iter()), |r| {
                Ok((
                    r.get::<_, String>(0)?,
                    r.get::<_, i64>(1)?,
                    r.get::<_, i64>(2)?,
                    r.get::<_, i64>(3)? != 0,
                ))
            })
            .map(|iter| iter.flatten().collect::<Vec<_>>())
            .unwrap_or_default();
        (rows, started.elapsed().as_secs_f64() * 1000.0)
    }

    pub fn page_filter(
        &self,
        filter: &Filter,
        sort: SortBy,
        offset: u64,
        limit: u64,
    ) -> (Vec<Row>, f64) {
        let (rows, ms) = self.page(&Query::from_filter(filter), sort, offset, limit);
        (
            rows.into_iter()
                .map(|(path, size, mtime, is_dir)| Row {
                    path,
                    size,
                    mtime,
                    is_dir,
                })
                .collect(),
            ms,
        )
    }

    /// 祖先链（从根到自身），与 v3 同形。
    pub fn ancestors(&self, path: &str) -> Vec<String> {
        let mut out = Vec::new();
        let mut current = path.to_owned();
        for _ in 0..64 {
            out.push(current.clone());
            match crate::db::parent_dir(&current) {
                Some(parent) if !parent.is_empty() => current = parent,
                _ => break,
            }
        }
        out.reverse();
        out
    }
}
#[cfg(test)]
mod tests_v4 {
    //! 只读层的自检：跨 v3/v4 的一致性 + 性能对比。
    //!
    //! 默认跳过（无人值守环境没有快照路径），用环境变量驱动：
    //!   DI_V3=<v3 快照>  DI_V4=<v4 快照>  DI_CASE=v3:count_dll
    //! 每个 case 起一个独立进程，RSS 峰值才有意义。
    use super::*;
    use crate::db::{Snapshot, SortBy};
    use crate::filter::Filter;

    fn var(key: &str) -> Option<String> {
        std::env::var(key).ok().filter(|v| !v.trim().is_empty())
    }

    fn median(mut values: Vec<f64>) -> f64 {
        if values.is_empty() {
            return 0.0;
        }
        values.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
        values[values.len() / 2]
    }

    /// 结果指纹：路径 + 大小，用来证明 v3 / v4 返回的内容逐条一致。
    fn fingerprint_paths(items: &[(String, i64)]) -> u64 {
        let mut hash: u64 = 0xcbf2_9ce4_8422_2325;
        for (path, size) in items {
            for byte in path.as_bytes() {
                hash ^= *byte as u64;
                hash = hash.wrapping_mul(0x100_0000_01b3);
            }
            for byte in size.to_le_bytes() {
                hash ^= byte as u64;
                hash = hash.wrapping_mul(0x100_0000_01b3);
            }
        }
        hash
    }

    fn dll_filter() -> Filter {
        Filter {
            terms: vec!["dll".to_owned()],
            ..Default::default()
        }
    }

    fn dll_scope_filter() -> Filter {
        Filter {
            terms: vec!["dll".to_owned()],
            scope: Some("C:\\Program Files".to_owned()),
            ..Default::default()
        }
    }

    fn dll_query() -> Query {
        Query::from_filter(&dll_filter())
    }

    fn dll_scope_query() -> Query {
        Query::from_filter(&dll_scope_filter())
    }

    fn windows() -> &'static str {
        "C:\\Windows"
    }

    #[test]
    fn bench_case() {
        let Some(case) = var("DI_CASE") else {
            return;
        };
        let (side, name) = match case.split_once(':') {
            Some(parts) => parts,
            None => panic!("DI_CASE 形如 v4:count_dll，实际 {case}"),
        };
        let v3_path = var("DI_V3").expect("缺 DI_V3");
        let v4_path = var("DI_V4").expect("缺 DI_V4");
        let mut timings: Vec<f64> = Vec::new();
        let mut summary = String::new();

        match side {
            "v4" => {
                let snap = V4Snapshot::open(Path::new(&v4_path)).expect("打开 v4 失败");
                let open_ms = snap.open_ms;
                for _ in 0..3 {
                    match name {
                        "count_dll" => {
                            let started = Instant::now();
                            let value = snap.count(&dll_query());
                            timings.push(started.elapsed().as_secs_f64() * 1000.0);
                            summary = format!("count={value}");
                        }
                        "page_dll" => {
                            let started = Instant::now();
                            let (rows, _ms) = snap.page(&dll_query(), SortBy::PathAsc, 0, 200);
                            timings.push(started.elapsed().as_secs_f64() * 1000.0);
                            let pairs: Vec<(String, i64)> =
                                rows.iter().map(|r| (r.0.clone(), r.1)).collect();
                            summary = format!(
                                "rows={} fp={:016x} first={:?}",
                                rows.len(),
                                fingerprint_paths(&pairs),
                                rows.first().map(|r| (r.0.as_str(), r.1))
                            );
                        }
                        "child_dirs" => {
                            let started = Instant::now();
                            let rows = snap.child_dirs(windows(), 100);
                            timings.push(started.elapsed().as_secs_f64() * 1000.0);
                            let pairs: Vec<(String, i64)> =
                                rows.iter().map(|r| (r.0.clone(), r.1)).collect();
                            summary = format!(
                                "rows={} fp={:016x} first={:?}",
                                rows.len(),
                                fingerprint_paths(&pairs),
                                rows.first().map(|r| (r.0.as_str(), r.1, r.2))
                            );
                        }
                        "child_files" => {
                            let started = Instant::now();
                            let rows = snap.child_files(windows(), 100);
                            timings.push(started.elapsed().as_secs_f64() * 1000.0);
                            let pairs: Vec<(String, i64)> =
                                rows.iter().map(|r| (r.0.clone(), r.1)).collect();
                            summary = format!(
                                "rows={} fp={:016x} first={:?}",
                                rows.len(),
                                fingerprint_paths(&pairs),
                                rows.first().map(|r| (r.0.as_str(), r.1, r.2))
                            );
                        }
                        "dir_info" => {
                            let started = Instant::now();
                            let info = snap.dir_info(windows());
                            timings.push(started.elapsed().as_secs_f64() * 1000.0);
                            summary = format!("info={info:?}");
                        }
                        "count_scope" => {
                            let started = Instant::now();
                            let value = snap.count(&dll_scope_query());
                            timings.push(started.elapsed().as_secs_f64() * 1000.0);
                            summary = format!("count={value}");
                        }
                        other => panic!("未知 case {other}"),
                    }
                }
                println!(
                    "SIDE=v4 CASE={name} open_ms={open_ms:.2} {summary} times={:?} median_ms={:.2} rss_mb={:.1} peak_mb={:.1}",
                    timings.iter().map(|v| (v * 100.0).round() / 100.0).collect::<Vec<_>>(),
                    median(timings.clone()),
                    crate::store::rss_bytes() as f64 / 1_048_576.0,
                    crate::store::peak_rss_bytes() as f64 / 1_048_576.0
                );
            }
            "v3" => {
                let snap = Snapshot::open_path(Path::new(&v3_path)).expect("打开 v3 失败");
                let open_ms = snap.open_ms;
                for _ in 0..3 {
                    match name {
                        "count_dll" => {
                            let started = Instant::now();
                            let value = snap.count(&dll_filter());
                            timings.push(started.elapsed().as_secs_f64() * 1000.0);
                            summary = format!("count={value}");
                        }
                        "page_dll" => {
                            let started = Instant::now();
                            let (rows, _ms) = snap.page(&dll_filter(), SortBy::PathAsc, 0, 200);
                            timings.push(started.elapsed().as_secs_f64() * 1000.0);
                            let pairs: Vec<(String, i64)> =
                                rows.iter().map(|r| (r.path.clone(), r.size)).collect();
                            summary = format!(
                                "rows={} fp={:016x} first={:?}",
                                rows.len(),
                                fingerprint_paths(&pairs),
                                rows.first().map(|r| (r.path.as_str(), r.size))
                            );
                        }
                        "child_dirs" => {
                            let started = Instant::now();
                            let rows = snap.child_dirs(windows(), 100);
                            timings.push(started.elapsed().as_secs_f64() * 1000.0);
                            let pairs: Vec<(String, i64)> =
                                rows.iter().map(|r| (r.path.clone(), r.size)).collect();
                            summary = format!(
                                "rows={} fp={:016x} first={:?}",
                                rows.len(),
                                fingerprint_paths(&pairs),
                                rows.first().map(|r| (r.path.as_str(), r.size, r.file_count))
                            );
                        }
                        "child_files" => {
                            let started = Instant::now();
                            let rows = snap.child_files(windows(), 100);
                            timings.push(started.elapsed().as_secs_f64() * 1000.0);
                            let pairs: Vec<(String, i64)> =
                                rows.iter().map(|r| (r.path.clone(), r.size)).collect();
                            summary = format!(
                                "rows={} fp={:016x} first={:?}",
                                rows.len(),
                                fingerprint_paths(&pairs),
                                rows.first().map(|r| (r.path.as_str(), r.size))
                            );
                        }
                        "dir_info" => {
                            let started = Instant::now();
                            let info = snap
                                .dir_info(windows())
                                .map(|row| (row.path, row.size, row.file_count));
                            timings.push(started.elapsed().as_secs_f64() * 1000.0);
                            summary = format!("info={info:?}");
                        }
                        "count_scope" => {
                            let started = Instant::now();
                            let value = snap.count(&dll_scope_filter());
                            timings.push(started.elapsed().as_secs_f64() * 1000.0);
                            summary = format!("count={value}");
                        }
                        other => panic!("未知 case {other}"),
                    }
                }
                println!(
                    "SIDE=v3 CASE={name} open_ms={open_ms:.2} {summary} times={:?} median_ms={:.2} rss_mb={:.1} peak_mb={:.1}",
                    timings.iter().map(|v| (v * 100.0).round() / 100.0).collect::<Vec<_>>(),
                    median(timings.clone()),
                    crate::store::rss_bytes() as f64 / 1_048_576.0,
                    crate::store::peak_rss_bytes() as f64 / 1_048_576.0
                );
            }
            other => panic!("未知 side {other}"),
        }
    }
}


#[cfg(test)]
mod tests_parity {
    //! v3 / v4 结果一致性矩阵（用 DI_PARITY=1 打开）。
    use super::*;
    use crate::db::{Snapshot, SortBy};
    use crate::filter::{Filter, Kind};

    fn var(key: &str) -> Option<String> {
        std::env::var(key).ok().filter(|v| !v.trim().is_empty())
    }

    fn fp(items: &[(String, i64)]) -> u64 {
        let mut hash: u64 = 0xcbf2_9ce4_8422_2325;
        for (path, size) in items {
            for byte in path.as_bytes() {
                hash ^= *byte as u64;
                hash = hash.wrapping_mul(0x100_0000_01b3);
            }
            for byte in size.to_le_bytes() {
                hash ^= byte as u64;
                hash = hash.wrapping_mul(0x100_0000_01b3);
            }
        }
        hash
    }

    fn terms(list: &[&str]) -> Vec<String> {
        list.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn parity_matrix() {
        if var("DI_PARITY").is_none() {
            return;
        }
        let v3_path = var("DI_V3").expect("缺 DI_V3");
        let v4_path = var("DI_V4").expect("缺 DI_V4");
        let v3 = Snapshot::open_path(Path::new(&v3_path)).expect("打开 v3 失败");
        let v4 = V4Snapshot::open(Path::new(&v4_path)).expect("打开 v4 失败");
        // 只读兼容：v4 读取层必须拒绝 v3 文件
        assert!(
            V4Snapshot::open(Path::new(&v3_path)).is_err(),
            "V4Snapshot 不该接受 format_version=3 的文件"
        );
        println!("PARITY v4 拒绝 v3 文件：OK");

        let cases: Vec<(&str, Filter)> = vec![
            ("term dll", Filter { terms: terms(&["dll"]), ..Default::default() }),
            ("term Windows", Filter { terms: terms(&["Windows"]), ..Default::default() }),
            ("term Program Files", Filter { terms: terms(&["Program Files"]), ..Default::default() }),
            ("term dll + ext exe", Filter { terms: terms(&["dll"]), exts: terms(&["exe"]), ..Default::default() }),
            ("ext py", Filter { exts: terms(&["py"]), ..Default::default() }),
            ("ext py,ts + term node", Filter { exts: terms(&["py", "ts"]), terms: terms(&["node"]), ..Default::default() }),
            ("size>1g", Filter { min_size: Some(1 << 30), ..Default::default() }),
            ("size 1k..10k kind file", Filter { min_size: Some(1024), max_size: Some(10240), kind: Some(Kind::File), ..Default::default() }),
            ("kind dir + node_modules", Filter { terms: terms(&["node_modules"]), kind: Some(Kind::Dir), ..Default::default() }),
            ("kind file + dll", Filter { terms: terms(&["dll"]), kind: Some(Kind::File), ..Default::default() }),
            ("scope C:\\Program Files + dll", Filter { terms: terms(&["dll"]), scope: Some("C:\\Program Files".to_owned()), ..Default::default() }),
            ("scope D: + dll", Filter { terms: terms(&["dll"]), scope: Some("D:".to_owned()), ..Default::default() }),
            ("scope C:\\Windows", Filter { scope: Some("C:\\Windows".to_owned()), ..Default::default() }),
            ("straddle Windows\\explorer", Filter { terms: terms(&["Windows\\explorer"]), ..Default::default() }),
            ("sep Windows\\System32", Filter { terms: terms(&["Windows\\System32"]), ..Default::default() }),
            ("cn 报告", Filter { terms: terms(&["报告"]), ..Default::default() }),
            ("brace {E52895A9", Filter { terms: terms(&["{E52895A9"]), ..Default::default() }),
            ("like meta 50%_off", Filter { terms: terms(&["50%_off"]), ..Default::default() }),
            ("empty", Filter::default()),
        ];
        let mut fails = 0;
        for (name, filter) in &cases {
            let left = v3.count(filter);
            let right = v4.count_filter(filter);
            let ok = left == right;
            if !ok {
                fails += 1;
            }
            println!(
                "PARITY count {name:34} v3={left:>9} v4={right:>9} {}",
                if ok { "OK" } else { "MISMATCH" }
            );
        }

        // 路径序分页：路径唯一，指纹可以逐条比对
        for (name, filter, offset, limit) in [
            ("page dll path 0..100", Filter { terms: terms(&["dll"]), ..Default::default() }, 0u64, 100u64),
            ("page dll path 1000..1100", Filter { terms: terms(&["dll"]), ..Default::default() }, 1000, 100),
            ("page all path 0..50", Filter::default(), 0, 50),
            ("page scope C:\\Windows path 0..50", Filter { scope: Some("C:\\Windows".to_owned()), ..Default::default() }, 0, 50),
        ] {
            let (left_rows, _) = v3.page(&filter, SortBy::PathAsc, offset, limit);
            let (right_rows, _) = v4.page_filter(&filter, SortBy::PathAsc, offset, limit);
            let left: Vec<(String, i64)> = left_rows.iter().map(|r| (r.path.clone(), r.size)).collect();
            let right: Vec<(String, i64)> = right_rows.iter().map(|r| (r.path.clone(), r.size)).collect();
            let ok = left.len() == right.len() && fp(&left) == fp(&right);
            if !ok {
                fails += 1;
                println!("PARITY {name:34} v3={} v4={} MISMATCH", left.len(), right.len());
                println!("   v3 first={:?} last={:?}", left.first(), left.last());
                println!("   v4 first={:?} last={:?}", right.first(), right.last());
            } else {
                println!("PARITY {name:34} v3={} v4={} OK", left.len(), right.len());
            }
        }

        // 目录类：child_dirs / child_files / dir_info / roots 逐项比对
        for dir in ["C:\\", "C:\\Windows", "C:\\Program Files", "D:\\", "F:\\"] {
            let left_dirs: Vec<(String, i64, i64)> = v3
                .child_dirs(dir, 50)
                .into_iter()
                .map(|r| (r.path, r.size, r.file_count))
                .collect();
            let right_dirs = v4.child_dirs(dir, 50);
            let left_files: Vec<(String, i64, i64)> = v3
                .child_files(dir, 50)
                .into_iter()
                .map(|r| (r.path, r.size, r.mtime))
                .collect();
            let right_files: Vec<(String, i64, i64)> = v4
                .child_files(dir, 50)
                .into_iter()
                .map(|r| (r.0, r.1, r.2))
                .collect();
            let info_left = v3.dir_info(dir).map(|r| (r.path, r.size, r.file_count));
            let info_right = v4.dir_info(dir);
            let ok = left_dirs == right_dirs && left_files == right_files && info_left == info_right;
            if !ok {
                fails += 1;
                println!("PARITY dir {dir:32} MISMATCH");
                println!("   child_dirs v3={} v4={}", left_dirs.len(), right_dirs.len());
                println!("   child_files v3={} v4={}", left_files.len(), right_files.len());
                println!("   info v3={info_left:?} v4={info_right:?}");
                if left_dirs != right_dirs {
                    println!("   first v3={:?}", left_dirs.first());
                    println!("   first v4={:?}", right_dirs.first());
                }
            } else {
                println!("PARITY dir {dir:32} dirs={} files={} OK", left_dirs.len(), left_files.len());
            }
        }
        let roots_left: Vec<(String, i64, i64)> = v3
            .roots_rows()
            .into_iter()
            .map(|r| (r.path, r.size, r.file_count))
            .collect();
        let roots_right = v4.roots();
        if roots_left != roots_right {
            fails += 1;
            println!("PARITY roots MISMATCH\n   v3={roots_left:?}\n   v4={roots_right:?}");
        } else {
            println!("PARITY roots {} OK", roots_left.len());
        }

        // 时间条件：v3 的 filter 侧把 Unix 秒当 FILETIME 比较（历史缺陷，恒为 0），
        // v4 按 Unix 秒正确比较，所以这里只记录差异、不计入失败。
        let after = Filter { after: Some(crate::filter::parse_date("2026-01-01").unwrap()), ..Default::default() };
        println!(
            "PARITY after:2026-01-01 v3={} v4={}（已知口径差异：v3 换算 FILETIME）",
            v3.count(&after),
            v4.count_filter(&after)
        );

        println!("PARITY_SUMMARY fails={fails} cases={}", cases.len() + 4 + 5 + 1);
        assert_eq!(fails, 0, "v3 / v4 结果不一致，见上方 MISMATCH");
    }
}

//! 快照增量：v3 全量快照 → v4 delta 文件（压缩）与 v4 delta → 标准快照（物化）。
//!
//! # delta 文件格式（v4）
//!
//! 单个 SQLite 文件（后缀 .delta.sqlite），两张表：
//!
//!     CREATE TABLE delta (
//!         path   TEXT PRIMARY KEY,
//!         op     INTEGER,   -- 1=新增 2=消失 3=变化（size / mtime / is_dir 至少一项不同）
//!         size   INTEGER,   -- op=1/3 取新快照的值；op=2 取旧快照（消失前）的值
//!         mtime  INTEGER,
//!         is_dir INTEGER
//!     ) WITHOUT ROWID;
//!     CREATE TABLE meta (key TEXT PRIMARY KEY, value TEXT NOT NULL);
//!
//! meta 必有的键：format_version（固定 4-delta）、base_id、target_id、
//! created_at（Unix 秒）、row_count、source_bytes（base + target 两份全量快照的
//! 字节数之和，即这份 delta 替代掉的全量存储量）。另附 base_bytes、
//! target_bytes、strategy 三个排障用键，读方忽略即可。
//!
//! # 三种计算策略
//!
//! | 策略 | 做法 | 本机实测（446 万行/份） |
//! |---|---|---|
//! | Merge | 两份快照各顺序扫一遍，在 Rust 里线性归并 | 见报告，默认 |
//! | FullOuterJoin | 需求原文方案：ATTACH + FULL OUTER JOIN 一次算出 | 24.5 s / 58.1～69.2 s |
//! | ThreeStep | 新增 / 消失 / 变化 各一条 SQL（FULL OUTER JOIN 超时的退路） | 42.8 s |
//!
//! 默认按快照结构挑：entries 是 WITHOUT ROWID 且主键为 path（v3 起）时，
//! 顺序扫出来天然按 path 升序，直接用归并；老结构（v1/v2，rowid 表）退回三步 SQL。
//! 归并过程会逐行校验升序，一旦发现乱序立即报错而不是写出错的 delta；
//! 需要强制指定策略时设环境变量 DISKINTEL_COMPACT_STRATEGY=merge|foj|steps。
//!
//! # 只读与落盘
//!
//! - 源快照一律 SQLITE_OPEN_READ_ONLY + PRAGMA query_only=ON 打开；
//! - 输出先写 <out>.tmp / <out>.partial，全部自检通过后才改名成目标文件，
//!   失败时保留半成品并在错误信息里指明路径（本工具不删除任何文件）；
//! - 目标文件已存在直接报错，绝不覆盖。
//!
//! # 物化的两处已知损失（delta 格式本身的边界）
//!
//! delta 只记 path / size / mtime / is_dir，因此物化时：
//! - op=3（变化）走 UPDATE，parent_path / attributes / ext 沿用库里的旧值，无损；
//! - op=1（新增）没有来源可查：parent_path 与 ext 由路径推出（与 v3 写法一致），
//!   attributes 记为 0。想完全无损就得让 delta 也存这三列，属格式变更，本期不做。
//!
//! folder_sizes 则整体重算（逐目录递归求子树和 + 文件数），不是估出来的：
//! 对原始快照做过同口径回放，635,426 行与库内预聚合值 0 处差异。

use std::cmp::Ordering;
use std::fs;
use std::io::Read;
use std::path::{Path, PathBuf};
use std::time::{Instant, UNIX_EPOCH};

use rusqlite::{Connection, OpenFlags, params};

use super::{DELTA_FORMAT_VERSION, OP_ADDED, OP_CHANGED, OP_REMOVED};

/// 每批写入 delta 的行数：限制单次事务的回滚页，也让长任务有稳定的提交节奏。
const BATCH_ROWS: usize = 20_000;

/// 顺序扫描（不带 ORDER BY）。v3 的 entries 是 WITHOUT ROWID + path 主键，
/// 表本身就是 path 序的 B 树，扫出来即升序，不需要额外排序（排序会多一遍临时表）。
const SCAN_SQL: &str = "SELECT path, size, mtime, is_dir FROM entries";

const INSERT_ROW_SQL: &str =
    "INSERT INTO delta(path, op, size, mtime, is_dir) VALUES (?1, ?2, ?3, ?4, ?5)";

const DELTA_SCHEMA_SQL: &str = "\
CREATE TABLE delta (
    path   TEXT PRIMARY KEY,
    op     INTEGER,
    size   INTEGER,
    mtime  INTEGER,
    is_dir INTEGER
) WITHOUT ROWID;
CREATE TABLE meta (
    key   TEXT PRIMARY KEY,
    value TEXT NOT NULL
);";

/// 需求原文方案：ATTACH + FULL OUTER JOIN 一次算出新增 / 消失 / 变化。
const FOJ_SQL: &str = "\
SELECT CASE WHEN s.path IS NULL THEN 1 WHEN t.path IS NULL THEN 2 ELSE 3 END AS op,
       CASE WHEN t.path IS NULL THEN s.path  ELSE t.path  END AS path,
       CASE WHEN t.path IS NULL THEN s.size  ELSE t.size  END AS size,
       CASE WHEN t.path IS NULL THEN s.mtime ELSE t.mtime END AS mtime,
       CASE WHEN t.path IS NULL THEN s.is_dir ELSE t.is_dir END AS is_dir
  FROM base.entries AS s
  FULL OUTER JOIN main.entries AS t ON t.path = s.path
 WHERE s.path IS NULL
    OR t.path IS NULL
    OR s.size  IS NOT t.size
    OR s.mtime IS NOT t.mtime
    OR s.is_dir IS NOT t.is_dir";

/// 三步 SQL 的第一条：新增。
const STEP_ADDED_SQL: &str = "\
SELECT 1, t.path, t.size, t.mtime, t.is_dir
  FROM main.entries AS t
  LEFT JOIN base.entries AS s ON s.path = t.path
 WHERE s.path IS NULL";

/// 三步 SQL 的第二条：消失。
const STEP_REMOVED_SQL: &str = "\
SELECT 2, s.path, s.size, s.mtime, s.is_dir
  FROM base.entries AS s
  LEFT JOIN main.entries AS t ON t.path = s.path
 WHERE t.path IS NULL";

/// 三步 SQL 的第三条：变化。
const STEP_CHANGED_SQL: &str = "\
SELECT 3, t.path, t.size, t.mtime, t.is_dir
  FROM main.entries AS t
  JOIN base.entries AS s ON s.path = t.path
 WHERE t.size IS NOT s.size OR t.mtime IS NOT s.mtime OR t.is_dir IS NOT s.is_dir";

/// 生成 delta 的计算策略。
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Strategy {
    /// 顺序归并：两份快照各顺序扫一遍，在 Rust 里线性合并（默认）。
    Merge,
    /// 需求原文方案：ATTACH + FULL OUTER JOIN。
    FullOuterJoin,
    /// 三步 SQL（新增 / 消失 / 变化 各一条）。
    ThreeStep,
}

impl Strategy {
    pub fn label(self) -> &'static str {
        match self {
            Strategy::Merge => "merge（顺序归并）",
            Strategy::FullOuterJoin => "foj（ATTACH + FULL OUTER JOIN）",
            Strategy::ThreeStep => "steps（三步 SQL）",
        }
    }

    /// 环境变量覆盖，用于复现测量：DISKINTEL_COMPACT_STRATEGY=merge|foj|steps。
    fn from_env() -> Option<Self> {
        let raw = std::env::var("DISKINTEL_COMPACT_STRATEGY").ok()?;
        match raw.trim().to_ascii_lowercase().as_str() {
            "merge" => Some(Strategy::Merge),
            "foj" | "full" | "full_outer_join" => Some(Strategy::FullOuterJoin),
            "steps" | "step" | "three_step" => Some(Strategy::ThreeStep),
            _ => None,
        }
    }
}

/// 生成 delta 的实测结果。
pub struct DeltaStats {
    pub base_id: String,
    pub target_id: String,
    pub added: u64,
    pub removed: u64,
    pub changed: u64,
    pub row_count: u64,
    pub base_bytes: u64,
    pub target_bytes: u64,
    pub delta_bytes: u64,
    pub batches: u64,
    pub elapsed_ms: f64,
    pub strategy: Strategy,
    pub quick_check: String,
}

/// 两份快照逐行对比的差异数（口径与 delta 一致）。
#[derive(Clone, Copy, Default, Debug)]
pub struct DiffCounts {
    pub added: u64,
    pub removed: u64,
    pub changed: u64,
}

impl DiffCounts {
    pub fn total(self) -> u64 {
        self.added + self.removed + self.changed
    }
}

/// 物化结果。
pub struct MaterializeStats {
    pub base_id: String,
    pub target_id: String,
    pub delta_ids: Vec<String>,
    pub rows: u64,
    pub file_count: u64,
    pub dir_count: u64,
    pub total_size: i64,
    pub folder_rows: u64,
    pub deleted: u64,
    pub updated: u64,
    pub inserted: u64,
    pub copy_ms: f64,
    pub apply_ms: f64,
    pub folder_ms: f64,
    pub verify_ms: f64,
    pub elapsed_ms: f64,
    pub integrity: String,
    pub format_version: i64,
    pub out_bytes: u64,
    /// 原始快照（base 同目录下的 <target_id>.sqlite）；找不到就没有比对对象。
    pub reference: Option<PathBuf>,
    pub reference_rows: u64,
    pub reference_total_size: i64,
    pub reference_folder_rows: u64,
    /// 行数 / 总大小是否与原始快照一致。
    pub counts_match: bool,
    /// 逐行对比（path/size/mtime/is_dir）结果；有参考文件时才有值。
    pub diff: Option<DiffCounts>,
    /// folder_sizes 与原始快照的差异行数；有参考文件时才有值。
    pub folder_mismatch: Option<u64>,
}

impl MaterializeStats {
    /// 验收口径：行数 / 总大小一致，且（有参考文件时）逐行与文件夹聚合都无差异。
    pub fn verified(&self) -> bool {
        self.integrity == "ok"
            && self.counts_match
            && self.diff.map(|diff| diff.total() == 0).unwrap_or(false)
            && self.folder_mismatch.map(|count| count == 0).unwrap_or(false)
    }
}

/// 生成一份 delta：base（旧快照） → target（新快照） → out。
///
/// 源快照只读；out 已存在时报错（不覆盖），成功返回前会做「行数回读 + quick_check」自检。
pub fn write_delta(base: &Path, target: &Path, out: &Path) -> Result<DeltaStats, String> {
    let started = Instant::now();
    ensure_regular_file(base, "base 快照")?;
    ensure_regular_file(target, "target 快照")?;
    if same_file(base, out) || same_file(target, out) {
        return Err("输出不能与源快照同路径：源快照只读，绝不覆盖".to_owned());
    }
    if out.exists() {
        return Err(format!(
            "输出已存在：{}。本工具不覆盖任何文件，请换一个文件名",
            out.display()
        ));
    }
    let tmp = sidecar(out, ".tmp");
    if tmp.exists() {
        return Err(format!(
            "临时文件已存在：{}。请先人工确认后处理（本工具不删除文件）",
            tmp.display()
        ));
    }
    if let Some(parent) = out.parent() {
        if !parent.as_os_str().is_empty() && !parent.exists() {
            fs::create_dir_all(parent)
                .map_err(|error| format!("创建 {} 失败：{error}", parent.display()))?;
        }
    }

    let base_id = snapshot_id(base)?;
    let target_id = snapshot_id(target)?;
    let base_bytes = file_len(base)?;
    let target_bytes = file_len(target)?;
    let strategy = Strategy::from_env().unwrap_or_else(|| pick_strategy(base, target));

    let conn = Connection::open_with_flags(
        &tmp,
        OpenFlags::SQLITE_OPEN_READ_WRITE | OpenFlags::SQLITE_OPEN_CREATE,
    )
    .map_err(|error| format!("新建 {} 失败：{error}", tmp.display()))?;
    // 产物是可重建的中间文件，写到一半崩溃只会留下 .tmp、不会污染任何源快照，
    // 因此关掉回滚日志、关掉 fsync 换写入速度；改名前的回读自检兜住正确性。
    conn.execute_batch(
        "PRAGMA journal_mode=MEMORY; PRAGMA synchronous=OFF; PRAGMA cache_size=-65536; PRAGMA temp_store=MEMORY;",
    )
    .map_err(|error| format!("设置 delta 写入参数失败：{error}"))?;
    conn.execute_batch(DELTA_SCHEMA_SQL)
        .map_err(|error| format!("创建 delta 表失败：{error}"))?;

    let mut writer = DeltaWriter::new(&conn);
    let streamed = match strategy {
        Strategy::Merge => merge_stream(base, target, &mut |row: DeltaRow| writer.push(row)),
        Strategy::FullOuterJoin => foj_stream(base, target, &mut |row: DeltaRow| writer.push(row)),
        Strategy::ThreeStep => steps_stream(base, target, &mut |row: DeltaRow| writer.push(row)),
    };
    if let Err(error) = streamed {
        return Err(format!(
            "{error}（未写完的临时文件保留在 {}，请人工确认后处理）",
            tmp.display()
        ));
    }
    if let Err(error) = writer.finish() {
        return Err(format!(
            "{error}（未写完的临时文件保留在 {}，请人工确认后处理）",
            tmp.display()
        ));
    }
    let (added, removed, changed, batches) = (
        writer.added,
        writer.removed,
        writer.changed,
        writer.batches,
    );
    drop(writer);

    let row_count = added + removed + changed;
    let now = unix_now();
    write_meta_pair(&conn, "format_version", DELTA_FORMAT_VERSION)?;
    write_meta_pair(&conn, "base_id", &base_id)?;
    write_meta_pair(&conn, "target_id", &target_id)?;
    write_meta_pair(&conn, "created_at", &now.to_string())?;
    write_meta_pair(&conn, "row_count", &row_count.to_string())?;
    write_meta_pair(&conn, "source_bytes", &(base_bytes + target_bytes).to_string())?;
    write_meta_pair(&conn, "base_bytes", &base_bytes.to_string())?;
    write_meta_pair(&conn, "target_bytes", &target_bytes.to_string())?;
    write_meta_pair(&conn, "strategy", strategy.label())?;

    let read_back = query_int(&conn, "SELECT COUNT(*) FROM delta")?;
    if read_back != row_count {
        return Err(format!(
            "delta 行数自检失败：写入 {row_count} 行，读回 {read_back} 行（保留在 {}）",
            tmp.display()
        ));
    }
    let quick_check = conn
        .query_row("PRAGMA quick_check", [], |row| row.get::<_, String>(0))
        .unwrap_or_else(|error| format!("quick_check 失败：{error}"));
    drop(conn);
    if quick_check != "ok" {
        return Err(format!(
            "delta 完整性自检失败：{quick_check}（保留在 {}）",
            tmp.display()
        ));
    }
    fs::rename(&tmp, out).map_err(|error| format!("改名 {} 失败：{error}", out.display()))?;
    let delta_bytes = file_len(out)?;

    Ok(DeltaStats {
        base_id,
        target_id,
        added,
        removed,
        changed,
        row_count,
        base_bytes,
        target_bytes,
        delta_bytes,
        batches,
        elapsed_ms: elapsed_ms(started),
        strategy,
        quick_check,
    })
}

/// 物化：base + 按 target_id 顺序排列的若干 deltas → 标准快照 out。
///
/// 做法是「先字节拷贝 base，再在副本上按 delta 做 DELETE / UPDATE / INSERT」，
/// 然后重算 folder_sizes 与 meta 的计数。源快照全程只读，out 已存在时报错。
pub fn materialize(base: &Path, deltas: &[PathBuf], out: &Path) -> Result<MaterializeStats, String> {
    let started = Instant::now();
    ensure_regular_file(base, "base 快照")?;
    if deltas.is_empty() {
        return Err("物化至少需要一份 delta".to_owned());
    }
    for delta in deltas {
        ensure_regular_file(delta, "delta 文件")?;
        if same_file(delta, out) {
            return Err("输出不能与 delta 同路径".to_owned());
        }
    }
    if same_file(base, out) {
        return Err("输出不能与 base 快照同路径：源快照只读，绝不覆盖".to_owned());
    }
    if out.exists() {
        return Err(format!(
            "输出已存在：{}。本工具不覆盖任何文件，请换一个文件名",
            out.display()
        ));
    }
    let tmp = sidecar(out, ".partial");
    if tmp.exists() {
        return Err(format!(
            "临时文件已存在：{}。请先人工确认后处理（本工具不删除文件）",
            tmp.display()
        ));
    }
    if let Some(parent) = out.parent() {
        if !parent.as_os_str().is_empty() && !parent.exists() {
            fs::create_dir_all(parent)
                .map_err(|error| format!("创建 {} 失败：{error}", parent.display()))?;
        }
    }
    ensure_byte_copyable(base)?;

    let base_id = snapshot_id(base)?;
    let mut delta_metas = Vec::with_capacity(deltas.len());
    for delta in deltas {
        delta_metas.push(read_delta_meta(delta)?);
    }
    // 链完整性：第一份 delta 的 base_id 必须是 base 快照，之后逐份首尾相接。
    let mut expected = base_id.clone();
    for (delta, meta) in deltas.iter().zip(delta_metas.iter()) {
        if meta.base_id != expected {
            return Err(format!(
                "delta 链断裂：{} 的 base_id 是 {}，期望 {}",
                delta.display(),
                meta.base_id,
                expected
            ));
        }
        expected = meta.target_id.clone();
    }
    let target_id = delta_metas
        .last()
        .map(|meta| meta.target_id.clone())
        .unwrap_or_else(|| base_id.clone());
    let delta_ids: Vec<String> = delta_metas.iter().map(|meta| meta.target_id.clone()).collect();

    let copy_started = Instant::now();
    let base_bytes = file_len(base)?;
    fs::copy(base, &tmp)
        .map_err(|error| format!("拷贝 {} → {} 失败：{error}", base.display(), tmp.display()))?;
    let copied_bytes = file_len(&tmp)?;
    if copied_bytes != base_bytes {
        return Err(format!(
            "拷贝不完整：base {base_bytes} 字节，副本 {copied_bytes} 字节（保留 {} 供核对）",
            tmp.display()
        ));
    }
    let copy_ms = elapsed_ms(copy_started);

    let conn = Connection::open_with_flags(&tmp, OpenFlags::SQLITE_OPEN_READ_WRITE)
        .map_err(|error| format!("打开副本 {} 失败：{error}", tmp.display()))?;
    conn.execute_batch(
        "PRAGMA journal_mode=MEMORY; PRAGMA synchronous=OFF; PRAGMA cache_size=-262144; PRAGMA temp_store=MEMORY;",
    )
    .map_err(|error| format!("设置副本写入参数失败：{error}"))?;

    let apply_started = Instant::now();
    let mut totals = ApplyCounts::default();
    for (index, delta) in deltas.iter().enumerate() {
        let counts = apply_delta(&conn, index, delta)?;
        totals.deleted += counts.deleted;
        totals.updated += counts.updated;
        totals.inserted += counts.inserted;
    }
    let apply_ms = elapsed_ms(apply_started);

    let folder_started = Instant::now();
    let folder_rows = rebuild_folder_sizes(&conn)?;
    let folder_ms = elapsed_ms(folder_started);

    let (rows, file_count, dir_count, total_size) = refresh_meta(&conn, &base_id, &target_id, &delta_ids)?;
    let integrity = conn
        .query_row("PRAGMA integrity_check", [], |row| row.get::<_, String>(0))
        .unwrap_or_else(|error| format!("integrity_check 失败：{error}"));
    let format_version = read_int_meta(&conn, "format_version");
    drop(conn);
    if integrity != "ok" {
        return Err(format!(
            "物化结果完整性失败：{integrity}（保留 {} 供核对）",
            tmp.display()
        ));
    }
    fs::rename(&tmp, out).map_err(|error| format!("改名 {} 失败：{error}", out.display()))?;
    let out_bytes = file_len(out)?;

    // ---- 验收：行数 / 总大小（以及尽量逐行）与原始快照对比 ----
    let verify_started = Instant::now();
    let reference = reference_snapshot(base, &target_id);
    let mut stats = MaterializeStats {
        base_id,
        target_id,
        delta_ids,
        rows,
        file_count,
        dir_count,
        total_size,
        folder_rows,
        deleted: totals.deleted,
        updated: totals.updated,
        inserted: totals.inserted,
        copy_ms,
        apply_ms,
        folder_ms,
        verify_ms: 0.0,
        elapsed_ms: 0.0,
        integrity,
        format_version,
        out_bytes,
        reference: None,
        reference_rows: 0,
        reference_total_size: 0,
        reference_folder_rows: 0,
        counts_match: false,
        diff: None,
        folder_mismatch: None,
    };

    if let Some(reference) = reference {
        let (ref_file_count, ref_dir_count, ref_total_size) = snapshot_counts(&reference)?;
        stats.reference_rows = ref_file_count + ref_dir_count;
        stats.reference_total_size = ref_total_size;
        stats.counts_match = stats.file_count == ref_file_count
            && stats.dir_count == ref_dir_count
            && stats.total_size == ref_total_size
            && stats.rows == stats.reference_rows;
        let strategy = pick_strategy(out, &reference);
        stats.diff = Some(count_diff(out, &reference, strategy)?);
        let (left_rows, right_rows, mismatches) = compare_folder_sizes(out, &reference)?;
        stats.reference_folder_rows = right_rows;
        stats.folder_mismatch = Some(if left_rows == right_rows {
            mismatches
        } else {
            mismatches + 1
        });
        stats.reference = Some(reference);
    }
    stats.verify_ms = elapsed_ms(verify_started);
    stats.elapsed_ms = elapsed_ms(started);
    Ok(stats)
}

/// 逐行对比两份快照，返回差异数（不写文件，口径同 delta）。
fn count_diff(base: &Path, target: &Path, strategy: Strategy) -> Result<DiffCounts, String> {
    let mut counts = DiffCounts::default();
    {
        let mut sink = |row: DeltaRow| -> Result<(), String> {
            match row.op {
                OP_ADDED => counts.added += 1,
                OP_REMOVED => counts.removed += 1,
                OP_CHANGED => counts.changed += 1,
                _ => {}
            }
            Ok(())
        };
        match strategy {
            Strategy::Merge => merge_stream(base, target, &mut sink)?,
            Strategy::FullOuterJoin => foj_stream(base, target, &mut sink)?,
            Strategy::ThreeStep => steps_stream(base, target, &mut sink)?,
        }
    }
    Ok(counts)
}

fn sidecar(path: &Path, suffix: &str) -> PathBuf {
    let mut raw = path.as_os_str().to_os_string();
    raw.push(suffix);
    PathBuf::from(raw)
}

fn elapsed_ms(started: Instant) -> f64 {
    started.elapsed().as_secs_f64() * 1000.0
}

fn unix_now() -> i64 {
    std::time::SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_secs() as i64)
        .unwrap_or(0)
}

fn file_len(path: &Path) -> Result<u64, String> {
    fs::metadata(path)
        .map(|meta| meta.len())
        .map_err(|error| format!("读取 {} 体积失败：{error}", path.display()))
}

fn ensure_regular_file(path: &Path, what: &str) -> Result<(), String> {
    match fs::metadata(path) {
        Ok(meta) if meta.is_file() => Ok(()),
        Ok(_) => Err(format!("{what} 不是普通文件：{}", path.display())),
        Err(error) => Err(format!("{what} 不存在：{}（{error}）", path.display())),
    }
}

fn same_file(left: &Path, right: &Path) -> bool {
    match (fs::canonicalize(left), fs::canonicalize(right)) {
        (Ok(left), Ok(right)) => left == right,
        _ => false,
    }
}

/// 打开源快照：只读 + query_only，并限制缓存与临时表落点。
fn open_read_only(path: &Path) -> Result<Connection, String> {
    let conn = Connection::open_with_flags(path, OpenFlags::SQLITE_OPEN_READ_ONLY)
        .map_err(|error| format!("只读打开 {} 失败：{error}", path.display()))?;
    conn.execute_batch("PRAGMA query_only=ON; PRAGMA cache_size=-262144; PRAGMA temp_store=FILE;")
        .map_err(|error| format!("设置 {} 只读参数失败：{error}", path.display()))?;
    Ok(conn)
}

/// ATTACH 的库名只做标识用，路径按 SQL 字符串字面量转义（单引号加倍）。
fn attach_read_only(conn: &Connection, path: &Path, alias: &str) -> Result<(), String> {
    let raw = path
        .to_str()
        .ok_or_else(|| format!("路径不是合法 UTF-8：{}", path.display()))?;
    let escaped = raw.replace('\'', "''");
    conn.execute_batch(&format!("ATTACH DATABASE '{escaped}' AS {alias}"))
        .map_err(|error| format!("ATTACH {} 失败：{error}", path.display()))
}

fn query_int(conn: &Connection, sql: &str) -> Result<u64, String> {
    conn.query_row(sql, [], |row| row.get::<_, i64>(0))
        .map(|value| value.max(0) as u64)
        .map_err(|error| format!("{sql} 失败：{error}"))
}

fn write_meta_pair(conn: &Connection, key: &str, value: &str) -> Result<(), String> {
    conn.execute(
        "INSERT INTO meta(key, value) VALUES (?1, ?2) ON CONFLICT(key) DO UPDATE SET value = excluded.value",
        params![key, value],
    )
    .map(|_| ())
    .map_err(|error| format!("写入 meta.{key} 失败：{error}"))
}

fn read_int_meta(conn: &Connection, key: &str) -> i64 {
    conn.query_row(
        "SELECT CAST(COALESCE((SELECT value FROM meta WHERE key = ?1), '0') AS INTEGER)",
        [key],
        |row| row.get::<_, i64>(0),
    )
    .unwrap_or(0)
}

fn has_column(conn: &Connection, table: &str, column: &str) -> Result<bool, String> {
    let mut stmt = conn
        .prepare(&format!("PRAGMA table_info({table})"))
        .map_err(|error| format!("读取 {table} 结构失败：{error}"))?;
    let mut rows = stmt
        .query([])
        .map_err(|error| format!("读取 {table} 结构失败：{error}"))?;
    while let Some(row) = rows
        .next()
        .map_err(|error| format!("读取 {table} 结构失败：{error}"))?
    {
        let name: String = row.get(1).map_err(|error| format!("读取列名失败：{error}"))?;
        if name.eq_ignore_ascii_case(column) {
            return Ok(true);
        }
    }
    Ok(false)
}

/// entries 是否「表即主键」的结构（v3 的 WITHOUT ROWID + path 主键）：
/// 只有这种结构顺序扫出来才天然按 path 升序，归并策略才成立。
fn is_path_ordered(path: &Path) -> bool {
    let Ok(conn) = open_read_only(path) else {
        return false;
    };
    let ddl: String = conn
        .query_row(
            "SELECT IFNULL(sql, '') FROM sqlite_master WHERE type = 'table' AND name = 'entries'",
            [],
            |row| row.get(0),
        )
        .unwrap_or_default();
    let upper = ddl.to_ascii_uppercase();
    upper.contains("WITHOUT ROWID") && upper.contains("PRIMARY KEY")
}

fn pick_strategy(base: &Path, target: &Path) -> Strategy {
    if is_path_ordered(base) && is_path_ordered(target) {
        Strategy::Merge
    } else {
        Strategy::ThreeStep
    }
}

/// base 同目录下按 target_id 找原始快照，用来做物化后的比对。
fn reference_snapshot(base: &Path, target_id: &str) -> Option<PathBuf> {
    let dir = base.parent()?;
    let candidate = dir.join(format!("{target_id}.sqlite"));
    if candidate.is_file() { Some(candidate) } else { None }
}

/// 快照 meta 里声明的条目数（与读取层同一口径）。
fn snapshot_counts(path: &Path) -> Result<(u64, u64, i64), String> {
    let conn = open_read_only(path)?;
    let file_count = read_int_meta(&conn, "file_count").max(0) as u64;
    let dir_count = read_int_meta(&conn, "dir_count").max(0) as u64;
    let total_size = read_int_meta(&conn, "total_size");
    Ok((file_count, dir_count, total_size))
}

/// 优先用 meta 里的 snapshot_id，没有就退回文件名主名。
fn snapshot_id(path: &Path) -> Result<String, String> {
    let conn = open_read_only(path)?;
    let from_meta: Option<String> = conn
        .query_row("SELECT value FROM meta WHERE key = 'snapshot_id'", [], |row| {
            row.get(0)
        })
        .ok();
    Ok(from_meta
        .filter(|value| !value.trim().is_empty())
        .unwrap_or_else(|| {
            path.file_stem()
                .and_then(|stem| stem.to_str())
                .unwrap_or("-")
                .to_owned()
        }))
}

/// 字节拷贝前后必须确认源文件不是 WAL：-wal 里未合并的写会丢。
fn ensure_byte_copyable(path: &Path) -> Result<(), String> {
    let mut file = fs::File::open(path).map_err(|error| format!("打开 {} 失败：{error}", path.display()))?;
    let mut header = [0u8; 20];
    if file.read_exact(&mut header).is_err() {
        return Ok(());
    }
    if header[18] == 2 && header[19] == 2 {
        return Err(format!(
            "{} 是 WAL 模式快照，字节拷贝可能丢未合并的写；请先 checkpoint，或改用非 WAL 快照",
            path.display()
        ));
    }
    Ok(())
}

/// 一行 delta 记录。
struct DeltaRow {
    path: String,
    op: i64,
    size: i64,
    mtime: i64,
    is_dir: i64,
}

/// 扫描出来的一行快照条目。
struct ScanRow {
    path: String,
    size: i64,
    mtime: i64,
    is_dir: i64,
}

fn read_scan_row(rows: &mut rusqlite::Rows<'_>) -> Result<Option<ScanRow>, String> {
    let Some(row) = rows.next().map_err(|error| format!("读取 entries 失败：{error}"))? else {
        return Ok(None);
    };
    Ok(Some(ScanRow {
        path: row.get(0).map_err(|error| format!("读取 path 失败：{error}"))?,
        size: row.get(1).map_err(|error| format!("读取 size 失败：{error}"))?,
        mtime: row.get(2).map_err(|error| format!("读取 mtime 失败：{error}"))?,
        is_dir: row.get(3).map_err(|error| format!("读取 is_dir 失败：{error}"))?,
    }))
}

/// 归并依赖「两条流都按 path 升序」，这里逐行校验；一旦乱序立即失败，
/// 绝不把错误的 delta 写出去。
fn ensure_ascending(previous: &mut String, path: &str, source: &Path) -> Result<(), String> {
    if path <= previous.as_str() {
        return Err(format!(
            "{} 的 entries 未按 path 升序返回（{:?} 之后是 {:?}）；顺序归并要求表本身是 path 主键，请设 DISKINTEL_COMPACT_STRATEGY=steps 用 SQL 方案",
            source.display(),
            previous,
            path
        ));
    }
    previous.clear();
    previous.push_str(path);
    Ok(())
}

/// 默认策略：两条顺序扫描流线性归并。
fn merge_stream(
    base: &Path,
    target: &Path,
    on_row: &mut dyn FnMut(DeltaRow) -> Result<(), String>,
) -> Result<(), String> {
    let base_conn = open_read_only(base)?;
    let target_conn = open_read_only(target)?;
    let mut base_stmt = base_conn
        .prepare(SCAN_SQL)
        .map_err(|error| format!("扫描 {} 失败：{error}", base.display()))?;
    let mut target_stmt = target_conn
        .prepare(SCAN_SQL)
        .map_err(|error| format!("扫描 {} 失败：{error}", target.display()))?;
    let mut base_rows = base_stmt
        .query([])
        .map_err(|error| format!("扫描 {} 失败：{error}", base.display()))?;
    let mut target_rows = target_stmt
        .query([])
        .map_err(|error| format!("扫描 {} 失败：{error}", target.display()))?;

    let mut next_base = read_scan_row(&mut base_rows)?;
    let mut next_target = read_scan_row(&mut target_rows)?;
    let mut previous_base = String::new();
    let mut previous_target = String::new();

    loop {
        let ordering = match (&next_base, &next_target) {
            (Some(left), Some(right)) => Some(left.path.cmp(&right.path)),
            (Some(_), None) => Some(Ordering::Less),
            (None, Some(_)) => Some(Ordering::Greater),
            (None, None) => None,
        };
        let Some(ordering) = ordering else { break };
        match ordering {
            Ordering::Less => {
                let left = next_base.take().expect("左侧一定有值");
                ensure_ascending(&mut previous_base, &left.path, base)?;
                on_row(DeltaRow {
                    path: left.path,
                    op: OP_REMOVED,
                    size: left.size,
                    mtime: left.mtime,
                    is_dir: left.is_dir,
                })?;
                next_base = read_scan_row(&mut base_rows)?;
            }
            Ordering::Greater => {
                let right = next_target.take().expect("右侧一定有值");
                ensure_ascending(&mut previous_target, &right.path, target)?;
                on_row(DeltaRow {
                    path: right.path,
                    op: OP_ADDED,
                    size: right.size,
                    mtime: right.mtime,
                    is_dir: right.is_dir,
                })?;
                next_target = read_scan_row(&mut target_rows)?;
            }
            Ordering::Equal => {
                let left = next_base.take().expect("左侧一定有值");
                let right = next_target.take().expect("右侧一定有值");
                ensure_ascending(&mut previous_base, &left.path, base)?;
                ensure_ascending(&mut previous_target, &right.path, target)?;
                if left.size != right.size || left.mtime != right.mtime || left.is_dir != right.is_dir {
                    on_row(DeltaRow {
                        path: right.path,
                        op: OP_CHANGED,
                        size: right.size,
                        mtime: right.mtime,
                        is_dir: right.is_dir,
                    })?;
                }
                next_base = read_scan_row(&mut base_rows)?;
                next_target = read_scan_row(&mut target_rows)?;
            }
        }
    }
    Ok(())
}

/// 需求原文方案：ATTACH + FULL OUTER JOIN 一次算出三类差异。
fn foj_stream(
    base: &Path,
    target: &Path,
    on_row: &mut dyn FnMut(DeltaRow) -> Result<(), String>,
) -> Result<(), String> {
    let conn = open_read_only(target)?;
    attach_read_only(&conn, base, "base")?;
    let mut stmt = conn
        .prepare(FOJ_SQL)
        .map_err(|error| format!("准备 FULL OUTER JOIN 失败：{error}"))?;
    let mut rows = stmt
        .query([])
        .map_err(|error| format!("执行 FULL OUTER JOIN 失败：{error}"))?;
    while let Some(row) = rows
        .next()
        .map_err(|error| format!("读取 FULL OUTER JOIN 结果失败：{error}"))?
    {
        on_row(DeltaRow {
            op: row.get(0).map_err(|error| format!("读取 op 失败：{error}"))?,
            path: row.get(1).map_err(|error| format!("读取 path 失败：{error}"))?,
            size: row.get(2).map_err(|error| format!("读取 size 失败：{error}"))?,
            mtime: row.get(3).map_err(|error| format!("读取 mtime 失败：{error}"))?,
            is_dir: row.get(4).map_err(|error| format!("读取 is_dir 失败：{error}"))?,
        })?;
    }
    Ok(())
}

/// 三步 SQL：FULL OUTER JOIN 超时时的退路（也用于 v1/v2 的 rowid 表）。
fn steps_stream(
    base: &Path,
    target: &Path,
    on_row: &mut dyn FnMut(DeltaRow) -> Result<(), String>,
) -> Result<(), String> {
    let conn = open_read_only(target)?;
    attach_read_only(&conn, base, "base")?;
    for (name, sql) in [
        ("新增", STEP_ADDED_SQL),
        ("消失", STEP_REMOVED_SQL),
        ("变化", STEP_CHANGED_SQL),
    ] {
        let mut stmt = conn
            .prepare(sql)
            .map_err(|error| format!("准备{name}查询失败：{error}"))?;
        let mut rows = stmt
            .query([])
            .map_err(|error| format!("执行{name}查询失败：{error}"))?;
        while let Some(row) = rows
            .next()
            .map_err(|error| format!("读取{name}结果失败：{error}"))?
        {
            on_row(DeltaRow {
                op: row.get(0).map_err(|error| format!("读取 op 失败：{error}"))?,
                path: row.get(1).map_err(|error| format!("读取 path 失败：{error}"))?,
                size: row.get(2).map_err(|error| format!("读取 size 失败：{error}"))?,
                mtime: row.get(3).map_err(|error| format!("读取 mtime 失败：{error}"))?,
                is_dir: row.get(4).map_err(|error| format!("读取 is_dir 失败：{error}"))?,
            })?;
        }
    }
    Ok(())
}

/// 把流出来的 delta 行按批写进目标库。
struct DeltaWriter<'a> {
    conn: &'a Connection,
    buffer: Vec<DeltaRow>,
    added: u64,
    removed: u64,
    changed: u64,
    batches: u64,
}

impl<'a> DeltaWriter<'a> {
    fn new(conn: &'a Connection) -> Self {
        Self {
            conn,
            buffer: Vec::with_capacity(BATCH_ROWS),
            added: 0,
            removed: 0,
            changed: 0,
            batches: 0,
        }
    }

    fn push(&mut self, row: DeltaRow) -> Result<(), String> {
        match row.op {
            OP_ADDED => self.added += 1,
            OP_REMOVED => self.removed += 1,
            OP_CHANGED => self.changed += 1,
            _ => {}
        }
        self.buffer.push(row);
        if self.buffer.len() >= BATCH_ROWS {
            self.flush()?;
        }
        Ok(())
    }

    fn finish(&mut self) -> Result<(), String> {
        self.flush()
    }

    fn flush(&mut self) -> Result<(), String> {
        if self.buffer.is_empty() {
            return Ok(());
        }
        self.conn
            .execute_batch("BEGIN")
            .map_err(|error| format!("开启事务失败：{error}"))?;
        let written = (|| -> Result<(), String> {
            let mut stmt = self
                .conn
                .prepare_cached(INSERT_ROW_SQL)
                .map_err(|error| format!("准备写入 delta 失败：{error}"))?;
            for row in self.buffer.iter() {
                stmt.execute(params![&row.path, row.op, row.size, row.mtime, row.is_dir])
                    .map_err(|error| format!("写入 delta 失败（{}）：{error}", row.path))?;
            }
            Ok(())
        })();
        match written {
            Ok(()) => {
                self.conn
                    .execute_batch("COMMIT")
                    .map_err(|error| format!("提交 delta 批次失败：{error}"))?;
                self.buffer.clear();
                self.batches += 1;
                Ok(())
            }
            Err(error) => {
                let _ = self.conn.execute_batch("ROLLBACK");
                Err(error)
            }
        }
    }
}

/// 一份 delta 的 meta 里物化需要的字段（先验链，再动手）。
struct DeltaMeta {
    base_id: String,
    target_id: String,
}

fn read_delta_meta(path: &Path) -> Result<DeltaMeta, String> {
    let conn = open_read_only(path)?;
    let get = |key: &str| -> Option<String> {
        conn.query_row("SELECT value FROM meta WHERE key = ?1", [key], |row| row.get(0))
            .ok()
    };
    let format_version = get("format_version")
        .ok_or_else(|| format!("{} 不是 delta 文件（meta 缺少 format_version）", path.display()))?;
    if format_version != DELTA_FORMAT_VERSION {
        return Err(format!(
            "{} 的 format_version 是 {format_version}，本程序只认 {DELTA_FORMAT_VERSION}",
            path.display()
        ));
    }
    let base_id = get("base_id").unwrap_or_default();
    let target_id = get("target_id").unwrap_or_default();
    if base_id.is_empty() || target_id.is_empty() {
        return Err(format!("{} 的 meta 缺少 base_id / target_id", path.display()));
    }
    Ok(DeltaMeta { base_id, target_id })
}

#[derive(Default)]
struct ApplyCounts {
    deleted: u64,
    updated: u64,
    inserted: u64,
}

/// 把一份 delta 应用到副本上：先删「消失」，再改「变化」，最后插「新增」。
fn apply_delta(conn: &Connection, index: usize, delta: &Path) -> Result<ApplyCounts, String> {
    let alias = format!("d{index}");
    attach_read_only(conn, delta, &alias)?;

    let deleted = conn
        .execute(
            &format!("DELETE FROM entries WHERE path IN (SELECT path FROM {alias}.delta WHERE op = {OP_REMOVED})"),
            [],
        )
        .map_err(|error| format!("应用 {} 的删除失败：{error}", delta.display()))? as u64;

    let updated = conn
        .execute(
            &format!(
                "UPDATE entries SET size = c.size, mtime = c.mtime, is_dir = c.is_dir \
                   FROM (SELECT path, size, mtime, is_dir FROM {alias}.delta WHERE op = {OP_CHANGED}) AS c \
                  WHERE entries.path = c.path"
            ),
            [],
        )
        .map_err(|error| format!("应用 {} 的更新失败：{error}", delta.display()))? as u64;

    let has_ext = has_column(conn, "entries", "ext")?;
    // attributes 不在 delta 里：按 v3 建库器的约定补（五个快照实测都是 目录=16、文件=0）。
    let insert_sql = if has_ext {
        "INSERT OR REPLACE INTO entries(path, parent_path, size, mtime, is_dir, attributes, ext) \
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)"
    } else {
        "INSERT OR REPLACE INTO entries(path, parent_path, size, mtime, is_dir, attributes) \
         VALUES (?1, ?2, ?3, ?4, ?5, ?6)"
    };
    let inserted = {
        let count;
        {
            let mut stmt = conn
                .prepare(&format!(
                    "SELECT path, size, mtime, is_dir FROM {alias}.delta WHERE op = {OP_ADDED}"
                ))
                .map_err(|error| format!("读取 {} 的新增失败：{error}", delta.display()))?;
            let mut rows = stmt
                .query([])
                .map_err(|error| format!("读取 {} 的新增失败：{error}", delta.display()))?;
            conn.execute_batch("BEGIN")
                .map_err(|error| format!("开启新增事务失败：{error}"))?;
            let written = (|| -> Result<u64, String> {
                let mut insert = conn
                    .prepare(insert_sql)
                    .map_err(|error| format!("准备写入 entries 失败：{error}"))?;
                let mut count = 0u64;
                while let Some(row) = rows
                    .next()
                    .map_err(|error| format!("读取 {} 的新增失败：{error}", delta.display()))?
                {
                    let path: String = row.get(0).map_err(|error| format!("读取 path 失败：{error}"))?;
                    let size: i64 = row.get(1).map_err(|error| format!("读取 size 失败：{error}"))?;
                    let mtime: i64 = row.get(2).map_err(|error| format!("读取 mtime 失败：{error}"))?;
                    let is_dir: i64 = row.get(3).map_err(|error| format!("读取 is_dir 失败：{error}"))?;
                    let parent = parent_of(&path);
                    let attributes: i64 = if is_dir != 0 { 16 } else { 0 };
                    if has_ext {
                        insert
                            .execute(params![&path, &parent, size, mtime, is_dir, attributes, ext_of(&path)])
                            .map_err(|error| format!("写入 entries 失败（{path}）：{error}"))?;
                    } else {
                        insert
                            .execute(params![&path, &parent, size, mtime, is_dir, attributes])
                            .map_err(|error| format!("写入 entries 失败（{path}）：{error}"))?;
                    }
                    count += 1;
                }
                Ok(count)
            })();
            match written {
                Ok(done) => {
                    conn.execute_batch("COMMIT")
                        .map_err(|error| format!("提交新增失败：{error}"))?;
                    count = done;
                }
                Err(error) => {
                    let _ = conn.execute_batch("ROLLBACK");
                    return Err(error);
                }
            }
        }
        count
    };

    conn.execute_batch(&format!("DETACH DATABASE {alias}"))
        .map_err(|error| format!("DETACH {} 失败：{error}", delta.display()))?;
    Ok(ApplyCounts {
        deleted,
        updated,
        inserted,
    })
}

/// 用物化后的 entries 整体重算 folder_sizes（逐目录子树和 + 文件数）。
///
/// 递归 CTE 把每个目录的「直接文件量」沿父链一路上推，最后按祖先分组求和；
/// 与 v3 建库器的口径在本机 635,426 行上验证过 0 处差异。
fn rebuild_folder_sizes(conn: &Connection) -> Result<u64, String> {
    let has_parent = has_column(conn, "folder_sizes", "parent_path")?;
    conn.execute_batch("DROP TABLE IF EXISTS temp.tot;")
        .map_err(|error| format!("清理临时表失败：{error}"))?;
    conn.execute_batch(
        "CREATE TEMP TABLE tot(up TEXT PRIMARY KEY, size INTEGER NOT NULL, files INTEGER NOT NULL) WITHOUT ROWID;",
    )
    .map_err(|error| format!("创建临时表失败：{error}"))?;
    conn.execute_batch(
        "INSERT INTO tot(up, size, files)
         WITH RECURSIVE agg(up, size, files) AS (
             SELECT parent_path, SUM(size), COUNT(*)
               FROM entries WHERE is_dir = 0 GROUP BY parent_path
             UNION ALL
             SELECT e.parent_path, agg.size, agg.files
               FROM agg JOIN entries AS e ON e.path = agg.up AND e.is_dir = 1
              WHERE e.parent_path <> ''
         )
         SELECT up, SUM(size), SUM(files) FROM agg GROUP BY up;",
    )
    .map_err(|error| format!("聚合目录大小失败：{error}"))?;
    conn.execute_batch("DELETE FROM folder_sizes;")
        .map_err(|error| format!("清空 folder_sizes 失败：{error}"))?;
    if has_parent {
        conn.execute_batch(
            "INSERT INTO folder_sizes(path, parent_path, size, file_count)
             SELECT e.path, e.parent_path, IFNULL(t.size, 0), IFNULL(t.files, 0)
               FROM entries AS e LEFT JOIN tot AS t ON t.up = e.path
              WHERE e.is_dir = 1;",
        )
        .map_err(|error| format!("写入 folder_sizes 失败：{error}"))?;
    } else {
        conn.execute_batch(
            "INSERT INTO folder_sizes(path, size, file_count)
             SELECT e.path, IFNULL(t.size, 0), IFNULL(t.files, 0)
               FROM entries AS e LEFT JOIN tot AS t ON t.up = e.path
              WHERE e.is_dir = 1;",
        )
        .map_err(|error| format!("写入 folder_sizes 失败：{error}"))?;
    }
    let rows = query_int(conn, "SELECT COUNT(*) FROM folder_sizes")?;
    let _ = conn.execute_batch("DROP TABLE IF EXISTS temp.tot;");
    Ok(rows)
}

/// 把 meta 的计数改成物化后的真实值，并记录 delta 链出处。
fn refresh_meta(
    conn: &Connection,
    base_id: &str,
    target_id: &str,
    delta_ids: &[String],
) -> Result<(u64, u64, u64, i64), String> {
    let (rows, file_count, dir_count, total_size) = conn
        .query_row(
            "SELECT COUNT(*),
                    IFNULL(SUM(CASE WHEN is_dir = 0 THEN 1 ELSE 0 END), 0),
                    IFNULL(SUM(CASE WHEN is_dir = 1 THEN 1 ELSE 0 END), 0),
                    IFNULL(SUM(CASE WHEN is_dir = 0 THEN size ELSE 0 END), 0)
               FROM entries",
            [],
            |row| {
                Ok((
                    row.get::<_, i64>(0)?,
                    row.get::<_, i64>(1)?,
                    row.get::<_, i64>(2)?,
                    row.get::<_, i64>(3)?,
                ))
            },
        )
        .map_err(|error| format!("统计物化结果失败：{error}"))?;
    let rows = rows.max(0) as u64;
    let file_count = file_count.max(0) as u64;
    let dir_count = dir_count.max(0) as u64;
    write_meta_pair(conn, "file_count", &file_count.to_string())?;
    write_meta_pair(conn, "dir_count", &dir_count.to_string())?;
    write_meta_pair(conn, "total_size", &total_size.to_string())?;
    write_meta_pair(conn, "snapshot_id", target_id)?;
    write_meta_pair(conn, "created_at", &unix_now().to_string())?;
    write_meta_pair(conn, "materialized_from_base", base_id)?;
    write_meta_pair(conn, "materialized_deltas", &delta_ids.join(","))?;
    // 只有结构确实是 v3（entries.ext + folder_sizes.parent_path）才敢写 format_version=3。
    let is_v3 = has_column(conn, "entries", "ext")? && has_column(conn, "folder_sizes", "parent_path")?;
    let version = if is_v3 { 3 } else { read_int_meta(conn, "format_version") };
    write_meta_pair(conn, "format_version", &version.to_string())?;
    Ok((rows, file_count, dir_count, total_size))
}

/// 文件夹聚合对比：返回（左表行数、右表行数、差异处数）。
fn compare_folder_sizes(left: &Path, right: &Path) -> Result<(u64, u64, u64), String> {
    let conn = open_read_only(left)?;
    attach_read_only(&conn, right, "right")?;
    let left_rows = query_int(&conn, "SELECT COUNT(*) FROM main.folder_sizes")?;
    let right_rows = query_int(&conn, "SELECT COUNT(*) FROM right.folder_sizes")?;
    let mismatches = query_int(
        &conn,
        "SELECT (SELECT COUNT(*) FROM main.folder_sizes AS f JOIN right.folder_sizes AS r ON r.path = f.path
                  WHERE f.size IS NOT r.size OR f.file_count IS NOT r.file_count)
              + (SELECT COUNT(*) FROM right.folder_sizes AS r
                  WHERE NOT EXISTS (SELECT 1 FROM main.folder_sizes AS f WHERE f.path = r.path))",
    )?;
    Ok((left_rows, right_rows, mismatches))
}

/// 路径的父目录（保留卷根末尾的反斜杠形态）。
///
/// 与 db::parent_dir 的差别：\\?\Volume{…}\ 这种卷根在快照里带尾反斜杠，
/// 这里补上，物化出来的 parent_path 才能与 v3 建库结果一致。
fn parent_of(path: &str) -> String {
    let trimmed = path.trim_end_matches(['\\', '/']);
    if trimmed.is_empty() {
        return String::new();
    }
    match trimmed.rfind(['\\', '/']) {
        None => String::new(),
        Some(0) => trimmed[..=0].to_owned(),
        Some(index) => {
            let parent = &trimmed[..index];
            if parent.is_empty() {
                String::new()
            } else if parent.ends_with(':') || is_volume_root(parent) {
                format!("{parent}\\")
            } else {
                parent.to_owned()
            }
        }
    }
}

fn is_volume_root(path: &str) -> bool {
    let lower = path.to_ascii_lowercase();
    lower.starts_with("\\\\?\\volume{") && lower.ends_with('}')
}

/// 扩展名（小写、不带点），与 v3 建库器口径一致：.gitignore → gitignore，
/// plugin.tar.gz → gz，无扩展名 → 空串。
fn ext_of(path: &str) -> String {
    let name = path
        .trim_end_matches(['\\', '/'])
        .rsplit(['\\', '/'])
        .next()
        .unwrap_or("");
    match name.rfind('.') {
        Some(index) => name[index + 1..].to_ascii_lowercase(),
        None => String::new(),
    }
}
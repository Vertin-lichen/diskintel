//! 快照轮转与收尾：清单、移入待删除、保留策略执行、v4 转换。
//!
//! 与 src/snapshot_policy.rs 的分工：那边只回答「该不该建、留几份、待删除放哪」（纯逻辑，可单测），
//! 这里负责落盘与外部命令。三条硬约束：
//! - 任何删除都走 store::trash（移入待删除 + MANIFEST），绝不永久删除；
//! - 移走 .sqlite 时同步 index.json（建库器缓存）与 notes.json（备注 sidecar），不留悬空条目；
//! - v4 转换在**独立进程**里跑（release 下 panic=abort，转换崩了不能带走界面进程），
//!   失败时 v3 原文件原样保留，成功的产物才替换 v3。
//!
//! 说明：scan / newest_created / should_build_on_start / convert_to_v4 / after_create 是给外壳
//! （app.rs，w6/lead 接线）的入口，接线落地前只有单测调用；先关掉 dead_code 噪声，接线后删掉下面这行。
#![allow(dead_code)]

use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use rusqlite::{Connection, OpenFlags};

use super::{notes, trash};
use crate::db;
use crate::snapshot_policy::{self, Policy};

/// 策略自动清理写进 MANIFEST 的操作者。
pub const OPERATOR_POLICY: &str = "快照策略";
/// 索引文件名（建库器写的缓存，格式是一层扁平 JSON）。
pub const INDEX_FILE: &str = "index.json";
/// v4 转换子进程的超时：1.5 GB 实测约 20 s，这里留足余量。
const CONVERT_TIMEOUT: Duration = Duration::from_secs(15 * 60);

/// 一份快照的清单信息（列表 / 保留策略共用）。
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SnapshotInfo {
    /// 文件名去掉 .sqlite（就是界面上显示的快照名）。
    pub name: String,
    pub path: PathBuf,
    pub created_unix: i64,
    pub bytes: u64,
    /// meta.format_version；读不到按 0 算（不影响清理决策）。
    pub format_version: i64,
}

impl SnapshotInfo {
    pub fn is_v4(&self) -> bool {
        self.format_version >= 4
    }

    /// 磁盘上的文件名（带 .sqlite），index.json 的键用它。
    pub fn file_name(&self) -> String {
        format!("{}.sqlite", self.name)
    }
}

/// 扫一个快照目录（任意目录，自定义快照目录同样适用），按创建时间倒序（最新在前）。
pub fn scan(snapshot_dir: &Path) -> Vec<SnapshotInfo> {
    let mut out = Vec::new();
    let Ok(entries) = std::fs::read_dir(snapshot_dir) else {
        return out;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.extension().and_then(|ext| ext.to_str()) != Some("sqlite") {
            continue;
        }
        let name = path
            .file_stem()
            .and_then(|stem| stem.to_str())
            .unwrap_or("-")
            .to_owned();
        let bytes = entry.metadata().map(|meta| meta.len()).unwrap_or(0);
        let created_unix = db::snapshot_created_unix(&path);
        let format_version = db::peek_format_version(&path).unwrap_or(0);
        out.push(SnapshotInfo {
            name,
            path,
            created_unix,
            bytes,
            format_version,
        });
    }
    out.sort_by(|left, right| {
        right
            .created_unix
            .cmp(&left.created_unix)
            .then_with(|| right.name.cmp(&left.name))
    });
    out
}

/// 最新一份快照的创建时间；目录为空返回 None（= 首次安装 / 空目录）。
///
/// 只读目录项与 mtime，不打开 sqlite，启动路径上是常数级开销。
pub fn newest_created(snapshot_dir: &Path) -> Option<i64> {
    let mut newest: Option<i64> = None;
    let Ok(entries) = std::fs::read_dir(snapshot_dir) else {
        return None;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.extension().and_then(|ext| ext.to_str()) != Some("sqlite") {
            continue;
        }
        let created = db::snapshot_created_unix(&path);
        if created > 0 && newest.is_none_or(|best| created > best) {
            newest = Some(created);
        }
    }
    newest
}

/// 启动时是否该建快照（把「最新一份的创建时间」读出来交给纯策略判断）。
pub fn should_build_on_start(policy: &Policy, snapshot_dir: &Path, now_unix: i64) -> bool {
    snapshot_policy::should_build_on_start(policy, newest_created(snapshot_dir), now_unix)
}

/// 把一份快照移入待删除（操作者固定为「快照策略」）。
pub fn move_to_trash(path: &Path, data_root: &Path, reason: &str) -> Result<PathBuf, String> {
    move_to_trash_as(path, data_root, reason, OPERATOR_POLICY)
}

/// 把一份快照移入待删除，并同步 index.json / notes.json。
///
/// 待删除落点由 snapshot_policy::trash_dir 决定：默认布局进 <data_root>\待删除，
/// 自定义快照目录就近进 <snapshot_dir>\待删除（同卷 rename，不跨盘复制 GB 级文件）。
pub fn move_to_trash_as(
    path: &Path,
    data_root: &Path,
    reason: &str,
    operator: &str,
) -> Result<PathBuf, String> {
    let snapshot_dir = path
        .parent()
        .ok_or_else(|| format!("{} 没有父目录，无法判断快照目录", path.display()))?;
    let trash_dir = snapshot_policy::trash_dir(snapshot_dir, data_root);
    let moved = if trash_dir == data_root.join(trash::TRASH_DIR) {
        // 默认布局：直接走 store::trash 的便捷入口（<data_root>\待删除）
        trash::move_to_trash(path, data_root, reason, operator)?
    } else {
        // 自定义快照目录：待删除就近放，显式给目录
        trash::move_to_trash_in(&trash_dir, path, reason, operator)?
    };

    // 同步 sidecar：失败只记日志 —— 文件已经安全移入待删除，不能因为缓存文件回滚删除动作。
    if let Some(file_name) = path.file_name().and_then(|name| name.to_str()) {
        if let Err(error) = remove_index_entry(snapshot_dir, file_name) {
            eprintln!("[快照] index.json 同步失败（移动已完成）：{error}");
        }
    }
    if let Some(name) = path.file_stem().and_then(|stem| stem.to_str()) {
        if let Err(error) = remove_note(snapshot_dir, name) {
            eprintln!("[快照] notes.json 同步失败（移动已完成）：{error}");
        }
    }
    Ok(moved)
}

/// 从 index.json 里删掉一份快照的条目；文件不存在直接成功。
pub fn remove_index_entry(snapshot_dir: &Path, file_name: &str) -> Result<(), String> {
    let path = snapshot_dir.join(INDEX_FILE);
    let text = match std::fs::read_to_string(&path) {
        Ok(text) => text,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(error) => return Err(format!("读 {} 失败：{error}", path.display())),
    };
    // 结构保险：花括号配平且确实有 entries 字段，才敢在文本上做手术
    let opens = text.bytes().filter(|byte| *byte == b'{').count();
    let closes = text.bytes().filter(|byte| *byte == b'}').count();
    if opens == 0 || opens != closes || !text.contains("\"entries\"") {
        return Err(format!(
            "{} 结构无法识别，未改动（保留原文件）",
            path.display()
        ));
    }
    // 本来就没有这一条：什么都不用做
    if !text.contains(&format!("\"{file_name}\"")) {
        return Ok(());
    }
    let Some(updated) = remove_json_entry(&text, file_name) else {
        return Err(format!(
            "{} 结构无法识别，未改动（保留原文件）",
            path.display()
        ));
    };
    if updated == text {
        return Ok(());
    }
    write_atomic(&path, &updated)
}

/// 从 notes.json 里删掉一份快照的备注。
pub fn remove_note(snapshot_dir: &Path, name: &str) -> Result<(), String> {
    let mut all = notes::load_in(snapshot_dir);
    if all.remove(name).is_none() {
        return Ok(());
    }
    notes::save_in(snapshot_dir, &all)
}

/// 保留策略执行结果。
#[derive(Clone, Debug, Default)]
pub struct PruneReport {
    /// 保留下来（不动）的快照名。
    pub kept: Vec<String>,
    /// 已移入待删除的快照名与落点。
    pub moved: Vec<(String, PathBuf)>,
    /// 移动失败（文件还在原地，下次再试）。
    pub failed: Vec<(String, String)>,
}

impl PruneReport {
    pub fn did_change(&self) -> bool {
        !self.moved.is_empty() || !self.failed.is_empty()
    }

    /// 一行给界面 / 日志用的摘要。
    pub fn summary(&self) -> String {
        if self.moved.is_empty() && self.failed.is_empty() {
            return format!("快照 {} 份，未到保留上限，无需清理", self.kept.len());
        }
        let mut text = format!(
            "保留 {} 份，已移入待删除 {} 份",
            self.kept.len(),
            self.moved.len()
        );
        if !self.failed.is_empty() {
            text.push_str(&format!("，{} 份清理失败", self.failed.len()));
        }
        text
    }
}

/// 按保留份数执行清理：超过上限的旧快照移入待删除（不永久删除）。
pub fn prune(snapshot_dir: &Path, data_root: &Path, keep_count: usize) -> PruneReport {
    let infos = scan(snapshot_dir);
    let rows: Vec<(String, i64, u64)> = infos
        .iter()
        .map(|info| (info.name.clone(), info.created_unix, info.bytes))
        .collect();
    let plan = snapshot_policy::prune_plan(&rows, keep_count);
    let mut report = PruneReport::default();
    for info in &infos {
        if !plan.iter().any(|name| name == &info.name) {
            report.kept.push(info.name.clone());
        }
    }
    for name in plan {
        let Some(info) = infos.iter().find(|info| info.name == name) else {
            continue;
        };
        match move_to_trash(&info.path, data_root, "保留策略：超出保留份数的旧快照") {
            Ok(target) => {
                eprintln!("[快照] 保留策略移入待删除：{} → {}", info.name, target.display());
                report.moved.push((name, target));
            }
            Err(error) => {
                eprintln!("[快照] 保留策略清理失败 {}：{error}", name);
                report.failed.push((name, error));
            }
        }
    }
    report
}

/// v4 转换结果。
#[derive(Clone, Debug)]
pub struct V4Report {
    pub source: PathBuf,
    pub target: PathBuf,
    /// v3 原文件移入待删除后的落点；移动失败时为 None（v3 还在原地，数据不丢）。
    pub trashed_source: Option<PathBuf>,
    pub message: String,
}

/// 校验一份文件确实是可用的 v4 快照（meta 版本 + names 表非空）。
pub fn verify_v4_file(path: &Path) -> Result<(), String> {
    let meta = std::fs::metadata(path).map_err(|error| format!("读不到 {}：{error}", path.display()))?;
    if meta.len() == 0 {
        return Err(format!("{} 是空文件", path.display()));
    }
    let conn = Connection::open_with_flags(path, OpenFlags::SQLITE_OPEN_READ_ONLY)
        .map_err(|error| format!("打开 {} 失败：{error}", path.display()))?;
    let version: i64 = conn
        .query_row(
            "SELECT CAST(value AS INTEGER) FROM meta WHERE key = 'format_version'",
            [],
            |row| row.get(0),
        )
        .map_err(|error| format!("{} 读 meta.format_version 失败：{error}", path.display()))?;
    if version != 4 {
        return Err(format!(
            "{} 的 meta.format_version={version}，不是 v4",
            path.display()
        ));
    }
    let names: i64 = conn
        .query_row("SELECT COUNT(*) FROM names", [], |row| row.get(0))
        .map_err(|error| format!("{} 缺 names 表：{error}", path.display()))?;
    if names <= 0 {
        return Err(format!("{} 的 names 表是空的", path.display()));
    }
    Ok(())
}

/// 手动把一个 v3 快照转成 v4，并用 v4 替换原文件（原 v3 移入待删除）。
///
/// 命名：先转成同目录的 <名>.v4.sqlite，校验通过、且 v3 已移入待删除后，
/// 再把 v4 改回 <名>.sqlite —— 快照名（时间戳）保持不变，列表/对比/命令行引用都不用改。
/// 任何一步失败都保留 v3 原文件；中间产物由转换器自己清理。
pub fn convert_to_v4(v3_path: &Path, data_root: &Path) -> Result<V4Report, String> {
    let source = v3_path.to_path_buf();
    let data_root = data_root.to_path_buf();
    convert_v4_impl(&source, &data_root, |v3, target| {
        run_self_convert(v3, target)
    })
}

fn convert_v4_impl(
    v3_path: &Path,
    data_root: &Path,
    run: impl FnOnce(&Path, &Path) -> Result<String, String>,
) -> Result<V4Report, String> {
    if is_already_v4(v3_path) {
        return Err(format!("{} 已经是 v4 快照，无需转换", v3_path.display()));
    }
    let snapshot_dir = v3_path
        .parent()
        .ok_or_else(|| format!("{} 没有父目录", v3_path.display()))?;
    let stem = v3_path
        .file_stem()
        .and_then(|stem| stem.to_str())
        .ok_or_else(|| format!("{} 文件名不是合法 UTF-8", v3_path.display()))?;
    let v4_target = snapshot_dir.join(format!("{stem}.v4.sqlite"));

    let log = run(v3_path, &v4_target)?;
    verify_v4_file(&v4_target).map_err(|error| {
        format!(
            "v4 产物校验失败：{error}（v3 原文件保留：{}；转换器输出：{}）",
            v3_path.display(),
            shorten(&log)
        )
    })?;

    // v3 移入待删除（失败则保留 v3，两个文件都在，不丢数据）
    let trashed = match move_to_trash(
        v3_path,
        data_root,
        "转 v4 成功，v3 原文件移入待删除",
    ) {
        Ok(target) => Some(target),
        Err(error) => {
            eprintln!("[快照] v3 移入待删除失败（保留原文件）：{error}");
            None
        }
    };

    // 把 v4 改回快照原名，界面上快照名不变（v3 已被移走，原路径空出来了）
    let final_path = snapshot_dir.join(format!("{stem}.sqlite"));
    let (target, message) = if trashed.is_some() {
        match std::fs::rename(&v4_target, &final_path) {
            Ok(()) => (
                final_path,
                format!("v4 转换完成，快照名保持 {stem}"),
            ),
            Err(error) => (
                v4_target,
                format!("v4 转换完成但改名失败（保留 .v4 后缀）：{error}"),
            ),
        }
    } else {
        (v4_target, "v4 转换完成（v3 仍在，可手动确认后清理）".to_owned())
    };
    Ok(V4Report {
        source: v3_path.to_path_buf(),
        target,
        trashed_source: trashed,
        message,
    })
}

/// 已经是 v4（meta 版本 4）就不要再转。
fn is_already_v4(path: &Path) -> bool {
    db::peek_format_version(path) == Some(4)
}

/// 用当前可执行文件的 --convert-v4 在独立进程里转换，返回转换器的输出摘要。
fn run_self_convert(v3: &Path, target: &Path) -> Result<String, String> {
    let exe = std::env::current_exe()
        .map_err(|error| format!("拿不到当前可执行文件路径：{error}"))?;
    let mut child = Command::new(&exe)
        .arg("--convert-v4")
        .arg(v3)
        .arg(target)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|error| format!("启动转换进程 {} 失败：{error}", exe.display()))?;
    let out_handle = child.stdout.take().map(|pipe| std::thread::spawn(move || read_pipe(pipe)));
    let err_handle = child.stderr.take().map(|pipe| std::thread::spawn(move || read_pipe(pipe)));

    let deadline = Instant::now() + CONVERT_TIMEOUT;
    let status = loop {
        match child.try_wait() {
            Ok(Some(status)) => break status,
            Ok(None) => {
                if Instant::now() >= deadline {
                    let _ = child.kill();
                    let _ = child.wait();
                    return Err(format!(
                        "转换超时（超过 {} 分钟），已终止子进程",
                        CONVERT_TIMEOUT.as_secs() / 60
                    ));
                }
                std::thread::sleep(Duration::from_millis(200));
            }
            Err(error) => return Err(format!("等待转换进程失败：{error}")),
        }
    };
    let stdout = out_handle
        .and_then(|handle| handle.join().ok())
        .unwrap_or_default();
    let stderr = err_handle
        .and_then(|handle| handle.join().ok())
        .unwrap_or_default();
    let log = format!("{stdout}{stderr}");
    if !status.success() {
        return Err(format!(
            "转换进程退出码 {:?}：{}",
            status.code(),
            shorten(&log)
        ));
    }
    Ok(log)
}

fn read_pipe(mut pipe: impl std::io::Read) -> String {
    let mut text = String::new();
    let _ = pipe.read_to_string(&mut text);
    text
}

fn shorten(text: &str) -> String {
    let trimmed = text.trim();
    let line = trimmed
        .lines()
        .rev()
        .find(|line| line.contains("失败") || line.contains("error"))
        .unwrap_or_else(|| trimmed.lines().last().unwrap_or(""));
    line.chars().take(240).collect()
}

/// 建库之后的收尾报告（外壳拿它拼提示条）。
#[derive(Clone, Debug, Default)]
pub struct AfterCreateReport {
    /// 可选 v4 转换的结果（策略关闭时为 None）。
    pub v4: Option<Result<V4Report, String>>,
    pub prune: PruneReport,
}

impl AfterCreateReport {
    pub fn summary(&self) -> String {
        let mut parts = Vec::new();
        match &self.v4 {
            Some(Ok(report)) => parts.push(report.message.clone()),
            Some(Err(error)) => parts.push(format!("v4 转换失败（v3 保留）：{error}")),
            None => {}
        }
        parts.push(self.prune.summary());
        parts.join("；")
    }
}

/// 建库成功后的收尾：可选 v4 转换（默认关闭）+ 保留策略清理。
///
/// 调用方：外壳在「建库完成」回调里跑一次（放后台线程，v4 转换要十几秒）。
pub fn after_create(snapshot_dir: &Path, data_root: &Path, policy: &Policy) -> AfterCreateReport {
    let mut report = AfterCreateReport::default();
    if policy.v4_after_create {
        let newest = scan(snapshot_dir).into_iter().next();
        if let Some(newest) = newest {
            if !newest.is_v4() {
                let outcome = convert_to_v4(&newest.path, data_root);
                match &outcome {
                    Ok(report) => eprintln!("[快照] {}", report.message),
                    Err(error) => eprintln!("[快照] v4 转换失败，v3 保留：{error}"),
                }
                report.v4 = Some(outcome);
            }
        }
    }
    report.prune = prune(snapshot_dir, data_root, policy.retained_count());
    report
}

/// 原子写：先写同目录临时文件再改名（index.json 被 Python 建库器共享，不能写一半）。
fn write_atomic(path: &Path, text: &str) -> Result<(), String> {
    let temporary = path.with_extension("json.tmp");
    std::fs::write(&temporary, text)
        .map_err(|error| format!("写 {} 失败：{error}", temporary.display()))?;
    std::fs::rename(&temporary, path).map_err(|error| {
        format!(
            "{} 写入失败（临时文件 {} 仍在）：{error}",
            path.display(),
            temporary.display()
        )
    })
}

/// 从 JSON 文本里删掉一个「键: 对象」成员，其余内容原样保留。
///
/// 只做一件事：找到键，按括号配对找到值对象的结尾，再连同一个逗号一起剪掉。
/// 结构认不出来（缺冒号、对象不闭合等）返回 None，调用方保留原文件不动。
fn remove_json_entry(text: &str, key: &str) -> Option<String> {
    let bytes = text.as_bytes();
    let needle = format!("\"{key}\"");
    let key_at = text.find(&needle)?;
    // 跳过键后面的空白与冒号，要求值是对象（index.json 的每个条目都是对象）
    let mut cursor = key_at + needle.len();
    while cursor < bytes.len() && bytes[cursor].is_ascii_whitespace() {
        cursor += 1;
    }
    if bytes.get(cursor) != Some(&b':') {
        return None;
    }
    cursor += 1;
    while cursor < bytes.len() && bytes[cursor].is_ascii_whitespace() {
        cursor += 1;
    }
    if bytes.get(cursor) != Some(&b'{') {
        return None;
    }
    // 括号配对（忽略字符串里的括号与转义）
    let mut depth = 0i64;
    let mut in_string = false;
    let mut escaped = false;
    let value_end;
    loop {
        let byte = *bytes.get(cursor)?;
        if in_string {
            if escaped {
                escaped = false;
            } else if byte == b'\\' {
                escaped = true;
            } else if byte == b'"' {
                in_string = false;
            }
        } else {
            match byte {
                b'"' => in_string = true,
                b'{' => depth += 1,
                b'}' => {
                    depth -= 1;
                    if depth == 0 {
                        value_end = cursor + 1;
                        break;
                    }
                }
                _ => {}
            }
        }
        cursor += 1;
    }

    // 剪掉「键: 值」以及它带走的一个逗号；如果它是最后一个成员，则剪掉前一个逗号。
    let mut start = key_at;
    let mut end = value_end;
    let mut after = end;
    while after < bytes.len() && bytes[after].is_ascii_whitespace() {
        after += 1;
    }
    if bytes.get(after) == Some(&b',') {
        end = after + 1;
        while end < bytes.len() && bytes[end].is_ascii_whitespace() {
            end += 1;
        }
    } else {
        let mut before = start;
        while before > 0 && bytes[before - 1].is_ascii_whitespace() {
            before -= 1;
        }
        if before > 0 && bytes[before - 1] == b',' {
            start = before - 1;
        }
    }
    let mut out = String::with_capacity(text.len());
    out.push_str(&text[..start]);
    out.push_str(&text[end..]);
    // 保险：键若仍出现说明结构有重复，放弃改动
    if out.contains(&needle) {
        return None;
    }
    // 保险：花括号必须仍然配平
    let opens = out.bytes().filter(|byte| *byte == b'{').count();
    let closes = out.bytes().filter(|byte| *byte == b'}').count();
    if opens != closes {
        return None;
    }
    Some(out)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::{Duration as StdDuration, SystemTime};

    /// 唯一临时目录（同仓库其它单测口径：只写自己的 %TEMP% 子目录，不做永久删除）。
    fn unique_dir(tag: &str) -> PathBuf {
        use std::sync::atomic::{AtomicU64, Ordering};
        static SEQ: AtomicU64 = AtomicU64::new(0);
        let nanos = SystemTime::now()
            .duration_since(SystemTime::UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0);
        let dir = std::env::temp_dir()
            .join("diskintel-prune-tests")
            .join(format!("{tag}-{nanos}-{}", SEQ.fetch_add(1, Ordering::Relaxed)));
        std::fs::create_dir_all(&dir).expect("建临时目录");
        dir
    }

    /// 造一个假快照（不是真 sqlite，列表/清理只看文件名与 mtime），并设定 mtime。
    fn fake_snapshot(dir: &Path, name: &str, created: u64, bytes: usize) -> PathBuf {
        std::fs::create_dir_all(dir).expect("建目录");
        let path = dir.join(format!("{name}.sqlite"));
        std::fs::write(&path, vec![0u8; bytes]).expect("写假快照");
        let file = std::fs::OpenOptions::new()
            .write(true)
            .open(&path)
            .expect("打开假快照");
        file.set_modified(SystemTime::UNIX_EPOCH + StdDuration::from_secs(created))
            .expect("设置 mtime");
        path
    }

    /// 造一个「结构像 v4」的 sqlite（只含 meta 与 names，够 verify_v4_file 通过）。
    fn stub_v4(path: &Path) {
        let conn = Connection::open(path).expect("建 stub v4");
        conn.execute_batch(
            "CREATE TABLE meta (key TEXT PRIMARY KEY, value TEXT NOT NULL);
             CREATE TABLE names (id INTEGER PRIMARY KEY, name TEXT NOT NULL);",
        )
        .expect("建表");
        conn.execute("INSERT INTO meta VALUES ('format_version', '4')", [])
            .expect("写版本");
        conn.execute("INSERT INTO names VALUES (1, 'C:\\\\')", [])
            .expect("写名字");
    }

    fn names(items: &[&str]) -> Vec<String> {
        items.iter().map(|name| (*name).to_owned()).collect()
    }

    /// 清单：按创建时间倒序，最新在前；不是 sqlite 的文件不算。
    #[test]
    fn scan_orders_newest_first() {
        let dir = unique_dir("scan");
        fake_snapshot(&dir, "20260101-000000", 1_700_000_100, 8);
        fake_snapshot(&dir, "20260102-000000", 1_700_000_200, 8);
        std::fs::write(dir.join("index.json"), b"{}").expect("写 index.json");
        let infos = scan(&dir);
        assert_eq!(infos.len(), 2);
        assert_eq!(infos[0].name, "20260102-000000");
        assert_eq!(infos[1].name, "20260101-000000");
        assert_eq!(infos[0].created_unix, 1_700_000_200);
        assert_eq!(infos[0].file_name(), "20260102-000000.sqlite");
        assert!(!infos[0].is_v4(), "假文件读不到版本按 v3 处理");
    }

    /// 最新创建时间：空目录 → None（= 首次安装）；有文件 → 最大 mtime。
    #[test]
    fn newest_created_handles_empty_and_files() {
        let dir = unique_dir("newest");
        assert_eq!(newest_created(&dir), None);
        assert_eq!(newest_created(&dir.join("not-there")), None);
        fake_snapshot(&dir, "20260101-000000", 1_700_000_100, 8);
        fake_snapshot(&dir, "20260102-000000", 1_700_000_300, 8);
        assert_eq!(newest_created(&dir), Some(1_700_000_300));
    }

    /// 启动判断：空目录即便自动开关关着也建（首次安装必须有一份）。
    #[test]
    fn should_build_on_empty_dir_even_when_switch_off() {
        let dir = unique_dir("should");
        let policy = Policy::default();
        assert!(!policy.auto_on_start);
        assert!(should_build_on_start(&policy, &dir, 1_800_000_000));
        fake_snapshot(&dir, "20260101-000000", 1_799_999_000, 8);
        assert!(!should_build_on_start(&policy, &dir, 1_800_000_000), "开关关闭时不自动建");
    }

    /// 默认布局：文件移到 <data_root>\待删除，MANIFEST 有记录，notes 条目一起消失。
    #[test]
    fn move_to_trash_default_layout_syncs_notes() {
        let root = unique_dir("trash");
        let data_root = root.join("data");
        let snapshots = data_root.join("snapshots");
        let source = fake_snapshot(&snapshots, "20260101-000000", 1_700_000_100, 16);
        let mut all = std::collections::HashMap::new();
        all.insert("20260101-000000".to_owned(), "旧备注".to_owned());
        all.insert("keep".to_owned(), "别动".to_owned());
        notes::save_in(&snapshots, &all).expect("写备注");

        let moved = move_to_trash(&source, &data_root, "测试清理").expect("移动应成功");

        assert!(!source.exists());
        assert!(moved.exists());
        assert_eq!(moved.parent(), Some(data_root.join(trash::TRASH_DIR).as_path()));
        let manifest = std::fs::read_to_string(data_root.join(trash::TRASH_DIR).join(trash::MANIFEST_FILE)).unwrap();
        assert!(manifest.contains("20260101-000000.sqlite"), "{manifest}");
        assert!(manifest.contains("测试清理"), "{manifest}");
        assert!(manifest.contains("| 快照策略 |"), "策略清理的操作者要登记：{manifest}");
        let left = notes::load_in(&snapshots);
        assert!(!left.contains_key("20260101-000000"), "删掉的快照不该留备注");
        assert_eq!(left.get("keep").map(String::as_str), Some("别动"), "别的备注不能动");
    }

    /// 自定义快照目录：待删除就近放在快照目录里（同卷 rename，不跨盘复制大文件）。
    #[test]
    fn move_to_trash_custom_dir_keeps_trash_local() {
        let root = unique_dir("custom");
        let data_root = root.join("data");
        let custom = root.join("我的快照");
        let source = fake_snapshot(&custom, "20260101-000000", 1_700_000_100, 16);
        let moved = move_to_trash(&source, &data_root, "自定义目录清理").expect("移动应成功");
        assert_eq!(moved.parent(), Some(custom.join(trash::TRASH_DIR).as_path()));
        assert!(!data_root.join(trash::TRASH_DIR).exists(), "默认布局的待删除不该被创建");
    }

    /// index.json：删掉一条后，其余条目与结构原样保留。
    #[test]
    fn index_entry_removal_keeps_other_entries() {
        let dir = unique_dir("index");
        let text = "{\"entries\": {\"20260101-000000.sqlite\": {\"meta\": {\"note\": \"旧\"}, \"mtime_ns\": 1, \"size\": 10}, \"20260102-000000.sqlite\": {\"meta\": {\"note\": \"中\"}, \"mtime_ns\": 2, \"size\": 20}, \"20260103-000000.sqlite\": {\"meta\": {\"note\": \"新\"}, \"mtime_ns\": 3, \"size\": 30}}, \"version\": 1}";
        std::fs::write(dir.join(INDEX_FILE), text).expect("写 index.json");

        remove_index_entry(&dir, "20260102-000000.sqlite").expect("删除中间条目");
        let after = std::fs::read_to_string(dir.join(INDEX_FILE)).unwrap();
        assert!(!after.contains("20260102-000000.sqlite"), "{after}");
        assert!(after.contains("20260101-000000.sqlite"), "{after}");
        assert!(after.contains("20260103-000000.sqlite"), "{after}");
        assert!(after.contains("\"version\": 1"), "{after}");

        // 删最后一个成员（要连带前面的逗号）
        remove_index_entry(&dir, "20260103-000000.sqlite").expect("删除最后条目");
        let after = std::fs::read_to_string(dir.join(INDEX_FILE)).unwrap();
        assert!(!after.contains("20260103-000000.sqlite"), "{after}");
        assert!(after.contains("20260101-000000.sqlite"), "{after}");

        // 删不存在的条目：文件内容不变
        let before = std::fs::read_to_string(dir.join(INDEX_FILE)).unwrap();
        remove_index_entry(&dir, "no-such.sqlite").expect("不存在也算成功");
        assert_eq!(std::fs::read_to_string(dir.join(INDEX_FILE)).unwrap(), before);
    }

    /// index.json 缺失直接成功；结构坏的保留原文件并报错（不能把建库器缓存写坏）。
    #[test]
    fn index_removal_is_safe_on_missing_or_broken_file() {
        let dir = unique_dir("index-broken");
        remove_index_entry(&dir, "20260101-000000.sqlite").expect("缺文件应成功");
        for broken in [
            "",
            "{",
            "not json",
            "{\"entries\": {\"20260101-000000.sqlite\": {\"meta\": {}}}",
        ] {
            std::fs::write(dir.join(INDEX_FILE), broken).expect("写坏内容");
            assert!(
                remove_index_entry(&dir, "20260101-000000.sqlite").is_err(),
                "坏内容应当报错而不是乱改：{broken:?}"
            );
            assert_eq!(
                std::fs::read_to_string(dir.join(INDEX_FILE)).unwrap(),
                broken,
                "坏文件必须原样保留"
            );
        }
        // 结构成立但没有这一条：不算错，也不改文件
        let plausible = "{\"entries\": {}, \"version\": 1}";
        std::fs::write(dir.join(INDEX_FILE), plausible).expect("写空 entries");
        remove_index_entry(&dir, "20260101-000000.sqlite").expect("没有这一条不算失败");
        assert_eq!(std::fs::read_to_string(dir.join(INDEX_FILE)).unwrap(), plausible);
    }

    /// 保留 8 份：10 份里最旧的 2 份移入待删除，剩余 8 份原地不动。
    #[test]
    fn prune_keeps_newest_eight_of_ten() {
        let root = unique_dir("prune");
        let data_root = root.join("data");
        let snapshots = data_root.join("snapshots");
        for day in 1..=10u64 {
            fake_snapshot(
                &snapshots,
                &format!("202601{day:02}-000000"),
                1_700_000_000 + day * 3600,
                8,
            );
        }
        let report = prune(&snapshots, &data_root, 8);
        assert_eq!(report.kept.len(), 8);
        assert_eq!(report.moved.len(), 2);
        assert!(report.failed.is_empty());
        assert_eq!(
            report.moved.iter().map(|(name, _)| name.clone()).collect::<Vec<_>>(),
            names(&["20260101-000000", "20260102-000000"]),
            "从最旧的开始清理"
        );
        let left = scan(&snapshots);
        assert_eq!(left.len(), 8);
        assert!(!snapshots.join("20260101-000000.sqlite").exists());
        assert!(snapshots.join("20260110-000000.sqlite").exists());
        let manifest = std::fs::read_to_string(data_root.join(trash::TRASH_DIR).join(trash::MANIFEST_FILE)).unwrap();
        assert_eq!(manifest.lines().filter(|line| line.starts_with("| 2")).count(), 2);
    }

    /// 份数没超上限：一份都不动。
    #[test]
    fn prune_does_nothing_below_limit() {
        let root = unique_dir("prune-skip");
        let data_root = root.join("data");
        let snapshots = data_root.join("snapshots");
        for day in 1..=3u64 {
            fake_snapshot(&snapshots, &format!("202601{day:02}-000000"), 1_700_000_000 + day, 8);
        }
        let report = prune(&snapshots, &data_root, 8);
        assert_eq!(report.kept.len(), 3);
        assert!(report.moved.is_empty());
        assert!(!report.did_change());
        assert!(report.summary().contains("无需清理"), "{}", report.summary());
        assert!(!data_root.join(trash::TRASH_DIR).exists());
    }

    /// v4 校验：接受结构完整的 v4，拒绝 v3 / 垃圾 / 空文件。
    #[test]
    fn verify_v4_only_accepts_real_v4() {
        let dir = unique_dir("verify");
        let good = dir.join("good.sqlite");
        stub_v4(&good);
        verify_v4_file(&good).expect("stub v4 应当通过");
        let bad = dir.join("bad.sqlite");
        std::fs::write(&bad, b"garbage").expect("写垃圾");
        assert!(verify_v4_file(&bad).is_err());
        assert!(verify_v4_file(&dir.join("missing.sqlite")).is_err());
        let empty = dir.join("empty.sqlite");
        std::fs::write(&empty, b"").expect("写空文件");
        assert!(verify_v4_file(&empty).is_err());
    }

    /// 转换成功：v3 移入待删除、v4 顶替原名（快照名不变）。
    #[test]
    fn convert_success_replaces_v3_with_same_name() {
        let root = unique_dir("convert-ok");
        let data_root = root.join("data");
        let snapshots = data_root.join("snapshots");
        let source = fake_snapshot(&snapshots, "20260101-000000", 1_700_000_100, 64);
        let report = convert_v4_impl(&source, &data_root, |_v3, target| {
            stub_v4(target);
            Ok("模拟转换成功".to_owned())
        })
        .expect("转换应成功");
        assert_eq!(report.target, snapshots.join("20260101-000000.sqlite"));
        assert!(report.target.exists());
        assert!(!snapshots.join("20260101-000000.v4.sqlite").exists(), "中间文件要改名掉");
        let trashed = report.trashed_source.expect("v3 应已移入待删除");
        assert!(trashed.exists(), "v3 原文件在待删除里");
        let manifest = std::fs::read_to_string(data_root.join(trash::TRASH_DIR).join(trash::MANIFEST_FILE)).unwrap();
        assert!(manifest.contains("转 v4 成功"), "{manifest}");
        assert_eq!(db::peek_format_version(&report.target), Some(4));
    }

    /// 转换失败：v3 原样保留，不产生 v4 文件，也不搬动 v3。
    #[test]
    fn convert_failure_keeps_v3() {
        let root = unique_dir("convert-fail");
        let data_root = root.join("data");
        let snapshots = data_root.join("snapshots");
        let source = fake_snapshot(&snapshots, "20260101-000000", 1_700_000_100, 64);
        let error = convert_v4_impl(&source, &data_root, |_v3, _target| Err("模拟失败".to_owned()))
            .unwrap_err();
        assert!(error.contains("模拟失败"), "{error}");
        assert!(source.exists(), "v3 必须还在");
        assert!(!snapshots.join("20260101-000000.v4.sqlite").exists());
        assert!(!data_root.join(trash::TRASH_DIR).exists(), "失败不该动 v3");
    }

    /// 转换成功但产物不是 v4（转换器撒谎）：v3 保留，不替换。
    #[test]
    fn convert_rejects_non_v4_output() {
        let root = unique_dir("convert-bad-output");
        let data_root = root.join("data");
        let snapshots = data_root.join("snapshots");
        let source = fake_snapshot(&snapshots, "20260101-000000", 1_700_000_100, 64);
        let error = convert_v4_impl(&source, &data_root, |_v3, target| {
            std::fs::write(target, b"not v4").expect("写假产物");
            Ok(String::new())
        })
        .unwrap_err();
        assert!(error.contains("v4 产物校验失败"), "{error}");
        assert!(source.exists(), "v3 必须还在");
    }

    /// 已经是 v4 的快照不重复转换。
    #[test]
    fn convert_skips_already_v4() {
        let root = unique_dir("convert-twice");
        let data_root = root.join("data");
        let snapshots = data_root.join("snapshots");
        std::fs::create_dir_all(&snapshots).expect("建目录");
        let source = snapshots.join("20260101-000000.sqlite");
        stub_v4(&source);
        let error = convert_v4_impl(&source, &data_root, |_v3, _target| {
            panic!("已经是 v4 不该再调用转换器")
        })
        .unwrap_err();
        assert!(error.contains("已经是 v4"), "{error}");
    }

    /// 建库后收尾：默认策略（v4 关闭）只做保留清理，不碰 v4 转换。
    #[test]
    fn after_create_respects_v4_switch_off() {
        let root = unique_dir("after-create");
        let data_root = root.join("data");
        let snapshots = data_root.join("snapshots");
        for day in 1..=3u64 {
            fake_snapshot(&snapshots, &format!("202601{day:02}-000000"), 1_700_000_000 + day, 8);
        }
        let policy = Policy::default();
        assert!(!policy.v4_after_create);
        let report = after_create(&snapshots, &data_root, &policy);
        assert!(report.v4.is_none(), "默认不转 v4");
        assert_eq!(report.prune.kept.len(), 3);
        assert!(report.summary().contains("无需清理"), "{}", report.summary());
    }

    /// 验收 / 运维用：DI_PRUNE_DIR=<数据根> 时对真实目录跑一次保留策略并打印明细。
    ///
    /// 默认 #[ignore]，只在显式请求时执行（cargo test --release -- --ignored prune_demo_dir）。
    /// 只做「移入待删除」，不永久删除；空目录 / 未设环境变量直接跳过。
    #[test]
    #[ignore]
    fn prune_demo_dir_from_env() {
        let Ok(data_root) = std::env::var("DI_PRUNE_DIR") else {
            eprintln!("跳过：未设 DI_PRUNE_DIR");
            return;
        };
        let data_root = PathBuf::from(data_root);
        let snapshot_dir = data_root.join("snapshots");
        let before = scan(&snapshot_dir);
        eprintln!("清理前 {} 份：", before.len());
        for info in &before {
            eprintln!(
                "  {}  created={}  {} 字节  v{}",
                info.name,
                info.created_unix,
                info.bytes,
                info.format_version
            );
        }
        let report = prune(&snapshot_dir, &data_root, 8);
        eprintln!("结果：{}", report.summary());
        eprintln!("保留 {} 份：", report.kept.len());
        for name in &report.kept {
            eprintln!("  {name}");
        }
        for (name, target) in &report.moved {
            eprintln!("  移入待删除：{name} → {}", target.display());
        }
        for (name, error) in &report.failed {
            eprintln!("  失败：{name}：{error}");
        }
        assert!(report.failed.is_empty(), "清理失败：{:?}", report.failed);
        assert_eq!(report.kept.len(), 8.min(before.len()));
        assert_eq!(report.moved.len(), before.len().saturating_sub(8));
    }
}

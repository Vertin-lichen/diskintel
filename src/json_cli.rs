//! AI / 脚本通道：`--json <verb>` 与 MCP 工具共用的一层只读查询。
//!
//! 口径权威是 Python 参考实现 `磁盘情报系统/src/diskintel/api.py`：同样的过滤、
//! 同样的排序、同样的字段名，保证「窗口里看到的」与「agent 拿到的」不是两套数据。
//!
//! 只读纪律：所有快照连接都是 `SQLITE_OPEN_READ_ONLY` + `PRAGMA query_only=ON`；
//! SQL 工具另有词法白名单 + 单语句检查 + 5 秒语句超时。这里不写任何文件。

use std::cmp::{Ordering, Reverse};
use std::collections::{BinaryHeap, HashMap, HashSet};
use std::io::Write as _;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering as AtomicOrdering};
use std::sync::{mpsc, Arc};
use std::thread;
use std::time::{Duration, Instant};

use chrono::TimeZone;
use rusqlite::types::{Value, ValueRef};
use rusqlite::{params_from_iter, Connection, OpenFlags};

pub const MAX_LIMIT: i64 = 1000;
/// 与 Python 版 `FORMAT_VERSION` 对齐：本程序读得动的快照格式上限。
pub const FORMAT_VERSION: i64 = 3;
pub const DEFAULT_DATA_ROOT: &str = r"D:\disk-intel-data";
pub const VERSION: &str = env!("CARGO_PKG_VERSION");
/// 只读查询端的页缓存与内存映射，取自 Python 版 `store.QUERY_*`（实测口径）。
const QUERY_CACHE_KIB: i64 = 65_536;
const QUERY_MMAP_BYTES: i64 = 1024 << 20;
/// SQL 工具语句级超时；用 `InterruptHandle` 实现（rusqlite 的 progress_handler 需 hooks 特性）。
const SQL_TIMEOUT_SECONDS: f64 = 5.0;

const BUILD_HINT: &str = "先建一份快照：窗口里点「重建索引」，或运行 diskintel --json snap";

// ---------------------------------------------------------------------------
// 极小 JSON 实现
// ---------------------------------------------------------------------------

/// 手写 JSON 值。工程没有 serde 依赖（Cargo.toml 不在本任务范围内），
/// 而协议层只需要「解析一条请求 / 写一条响应」，所以自带一个小实现。
/// 对象保留插入顺序，便于人工比对与快照测试。
#[derive(Clone, Debug, PartialEq)]
pub enum Json {
    Null,
    Bool(bool),
    Int(i64),
    Real(f64),
    Str(String),
    Arr(Vec<Json>),
    Obj(Vec<(String, Json)>),
}

pub fn jstr(value: impl Into<String>) -> Json {
    Json::Str(value.into())
}

pub fn jnum(value: i64) -> Json {
    Json::Int(value)
}

pub fn jbool(value: bool) -> Json {
    Json::Bool(value)
}

impl Json {
    /// 用 `(&str, Json)` 列表构造对象，省掉一堆 `vec![(..)]` 噪声。
    pub fn obj(pairs: Vec<(&str, Json)>) -> Json {
        Json::Obj(
            pairs
                .into_iter()
                .map(|(key, value)| (key.to_owned(), value))
                .collect(),
        )
    }

    pub fn arr(items: Vec<Json>) -> Json {
        Json::Arr(items)
    }

    /// 覆盖写入（已存在同名键时替换，否则追加）。
    pub fn set(&mut self, key: &str, value: Json) {
        if let Json::Obj(pairs) = self {
            for pair in pairs.iter_mut() {
                if pair.0 == key {
                    pair.1 = value;
                    return;
                }
            }
            pairs.push((key.to_owned(), value));
        }
    }

    pub fn get(&self, key: &str) -> Option<&Json> {
        match self {
            Json::Obj(pairs) => pairs
                .iter()
                .rev()
                .find(|(name, _)| name == key)
                .map(|(_, value)| value),
            _ => None,
        }
    }

    pub fn as_str(&self) -> Option<&str> {
        match self {
            Json::Str(value) => Some(value.as_str()),
            _ => None,
        }
    }

    pub fn is_null(&self) -> bool {
        matches!(self, Json::Null)
    }

    pub fn to_json_string(&self) -> String {
        let mut out = String::new();
        self.write_into(&mut out);
        out
    }

    pub fn write_into(&self, out: &mut String) {
        match self {
            Json::Null => out.push_str("null"),
            Json::Bool(true) => out.push_str("true"),
            Json::Bool(false) => out.push_str("false"),
            Json::Int(value) => out.push_str(&value.to_string()),
            Json::Real(value) => write_real(*value, out),
            Json::Str(value) => write_string(value, out),
            Json::Arr(items) => {
                out.push('[');
                for (index, item) in items.iter().enumerate() {
                    if index > 0 {
                        out.push(',');
                    }
                    item.write_into(out);
                }
                out.push(']');
            }
            Json::Obj(pairs) => {
                out.push('{');
                for (index, (key, value)) in pairs.iter().enumerate() {
                    if index > 0 {
                        out.push(',');
                    }
                    write_string(key, out);
                    out.push(':');
                    value.write_into(out);
                }
                out.push('}');
            }
        }
    }

    pub fn parse(text: &str) -> Result<Json, String> {
        let mut parser = JsonParser {
            bytes: text.as_bytes(),
            text,
            pos: 0,
            depth: 0,
        };
        let value = parser.value()?;
        parser.skip_ws();
        if parser.pos != parser.bytes.len() {
            return Err(format!("JSON 末尾有多余字符（偏移 {}）", parser.pos));
        }
        Ok(value)
    }
}

fn write_string(value: &str, out: &mut String) {
    out.push('"');
    for ch in value.chars() {
        match ch {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            '\u{08}' => out.push_str("\\b"),
            '\u{0c}' => out.push_str("\\f"),
            ch if (ch as u32) < 0x20 => {
                out.push_str(&format!("\\u{:04x}", ch as u32));
            }
            ch => out.push(ch),
        }
    }
    out.push('"');
}

/// 浮点一律按 4 位小数写出再裁尾零：与 Python 侧 `round(x, 4)` 的显示口径一致。
fn write_real(value: f64, out: &mut String) {
    if !value.is_finite() {
        out.push_str("null");
        return;
    }
    let mut text = format!("{value:.4}");
    if text.contains('.') {
        while text.ends_with('0') {
            text.pop();
        }
        if text.ends_with('.') {
            text.push('0');
        }
    }
    out.push_str(&text);
}

struct JsonParser<'a> {
    bytes: &'a [u8],
    text: &'a str,
    pos: usize,
    depth: u32,
}

impl<'a> JsonParser<'a> {
    fn skip_ws(&mut self) {
        while let Some(byte) = self.bytes.get(self.pos) {
            if matches!(byte, b' ' | b'\t' | b'\n' | b'\r') {
                self.pos += 1;
            } else {
                break;
            }
        }
    }

    fn peek(&self) -> Option<u8> {
        self.bytes.get(self.pos).copied()
    }

    fn value(&mut self) -> Result<Json, String> {
        self.skip_ws();
        let byte = self
            .peek()
            .ok_or_else(|| "JSON 意外结束".to_owned())?;
        match byte {
            b'{' => self.object(),
            b'[' => self.array(),
            b'"' => Ok(Json::Str(self.string()?)),
            b't' => {
                self.literal("true")?;
                Ok(Json::Bool(true))
            }
            b'f' => {
                self.literal("false")?;
                Ok(Json::Bool(false))
            }
            b'n' => {
                self.literal("null")?;
                Ok(Json::Null)
            }
            b'-' | b'0'..=b'9' => self.number(),
            other => Err(format!("JSON 非法字符 {:?}（偏移 {}）", other as char, self.pos)),
        }
    }

    fn literal(&mut self, word: &str) -> Result<(), String> {
        if self.bytes[self.pos..].starts_with(word.as_bytes()) {
            self.pos += word.len();
            Ok(())
        } else {
            Err(format!("JSON 非法字面量（偏移 {}）", self.pos))
        }
    }

    fn enter(&mut self) -> Result<(), String> {
        self.depth += 1;
        if self.depth > 128 {
            return Err("JSON 嵌套过深（>128 层）".to_owned());
        }
        Ok(())
    }

    fn object(&mut self) -> Result<Json, String> {
        self.enter()?;
        self.pos += 1; // {
        let mut pairs: Vec<(String, Json)> = Vec::new();
        self.skip_ws();
        if self.peek() == Some(b'}') {
            self.pos += 1;
            self.depth -= 1;
            return Ok(Json::Obj(pairs));
        }
        loop {
            self.skip_ws();
            let key = self.string()?;
            self.skip_ws();
            if self.peek() != Some(b':') {
                return Err(format!("JSON 对象缺少冒号（偏移 {}）", self.pos));
            }
            self.pos += 1;
            let value = self.value()?;
            pairs.push((key, value));
            self.skip_ws();
            match self.peek() {
                Some(b',') => {
                    self.pos += 1;
                }
                Some(b'}') => {
                    self.pos += 1;
                    self.depth -= 1;
                    return Ok(Json::Obj(pairs));
                }
                _ => return Err(format!("JSON 对象未闭合（偏移 {}）", self.pos)),
            }
        }
    }

    fn array(&mut self) -> Result<Json, String> {
        self.enter()?;
        self.pos += 1; // [
        let mut items = Vec::new();
        self.skip_ws();
        if self.peek() == Some(b']') {
            self.pos += 1;
            self.depth -= 1;
            return Ok(Json::Arr(items));
        }
        loop {
            let value = self.value()?;
            items.push(value);
            self.skip_ws();
            match self.peek() {
                Some(b',') => {
                    self.pos += 1;
                }
                Some(b']') => {
                    self.pos += 1;
                    self.depth -= 1;
                    return Ok(Json::Arr(items));
                }
                _ => return Err(format!("JSON 数组未闭合（偏移 {}）", self.pos)),
            }
        }
    }

    fn string(&mut self) -> Result<String, String> {
        if self.peek() != Some(b'"') {
            return Err(format!("JSON 字符串缺少引号（偏移 {}）", self.pos));
        }
        self.pos += 1;
        let mut out = String::new();
        loop {
            let start = self.pos;
            while let Some(byte) = self.peek() {
                if byte == b'"' || byte == b'\\' {
                    break;
                }
                self.pos += 1;
            }
            out.push_str(
                std::str::from_utf8(&self.bytes[start..self.pos])
                    .map_err(|_| "JSON 字符串不是合法 UTF-8".to_owned())?,
            );
            match self.peek() {
                Some(b'"') => {
                    self.pos += 1;
                    return Ok(out);
                }
                Some(b'\\') => {
                    self.pos += 1;
                    out.push(self.escape()?);
                }
                _ => return Err(format!("JSON 字符串未闭合（偏移 {}）", self.pos)),
            }
        }
    }

    fn escape(&mut self) -> Result<char, String> {
        let byte = self
            .peek()
            .ok_or_else(|| "JSON 转义序列不完整".to_owned())?;
        self.pos += 1;
        match byte {
            b'"' => Ok('"'),
            b'\\' => Ok('\\'),
            b'/' => Ok('/'),
            b'b' => Ok('\u{08}'),
            b'f' => Ok('\u{0c}'),
            b'n' => Ok('\n'),
            b'r' => Ok('\r'),
            b't' => Ok('\t'),
            b'u' => {
                let high = self.hex4()?;
                if (0xD800..=0xDBFF).contains(&high) {
                    if self.peek() == Some(b'\\') {
                        self.pos += 1;
                        if self.peek() == Some(b'u') {
                            self.pos += 1;
                            let low = self.hex4()?;
                            if (0xDC00..=0xDFFF).contains(&low) {
                                let code = 0x10000 + ((high - 0xD800) << 10) + (low - 0xDC00);
                                return char::from_u32(code)
                                    .ok_or_else(|| "JSON 代理对非法".to_owned());
                            }
                        }
                    }
                    return Err("JSON 高位代理缺少低位代理".to_owned());
                }
                char::from_u32(high).ok_or_else(|| "JSON \\u 转义非法".to_owned())
            }
            other => Err(format!("JSON 未知转义 \\{}", other as char)),
        }
    }

    fn hex4(&mut self) -> Result<u32, String> {
        if self.pos + 4 > self.bytes.len() {
            return Err("JSON \\u 转义不完整".to_owned());
        }
        let text = std::str::from_utf8(&self.bytes[self.pos..self.pos + 4])
            .map_err(|_| "JSON \\u 转义非法".to_owned())?;
        let value = u32::from_str_radix(text, 16).map_err(|_| "JSON \\u 转义非法".to_owned())?;
        self.pos += 4;
        Ok(value)
    }

    fn number(&mut self) -> Result<Json, String> {
        let start = self.pos;
        if self.peek() == Some(b'-') || self.peek() == Some(b'+') {
            self.pos += 1;
        }
        let mut real = false;
        while let Some(byte) = self.peek() {
            match byte {
                b'0'..=b'9' => self.pos += 1,
                b'.' | b'e' | b'E' => {
                    real = true;
                    self.pos += 1;
                }
                b'+' | b'-' => self.pos += 1,
                _ => break,
            }
        }
        let text = &self.text[start..self.pos];
        if text.is_empty() {
            return Err("JSON 数字为空".to_owned());
        }
        if !real {
            if let Ok(value) = text.parse::<i64>() {
                return Ok(Json::Int(value));
            }
        }
        text.parse::<f64>()
            .map(Json::Real)
            .map_err(|_| format!("JSON 数字非法：{text}"))
    }
}

// ---------------------------------------------------------------------------
// 错误与快照目录
// ---------------------------------------------------------------------------

/// 业务错误：一律带稳定的 code（对齐 Python 版 ApiError.code），调用方按 code 分流。
#[derive(Clone, Debug)]
pub struct CoreError {
    pub code: &'static str,
    pub message: String,
    pub hint: String,
}

impl CoreError {
    pub fn new(code: &'static str, message: impl Into<String>) -> Self {
        Self {
            code,
            message: message.into(),
            hint: String::new(),
        }
    }

    pub fn hint(mut self, hint: impl Into<String>) -> Self {
        self.hint = hint.into();
        self
    }

    pub fn to_json(&self) -> Json {
        let mut pairs = vec![
            ("code", jstr(self.code)),
            ("message", jstr(self.message.clone())),
        ];
        if !self.hint.is_empty() {
            pairs.push(("hint", jstr(self.hint.clone())));
        }
        Json::obj(pairs)
    }
}

fn snapshot_error(message: impl Into<String>) -> CoreError {
    CoreError::new("SNAPSHOT_NOT_FOUND", message).hint(BUILD_HINT)
}

pub fn snapshots_dir(data_root: &Path) -> PathBuf {
    data_root.join("snapshots")
}

#[derive(Clone, Debug)]
pub struct SnapInfo {
    pub id: String,
    pub path: PathBuf,
    pub created_at: i64,
    pub file_count: i64,
    pub dir_count: i64,
    pub total_size: i64,
    pub bytes_on_disk: i64,
    pub note: String,
    pub format_version: i64,
    pub excluded_count: i64,
    pub excluded_bytes: i64,
    pub excluded_roots: Vec<String>,
}

fn read_meta_map(path: &Path) -> Option<HashMap<String, String>> {
    let conn = Connection::open_with_flags(path, OpenFlags::SQLITE_OPEN_READ_ONLY).ok()?;
    let mut stmt = conn.prepare("SELECT key, value FROM meta").ok()?;
    let rows = stmt
        .query_map([], |row| {
            Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
        })
        .ok()?;
    let mut map = HashMap::new();
    for (key, value) in rows.flatten() {
        map.insert(key, value);
    }
    Some(map)
}

fn meta_i64(map: &HashMap<String, String>, key: &str) -> i64 {
    map.get(key)
        .and_then(|value| value.trim().parse::<i64>().ok())
        .unwrap_or(0)
}

/// meta 里的字符串数组（volume_roots / excluded_roots）就是 JSON 字面量，
/// 复用自身解析器，避免为两个字段再引 serde。
fn meta_strings(map: &HashMap<String, String>, key: &str) -> Vec<String> {
    match map.get(key).map(|raw| Json::parse(raw)) {
        Some(Ok(Json::Arr(items))) => items
            .into_iter()
            .filter_map(|item| item.as_str().map(str::to_owned))
            .collect(),
        _ => Vec::new(),
    }
}

/// 与 Python store.snapshot_sort_key 一致：20261109-231535-2 要排在
/// 20261109-231535 之后，否则 latest 会指向旧快照、diff 方向颠倒。
fn snapshot_sort_key(id: &str) -> (String, i64) {
    if let Some((base, suffix)) = id.rsplit_once('-') {
        if base.len() == 15
            && base.as_bytes().get(8) == Some(&b'-')
            && !suffix.is_empty()
            && suffix.bytes().all(|byte| byte.is_ascii_digit())
        {
            if let Ok(number) = suffix.parse::<i64>() {
                return (base.to_owned(), number);
            }
        }
    }
    (id.to_owned(), 0)
}

pub fn list_snapshots(data_root: &Path) -> Vec<SnapInfo> {
    let mut out = Vec::new();
    let Ok(entries) = std::fs::read_dir(snapshots_dir(data_root)) else {
        return out;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.extension().and_then(|value| value.to_str()) != Some("sqlite") {
            continue;
        }
        let Some(meta) = read_meta_map(&path) else {
            continue;
        };
        // 建库中断留下的半成品没有这两个键，跳过而不是当成有效快照（同 Python）。
        if !meta.contains_key("format_version") || !meta.contains_key("snapshot_id") {
            continue;
        }
        let id = meta
            .get("snapshot_id")
            .cloned()
            .or_else(|| {
                path.file_stem()
                    .and_then(|value| value.to_str())
                    .map(str::to_owned)
            })
            .unwrap_or_else(|| "-".to_owned());
        let bytes_on_disk = entry.metadata().map(|meta| meta.len()).unwrap_or(0) as i64;
        out.push(SnapInfo {
            id,
            path,
            created_at: meta_i64(&meta, "created_at"),
            file_count: meta_i64(&meta, "file_count"),
            dir_count: meta_i64(&meta, "dir_count"),
            total_size: meta_i64(&meta, "total_size"),
            bytes_on_disk,
            note: meta.get("note").cloned().unwrap_or_default(),
            format_version: meta_i64(&meta, "format_version"),
            excluded_count: meta_i64(&meta, "excluded_count"),
            excluded_bytes: meta_i64(&meta, "excluded_bytes"),
            excluded_roots: meta_strings(&meta, "excluded_roots"),
        });
    }
    out.sort_by(|left, right| snapshot_sort_key(&left.id).cmp(&snapshot_sort_key(&right.id)));
    out
}

fn is_snapshot_alias(value: &str) -> bool {
    if value == "latest" {
        return true;
    }
    match value.strip_prefix("latest-") {
        Some(rest) => !rest.is_empty() && rest.bytes().all(|byte| byte.is_ascii_digit()),
        None => false,
    }
}

/// 快照 ID 白名单：ID 来自 MCP / CLI 等外部入口，直接拼进文件路径会路径穿越。
fn is_snapshot_id(value: &str) -> bool {
    let Some((date, rest)) = value.split_once('-') else {
        return false;
    };
    if date.len() != 8 || !date.bytes().all(|byte| byte.is_ascii_digit()) {
        return false;
    }
    let (clock, suffix) = match rest.split_once('-') {
        Some((clock, suffix)) => (clock, Some(suffix)),
        None => (rest, None),
    };
    if clock.len() != 6 || !clock.bytes().all(|byte| byte.is_ascii_digit()) {
        return false;
    }
    match suffix {
        None => true,
        Some(suffix) => !suffix.is_empty() && suffix.bytes().all(|byte| byte.is_ascii_digit()),
    }
}

pub fn resolve_snapshot(data_root: &Path, snapshot: &str) -> Result<PathBuf, CoreError> {
    let value = snapshot.trim();
    if is_snapshot_alias(value) {
        let snapshots = list_snapshots(data_root);
        if snapshots.is_empty() {
            return Err(snapshot_error("尚无任何快照"));
        }
        let offset = value
            .strip_prefix("latest-")
            .and_then(|rest| rest.parse::<usize>().ok())
            .unwrap_or(0);
        if offset >= snapshots.len() {
            return Err(snapshot_error(format!(
                "快照别名越界：{value}（共 {} 份）",
                snapshots.len()
            )));
        }
        return Ok(snapshots[snapshots.len() - 1 - offset].path.clone());
    }
    if !is_snapshot_id(value) {
        return Err(snapshot_error(format!("快照 ID 不合法：{snapshot}")));
    }
    let path = snapshots_dir(data_root).join(format!("{value}.sqlite"));
    if path.is_file() {
        Ok(path)
    } else {
        Err(snapshot_error(format!("快照不存在：{value}")))
    }
}

/// 只读打开：SQLITE_OPEN_READ_ONLY + query_only=ON，页缓存与 mmap 取 Python 侧实测值。
pub fn open_snapshot(path: &Path) -> Result<Connection, CoreError> {
    let conn =
        Connection::open_with_flags(path, OpenFlags::SQLITE_OPEN_READ_ONLY).map_err(|error| {
            CoreError::new("SNAPSHOT_OPEN_FAILED", format!("打开快照失败：{error}"))
                .hint(BUILD_HINT)
        })?;
    let _ = conn.execute_batch(&format!(
        "PRAGMA query_only=ON; PRAGMA mmap_size={QUERY_MMAP_BYTES}; \
         PRAGMA cache_size=-{QUERY_CACHE_KIB}; PRAGMA temp_store=FILE;"
    ));
    Ok(conn)
}

fn table_has_column(conn: &Connection, table: &str, column: &str) -> bool {
    let Ok(mut stmt) = conn.prepare(&format!("PRAGMA table_info({table})")) else {
        return false;
    };
    let names: Vec<String> = stmt
        .query_map([], |row| row.get::<_, String>(1))
        .map(|rows| rows.flatten().collect())
        .unwrap_or_default();
    names.iter().any(|name| name == column)
}

/// 与 Python api._like_literal 一致：把反斜杠与 LIKE 通配符转义成字面量。
fn like_literal(value: &str) -> String {
    value
        .replace('\\', "\\\\")
        .replace('%', "\\%")
        .replace('_', "\\_")
}

fn clamp_limit(value: i64) -> i64 {
    value.clamp(0, MAX_LIMIT)
}

// ---------------------------------------------------------------------------
// 检索
// ---------------------------------------------------------------------------

const DOC_EXTS: [&str; 17] = [
    "pdf", "doc", "docx", "xls", "xlsx", "ppt", "pptx", "txt", "md", "rtf", "odt", "ods", "odp",
    "csv", "wps", "et", "dps",
];

const SYSTEM_MARKERS: [&str; 15] = [
    "\\windows\\",
    "\\program files",
    "\\programdata\\",
    "\\appdata\\",
    "\\$recycle.bin\\",
    "\\system volume information\\",
    "\\temp\\",
    "\\tmp\\",
    "\\cache\\",
    "\\node_modules\\",
    "\\.git\\",
    "\\logs\\",
    "\\pagefile.sys",
    "\\hiberfil.sys",
    "\\swapfile.sys",
];

fn sort_alias(value: &str) -> String {
    match value.to_lowercase().as_str() {
        "smart" | "智能" => "smart",
        "newest" | "latest" | "最新" | "new" => "newest",
        "oldest" | "最早" | "old" => "oldest",
        "folders" | "folder" | "文件夹" => "folders",
        "documents" | "document" | "docs" | "文档" => "documents",
        "size" | "大小" => "size",
        "name" | "名称" => "name",
        other => other,
    }
    .to_owned()
}

/// 与 Python api._sort_sql 逐字对应：窗口里的「智能排序」就是这一条 CASE。
fn sort_sql(sort: &str, has_ext: bool) -> String {
    let sort = sort_alias(sort);
    let ext_expr = if has_ext { "ext" } else { "''" };
    let doc_exts = DOC_EXTS
        .iter()
        .map(|ext| format!("'{ext}'"))
        .collect::<Vec<_>>()
        .join(",");
    let system_sql = SYSTEM_MARKERS
        .iter()
        .map(|marker| format!("path LIKE '%{marker}%'"))
        .collect::<Vec<_>>()
        .join(" OR ");
    match sort.as_str() {
        "newest" => "mtime DESC, size DESC, path ASC".to_owned(),
        "oldest" => "mtime ASC, size ASC, path ASC".to_owned(),
        "folders" => "is_dir DESC, size DESC, path ASC".to_owned(),
        "documents" => format!(
            "CASE WHEN is_dir = 0 AND {ext_expr} IN ({doc_exts}) THEN 0 ELSE 1 END, \
             size DESC, path ASC"
        ),
        "size" => "size DESC, path ASC".to_owned(),
        "name" => "path ASC".to_owned(),
        _ => format!(
            "CASE WHEN is_dir = 0 AND {ext_expr} IN ({doc_exts}) THEN 0 \
             WHEN is_dir = 1 THEN 1 WHEN {system_sql} THEN 3 ELSE 2 END, \
             mtime DESC, size DESC, path ASC"
        ),
    }
}

#[derive(Clone, Debug, Default)]
struct ParsedQuery {
    terms: Vec<String>,
    ext: Option<String>,
    min_size: Option<i64>,
    max_size: Option<i64>,
    after: Option<i64>,
    before: Option<i64>,
    kind: Option<String>,
    scope: Option<String>,
    sort: Option<String>,
    limit: Option<i64>,
}

/// 近似 shlex.split(posix=False)：引号内不切分、反斜杠不当转义，
/// 这样 "D:\Program Files" 仍是一个词，与 Python 版查询串语义一致。
fn tokenize_query(text: &str) -> Vec<String> {
    let mut tokens = Vec::new();
    let mut current = String::new();
    let mut quoted = false;
    let mut started = false;
    for ch in text.chars() {
        if ch == '"' {
            quoted = !quoted;
            current.push(ch);
            started = true;
            continue;
        }
        if ch.is_whitespace() && !quoted {
            if started {
                tokens.push(std::mem::take(&mut current));
                started = false;
            }
            continue;
        }
        current.push(ch);
        started = true;
    }
    if started {
        tokens.push(current);
    }
    tokens
}

fn clean_token(raw: &str) -> String {
    raw.trim().trim_matches('"').trim_matches('\'').to_owned()
}

fn parse_size_value(raw: &str) -> Result<i64, ()> {
    let value = raw.trim();
    let rest = value
        .strip_prefix(">=")
        .or_else(|| value.strip_prefix("<="))
        .or_else(|| value.strip_prefix('>'))
        .or_else(|| value.strip_prefix('<'))
        .unwrap_or(value)
        .trim();
    let digits_end = rest
        .find(|ch: char| !(ch.is_ascii_digit() || ch == '.'))
        .unwrap_or(rest.len());
    if digits_end == 0 {
        return Err(());
    }
    let number: f64 = rest[..digits_end].parse().map_err(|_| ())?;
    if !number.is_finite() {
        return Err(());
    }
    let unit = rest[digits_end..].trim().to_ascii_lowercase();
    let scale: f64 = match unit.as_str() {
        "" | "b" => 1.0,
        "k" => 1024.0,
        "m" => 1_048_576.0,
        "g" => 1_073_741_824.0,
        "t" => 1_099_511_627_776.0,
        _ => return Err(()),
    };
    Ok((number * scale) as i64)
}

fn apply_size_filter(value: &str) -> Result<(Option<i64>, Option<i64>), ()> {
    let value = value.trim();
    if value.contains('-') && !value.starts_with('>') && !value.starts_with('<') {
        let (left, right) = value.split_once('-').ok_or(())?;
        return Ok((Some(parse_size_value(left)?), Some(parse_size_value(right)?)));
    }
    let number = parse_size_value(value)?;
    if value.starts_with(">") && !value.starts_with(">=") {
        return Ok((Some(number + 1), None));
    }
    if value.starts_with(">=") {
        return Ok((Some(number), None));
    }
    if value.starts_with("<") && !value.starts_with("<=") {
        return Ok((None, Some((number - 1).max(0))));
    }
    if value.starts_with("<=") {
        return Ok((None, Some(number)));
    }
    Ok((Some(number), None))
}

fn parse_date_value(raw: &str) -> Result<i64, ()> {
    let value = raw.trim();
    if !value.is_empty() && value.bytes().all(|byte| byte.is_ascii_digit()) {
        return value.parse::<i64>().map_err(|_| ());
    }
    for format in ["%Y-%m-%d", "%Y/%m/%d"] {
        if let Ok(date) = chrono::NaiveDate::parse_from_str(value, format) {
            if let Some(local) = date.and_hms_opt(0, 0, 0) {
                if let Some(stamped) = chrono::Local.from_local_datetime(&local).single() {
                    return Ok(stamped.timestamp());
                }
                if let Some(stamped) = chrono::Local.from_local_datetime(&local).earliest() {
                    return Ok(stamped.timestamp());
                }
            }
        }
    }
    Err(())
}

fn parse_query(query: &str) -> ParsedQuery {
    let mut parsed = ParsedQuery::default();
    for raw in tokenize_query(query) {
        let token = clean_token(&raw);
        if token.is_empty() {
            continue;
        }
        let Some((raw_key, raw_value)) = token.split_once(':') else {
            parsed.terms.push(token);
            continue;
        };
        let key = raw_key.trim().to_lowercase();
        let value = clean_token(raw_value);
        let mut handled = true;
        match key.as_str() {
            "ext" | "extension" | "扩展名" => parsed.ext = Some(value),
            "size" | "大小" => match apply_size_filter(&value) {
                Ok((min_size, max_size)) => {
                    parsed.min_size = min_size;
                    parsed.max_size = max_size;
                }
                Err(()) => handled = false,
            },
            "kind" | "type" | "类型" => match value.to_lowercase().as_str() {
                "file" | "文件" => parsed.kind = Some("file".to_owned()),
                "dir" | "folder" | "目录" | "文件夹" => parsed.kind = Some("dir".to_owned()),
                _ => handled = false,
            },
            "after" | "since" | "之后" => match parse_date_value(&value) {
                Ok(stamp) => parsed.after = Some(stamp),
                Err(()) => handled = false,
            },
            "before" | "until" | "之前" => match parse_date_value(&value) {
                Ok(stamp) => parsed.before = Some(stamp),
                Err(()) => handled = false,
            },
            "path" | "scope" | "路径" => parsed.scope = Some(value),
            "volume" | "drive" | "盘符" | "卷" => {
                let letter = value
                    .to_uppercase()
                    .trim_matches(|ch| ch == ':' || ch == '\\')
                    .to_owned();
                let mut chars = letter.chars();
                match (chars.next(), chars.next()) {
                    (Some(head), None) if head.is_ascii_alphabetic() => {
                        parsed.scope = Some(format!("{head}:\\"));
                    }
                    _ => handled = false,
                }
            }
            "sort" | "排序" => parsed.sort = Some(sort_alias(&value)),
            "limit" | "数量" => match value.trim().parse::<i64>() {
                Ok(number) => parsed.limit = Some(number.max(0)),
                Err(_) => handled = false,
            },
            _ => handled = false,
        }
        if !handled {
            parsed.terms.push(token);
        }
    }
    parsed
}

/// 目录范围收口：返回（目录自身，带尾分隔符的前缀）。兄弟目录 D:\foobar
/// 不会被 D:\foo 的 LIKE 前缀一并收进来，与 diff 的 scope 口径一致。
fn scope_bounds(scope: &str) -> Option<(String, String)> {
    let mut root = scope.replace('/', "\\");
    root = root.trim_end_matches('\\').to_owned();
    if root.is_empty() {
        return None;
    }
    if root.len() == 2 && root.as_bytes().get(1) == Some(&b':') {
        root.push('\\');
    }
    let prefix = if root.ends_with('\\') {
        root.clone()
    } else {
        format!("{root}\\")
    };
    Some((root, prefix))
}

#[derive(Clone, Debug)]
pub struct SearchRequest {
    pub query: String,
    pub limit: i64,
    pub offset: i64,
    pub scope: Option<String>,
    pub snapshot: String,
    pub kind: String,
    pub sort: Option<String>,
    pub ext: Option<String>,
    pub min_size: Option<i64>,
    pub max_size: Option<i64>,
    pub after: Option<i64>,
    pub before: Option<i64>,
}

impl Default for SearchRequest {
    fn default() -> Self {
        Self {
            query: String::new(),
            limit: 100,
            offset: 0,
            scope: None,
            snapshot: "latest".to_owned(),
            kind: "all".to_owned(),
            sort: None,
            ext: None,
            min_size: None,
            max_size: None,
            after: None,
            before: None,
        }
    }
}

pub fn search(data_root: &Path, request: &SearchRequest) -> Result<Json, CoreError> {
    let started = Instant::now();
    let parsed = parse_query(&request.query);
    let terms = parsed.terms.clone();
    let ext = request.ext.clone().or_else(|| parsed.ext.clone());
    let min_size = request.min_size.or(parsed.min_size);
    let max_size = request.max_size.or(parsed.max_size);
    let after = request.after.or(parsed.after);
    let before = request.before.or(parsed.before);
    let scope = request
        .scope
        .clone()
        .filter(|value| !value.is_empty())
        .or_else(|| parsed.scope.clone());
    let kind = if request.kind != "all" {
        request.kind.clone()
    } else {
        parsed.kind.clone().unwrap_or_else(|| "all".to_owned())
    };
    let sort = parsed
        .sort
        .clone()
        .or_else(|| request.sort.clone())
        .unwrap_or_else(|| "smart".to_owned());
    let limit = clamp_limit(parsed.limit.unwrap_or(request.limit));
    let offset = request.offset.max(0);

    let nothing = terms.is_empty()
        && ext.is_none()
        && min_size.is_none()
        && max_size.is_none()
        && after.is_none()
        && before.is_none()
        && kind == "all"
        && scope.is_none();
    if nothing {
        return Err(CoreError::new("INVALID_ARGUMENT", "检索词不能为空")
            .hint("例：--json search \"报告 ext:pdf size:>10m\" --limit 20"));
    }

    let path = resolve_snapshot(data_root, &request.snapshot)?;
    let conn = open_snapshot(&path)?;
    let has_ext = table_has_column(&conn, "entries", "ext");

    let mut clauses: Vec<String> = Vec::new();
    let mut params: Vec<Value> = Vec::new();
    for term in &terms {
        clauses.push("path LIKE ? ESCAPE '\\'".to_owned());
        params.push(Value::Text(format!("%{}%", like_literal(term))));
    }
    if let Some(scope) = scope.as_deref() {
        if let Some((root, prefix)) = scope_bounds(scope) {
            clauses.push("(path = ? OR path LIKE ? ESCAPE '\\')".to_owned());
            params.push(Value::Text(root));
            params.push(Value::Text(format!("{}%", like_literal(&prefix))));
        }
    }
    if kind == "file" {
        clauses.push("is_dir = 0".to_owned());
    } else if kind == "dir" {
        clauses.push("is_dir = 1".to_owned());
    }
    if let Some(ext) = ext.as_deref() {
        let wanted: Vec<String> = ext
            .split(',')
            .map(|part| part.trim().to_lowercase().trim_start_matches('.').to_owned())
            .filter(|part| !part.is_empty())
            .collect();
        if !wanted.is_empty() {
            if has_ext {
                let holes = vec!["?"; wanted.len()].join(",");
                clauses.push(format!("ext IN ({holes})"));
                params.extend(wanted.into_iter().map(Value::Text));
            } else {
                let likes = vec!["path LIKE ? ESCAPE '\\'"; wanted.len()].join(" OR ");
                clauses.push(format!("({likes})"));
                params.extend(
                    wanted
                        .into_iter()
                        .map(|part| Value::Text(format!("%.{}", like_literal(&part)))),
                );
            }
        }
    }
    if let Some(min_size) = min_size {
        clauses.push("size >= ?".to_owned());
        params.push(Value::Integer(min_size));
    }
    if let Some(max_size) = max_size {
        clauses.push("size <= ?".to_owned());
        params.push(Value::Integer(max_size));
    }
    if let Some(after) = after {
        clauses.push("mtime >= ?".to_owned());
        params.push(Value::Integer(after));
    }
    if let Some(before) = before {
        clauses.push("mtime <= ?".to_owned());
        params.push(Value::Integer(before));
    }
    let where_sql = if clauses.is_empty() {
        "1=1".to_owned()
    } else {
        clauses.join(" AND ")
    };

    let ext_expr = if has_ext { "ext" } else { "''" };
    let order = sort_sql(&sort, has_ext);
    // 与 Python 同一条语句：COUNT(*) OVER () 顺带带回总数，省掉一次全表 LIKE 扫描。
    // 与参考实现一致：offset 越界时没有行，total 也就是 0（历史行为，不单独修正）。
    let sql = format!(
        "SELECT path, size, mtime, is_dir, {ext_expr}, COUNT(*) OVER () FROM entries \
         WHERE {where_sql} ORDER BY {order} LIMIT ? OFFSET ?"
    );
    let mut page_params = params.clone();
    page_params.push(Value::Integer(limit));
    page_params.push(Value::Integer(offset));
    let mut stmt = conn
        .prepare(&sql)
        .map_err(|error| CoreError::new("SQL_ERROR", format!("检索语句准备失败：{error}")))?;
    let rows = stmt
        .query_map(params_from_iter(page_params.iter()), |row| {
            Ok((
                Json::obj(vec![
                    ("path", jstr(row.get::<_, String>(0)?)),
                    ("size", jnum(row.get::<_, i64>(1)?)),
                    ("mtime", jnum(row.get::<_, i64>(2)?)),
                    ("is_dir", jbool(row.get::<_, i64>(3)? != 0)),
                    ("ext", jstr(row.get::<_, Option<String>>(4)?.unwrap_or_default())),
                ]),
                row.get::<_, i64>(5)?,
            ))
        })
        .map_err(|error| CoreError::new("SQL_ERROR", format!("检索语句执行失败：{error}")))?;
    let mut items: Vec<Json> = Vec::new();
    let mut total = 0i64;
    for (item, count) in rows.flatten() {
        if items.is_empty() {
            total = count;
        }
        items.push(item);
    }
    let more = (offset + items.len() as i64) < total;

    Ok(Json::obj(vec![
        ("query", jstr(request.query.clone())),
        ("snapshot", jstr(request.snapshot.clone())),
        ("total", jnum(total)),
        ("items", Json::Arr(items)),
        ("more", jbool(more)),
        ("offset", jnum(offset)),
        ("limit", jnum(limit)),
        ("mode", jstr("snapshot")),
        ("terms", Json::Arr(terms.iter().cloned().map(jstr).collect())),
        ("sort", jstr(sort)),
        ("seconds", Json::Real(started.elapsed().as_secs_f64())),
        ("engine_available", jbool(false)),
        ("engine_source", jstr("snapshot")),
        ("orchestration", Json::obj(Vec::new())),
    ]))
}

// ---------------------------------------------------------------------------
// 目录聚合（du）与排行（top）
// ---------------------------------------------------------------------------

#[derive(Clone)]
struct FolderItem {
    path: String,
    size: i64,
    file_count: i64,
    depth: i64,
}

/// 卷根识别：盘符根 D:\ 与 GUID 根 \\?\Volume{...}\（同 Python paths.is_volume_root）。
fn is_volume_root(path: &str) -> bool {
    let bytes = path.as_bytes();
    if bytes.len() == 3 && bytes[1] == b':' && bytes[2] == b'\\' {
        return true;
    }
    if let Some(rest) = path.strip_prefix("\\\\?\\Volume{") {
        if let Some(name) = rest.strip_suffix("}\\") {
            return !name.is_empty() && !name.contains('}');
        }
    }
    false
}

/// 与 Python api._tree_v2 同一口径：盘符补尾斜杠，普通目录去尾斜杠。
fn normalize_tree_root(path: &str) -> String {
    if path.len() == 2 && path.as_bytes().get(1) == Some(&b':') {
        return format!("{path}\\");
    }
    if is_volume_root(path) {
        return path.to_owned();
    }
    path.trim_end_matches('\\').to_owned()
}

/// v1/v2 老快照没有 folder_sizes.parent_path，退化成前缀扫描（同 Python _tree_v1）。
fn tree_v1(
    conn: &Connection,
    path: &str,
    depth: i64,
) -> Result<Vec<FolderItem>, CoreError> {
    let mut items = Vec::new();
    if !path.is_empty() {
        let prefix = if is_volume_root(path) {
            path.to_owned()
        } else {
            path.trim_end_matches('\\').to_owned()
        };
        let child = if prefix.ends_with('\\') {
            prefix.clone()
        } else {
            format!("{prefix}\\")
        };
        let child_len = child.chars().count() as i64;
        let mut stmt = conn
            .prepare(
                "SELECT path, size, file_count FROM folder_sizes \
                 WHERE path = ?1 OR substr(path, 1, ?2) = ?3 ORDER BY size DESC",
            )
            .map_err(|error| CoreError::new("SQL_ERROR", error.to_string()))?;
        let rows = stmt
            .query_map(params_from_iter([
                Value::Text(prefix.clone()),
                Value::Integer(child_len),
                Value::Text(child),
            ].iter()), |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    row.get::<_, i64>(1)?,
                    row.get::<_, i64>(2)?,
                ))
            })
            .map_err(|error| CoreError::new("SQL_ERROR", error.to_string()))?;
        let prefix_len = prefix.chars().count();
        for (item_path, size, file_count) in rows.flatten() {
            let tail: String = item_path.chars().skip(prefix_len).collect();
            let tail = tail.trim_start_matches('\\');
            let level = if item_path == prefix {
                0
            } else {
                tail.chars().filter(|ch| *ch == '\\').count() as i64 + 1
            };
            if level > depth {
                continue;
            }
            items.push(FolderItem {
                path: item_path,
                size,
                file_count,
                depth: level,
            });
        }
        return Ok(items);
    }
    let mut stmt = conn
        .prepare(
            "SELECT path, size, file_count FROM folder_sizes \
             WHERE length(path) <= 3 OR path LIKE '%Volume{%' ORDER BY size DESC",
        )
        .map_err(|error| CoreError::new("SQL_ERROR", error.to_string()))?;
    let rows = stmt
        .query_map([], |row| {
            Ok((
                row.get::<_, String>(0)?,
                row.get::<_, i64>(1)?,
                row.get::<_, i64>(2)?,
            ))
        })
        .map_err(|error| CoreError::new("SQL_ERROR", error.to_string()))?;
    for (item_path, size, file_count) in rows.flatten() {
        if !is_volume_root(&item_path) {
            continue;
        }
        items.push(FolderItem {
            path: item_path,
            size,
            file_count,
            depth: 0,
        });
    }
    Ok(items)
}

/// 目录自身 mtime：folder_sizes 不存时间，按主键回查 entries（同 Python _dir_mtimes）。
fn dir_mtimes(conn: &Connection, paths: &[String]) -> HashMap<String, i64> {
    let mut marks = HashMap::new();
    for chunk in paths.chunks(400) {
        let values: Vec<Value> = chunk.iter().cloned().map(Value::Text).collect();
        let holes = vec!["?"; chunk.len()].join(",");
        let sql = format!("SELECT path, mtime FROM entries WHERE path IN ({holes})");
        let Ok(mut stmt) = conn.prepare(&sql) else {
            continue;
        };
        let Ok(rows) = stmt.query_map(params_from_iter(values.iter()), |row| {
            Ok((row.get::<_, String>(0)?, row.get::<_, i64>(1)?))
        }) else {
            continue;
        };
        for (path, mtime) in rows.flatten() {
            marks.insert(path, mtime);
        }
    }
    marks
}

pub fn du(
    data_root: &Path,
    snapshot: &str,
    path: &str,
    depth: i64,
    limit: i64,
) -> Result<Json, CoreError> {
    let raw_path = path.to_owned();
    let normalized = path.replace('/', "\\");
    let depth = depth.max(0);
    let limit = clamp_limit(limit) as usize;
    let file = resolve_snapshot(data_root, snapshot)?;
    let conn = open_snapshot(&file)?;
    let has_parent = table_has_column(&conn, "folder_sizes", "parent_path");

    let items = if has_parent {
        if normalized.is_empty() {
            let mut stmt = conn
                .prepare(
                    "SELECT path, size, file_count FROM folder_sizes \
                     WHERE parent_path = '' ORDER BY size DESC",
                )
                .map_err(|error| CoreError::new("SQL_ERROR", error.to_string()))?;
            let rows = stmt
                .query_map([], |row| {
                    Ok(FolderItem {
                        path: row.get(0)?,
                        size: row.get(1)?,
                        file_count: row.get(2)?,
                        depth: 0,
                    })
                })
                .map_err(|error| CoreError::new("SQL_ERROR", error.to_string()))?;
            rows.flatten().collect::<Vec<_>>()
        } else {
            let root = normalize_tree_root(&normalized);
            let mut stmt = conn
                .prepare(
                    "WITH RECURSIVE tree(path, size, file_count, level) AS ( \
                       SELECT path, size, file_count, 0 FROM folder_sizes WHERE path = ?1 \
                       UNION ALL \
                       SELECT f.path, f.size, f.file_count, t.level + 1 \
                       FROM folder_sizes AS f JOIN tree AS t ON f.parent_path = t.path \
                       WHERE t.level < ?2 \
                     ) SELECT path, size, file_count, level FROM tree ORDER BY size DESC",
                )
                .map_err(|error| CoreError::new("SQL_ERROR", error.to_string()))?;
            let rows = stmt
                .query_map(
                    params_from_iter([Value::Text(root), Value::Integer(depth)].iter()),
                    |row| {
                        Ok(FolderItem {
                            path: row.get(0)?,
                            size: row.get(1)?,
                            file_count: row.get(2)?,
                            depth: row.get(3)?,
                        })
                    },
                )
                .map_err(|error| CoreError::new("SQL_ERROR", error.to_string()))?;
            rows.flatten().collect::<Vec<_>>()
        }
    } else {
        tree_v1(&conn, &normalized, depth)?
    };

    let total = items.len() as i64;
    let visible: Vec<FolderItem> = items.into_iter().take(limit).collect();
    let dir_paths: Vec<String> = visible.iter().map(|item| item.path.clone()).collect();
    let marks = if dir_paths.is_empty() {
        HashMap::new()
    } else {
        dir_mtimes(&conn, &dir_paths)
    };
    let json_items = visible
        .iter()
        .map(|item| {
            Json::obj(vec![
                ("path", jstr(item.path.clone())),
                ("size", jnum(item.size)),
                ("file_count", jnum(item.file_count)),
                ("is_dir", jbool(true)),
                ("depth", jnum(item.depth)),
                (
                    "mtime",
                    jnum(marks.get(&item.path).copied().unwrap_or(0)),
                ),
            ])
        })
        .collect();

    Ok(Json::obj(vec![
        ("snapshot", jstr(snapshot)),
        ("path", jstr(raw_path)),
        ("items", Json::Arr(json_items)),
        ("total", jnum(total)),
    ]))
}

pub fn top(
    data_root: &Path,
    snapshot: &str,
    limit: i64,
    files_only: bool,
    by: &str,
) -> Result<Json, CoreError> {
    if by != "size" && by != "count" {
        return Err(CoreError::new(
            "INVALID_ARGUMENT",
            format!("未知排序字段：{by}"),
        ));
    }
    let limit = clamp_limit(limit);
    let file = resolve_snapshot(data_root, snapshot)?;
    let conn = open_snapshot(&file)?;
    let items: Vec<Json> = if files_only {
        let mut stmt = conn
            .prepare(
                "SELECT path, size, mtime FROM entries WHERE is_dir = 0 \
                 ORDER BY size DESC LIMIT ?1",
            )
            .map_err(|error| CoreError::new("SQL_ERROR", error.to_string()))?;
        let rows = stmt
            .query_map([limit], |row| {
                Ok(Json::obj(vec![
                    ("path", jstr(row.get::<_, String>(0)?)),
                    ("size", jnum(row.get::<_, i64>(1)?)),
                    ("mtime", jnum(row.get::<_, i64>(2)?)),
                ]))
            })
            .map_err(|error| CoreError::new("SQL_ERROR", error.to_string()))?;
        rows.flatten().collect()
    } else {
        let column = if by == "count" { "file_count" } else { "size" };
        let mut stmt = conn
            .prepare(&format!(
                "SELECT path, size, file_count FROM folder_sizes \
                 ORDER BY {column} DESC LIMIT ?1"
            ))
            .map_err(|error| CoreError::new("SQL_ERROR", error.to_string()))?;
        let rows = stmt
            .query_map([limit], |row| {
                Ok(Json::obj(vec![
                    ("path", jstr(row.get::<_, String>(0)?)),
                    ("size", jnum(row.get::<_, i64>(1)?)),
                    ("file_count", jnum(row.get::<_, i64>(2)?)),
                ]))
            })
            .map_err(|error| CoreError::new("SQL_ERROR", error.to_string()))?;
        rows.flatten().collect()
    };

    Ok(Json::obj(vec![
        ("snapshot", jstr(snapshot)),
        ("by", jstr(by)),
        ("items", Json::Arr(items)),
    ]))
}

// ---------------------------------------------------------------------------
// 快照对比（diff）
// ---------------------------------------------------------------------------

#[derive(Clone)]
struct Change {
    path: String,
    is_dir: bool,
    kind: &'static str,
    old_size: Option<i64>,
    new_size: Option<i64>,
    delta: i64,
    old_mtime: Option<i64>,
    new_mtime: Option<i64>,
}

impl Change {
    fn to_json(&self) -> Json {
        Json::obj(vec![
            ("path", jstr(self.path.clone())),
            ("is_dir", jbool(self.is_dir)),
            ("kind", jstr(self.kind)),
            ("old_size", self.old_size.map(jnum).unwrap_or(Json::Null)),
            ("new_size", self.new_size.map(jnum).unwrap_or(Json::Null)),
            ("delta", jnum(self.delta)),
            ("old_mtime", self.old_mtime.map(jnum).unwrap_or(Json::Null)),
            ("new_mtime", self.new_mtime.map(jnum).unwrap_or(Json::Null)),
        ])
    }
}

struct Candidate {
    abs: i64,
    seq: u64,
    change: Change,
}

impl PartialEq for Candidate {
    fn eq(&self, other: &Self) -> bool {
        self.abs == other.abs && self.seq == other.seq
    }
}

impl Eq for Candidate {}

impl Ord for Candidate {
    fn cmp(&self, other: &Self) -> Ordering {
        self.abs
            .cmp(&other.abs)
            .then_with(|| self.seq.cmp(&other.seq))
    }
}

impl PartialOrd for Candidate {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}

/// 只保留 |delta| 最大的 limit 条：真实快照的变更随手就是几十万条，
/// 全量物化再排序会把 RSS 顶上去。与 Python _TopChanges 同一取舍与同序。
struct TopChanges {
    limit: i64,
    kind: String,
    heap: BinaryHeap<Reverse<Candidate>>,
    seq: u64,
}

impl TopChanges {
    fn new(limit: i64, kind: &str) -> Self {
        Self {
            limit: limit.max(0),
            kind: kind.to_owned(),
            heap: BinaryHeap::new(),
            seq: 0,
        }
    }

    fn add(&mut self, change: Change) {
        if self.limit <= 0 {
            return;
        }
        if self.kind != "all" && change.kind != self.kind {
            return;
        }
        let abs = change.delta.abs();
        let candidate = Candidate {
            abs,
            seq: self.seq,
            change,
        };
        self.seq += 1;
        if (self.heap.len() as i64) < self.limit {
            self.heap.push(Reverse(candidate));
            return;
        }
        if let Some(Reverse(smallest)) = self.heap.peek() {
            if abs > smallest.abs {
                self.heap.pop();
                self.heap.push(Reverse(candidate));
            }
        }
    }

    fn into_sorted(self) -> Vec<Change> {
        let mut items: Vec<Candidate> = self
            .heap
            .into_iter()
            .map(|Reverse(candidate)| candidate)
            .collect();
        items.sort_by(|left, right| {
            right
                .abs
                .cmp(&left.abs)
                .then_with(|| left.seq.cmp(&right.seq))
        });
        items.into_iter().map(|candidate| candidate.change).collect()
    }
}

/// 与 Python diff.normalize_scope 一致：正斜杠、尾部分隔符、盘符根。
fn normalize_scope(scope: &str) -> String {
    let mut root = scope.replace('/', "\\");
    root = root.trim_end_matches('\\').to_owned();
    if root.len() == 2 && root.as_bytes().get(1) == Some(&b':') {
        root.push('\\');
    }
    root
}

/// 把 scope 变成可用主键的字符串区间 [lower, upper)；兄弟目录由 in_scope 剔除。
fn scope_range(scope: &str) -> Option<(String, String)> {
    let mut chars = scope.char_indices().rev();
    let (index, last) = chars.next()?;
    let next = char::from_u32(last as u32 + 1)?;
    Some((
        scope.to_owned(),
        format!("{}{next}", &scope[..index]),
    ))
}

fn in_scope(path: &str, scope: &str) -> bool {
    if scope.is_empty() {
        return true;
    }
    if !path.starts_with(scope) {
        return false;
    }
    if scope.ends_with('\\') || path.len() == scope.len() {
        return true;
    }
    path.as_bytes().get(scope.len()) == Some(&b'\\')
}

struct Entry {
    path: String,
    is_dir: bool,
    size: i64,
    mtime: i64,
}

fn sql_error(error: rusqlite::Error) -> CoreError {
    CoreError::new("SQL_ERROR", error.to_string())
}

fn next_entry(rows: &mut rusqlite::Rows<'_>) -> Result<Option<Entry>, CoreError> {
    match rows.next() {
        Ok(Some(row)) => Ok(Some(Entry {
            path: row.get(0).map_err(sql_error)?,
            is_dir: row.get::<_, i64>(1).map_err(sql_error)? != 0,
            size: row.get(2).map_err(sql_error)?,
            mtime: row.get(3).map_err(sql_error)?,
        })),
        Ok(None) => Ok(None),
        Err(error) => Err(sql_error(error)),
    }
}

/// 只读 ATTACH 用 URI 文件名：主连接开 URI 标志后，ATTACH 的 mode=ro 才生效。
fn uri_path(path: &Path) -> String {
    let text = path.to_string_lossy().replace('\\', "/");
    let mut out = String::with_capacity(text.len() + 8);
    for ch in text.chars() {
        match ch {
            '%' => out.push_str("%25"),
            '?' => out.push_str("%3F"),
            '#' => out.push_str("%23"),
            ' ' => out.push_str("%20"),
            other => out.push(other),
        }
    }
    out
}

fn attach_readonly(conn: &Connection, alias: &str, path: &Path) -> Result<(), CoreError> {
    let uri = format!("file:{}?mode=ro", uri_path(path));
    conn.execute(&format!("ATTACH DATABASE ?1 AS {alias}"), [uri])
        .map_err(|error| {
            CoreError::new(
                "SNAPSHOT_OPEN_FAILED",
                format!("只读挂载快照 {} 失败：{error}", path.display()),
            )
            .hint(BUILD_HINT)
        })?;
    Ok(())
}

pub fn diff(
    data_root: &Path,
    old: Option<&str>,
    new: Option<&str>,
    limit: i64,
    scope: &str,
    kind: &str,
) -> Result<Json, CoreError> {
    if !matches!(kind, "all" | "added" | "removed" | "grown" | "shrunk") {
        return Err(CoreError::new(
            "INVALID_ARGUMENT",
            format!("未知 diff 分类：{kind}"),
        ));
    }
    let snapshots = list_snapshots(data_root);
    if snapshots.len() < 2 && !(old.is_some() && new.is_some()) {
        return Err(snapshot_error("至少需要两份快照").hint("先再建一份快照，或显式给出 --old/--new"));
    }
    let old_label_arg = old.unwrap_or("latest-1");
    let new_label_arg = new.unwrap_or("latest");
    let old_path = resolve_snapshot(data_root, old_label_arg)?;
    let new_path = resolve_snapshot(data_root, new_label_arg)?;
    let old_label = old_path
        .file_stem()
        .and_then(|value| value.to_str())
        .unwrap_or(old_label_arg)
        .to_owned();
    let new_label = new_path
        .file_stem()
        .and_then(|value| value.to_str())
        .unwrap_or(new_label_arg)
        .to_owned();

    // 两份快照挂进同一个内存连接：FULL OUTER JOIN 用「两侧 ORDER BY path 游标归并」
    // 等价实现——语义相同，但是纯顺序 IO，不会让 SQLite 把 400 万行物化成临时表。
    let conn = Connection::open_with_flags(
        ":memory:",
        OpenFlags::SQLITE_OPEN_READ_WRITE
            | OpenFlags::SQLITE_OPEN_CREATE
            | OpenFlags::SQLITE_OPEN_URI,
    )
    .map_err(|error| CoreError::new("INTERNAL_ERROR", format!("打开内存库失败：{error}")))?;
    attach_readonly(&conn, "old", &old_path)?;
    attach_readonly(&conn, "new", &new_path)?;
    let _ = conn.execute_batch(&format!(
        "PRAGMA old.cache_size=-65536; PRAGMA new.cache_size=-65536; \
         PRAGMA old.mmap_size={QUERY_MMAP_BYTES}; PRAGMA new.mmap_size={QUERY_MMAP_BYTES};"
    ));

    let scope = normalize_scope(scope);
    let limit = clamp_limit(limit);
    let bound = if scope.is_empty() {
        None
    } else {
        scope_range(&scope)
    };
    let (old_sql, new_sql) = match &bound {
        Some(_) => (
            "SELECT path, is_dir, size, mtime FROM old.entries \
             WHERE path >= ?1 AND path < ?2 ORDER BY path",
            "SELECT path, is_dir, size, mtime FROM new.entries \
             WHERE path >= ?1 AND path < ?2 ORDER BY path",
        ),
        None => (
            "SELECT path, is_dir, size, mtime FROM old.entries ORDER BY path",
            "SELECT path, is_dir, size, mtime FROM new.entries ORDER BY path",
        ),
    };
    let bound_params: Vec<Value> = match &bound {
        Some((lower, upper)) => vec![Value::Text(lower.clone()), Value::Text(upper.clone())],
        None => Vec::new(),
    };
    let mut old_stmt = conn.prepare(old_sql).map_err(sql_error)?;
    let mut new_stmt = conn.prepare(new_sql).map_err(sql_error)?;
    let mut old_rows = old_stmt
        .query(params_from_iter(bound_params.iter()))
        .map_err(sql_error)?;
    let mut new_rows = new_stmt
        .query(params_from_iter(bound_params.iter()))
        .map_err(sql_error)?;
    let mut left = next_entry(&mut old_rows)?;
    let mut right = next_entry(&mut new_rows)?;

    let mut added = 0i64;
    let mut removed = 0i64;
    let mut grown = 0i64;
    let mut shrunk = 0i64;
    let mut total_delta = 0i64;
    let mut top_changes = TopChanges::new(limit, kind);
    let scoped = !scope.is_empty();

    while left.is_some() || right.is_some() {
        let ordering = match (&left, &right) {
            (Some(left), Some(right)) => left.path.cmp(&right.path),
            (Some(_), None) => Ordering::Less,
            (None, Some(_)) => Ordering::Greater,
            (None, None) => break,
        };
        match ordering {
            Ordering::Less => {
                let Some(entry) = left.take() else { break };
                if !scoped || in_scope(&entry.path, &scope) {
                    removed += 1;
                    total_delta -= entry.size;
                    top_changes.add(Change {
                        path: entry.path,
                        is_dir: entry.is_dir,
                        kind: "removed",
                        old_size: Some(entry.size),
                        new_size: None,
                        delta: -entry.size,
                        old_mtime: Some(entry.mtime),
                        new_mtime: None,
                    });
                }
                left = next_entry(&mut old_rows)?;
            }
            Ordering::Greater => {
                let Some(entry) = right.take() else { break };
                if !scoped || in_scope(&entry.path, &scope) {
                    added += 1;
                    total_delta += entry.size;
                    top_changes.add(Change {
                        path: entry.path,
                        is_dir: entry.is_dir,
                        kind: "added",
                        old_size: None,
                        new_size: Some(entry.size),
                        delta: entry.size,
                        old_mtime: None,
                        new_mtime: Some(entry.mtime),
                    });
                }
                right = next_entry(&mut new_rows)?;
            }
            Ordering::Equal => {
                let (Some(old_entry), Some(new_entry)) = (left.take(), right.take()) else {
                    break;
                };
                let changed = old_entry.size != new_entry.size;
                if changed && (!scoped || in_scope(&old_entry.path, &scope)) {
                    let delta = new_entry.size - old_entry.size;
                    let (kind, counter) = if delta > 0 {
                        ("grown", &mut grown)
                    } else {
                        ("shrunk", &mut shrunk)
                    };
                    *counter += 1;
                    total_delta += delta;
                    top_changes.add(Change {
                        path: old_entry.path,
                        is_dir: new_entry.is_dir,
                        kind,
                        old_size: Some(old_entry.size),
                        new_size: Some(new_entry.size),
                        delta,
                        old_mtime: Some(old_entry.mtime),
                        new_mtime: Some(new_entry.mtime),
                    });
                }
                left = next_entry(&mut old_rows)?;
                right = next_entry(&mut new_rows)?;
            }
        }
    }

    let changes = top_changes.into_sorted();
    let returned = changes.len() as i64;
    Ok(Json::obj(vec![
        ("old", jstr(old_label)),
        ("new", jstr(new_label)),
        ("scope", jstr(scope)),
        ("kind", jstr(kind)),
        (
            "counts",
            Json::obj(vec![
                ("added", jnum(added)),
                ("removed", jnum(removed)),
                ("grown", jnum(grown)),
                ("shrunk", jnum(shrunk)),
            ]),
        ),
        ("total_delta", jnum(total_delta)),
        ("returned", jnum(returned)),
        (
            "changes",
            Json::Arr(changes.iter().map(Change::to_json).collect()),
        ),
        ("strategy", jstr("merge")),
    ]))
}

// ---------------------------------------------------------------------------
// 只读 SQL：词法白名单 + 单语句 + 语句超时
// ---------------------------------------------------------------------------

const SQL_WRITE_KEYWORDS: [&str; 17] = [
    "delete",
    "insert",
    "update",
    "drop",
    "create",
    "alter",
    "attach",
    "detach",
    "vacuum",
    "pragma",
    "reindex",
    "load_extension",
    "writefile",
    "readfile",
    "edit",
    "fts3_tokenizer",
    "replace",
];

/// 把字符串字面量、带引号标识符、注释替换成空格，保留词边界。
/// 这一步让 delete/**/from 变成真实的空白，也让 SELECT ';' 不算多语句（同 sqlguard.py）。
fn strip_literals(query: &str) -> String {
    let bytes = query.as_bytes();
    let mut out = String::with_capacity(query.len());
    let mut index = 0;
    while index < bytes.len() {
        match bytes[index] {
            quote @ (b'\'' | b'"' | b'\x60') => {
                index += 1;
                while index < bytes.len() {
                    if bytes[index] == quote {
                        if index + 1 < bytes.len() && bytes[index + 1] == quote {
                            index += 2;
                            continue;
                        }
                        index += 1;
                        break;
                    }
                    index += 1;
                }
                out.push(' ');
            }
            b'[' => {
                index += 1;
                while index < bytes.len() && bytes[index] != b']' {
                    index += 1;
                }
                if index < bytes.len() {
                    index += 1;
                }
                out.push(' ');
            }
            b'-' if index + 1 < bytes.len() && bytes[index + 1] == b'-' => {
                while index < bytes.len() && bytes[index] != b'\n' {
                    index += 1;
                }
                out.push(' ');
            }
            b'/' if index + 1 < bytes.len() && bytes[index + 1] == b'*' => {
                index += 2;
                while index + 1 < bytes.len()
                    && !(bytes[index] == b'*' && bytes[index + 1] == b'/')
                {
                    index += 1;
                }
                index = (index + 2).min(bytes.len());
                out.push(' ');
            }
            _ => {
                let Some(ch) = query[index..].chars().next() else {
                    break;
                };
                out.push(ch);
                index += ch.len_utf8();
            }
        }
    }
    out
}

/// 词法扫描出的标识符及其结束位置（用于 replace 的函数调用例外判断）。
fn ident_tokens(text: &str) -> Vec<(String, usize)> {
    let mut tokens = Vec::new();
    let mut current = String::new();
    for (index, ch) in text.char_indices() {
        if ch.is_alphanumeric() || ch == '_' {
            current.push(ch);
        } else if !current.is_empty() {
            let token = std::mem::take(&mut current);
            tokens.push((token, index));
        }
    }
    if !current.is_empty() {
        tokens.push((current, text.len()));
    }
    tokens
}

fn validate_sql(query: &str) -> Result<(), CoreError> {
    if query.trim().is_empty() {
        return Err(CoreError::new("INVALID_ARGUMENT", "缺少 SQL 语句"));
    }
    let mut core = strip_literals(query);
    let trimmed = core.trim_end().len();
    core.truncate(trimmed);
    if core.ends_with(';') {
        core.pop();
    }
    let core = core.trim().to_owned();
    if core.contains(';') {
        return Err(CoreError::new("SQL_REJECTED", "拒绝多语句"));
    }
    let tokens = ident_tokens(&core);
    let Some((first, _)) = tokens.first() else {
        return Err(CoreError::new("INVALID_ARGUMENT", "缺少 SQL 语句"));
    };
    if !matches!(
        first.to_ascii_lowercase().as_str(),
        "select" | "with" | "explain"
    ) {
        return Err(CoreError::new(
            "SQL_REJECTED",
            "只允许 SELECT / WITH / EXPLAIN",
        ));
    }
    for (token, end) in &tokens {
        let keyword = token.to_ascii_lowercase();
        if !SQL_WRITE_KEYWORDS.contains(&keyword.as_str()) {
            continue;
        }
        if keyword == "replace" {
            // replace(a, b, c) 是只读字符串函数，只拒绝非函数调用形式。
            let rest = core[*end..].trim_start();
            if rest.starts_with('(') {
                continue;
            }
        }
        return Err(CoreError::new(
            "SQL_REJECTED",
            "拒绝含写操作或危险函数的语句",
        ));
    }
    Ok(())
}

fn value_to_json(value: ValueRef<'_>) -> Json {
    match value {
        ValueRef::Null => Json::Null,
        ValueRef::Integer(number) => Json::Int(number),
        ValueRef::Real(number) => Json::Real(number),
        ValueRef::Text(bytes) => Json::Str(String::from_utf8_lossy(bytes).into_owned()),
        // BLOB 不属于只读分析口径；给一个稳定可读的占位而不是让整条查询失败。
        ValueRef::Blob(bytes) => Json::Str(format!("<blob {} bytes>", bytes.len())),
    }
}

fn timeout_message() -> String {
    format!(
        "查询超过 {} 秒被中断（请缩小范围或加 LIMIT）",
        SQL_TIMEOUT_SECONDS
    )
}

fn run_sql_query(
    conn: &Connection,
    query: &str,
    limit: i64,
) -> Result<(Vec<String>, Vec<Vec<Json>>, bool), CoreError> {
    // rusqlite 的 progress_handler 需要 hooks 特性（Cargo.toml 不在本任务范围内），
    // 用 InterruptHandle + 看门狗线程达到同样效果：无终止递归 CTE 不会挂死 MCP 进程。
    let (sender, receiver) = mpsc::channel::<()>();
    let interrupted = Arc::new(AtomicBool::new(false));
    let handle = conn.get_interrupt_handle();
    let flag = Arc::clone(&interrupted);
    let watchdog = thread::spawn(move || {
        if matches!(
            receiver.recv_timeout(Duration::from_secs_f64(SQL_TIMEOUT_SECONDS)),
            Err(mpsc::RecvTimeoutError::Timeout)
        ) {
            flag.store(true, AtomicOrdering::SeqCst);
            handle.interrupt();
        }
    });

    let outcome = (|| -> Result<(Vec<String>, Vec<Vec<Json>>, bool), CoreError> {
        let mut stmt = conn.prepare(query).map_err(sql_error)?;
        let columns: Vec<String> = stmt
            .column_names()
            .into_iter()
            .map(|name| name.to_owned())
            .collect();
        let mut rows = stmt.query([]).map_err(sql_error)?;
        let mut out: Vec<Vec<Json>> = Vec::new();
        let mut truncated = false;
        while let Some(row) = rows.next().map_err(sql_error)? {
            if out.len() as i64 >= limit {
                truncated = true;
                break;
            }
            let mut values = Vec::with_capacity(columns.len());
            for index in 0..columns.len() {
                values.push(value_to_json(row.get_ref(index).map_err(sql_error)?));
            }
            out.push(values);
        }
        Ok((columns, out, truncated))
    })();

    drop(sender);
    let _ = watchdog.join();

    match outcome {
        Ok(result) => Ok(result),
        Err(error) => {
            if interrupted.load(AtomicOrdering::SeqCst) {
                Err(CoreError::new("SQL_ERROR", timeout_message()))
            } else {
                Err(error)
            }
        }
    }
}

pub fn sql(data_root: &Path, snapshot: &str, query: &str, limit: i64) -> Result<Json, CoreError> {
    validate_sql(query)?;
    let limit = clamp_limit(limit);
    let file = resolve_snapshot(data_root, snapshot)?;
    let conn = open_snapshot(&file)?;
    let _ = conn.execute_batch("PRAGMA query_only=ON;");
    let (columns, rows, truncated) = run_sql_query(&conn, query, limit)?;
    Ok(Json::obj(vec![
        ("snapshot", jstr(snapshot)),
        ("columns", Json::Arr(columns.into_iter().map(jstr).collect())),
        (
            "rows",
            Json::Arr(
                rows.into_iter()
                    .map(Json::Arr)
                    .collect::<Vec<Json>>(),
            ),
        ),
        ("truncated", jbool(truncated)),
    ]))
}

// ---------------------------------------------------------------------------
// 概况 / 快照清单 / 两个本期未启用的后端
// ---------------------------------------------------------------------------

const ENGINE_OFF: &str =
    "本通道（--json / MCP）本期只走快照引擎；窗口里的「实时索引」走 goz 常驻管道，两者数据口径一致。";
const EVIDENCE_OFF: &str =
    "该后端本期未启用：实时变更证据（USN / 目录监听）只在 Python 版采集，原生版未接入。请用 Python 版 diskintel evidence，或在窗口「对比」视图里看两次快照的差异。";

fn now_unix() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|value| value.as_secs() as i64)
        .unwrap_or(0)
}

pub fn status(data_root: &Path) -> Json {
    let snapshots = list_snapshots(data_root);
    let latest = snapshots
        .last()
        .map(|info| jstr(info.id.clone()))
        .unwrap_or(Json::Null);
    Json::obj(vec![
        ("data_root", jstr(data_root.to_string_lossy())),
        (
            "engine",
            Json::obj(vec![
                ("available", jbool(false)),
                ("error", jstr(ENGINE_OFF)),
            ]),
        ),
        ("snapshot_count", jnum(snapshots.len() as i64)),
        ("latest", latest),
        ("format_version", jnum(FORMAT_VERSION)),
    ])
}

/// v1/v2 老快照才需要打开确认；v3 的 entries 是 WITHOUT ROWID 主键表，天然有序覆盖。
fn ordered_path_scan(path: &Path) -> bool {
    let Ok(conn) = Connection::open_with_flags(path, OpenFlags::SQLITE_OPEN_READ_ONLY) else {
        return false;
    };
    if let Ok(mut stmt) =
        conn.prepare("SELECT 1 FROM sqlite_master WHERE type='index' AND name='idx_entries_diff'")
    {
        if stmt.exists([]).unwrap_or(false) {
            return true;
        }
    }
    let Ok(mut stmt) = conn.prepare(
        "EXPLAIN QUERY PLAN SELECT path, is_dir, size, mtime FROM entries ORDER BY path",
    ) else {
        return false;
    };
    let Ok(rows) = stmt.query_map([], |row| row.get::<_, String>(3)) else {
        return false;
    };
    let mut any = false;
    for detail in rows.flatten() {
        any = true;
        if detail.to_uppercase().contains("TEMP B-TREE") {
            return false;
        }
    }
    any
}

fn round1(value: f64) -> f64 {
    (value * 10.0).round() / 10.0
}

pub fn snapshots(data_root: &Path) -> Json {
    let snapshots = list_snapshots(data_root);
    let now = now_unix();
    // 冷热分层同 Python storage.classify_snapshots：最近 3 份 hot，7 天内 warm，其余 cold。
    let mut ordered: Vec<&SnapInfo> = snapshots.iter().collect();
    ordered.sort_by(|left, right| {
        right
            .created_at
            .cmp(&left.created_at)
            .then_with(|| snapshot_sort_key(&right.id).cmp(&snapshot_sort_key(&left.id)))
    });
    let mut tiers: HashMap<String, (&'static str, f64)> = HashMap::new();
    for (index, info) in ordered.iter().enumerate() {
        let age_seconds = (now - info.created_at).max(0) as f64;
        let tier = if index < 3 {
            "hot"
        } else if age_seconds <= 7.0 * 86400.0 {
            "warm"
        } else {
            "cold"
        };
        tiers.insert(info.id.clone(), (tier, round1(age_seconds / 86400.0)));
    }

    let items = snapshots
        .iter()
        .map(|info| {
            let (tier, age_days) = tiers.get(&info.id).copied().unwrap_or(("hot", 0.0));
            let has_diff_index = if info.format_version >= 3 {
                true
            } else {
                ordered_path_scan(&info.path)
            };
            Json::obj(vec![
                ("snapshot_id", jstr(info.id.clone())),
                ("path", jstr(info.path.to_string_lossy())),
                ("created_at", jnum(info.created_at)),
                ("file_count", jnum(info.file_count)),
                ("dir_count", jnum(info.dir_count)),
                ("total_size", jnum(info.total_size)),
                ("bytes_on_disk", jnum(info.bytes_on_disk)),
                ("note", jstr(info.note.clone())),
                ("format_version", jnum(info.format_version)),
                ("excluded_count", jnum(info.excluded_count)),
                ("excluded_bytes", jnum(info.excluded_bytes)),
                (
                    "excluded_roots",
                    Json::Arr(info.excluded_roots.iter().cloned().map(jstr).collect()),
                ),
                ("tier", jstr(tier)),
                ("age_days", Json::Real(age_days)),
                ("has_diff_index", jbool(has_diff_index)),
            ])
        })
        .collect();

    Json::obj(vec![("snapshots", Json::Arr(items))])
}

/// 证据后端本期未启用，但字段结构保持与 Python 版 evidence 一致（空值 + 明确说明）。
pub fn evidence(window: i64, volume: &str, limit: i64) -> Json {
    let seconds = window.clamp(1, 7 * 86400);
    let until = now_unix() + 1;
    let since = until - seconds;
    let _ = clamp_limit(limit);
    Json::obj(vec![
        ("since", jnum(since)),
        ("until", jnum(until)),
        ("window_seconds", jnum(seconds)),
        ("volume", jstr(volume)),
        ("available", jbool(false)),
        ("message", jstr(EVIDENCE_OFF)),
        (
            "summary",
            Json::obj(vec![
                ("since", jnum(since)),
                ("until", jnum(until)),
                ("volume", jstr(volume)),
                ("events", jnum(0)),
                ("added_count", jnum(0)),
                ("removed_count", jnum(0)),
                ("modified_count", jnum(0)),
                ("renamed_count", jnum(0)),
                ("grown_bytes", jnum(0)),
                ("shrunk_bytes", jnum(0)),
                ("net_bytes", jnum(0)),
                ("top_dirs", Json::arr(Vec::new())),
            ]),
        ),
        ("top_dirs", Json::arr(Vec::new())),
        ("top_files", Json::arr(Vec::new())),
        (
            "stats",
            Json::obj(vec![
                ("path", jstr("")),
                ("db_bytes", jnum(0)),
                ("events", jnum(0)),
                ("dir_delta_rows", jnum(0)),
                ("oldest_ts", jnum(0)),
                ("newest_ts", jnum(0)),
                ("bucket_seconds", jnum(0)),
            ]),
        ),
    ])
}

/// 编排器后端本期未启用；字段结构对齐 Python engine_status 的「编排器未初始化」分支。
pub fn engine_status(probe_bridge: bool) -> Json {
    Json::obj(vec![
        ("state", jstr("off")),
        ("policy", jstr("unknown")),
        ("engine", jstr("goz")),
        ("available", jbool(false)),
        ("error", jstr(ENGINE_OFF)),
        (
            "bootstrap",
            Json::obj(vec![
                ("status", jstr("idle")),
                ("phase", jstr("")),
                ("message", jstr("原生版未接入后台建库编排")),
                ("snapshot_id", jstr("")),
                ("efu_bytes", jnum(0)),
                ("started_at", Json::Null),
                ("finished_at", Json::Null),
                ("elapsed_seconds", Json::Null),
                ("error", jstr("")),
            ]),
        ),
        (
            "bridge",
            Json::obj(vec![
                ("authorized", jbool(false)),
                (
                    "start",
                    Json::obj(vec![
                        ("task", jstr("DiskIntelEngineStart")),
                        ("exists", jbool(false)),
                    ]),
                ),
                (
                    "stop",
                    Json::obj(vec![
                        ("task", jstr("DiskIntelEngineStop")),
                        ("exists", jbool(false)),
                    ]),
                ),
                (
                    "hint",
                    jstr("原生版未接入一次性管理员授权桥；建库请用 Python 版 DiskIntelCLI snap 或窗口里的「重建索引」"),
                ),
            ]),
        ),
        (
            "health",
            Json::obj(vec![
                ("available", jbool(false)),
                ("message", jstr(ENGINE_OFF)),
            ]),
        ),
        ("probe_bridge", jbool(probe_bridge)),
    ])
}

// ---------------------------------------------------------------------------
// 工具分派：MCP tools/call 与 --json 动词走同一条实现
// ---------------------------------------------------------------------------

pub const TOOL_NAMES: [&str; 9] = [
    "diskintel_status",
    "diskintel_search",
    "diskintel_snapshots",
    "diskintel_du",
    "diskintel_top",
    "diskintel_diff",
    "diskintel_sql",
    "diskintel_evidence",
    "diskintel_engine",
];

fn arg_text(args: &Json, key: &str, default: &str) -> String {
    match args.get(key) {
        Some(Json::Str(value)) => value.clone(),
        Some(Json::Int(value)) => value.to_string(),
        _ => default.to_owned(),
    }
}

fn arg_opt_text(args: &Json, key: &str) -> Option<String> {
    match args.get(key) {
        Some(Json::Str(value)) if !value.is_empty() => Some(value.clone()),
        Some(Json::Int(value)) => Some(value.to_string()),
        _ => None,
    }
}

fn arg_i64(args: &Json, key: &str, default: i64) -> Result<i64, CoreError> {
    match args.get(key) {
        None | Some(Json::Null) => Ok(default),
        Some(Json::Int(value)) => Ok(*value),
        Some(Json::Real(value)) => Ok(*value as i64),
        Some(Json::Str(text)) => text.trim().parse::<i64>().map_err(|_| {
            CoreError::new(
                "INVALID_ARGUMENT",
                format!("参数 {key} 需要整数，收到 {text}"),
            )
        }),
        Some(_) => Err(CoreError::new(
            "INVALID_ARGUMENT",
            format!("参数 {key} 需要整数"),
        )),
    }
}

fn arg_bool(args: &Json, key: &str, default: bool) -> bool {
    match args.get(key) {
        Some(Json::Bool(value)) => *value,
        Some(Json::Str(text)) => matches!(
            text.trim().to_ascii_lowercase().as_str(),
            "true" | "1" | "yes" | "on"
        ),
        _ => default,
    }
}

fn search_from_args(data_root: &Path, args: &Json) -> Result<Json, CoreError> {
    let request = SearchRequest {
        query: arg_text(args, "query", ""),
        limit: arg_i64(args, "limit", 100)?,
        offset: arg_i64(args, "offset", 0)?,
        scope: arg_opt_text(args, "scope"),
        snapshot: arg_text(args, "snapshot", "latest"),
        kind: arg_text(args, "kind", "all"),
        sort: arg_opt_text(args, "sort"),
        ext: arg_opt_text(args, "ext"),
        min_size: match args.get("min_size") {
            Some(value) if !value.is_null() => Some(arg_i64(args, "min_size", 0)?),
            _ => None,
        },
        max_size: match args.get("max_size") {
            Some(value) if !value.is_null() => Some(arg_i64(args, "max_size", 0)?),
            _ => None,
        },
        after: match args.get("after") {
            Some(value) if !value.is_null() => Some(arg_i64(args, "after", 0)?),
            _ => None,
        },
        before: match args.get("before") {
            Some(value) if !value.is_null() => Some(arg_i64(args, "before", 0)?),
            _ => None,
        },
    };
    search(data_root, &request)
}

fn diff_from_args(data_root: &Path, args: &Json) -> Result<Json, CoreError> {
    let old = arg_opt_text(args, "old");
    let new = arg_opt_text(args, "new");
    diff(
        data_root,
        old.as_deref(),
        new.as_deref(),
        arg_i64(args, "limit", 100)?,
        &arg_text(args, "scope", ""),
        &arg_text(args, "kind", "all"),
    )
}

fn sql_from_args(data_root: &Path, args: &Json) -> Result<Json, CoreError> {
    sql(
        data_root,
        &arg_text(args, "snapshot", "latest"),
        &arg_text(args, "query", ""),
        arg_i64(args, "limit", 1000)?,
    )
}

fn du_from_args(data_root: &Path, args: &Json) -> Result<Json, CoreError> {
    du(
        data_root,
        &arg_text(args, "snapshot", "latest"),
        &arg_text(args, "path", ""),
        arg_i64(args, "depth", 1)?,
        arg_i64(args, "limit", 200)?,
    )
}

fn top_from_args(data_root: &Path, args: &Json) -> Result<Json, CoreError> {
    top(
        data_root,
        &arg_text(args, "snapshot", "latest"),
        arg_i64(args, "limit", 20)?,
        arg_bool(args, "files_only", false),
        &arg_text(args, "by", "size"),
    )
}

pub fn call_tool(data_root: &Path, tool: &str, args: &Json) -> (Json, Option<CoreError>) {
    let outcome = match tool {
        "diskintel_status" => Ok(status(data_root)),
        "diskintel_snapshots" => Ok(snapshots(data_root)),
        "diskintel_search" => search_from_args(data_root, args),
        "diskintel_du" => du_from_args(data_root, args),
        "diskintel_top" => top_from_args(data_root, args),
        "diskintel_diff" => diff_from_args(data_root, args),
        "diskintel_sql" => sql_from_args(data_root, args),
        "diskintel_evidence" => Ok(evidence(
            arg_i64(args, "window", 300).unwrap_or(300),
            &arg_text(args, "volume", ""),
            arg_i64(args, "limit", 20).unwrap_or(20),
        )),
        "diskintel_engine" => Ok(engine_status(arg_bool(args, "probe_bridge", false))),
        other => Err(CoreError::new(
            "INVALID_ARGUMENT",
            format!("Unknown tool: {other}"),
        )),
    };
    match outcome {
        Ok(data) => (data, None),
        Err(error) => (error_data(tool, args, &error), Some(error)),
    }
}

/// 出错时也给一份「空壳」数据：调用方拿到的是空列表 + 可读错误，而不是异常中断。
fn error_data(tool: &str, args: &Json, error: &CoreError) -> Json {
    let mut data = match tool {
        "diskintel_status" => Json::obj(vec![
            ("data_root", jstr("")),
            ("engine", Json::obj(vec![("available", jbool(false))])),
            ("snapshot_count", jnum(0)),
            ("latest", Json::Null),
            ("format_version", jnum(FORMAT_VERSION)),
        ]),
        "diskintel_snapshots" => Json::obj(vec![("snapshots", Json::arr(Vec::new()))]),
        "diskintel_search" => {
            let offset = arg_i64(args, "offset", 0).unwrap_or(0);
            let limit = clamp_limit(arg_i64(args, "limit", 100).unwrap_or(100));
            Json::obj(vec![
                ("query", jstr(arg_text(args, "query", ""))),
                ("snapshot", jstr(arg_text(args, "snapshot", "latest"))),
                ("total", jnum(0)),
                ("items", Json::arr(Vec::new())),
                ("more", jbool(false)),
                ("offset", jnum(offset)),
                ("limit", jnum(limit)),
                ("mode", jstr("snapshot")),
                (
                    "terms",
                    Json::arr(
                        parse_query(&arg_text(args, "query", ""))
                            .terms
                            .into_iter()
                            .map(jstr)
                            .collect(),
                    ),
                ),
                ("sort", jstr("smart")),
                ("seconds", Json::Real(0.0)),
                ("engine_available", jbool(false)),
                ("engine_source", jstr("snapshot")),
                ("orchestration", Json::obj(Vec::new())),
            ])
        }
        "diskintel_du" => Json::obj(vec![
            ("snapshot", jstr(arg_text(args, "snapshot", "latest"))),
            ("path", jstr(arg_text(args, "path", ""))),
            ("items", Json::arr(Vec::new())),
            ("total", jnum(0)),
        ]),
        "diskintel_top" => Json::obj(vec![
            ("snapshot", jstr(arg_text(args, "snapshot", "latest"))),
            ("by", jstr(arg_text(args, "by", "size"))),
            ("items", Json::arr(Vec::new())),
        ]),
        "diskintel_diff" => Json::obj(vec![
            ("old", jstr(arg_text(args, "old", "latest-1"))),
            ("new", jstr(arg_text(args, "new", "latest"))),
            ("scope", jstr(arg_text(args, "scope", ""))),
            ("kind", jstr(arg_text(args, "kind", "all"))),
            (
                "counts",
                Json::obj(vec![
                    ("added", jnum(0)),
                    ("removed", jnum(0)),
                    ("grown", jnum(0)),
                    ("shrunk", jnum(0)),
                ]),
            ),
            ("total_delta", jnum(0)),
            ("returned", jnum(0)),
            ("changes", Json::arr(Vec::new())),
            ("strategy", jstr("merge")),
        ]),
        "diskintel_sql" => Json::obj(vec![
            ("snapshot", jstr(arg_text(args, "snapshot", "latest"))),
            ("columns", Json::arr(Vec::new())),
            ("rows", Json::arr(Vec::new())),
            ("truncated", jbool(false)),
        ]),
        _ => Json::obj(Vec::new()),
    };
    data.set("ok", jbool(false));
    data.set("error", error.to_json());
    data
}

// ---------------------------------------------------------------------------
// --json 动词（脚本 / 窗口状态栏）
// ---------------------------------------------------------------------------

const VALUE_FLAGS: [&str; 20] = [
    "--data-root",
    "--limit",
    "--offset",
    "--snapshot",
    "--path",
    "--depth",
    "--scope",
    "--kind",
    "--by",
    "--old",
    "--new",
    "--query",
    "--ext",
    "--min-size",
    "--max-size",
    "--after",
    "--before",
    "--sort",
    "--window",
    "--volume",
];

const BOOL_FLAGS: [&str; 2] = ["--files-only", "--probe-bridge"];

struct CliArgs {
    data_root: PathBuf,
    verb: String,
    positionals: Vec<String>,
    values: HashMap<String, String>,
    flags: HashSet<String>,
}

fn parse_cli(argv: &[String]) -> CliArgs {
    let mut verb = String::new();
    let mut positionals = Vec::new();
    let mut values: HashMap<String, String> = HashMap::new();
    let mut flags: HashSet<String> = HashSet::new();
    let mut index = 0;
    while index < argv.len() {
        let arg = argv[index].as_str();
        if arg == "--json" {
            if let Some(next) = argv.get(index + 1) {
                if !next.starts_with('-') {
                    verb = next.clone();
                    index += 1;
                }
            }
        } else if let Some(name) = arg.strip_prefix("--json=") {
            verb = name.to_owned();
        } else if let Some((name, value)) = arg
            .strip_prefix("--")
            .and_then(|rest| rest.split_once('='))
            .map(|(name, value)| (format!("--{name}"), value.to_owned()))
        {
            if VALUE_FLAGS.contains(&name.as_str()) {
                values.insert(name, value);
            } else if BOOL_FLAGS.contains(&name.as_str()) {
                flags.insert(name);
            }
        } else if VALUE_FLAGS.contains(&arg) {
            if let Some(value) = argv.get(index + 1) {
                values.insert(arg.to_owned(), value.clone());
                index += 1;
            }
        } else if BOOL_FLAGS.contains(&arg) {
            flags.insert(arg.to_owned());
        } else if !arg.starts_with('-') {
            positionals.push(arg.to_owned());
        }
        index += 1;
    }

    let data_root = values
        .get("--data-root")
        .map(PathBuf::from)
        .or_else(|| {
            std::env::var("DISKINTEL_DATA_ROOT")
                .ok()
                .map(PathBuf::from)
        })
        .unwrap_or_else(|| PathBuf::from(DEFAULT_DATA_ROOT));

    CliArgs {
        data_root,
        verb,
        positionals,
        values,
        flags,
    }
}

fn numeric_value(
    values: &HashMap<String, String>,
    key: &str,
    default: i64,
) -> Result<i64, CoreError> {
    match values.get(key) {
        Some(text) => text.trim().parse::<i64>().map_err(|_| {
            CoreError::new(
                "INVALID_ARGUMENT",
                format!("参数 {key} 需要整数，收到 {text}"),
            )
        }),
        None => Ok(default),
    }
}

fn optional_numeric(
    values: &HashMap<String, String>,
    key: &str,
) -> Result<Option<i64>, CoreError> {
    match values.get(key) {
        Some(_) => Ok(Some(numeric_value(values, key, 0)?)),
        None => Ok(None),
    }
}

fn tool_for_verb(verb: &str) -> Option<&'static str> {
    Some(match verb {
        "status" => "diskintel_status",
        "snapshots" => "diskintel_snapshots",
        "search" => "diskintel_search",
        "du" => "diskintel_du",
        "top" => "diskintel_top",
        "diff" => "diskintel_diff",
        "sql" => "diskintel_sql",
        "evidence" => "diskintel_evidence",
        "engine" => "diskintel_engine",
        _ => return None,
    })
}

fn build_args(cli: &CliArgs) -> Result<Json, CoreError> {
    let values = &cli.values;
    let positionals = &cli.positionals;
    let mut args = Json::obj(Vec::new());
    match cli.verb.as_str() {
        "search" => {
            let query = positionals
                .first()
                .cloned()
                .or_else(|| values.get("--query").cloned())
                .unwrap_or_default();
            args.set("query", jstr(query));
            args.set("limit", jnum(numeric_value(values, "--limit", 100)?));
            args.set("offset", jnum(numeric_value(values, "--offset", 0)?));
            if let Some(scope) = values
                .get("--scope")
                .or_else(|| values.get("--path"))
                .cloned()
            {
                args.set("scope", jstr(scope));
            }
            if let Some(snapshot) = values.get("--snapshot").cloned() {
                args.set("snapshot", jstr(snapshot));
            }
            if let Some(kind) = values.get("--kind").cloned() {
                args.set("kind", jstr(kind));
            }
            if let Some(sort) = values.get("--sort").cloned() {
                args.set("sort", jstr(sort));
            }
            if let Some(ext) = values.get("--ext").cloned() {
                args.set("ext", jstr(ext));
            }
            if let Some(min_size) = optional_numeric(values, "--min-size")? {
                args.set("min_size", jnum(min_size));
            }
            if let Some(max_size) = optional_numeric(values, "--max-size")? {
                args.set("max_size", jnum(max_size));
            }
            if let Some(after) = optional_numeric(values, "--after")? {
                args.set("after", jnum(after));
            }
            if let Some(before) = optional_numeric(values, "--before")? {
                args.set("before", jnum(before));
            }
        }
        "du" => {
            if let Some(snapshot) = values.get("--snapshot").cloned() {
                args.set("snapshot", jstr(snapshot));
            }
            if let Some(path) = values.get("--path").cloned() {
                args.set("path", jstr(path));
            }
            args.set("depth", jnum(numeric_value(values, "--depth", 1)?));
            args.set("limit", jnum(numeric_value(values, "--limit", 200)?));
        }
        "top" => {
            if let Some(snapshot) = values.get("--snapshot").cloned() {
                args.set("snapshot", jstr(snapshot));
            }
            args.set("limit", jnum(numeric_value(values, "--limit", 20)?));
            if let Some(by) = values.get("--by").cloned() {
                args.set("by", jstr(by));
            }
            if cli.flags.contains("--files-only") {
                args.set("files_only", jbool(true));
            }
        }
        "diff" => {
            let old = positionals
                .first()
                .cloned()
                .or_else(|| values.get("--old").cloned());
            let new = positionals
                .get(1)
                .cloned()
                .or_else(|| values.get("--new").cloned());
            if let Some(old) = old {
                args.set("old", jstr(old));
            }
            if let Some(new) = new {
                args.set("new", jstr(new));
            }
            args.set("limit", jnum(numeric_value(values, "--limit", 100)?));
            if let Some(scope) = values.get("--scope").cloned() {
                args.set("scope", jstr(scope));
            }
            if let Some(kind) = values.get("--kind").cloned() {
                args.set("kind", jstr(kind));
            }
        }
        "sql" => {
            let query = positionals
                .first()
                .cloned()
                .or_else(|| values.get("--query").cloned())
                .unwrap_or_default();
            args.set("query", jstr(query));
            if let Some(snapshot) = values.get("--snapshot").cloned() {
                args.set("snapshot", jstr(snapshot));
            }
            args.set("limit", jnum(numeric_value(values, "--limit", 1000)?));
        }
        "evidence" => {
            args.set("window", jnum(numeric_value(values, "--window", 300)?));
            if let Some(volume) = values.get("--volume").cloned() {
                args.set("volume", jstr(volume));
            }
            args.set("limit", jnum(numeric_value(values, "--limit", 20)?));
        }
        "engine" => {
            if cli.flags.contains("--probe-bridge") {
                args.set("probe_bridge", jbool(true));
            }
        }
        _ => {}
    }
    Ok(args)
}

fn exit_code_for(error: Option<&CoreError>) -> i32 {
    match error.map(|error| error.code) {
        None => 0,
        Some("INVALID_ARGUMENT") | Some("SQL_REJECTED") => 2,
        Some("SNAPSHOT_NOT_FOUND") | Some("ENGINE_UNAVAILABLE") => 3,
        _ => 1,
    }
}

fn emit(payload: &Json) {
    let mut stdout = std::io::stdout();
    let text = payload.to_json_string();
    let _ = writeln!(stdout, "{text}");
    let _ = stdout.flush();
}

/// --json <verb> 分流；没有该选项时返回 None，主程序继续进 GUI。
pub fn run_if_requested() -> Option<i32> {
    let argv: Vec<String> = std::env::args().skip(1).collect();
    if !argv
        .iter()
        .any(|arg| arg == "--json" || arg.starts_with("--json="))
    {
        return None;
    }
    Some(run_cli(&argv))
}

fn run_cli(argv: &[String]) -> i32 {
    let started = Instant::now();
    let cli = parse_cli(argv);
    let Some(tool) = tool_for_verb(&cli.verb) else {
        let error = CoreError::new("INVALID_ARGUMENT", format!("未知动词：{}", cli.verb))
            .hint("可用动词：status / snapshots / search / du / top / diff / sql / evidence / engine");
        eprintln!(
            "用法：--json <动词> [参数]；动词：status / snapshots / search / du / top / diff / sql / evidence / engine"
        );
        let payload = Json::obj(vec![
            ("ok", jbool(false)),
            ("verb", jstr(cli.verb)),
            ("error", error.to_json()),
            ("data", Json::Null),
            (
                "meta",
                Json::obj(vec![
                    ("elapsed_ms", jnum(started.elapsed().as_millis() as i64)),
                    ("version", jstr(VERSION)),
                ]),
            ),
        ]);
        emit(&payload);
        return 2;
    };

    let args = match build_args(&cli) {
        Ok(args) => args,
        Err(error) => {
            let payload = Json::obj(vec![
                ("ok", jbool(false)),
                ("verb", jstr(cli.verb.clone())),
                ("error", error.to_json()),
                ("data", Json::Null),
                (
                    "meta",
                    Json::obj(vec![
                        ("elapsed_ms", jnum(started.elapsed().as_millis() as i64)),
                        ("version", jstr(VERSION)),
                    ]),
                ),
            ]);
            emit(&payload);
            return 2;
        }
    };

    let (data, error) = call_tool(&cli.data_root, tool, &args);
    let meta = Json::obj(vec![
        ("elapsed_ms", jnum(started.elapsed().as_millis() as i64)),
        ("version", jstr(VERSION)),
    ]);
    let payload = match &error {
        Some(error) => Json::obj(vec![
            ("ok", jbool(false)),
            ("verb", jstr(cli.verb.clone())),
            ("error", error.to_json()),
            ("data", data),
            ("meta", meta),
        ]),
        None => Json::obj(vec![
            ("ok", jbool(true)),
            ("verb", jstr(cli.verb.clone())),
            ("data", data),
            ("meta", meta),
        ]),
    };
    emit(&payload);
    exit_code_for(error.as_ref())
}

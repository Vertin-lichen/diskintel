//! goz v1 线上协议：帧格式、请求 JSON、应答解码（JSON 控制帧 + 二进制结果页）。
//!
//! 事实来源：goz v0.6.0 crates/goz-core/src/proto/{types,frame,binary}.rs（proto = 1）。
//! 这里手写一遍而不是 link goz-core：本 crate 没有 serde 依赖，整套协议只用
//! 到极小的 JSON 子集（Hello / Status / Error 三个控制帧），自带解析器反而干净，
//! 也不用把 serde/rayon/zerocopy 一串依赖拖进 GUI。
//!
//! 线上格式：
//!
//!   客户端 → 服务端：[u32 LE 长度][JSON 请求]
//!   服务端 → 客户端：[u32 LE 长度][tag: u8][体]
//!     tag 0 = JSON 控制应答（Hello / Status / Error）
//!     tag 1 = 一页二进制结果（字段全部小端）
//!              total u64 | generation u64 | flags u8 | count u32
//!              [total_size u64，仅当 flags bit3]
//!              每条：path_len u32 | path（WTF-8 原始字节）| iflags u8
//!                    [size u64] [mtime i64]（按 iflags 存在）

/// 协议版本（gozd 0.6.0 = 1）。
pub const PROTO_VERSION: u16 = 1;

/// 守护进程的命名管道。
pub const PIPE_NAME: &str = r"\\.\pipe\goz-v1";

/// 客户端 → 服务端单帧上限（1 MiB）。
pub const MAX_CLIENT_FRAME: u32 = 1 << 20;

/// 服务端 → 客户端单帧上限（16 MiB）。
pub const MAX_SERVER_FRAME: u32 = 16 << 20;

/// 服务端一页最多多少行。
pub const PAGE_ROWS: u32 = 4096;

/// FILETIME 起点（1601-01-01）到 Unix 纪元之间的秒数。
const FILETIME_UNIX_OFFSET: i64 = 11_644_473_600;

const RESP_TAG_JSON: u8 = 0;
const RESP_TAG_RESULTS: u8 = 1;

const FLAG_MORE: u8 = 1;
const FLAG_VOLUMES_INCOMPLETE: u8 = 1 << 1;
const FLAG_METADATA_PENDING: u8 = 1 << 2;
const FLAG_TOTAL_SIZE: u8 = 1 << 3;

const IFLAG_IS_DIR: u8 = 1;
const IFLAG_HAS_SIZE: u8 = 1 << 1;
const IFLAG_HAS_MTIME: u8 = 1 << 2;

/// FILETIME（100 ns 计时，自 1601 起）→ Unix 秒。
pub fn filetime_to_unix(filetime: i64) -> i64 {
    filetime / 10_000_000 - FILETIME_UNIX_OFFSET
}

/// 结果排序键（与 goz 的 SortKey 同名）。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SortKey {
    /// 文件名（索引里本来就有序，最便宜，默认）。
    Name,
    /// 整条路径，按 (父路径, 名) 比较——全盘重排最贵（实测 ~292 ms）。
    Path,
    /// 大小（全盘重排实测 ~177 ms）。
    Size,
    /// 修改时间（全盘重排实测 ~175 ms）。
    DateModified,
}

impl SortKey {
    /// 线上取值（serde 枚举名）。
    pub fn wire(self) -> &'static str {
        match self {
            Self::Name => "Name",
            Self::Path => "Path",
            Self::Size => "Size",
            Self::DateModified => "DateModified",
        }
    }

    /// es 的默认方向：名称/路径升序，大小/时间降序。
    pub fn default_dir(self) -> SortDir {
        match self {
            Self::Name | Self::Path => SortDir::Asc,
            Self::Size | Self::DateModified => SortDir::Desc,
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            Self::Name => "名称",
            Self::Path => "路径",
            Self::Size => "大小",
            Self::DateModified => "修改时间",
        }
    }
}

/// 排序方向。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SortDir {
    Asc,
    Desc,
}

impl SortDir {
    pub fn wire(self) -> &'static str {
        match self {
            Self::Asc => "Asc",
            Self::Desc => "Desc",
        }
    }

    pub fn flipped(self) -> Self {
        match self {
            Self::Asc => Self::Desc,
            Self::Desc => Self::Asc,
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            Self::Asc => "升序",
            Self::Desc => "降序",
        }
    }
}

/// 一条结果（只保留 UI 真正要的四列）。
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Item {
    /// 完整路径（WTF-8 字节按 UTF-8 lossy 解码，孤立代理项会变成 U+FFFD）。
    pub path: String,
    /// 字节数；None = 未知 / 没要 / 目录。
    pub size: Option<u64>,
    /// 原始 FILETIME；展示用 unix_mtime。
    pub mtime_ft: Option<i64>,
    pub is_dir: bool,
}

impl Item {
    /// Unix 秒，可直接喂 db::format_unix。未知或 0 值返回 None。
    pub fn unix_mtime(&self) -> Option<i64> {
        self.mtime_ft
            .map(filetime_to_unix)
            .filter(|seconds| *seconds > 0)
    }
}

/// 一页结果。
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Page {
    /// 全部命中数（不受 limit 影响，可当「命中 X 条」直接显示）。
    pub total: u64,
    /// 恒 0，兼容位。
    pub generation: u64,
    /// 还有下一页。
    pub more: bool,
    /// 至少一个卷在 bootstrap/rescan，结果可能不全——必须如实告知用户。
    pub volumes_incomplete: bool,
    /// size/mtime 还没补齐，对应列可能是 None。
    pub metadata_pending: bool,
    /// 命中文件的字节合计；只有 want_total_size 时才有。
    pub total_size: Option<u64>,
    pub items: Vec<Item>,
}

/// 一个卷的状态（Status 应答里的一行）。
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct VolumeInfo {
    pub guid: String,
    /// 挂载点（盘符 / 目录挂载）。
    pub mounts: Vec<String>,
    /// 阶段名（bootstrapping / live / rescanning / snapshot / offline / failed）。
    pub phase: String,
    /// failed 阶段的原始原因。
    pub reason: Option<String>,
    pub entries: u64,
    pub metadata_pending: bool,
}

impl VolumeInfo {
    /// 展示用挂载点：优先第一个挂载点，否则退回卷 GUID。
    pub fn mount(&self) -> &str {
        self.mounts
            .first()
            .map(String::as_str)
            .unwrap_or(self.guid.as_str())
    }
}

/// Status 应答。
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct StatusInfo {
    pub volumes: Vec<VolumeInfo>,
    pub volumes_incomplete: bool,
    pub process_working_set: u64,
    pub process_private_bytes: u64,
}

impl StatusInfo {
    /// 所有卷的条目合计。
    pub fn total_entries(&self) -> u64 {
        self.volumes.iter().map(|volume| volume.entries).sum()
    }

    /// 有卷还在补 size/mtime。
    pub fn metadata_pending(&self) -> bool {
        self.volumes.iter().any(|volume| volume.metadata_pending)
    }
}

/// 控制应答。
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Reply {
    Hello {
        proto: u16,
        server: String,
        ready: bool,
    },
    Status(StatusInfo),
    Error {
        code: String,
        message: String,
    },
    /// 本客户端不认识的应答类型（协议加法演进时可能出现）。
    Unknown(String),
}

/// 一帧解码结果。
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Frame {
    Page(Page),
    Reply(Reply),
}

/// 一次查询请求。
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct QueryRequest {
    pub query: String,
    /// 作用域目录（es 的 -path）。
    pub scope: Option<String>,
    pub sort_key: SortKey,
    pub sort_dir: SortDir,
    /// 跳过多少条（分页）。
    pub offset: u32,
    /// 最多返回多少条；None = 全量流式。
    pub limit: Option<u32>,
    pub want_size: bool,
    pub want_mtime: bool,
    pub match_case: bool,
    /// 让服务端把命中文件的总字节也算出来（多一次求和）。
    pub want_total_size: bool,
}

impl Default for QueryRequest {
    fn default() -> Self {
        Self {
            query: String::new(),
            scope: None,
            sort_key: SortKey::Name,
            sort_dir: SortDir::Asc,
            offset: 0,
            limit: Some(PAGE_ROWS),
            want_size: true,
            want_mtime: true,
            match_case: false,
            want_total_size: false,
        }
    }
}

impl QueryRequest {
    /// 编码成线上 JSON。字段一个都不能少：服务端结构体没有 serde 默认值
    /// （只有 want_total_size 可选），少字段会被整帧拒收。
    pub fn to_json(&self) -> String {
        let mut query = String::new();
        escape_json(&self.query, &mut query);
        let scope = match &self.scope {
            Some(scope) => {
                let mut escaped = String::new();
                escape_json(scope, &mut escaped);
                format!("\"{escaped}\"")
            }
            None => "null".to_owned(),
        };
        let limit = match self.limit {
            Some(limit) => limit.to_string(),
            None => "null".to_owned(),
        };
        let total_size = if self.want_total_size {
            r#","want_total_size":true"#
        } else {
            ""
        };
        format!(
            r#"{{"t":"Query","query":"{query}","scope":{scope},"sort":{{"key":"{key}","dir":"{dir}"}},"offset":{offset},"limit":{limit},"want_size":{want_size},"want_mtime":{want_mtime},"match_case":{match_case}{total_size}}}"#,
            key = self.sort_key.wire(),
            dir = self.sort_dir.wire(),
            offset = self.offset,
            want_size = self.want_size,
            want_mtime = self.want_mtime,
            match_case = self.match_case,
        )
    }
}

/// Hello 请求：独立发布的客户端应当先协商版本，被拒就退出。
pub fn hello_request(client: &str) -> String {
    let mut escaped = String::new();
    escape_json(client, &mut escaped);
    format!(
        r#"{{"t":"Hello","proto_min":{version},"proto_max":{version},"client":"{escaped}"}}"#,
        version = PROTO_VERSION,
    )
}

/// Status 请求。
pub const STATUS_REQUEST: &str = r#"{"t":"Status"}"#;

/// 把 text 按 JSON 字符串规则转义（不含两侧引号）。
pub fn escape_json(text: &str, out: &mut String) {
    for ch in text.chars() {
        match ch {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            '\u{8}' => out.push_str("\\b"),
            '\u{c}' => out.push_str("\\f"),
            ch if (ch as u32) < 0x20 => {
                out.push_str(&format!("\\u{:04x}", ch as u32));
            }
            ch => out.push(ch),
        }
    }
}

/// 解码一帧完整载荷（[tag][body]）。
pub fn decode_frame(frame: &[u8]) -> Result<Frame, String> {
    let Some((&tag, body)) = frame.split_first() else {
        return Err("收到空的应答帧".to_owned());
    };
    match tag {
        RESP_TAG_JSON => {
            let text = std::str::from_utf8(body)
                .map_err(|error| format!("控制帧不是合法 UTF-8：{error}"))?;
            let json = parse(text)?;
            Ok(Frame::Reply(decode_reply(&json)?))
        }
        RESP_TAG_RESULTS => Ok(Frame::Page(decode_page(body)?)),
        other => Err(format!("未知的应答帧标签 {other}")),
    }
}

/// 解码 JSON 控制应答。
pub fn decode_reply(json: &Json) -> Result<Reply, String> {
    let kind = json
        .get("t")
        .and_then(Json::as_str)
        .ok_or_else(|| "控制应答缺少 t 字段".to_owned())?;
    match kind {
        "Hello" => Ok(Reply::Hello {
            proto: json.get("proto").and_then(Json::as_u64).unwrap_or(0) as u16,
            server: json
                .get("server")
                .and_then(Json::as_str)
                .unwrap_or_default()
                .to_owned(),
            ready: json.get("ready").and_then(Json::as_bool).unwrap_or(false),
        }),
        "Status" => Ok(Reply::Status(decode_status(json)?)),
        "Error" => Ok(Reply::Error {
            code: json
                .get("code")
                .and_then(Json::as_str)
                .unwrap_or("Unknown")
                .to_owned(),
            message: json
                .get("message")
                .and_then(Json::as_str)
                .unwrap_or_default()
                .to_owned(),
        }),
        other => Ok(Reply::Unknown(other.to_owned())),
    }
}

const NO_ITEMS: &[Json] = &[];

fn decode_status(json: &Json) -> Result<StatusInfo, String> {
    let volumes = json
        .get("volumes")
        .and_then(Json::as_array)
        .unwrap_or(NO_ITEMS)
        .iter()
        .map(decode_volume)
        .collect::<Result<Vec<_>, String>>()?;
    Ok(StatusInfo {
        volumes,
        volumes_incomplete: json
            .get("volumes_incomplete")
            .and_then(Json::as_bool)
            .unwrap_or(false),
        process_working_set: json
            .get("process_working_set")
            .and_then(Json::as_u64)
            .unwrap_or(0),
        process_private_bytes: json
            .get("process_private_bytes")
            .and_then(Json::as_u64)
            .unwrap_or(0),
    })
}

fn decode_volume(json: &Json) -> Result<VolumeInfo, String> {
    let mounts = json
        .get("mounts")
        .and_then(Json::as_array)
        .unwrap_or(NO_ITEMS)
        .iter()
        .filter_map(|mount| mount.as_str().map(str::to_owned))
        .collect();
    // VolumePhase 是内部标记枚举：单位变体是 {"phase":"Live"}，
    // Failed 是 {"phase":"Failed","reason":"…"}。
    let (phase, reason) = match json.get("phase") {
        Some(Json::Str(text)) => (text.clone(), None),
        Some(node @ Json::Object(_)) => (
            node.get("phase")
                .and_then(Json::as_str)
                .unwrap_or("unknown")
                .to_owned(),
            node.get("reason").and_then(Json::as_str).map(str::to_owned),
        ),
        _ => ("unknown".to_owned(), None),
    };
    Ok(VolumeInfo {
        guid: json
            .get("guid")
            .and_then(Json::as_str)
            .unwrap_or_default()
            .to_owned(),
        mounts,
        phase,
        reason,
        entries: json.get("entries").and_then(Json::as_u64).unwrap_or(0),
        metadata_pending: json
            .get("metadata_pending")
            .and_then(Json::as_bool)
            .unwrap_or(false),
    })
}

/// 解码一页二进制结果（body 是 tag 之后的部分）。
pub fn decode_page(body: &[u8]) -> Result<Page, String> {
    let mut reader = Reader::new(body);
    let total = reader.u64()?;
    let generation = reader.u64()?;
    let flags = reader.u8()?;
    let count = reader.u32()? as usize;
    let total_size = if flags & FLAG_TOTAL_SIZE != 0 {
        Some(reader.u64()?)
    } else {
        None
    };
    let mut items = Vec::with_capacity(count.min(PAGE_ROWS as usize * 4));
    for _ in 0..count {
        let path_len = reader.u32()? as usize;
        let path = reader.bytes(path_len)?;
        let iflags = reader.u8()?;
        let size = if iflags & IFLAG_HAS_SIZE != 0 {
            Some(reader.u64()?)
        } else {
            None
        };
        let mtime_ft = if iflags & IFLAG_HAS_MTIME != 0 {
            Some(reader.i64()?)
        } else {
            None
        };
        items.push(Item {
            // 路径是 WTF-8 原始字节：合法 UTF-8 原样保留，孤立代理项按 U+FFFD 处理
            // （与 goz 自己的 to_string_lossy 同口径）。
            path: String::from_utf8_lossy(path).into_owned(),
            size,
            mtime_ft,
            is_dir: iflags & IFLAG_IS_DIR != 0,
        });
    }
    Ok(Page {
        total,
        generation,
        more: flags & FLAG_MORE != 0,
        volumes_incomplete: flags & FLAG_VOLUMES_INCOMPLETE != 0,
        metadata_pending: flags & FLAG_METADATA_PENDING != 0,
        total_size,
        items,
    })
}

struct Reader<'a> {
    buf: &'a [u8],
    pos: usize,
}

impl<'a> Reader<'a> {
    fn new(buf: &'a [u8]) -> Self {
        Self { buf, pos: 0 }
    }

    fn u8(&mut self) -> Result<u8, String> {
        let byte = *self
            .buf
            .get(self.pos)
            .ok_or_else(|| "结果页被截断".to_owned())?;
        self.pos += 1;
        Ok(byte)
    }

    fn u32(&mut self) -> Result<u32, String> {
        Ok(u32::from_le_bytes(self.array()?))
    }

    fn u64(&mut self) -> Result<u64, String> {
        Ok(u64::from_le_bytes(self.array()?))
    }

    fn i64(&mut self) -> Result<i64, String> {
        Ok(i64::from_le_bytes(self.array()?))
    }

    fn array<const N: usize>(&mut self) -> Result<[u8; N], String> {
        let end = self
            .pos
            .checked_add(N)
            .ok_or_else(|| "结果页长度溢出".to_owned())?;
        let slice = self
            .buf
            .get(self.pos..end)
            .ok_or_else(|| "结果页被截断".to_owned())?;
        self.pos = end;
        slice
            .try_into()
            .map_err(|_| "结果页字段长度不符".to_owned())
    }

    fn bytes(&mut self, length: usize) -> Result<&'a [u8], String> {
        let end = self
            .pos
            .checked_add(length)
            .ok_or_else(|| "结果页长度溢出".to_owned())?;
        let slice = self
            .buf
            .get(self.pos..end)
            .ok_or_else(|| "结果页路径被截断".to_owned())?;
        self.pos = end;
        Ok(slice)
    }
}

// ───────────────────────────── 极简 JSON ─────────────────────────────
//
// 只服务三个控制帧，但按完整 JSON 写：协议是加法演进的，多出来的字段
// 必须能安静跳过，而不是把整条连接判死。

/// 一个 JSON 值。
#[derive(Clone, Debug, PartialEq)]
pub enum Json {
    Null,
    Bool(bool),
    /// 数字保留原始字面量，取用时再解析，避免 f64 精度损失。
    Num(String),
    Str(String),
    Array(Vec<Json>),
    Object(Vec<(String, Json)>),
}

impl Json {
    pub fn get(&self, key: &str) -> Option<&Json> {
        match self {
            Json::Object(fields) => fields
                .iter()
                .find(|(name, _)| name == key)
                .map(|(_, value)| value),
            _ => None,
        }
    }

    pub fn as_str(&self) -> Option<&str> {
        match self {
            Json::Str(text) => Some(text),
            _ => None,
        }
    }

    pub fn as_bool(&self) -> Option<bool> {
        match self {
            Json::Bool(value) => Some(*value),
            _ => None,
        }
    }

    pub fn as_u64(&self) -> Option<u64> {
        match self {
            Json::Num(raw) => raw.parse::<u64>().ok().or_else(|| {
                raw.parse::<f64>()
                    .ok()
                    .filter(|value| *value >= 0.0)
                    .map(|value| value as u64)
            }),
            _ => None,
        }
    }

    pub fn as_array(&self) -> Option<&[Json]> {
        match self {
            Json::Array(items) => Some(items),
            _ => None,
        }
    }
}

/// 最大嵌套深度（防御畸形输入把栈打穿）。
const MAX_DEPTH: usize = 64;

/// 解析一段 JSON 文本。
pub fn parse(text: &str) -> Result<Json, String> {
    let bytes = text.as_bytes();
    let mut parser = Parser {
        bytes,
        pos: 0,
        depth: 0,
    };
    let value = parser.value()?;
    parser.skip_whitespace();
    if parser.pos != bytes.len() {
        return Err(format!("JSON 尾部有多余内容（位置 {}）", parser.pos));
    }
    Ok(value)
}

struct Parser<'a> {
    bytes: &'a [u8],
    pos: usize,
    depth: usize,
}

impl Parser<'_> {
    fn peek(&self) -> Option<u8> {
        self.bytes.get(self.pos).copied()
    }

    fn skip_whitespace(&mut self) {
        while matches!(self.peek(), Some(b' ' | b'\t' | b'\n' | b'\r')) {
            self.pos += 1;
        }
    }

    fn value(&mut self) -> Result<Json, String> {
        self.skip_whitespace();
        match self.peek() {
            None => Err("JSON 提前结束".to_owned()),
            Some(b'{') => self.object(),
            Some(b'[') => self.array(),
            Some(b'"') => Ok(Json::Str(self.string()?)),
            Some(b't') => self.literal("true", Json::Bool(true)),
            Some(b'f') => self.literal("false", Json::Bool(false)),
            Some(b'n') => self.literal("null", Json::Null),
            Some(b'-' | b'0'..=b'9') => self.number(),
            Some(other) => Err(format!("JSON 出现意外字节 {other:#04x}（位置 {}）", self.pos)),
        }
    }

    fn literal(&mut self, text: &str, value: Json) -> Result<Json, String> {
        if self.bytes[self.pos..].starts_with(text.as_bytes()) {
            self.pos += text.len();
            Ok(value)
        } else {
            Err(format!("JSON 字面量无法识别（位置 {}）", self.pos))
        }
    }

    fn number(&mut self) -> Result<Json, String> {
        let start = self.pos;
        if self.peek() == Some(b'-') {
            self.pos += 1;
        }
        while matches!(self.peek(), Some(b'0'..=b'9')) {
            self.pos += 1;
        }
        if self.peek() == Some(b'.') {
            self.pos += 1;
            while matches!(self.peek(), Some(b'0'..=b'9')) {
                self.pos += 1;
            }
        }
        if matches!(self.peek(), Some(b'e' | b'E')) {
            self.pos += 1;
            if matches!(self.peek(), Some(b'+' | b'-')) {
                self.pos += 1;
            }
            while matches!(self.peek(), Some(b'0'..=b'9')) {
                self.pos += 1;
            }
        }
        let raw = std::str::from_utf8(&self.bytes[start..self.pos])
            .map_err(|_| "JSON 数字不是合法 UTF-8".to_owned())?;
        if raw.is_empty() || raw == "-" {
            return Err(format!("JSON 数字为空（位置 {start}）"));
        }
        Ok(Json::Num(raw.to_owned()))
    }

    fn string(&mut self) -> Result<String, String> {
        // 进入时当前字节是引号。
        self.pos += 1;
        let mut out = String::new();
        loop {
            let byte = *self
                .bytes
                .get(self.pos)
                .ok_or_else(|| "JSON 字符串未闭合".to_owned())?;
            match byte {
                b'"' => {
                    self.pos += 1;
                    return Ok(out);
                }
                b'\\' => {
                    self.pos += 1;
                    let escape = *self
                        .bytes
                        .get(self.pos)
                        .ok_or_else(|| "JSON 转义未结束".to_owned())?;
                    self.pos += 1;
                    match escape {
                        b'"' => out.push('"'),
                        b'\\' => out.push('\\'),
                        b'/' => out.push('/'),
                        b'b' => out.push('\u{8}'),
                        b'f' => out.push('\u{c}'),
                        b'n' => out.push('\n'),
                        b'r' => out.push('\r'),
                        b't' => out.push('\t'),
                        b'u' => self.unicode_escape(&mut out)?,
                        other => {
                            return Err(format!("JSON 未知转义 \\{}", other as char));
                        }
                    }
                }
                _ => {
                    let rest = std::str::from_utf8(&self.bytes[self.pos..])
                        .map_err(|_| "JSON 字符串不是合法 UTF-8".to_owned())?;
                    let ch = rest
                        .chars()
                        .next()
                        .ok_or_else(|| "JSON 字符串未闭合".to_owned())?;
                    out.push(ch);
                    self.pos += ch.len_utf8();
                }
            }
        }
    }

    fn unicode_escape(&mut self, out: &mut String) -> Result<(), String> {
        let high = self.hex4()?;
        if (0xD800..0xDC00).contains(&high) {
            // 代理对：高位后面必须紧跟 \uDC00..\uDFFF。
            if self.peek() == Some(b'\\') && self.bytes.get(self.pos + 1) == Some(&b'u') {
                self.pos += 2;
                let low = self.hex4()?;
                if (0xDC00..0xE000).contains(&low) {
                    let combined =
                        0x1_0000 + ((high as u32 - 0xD800) << 10) + (low as u32 - 0xDC00);
                    out.push(char::from_u32(combined).unwrap_or('\u{FFFD}'));
                } else {
                    out.push('\u{FFFD}');
                    out.push(char::from_u32(low as u32).unwrap_or('\u{FFFD}'));
                }
            } else {
                out.push('\u{FFFD}');
            }
        } else {
            out.push(char::from_u32(high as u32).unwrap_or('\u{FFFD}'));
        }
        Ok(())
    }

    fn hex4(&mut self) -> Result<u16, String> {
        if self.pos + 4 > self.bytes.len() {
            return Err("JSON \\u 转义不完整".to_owned());
        }
        let text = std::str::from_utf8(&self.bytes[self.pos..self.pos + 4])
            .map_err(|_| "JSON \\u 转义不是合法十六进制".to_owned())?;
        let value = u16::from_str_radix(text, 16)
            .map_err(|_| format!("JSON \\u 转义不是合法十六进制：{text}"))?;
        self.pos += 4;
        Ok(value)
    }

    fn array(&mut self) -> Result<Json, String> {
        self.enter()?;
        self.pos += 1; // '['
        let mut items = Vec::new();
        self.skip_whitespace();
        if self.peek() == Some(b']') {
            self.pos += 1;
            self.depth -= 1;
            return Ok(Json::Array(items));
        }
        loop {
            items.push(self.value()?);
            self.skip_whitespace();
            match self.peek() {
                Some(b',') => self.pos += 1,
                Some(b']') => {
                    self.pos += 1;
                    self.depth -= 1;
                    return Ok(Json::Array(items));
                }
                _ => return Err(format!("JSON 数组缺少逗号或右括号（位置 {}）", self.pos)),
            }
        }
    }

    fn object(&mut self) -> Result<Json, String> {
        self.enter()?;
        self.pos += 1; // '{'
        let mut fields = Vec::new();
        self.skip_whitespace();
        if self.peek() == Some(b'}') {
            self.pos += 1;
            self.depth -= 1;
            return Ok(Json::Object(fields));
        }
        loop {
            self.skip_whitespace();
            if self.peek() != Some(b'"') {
                return Err(format!("JSON 对象的键必须是字符串（位置 {}）", self.pos));
            }
            let key = self.string()?;
            self.skip_whitespace();
            if self.peek() != Some(b':') {
                return Err(format!("JSON 对象缺少冒号（位置 {}）", self.pos));
            }
            self.pos += 1;
            let value = self.value()?;
            fields.push((key, value));
            self.skip_whitespace();
            match self.peek() {
                Some(b',') => self.pos += 1,
                Some(b'}') => {
                    self.pos += 1;
                    self.depth -= 1;
                    return Ok(Json::Object(fields));
                }
                _ => return Err(format!("JSON 对象缺少逗号或右括号（位置 {}）", self.pos)),
            }
        }
    }

    fn enter(&mut self) -> Result<(), String> {
        self.depth += 1;
        if self.depth > MAX_DEPTH {
            return Err(format!("JSON 嵌套超过 {MAX_DEPTH} 层"));
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn filetime_maps_to_unix_seconds() {
        // 133_500_000_000_000_000 = 13_350_000_000 s − 11_644_473_600 s。
        assert_eq!(filetime_to_unix(133_500_000_000_000_000), 1_705_526_400);
        assert_eq!(filetime_to_unix(0), -11_644_473_600);
    }

    #[test]
    fn hello_reply_decodes() {
        let json = parse(r#"{"t":"Hello","proto":1,"server":"gozd 0.6.0","ready":true}"#).unwrap();
        assert_eq!(
            decode_reply(&json).unwrap(),
            Reply::Hello {
                proto: 1,
                server: "gozd 0.6.0".to_owned(),
                ready: true,
            }
        );
    }

    #[test]
    fn error_reply_decodes() {
        let json =
            parse(r#"{"t":"Error","code":"BadQuery","message":"operator '|' is not"}"#).unwrap();
        assert_eq!(
            decode_reply(&json).unwrap(),
            Reply::Error {
                code: "BadQuery".to_owned(),
                message: "operator '|' is not".to_owned(),
            }
        );
    }

    #[test]
    fn status_reply_decodes_volumes_and_phase_objects() {
        let text = r#"{"t":"Status","volumes":[
            {"guid":"g1","mounts":["C:\\"],"phase":{"phase":"Live"},"entries":2350971,
             "generation":3,"metadata_pending":false,"has_orphans":false},
            {"guid":"g2","mounts":[],"phase":{"phase":"Failed","reason":"journal wrapped"},
             "entries":41,"generation":0,"metadata_pending":true}
        ],"volumes_incomplete":true,"process_working_set":331350016,"process_private_bytes":662700032}"#;
        let json = parse(text).unwrap();
        let Reply::Status(status) = decode_reply(&json).unwrap() else {
            panic!("不是 Status");
        };
        assert_eq!(status.volumes.len(), 2);
        assert_eq!(status.volumes[0].mount(), r"C:\");
        assert_eq!(status.volumes[0].phase, "Live");
        assert_eq!(status.volumes[0].entries, 2_350_971);
        assert_eq!(status.volumes[1].phase, "Failed");
        assert_eq!(status.volumes[1].reason.as_deref(), Some("journal wrapped"));
        assert!(status.volumes[1].metadata_pending);
        assert!(status.volumes_incomplete);
        assert_eq!(status.total_entries(), 2_350_971 + 41);
        assert!(status.metadata_pending());
        assert_eq!(status.process_private_bytes, 662_700_032);
    }

    fn sample_page() -> Vec<u8> {
        let mut out = Vec::new();
        out.extend_from_slice(&7u64.to_le_bytes()); // total
        out.extend_from_slice(&0u64.to_le_bytes()); // generation
        out.push(FLAG_MORE | FLAG_VOLUMES_INCOMPLETE); // flags
        out.extend_from_slice(&2u32.to_le_bytes()); // count
        // 第一条：文件，带 size + mtime。
        let path = br"C:\a.txt";
        out.extend_from_slice(&(path.len() as u32).to_le_bytes());
        out.extend_from_slice(path);
        out.push(IFLAG_HAS_SIZE | IFLAG_HAS_MTIME);
        out.extend_from_slice(&42u64.to_le_bytes());
        out.extend_from_slice(&133_500_000_000_000_000i64.to_le_bytes());
        // 第二条：目录，无 size/mtime。
        let path = br"C:\dir";
        out.extend_from_slice(&(path.len() as u32).to_le_bytes());
        out.extend_from_slice(path);
        out.push(IFLAG_IS_DIR);
        out
    }

    #[test]
    fn binary_page_decodes_items_and_flags() {
        let page = decode_page(&sample_page()).unwrap();
        assert_eq!(page.total, 7);
        assert!(page.more);
        assert!(page.volumes_incomplete);
        assert!(!page.metadata_pending);
        assert_eq!(page.total_size, None);
        assert_eq!(page.items.len(), 2);
        assert_eq!(page.items[0].path, r"C:\a.txt");
        assert_eq!(page.items[0].size, Some(42));
        assert_eq!(page.items[0].unix_mtime(), Some(1_705_526_400));
        assert!(!page.items[0].is_dir);
        assert_eq!(page.items[1].path, r"C:\dir");
        assert_eq!(page.items[1].size, None);
        assert_eq!(page.items[1].unix_mtime(), None);
        assert!(page.items[1].is_dir);
    }

    #[test]
    fn binary_page_decodes_total_size_when_flag_set() {
        let mut payload = Vec::new();
        payload.extend_from_slice(&3u64.to_le_bytes());
        payload.extend_from_slice(&0u64.to_le_bytes());
        payload.push(FLAG_TOTAL_SIZE);
        payload.extend_from_slice(&0u32.to_le_bytes());
        payload.extend_from_slice(&2_707_000_000_000u64.to_le_bytes());
        let page = decode_page(&payload).unwrap();
        assert_eq!(page.total_size, Some(2_707_000_000_000));
        assert!(page.items.is_empty());
        assert!(!page.more);
    }

    #[test]
    fn truncated_page_is_an_error_not_a_panic() {
        let mut payload = sample_page();
        payload.truncate(payload.len() - 2);
        assert!(decode_page(&payload).is_err());
    }

    #[test]
    fn unknown_frame_tag_is_rejected() {
        assert!(decode_frame(&[9, 1, 2, 3]).is_err());
        assert!(decode_frame(&[]).is_err());
    }

    #[test]
    fn query_request_json_round_trips_through_our_parser() {
        let request = QueryRequest {
            query: r#"*.exe "a b""#.to_owned(),
            scope: Some(r"C:\Windows".to_owned()),
            sort_key: SortKey::Size,
            sort_dir: SortDir::Desc,
            offset: 400,
            limit: Some(2000),
            want_size: true,
            want_mtime: true,
            match_case: false,
            want_total_size: true,
        };
        let json = parse(&request.to_json()).unwrap();
        assert_eq!(json.get("t").and_then(Json::as_str), Some("Query"));
        assert_eq!(
            json.get("query").and_then(Json::as_str),
            Some(r#"*.exe "a b""#)
        );
        assert_eq!(
            json.get("scope").and_then(Json::as_str),
            Some(r"C:\Windows")
        );
        let sort = json.get("sort").unwrap();
        assert_eq!(sort.get("key").and_then(Json::as_str), Some("Size"));
        assert_eq!(sort.get("dir").and_then(Json::as_str), Some("Desc"));
        assert_eq!(json.get("offset").and_then(Json::as_u64), Some(400));
        assert_eq!(json.get("limit").and_then(Json::as_u64), Some(2000));
        assert_eq!(
            json.get("want_total_size").and_then(Json::as_bool),
            Some(true)
        );
    }

    #[test]
    fn query_request_without_limit_and_scope_sends_nulls() {
        let request = QueryRequest {
            query: String::new(),
            scope: None,
            limit: None,
            ..QueryRequest::default()
        };
        let json = parse(&request.to_json()).unwrap();
        assert_eq!(json.get("scope"), Some(&Json::Null));
        assert_eq!(json.get("limit"), Some(&Json::Null));
        assert!(json.get("want_total_size").is_none());
    }

    #[test]
    fn hello_request_json_round_trips() {
        let json = parse(&hello_request("diskintel-rs/0.3")).unwrap();
        assert_eq!(json.get("t").and_then(Json::as_str), Some("Hello"));
        assert_eq!(
            json.get("proto_min").and_then(Json::as_u64),
            Some(PROTO_VERSION as u64)
        );
        assert_eq!(
            json.get("client").and_then(Json::as_str),
            Some("diskintel-rs/0.3")
        );
    }

    #[test]
    fn escape_json_survives_the_parser() {
        let original = "a\"b\\c\nd\te\u{1}中文";
        let mut escaped = String::new();
        escape_json(original, &mut escaped);
        let json = parse(&format!("\"{escaped}\"")).unwrap();
        assert_eq!(json.as_str(), Some(original));
    }

    #[test]
    fn unicode_escapes_and_surrogate_pairs_decode() {
        let json = parse(r#""\u4e2d\u6587 \ud83d\ude00""#).unwrap();
        assert_eq!(json.as_str(), Some("中文 😀"));
    }

    #[test]
    fn numbers_keep_their_precision() {
        let json = parse(r#"{"n":18446744073709551615,"f":1.5,"e":1e3}"#).unwrap();
        assert_eq!(json.get("n").and_then(Json::as_u64), Some(u64::MAX));
        assert_eq!(json.get("f").and_then(Json::as_u64), Some(1));
        assert_eq!(json.get("e").and_then(Json::as_u64), Some(1000));
    }
}

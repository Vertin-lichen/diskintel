//! MCP server：stdio JSON-RPC 2.0（换行分隔 UTF-8 JSON）。
//!
//! 传输依据 MCP transports：换行分隔的 UTF-8 JSON，消息内不含嵌入换行；
//! stdout 只写协议消息，日志一律走 stderr。工具名与 Python 版完全一致，
//! 保证既有 agent 配置不失效。

use std::io::{BufRead, Write};
use std::path::{Path, PathBuf};

use crate::json_cli::{self, jbool, jstr, Json};

pub const PROTOCOL_VERSIONS: [&str; 3] = ["2026-07-28", "2025-06-18", "2024-11-05"];
pub const DEFAULT_PROTOCOL_VERSION: &str = "2025-06-18";

/// --mcp 分流；没有该选项时返回 None，主程序继续进 GUI。
pub fn run_if_requested() -> Option<i32> {
    let argv: Vec<String> = std::env::args().skip(1).collect();
    if !argv.iter().any(|arg| arg == "--mcp") {
        return None;
    }
    Some(serve(&data_root_from(&argv)))
}

fn data_root_from(argv: &[String]) -> PathBuf {
    if let Some(index) = argv.iter().position(|arg| arg == "--data-root") {
        if let Some(value) = argv.get(index + 1) {
            return PathBuf::from(value);
        }
    }
    if let Some(value) = argv.iter().find_map(|arg| arg.strip_prefix("--data-root=")) {
        return PathBuf::from(value);
    }
    std::env::var("DISKINTEL_DATA_ROOT")
        .map(PathBuf::from)
        .unwrap_or_else(|_| PathBuf::from(json_cli::DEFAULT_DATA_ROOT))
}

fn schema(properties: Vec<(&str, Json)>, required: Vec<&str>) -> Json {
    let mut object = Json::obj(vec![
        ("type", jstr("object")),
        ("properties", Json::obj(properties)),
    ]);
    if !required.is_empty() {
        object.set(
            "required",
            Json::arr(required.into_iter().map(jstr).collect()),
        );
    }
    object
}

fn property(kind: &str, description: &str) -> Json {
    Json::obj(vec![
        ("type", jstr(kind)),
        ("description", jstr(description)),
    ])
}

fn enum_property(kind: &str, values: Vec<&str>, description: &str) -> Json {
    Json::obj(vec![
        ("type", jstr(kind)),
        ("enum", Json::arr(values.into_iter().map(jstr).collect())),
        ("description", jstr(description)),
    ])
}

fn tool(name: &str, description: &str, input_schema: Json) -> Json {
    Json::obj(vec![
        ("name", jstr(name)),
        ("description", jstr(description)),
        ("inputSchema", input_schema),
    ])
}

/// 9 个工具与 Python 版 diskintel.mcp.TOOLS 一一对应（名称、描述、参数 schema）。
pub fn tools() -> Json {
    Json::arr(vec![
        tool(
            "diskintel_status",
            "引擎、数据根与最新快照概况；调用任何工具前可先用它确认可用性",
            schema(Vec::new(), Vec::new()),
        ),
        tool(
            "diskintel_search",
            "按文件名实时检索（goz MFT+USN 索引，非快照数据），输入即出",
            schema(
                vec![
                    ("query", property("string", "goz / es.exe 兼容查询串")),
                    ("limit", property("integer", "返回条数，默认 100")),
                    ("scope", property("string", "限定目录，如 C:\\Users")),
                ],
                vec!["query"],
            ),
        ),
        tool(
            "diskintel_snapshots",
            "列出全部快照及其时间、文件数、总大小",
            schema(Vec::new(), Vec::new()),
        ),
        tool(
            "diskintel_du",
            "目录空间占用（子树大小与文件数），可下钻",
            schema(
                vec![
                    ("snapshot", property("string", "快照 id 或 latest，默认 latest")),
                    ("path", property("string", "目录绝对路径，空则返回各卷根")),
                    ("depth", property("integer", "下钻层级，默认 1")),
                    ("limit", property("integer", "返回条数，默认 200")),
                ],
                Vec::new(),
            ),
        ),
        tool(
            "diskintel_top",
            "占用空间最大的目录排行；files_only=true 时改排文件",
            schema(
                vec![
                    ("snapshot", property("string", "快照 id 或 latest")),
                    ("limit", property("integer", "返回条数，默认 20")),
                    (
                        "by",
                        enum_property(
                            "string",
                            vec!["size", "count"],
                            "目录榜排序字段：size 体积 / count 文件数，默认 size",
                        ),
                    ),
                    (
                        "files_only",
                        property("boolean", "true 时只排文件（忽略 by），默认 false"),
                    ),
                ],
                Vec::new(),
            ),
        ),
        tool(
            "diskintel_diff",
            "对比两份快照，返回新增/消失/增长/缩小及净变化",
            schema(
                vec![
                    ("old", property("string", "旧快照 id，默认 latest-1")),
                    ("new", property("string", "新快照 id，默认 latest")),
                    ("limit", property("integer", "返回条数，默认 100")),
                    ("scope", property("string", "限定路径前缀")),
                    (
                        "kind",
                        enum_property(
                            "string",
                            vec!["all", "added", "removed", "grown", "shrunk"],
                            "差异分类过滤，默认 all",
                        ),
                    ),
                ],
                Vec::new(),
            ),
        ),
        tool(
            "diskintel_sql",
            "对快照库执行只读 SQL（SELECT / WITH / EXPLAIN），可查 entries、folder_sizes、meta",
            schema(
                vec![
                    ("query", property("string", "只读 SQL 语句")),
                    ("snapshot", property("string", "快照 id 或 latest")),
                    ("limit", property("integer", "返回行数上限，默认 1000")),
                ],
                vec!["query"],
            ),
        ),
        tool(
            "diskintel_evidence",
            "近期「谁吃掉了空间」：实时变更证据窗口内的增长目录与文件排行；需先运行 changes 或开启哨兵证据采集才有数据",
            schema(
                vec![
                    ("window", property("integer", "回溯秒数，默认 300")),
                    ("volume", property("string", "限定卷，如 D:\\，空则全部")),
                    ("limit", property("integer", "各榜返回条数，默认 20")),
                ],
                Vec::new(),
            ),
        ),
        tool(
            "diskintel_engine",
            "双引擎编排状态（只读）：三态策略、GoZ 健康检查、一次性管理员授权桥、首次后台建立自研索引库的进度",
            schema(
                vec![(
                    "probe_bridge",
                    property(
                        "boolean",
                        "是否额外探测计划任务授权桥，默认 false（省一次外部查询）",
                    ),
                )],
                Vec::new(),
            ),
        ),
    ])
}

fn response(id: Json, result: Json) -> Json {
    Json::obj(vec![
        ("jsonrpc", jstr("2.0")),
        ("id", id),
        ("result", result),
    ])
}

fn error_response(id: Json, code: i64, message: impl Into<String>) -> Json {
    Json::obj(vec![
        ("jsonrpc", jstr("2.0")),
        ("id", id),
        (
            "error",
            Json::obj(vec![("code", Json::Int(code)), ("message", jstr(message.into()))]),
        ),
    ])
}

fn id_of(message: &Json) -> Json {
    message.get("id").cloned().unwrap_or(Json::Null)
}

fn handle_tools_call(data_root: &Path, id: Json, params: &Json) -> Json {
    let name = params.get("name").and_then(Json::as_str).unwrap_or("");
    let arguments = match params.get("arguments") {
        None | Some(Json::Null) => Json::obj(Vec::new()),
        Some(value @ Json::Obj(_)) => value.clone(),
        Some(_) => return error_response(id, -32602, "arguments 必须是对象"),
    };
    if !json_cli::TOOL_NAMES.contains(&name) {
        return error_response(id, -32602, format!("Unknown tool: {name}"));
    }
    let (data, error) = json_cli::call_tool(data_root, name, &arguments);
    // 成功时 content.text 就是 data 的 JSON；业务错误时 text 是可读原因，
    // 同时仍给出结构化 structuredContent（空列表 + ok:false + error），
    // 让 agent 既能读懂为什么失败，也能照常解析字段。
    let text = match &error {
        Some(error) => error.message.clone(),
        None => data.to_json_string(),
    };
    let result = Json::obj(vec![
        (
            "content",
            Json::arr(vec![Json::obj(vec![
                ("type", jstr("text")),
                ("text", jstr(text)),
            ])]),
        ),
        ("structuredContent", data),
        ("isError", jbool(error.is_some())),
    ]);
    response(id, result)
}

/// 处理一条消息；任何异常都返回 JSON-RPC 错误，绝不打断 stdio 循环。
fn handle_message(data_root: &Path, message: &Json) -> Option<Json> {
    if !matches!(message, Json::Obj(_)) {
        return Some(error_response(Json::Null, -32600, "Invalid Request"));
    }
    if message.get("jsonrpc").and_then(Json::as_str) != Some("2.0") {
        return Some(error_response(
            id_of(message),
            -32600,
            "Invalid Request: jsonrpc 必须为 2.0",
        ));
    }
    if let Some(value) = message.get("id") {
        match value {
            Json::Null | Json::Str(_) | Json::Int(_) | Json::Real(_) => {}
            _ => {
                return Some(error_response(
                    Json::Null,
                    -32600,
                    "Invalid Request: id 类型非法",
                ))
            }
        }
    }
    let id = id_of(message);
    let params = match message.get("params") {
        None | Some(Json::Null) => Json::obj(Vec::new()),
        Some(value @ Json::Obj(_)) => value.clone(),
        Some(_) => return Some(error_response(id, -32602, "params 必须是对象")),
    };
    let Some(method) = message.get("method").and_then(Json::as_str) else {
        return Some(error_response(id, -32600, "method 必须是字符串"));
    };
    let notification = message.get("id").is_none();

    match method {
        "initialize" => {
            let requested = params
                .get("protocolVersion")
                .and_then(Json::as_str)
                .unwrap_or("");
            let version = if PROTOCOL_VERSIONS.contains(&requested) {
                requested
            } else {
                DEFAULT_PROTOCOL_VERSION
            };
            let result = Json::obj(vec![
                ("protocolVersion", jstr(version)),
                (
                    "capabilities",
                    Json::obj(vec![("tools", Json::obj(Vec::new()))]),
                ),
                (
                    "serverInfo",
                    Json::obj(vec![
                        ("name", jstr("diskintel")),
                        ("title", jstr("磁盘情报系统")),
                        ("version", jstr(json_cli::VERSION)),
                    ]),
                ),
                (
                    "instructions",
                    jstr("只读磁盘情报：实时检索、空间聚合、快照对比与只读 SQL。"),
                ),
            ]);
            Some(response(id, result))
        }
        "notifications/initialized" => None,
        "ping" => Some(response(id, Json::obj(Vec::new()))),
        "tools/list" => Some(response(id, Json::obj(vec![("tools", tools())]))),
        "tools/call" => Some(handle_tools_call(data_root, id, &params)),
        "resources/list" => Some(response(
            id,
            Json::obj(vec![("resources", Json::arr(Vec::new()))]),
        )),
        "prompts/list" => Some(response(
            id,
            Json::obj(vec![("prompts", Json::arr(Vec::new()))]),
        )),
        other => {
            if notification || other.starts_with("notifications/") {
                None
            } else {
                Some(error_response(
                    id,
                    -32601,
                    format!("Method not found: {other}"),
                ))
            }
        }
    }
}

fn write_response(writer: &mut impl Write, payload: &Json) {
    let text = payload.to_json_string();
    if writeln!(writer, "{text}").is_err() || writer.flush().is_err() {
        eprintln!("[mcp] 写 stdout 失败（对端可能已关闭管道）");
    }
}

/// 阻塞式 stdio 循环：逐行读 UTF-8 JSON，逐行写响应。
pub fn serve(data_root: &Path) -> i32 {
    let stdin = std::io::stdin();
    let mut reader = std::io::BufReader::new(stdin.lock());
    let stdout = std::io::stdout();
    let mut writer = stdout.lock();
    let mut buffer: Vec<u8> = Vec::new();
    loop {
        buffer.clear();
        match reader.read_until(b'\n', &mut buffer) {
            Ok(0) => break,
            Ok(_) => {}
            Err(error) => {
                eprintln!("[mcp] 读取 stdin 失败：{error}");
                break;
            }
        }
        let mut slice = buffer.as_slice();
        if slice.starts_with(&[0xEF, 0xBB, 0xBF]) {
            slice = &slice[3..];
        }
        let text = match std::str::from_utf8(slice) {
            Ok(text) => text.trim().to_owned(),
            Err(_) => {
                write_response(
                    &mut writer,
                    &error_response(Json::Null, -32700, "Parse error: invalid UTF-8"),
                );
                continue;
            }
        };
        if text.is_empty() {
            continue;
        }
        let response = match Json::parse(&text) {
            Ok(message) => handle_message(data_root, &message),
            Err(error) => Some(error_response(
                Json::Null,
                -32700,
                format!("Parse error: {error}"),
            )),
        };
        if let Some(response) = response {
            write_response(&mut writer, &response);
        }
    }
    0
}

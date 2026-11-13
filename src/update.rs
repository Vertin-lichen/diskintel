//! 检查更新（热更新入口）：只读 GitHub Releases，不自动下载、不自动替换文件。
//!
//! 设计边界：
//! - 网络只走 WinHTTP 原生 API（winhttp.dll），不引第三方 HTTP crate；
//! - 只认 https，超时 5 秒；失败返回可读的中文错误，由界面降级成「打开 Releases 页面」；
//! - 「热更新」是人工动作：界面只负责发现新版本并打开下载页，替换文件由用户完成。
//!
//! 截图钩子：环境变量 DISKINTEL_UPDATE_FAKE=1 时 latest_release 返回固定假结果、
//! DISKINTEL_UPDATE_FAKE=fail 返回固定错误，仅用于验收截图；不设置时永远走真实网络。

use std::ffi::c_void;
use std::ptr;
use std::sync::mpsc::{channel, Receiver};
use std::time::Duration;

/// GitHub 仓库 slug（API 与发布页共用）。
pub const REPO_SLUG: &str = "Vertin-lichen/diskintel";
/// 仓库主页。
pub const REPO_URL: &str = "https://github.com/Vertin-lichen/diskintel";
/// Releases 列表页（检查更新失败时的降级入口）。
pub const RELEASES_URL: &str = "https://github.com/Vertin-lichen/diskintel/releases";
/// 最新 Release 的 API。
pub const LATEST_API: &str = "https://api.github.com/repos/Vertin-lichen/diskintel/releases/latest";
/// README 的「更新」章节（手动热更新说明）。
pub const UPDATE_DOC_URL: &str = "https://github.com/Vertin-lichen/diskintel#更新";
/// 当前编译进来的版本号（Cargo.toml 的 version）。
pub const CURRENT_VERSION: &str = env!("CARGO_PKG_VERSION");

/// body 摘要最多取多少字符（完整 release notes 通常很长，窗口里放不下）。
const BODY_EXCERPT_CHARS: usize = 200;
/// 网络超时统一 5 秒（解析 / 连接 / 发送 / 接收）。
const TIMEOUT_MS: i32 = 5000;
/// 响应体上限：release JSON 正常几十 KB，超过 1 MB 视为异常。
const MAX_RESPONSE_BYTES: usize = 1_048_576;
/// HTTPS 默认端口。
const HTTPS_PORT: u16 = 443;

/// WinHTTP 访问类型：自动读取系统代理（Win8.1+）。
const WINHTTP_ACCESS_TYPE_AUTOMATIC_PROXY: u32 = 4;
/// WinHTTP 请求标志：https。
const WINHTTP_FLAG_SECURE: u32 = 0x0080_0000;
/// 查询 HTTP 状态码。
const WINHTTP_QUERY_STATUS_CODE: u32 = 19;
/// 让 QueryHeaders 把结果按整数写入缓冲区。
const WINHTTP_QUERY_FLAG_NUMBER: u32 = 0x2000_0000;
/// ShellExecuteW 的显示方式：正常窗口。
const SW_SHOWNORMAL: i32 = 1;

/// 一次「最新 Release」查询的结果。
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ReleaseInfo {
    /// 版本标签，例如 v0.3.2。
    pub tag_name: String,
    /// 发布页地址（打开下载页用它）。
    pub html_url: String,
    /// 更新说明摘要（前 BODY_EXCERPT_CHARS 个字符）。
    pub body: String,
}

/// 查询 GitHub 最新 Release。
///
/// 成功返回 ReleaseInfo；任何网络 / HTTP / 解析错误都返回中文错误文本，绝不 panic。
pub fn latest_release() -> Result<ReleaseInfo, String> {
    if let Some(mode) = fake_mode() {
        std::thread::sleep(Duration::from_millis(150));
        return match mode {
            FakeMode::Success => Ok(fake_release()),
            FakeMode::Failure => Err("（截图用）模拟失败：连接 api.github.com 超时".to_owned()),
        };
    }
    let agent = format!("DiskIntel/{CURRENT_VERSION}");
    let json = https_get(LATEST_API, &agent)?;
    parse_release(&json)
}

/// 后台线程检查更新：结果通过 channel 回传，线程结束时请求一次重绘。
///
/// 界面每帧 try_recv；不用 join，也不会阻塞 UI。
pub fn spawn_check(repaint: impl Fn() + Send + 'static) -> Receiver<Result<ReleaseInfo, String>> {
    let (tx, rx) = channel();
    std::thread::spawn(move || {
        let result = latest_release();
        let _ = tx.send(result);
        repaint();
    });
    rx
}

/// 用系统默认浏览器打开链接（ShellExecuteW）。
pub fn open_url(url: &str) -> Result<(), String> {
    let operation = wide("open");
    let file = wide(url);
    #[link(name = "shell32")]
    unsafe extern "system" {
        fn ShellExecuteW(
            hwnd: *mut c_void,
            operation: *const u16,
            file: *const u16,
            parameters: *const u16,
            directory: *const u16,
            show_cmd: i32,
        ) -> isize;
    }
    let result = unsafe {
        ShellExecuteW(
            ptr::null_mut(),
            operation.as_ptr(),
            file.as_ptr(),
            ptr::null(),
            ptr::null(),
            SW_SHOWNORMAL,
        )
    };
    if result > 32 {
        Ok(())
    } else {
        Err(format!("打开链接失败（ShellExecuteW 返回 {result}）：{url}"))
    }
}

/// --check-update 命令行分支：没有该参数时返回 None，主程序继续进 GUI。
pub fn run_if_requested() -> Option<i32> {
    if !std::env::args().skip(1).any(|arg| arg == "--check-update") {
        return None;
    }
    println!("磁盘情报系统 {CURRENT_VERSION}（仓库 {REPO_SLUG}）");
    match latest_release() {
        Ok(info) => {
            if is_newer(&info.tag_name, CURRENT_VERSION) {
                println!("最新版本 {}（有新版本）", info.tag_name);
            } else {
                println!("最新版本 {}（已是最新）", info.tag_name);
            }
            println!("发布页：{}", info.html_url);
            if !info.body.is_empty() {
                println!("更新说明：{}", info.body);
            }
            Some(0)
        }
        Err(error) => {
            println!("检查更新失败：{error}");
            println!("可手动打开 Releases 页面：{RELEASES_URL}");
            Some(1)
        }
    }
}

/// 版本比较：latest 是否比 current 新（按点分数字逐段比较，缺段补 0）。
pub fn is_newer(latest: &str, current: &str) -> bool {
    let latest = version_parts(latest);
    let current = version_parts(current);
    if latest.is_empty() {
        return false;
    }
    for index in 0..latest.len().max(current.len()) {
        let left = latest.get(index).copied().unwrap_or(0);
        let right = current.get(index).copied().unwrap_or(0);
        if left != right {
            return left > right;
        }
    }
    false
}

/// 是否处于截图用的假结果模式（启动时用它决定要不要自动打开「检查更新」窗口）。
pub fn fake_enabled() -> bool {
    fake_mode().is_some()
}

// ---------- 内部实现 ----------

enum FakeMode {
    Success,
    Failure,
}

fn fake_mode() -> Option<FakeMode> {
    let value = std::env::var("DISKINTEL_UPDATE_FAKE").ok()?;
    match value.trim() {
        "1" | "true" | "ok" => Some(FakeMode::Success),
        "fail" | "error" => Some(FakeMode::Failure),
        _ => None,
    }
}

fn fake_release() -> ReleaseInfo {
    ReleaseInfo {
        tag_name: "v0.3.2".to_owned(),
        html_url: format!("{RELEASES_URL}/tag/v0.3.2"),
        body: "本次更新：菜单栏精简（品牌块移到 Hero 与关于窗口）；帮助菜单新增「检查更新」；\
               工具菜单新增「启动时自动建快照」与「启动时检查更新」；关于窗口显示版本号与仓库地址。"
            .to_owned(),
    }
}

/// 把版本标签拆成数字段（v0.3.2 -beta → [0, 3, 2]）。
fn version_parts(text: &str) -> Vec<u64> {
    let text = text.trim().trim_start_matches(['v', 'V']);
    text.split(|ch: char| !ch.is_ascii_digit())
        .filter(|part| !part.is_empty())
        .map(|part| part.parse().unwrap_or(0))
        .collect()
}

/// 解析 GitHub Release JSON 里需要的三个字段；格式不符时给出可读错误。
fn parse_release(json: &str) -> Result<ReleaseInfo, String> {
    if !json.contains("\"tag_name\"") {
        return Err(match json_str(json, "message") {
            Some(message) => format!("GitHub API：{message}"),
            None => "GitHub 响应里没有 tag_name（仓库可能还没有发布 Release）".to_owned(),
        });
    }
    let tag_name = json_str(json, "tag_name").unwrap_or_default();
    if tag_name.trim().is_empty() {
        return Err("GitHub 响应里的 tag_name 是空的".to_owned());
    }
    let html_url = json_str(json, "html_url").unwrap_or_else(|| RELEASES_URL.to_owned());
    let body = json_str(json, "body").unwrap_or_default();
    Ok(ReleaseInfo {
        tag_name: tag_name.trim().to_owned(),
        html_url,
        body: excerpt(&body, BODY_EXCERPT_CHARS),
    })
}

/// 取前 max_chars 个字符，被截断时补省略号。
fn excerpt(text: &str, max_chars: usize) -> String {
    let text = text.trim();
    let mut out: String = text.chars().take(max_chars).collect();
    if text.chars().nth(max_chars).is_some() {
        out.push('…');
    }
    out
}

/// 从 JSON 文本里取某个键的字符串值（极小实现，只认第一个同名键）。
fn json_str(json: &str, key: &str) -> Option<String> {
    let needle = format!("\"{key}\"");
    let start = json.find(&needle)? + needle.len();
    let rest = json[start..].trim_start();
    let rest = rest.strip_prefix(':')?.trim_start();
    let rest = rest.strip_prefix('"')?;
    Some(decode_json_string(string_body(rest)))
}

/// 找到字符串值结尾的引号（跳过反斜杠转义），返回不含引号的内容。
fn string_body(text: &str) -> &str {
    let bytes = text.as_bytes();
    let mut index = 0;
    while index < bytes.len() {
        match bytes[index] {
            b'\\' => index += 2,
            b'"' => return &text[..index],
            _ => index += 1,
        }
    }
    text
}

/// 还原 JSON 字符串转义（"、\、/、b、f、n、r、t、uXXXX）。
fn decode_json_string(text: &str) -> String {
    let chars: Vec<char> = text.chars().collect();
    let mut out = String::with_capacity(text.len());
    let mut index = 0;
    while index < chars.len() {
        let ch = chars[index];
        index += 1;
        if ch != '\\' {
            out.push(ch);
            continue;
        }
        let Some(escape) = chars.get(index).copied() else {
            break;
        };
        index += 1;
        match escape {
            '"' => out.push('"'),
            '\\' => out.push('\\'),
            '/' => out.push('/'),
            'b' => out.push('\u{0008}'),
            'f' => out.push('\u{000c}'),
            'n' => out.push('\n'),
            'r' => out.push('\r'),
            't' => out.push('\t'),
            'u' => {
                let mut code = 0u32;
                for _ in 0..4 {
                    let Some(digit) = chars.get(index).and_then(|c| c.to_digit(16)) else {
                        break;
                    };
                    code = code * 16 + digit;
                    index += 1;
                }
                out.push(char::from_u32(code).unwrap_or('\u{fffd}'));
            }
            other => out.push(other),
        }
    }
    out
}

/// 转成 UTF-16 并以 NUL 结尾（Win32 宽字符 API 用）。
fn wide(text: &str) -> Vec<u16> {
    text.encode_utf16().chain(std::iter::once(0)).collect()
}

// ---------- WinHTTP ----------

type WinHttpHandle = *mut c_void;

/// 句柄守卫：离开作用域必定 WinHttpCloseHandle，错误分支也不会泄漏。
struct Handle(WinHttpHandle);

impl Drop for Handle {
    fn drop(&mut self) {
        if !self.0.is_null() {
            unsafe {
                WinHttpCloseHandle(self.0);
            }
        }
    }
}

#[link(name = "winhttp")]
unsafe extern "system" {
    fn WinHttpOpen(
        agent: *const u16,
        access_type: u32,
        proxy: *const u16,
        proxy_bypass: *const u16,
        flags: u32,
    ) -> WinHttpHandle;
    fn WinHttpSetTimeouts(
        session: WinHttpHandle,
        resolve_ms: i32,
        connect_ms: i32,
        send_ms: i32,
        receive_ms: i32,
    ) -> i32;
    fn WinHttpConnect(
        session: WinHttpHandle,
        server: *const u16,
        port: u16,
        reserved: u32,
    ) -> WinHttpHandle;
    fn WinHttpOpenRequest(
        connect: WinHttpHandle,
        verb: *const u16,
        object: *const u16,
        version: *const u16,
        referrer: *const u16,
        accept_types: *const *const u16,
        flags: u32,
    ) -> WinHttpHandle;
    fn WinHttpSendRequest(
        request: WinHttpHandle,
        headers: *const u16,
        headers_len: u32,
        optional: *mut c_void,
        optional_len: u32,
        total_len: u32,
        context: usize,
    ) -> i32;
    fn WinHttpReceiveResponse(request: WinHttpHandle, reserved: *mut c_void) -> i32;
    fn WinHttpQueryHeaders(
        request: WinHttpHandle,
        info_level: u32,
        name: *const u16,
        buffer: *mut c_void,
        buffer_len: *mut u32,
        index: *mut u32,
    ) -> i32;
    fn WinHttpReadData(
        request: WinHttpHandle,
        buffer: *mut c_void,
        to_read: u32,
        read: *mut u32,
    ) -> i32;
    fn WinHttpCloseHandle(handle: WinHttpHandle) -> i32;
}

/// https GET，返回响应体文本；失败给中文提示。
fn https_get(url: &str, user_agent: &str) -> Result<String, String> {
    let Some(rest) = url.strip_prefix("https://") else {
        return Err(format!("检查更新只支持 https 地址：{url}"));
    };
    let (host, object) = match rest.find('/') {
        Some(index) => (&rest[..index], &rest[index..]),
        None => (rest, "/"),
    };
    let host_w = wide(host);
    let object_w = wide(object);
    let verb_w = wide("GET");
    let agent_w = wide(user_agent);
    let headers_w = wide(
        "Accept: application/vnd.github+json\r\nX-GitHub-Api-Version: 2022-11-28\r\n",
    );

    unsafe {
        let session = Handle(WinHttpOpen(
            agent_w.as_ptr(),
            WINHTTP_ACCESS_TYPE_AUTOMATIC_PROXY,
            ptr::null(),
            ptr::null(),
            0,
        ));
        if session.0.is_null() {
            return Err("WinHttpOpen 失败：无法初始化网络会话".to_owned());
        }
        if WinHttpSetTimeouts(session.0, TIMEOUT_MS, TIMEOUT_MS, TIMEOUT_MS, TIMEOUT_MS) == 0 {
            return Err("WinHttpSetTimeouts 失败：无法设置 5 秒超时".to_owned());
        }
        let connect = Handle(WinHttpConnect(session.0, host_w.as_ptr(), HTTPS_PORT, 0));
        if connect.0.is_null() {
            return Err(format!("无法连接 {host}:443（网络不可用或被代理拦截）"));
        }
        let request = Handle(WinHttpOpenRequest(
            connect.0,
            verb_w.as_ptr(),
            object_w.as_ptr(),
            ptr::null(),
            ptr::null(),
            ptr::null(),
            WINHTTP_FLAG_SECURE,
        ));
        if request.0.is_null() {
            return Err("WinHttpOpenRequest 失败：无法创建 HTTPS 请求".to_owned());
        }
        if WinHttpSendRequest(
            request.0,
            headers_w.as_ptr(),
            u32::MAX,
            ptr::null_mut(),
            0,
            0,
            0,
        ) == 0
        {
            return Err("WinHttpSendRequest 失败：无法发出请求".to_owned());
        }
        if WinHttpReceiveResponse(request.0, ptr::null_mut()) == 0 {
            return Err("WinHttpReceiveResponse 失败：5 秒内没有收到响应".to_owned());
        }
        let mut status: u32 = 0;
        let mut length = std::mem::size_of::<u32>() as u32;
        if WinHttpQueryHeaders(
            request.0,
            WINHTTP_QUERY_STATUS_CODE | WINHTTP_QUERY_FLAG_NUMBER,
            ptr::null(),
            &mut status as *mut u32 as *mut c_void,
            &mut length,
            ptr::null_mut(),
        ) == 0
        {
            return Err("WinHttpQueryHeaders 失败：读不到 HTTP 状态码".to_owned());
        }
        match status {
            200 => {}
            403 => return Err("GitHub 拒绝访问（HTTP 403）：可能被代理拦截或触发频率限制".to_owned()),
            404 => {
                return Err(
                    "GitHub 返回 404：仓库或 Release 不存在，或网络被代理拦截".to_owned()
                );
            }
            other => return Err(format!("GitHub 返回 HTTP {other}")),
        }
        let mut body: Vec<u8> = Vec::new();
        loop {
            let mut buffer = [0u8; 8192];
            let mut read: u32 = 0;
            if WinHttpReadData(
                request.0,
                buffer.as_mut_ptr() as *mut c_void,
                buffer.len() as u32,
                &mut read,
            ) == 0
            {
                return Err("WinHttpReadData 失败：读取响应中断".to_owned());
            }
            if read == 0 {
                break;
            }
            body.extend_from_slice(&buffer[..read as usize]);
            if body.len() > MAX_RESPONSE_BYTES {
                return Err("GitHub 响应异常大，已中止".to_owned());
            }
        }
        String::from_utf8(body).map_err(|error| format!("响应不是 UTF-8：{error}"))
    }
}

#[cfg(test)]
mod tests {
    use super::{excerpt, is_newer, json_str, parse_release};

    #[test]
    fn version_compare_handles_prefix_and_extra_parts() {
        assert!(is_newer("v0.3.2", "0.3.1"));
        assert!(!is_newer("v0.3.1", "0.3.1"));
        assert!(!is_newer("v0.3.0", "0.3.1"));
        assert!(is_newer("0.4", "0.3.9"));
        assert!(is_newer("0.3.1.1", "0.3.1"));
        assert!(is_newer("v1.0.0", "0.9.9"));
        // 无法识别的标签不算「更新」。
        assert!(!is_newer("latest", "0.3.1"));
    }

    #[test]
    fn json_string_decodes_escapes() {
        assert_eq!(json_str(r#"{"a":"x"}"#, "a").as_deref(), Some("x"));
        assert_eq!(json_str(r#"{"tag_name": "v0.3.2"}"#, "tag_name").as_deref(), Some("v0.3.2"));
        assert_eq!(
            json_str(r#"{"body":"带\"引号\"\n换行\u62a5"}"#, "body").as_deref(),
            Some("带\"引号\"\n换行报")
        );
        assert_eq!(json_str("{}", "a"), None);
        assert_eq!(json_str(r#"{"a":123}"#, "a"), None);
    }

    #[test]
    fn release_parse_extracts_three_fields_and_excerpts_body() {
        let body = "a".repeat(260);
        let json = format!(
            r#"{{"tag_name":"v0.3.2","html_url":"https://github.com/x/y/releases/tag/v0.3.2","body":"{body}"}}"#
        );
        let info = parse_release(&json).unwrap();
        assert_eq!(info.tag_name, "v0.3.2");
        assert_eq!(info.html_url, "https://github.com/x/y/releases/tag/v0.3.2");
        assert_eq!(info.body.chars().count(), 201); // 200 字 + 省略号
        assert!(info.body.ends_with('…'));
    }

    #[test]
    fn release_parse_keeps_short_body_and_fallback_url() {
        let json = r#"{"tag_name":"v0.3.2","body":"小改动"}"#;
        let info = parse_release(json).unwrap();
        assert_eq!(info.body, "小改动");
        assert!(info.html_url.ends_with("/releases"));
    }

    #[test]
    fn release_parse_reports_github_error_message() {
        let json = r#"{"message":"Not Found","documentation_url":"https://docs.github.com"}"#;
        let error = parse_release(json).unwrap_err();
        assert!(error.contains("Not Found"), "{error}");
        assert!(parse_release("{}").is_err());
        assert!(parse_release(r#"{"tag_name":""}"#).is_err());
    }

    #[test]
    fn excerpt_does_not_add_ellipsis_when_short_enough() {
        assert_eq!(excerpt("abc", 200), "abc");
        assert_eq!(excerpt("  abc  ", 200), "abc");
        assert_eq!(excerpt("abcd", 3), "abc…");
    }
}

//! 命名管道传输层：连接、属主校验、帧收发、请求回合（Hello / Status / Query）。
//!
//! 只在后台工作线程里调用；任何失败都返回 GozError，绝不 panic、绝不阻塞 UI。
//!
//! 三处必须照抄 goz 自己的做法，否则会踩坑：
//! 1. 打开管道要带 SECURITY_SQOS_PRESENT（请求匿名模拟级），否则一个抢注了
//!    管道名的服务端可以冒用本进程令牌；
//! 2. 打开后、发任何字节前，先校验管道属主是 Local System 或 Administrators
//!    （fail-closed）。抢注者通常是非提权用户，属主是他的用户 SID；
//! 3. 读写要有截止时间。用 PeekNamedPipe 轮询而不是阻塞读，卡死的守护进程
//!    会被判超时并让状态机降级，而不是把线程挂死。

use std::fs::{File, OpenOptions};
use std::io::{Read, Write};
use std::os::windows::fs::OpenOptionsExt;
use std::os::windows::io::AsRawHandle;
use std::time::{Duration, Instant};

use super::proto::{self, Page, QueryRequest, StatusInfo};

/// 打开管道时请求匿名模拟级（值 0 就是 SECURITY_ANONYMOUS，只留 present 位）。
const SECURITY_SQOS_PRESENT: u32 = 0x0010_0000;

/// SE_OBJECT_TYPE::SE_KERNEL_OBJECT
const SE_KERNEL_OBJECT: u32 = 6;
/// OWNER_SECURITY_INFORMATION
const OWNER_SECURITY_INFORMATION: u32 = 0x0000_0001;
/// SECURITY_MAX_SID_SIZE
const MAX_SID_SIZE: usize = 68;
/// WELL_KNOWN_SID_TYPE::WinLocalSystemSid
const WIN_LOCAL_SYSTEM_SID: u32 = 22;
/// WELL_KNOWN_SID_TYPE::WinBuiltinAdministratorsSid
const WIN_BUILTIN_ADMINISTRATORS_SID: u32 = 26;

const ERROR_FILE_NOT_FOUND: i32 = 2;
const ERROR_PATH_NOT_FOUND: i32 = 3;
const ERROR_ACCESS_DENIED: i32 = 5;
const ERROR_BROKEN_PIPE: i32 = 109;
const ERROR_PIPE_BUSY: i32 = 231;
const ERROR_NO_DATA: i32 = 232;
const ERROR_PIPE_NOT_CONNECTED: i32 = 233;

/// 抢管道名失败（ERROR_PIPE_BUSY）时的退避重试次数与间隔，合计约 500 ms。
const PIPE_BUSY_RETRIES: u32 = 20;
const PIPE_BUSY_BACKOFF: Duration = Duration::from_millis(25);

/// 读一帧时两次轮询之间的间隔。
const POLL_INTERVAL: Duration = Duration::from_millis(2);

/// Hello 回合的读超时。握手要么很快，要么就是对面不认这个协议。
const HELLO_TIMEOUT: Duration = Duration::from_secs(3);

#[allow(non_snake_case)]
#[link(name = "advapi32")]
unsafe extern "system" {
    fn GetSecurityInfo(
        handle: *mut core::ffi::c_void,
        object_type: u32,
        security_info: u32,
        owner: *mut *mut core::ffi::c_void,
        group: *mut *mut core::ffi::c_void,
        dacl: *mut *mut core::ffi::c_void,
        sacl: *mut *mut core::ffi::c_void,
        descriptor: *mut *mut core::ffi::c_void,
    ) -> u32;
    fn CreateWellKnownSid(
        kind: u32,
        domain: *mut core::ffi::c_void,
        sid: *mut core::ffi::c_void,
        size: *mut u32,
    ) -> i32;
    fn EqualSid(first: *mut core::ffi::c_void, second: *mut core::ffi::c_void) -> i32;
}

#[allow(non_snake_case)]
#[link(name = "kernel32")]
unsafe extern "system" {
    fn LocalFree(memory: *mut core::ffi::c_void) -> *mut core::ffi::c_void;
    fn PeekNamedPipe(
        pipe: *mut core::ffi::c_void,
        buffer: *mut core::ffi::c_void,
        size: u32,
        read: *mut u32,
        available: *mut u32,
        left: *mut u32,
    ) -> i32;
}

/// 引擎侧的全部失败类型。UI 只按 is_transport / is_fatal 决策，不去猜字符串。
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum GozError {
    /// 管道不存在：gozd 没在运行。
    Unavailable(String),
    /// 管道在，但打开被拒绝。
    Denied(String),
    /// 管道实例一时全忙，退避重试也没抢到。
    Busy(String),
    /// 属主不是 SYSTEM / Administrators：疑似有人抢注管道名。fail-closed。
    Untrusted(String),
    /// 服务端长时间没有字节。
    Timeout(String),
    /// 连接被对端关闭 / 管道断了。
    Closed(String),
    /// 守护进程明确拒绝了这个请求（查询语法、单卷预算、协议版本）。
    Refused { code: String, message: String },
    /// 帧或应答不符合协议。
    Protocol(String),
    /// 其它 OS 错误。
    Io(String),
}

impl GozError {
    /// 连接层面的故障：可以丢掉连接、重连一次再试。
    pub fn is_transport(&self) -> bool {
        matches!(self, Self::Closed(_) | Self::Io(_) | Self::Busy(_))
    }

    /// 绝不该自动重试的失败（信任问题）。
    pub fn is_fatal(&self) -> bool {
        matches!(self, Self::Untrusted(_))
    }

    /// 是否需要把连接丢掉（读写出错、超时）。
    pub fn is_disconnect(&self) -> bool {
        matches!(self, Self::Closed(_) | Self::Io(_) | Self::Timeout(_))
    }
}

impl std::fmt::Display for GozError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Unavailable(message)
            | Self::Denied(message)
            | Self::Busy(message)
            | Self::Untrusted(message)
            | Self::Timeout(message)
            | Self::Closed(message)
            | Self::Protocol(message)
            | Self::Io(message) => formatter.write_str(message),
            Self::Refused { code, message } => {
                write!(formatter, "goz 拒绝请求（{code}）：{message}")
            }
        }
    }
}

impl std::error::Error for GozError {}

/// 把 io::Error 归到协议语义上：没装 / 断了 / 被拒 / 其它。
fn map_io(error: std::io::Error) -> GozError {
    match error.raw_os_error() {
        Some(ERROR_FILE_NOT_FOUND | ERROR_PATH_NOT_FOUND) => {
            GozError::Unavailable(format!("命名管道不存在（gozd 未运行）：{error}"))
        }
        Some(ERROR_BROKEN_PIPE | ERROR_NO_DATA | ERROR_PIPE_NOT_CONNECTED) => {
            GozError::Closed(format!("管道已断开：{error}"))
        }
        Some(ERROR_PIPE_BUSY) => GozError::Busy(format!("管道实例全部占用：{error}")),
        Some(ERROR_ACCESS_DENIED) => GozError::Denied(format!("管道拒绝访问：{error}")),
        _ => GozError::Io(format!("管道读写失败：{error}")),
    }
}

/// 管道属主是不是 Local System 或内置 Administrators。
///
/// 一个非提权用户可以抢在 gozd 之前建出同名管道并喂假结果、顺便看走查询词；
/// gozd 自己也是靠这条判据（goz-winfs/src/pipe_trust.rs）认对方的。客户端侧
/// 必须自己做一遍，任何错误都按「不可信」处理。
pub fn pipe_owner_is_trusted(file: &File) -> Result<bool, GozError> {
    let mut owner: *mut core::ffi::c_void = std::ptr::null_mut();
    let mut descriptor: *mut core::ffi::c_void = std::ptr::null_mut();
    // SAFETY: file 是活着的管道句柄；owner / descriptor 都是本地变量，
    // GetSecurityInfo 成功时写入，失败时不写。descriptor 由 LocalFree 释放。
    let status = unsafe {
        GetSecurityInfo(
            file.as_raw_handle(),
            SE_KERNEL_OBJECT,
            OWNER_SECURITY_INFORMATION,
            &mut owner,
            std::ptr::null_mut(),
            std::ptr::null_mut(),
            std::ptr::null_mut(),
            &mut descriptor,
        )
    };
    if status != 0 {
        return Err(GozError::Untrusted(format!(
            "读不到管道属主（GetSecurityInfo 错误码 {status}），按不可信处理"
        )));
    }
    let verdict = if owner.is_null() {
        Ok(false)
    } else {
        owner_matches_trusted_sid(owner)
    };
    if !descriptor.is_null() {
        // SAFETY: descriptor 由 GetSecurityInfo 用 LocalAlloc 分配，LocalFree 是
        // 文档指定的释放方式；owner 指向 descriptor 内部，只在上面的判定里用过。
        unsafe {
            LocalFree(descriptor);
        }
    }
    verdict
}

fn owner_matches_trusted_sid(owner: *mut core::ffi::c_void) -> Result<bool, GozError> {
    for kind in [WIN_LOCAL_SYSTEM_SID, WIN_BUILTIN_ADMINISTRATORS_SID] {
        let mut sid = [0u8; MAX_SID_SIZE];
        let mut size = MAX_SID_SIZE as u32;
        // SAFETY: sid 是 MAX_SID_SIZE 字节的缓冲，size 说明容量；这两个内置 SID
        // 不需要域 SID，传空。
        let ok = unsafe {
            CreateWellKnownSid(
                kind,
                std::ptr::null_mut(),
                sid.as_mut_ptr().cast(),
                &mut size,
            )
        };
        if ok == 0 {
            let code = std::io::Error::last_os_error()
                .raw_os_error()
                .unwrap_or_default();
            return Err(GozError::Untrusted(format!(
                "构造可信 SID 失败（错误码 {code}），按不可信处理"
            )));
        }
        // SAFETY: owner 来自 GetSecurityInfo，sid 刚由 CreateWellKnownSid 填好，
        // 两者都是合法 SID，EqualSid 只读。
        let equal = unsafe { EqualSid(owner, sid.as_mut_ptr().cast()) };
        if equal != 0 {
            return Ok(true);
        }
    }
    Ok(false)
}

/// 一条已打开的管道连接。
pub struct Pipe {
    file: File,
}

impl Pipe {
    /// 打开 \\.\pipe\goz-v1 并校验属主。
    ///
    /// ERROR_PIPE_BUSY 是瞬时的（守护进程只有一个监听实例，接受一个客户端后
    /// 才重建下一个），按 25 ms 退避重试到 ~500 ms，不能当成「没装」。
    pub fn open() -> Result<Self, GozError> {
        let mut attempts = 0;
        let file = loop {
            match OpenOptions::new()
                .read(true)
                .write(true)
                .custom_flags(SECURITY_SQOS_PRESENT)
                .open(proto::PIPE_NAME)
            {
                Ok(file) => break file,
                Err(error) => match error.raw_os_error() {
                    Some(ERROR_FILE_NOT_FOUND | ERROR_PATH_NOT_FOUND) => {
                        return Err(GozError::Unavailable(format!(
                            "{} 不存在（gozd 未运行）",
                            proto::PIPE_NAME
                        )));
                    }
                    Some(ERROR_PIPE_BUSY) if attempts < PIPE_BUSY_RETRIES => {
                        attempts += 1;
                        std::thread::sleep(PIPE_BUSY_BACKOFF);
                    }
                    Some(ERROR_PIPE_BUSY) => {
                        return Err(GozError::Busy(format!(
                            "管道实例全部占用（重试 {attempts} 次仍失败）"
                        )));
                    }
                    _ => return Err(map_io(error)),
                },
            }
        };
        // 安全校验必须在发出任何字节之前。
        if pipe_owner_is_trusted(&file)? {
            Ok(Self { file })
        } else {
            Err(GozError::Untrusted(
                "管道属主既不是 Local System 也不是 Administrators，已拒绝通信".to_owned(),
            ))
        }
    }

    /// 写一条 JSON 请求（一次 write，别拆两次小写）。
    pub fn send_json(&mut self, payload: &str) -> Result<(), GozError> {
        let body = payload.as_bytes();
        if body.len() as u32 > proto::MAX_CLIENT_FRAME {
            return Err(GozError::Protocol(format!(
                "请求帧 {} 字节，超过上限 {}",
                body.len(),
                proto::MAX_CLIENT_FRAME
            )));
        }
        let mut frame = Vec::with_capacity(4 + body.len());
        frame.extend_from_slice(&(body.len() as u32).to_le_bytes());
        frame.extend_from_slice(body);
        self.file.write_all(&frame).map_err(map_io)
    }

    /// 读一帧（长度前缀 + 载荷）。timeout 是「整帧」的绝对预算，不是静默时间。
    pub fn recv_frame(&mut self, timeout: Duration) -> Result<Vec<u8>, GozError> {
        let deadline = Instant::now() + timeout;
        let mut prefix = [0u8; 4];
        self.read_exact(&mut prefix, deadline)?;
        let length = u32::from_le_bytes(prefix);
        if length > proto::MAX_SERVER_FRAME {
            return Err(GozError::Protocol(format!(
                "服务端帧 {length} 字节，超过上限 {}",
                proto::MAX_SERVER_FRAME
            )));
        }
        let mut body = vec![0u8; length as usize];
        if length > 0 {
            self.read_exact(&mut body, deadline)?;
        }
        Ok(body)
    }

    fn read_exact(&mut self, buffer: &mut [u8], deadline: Instant) -> Result<(), GozError> {
        let mut filled = 0;
        while filled < buffer.len() {
            let available = self.available()?;
            if available == 0 {
                if Instant::now() >= deadline {
                    return Err(GozError::Timeout(
                        "守护进程长时间没有回字节（读超时）".to_owned(),
                    ));
                }
                std::thread::sleep(POLL_INTERVAL);
                continue;
            }
            let want = (buffer.len() - filled).min(available as usize);
            match self.file.read(&mut buffer[filled..filled + want]) {
                Ok(0) => return Err(GozError::Closed("守护进程关闭了管道".to_owned())),
                Ok(read) => filled += read,
                Err(error) => return Err(map_io(error)),
            }
        }
        Ok(())
    }

    /// 管道里现在有多少字节可读（不消费）。
    fn available(&self) -> Result<u32, GozError> {
        let mut available = 0u32;
        // SAFETY: file 是活着的管道句柄；只问字节数，缓冲区与已读数都传空。
        let ok = unsafe {
            PeekNamedPipe(
                self.file.as_raw_handle(),
                std::ptr::null_mut(),
                0,
                std::ptr::null_mut(),
                &mut available,
                std::ptr::null_mut(),
            )
        };
        if ok == 0 {
            return Err(map_io(std::io::Error::last_os_error()));
        }
        Ok(available)
    }
}

/// 一次查询的统计（行本身通过回调交出去，这里只留数字）。
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct QueryStats {
    /// 全部命中数。
    pub total: u64,
    /// 实际返回的行数。
    pub rows: usize,
    /// 收了几页。
    pub pages: u32,
    /// 命中文件字节合计（want_total_size 时才有）。
    pub total_size: Option<u64>,
    pub volumes_incomplete: bool,
    pub metadata_pending: bool,
    /// 读者中途收手（例如已经够了），连接被丢掉。
    pub aborted: bool,
    /// 首屏耗时（毫秒）。
    pub first_page_ms: f64,
    /// 整次查询耗时（毫秒）。
    pub elapsed_ms: f64,
}

/// 一条常驻连接 + 它的握手信息。惰性连接：第一次用的时候才连。
pub struct Session {
    pipe: Option<Pipe>,
    /// Hello 回来的服务端标识。
    pub server: String,
    /// 协商出的协议版本。
    pub proto: u16,
    /// 服务端自报索引已就绪。
    pub server_ready: bool,
    pub connect_ms: f64,
    pub hello_ms: f64,
    /// 最后一次成功收发的时间，空闲断连的判据。
    pub last_used: Instant,
}

impl Default for Session {
    fn default() -> Self {
        Self::new()
    }
}

impl Session {
    pub fn new() -> Self {
        Self {
            pipe: None,
            server: String::new(),
            proto: 0,
            server_ready: false,
            connect_ms: 0.0,
            hello_ms: 0.0,
            last_used: Instant::now(),
        }
    }

    pub fn is_connected(&self) -> bool {
        self.pipe.is_some()
    }

    /// 关掉连接，下一次用会自动重连（0.03 ms 量级）。
    pub fn close(&mut self) {
        self.pipe = None;
    }

    /// 空闲了多久。
    pub fn idle_for(&self) -> Duration {
        self.last_used.elapsed()
    }

    /// 确保已连接并完成 Hello 协商。已连接时是空操作。
    pub fn ensure(&mut self) -> Result<(), GozError> {
        if self.pipe.is_some() {
            return Ok(());
        }
        let started = Instant::now();
        let mut pipe = Pipe::open()?;
        self.connect_ms = millis(started);

        let started = Instant::now();
        pipe.send_json(&proto::hello_request("diskintel-rs/0.3"))?;
        let frame = pipe.recv_frame(HELLO_TIMEOUT)?;
        self.hello_ms = millis(started);
        match proto::decode_frame(&frame).map_err(GozError::Protocol)? {
            proto::Frame::Reply(proto::Reply::Hello {
                proto: version,
                server,
                ready,
            }) => {
                if version != proto::PROTO_VERSION {
                    return Err(GozError::Protocol(format!(
                        "协议版本不匹配：服务端 proto={version}，本客户端只支持 proto={}",
                        proto::PROTO_VERSION
                    )));
                }
                self.proto = version;
                self.server = server;
                self.server_ready = ready;
            }
            proto::Frame::Reply(proto::Reply::Error { code, message }) => {
                return Err(GozError::Refused { code, message });
            }
            _ => return Err(GozError::Protocol("Hello 收到了非预期应答".to_owned())),
        }
        self.pipe = Some(pipe);
        self.last_used = Instant::now();
        Ok(())
    }

    /// 问一次卷状态。
    pub fn status(&mut self) -> Result<(StatusInfo, f64), GozError> {
        self.ensure()?;
        let started = Instant::now();
        let frame = self.round_trip(proto::STATUS_REQUEST, Duration::from_secs(5))?;
        let elapsed = millis(started);
        match proto::decode_frame(&frame).map_err(GozError::Protocol)? {
            proto::Frame::Reply(proto::Reply::Status(status)) => Ok((status, elapsed)),
            proto::Frame::Reply(proto::Reply::Error { code, message }) => {
                Err(GozError::Refused { code, message })
            }
            _ => Err(GozError::Protocol("Status 收到了非预期应答".to_owned())),
        }
    }

    fn round_trip(&mut self, payload: &str, timeout: Duration) -> Result<Vec<u8>, GozError> {
        let outcome = match self.pipe.as_mut() {
            Some(pipe) => match pipe.send_json(payload) {
                Ok(()) => pipe.recv_frame(timeout),
                Err(error) => Err(error),
            },
            None => Err(GozError::Io("连接已关闭".to_owned())),
        };
        match &outcome {
            Ok(_) => {
                self.last_used = Instant::now();
            }
            Err(error) if error.is_disconnect() => self.close(),
            Err(_) => {}
        }
        outcome
    }

    /// 跑一次查询，逐页交给 on_page；on_page 返回 false 表示「够了」。
    ///
    /// 注意：中途收手后管道里还剩这次查询的后续页，无法安全复用，所以直接
    /// 丢连接（stats.aborted = true）。下一次查询会重连。
    pub fn query(
        &mut self,
        request: &QueryRequest,
        timeout: Duration,
        on_page: &mut dyn FnMut(Page) -> bool,
    ) -> Result<QueryStats, GozError> {
        self.ensure()?;
        let payload = request.to_json();
        let started = Instant::now();
        {
            let Some(pipe) = self.pipe.as_mut() else {
                return Err(GozError::Io("连接已关闭".to_owned()));
            };
            if let Err(error) = pipe.send_json(&payload) {
                self.close();
                return Err(error);
            }
        }
        let mut stats = QueryStats::default();
        loop {
            let frame = match self.pipe.as_mut() {
                Some(pipe) => pipe.recv_frame(timeout),
                None => return Err(GozError::Io("连接已关闭".to_owned())),
            };
            let frame = match frame {
                Ok(frame) => frame,
                Err(error) => {
                    self.close();
                    return Err(error);
                }
            };
            match proto::decode_frame(&frame).map_err(GozError::Protocol)? {
                proto::Frame::Page(page) => {
                    if stats.pages == 0 {
                        stats.first_page_ms = millis(started);
                    }
                    stats.pages += 1;
                    stats.total = page.total;
                    stats.rows += page.items.len();
                    stats.volumes_incomplete |= page.volumes_incomplete;
                    stats.metadata_pending |= page.metadata_pending;
                    if stats.total_size.is_none() {
                        stats.total_size = page.total_size;
                    }
                    let more = page.more;
                    if !on_page(page) {
                        self.close();
                        stats.aborted = true;
                        break;
                    }
                    if !more {
                        break;
                    }
                }
                proto::Frame::Reply(proto::Reply::Error { code, message }) => {
                    // 单帧应答，连接还是同步的，留着继续用。
                    return Err(GozError::Refused { code, message });
                }
                _ => {
                    self.close();
                    return Err(GozError::Protocol("查询收到了非结果帧".to_owned()));
                }
            }
        }
        self.last_used = Instant::now();
        stats.elapsed_ms = millis(started);
        Ok(stats)
    }
}

fn millis(started: Instant) -> f64 {
    started.elapsed().as_secs_f64() * 1000.0
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn os_errors_map_to_engine_semantics() {
        let missing = map_io(std::io::Error::from_raw_os_error(ERROR_FILE_NOT_FOUND));
        assert!(matches!(missing, GozError::Unavailable(_)));
        let gone = map_io(std::io::Error::from_raw_os_error(ERROR_BROKEN_PIPE));
        assert!(matches!(gone, GozError::Closed(_)));
        assert!(gone.is_transport());
        let busy = map_io(std::io::Error::from_raw_os_error(ERROR_PIPE_BUSY));
        assert!(matches!(busy, GozError::Busy(_)));
        let denied = map_io(std::io::Error::from_raw_os_error(ERROR_ACCESS_DENIED));
        assert!(matches!(denied, GozError::Denied(_)));
        assert!(!denied.is_transport());
    }

    #[test]
    fn error_predicates_pick_retry_and_fail_closed() {
        assert!(GozError::Timeout("t".into()).is_disconnect());
        assert!(!GozError::Timeout("t".into()).is_transport());
        assert!(GozError::Untrusted("t".into()).is_fatal());
        assert!(!GozError::Refused {
            code: "BadQuery".into(),
            message: "x".into()
        }
        .is_transport());
    }

    #[test]
    fn new_session_is_lazy() {
        let session = Session::new();
        assert!(!session.is_connected());
        assert_eq!(session.proto, 0);
        assert!(session.server.is_empty());
    }
}

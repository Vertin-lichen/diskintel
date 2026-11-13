//! 内存引擎：goz 常驻管道客户端（双引擎里的「即时」那一个）。
//!
//! 定位——存储引擎（SQLite 快照）卖「可证」，内存引擎卖「即时」：
//! - 索引在 gozd 里，本进程不占那份 467–498 MB 内存；
//! - 一次键入查询实测 3–10 ms（连接 0.03 ms、Hello 0.22 ms、Status 0.12 ms）；
//! - 查询不需要提权：gozd 提权运行，普通用户进程可读写管道。
//!
//! 设计约束（照抄调研结论，别自作聪明）：
//! 1. 绝不阻塞 UI：所有管道 I/O 在一条后台线程里，UI 只往 mpsc 发命令、
//!    每帧 poll 事件。任何失败只改状态机，不弹窗、不退化成忙等。
//! 2. 惰性连接 + 30 s 空闲断开：服务端在首个 Query 之前 30 s 就会踢掉空闲
//!    连接，与其被踢不如自己收好；重连只要 0.03 ms。
//! 3. 断线重连一次：gozd 重启、空闲被踢、管道瞬时全忙都走这条路，重连后
//!    重跑同一条查询；再失败就降级，不无限重试。
//! 4. 安全 fail-closed：管道属主必须是 SYSTEM / Administrators，否则拒绝
//!    通信（GozError::Untrusted），这是抢注管道名的唯一防线。
//! 5. 查询守卫：goz 只有 AND——「|」和「!」会 exit 4，而 /regex/ 更坏：
//!    被当普通文字、静默返回 0 条。这三样一律在客户端拦下，不下发。
#![allow(dead_code)]

pub mod goz;
pub mod proto;

use std::sync::mpsc::{self, Receiver, Sender};
use std::time::{Duration, Instant};

pub use proto::{SortDir, SortKey};

/// 引擎名（写进结果的 engine_source，供 AI 接口与状态栏如实标注）。
pub const ENGINE_NAME: &str = "goz";

/// 空闲多久主动断开连接。服务端的空转超时也是 30 s（首个 Query 之前）。
pub const IDLE_TIMEOUT: Duration = Duration::from_secs(30);

/// 单次查询默认读超时。gozd 单卷查询预算是 10 s，真到这一步基本是被拒而不是超时。
pub const DEFAULT_QUERY_TIMEOUT: Duration = Duration::from_secs(8);

/// 单次查询默认最多收多少行（避免 limit=None 的流式查询把内存吃光）。
pub const DEFAULT_MAX_ROWS: usize = 50_000;

// ───────────────────────────── 状态机 ─────────────────────────────

/// 失败分类。UI 只看这个决定「提示什么」，不去猜字符串。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum FailCause {
    /// 管道不存在：gozd 没在运行。
    Absent,
    /// 管道在但打不开（权限）。
    Denied,
    /// 管道实例一时全忙。
    Busy,
    /// 属主不是 SYSTEM / Administrators：疑似抢注，fail-closed。
    Untrusted,
    /// 读超时。
    Timeout,
    /// 连接断开。
    Closed,
    /// 帧/应答不符合协议（多见于 gozd 大版本升级）。
    Protocol,
    /// 守护进程明确拒绝了这个请求。
    Refused,
    /// 其它 IO 故障。
    Io,
}

impl FailCause {
    /// 状态栏 / 报告用的中文短标签。
    pub fn label(self) -> &'static str {
        match self {
            Self::Absent => "未运行",
            Self::Denied => "访问被拒",
            Self::Busy => "实例全忙",
            Self::Untrusted => "管道不可信",
            Self::Timeout => "超时",
            Self::Closed => "连接断开",
            Self::Protocol => "协议不符",
            Self::Refused => "请求被拒",
            Self::Io => "IO 故障",
        }
    }
}

/// 引擎状态机。UI 的每一个决策都只读这个枚举。
#[derive(Clone, Debug, Default, PartialEq)]
pub enum EngineState {
    /// 尚未探测（初始态，不连任何东西）。
    #[default]
    Unavailable,
    /// 正在握手（连接 → 属主校验 → Hello → Status）。
    Starting,
    /// 可用。
    Ready(EngineInfo),
    /// 不可用，附分类与原因。
    Failed { cause: FailCause, reason: String },
}

impl EngineState {
    pub fn label(&self) -> &'static str {
        match self {
            Self::Unavailable => "未探测",
            Self::Starting => "握手",
            Self::Ready(_) => "就绪",
            Self::Failed { .. } => "不可用",
        }
    }

    pub fn is_ready(&self) -> bool {
        matches!(self, Self::Ready(_))
    }

    /// 是不是「有人抢注了管道名」这种安全事件（UI 该红字提示，且本会话不再连）。
    pub fn is_untrusted(&self) -> bool {
        matches!(
            self,
            Self::Failed {
                cause: FailCause::Untrusted,
                ..
            }
        )
    }

    /// 一行可读描述（状态栏直接用）。
    pub fn describe(&self) -> String {
        match self {
            Self::Unavailable => "尚未探测".to_owned(),
            Self::Starting => "正在握手（连接 → 属主校验 → Hello → Status）".to_owned(),
            Self::Ready(info) => {
                let mut text = format!(
                    "{} · proto {} · {} 卷 · {} 条",
                    info.server, info.proto, info.volumes, info.entries
                );
                if info.volumes_incomplete {
                    text.push_str(" · 索引未完成");
                }
                text
            }
            Self::Failed { cause, reason } => format!("{}：{reason}", cause.label()),
        }
    }
}

/// Ready 状态下缓存的引擎信息（来自 Hello + Status）。
#[derive(Clone, Debug, Default, PartialEq)]
pub struct EngineInfo {
    pub server: String,
    pub proto: u16,
    /// 卷数。
    pub volumes: usize,
    /// 条目合计。
    pub entries: u64,
    pub volumes_incomplete: bool,
    pub metadata_pending: bool,
    /// gozd 私有字节 / 工作集。
    pub private_bytes: u64,
    pub working_set: u64,
    pub connect_ms: f64,
    pub hello_ms: f64,
    pub status_ms: f64,
}

// ───────────────────────────── 查询 ─────────────────────────────

/// 一条结果行。字段与 db::Row 对齐（path / size / mtime / is_dir）。
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct EngineRow {
    pub path: String,
    /// 字节数；目录或未取到为 None。
    pub size: Option<u64>,
    /// 原始 FILETIME（要 Unix 秒用 mtime()）。
    pub mtime_ft: Option<i64>,
    pub is_dir: bool,
}

impl EngineRow {
    /// Unix 秒，与 db::Row.mtime 同口径，可直接喂 db::format_unix。
    pub fn mtime(&self) -> Option<i64> {
        self.mtime_ft
            .map(proto::filetime_to_unix)
            .filter(|seconds| *seconds > 0)
    }
}

/// 一次查询的参数。默认值就是「键入场景」：名称升序、2000 行、要大小与时间。
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct QuerySpec {
    /// 查询串（goz 语法：空格 AND、引号短语、星号/问号通配、ext:/size:/dm:/path:/file:/folder:）。
    pub text: String,
    /// 作用域目录（es 的 -path）；None = 全盘。
    pub scope: Option<String>,
    pub sort_key: SortKey,
    pub sort_dir: SortDir,
    pub offset: u32,
    /// None = 让服务端流式返回全部（受 max_rows 兜底）。
    pub limit: Option<u32>,
    pub want_size: bool,
    pub want_mtime: bool,
    pub match_case: bool,
    pub want_total_size: bool,
    /// 客户端最多收多少行；到了就断开连接（管道里剩下的页无法安全跳过）。
    pub max_rows: Option<usize>,
    /// 读超时。
    pub timeout: Duration,
}

impl Default for QuerySpec {
    fn default() -> Self {
        Self {
            text: String::new(),
            scope: None,
            sort_key: SortKey::Name,
            sort_dir: SortDir::Asc,
            offset: 0,
            limit: Some(proto::PAGE_ROWS),
            want_size: true,
            want_mtime: true,
            match_case: false,
            want_total_size: false,
            max_rows: Some(DEFAULT_MAX_ROWS),
            timeout: DEFAULT_QUERY_TIMEOUT,
        }
    }
}

impl QuerySpec {
    pub fn new(text: impl Into<String>) -> Self {
        Self {
            text: text.into(),
            ..Self::default()
        }
    }

    /// 设置排序；方向取该键的 es 默认值。
    pub fn sorted_by(mut self, key: SortKey) -> Self {
        self.sort_key = key;
        self.sort_dir = key.default_dir();
        self
    }

    pub fn sorted(mut self, key: SortKey, dir: SortDir) -> Self {
        self.sort_key = key;
        self.sort_dir = dir;
        self
    }

    /// 分页：跳过 offset 条，最多返回 limit 条。
    pub fn page(mut self, offset: u32, limit: Option<u32>) -> Self {
        self.offset = offset;
        self.limit = limit;
        self
    }

    /// 限定作用域目录。
    pub fn within(mut self, scope: impl Into<String>) -> Self {
        self.scope = Some(scope.into());
        self
    }

    /// 转成线上请求。
    pub fn request(&self) -> proto::QueryRequest {
        proto::QueryRequest {
            query: self.text.clone(),
            scope: self.scope.clone(),
            sort_key: self.sort_key,
            sort_dir: self.sort_dir,
            offset: self.offset,
            limit: self.limit,
            want_size: self.want_size,
            want_mtime: self.want_mtime,
            match_case: self.match_case,
            want_total_size: self.want_total_size,
        }
    }
}

/// 一次查询的结果。
#[derive(Clone, Debug, Default, PartialEq)]
pub struct QueryOutcome {
    pub rows: Vec<EngineRow>,
    /// 全部命中数（不受 limit / max_rows 影响）。
    pub total: u64,
    pub pages: u32,
    /// 命中文件字节合计（want_total_size 时）。
    pub total_size: Option<u64>,
    pub volumes_incomplete: bool,
    pub metadata_pending: bool,
    /// 客户端主动截断（max_rows 到了）。
    pub truncated: bool,
    /// 首屏耗时（毫秒）。
    pub first_page_ms: f64,
    /// 整次查询耗时（毫秒，含所有页的解码）。
    pub elapsed_ms: f64,
}

impl QueryOutcome {
    /// 结果来源标签，写进 engine_source。
    pub fn engine_source(&self) -> &'static str {
        ENGINE_NAME
    }
}

/// 查询守卫：把 goz 会「明确报错」和「静默错答」的写法拦在客户端。
///
/// - 「|」（或）与「!」（排除）目前 goz 直接拒绝，下发只会浪费一次往返；
/// - /正则/ 最危险：goz 当普通文字，返回 0 条且 exit 0，用户会以为「真的没有」。
///
/// 引号里的内容一律放行（那是字面量搜索），所以 "a|b" 与 "/x/" 不会被误拦。
pub fn guard_query(text: &str) -> Result<(), String> {
    let mut quoted = false;
    let mut plain = String::with_capacity(text.len());
    for ch in text.chars() {
        match ch {
            '"' => {
                quoted = !quoted;
                plain.push(' ');
            }
            '|' if !quoted => {
                return Err(
                    "goz 不支持「或」运算（|）：请拆成多条查询在客户端归并，或改用快照引擎"
                        .to_owned(),
                );
            }
            '!' if !quoted => {
                return Err(
                    "goz 不支持「排除」运算（!）：请在结果侧过滤，或改用快照引擎".to_owned(),
                );
            }
            _ if quoted => plain.push(' '),
            _ => plain.push(ch),
        }
    }
    for token in plain.split_whitespace() {
        if token.len() >= 3 && token.starts_with('/') && token.ends_with('/') {
            return Err(format!(
                "goz 会把「{token}」当普通文字（静默返回 0 条），已拦下：去掉斜杠，或改用快照引擎的正则"
            ));
        }
    }
    Ok(())
}

/// 引擎探测结果（Hello + Status 的握手明细）。
#[derive(Clone, Debug, Default, PartialEq)]
pub struct ProbeReport {
    pub server: String,
    pub proto: u16,
    pub connect_ms: f64,
    pub hello_ms: f64,
    pub status_ms: f64,
    pub volumes: Vec<proto::VolumeInfo>,
    pub volumes_incomplete: bool,
    pub working_set: u64,
    pub private_bytes: u64,
}

impl ProbeReport {
    pub fn entries(&self) -> u64 {
        self.volumes.iter().map(|volume| volume.entries).sum()
    }

    pub fn metadata_pending(&self) -> bool {
        self.volumes.iter().any(|volume| volume.metadata_pending)
    }

    /// 转成状态机里的 EngineInfo。
    pub fn info(&self) -> EngineInfo {
        EngineInfo {
            server: self.server.clone(),
            proto: self.proto,
            volumes: self.volumes.len(),
            entries: self.entries(),
            volumes_incomplete: self.volumes_incomplete,
            metadata_pending: self.metadata_pending(),
            private_bytes: self.private_bytes,
            working_set: self.working_set,
            connect_ms: self.connect_ms,
            hello_ms: self.hello_ms,
            status_ms: self.status_ms,
        }
    }
}

/// 探测一次：必要时连接 + Hello，再问 Status。任何失败都是 Err，不 panic。
pub fn probe(session: &mut goz::Session) -> Result<ProbeReport, goz::GozError> {
    session.ensure()?;
    let (status, status_ms) = session.status()?;
    Ok(ProbeReport {
        server: session.server.clone(),
        proto: session.proto,
        connect_ms: session.connect_ms,
        hello_ms: session.hello_ms,
        status_ms,
        volumes: status.volumes,
        volumes_incomplete: status.volumes_incomplete,
        working_set: status.process_working_set,
        private_bytes: status.process_private_bytes,
    })
}

/// 跑一次查询（守卫 → 下发 → 收页 → 收行）。工作线程与 CLI 自检共用这一条。
pub fn run_query(
    session: &mut goz::Session,
    spec: &QuerySpec,
) -> Result<QueryOutcome, goz::GozError> {
    guard_query(&spec.text).map_err(|message| goz::GozError::Refused {
        code: "ClientRefused".to_owned(),
        message,
    })?;
    let request = spec.request();
    let max_rows = spec.max_rows.unwrap_or(usize::MAX);
    let mut rows: Vec<EngineRow> = Vec::new();
    let mut truncated = false;
    let stats = session.query(&request, spec.timeout, &mut |page: proto::Page| {
        let mut overflow = false;
        for item in page.items {
            if rows.len() >= max_rows {
                overflow = true;
                break;
            }
            rows.push(EngineRow {
                path: item.path,
                size: item.size,
                mtime_ft: item.mtime_ft,
                is_dir: item.is_dir,
            });
        }
        if overflow {
            truncated = true;
            return false;
        }
        true
    })?;
    Ok(QueryOutcome {
        rows,
        total: stats.total,
        pages: stats.pages,
        total_size: stats.total_size,
        volumes_incomplete: stats.volumes_incomplete,
        metadata_pending: stats.metadata_pending,
        truncated,
        first_page_ms: stats.first_page_ms,
        elapsed_ms: stats.elapsed_ms,
    })
}

/// 断线重连一次：gozd 重启、空闲被踢、管道瞬时全忙都走这里。
fn run_with_reconnect(
    session: &mut goz::Session,
    spec: &QuerySpec,
) -> Result<QueryOutcome, goz::GozError> {
    match run_query(session, spec) {
        Ok(outcome) => Ok(outcome),
        Err(error) if error.is_transport() => {
            session.close();
            session.ensure()?;
            run_query(session, spec)
        }
        Err(error) => Err(error),
    }
}

// ───────────────────────────── 后台线程 ─────────────────────────────

/// UI → 工作线程的命令。
enum Cmd {
    Probe,
    Query { id: u64, spec: QuerySpec },
    Shutdown,
}

/// 工作线程 → UI 的事件。
#[derive(Clone, Debug)]
pub enum EngineEvent {
    /// 状态机变化（只在真的变化时发）。
    State(EngineState),
    /// 一次查询的最终结果；id 与 Engine::query 的返回值对应。
    Query {
        id: u64,
        result: Result<QueryOutcome, EngineFailure>,
    },
    /// 空闲自动断开（连接已关，下一次查询会重连）。
    Idle,
}

/// 查询失败的分类 + 原因（能直接显示给用户）。
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct EngineFailure {
    pub cause: FailCause,
    pub reason: String,
}

impl From<goz::GozError> for EngineFailure {
    fn from(error: goz::GozError) -> Self {
        Self {
            cause: fail_cause(&error),
            reason: error.to_string(),
        }
    }
}

/// GozError → 状态机分类。
pub fn fail_cause(error: &goz::GozError) -> FailCause {
    match error {
        goz::GozError::Unavailable(_) => FailCause::Absent,
        goz::GozError::Denied(_) => FailCause::Denied,
        goz::GozError::Busy(_) => FailCause::Busy,
        goz::GozError::Untrusted(_) => FailCause::Untrusted,
        goz::GozError::Timeout(_) => FailCause::Timeout,
        goz::GozError::Closed(_) => FailCause::Closed,
        goz::GozError::Protocol(_) => FailCause::Protocol,
        goz::GozError::Refused { .. } => FailCause::Refused,
        goz::GozError::Io(_) => FailCause::Io,
    }
}

/// 常驻引擎句柄。UI 侧只调 probe / query / poll，三个都不会阻塞。
pub struct Engine {
    tx: Sender<Cmd>,
    rx: Receiver<EngineEvent>,
    state: EngineState,
    next_id: u64,
}

impl Default for Engine {
    fn default() -> Self {
        Self::new()
    }
}

impl Engine {
    /// 起一条工作线程（不动管道，管道等第一次查询才连）。
    pub fn new() -> Self {
        Self::with_repaint(None)
    }

    /// 带重绘钩子的版本：GUI 里传 egui 的 request_repaint，
    /// 结果到达时直接唤醒一帧，不用等下一次输入。
    pub fn with_repaint(repaint: Option<Box<dyn Fn() + Send + 'static>>) -> Self {
        let (cmd_tx, cmd_rx) = mpsc::channel();
        let (event_tx, event_rx) = mpsc::channel();
        let spawned = std::thread::Builder::new()
            .name("diskintel-engine".to_owned())
            .spawn(move || worker(cmd_rx, event_tx, repaint));
        let state = match spawned {
            Ok(_) => EngineState::Unavailable,
            Err(error) => EngineState::Failed {
                cause: FailCause::Io,
                reason: format!("无法启动引擎线程：{error}"),
            },
        };
        Self {
            tx: cmd_tx,
            rx: event_rx,
            state,
            next_id: 1,
        }
    }

    /// 当前状态（每帧读它，零开销）。
    pub fn state(&self) -> &EngineState {
        &self.state
    }

    pub fn is_ready(&self) -> bool {
        self.state.is_ready()
    }

    /// 请求探测一次（异步）。UI 启动时调一次即可。
    pub fn probe(&self) {
        let _ = self.tx.send(Cmd::Probe);
    }

    /// 发一条查询（异步），返回 id；结果以 EngineEvent::Query 回来。
    pub fn query(&mut self, spec: QuerySpec) -> u64 {
        let id = self.next_id;
        self.next_id = self.next_id.wrapping_add(1);
        let _ = self.tx.send(Cmd::Query { id, spec });
        id
    }

    /// 取走这一帧攒下的事件（顺带更新 state）。
    pub fn poll(&mut self) -> Vec<EngineEvent> {
        let mut events = Vec::new();
        while let Ok(event) = self.rx.try_recv() {
            if let EngineEvent::State(state) = &event {
                self.state = state.clone();
            }
            events.push(event);
        }
        events
    }
}

impl Drop for Engine {
    fn drop(&mut self) {
        // 线程自己会把管道关掉；这里只让它退出循环。
        let _ = self.tx.send(Cmd::Shutdown);
    }
}

fn worker(
    cmd_rx: Receiver<Cmd>,
    event_tx: Sender<EngineEvent>,
    repaint: Option<Box<dyn Fn() + Send + 'static>>,
) {
    let repaint = repaint.as_deref();
    let mut session = goz::Session::new();
    let mut state = EngineState::Unavailable;
    let mut last_used = Instant::now();
    loop {
        // 空闲断开：服务端在首个 Query 之前 30 s 就会踢连接，自己收好更干净。
        if session.is_connected() && last_used.elapsed() >= IDLE_TIMEOUT {
            session.close();
            emit_event(&event_tx, repaint, EngineEvent::Idle);
            continue;
        }
        let wait = if session.is_connected() {
            IDLE_TIMEOUT
                .saturating_sub(last_used.elapsed())
                .max(Duration::from_millis(20))
        } else {
            // 没有连接要守，睡到有命令为止（仍能被 Shutdown / Drop 唤醒）。
            Duration::from_secs(3600)
        };
        let command = match cmd_rx.recv_timeout(wait) {
            Ok(command) => command,
            Err(mpsc::RecvTimeoutError::Timeout) => continue,
            Err(mpsc::RecvTimeoutError::Disconnected) => break,
        };
        match command {
            Cmd::Shutdown => break,
            Cmd::Probe => {
                emit_state(&event_tx, &mut state, repaint, EngineState::Starting);
                match probe(&mut session) {
                    Ok(report) => emit_state(
                        &event_tx,
                        &mut state,
                        repaint,
                        EngineState::Ready(report.info()),
                    ),
                    Err(error) => {
                        session.close();
                        emit_state(&event_tx, &mut state, repaint, failed_state(&error));
                    }
                }
            }
            Cmd::Query { id, spec } => {
                if !state.is_ready() {
                    emit_state(&event_tx, &mut state, repaint, EngineState::Starting);
                }
                match run_with_reconnect(&mut session, &spec) {
                    Ok(outcome) => {
                        if !state.is_ready() {
                            if let Ok(report) = probe(&mut session) {
                                emit_state(
                                    &event_tx,
                                    &mut state,
                                    repaint,
                                    EngineState::Ready(report.info()),
                                );
                            }
                        }
                        emit_event(
                            &event_tx,
                            repaint,
                            EngineEvent::Query {
                                id,
                                result: Ok(outcome),
                            },
                        );
                    }
                    Err(error) => {
                        let failure = EngineFailure::from(error);
                        if failure.cause != FailCause::Refused {
                            // 被拒只是这一条查询的问题（语法/预算），引擎还活着。
                            session.close();
                            emit_state(
                                &event_tx,
                                &mut state,
                                repaint,
                                EngineState::Failed {
                                    cause: failure.cause,
                                    reason: failure.reason.clone(),
                                },
                            );
                        }
                        emit_event(
                            &event_tx,
                            repaint,
                            EngineEvent::Query {
                                id,
                                result: Err(failure),
                            },
                        );
                    }
                }
            }
        }
        last_used = Instant::now();
    }
}

fn failed_state(error: &goz::GozError) -> EngineState {
    EngineState::Failed {
        cause: fail_cause(error),
        reason: error.to_string(),
    }
}

fn emit_event(
    event_tx: &Sender<EngineEvent>,
    repaint: Option<&(dyn Fn() + Send)>,
    event: EngineEvent,
) {
    if event_tx.send(event).is_err() {
        return;
    }
    if let Some(repaint) = repaint {
        repaint();
    }
}

fn emit_state(
    event_tx: &Sender<EngineEvent>,
    state: &mut EngineState,
    repaint: Option<&(dyn Fn() + Send)>,
    next: EngineState,
) {
    if *state == next {
        return;
    }
    *state = next.clone();
    emit_event(event_tx, repaint, EngineEvent::State(next));
}

// ───────────────────────────── 命令行自检 ─────────────────────────────

/// --engine-probe 的实现：打印状态机、握手耗时，跑三条查询，
/// 再走一遍后台线程（UI 用的那条路径）验证 mpsc 链路。
///
/// 管道不可用时只打印原因并正常返回，绝不 panic（gozd 没运行是常态）。
pub fn print_probe() {
    use crate::db::{format_unix, human_size, thousands};

    println!("内存引擎自检（goz 常驻管道，双引擎之一）");
    println!("  管道　　　　{}", proto::PIPE_NAME);

    let mut session = goz::Session::new();
    let started = Instant::now();
    let report = match probe(&mut session) {
        Ok(report) => report,
        Err(error) => {
            let state = EngineState::Failed {
                cause: fail_cause(&error),
                reason: error.to_string(),
            };
            println!("  状态　　　　{}", state.label());
            println!("  原因　　　　{}", state.describe());
            println!(
                "  耗时　　　　{:.2} ms",
                started.elapsed().as_secs_f64() * 1000.0
            );
            println!("  降级　　　　检索自动走快照引擎（不阻塞、不弹窗、不假装数据完整）");
            return;
        }
    };

    println!("  状态　　　　{}", EngineState::Ready(report.info()).label());
    println!(
        "  服务端　　　{}（proto {}）",
        report.server, report.proto
    );
    println!("  连接　　　　{:.3} ms", report.connect_ms);
    println!("  Hello　　　 {:.3} ms", report.hello_ms);
    println!("  Status　　　{:.3} ms", report.status_ms);
    println!(
        "  卷　　　　　{} 个 · {} 条 · 索引未完成 {}",
        report.volumes.len(),
        thousands(report.entries()),
        report.volumes_incomplete
    );
    println!(
        "  守护进程　　私有 {} · 工作集 {}",
        human_size(report.private_bytes as i64),
        human_size(report.working_set as i64)
    );
    println!("  属主校验　　通过（Local System / Administrators）");
    for volume in report.volumes.iter().take(10) {
        println!(
            "    {:<10} {:>12} 条  {}{}",
            volume.mount(),
            thousands(volume.entries),
            volume.phase.to_ascii_lowercase(),
            volume
                .reason
                .as_deref()
                .map(|reason| format!("（{reason}）"))
                .unwrap_or_default()
        );
    }

    let specs = [
        ("*.exe", QuerySpec::new("*.exe").page(0, Some(100))),
        ("dll", QuerySpec::new("dll").page(0, Some(100))),
        (
            "全盘按大小前 100",
            QuerySpec::default()
                .page(0, Some(100))
                .sorted(SortKey::Size, SortDir::Desc),
        ),
    ];
    println!("查询（同一连接，时间为协议往返：写入 → 最后一页读完）：");
    for (index, (label, spec)) in specs.iter().enumerate() {
        let started = Instant::now();
        match run_query(&mut session, spec) {
            Ok(outcome) => {
                println!(
                    "  [{}] {:<16} 往返 {:>7.2} ms · 首屏 {:>6.2} ms · 命中 {} · 返回 {} 行 · {} 页{}",
                    index + 1,
                    label,
                    outcome.elapsed_ms,
                    outcome.first_page_ms,
                    thousands(outcome.total),
                    outcome.rows.len(),
                    outcome.pages,
                    if outcome.volumes_incomplete {
                        " · 索引未完成"
                    } else {
                        ""
                    }
                );
                println!(
                    "       排序 {} {} · 外部计时 {:.2} ms",
                    spec.sort_key.label(),
                    spec.sort_dir.label(),
                    started.elapsed().as_secs_f64() * 1000.0
                );
                for row in outcome.rows.iter().take(3) {
                    let size = match (row.size, row.is_dir) {
                        (Some(bytes), _) => human_size(bytes as i64),
                        (None, true) => "<目录>".to_owned(),
                        (None, false) => "-".to_owned(),
                    };
                    let mtime = row
                        .mtime()
                        .map(format_unix)
                        .unwrap_or_else(|| "-".to_owned());
                    println!("        {:>10}  {}  {}", size, mtime, row.path);
                }
            }
            Err(error) => println!("  [{}] {:<16} 失败：{error}", index + 1, label),
        }
    }

    // 后台线程路径：和 GUI 完全同一条（Engine + mpsc + 状态机）。
    println!("后台线程（Engine + mpsc，UI 走的就是这条）：");
    let mut engine = Engine::new();
    engine.probe();
    let deadline = Instant::now() + Duration::from_secs(5);
    let mut described = "等待超时".to_owned();
    while Instant::now() < deadline {
        for event in engine.poll() {
            if let EngineEvent::State(state) = event {
                described = format!("{} · {}", state.label(), state.describe());
            }
        }
        if engine.is_ready() {
            break;
        }
        std::thread::sleep(Duration::from_millis(5));
    }
    println!("  探测状态　　{described}");

    let id = engine.query(QuerySpec::new("*.exe").page(0, Some(5)));
    let deadline = Instant::now() + Duration::from_secs(10);
    let mut answered = false;
    while Instant::now() < deadline && !answered {
        for event in engine.poll() {
            if let EngineEvent::Query {
                id: event_id,
                result,
            } = event
            {
                if event_id != id {
                    continue;
                }
                match result {
                    Ok(outcome) => println!(
                        "  后台查询　　往返 {:.2} ms · 命中 {} · 返回 {} 行",
                        outcome.elapsed_ms,
                        thousands(outcome.total),
                        outcome.rows.len()
                    ),
                    Err(failure) => println!(
                        "  后台查询　　失败（{}）：{}",
                        failure.cause.label(),
                        failure.reason
                    ),
                }
                answered = true;
            }
        }
        if !answered {
            std::thread::sleep(Duration::from_millis(5));
        }
    }
    if !answered {
        println!("  后台查询　　超时（10 s 内没有回事件）");
    }

    session.close();
    println!(
        "  收尾　　　　连接已关闭；空闲 {} s 自动断开",
        IDLE_TIMEOUT.as_secs()
    );
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn guard_rejects_or_exclude_and_regex() {
        assert!(guard_query("*.exe").is_ok());
        assert!(guard_query("kernel32.dll").is_ok());
        assert!(guard_query("a|b").is_err());
        assert!(guard_query("!kernel32").is_err());
        assert!(guard_query(r"/kernel32\.dll/").is_err());
    }

    #[test]
    fn guard_allows_quoted_literals_and_paths_with_slashes() {
        assert!(guard_query(r#""a|b""#).is_ok());
        assert!(guard_query(r#""!kernel32""#).is_ok());
        assert!(guard_query(r#""/x/""#).is_ok());
        assert!(guard_query(r"C:/Users/<user>").is_ok());
        assert!(guard_query(r"path:C:/Users").is_ok());
        // 单个斜杠不是正则，放行。
        assert!(guard_query("/").is_ok());
    }

    #[test]
    fn spec_defaults_are_the_typing_case() {
        let spec = QuerySpec::new("*.exe");
        assert_eq!(spec.sort_key, SortKey::Name);
        assert_eq!(spec.sort_dir, SortDir::Asc);
        assert_eq!(spec.limit, Some(proto::PAGE_ROWS));
        assert_eq!(spec.offset, 0);
        assert!(spec.want_size && spec.want_mtime);
        assert_eq!(spec.max_rows, Some(DEFAULT_MAX_ROWS));
    }

    #[test]
    fn spec_builders_and_request_agree() {
        let spec = QuerySpec::new("dll")
            .within(r"C:\Windows")
            .sorted_by(SortKey::Size)
            .page(100, Some(500));
        assert_eq!(spec.sort_dir, SortDir::Desc);
        let request = spec.request();
        assert_eq!(request.query, "dll");
        assert_eq!(request.scope.as_deref(), Some(r"C:\Windows"));
        assert_eq!(request.sort_key, SortKey::Size);
        assert_eq!(request.sort_dir, SortDir::Desc);
        assert_eq!(request.offset, 100);
        assert_eq!(request.limit, Some(500));
    }

    #[test]
    fn state_labels_and_flags() {
        assert_eq!(EngineState::Unavailable.label(), "未探测");
        assert!(!EngineState::Unavailable.is_ready());
        let untrusted = EngineState::Failed {
            cause: FailCause::Untrusted,
            reason: "抢注".to_owned(),
        };
        assert!(untrusted.is_untrusted());
        assert!(untrusted.describe().contains("管道不可信"));
        let ready = EngineState::Ready(EngineInfo {
            server: "gozd 0.6.0".to_owned(),
            proto: 1,
            volumes: 7,
            entries: 4_576_118,
            ..EngineInfo::default()
        });
        assert!(ready.is_ready());
        assert!(ready.describe().contains("gozd 0.6.0"));
        assert!(ready.describe().contains("7 卷"));
    }

    #[test]
    fn failure_causes_map_from_errors() {
        assert_eq!(
            fail_cause(&goz::GozError::Unavailable("x".into())),
            FailCause::Absent
        );
        assert_eq!(
            fail_cause(&goz::GozError::Untrusted("x".into())),
            FailCause::Untrusted
        );
        assert_eq!(
            fail_cause(&goz::GozError::Refused {
                code: "BadQuery".into(),
                message: "y".into()
            }),
            FailCause::Refused
        );
    }

    #[test]
    fn row_mtime_is_unix_seconds() {
        let row = EngineRow {
            path: r"C:\a.txt".to_owned(),
            size: Some(10),
            mtime_ft: Some(133_500_000_000_000_000),
            is_dir: false,
        };
        assert_eq!(row.mtime(), Some(1_705_526_400));
        let unknown = EngineRow::default();
        assert_eq!(unknown.mtime(), None);
    }

    #[test]
    fn engine_reports_state_without_a_daemon_call() {
        // 建引擎不连管道：状态就是「未探测」，绝不阻塞。
        let engine = Engine::new();
        assert_eq!(*engine.state(), EngineState::Unavailable);
        assert!(!engine.is_ready());
    }
}

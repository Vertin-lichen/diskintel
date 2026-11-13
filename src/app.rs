//! 应用外壳：菜单栏 / 工具栏 / 视图路由 / 状态栏 / 截图自检。
//!
//! 外壳只负责「摆放与导航」，具体视图在 views/ 下，数据访问在 db（未来是 data）。

use std::path::PathBuf;
use std::sync::mpsc::Receiver;
use std::time::Instant;

use eframe::egui::{self, Color32, CornerRadius, Frame, Margin, RichText, Stroke, Vec2};

use crate::engine;
use crate::store::trash;
use crate::tray::TrayCommand;
use crate::views;
use crate::{backend, db, history, icons, settings, theme, tray, update, Cli};

// ---------- 外壳尺寸与动效节奏 ----------

/// 菜单栏高度 28 px（theme::MENUBAR_H 是 30，二期 E 压掉 2 px；theme.rs 是公共文件，由 lead 统一收口）。
const MENUBAR_H: f32 = theme::MENUBAR_H;
/// 工具栏高度 36 px（theme::TOOLBAR_H 是 40，二期 E 压掉 4 px）。
const TOOLBAR_H: f32 = theme::TOOLBAR_H;
/// 常规态检索框宽度上限；下限按窗口自适应（窄窗口先让检索框变窄，不挤压右侧控件）。
const SEARCH_MAX_W: f32 = 560.0;
/// Hero 缓动进度在 egui 动画内存里的 Id（必须是稳定 Id）。
const HERO_ANIM_ID: &str = "hero-search-progress";
/// Hero 动画结束判定（留一点浮点余量）。
const HERO_DONE: f32 = 0.999;
/// 判为「Hero 已经完全就位」的阈值：低于它才画历史下拉，避免面板跟着动画跑。
const HERO_IDLE: f32 = 0.05;
/// Hero 检索框垂直位置：占中央区高度的 38%（中央偏上）。
const HERO_Y_RATIO: f32 = 0.38;
/// 启动后自动聚焦检索框的延迟上限（毫秒）。
const AUTO_FOCUS_MS: u64 = 120;
/// 历史下拉最多显示条数。
const HISTORY_MAX_ROWS: usize = 8;
/// 启动自动任务（自动建快照 / 自动检查更新）的延迟：给首帧和快照加载让路。
const AUTO_TASKS_DELAY: std::time::Duration = std::time::Duration::from_secs(3);
/// 自动清理 / 周期快照的秒/天换算。
const SECONDS_PER_DAY: i64 = 86_400;
/// v4 后台转换产物的临时后缀（不以 .sqlite 结尾，快照列表不会读到）。
const V4_TEMP_EXT: &str = "v4tmp";
/// 托盘未就绪时给用户的提示。
const TRAY_HINT: &str = "托盘未启用，关闭窗口将直接退出程序";

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum View {
    Search,
    Space,
    Diff,
    Snapshots,
}

impl View {
    pub fn label(self) -> &'static str {
        match self {
            View::Search => "检索",
            View::Space => "空间",
            View::Diff => "对比",
            View::Snapshots => "快照",
        }
    }
    pub fn all() -> [View; 4] {
        [View::Search, View::Space, View::Diff, View::Snapshots]
    }
    pub fn from_name(name: &str) -> Option<Self> {
        match name {
            "search" | "检索" => Some(View::Search),
            "space" | "tree" | "空间" | "树图" => Some(View::Space),
            "diff" | "对比" => Some(View::Diff),
            "snapshots" | "snapshot" | "快照" => Some(View::Snapshots),
            _ => None,
        }
    }
}

struct Shot {
    path: PathBuf,
    frames: u64,
    requested: bool,
}

pub struct App {
    pub pal: theme::Palette,
    pub data_root: PathBuf,
    pub snapshots: Vec<db::SnapshotFile>,
    pub snap: db::Snapshot,
    pub view: View,
    pub search: views::search::SearchState,
    pub space: views::space::SpaceState,
    pub diff: views::diff::DiffState,
    pub snapshots_view: views::snapshots::SnapshotsState,
    pub startup_ms: f64,
    frame_no: u64,
    shot: Option<Shot>,
    pub build_rx: Option<Receiver<backend::BuildEvent>>,
    pub build_progress: Option<String>,
    pub toast: Option<(String, Instant)>,
    pub show_help: bool,
    /// 「快捷键一览」窗口是否打开。
    pub show_shortcuts: bool,
    pub last_refresh: Instant,
    pub engine: engine::Engine,
    /// 本机设置（快照策略 / 启动检查更新），持久化在 %APPDATA%\DiskIntel\settings.json。
    pub settings: settings::Settings,
    /// 启动自动任务的截止时刻：启动 3 秒后统一处理自动建快照 / 自动检查更新。
    auto_tasks_at: Option<Instant>,
    /// 检查更新：后台线程的结果接收端（None = 当前没有在检查）。
    update_rx: Option<Receiver<Result<update::ReleaseInfo, String>>>,
    /// 「检查更新」窗口要显示的最近一次结果。
    update_result: Option<Result<update::ReleaseInfo, String>>,
    /// 「检查更新」窗口是否打开。
    pub show_update: bool,
    /// 本次检查由启动自动触发：完成后只 toast，不弹窗。
    update_auto: bool,
    /// 检索历史（最近在前，最多 20 条），持久化在 %APPDATA%\DiskIntel\search-history.json。
    pub history: Vec<String>,
    /// 历史下拉里被 ↑/↓ 选中的下标。
    history_selected: usize,
    /// Esc 主动关掉历史下拉后置位；重新获得焦点或输入非空时复位。
    history_dismissed: bool,
    /// Hero 缓动进度：0 = 中央大框，1 = 收回顶部工具栏。
    hero_t: f32,
    /// 顶部工具栏留给检索框的矩形（工具栏每帧记录，Hero 动画用它算落点）。
    search_slot: Option<egui::Rect>,
    /// 启动后自动聚焦的截止时刻。
    auto_focus_at: Option<Instant>,
    /// 上一帧看到的「已生效查询」，用于只在真正提交时记历史。
    last_seen_applied: String,
    /// 实际生效的快照目录（自定义目录或 <数据根>\snapshots）。
    pub snapshots_dir: PathBuf,
    /// 「设置…」窗口是否打开。
    pub show_settings: bool,
    /// 本次建库是否由自动策略触发：成功 toast 用「已自动创建快照」。
    build_auto: bool,
    /// 建库开始前默认快照目录里已有的文件名（自定义目录时只搬本次新建的）。
    build_before: Vec<String>,
    /// v4 后台转换：结果接收端。
    convert_rx: Option<Receiver<Result<String, String>>>,
    /// 是否允许关闭窗口（托盘「退出」/ 文件菜单「退出」置位，绕过关闭到托盘）。
    allow_close: bool,
    /// 启动时最小化：首帧处理一次。
    start_minimized_pending: bool,
}

impl App {
    pub fn new(cc: &eframe::CreationContext<'_>, cli: Cli, snap: db::Snapshot, started: Instant) -> Self {
        let dark = match cli.theme.as_deref() {
            Some("light") => false,
            Some("dark") => true,
            _ => true,
        };
        let pal = if dark { theme::Palette::dark() } else { theme::Palette::light() };
        let scale = cc.egui_ctx.pixels_per_point().max(1.0);
        eprintln!("[t] 进入 App::new +{:.0}ms", started.elapsed().as_secs_f64() * 1000.0);
        theme::install_fonts(&cc.egui_ctx, 1.0);
        eprintln!("[t] 字体加载完 +{:.0}ms", started.elapsed().as_secs_f64() * 1000.0);
        theme::apply_style(&cc.egui_ctx, &pal, 1.0);
        apply_icons(&cc.egui_ctx);

        let data_root = cli.data_root.clone();
        // 本机设置先读：它决定快照目录、自动建库策略与托盘行为。
        let settings = settings::load();
        let snapshots_dir = settings.effective_snapshots_dir(&data_root);
        let snapshots = db::list_snapshots(&snapshots_dir);
        eprintln!(
            "[t] 快照列表 +{:.0}ms（{} 份，目录 {}）",
            started.elapsed().as_secs_f64() * 1000.0,
            snapshots.len(),
            snapshots_dir.display()
        );
        // 折叠策略先装好，首帧的检索结果就按用户配置分类。
        apply_fold_config(&settings);
        // 让卸载器知道快照在哪：失败不影响运行，只记一行日志。
        if let Err(error) = tray::write_snapshot_dir_registry(&snapshots_dir.display().to_string()) {
            eprintln!("[settings] 写注册表 SnapshotDir 失败：{error}");
        }
        let mut search = views::search::SearchState::new();
        // 折叠总开关：fold_enabled=false 时默认展开低价值行；
        // 环境变量 DISKINTEL_UI_SHOW_LOW 仍优先（测试 / 排障开关）。
        if std::env::var_os("DISKINTEL_UI_SHOW_LOW").is_none() {
            search.show_low_value = !settings.fold_enabled;
        }
        // 折叠分类配置接线点：w3 的 lowvalue::install_config 合入后在这里接上。
        apply_fold_config(&settings);
        if let Some(query) = cli.query.clone() {
            search.query = query;
        }
        if let Some(source) = cli.source.as_deref() {
            search.source = match source {
                "snapshot" => views::search::SourceMode::Snapshot,
                "live" => views::search::SourceMode::Live,
                _ => views::search::SourceMode::Auto,
            };
        }
        // Hero 态（空查询）不启动检索：列表根本不画，先查全表只会白占 CPU，还会让状态栏一直「检索中…」。
        if !search.query.trim().is_empty() {
            search.refresh(&snap, &cc.egui_ctx);
        }
        // 首帧之前的「已生效查询」不算一次提交，避免 --query 启动时污染历史。
        let last_seen_applied = search.last_applied.clone();
        let search_history = history::load();
        let space = views::space::SpaceState::new(&snap);
        let mut diff = views::diff::DiffState::new(&snapshots, &snap);
        if let Some(old) = cli.diff_old.clone() {
            diff.old = old;
        }
        if let Some(new) = cli.diff_new.clone() {
            diff.new = new;
        }
        let snapshots_view = views::snapshots::SnapshotsState::new(&snapshots, &snap);
        let view = cli
            .view
            .as_deref()
            .and_then(View::from_name)
            .unwrap_or(View::Search);
        // 首帧就按最终形态出图：空查询 + 检索视图 = Hero，否则直接是最小检索框。
        let hero_t = if view == View::Search && search.query.trim().is_empty() {
            0.0
        } else {
            1.0
        };

        // 截图钩子：DISKINTEL_UPDATE_FAKE 设置时启动即打开「检查更新」窗口走假结果。
        let fake_update = update::fake_enabled();
        // 启动 3 秒后的自动任务：首次安装必建 / 周期到期 / 每次启动建 / 检查更新。
        // --shot 截图模式跳过，保证验收截图不被全盘扫描打断。
        let auto_tasks_at = if cli.shot.is_some() {
            None
        } else {
            Some(Instant::now() + AUTO_TASKS_DELAY)
        };
        // start_minimized 只在正常启动时生效（截图必须看到窗口）。
        let start_minimized_pending = settings.start_minimized && cli.shot.is_none();

        // 内存引擎：常驻线程 + 命名管道，启动只做一次探测；不可用就静默降级到快照。
        let repaint_ctx = cc.egui_ctx.clone();
        let engine = engine::Engine::with_repaint(Some(Box::new(move || {
            repaint_ctx.request_repaint();
        })));
        engine.probe();

        let shot = cli.shot.as_ref().map(|path| Shot {
            path: path.clone(),
            frames: cli.frames.max(2) as u64,
            requested: false,
        });
        eprintln!("首帧准备 {:.0} ms（scale {scale:.2}）", started.elapsed().as_secs_f64() * 1000.0);

        let mut app = Self {
            pal,
            data_root,
            snapshots_dir,
            snapshots,
            snap,
            view,
            search,
            space,
            diff,
            snapshots_view,
            startup_ms: started.elapsed().as_secs_f64() * 1000.0,
            frame_no: 0,
            shot,
            build_rx: None,
            build_progress: None,
            toast: None,
            show_help: false,
            show_shortcuts: false,
            last_refresh: Instant::now(),
            engine,
            settings,
            auto_tasks_at,
            update_rx: None,
            update_result: None,
            show_update: false,
            update_auto: false,
            history: search_history,
            history_selected: 0,
            history_dismissed: false,
            hero_t,
            search_slot: None,
            auto_focus_at: Some(Instant::now() + std::time::Duration::from_millis(AUTO_FOCUS_MS)),
            last_seen_applied,
            show_settings: std::env::var_os("DISKINTEL_SHOW_SETTINGS").is_some(),
            build_auto: false,
            build_before: Vec::new(),
            convert_rx: None,
            allow_close: false,
            start_minimized_pending,
        };
        if fake_update {
            app.start_update_check(&cc.egui_ctx, true);
        }
        // 托盘常驻：--shot 截图与 DISKINTEL_NO_TRAY 环境变量下跳过，
        // 避免截图进程退出后留下幽灵图标（自动化测试单独跑托盘用例）。
        if cli.shot.is_none() && std::env::var_os("DISKINTEL_NO_TRAY").is_none() {
            if tray::start(Some(cc.egui_ctx.clone())) {
                eprintln!("[tray] 常驻托盘已就绪（关闭窗口默认隐藏到托盘）");
            } else {
                eprintln!("[tray] 启动失败：{}；{}", tray::last_error(), TRAY_HINT);
            }
        }
        app
    }

    fn toast(&mut self, text: impl Into<String>) {
        self.toast = Some((text.into(), Instant::now()));
    }

    fn reload_snapshots(&mut self, ctx: &egui::Context) {
        let dir = self.snapshots_dir.clone();
        self.snapshots = db::list_snapshots(&dir);
        if let Some(newest) = self.snapshots.first() {
            if newest.name != self.snap.name {
                if let Ok(snap) = db::Snapshot::open_path(&newest.path) {
                    self.snap = snap;
                    self.search.refresh(&self.snap, ctx);
                    self.space = views::space::SpaceState::new(&self.snap);
                    self.snapshots_view =
                        views::snapshots::SnapshotsState::new(&self.snapshots, &self.snap);
                    self.diff = views::diff::DiffState::new(&self.snapshots, &self.snap);
                }
            }
        }
    }

    fn switch_snapshot(&mut self, name: &str, ctx: &egui::Context) {
        let path = self.snapshots_dir.join(format!("{name}.sqlite"));
        match db::Snapshot::open_path(&path) {
            Ok(snap) => {
                self.snap = snap;
                { let ctx = ctx.clone(); self.search.refresh(&self.snap, &ctx); }
                self.space = views::space::SpaceState::new(&self.snap);
                self.snapshots_view = views::snapshots::SnapshotsState::new(&self.snapshots, &self.snap);
                self.diff = views::diff::DiffState::new(&self.snapshots, &self.snap);
                self.toast(format!("已切换到快照 {name}"));
            }
            Err(message) => self.toast(message),
        }
    }

    fn start_rebuild(&mut self, ctx: &egui::Context) {
        // Python 建库器固定写 <data-root>/snapshots，无法指定任意目录；
        // 自定义快照目录用「建完把新文件搬到目标目录」实现（见 after_build）。
        let Some(mut backend) = backend::Backend::locate(&self.data_root.join("snapshots")) else {
            self.toast("未找到建库器 DiskIntelCLI.exe；可继续浏览现有快照");
            return;
        };
        backend.data_root = self.data_root.clone();
        // 记下建库前已有文件：自定义目录时只搬本次新建的，不动历史文件。
        self.build_before = db::list_snapshots(&self.data_root.join("snapshots"))
            .into_iter()
            .map(|file| file.name)
            .collect();
        let (tx, rx) = std::sync::mpsc::channel();
        let repaint = ctx.clone();
        backend.spawn_rebuild(tx, move || repaint.request_repaint());
        self.build_rx = Some(rx);
        self.build_progress = Some("正在扫描全盘…".to_owned());
    }

    fn poll_build(&mut self, ctx: &egui::Context) {
        let Some(rx) = &self.build_rx else { return };
        let mut finished = None;
        while let Ok(event) = rx.try_recv() {
            match event {
                backend::BuildEvent::Stage(text) => self.build_progress = Some(text),
                backend::BuildEvent::Finished(result) => {
                    finished = Some(result);
                    break;
                }
            }
        }
        if let Some(result) = finished {
            self.build_rx = None;
            self.build_progress = None;
            let auto = self.build_auto;
            self.build_auto = false;
            match result {
                Ok(text) => {
                    if auto {
                        self.toast(format!("已自动创建快照：{text}"));
                    } else {
                        self.toast(text);
                    }
                    self.after_build(ctx);
                }
                Err(text) => self.toast(format!("建库失败：{text}")),
            }
        }
    }

    /// 建库完成后的收尾：搬运到自定义目录 → 自动保留清理 → 后台 v4 转换 → 重载列表。
    fn after_build(&mut self, ctx: &egui::Context) {
        let moved = self.relocate_new_snapshots();
        // 先按新目录重载，清理与转换都基于最新列表。
        self.reload_snapshots(ctx);
        let pruned = self.prune_snapshots();
        if pruned > 0 {
            self.toast(format!(
                "已清理 {pruned} 份旧快照（移入待删除，保留 {} 份）",
                self.settings.keep_snapshot_count
            ));
            self.reload_snapshots(ctx);
        }
        if moved > 0 {
            eprintln!("[snap-dir] 本次新建的 {moved} 份快照已搬到 {}", self.snapshots_dir.display());
        }
        self.start_v4_convert(ctx);
    }

    /// 自定义快照目录：把本次新建的快照从 <数据根>/snapshots 搬到 settings.snapshot_dir。
    ///
    /// 只搬本次新建（build_before 之后出现的），历史文件留在原处；
    /// 同盘 rename 瞬间完成，跨盘回退到「复制成功后再入待删除」。
    fn relocate_new_snapshots(&mut self) -> usize {
        if !self.settings.has_custom_snapshot_dir() {
            return 0;
        }
        let source = self.data_root.join("snapshots");
        let target = self.snapshots_dir.clone();
        if source == target {
            return 0;
        }
        if let Err(error) = std::fs::create_dir_all(&target) {
            eprintln!("[snap-dir] 创建自定义快照目录失败：{error}");
            return 0;
        }
        let Ok(entries) = std::fs::read_dir(&source) else {
            return 0;
        };
        let mut moved = 0;
        for entry in entries.flatten() {
            let path = entry.path();
            if path.extension().and_then(|ext| ext.to_str()) != Some("sqlite") {
                continue;
            }
            let Some(name) = path
                .file_stem()
                .and_then(|stem| stem.to_str())
                .map(str::to_owned)
            else {
                continue;
            };
            if self.build_before.contains(&name) {
                continue;
            }
            let destination = target.join(format!("{name}.sqlite"));
            if destination.exists() {
                continue;
            }
            match std::fs::rename(&path, &destination) {
                Ok(()) => moved += 1,
                Err(_) => match std::fs::copy(&path, &destination) {
                    Ok(_) => {
                        if let Err(error) = trash::move_to_trash(
                            &path,
                            &self.data_root,
                            "自定义快照目录：搬运后清理默认目录副本",
                            "DiskIntel 自动快照",
                        ) {
                            eprintln!("[snap-dir] 旧副本入待删除失败：{error}");
                        }
                        moved += 1;
                    }
                    Err(error) => {
                        eprintln!("[snap-dir] 搬运 {name}.sqlite 失败：{error}");
                    }
                },
            }
        }
        moved
    }

    /// 自动保留策略：超出 keep_snapshot_count 的旧快照移入数据根下的「待删除」目录。
    ///
    /// 当前正在查看的快照永远不清理。
    fn prune_snapshots(&mut self) -> usize {
        let keep = self.settings.keep_snapshot_count as usize;
        if keep == 0 {
            return 0;
        }
        let files = db::list_snapshots(&self.snapshots_dir);
        if files.len() <= keep {
            return 0;
        }
        let mut removed = 0;
        for file in files.into_iter().skip(keep) {
            if file.name == self.snap.name {
                continue;
            }
            match trash::move_to_trash(
                &file.path,
                &self.data_root,
                "自动保留策略：超出保留份数",
                "DiskIntel 自动快照",
            ) {
                Ok(target) => {
                    removed += 1;
                    eprintln!(
                        "[snap-prune] {} → {}",
                        file.name,
                        target.display()
                    );
                }
                Err(error) => eprintln!("[snap-prune] 清理 {} 失败：{error}", file.name),
            }
        }
        removed
    }

    /// 建库完成后按设置后台转 v4。
    ///
    /// 转换产物写到 <快照名>.v4tmp（不在快照列表里），下次启动前由
    /// finalize_pending_v4 原子替换原文件并清理旧文件 —— 这样当前会话仍可读原快照，
    /// 也避开 Windows「文件被打开时不能 rename 覆盖」的限制。
    fn start_v4_convert(&mut self, ctx: &egui::Context) {
        if !self.settings.convert_snapshot_v4 || self.convert_rx.is_some() {
            return;
        }
        let Some(file) = db::list_snapshots(&self.snapshots_dir).into_iter().next() else {
            return;
        };
        if snapshot_is_v4(&file.path) {
            eprintln!("[v4] 最新快照已是 v4，跳过转换");
            return;
        }
        let source = file.path.clone();
        let temp = source.with_extension(V4_TEMP_EXT);
        if temp.exists() {
            eprintln!("[v4] 已有待落位的转换产物 {}，跳过", temp.display());
            return;
        }
        let (tx, rx) = std::sync::mpsc::channel();
        let repaint = ctx.clone();
        eprintln!("[v4] 开始后台转换 {}", source.display());
        std::thread::spawn(move || {
            let result = match crate::store::v4::convert_v3_to_v4(&source, &temp) {
                Ok(stats) => Ok(format!(
                    "v4 转换完成：{:.1} MB → {:.1} MB（{:.2}×，{:.0} s）",
                    stats.v3_bytes as f64 / 1_048_576.0,
                    stats.out_bytes as f64 / 1_048_576.0,
                    stats.ratio,
                    stats.seconds
                )),
                Err(error) => Err(format!("v4 转换失败：{error}")),
            };
            let _ = tx.send(result);
            repaint.request_repaint();
        });
        self.convert_rx = Some(rx);
        self.toast("快照已建好，正在后台转 v4（下次启动自动生效）");
    }

    /// 轮询 v4 转换结果。
    fn poll_convert(&mut self, ctx: &egui::Context) {
        let outcome = self.convert_rx.as_ref().map(|rx| rx.try_recv());
        match outcome {
            None => {}
            Some(Ok(Ok(message))) => {
                self.convert_rx = None;
                eprintln!("[v4] {message}");
                self.toast("v4 转换完成，下次启动自动生效（体积更小）");
            }
            Some(Ok(Err(message))) => {
                self.convert_rx = None;
                eprintln!("[v4] {message}");
                self.toast(message);
            }
            Some(Err(std::sync::mpsc::TryRecvError::Empty)) => {
                ctx.request_repaint_after(std::time::Duration::from_millis(250));
            }
            Some(Err(std::sync::mpsc::TryRecvError::Disconnected)) => {
                self.convert_rx = None;
            }
        }
    }

    fn menubar(&mut self, ui: &mut egui::Ui) {
        let pal = self.pal;
        let ctx = ui.ctx().clone();
        egui::Panel::top("menubar")
            .exact_size(MENUBAR_H)
            .frame(
                Frame::NONE
                    .fill(pal.panel)
                    .inner_margin(Margin::symmetric(10, 0)),
            )
            .show(ui, |ui| {
                ui.horizontal_centered(|ui| {
                    // 菜单栏直接从「文件」开始：产品名已在窗口标题、Hero 与关于窗口里，
                    // 这里不再重复画 Logo + 名称（菜单栏高度 28 与右侧统计贴右都不变）。
                    ui.menu_button("文件", |ui| {
                        if ui.button("重建索引（扫描并新建快照）").clicked() {
                            self.start_rebuild(&ctx);
                            ui.close();
                        }
                        if ui.button("打开数据目录").clicked() {
                            let _ = open_in_explorer(&self.data_root);
                            ui.close();
                        }
                        ui.separator();
                        if ui.button("退出").clicked() {
                            // 文件菜单的「退出」= 真退出，绕过「关闭到托盘」。
                            self.allow_close = true;
                            tray::shutdown();
                            ctx.send_viewport_cmd(egui::ViewportCommand::Close);
                        }
                    });
                    ui.menu_button("视图", |ui| {
                        for view in View::all() {
                            let label = format!("{}{}", view.label(), view_hint(view));
                            if ui
                                .selectable_label(self.view == view, label)
                                .clicked()
                            {
                                self.view = view;
                                ui.close();
                            }
                        }
                        ui.separator();
                        if ui.button("切换深色 / 浅色").clicked() {
                            self.pal = self.pal.toggle();
                            theme::apply_style(&ctx, &self.pal, 1.0);
                            ui.close();
                        }
                    });
                    ui.menu_button("工具", |ui| {
                        if ui.button("设置…（快照 / 折叠 / 托盘）").clicked() {
                            self.show_settings = true;
                            ui.close();
                        }
                        ui.separator();
                        if ui.button("重新加载快照列表").clicked() {
                            self.reload_snapshots(&ctx);
                            ui.close();
                        }
                        if ui.button("自检（路径 / 建库器 / 快照）").clicked() {
                            let exe = std::env::current_exe()
                                .map(|p| p.display().to_string())
                                .unwrap_or_default();
                            self.toast(format!("程序：{exe}｜快照：{}", self.snap.name));
                            ui.close();
                        }
                        if ui.button("复制当前检索命令").clicked() {
                            let cmd = format!(
                                "diskintel --json search \"{}\" --limit 200",
                                self.search.query
                            );
                            ui.ctx().copy_text(cmd.clone());
                            self.toast(format!("已复制：{cmd}"));
                            ui.close();
                        }
                    });
                    ui.menu_button("帮助", |ui| {
                        if ui.button("快捷键一览（键位 + 检索语法）").clicked() {
                            self.show_shortcuts = true;
                            ui.close();
                        }
                        if ui.button("关于").clicked() {
                            self.show_help = !self.show_help;
                            ui.close();
                        }
                        ui.separator();
                        if ui.button("检查更新…").clicked() {
                            self.start_update_check(&ctx, true);
                            ui.close();
                        }
                        if ui.button("更新说明（手动热更新）").clicked() {
                            if let Err(error) = update::open_url(update::UPDATE_DOC_URL) {
                                self.toast(error);
                            }
                            ui.close();
                        }
                        ui.separator();
                        // 机主疑问「快照是自动还是手动」：在帮助里写清口径与开启方式。
                        ui.label(
                            RichText::new("快照策略：首次安装自动建一份；之后每 N 天自动建（默认 7 天）。")
                                .size(10.5)
                                .color(pal.text_faint),
                        );
                        ui.label(
                            RichText::new("「设置 → 快照」可改间隔 / 保留份数 / 自定义目录 / v4 转换；")
                                .size(10.5)
                                .color(pal.text_faint),
                        );
                        ui.label(
                            RichText::new("勾选「每次启动都建」则每次启动 3 秒后后台扫描建库。")
                                .size(10.5)
                                .color(pal.text_faint),
                        );
                        ui.label(
                            RichText::new(if tray::is_ready() {
                                "托盘：关闭窗口默认隐藏到托盘（进程常驻）；托盘菜单「退出」才真退出。"
                            } else {
                                "托盘：未启用，关闭窗口将直接退出程序。"
                            })
                            .size(10.5)
                            .color(pal.text_faint),
                        );
                    });

                    ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                        let engine_ready = true;
                        crate::widgets::status_dot(ui, &pal, engine_ready, "本地引擎");
                        ui.add_space(4.0);
                        ui.label(
                            RichText::new(format!(
                                "{} 条 · {}",
                                db::thousands(self.snap.total_entries),
                                db::human_size(self.snap.corpus_bytes as i64)
                            ))
                            .size(11.0)
                            .color(pal.text_faint),
                        );
                    });
                });
            });
    }

    fn toolbar(&mut self, ui: &mut egui::Ui) {
        let pal = self.pal;
        let ctx = ui.ctx().clone();
        // Hero 没收完之前，检索框由 hero_overlay 画在「正在移动的位置」上；
        // 工具栏这里只占位不绘制 —— 于是两种形态的工具栏布局完全一致，不会跳。
        let draw_field = self.hero_t >= HERO_DONE;
        let mut field_response: Option<egui::Response> = None;
        egui::Panel::top("toolbar")
            .exact_size(TOOLBAR_H)
            .frame(
                Frame::NONE
                    .fill(pal.panel)
                    .inner_margin(Margin::symmetric(10, 0)),
            )
            .show(ui, |ui| {
                // 宽度预算先算后摆：左侧标签 + 中间检索框 + 右侧快照/扫描，窗口再窄也不会互相挤掉。
                let avail = ui.available_width();
                let right_w = 150.0 + 6.0 + 84.0 + 8.0;
                let left_w = View::all()
                    .iter()
                    .map(|view| view.label().chars().count() as f32 * 13.5 + 26.0)
                    .sum::<f32>()
                    + 24.0;
                // 二期 E：常规态检索框 420–560（窗口不够时先让它变窄，右侧控件不挤压）。
                let search_w = (avail - left_w - right_w - 24.0).clamp(160.0, SEARCH_MAX_W);
                ui.horizontal_centered(|ui| {
                    for view in View::all() {
                        if crate::widgets::view_tab(ui, &pal, view.label(), self.view == view)
                            .clicked()
                        {
                            self.view = view;
                        }
                    }
                    ui.add_space(6.0);
                    crate::widgets::vsep(ui, &pal);
                    ui.add_space(6.0);
                    // 先占位，拿到检索框的「最终落点」，Hero 动画按它算插值终点。
                    let (slot, _) = ui.allocate_exact_size(
                        Vec2::new(search_w, crate::widgets::FIELD_H),
                        egui::Sense::hover(),
                    );
                    self.search_slot = Some(slot);
                    if draw_field {
                        let mut child = ui.new_child(
                            egui::UiBuilder::new()
                                .max_rect(slot)
                                .layout(egui::Layout::left_to_right(egui::Align::Center)),
                        );
                        field_response = Some(crate::widgets::search_field(
                            &mut child,
                            &pal,
                            &mut self.search.query,
                            crate::widgets::SEARCH_HINT,
                            search_w,
                        ));
                    }

                    // 把剩下的空间推给右侧，保证右端控件贴边。
                    let used = ui.min_rect().width();
                    let gap = (ui.max_rect().width() - used - right_w).max(4.0);
                    ui.add_space(gap);

                    let mut selected = self.snap.name.clone();
                    egui::ComboBox::from_id_salt("snapshot-pick")
                        .selected_text(RichText::new(snapshot_label(&selected)).size(12.0))
                        .width(148.0)
                        .show_ui(ui, |ui| {
                            for file in &self.snapshots {
                                let label = format!(
                                    "{}  ·  {} 条",
                                    file.name,
                                    db::thousands(file.entries)
                                );
                                ui.selectable_value(&mut selected, file.name.clone(), label);
                            }
                        });
                    if selected != self.snap.name {
                        self.switch_snapshot(&selected, &ctx);
                    }

                    ui.add_space(4.0);
                    if self.build_rx.is_some() {
                        ui.spinner();
                        ui.label(
                            RichText::new(self.build_progress.clone().unwrap_or_default())
                                .size(11.0)
                                .color(pal.text_dim),
                        );
                    } else if crate::widgets::primary_button(ui, &pal, "扫描", Some(icons::Glyph::Scan))
                        .clicked()
                    {
                        self.start_rebuild(&ctx);
                    }
                });
            });
        // 输入变化必须在面板闭包之外处理：这里 self 的借用已经结束。
        if let Some(response) = field_response {
            self.on_search_response(&response, &ctx);
        }
    }

    fn statusbar(&mut self, ui: &mut egui::Ui) {
        let pal = self.pal;
        egui::Panel::bottom("statusbar")
            .exact_size(theme::STATUSBAR_H)
            .frame(
                Frame::NONE
                    .fill(pal.panel_alt)
                    .inner_margin(Margin::symmetric(10, 0)),
            )
            .show(ui, |ui| {
                ui.horizontal_centered(|ui| {
                    let hit_text = if self.view == View::Search {
                        if self.hero_t < HERO_DONE {
                            // Hero 态没有结果区，也就没有「命中数」可言。
                            "—".to_owned()
                        } else if self.search.busy() {
                            "检索中…".to_owned()
                        } else {
                            format!(
                                "命中 {} · {:.0} ms",
                                db::thousands(self.search.total),
                                self.search.sql_ms
                            )
                        }
                    } else {
                        "—".to_owned()
                    };
                    let engine_cell = match self.engine.state() {
                        engine::EngineState::Ready(info) => {
                            format!("引擎 goz · {} 万条", info.entries / 10_000)
                        }
                        engine::EngineState::Starting => "引擎 连接中".to_owned(),
                        engine::EngineState::Unavailable => "引擎 未启用".to_owned(),
                        engine::EngineState::Failed { .. } => "引擎 不可用".to_owned(),
                    };
                    let cells: Vec<String> = vec![
                        format!(
                            "对象 {} · {}",
                            db::thousands(self.snap.total_entries),
                            db::human_size(self.snap.corpus_bytes as i64)
                        ),
                        hit_text,
                        format!("快照 {}", self.snap.name),
                        engine_cell,
                        format!(
                            "库体积 {}",
                            db::human_size(self.snap.file_bytes as i64)
                        ),
                        format!("RSS {:.0} MB", rss_mb()),
                        format!("首帧 {:.0} ms", self.startup_ms),
                    ];
                    for (index, text) in cells.iter().enumerate() {
                        if index > 0 {
                            ui.add_space(10.0);
                            ui.painter().rect_filled(
                                egui::Rect::from_center_size(
                                    egui::pos2(ui.cursor().left(), ui.max_rect().center().y),
                                    Vec2::new(1.0, 12.0),
                                ),
                                CornerRadius::same(0),
                                pal.border,
                            );
                            ui.add_space(10.0);
                        }
                        ui.label(RichText::new(text).size(11.0).color(pal.text_dim));
                    }
                    let narrow = ui.max_rect().width() < 1150.0;
                    ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                        if narrow {
                            return;
                        }
                        let cmd = if self.view == View::Search {
                            format!(
                                "diskintel --json search \"{}\" --limit 200",
                                self.search.query
                            )
                        } else {
                            format!("diskintel --json {}", view_verb(self.view))
                        };
                        ui.label(
                            RichText::new(cmd)
                                .family(egui::FontFamily::Monospace)
                                .size(10.5)
                                .color(pal.text_faint),
                        );
                    });
                });
            });
    }

    // ---------- Hero 检索态 ----------

    /// Hero 目标值：0 = 中央大框（检索视图 + 空查询），1 = 收回顶部工具栏。
    fn hero_target(&self) -> f32 {
        if self.view == View::Search && self.search.query.trim().is_empty() {
            0.0
        } else {
            1.0
        }
    }

    /// 启动后 120 ms 内自动聚焦检索框：首帧就聚焦，用户不用先点一下输入框。
    fn handle_auto_focus(&mut self, ctx: &egui::Context) {
        let Some(deadline) = self.auto_focus_at else { return };
        let now = Instant::now();
        let first = self.frame_no == 1;
        if first || now >= deadline {
            // 首帧无条件聚焦；120 ms 补那一次只在「还没有任何控件拿到焦点」时生效，
            // 避免用户已经点了别的控件又被抢回来。
            let nothing_focused = ctx.memory(|m| m.focused()).is_none();
            if self.view == View::Search && (first || nothing_focused) {
                crate::widgets::focus_search(ctx);
            }
            if now >= deadline {
                self.auto_focus_at = None;
                return;
            }
        }
        ctx.request_repaint_after(deadline.saturating_duration_since(Instant::now()));
    }

    /// 检索框这一帧的交互结果：输入即置脏；Hero 态输入即开查，动画落地时结果就绪。
    fn on_search_response(&mut self, response: &egui::Response, ctx: &egui::Context) {
        if response.changed() {
            self.search.dirty = true;
            self.history_dismissed = false;
            if self.hero_t < HERO_DONE && !self.search.query.trim().is_empty() {
                // 220 ms 后列表出来，查询在动画期间并行跑完，不会「先空转再看见结果」。
                self.search.refresh(&self.snap, ctx);
                self.search.dirty = false;
            }
        }
        if response.gained_focus() {
            self.history_dismissed = false;
            self.history_selected = 0;
        }
    }

    /// 只记录「真正提交并生效」的查询（回车或防抖落地），空查询不算一次检索。
    fn record_history(&mut self) {
        let applied = self.search.last_applied.trim().to_owned();
        if applied == self.last_seen_applied {
            return;
        }
        self.last_seen_applied = applied.clone();
        if applied.is_empty() {
            return;
        }
        self.history = history::push(&applied);
        self.history_selected = 0;
        self.history_dismissed = false;
    }

    /// 应用一条历史：文本回填 + 立即开查 + 保持焦点。
    fn apply_history(&mut self, query: &str, ctx: &egui::Context) {
        self.search.query = query.to_owned();
        self.history_dismissed = true;
        self.search.dirty = true;
        self.search.refresh(&self.snap, ctx);
        self.search.dirty = false;
        crate::widgets::focus_search(ctx);
    }

    /// 历史下拉是否该出现：检索视图 + Hero 已就位 + 空查询 + 检索框有焦点。
    fn history_open(&self, ctx: &egui::Context) -> bool {
        self.view == View::Search
            && self.hero_t < HERO_IDLE
            && self.search.query.trim().is_empty()
            && !self.history.is_empty()
            && !self.history_dismissed
            && crate::widgets::search_focused(ctx)
    }

    /// F5：按当前视图刷新，而不是无脑全盘重建索引。
    fn refresh_current_view(&mut self, ctx: &egui::Context) {
        match self.view {
            View::Search => {
                if self.search.query.trim().is_empty() {
                    self.toast("Hero 态：先输入关键词再刷新");
                } else {
                    self.search.refresh(&self.snap, ctx);
                    self.search.dirty = false;
                    self.toast("已重新查询");
                }
            }
            View::Space => {
                self.space = views::space::SpaceState::new(&self.snap);
                self.toast("已重载卷信息");
            }
            View::Snapshots => {
                self.reload_snapshots(ctx);
                self.toast("已重新扫描快照目录");
            }
            View::Diff => {
                self.diff = views::diff::DiffState::new(&self.snapshots, &self.snap);
                self.toast("已重新对比");
            }
        }
        self.last_refresh = Instant::now();
    }

    fn handle_shortcuts(&mut self, ctx: &egui::Context) {
        // 历史下拉开着时，↑/↓/回车/Esc 先归面板：consume_key 把事件从输入流里删掉，
        // 单行 TextEdit 就不会再拿这些键去移光标或让出焦点（不抢 Enter）。
        if self.history_open(ctx) {
            let rows = self.history.len().min(HISTORY_MAX_ROWS);
            if ctx.input_mut(|i| i.consume_key(egui::Modifiers::NONE, egui::Key::ArrowDown)) {
                self.history_selected = (self.history_selected + 1).min(rows.saturating_sub(1));
            }
            if ctx.input_mut(|i| i.consume_key(egui::Modifiers::NONE, egui::Key::ArrowUp)) {
                self.history_selected = self.history_selected.saturating_sub(1);
            }
            if ctx.input_mut(|i| i.consume_key(egui::Modifiers::NONE, egui::Key::Enter)) {
                if let Some(query) = self.history.get(self.history_selected).cloned() {
                    self.apply_history(&query, ctx);
                }
                return;
            }
            if ctx.input_mut(|i| i.consume_key(egui::Modifiers::NONE, egui::Key::Escape)) {
                self.history_dismissed = true;
                return;
            }
        }

        let (ctrl_f, ctrl_l, ctrl_1, ctrl_2, ctrl_3, ctrl_4, f5, esc) = ctx.input(|i| {
            (
                i.modifiers.command && i.key_pressed(egui::Key::F),
                i.modifiers.command && i.key_pressed(egui::Key::L),
                i.modifiers.command && i.key_pressed(egui::Key::Num1),
                i.modifiers.command && i.key_pressed(egui::Key::Num2),
                i.modifiers.command && i.key_pressed(egui::Key::Num3),
                i.modifiers.command && i.key_pressed(egui::Key::Num4),
                i.key_pressed(egui::Key::F5),
                i.key_pressed(egui::Key::Escape),
            )
        });

        if ctrl_f || ctrl_l {
            self.view = View::Search;
            self.history_dismissed = false;
            crate::widgets::focus_search(ctx);
        }
        // Ctrl+1..4 在输入框有焦点时照常生效：切视图是外壳行为，不该被输入框吞掉。
        if ctrl_1 { self.view = View::Search; }
        if ctrl_2 { self.view = View::Space; }
        if ctrl_3 { self.view = View::Diff; }
        if ctrl_4 { self.view = View::Snapshots; }
        if f5 {
            self.refresh_current_view(ctx);
        }
        // Esc：只有检索框真的有焦点时才清空并失焦；否则无副作用（不抢别的控件的 Esc）。
        if esc && crate::widgets::search_focused(ctx) {
            if !self.search.query.is_empty() {
                self.search.query.clear();
                self.search.dirty = true;
            }
            self.history_dismissed = true;
            crate::widgets::blur_search(ctx);
        }
    }

    fn handle_shot(&mut self, ctx: &egui::Context) {
        let Some(shot) = &self.shot else { return };
        if shot.requested {
            return;
        }
        // 等后台查询落地 + Hero 动画走完再截图，否则截到的是「正在检索」或「正在上移」的中间态；
        // 最多多等 400 帧兜底，保证 --shot 稳定。
        let hero_settled = self.hero_t <= 0.0001 || self.hero_t >= HERO_DONE;
        let ready = (!self.search.busy() && !self.diff.busy() && hero_settled)
            || self.frame_no > shot.frames + 4000;
        if self.frame_no >= shot.frames && ready {
            let path = shot.path.clone();
            self.shot.as_mut().unwrap().requested = true;
            ctx.request_screenshot(move |image| {
                if let Some(dir) = path.parent() {
                    let _ = std::fs::create_dir_all(dir);
                }
                let width = image.width() as u32;
                let height = image.height() as u32;
                let mut bytes = Vec::with_capacity(width as usize * height as usize * 4);
                for pixel in &image.pixels {
                    bytes.extend_from_slice(&[pixel.r(), pixel.g(), pixel.b(), pixel.a()]);
                }
                match image::RgbaImage::from_raw(width, height, bytes) {
                    Some(buffer) => {
                        if let Err(error) = buffer.save(&path) {
                            eprintln!("截图保存失败：{error}");
                        } else {
                            eprintln!("截图已保存：{} ({width}x{height})", path.display());
                        }
                    }
                    None => eprintln!("截图缓冲尺寸不匹配"),
                }
                eprintln!("[rss] {:.1} MB", rss_mb());
                std::process::exit(0);
            });
        }
        ctx.request_repaint();
    }

    fn help_window(&mut self, ctx: &egui::Context) {
        if !self.show_help {
            return;
        }
        let pal = self.pal;
        let mut open = self.show_help;
        egui::Window::new("关于 · 磁盘情报系统")
            .open(&mut open)
            .collapsible(false)
            .resizable(false)
            // Hero 的检索框 / 历史下拉在 Middle 层，弹窗必须压在最上面才是「窗口」。
            .order(egui::Order::Foreground)
            .anchor(egui::Align2::CENTER_CENTER, Vec2::ZERO)
            .show(ctx, |ui| {
                ui.set_min_width(360.0);
                ui.label(RichText::new("一个软件同时服务人类与 AI").size(13.0).strong());
                ui.add_space(6.0);
                ui.label(
                    RichText::new(format!(
                        "快照 {} · {} 条 · {}",
                        self.snap.name,
                        db::thousands(self.snap.total_entries),
                        db::human_size(self.snap.corpus_bytes as i64)
                    ))
                    .size(11.5)
                    .color(pal.text_dim),
                );
                ui.add_space(8.0);
                ui.label(
                    RichText::new("键位与检索语法见「帮助 → 快捷键一览」。")
                        .size(11.5)
                        .color(pal.text_faint),
                );
                ui.add_space(10.0);
                ui.separator();
                ui.add_space(6.0);
                ui.label(
                    RichText::new(format!(
                        "版本 {} · 仓库 {}",
                        update::CURRENT_VERSION,
                        update::REPO_SLUG
                    ))
                    .size(11.5)
                    .color(pal.text_dim),
                );
                ui.add_space(4.0);
                ui.horizontal(|ui| {
                    ui.label(
                        RichText::new(update::REPO_URL)
                            .family(egui::FontFamily::Monospace)
                            .size(10.5)
                            .color(pal.accent),
                    );
                    if ui.small_button("打开仓库").clicked()
                        && let Err(error) = update::open_url(update::REPO_URL)
                    {
                        self.toast(error);
                    }
                });
            });
        self.show_help = open;
    }

    /// 帮助 → 快捷键一览：键位表 + 检索语法表（口径与 filter.rs 的实际解析一致）。
    fn shortcuts_window(&mut self, ctx: &egui::Context) {
        if !self.show_shortcuts {
            return;
        }
        let pal = self.pal;
        let mut open = self.show_shortcuts;
        egui::Window::new("快捷键一览")
            .open(&mut open)
            .collapsible(false)
            .resizable(false)
            .order(egui::Order::Foreground)
            .anchor(egui::Align2::CENTER_CENTER, Vec2::ZERO)
            .show(ctx, |ui| {
                ui.set_min_width(520.0);
                ui.label(RichText::new("键位").size(13.0).strong().color(pal.text));
                ui.add_space(4.0);
                for (key, text) in [
                    ("Ctrl+F / Ctrl+L", "聚焦检索框（任何视图都会先切到检索）"),
                    ("Esc", "检索框有焦点时清空并失焦；历史下拉开着时先关面板"),
                    ("Ctrl+1 / 2 / 3 / 4", "切换 检索 / 空间 / 对比 / 快照"),
                    ("F5", "刷新当前视图：检索重查、快照重扫目录、空间重载卷"),
                    ("Enter", "提交检索；历史下拉开着时应用选中项"),
                    ("Up / Down", "历史下拉里选历史；检索框无焦点时移动结果选中行"),
                    ("Ctrl+C", "复制选中项路径"),
                ] {
                    ui.horizontal(|ui| {
                        ui.label(
                            RichText::new(key)
                                .family(egui::FontFamily::Monospace)
                                .size(11.0)
                                .color(pal.accent),
                        );
                        ui.label(RichText::new(text).size(12.0).color(pal.text_dim));
                    });
                }
                ui.add_space(10.0);
                ui.separator();
                ui.add_space(6.0);
                ui.label(RichText::new("检索语法").size(13.0).strong().color(pal.text));
                ui.add_space(4.0);
                for (syntax, text) in [
                    ("关键词 关键词", "空格分隔 = 同时满足（AND）"),
                    ("ext:pdf", "按扩展名过滤，可写多个"),
                    ("size:>10m", "大于 10 MB（k / m / g 均可）；size:<500k 同理"),
                    ("after:2026-01-01", "修改时间不早于该日"),
                    ("before:2026-06-30", "修改时间不晚于该日"),
                    ("path:D:\\资料", "限定在某个目录下（右侧「范围」框同义）"),
                    ("kind:file", "只要文件；kind:dir 只要目录"),
                ] {
                    ui.horizontal(|ui| {
                        ui.label(
                            RichText::new(syntax)
                                .family(egui::FontFamily::Monospace)
                                .size(11.0)
                                .color(pal.accent),
                        );
                        ui.label(RichText::new(text).size(12.0).color(pal.text_dim));
                    });
                }
                ui.add_space(8.0);
                ui.label(
                    RichText::new("Hero 态直接打字即可：输入后检索框会自动收回到顶部工具栏。")
                        .size(11.0)
                        .color(pal.text_faint),
                );
            });
        self.show_shortcuts = open;
    }

    // ---------- Hero 层的绘制 ----------

    /// Hero 层：品牌、检索框（带 220 ms 过渡）、提示行、历史下拉。
    ///
    /// 全部用 Area 画在面板之上，好处有两个：
    /// 1. 检索框从「中央 38% 处」滑到「顶部工具栏」时可以跨越中央面板边界，不被裁剪；
    /// 2. 两种形态共用 widgets::search_id()，焦点 / 光标 / 选中文本一路连续。
    fn hero_overlay(&mut self, ctx: &egui::Context, central: egui::Rect) {
        if self.hero_t >= HERO_DONE {
            return; // 已经收回工具栏，由 toolbar 自己画
        }
        let Some(slot) = self.search_slot else { return };
        let pal = self.pal;
        let t = self.hero_t;
        let eased = smoothstep(t);
        // Hero 原位的锚点（品牌/提示/历史下拉都按它摆，不跟着动画跑）。
        let hero_w = hero_width(central);
        let hero_h = crate::widgets::HERO_FIELD_H;
        let hero_top = egui::pos2(
            central.center().x - hero_w * 0.5,
            central.top() + central.height() * HERO_Y_RATIO - hero_h * 0.5,
        );
        let (pos, width) = hero_field_geometry(central, slot, t);

        // 品牌与提示只在真正的 Hero 态出现，并按 t 前 1/3 淡出，不参与几何插值。
        if self.view == View::Search {
            let fade = (1.0 - eased * 3.0).clamp(0.0, 1.0);
            let dropdown = self.history_open(ctx);
            if fade > 0.02 {
                self.hero_brand(ctx, hero_top, hero_w, fade);
                if !dropdown {
                    self.hero_hint(ctx, hero_top, hero_h, hero_w, fade);
                }
            }
        }

        let mut field_response: Option<egui::Response> = None;
        egui::Area::new(egui::Id::unique("hero-search"))
            .order(egui::Order::Middle)
            .movable(false)
            .fade_in(false)
            .constrain(false)
            .fixed_pos(pos)
            .show(ctx, |ui| {
                ui.spacing_mut().item_spacing = Vec2::ZERO;
                field_response = Some(if t <= HERO_IDLE {
                    crate::widgets::hero_search(ui, &pal, &mut self.search.query, width)
                } else {
                    crate::widgets::search_field_animated(ui, &pal, &mut self.search.query, width, t)
                });
            });
        if let Some(response) = field_response {
            self.on_search_response(&response, ctx);
        }

        if self.view == View::Search && self.history_open(ctx) {
            self.history_dropdown(ctx, central, hero_top, hero_h, hero_w);
        }
    }

    /// Hero 品牌块：Logo + 产品名 + 一句话说明。
    fn hero_brand(&self, ctx: &egui::Context, hero_top: egui::Pos2, width: f32, fade: f32) {
        let pal = self.pal;
        // 品牌块实际高度约 120 px，与检索框之间留 22 px。
        let brand_h = 120.0;
        let pos = egui::pos2(hero_top.x, hero_top.y - 22.0 - brand_h);
        egui::Area::new(egui::Id::unique("hero-brand"))
            .order(egui::Order::Middle)
            .interactable(false)
            .movable(false)
            .fade_in(false)
            .constrain(false)
            .fixed_pos(pos)
            .show(ctx, |ui| {
                ui.set_width(width);
                ui.multiply_opacity(fade);
                ui.vertical_centered(|ui| {
                    let (tile, _) =
                        ui.allocate_exact_size(Vec2::splat(56.0), egui::Sense::hover());
                    ui.painter()
                        .rect_filled(tile, CornerRadius::same(14), pal.accent_weak);
                    icons::paint_logo(
                        ui.painter(),
                        egui::Rect::from_center_size(tile.center(), Vec2::splat(34.0)),
                        pal.accent,
                        pal.panel,
                    );
                    ui.add_space(12.0);
                    ui.label(RichText::new("磁盘情报系统").size(22.0).strong().color(pal.text));
                    ui.add_space(4.0);
                    ui.label(
                        RichText::new("输入关键词，全盘任何文件毫秒级呈现")
                            .size(13.0)
                            .color(pal.text_dim),
                    );
                });
            });
    }

    /// Hero 提示行：检索语法速查 + 快捷聚焦方式。
    fn hero_hint(
        &self,
        ctx: &egui::Context,
        hero_top: egui::Pos2,
        hero_h: f32,
        width: f32,
        fade: f32,
    ) {
        let pal = self.pal;
        let pos = egui::pos2(hero_top.x, hero_top.y + hero_h + 14.0);
        egui::Area::new(egui::Id::unique("hero-hint"))
            .order(egui::Order::Middle)
            .interactable(false)
            .movable(false)
            .fade_in(false)
            .constrain(false)
            .fixed_pos(pos)
            .show(ctx, |ui| {
                ui.set_width(width);
                ui.multiply_opacity(fade);
                ui.vertical_centered(|ui| {
                    ui.label(
                        RichText::new(
                            "空格=同时满足 · ext:pdf · size:>10m · after:2026-01-01 · path:D:\\ ｜ Ctrl+F 聚焦 · Esc 清空",
                        )
                        .size(11.5)
                        .color(pal.text_faint),
                    );
                });
            });
    }

    /// 历史下拉：聚焦 + 空查询时挂在 Hero 检索框下方。
    ///
    /// 行高 28 px，能塞几行按「到状态栏的距离」算 —— 900×560 的小窗口也不会压到状态栏。
    fn history_dropdown(
        &mut self,
        ctx: &egui::Context,
        central: egui::Rect,
        hero_top: egui::Pos2,
        hero_h: f32,
        width: f32,
    ) {
        let pal = self.pal;
        let row_h = 28.0;
        let footer_h = 26.0;
        let margin = 6.0;
        let pos = egui::pos2(hero_top.x, hero_top.y + hero_h + 8.0);
        let room = (central.bottom() - 8.0 - pos.y - footer_h - margin * 2.0).max(row_h);
        let rows = ((room / row_h).floor() as usize)
            .clamp(1, HISTORY_MAX_ROWS)
            .min(self.history.len());
        let entries: Vec<String> = self.history.iter().take(rows).cloned().collect();
        let total = self.history.len();
        let selected = self.history_selected.min(rows.saturating_sub(1));
        let mut apply: Option<String> = None;
        let mut remove: Option<String> = None;
        let mut clear = false;

        egui::Area::new(egui::Id::unique("hero-history"))
            .order(egui::Order::Middle)
            .movable(false)
            .fade_in(false)
            .constrain(false)
            .fixed_pos(pos)
            .show(ctx, |ui| {
                ui.set_width(width);
                Frame::NONE
                    .fill(pal.card)
                    .stroke(Stroke::new(1.0, pal.border_soft))
                    .corner_radius(CornerRadius::same(10))
                    .inner_margin(Margin::same(6))
                    .show(ui, |ui| {
                        ui.set_width(width - 12.0);
                        ui.spacing_mut().item_spacing.y = 1.0;
                        for (index, entry) in entries.iter().enumerate() {
                            let is_selected = index == selected;
                            let (rect, response) = ui.allocate_exact_size(
                                Vec2::new(ui.available_width(), row_h),
                                egui::Sense::click(),
                            );
                            if is_selected {
                                ui.painter().rect_filled(rect, CornerRadius::same(6), pal.selection);
                            } else if response.hovered() {
                                ui.painter().rect_filled(rect, CornerRadius::same(6), pal.hover);
                            }
                            icons::paint_glyph(
                                ui.painter(),
                                egui::Rect::from_center_size(
                                    egui::pos2(rect.left() + 14.0, rect.center().y),
                                    Vec2::splat(13.0),
                                ),
                                icons::Glyph::Clock,
                                pal.text_faint,
                            );
                            // 查询文本：超长时按右侧留白裁剪，不换行、不撑破面板。
                            let text_rect = egui::Rect::from_min_max(
                                egui::pos2(rect.left() + 26.0, rect.top()),
                                egui::pos2(rect.right() - 32.0, rect.bottom()),
                            );
                            ui.painter().with_clip_rect(text_rect).text(
                                egui::pos2(text_rect.left(), rect.center().y),
                                egui::Align2::LEFT_CENTER,
                                entry,
                                egui::FontId::proportional(12.5),
                                if is_selected { pal.text } else { pal.text_dim },
                            );
                            let close_rect = egui::Rect::from_center_size(
                                egui::pos2(rect.right() - 14.0, rect.center().y),
                                Vec2::splat(18.0),
                            );
                            let close = ui.interact(
                                close_rect,
                                egui::Id::unique(("history-del", index)),
                                egui::Sense::click(),
                            );
                            if close.hovered() {
                                ui.painter().circle_filled(close_rect.center(), 8.0, pal.hover);
                            }
                            icons::paint_glyph(
                                ui.painter(),
                                close_rect.shrink(5.0),
                                icons::Glyph::Close,
                                if close.hovered() { pal.text } else { pal.text_faint },
                            );
                            if close.clicked() {
                                remove = Some(entry.clone());
                            } else if response.clicked() {
                                apply = Some(entry.clone());
                            }
                        }
                        ui.add_space(2.0);
                        ui.horizontal(|ui| {
                            ui.label(
                                RichText::new(format!(
                                    "最近 {} 条 · 共 {total} 条",
                                    entries.len()
                                ))
                                .size(10.5)
                                .color(pal.text_faint),
                            );
                            ui.with_layout(
                                egui::Layout::right_to_left(egui::Align::Center),
                                |ui| {
                                    if ui
                                        .add(
                                            egui::Button::new(
                                                RichText::new("清空历史").size(11.5),
                                            )
                                            .corner_radius(4),
                                        )
                                        .clicked()
                                    {
                                        clear = true;
                                    }
                                },
                            );
                        });
                    });
            });

        if let Some(entry) = apply {
            self.apply_history(&entry, ctx);
        } else if let Some(entry) = remove {
            self.history = history::remove(&entry);
            self.history_selected = self
                .history_selected
                .min(self.history.len().saturating_sub(1));
            if self.history.is_empty() {
                self.history_dismissed = true;
            }
        } else if clear {
            self.history = history::clear();
            self.history_dismissed = true;
        }
    }

    // ---------- 启动自动任务 / 检查更新 ----------

    /// 启动 3 秒后的自动任务：按策略决定是否自动建快照，并可选静默检查更新。
    fn handle_auto_tasks(&mut self, ctx: &egui::Context) {
        let Some(deadline) = self.auto_tasks_at else { return };
        let now = Instant::now();
        if now < deadline {
            ctx.request_repaint_after(deadline.saturating_duration_since(now));
            return;
        }
        self.auto_tasks_at = None;
        self.maybe_auto_snapshot(ctx);
        if self.settings.check_update_on_start {
            self.start_update_check(ctx, false);
        }
    }

    /// 自动快照调度：首次安装必建 → 每次启动建 → 每 N 天到期建，否则跳过并提示。
    ///
    /// 复用现有 start_rebuild；建完的清理 / 搬运 / v4 转换统一在 after_build 里收口。
    fn maybe_auto_snapshot(&mut self, ctx: &egui::Context) {
        if self.build_rx.is_some() {
            eprintln!("[auto] 已有建库任务在跑，跳过自动建快照");
            return;
        }
        if let Err(error) = std::fs::create_dir_all(&self.snapshots_dir) {
            eprintln!("[auto] 创建快照目录失败：{error}");
            return;
        }
        let files = db::list_snapshots(&self.snapshots_dir);
        let latest = newest_snapshot_time(&files);
        let age = now_unix().saturating_sub(latest.unwrap_or(0));
        let days = self.settings.auto_snapshot_every_days;
        let decision = auto_snapshot_decision(
            !files.is_empty(),
            self.settings.auto_snapshot_on_start,
            days,
            age,
        );
        match decision {
            AutoSnapshot::FirstInstall => {
                eprintln!("[auto] 首次安装（快照目录为空），立即建库");
                self.toast("首次启动：正在自动创建第一份快照…");
                self.build_auto = true;
                self.start_rebuild(ctx);
            }
            AutoSnapshot::OnStart => {
                eprintln!("[auto] 设置「每次启动都建」= true，启动即建库");
                self.toast("已开启「每次启动建快照」：正在后台建库…");
                self.build_auto = true;
                self.start_rebuild(ctx);
            }
            AutoSnapshot::Due => {
                let text = format!("开始自动建快照：距上次 {}（阈值 {days} 天）", human_age(age));
                eprintln!("[auto] {text}");
                self.toast(text);
                self.build_auto = true;
                self.start_rebuild(ctx);
            }
            AutoSnapshot::Skip => {
                let text = format!("跳过自动快照：距上次 {}（阈值 {days} 天）", human_age(age));
                eprintln!("[auto] {text}");
                self.toast(text);
            }
            AutoSnapshot::Disabled => {
                eprintln!("[auto] 周期自动快照已关闭（auto_snapshot_every_days=0）");
            }
        }
    }

    /// 检查更新：后台线程查询，popup 决定是否打开结果窗口（启动自动检查只看 toast）。
    fn start_update_check(&mut self, ctx: &egui::Context, popup: bool) {
        if self.update_rx.is_some() {
            if popup {
                self.show_update = true;
            }
            return;
        }
        if popup {
            self.show_update = true;
        }
        self.update_result = None;
        self.update_auto = !popup;
        let repaint = ctx.clone();
        self.update_rx = Some(update::spawn_check(move || repaint.request_repaint()));
        eprintln!(
            "[update] 开始检查更新（当前 {}）",
            update::CURRENT_VERSION
        );
    }

    /// 每帧轮询检查更新线程：不阻塞 UI，等待期间 120 ms 补一次重绘。
    fn poll_update(&mut self, ctx: &egui::Context) {
        let outcome = self.update_rx.as_ref().map(|rx| rx.try_recv());
        match outcome {
            None => {}
            Some(Ok(result)) => {
                self.update_rx = None;
                if self.update_auto {
                    match &result {
                        Ok(info) if update::is_newer(&info.tag_name, update::CURRENT_VERSION) => {
                            self.toast(format!("发现新版本 {}（帮助 → 检查更新…）", info.tag_name));
                        }
                        Ok(info) => eprintln!("[update] 已是最新：{}", info.tag_name),
                        Err(error) => eprintln!("[update] 启动检查更新失败：{error}"),
                    }
                }
                self.update_result = Some(result);
                ctx.request_repaint();
            }
            Some(Err(std::sync::mpsc::TryRecvError::Empty)) => {
                ctx.request_repaint_after(std::time::Duration::from_millis(120));
            }
            Some(Err(std::sync::mpsc::TryRecvError::Disconnected)) => {
                self.update_rx = None;
                self.update_result = Some(Err("检查更新线程意外退出".to_owned()));
            }
        }
    }

    /// 「检查更新」窗口：检查中转圈；成功显示「当前 / 最新 / 打开下载页」；
    /// 失败显示错误 + 「打开 Releases 页面」，不阻塞界面。
    fn update_window(&mut self, ctx: &egui::Context) {
        if !self.show_update {
            return;
        }
        let pal = self.pal;
        let checking = self.update_rx.is_some();
        let result = self.update_result.clone();
        let mut open = self.show_update;
        let mut action: Option<UpdateAction> = None;
        egui::Window::new("检查更新")
            .open(&mut open)
            .collapsible(false)
            .resizable(false)
            // Hero 的检索框 / 历史下拉画在 Middle 层；检查更新是用户主动打开的，
            // 必须压在最上面，否则会被 Hero 挡住（截图上尤其明显）。
            .order(egui::Order::Foreground)
            .anchor(egui::Align2::CENTER_CENTER, Vec2::ZERO)
            .show(ctx, |ui| {
                ui.set_min_width(430.0);
                ui.horizontal(|ui| {
                    ui.label(RichText::new("当前版本").size(12.0).color(pal.text_dim));
                    ui.label(
                        RichText::new(update::CURRENT_VERSION)
                            .size(13.5)
                            .strong()
                            .color(pal.text),
                    );
                });
                ui.add_space(6.0);
                if checking {
                    ui.horizontal(|ui| {
                        ui.spinner();
                        ui.label(
                            RichText::new("正在连接 GitHub 查询最新版本…")
                                .size(12.0)
                                .color(pal.text_dim),
                        );
                    });
                    ui.add_space(6.0);
                    ui.label(
                        RichText::new(update::LATEST_API)
                            .family(egui::FontFamily::Monospace)
                            .size(10.0)
                            .color(pal.text_faint),
                    );
                    return;
                }
                match result {
                    Some(Ok(info)) => {
                        let newer = update::is_newer(&info.tag_name, update::CURRENT_VERSION);
                        ui.horizontal(|ui| {
                            ui.label(RichText::new("最新版本").size(12.0).color(pal.text_dim));
                            ui.label(
                                RichText::new(&info.tag_name)
                                    .size(13.5)
                                    .strong()
                                    .color(if newer { pal.ok } else { pal.text }),
                            );
                            ui.label(
                                RichText::new(if newer { "有新版本可用" } else { "已是最新" })
                                    .size(11.5)
                                    .color(if newer { pal.ok } else { pal.text_faint }),
                            );
                        });
                        ui.add_space(4.0);
                        ui.label(
                            RichText::new(&info.html_url)
                                .family(egui::FontFamily::Monospace)
                                .size(10.0)
                                .color(pal.text_faint),
                        );
                        if !info.body.is_empty() {
                            ui.add_space(8.0);
                            Frame::NONE
                                .fill(pal.panel_alt)
                                .corner_radius(CornerRadius::same(6))
                                .inner_margin(Margin::same(8))
                                .show(ui, |ui| {
                                    ui.set_width(404.0);
                                    ui.label(
                                        RichText::new(&info.body)
                                            .size(11.5)
                                            .color(pal.text_dim),
                                    );
                                });
                        }
                        ui.add_space(10.0);
                        ui.horizontal(|ui| {
                            if crate::widgets::primary_button(ui, &pal, "打开下载页", None).clicked()
                            {
                                action = Some(UpdateAction::Open(info.html_url.clone()));
                            }
                            if ui.button("更新说明（手动热更新）").clicked() {
                                action =
                                    Some(UpdateAction::Open(update::UPDATE_DOC_URL.to_owned()));
                            }
                            if ui.button("重新检查").clicked() {
                                action = Some(UpdateAction::Recheck);
                            }
                        });
                    }
                    Some(Err(error)) => {
                        ui.label(
                            RichText::new("检查更新失败")
                                .size(12.5)
                                .strong()
                                .color(pal.danger),
                        );
                        ui.add_space(4.0);
                        ui.label(RichText::new(error).size(11.5).color(pal.text_dim));
                        ui.add_space(10.0);
                        ui.horizontal(|ui| {
                            if crate::widgets::primary_button(
                                ui,
                                &pal,
                                "打开 Releases 页面",
                                None,
                            )
                            .clicked()
                            {
                                action = Some(UpdateAction::Open(update::RELEASES_URL.to_owned()));
                            }
                            if ui.button("重新检查").clicked() {
                                action = Some(UpdateAction::Recheck);
                            }
                        });
                    }
                    None => {
                        ui.label(RichText::new("尚未检查。").size(12.0).color(pal.text_dim));
                        ui.add_space(8.0);
                        if ui.button("开始检查").clicked() {
                            action = Some(UpdateAction::Recheck);
                        }
                    }
                }
                ui.add_space(6.0);
                ui.label(
                    RichText::new(format!(
                        "只读 GitHub Releases，不自动下载、不自动替换文件；仓库 {}",
                        update::REPO_SLUG
                    ))
                    .size(10.5)
                    .color(pal.text_faint),
                );
            });
        self.show_update = open;
        if let Some(action) = action {
            match action {
                UpdateAction::Open(url) => {
                    if let Err(error) = update::open_url(&url) {
                        self.toast(error);
                    }
                }
                UpdateAction::Recheck => self.start_update_check(ctx, true),
            }
        }
    }

    // ---------- 设置窗口 / 托盘命令 ----------

    /// 工具 → 设置…：快照 / 折叠 / 托盘三组开关，改动即时保存。
    fn settings_window(&mut self, ctx: &egui::Context) {
        if !self.show_settings {
            return;
        }
        let pal = self.pal;
        let dir_before = self.snapshots_dir.clone();
        let mut open = self.show_settings;
        let mut changed = false;
        let mut quiet = false;
        let mut pick_dir = false;
        egui::Window::new("设置")
            .open(&mut open)
            .collapsible(false)
            .resizable(false)
            .order(egui::Order::Foreground)
            .anchor(egui::Align2::CENTER_CENTER, Vec2::ZERO)
            .show(ctx, |ui| {
                ui.set_min_width(640.0);
                ui.label(RichText::new("快照").size(13.0).strong().color(pal.text));
                ui.add_space(4.0);
                ui.horizontal(|ui| {
                    ui.label(RichText::new("快照目录").size(12.0).color(pal.text_dim));
                    let hint = format!("留空 = {}", self.data_root.join("snapshots").display());
                    let response = ui.add(
                        egui::TextEdit::singleline(&mut self.settings.snapshot_dir)
                            .desired_width(380.0)
                            .hint_text(hint),
                    );
                    if response.changed() {
                        changed = true;
                        quiet = true;
                    }
                    if ui.button("选择…").clicked() {
                        pick_dir = true;
                    }
                    if ui.button("恢复默认").clicked() {
                        self.settings.snapshot_dir.clear();
                        changed = true;
                    }
                });
                ui.horizontal(|ui| {
                    ui.label(RichText::new("周期自动快照").size(12.0).color(pal.text_dim));
                    let mut days = self.settings.auto_snapshot_every_days;
                    if ui
                        .add(egui::DragValue::new(&mut days).range(0..=365).suffix(" 天"))
                        .changed()
                    {
                        self.settings.auto_snapshot_every_days = days;
                        changed = true;
                    }
                    ui.label(
                        RichText::new("（0 = 关闭周期自动快照；首次安装总会建第一份）")
                            .size(10.5)
                            .color(pal.text_faint),
                    );
                });
                ui.horizontal(|ui| {
                    ui.label(RichText::new("保留份数").size(12.0).color(pal.text_dim));
                    let mut keep = self.settings.keep_snapshot_count;
                    if ui
                        .add(egui::DragValue::new(&mut keep).range(0..=500).suffix(" 份"))
                        .changed()
                    {
                        self.settings.keep_snapshot_count = keep;
                        changed = true;
                    }
                    ui.label(
                        RichText::new("（0 = 不自动清理；超出部分移入「待删除」，不永久删除）")
                            .size(10.5)
                            .color(pal.text_faint),
                    );
                });
                if ui
                    .checkbox(
                        &mut self.settings.convert_snapshot_v4,
                        "建完后台转 v4（下次启动自动落位，体积更小）",
                    )
                    .changed()
                {
                    changed = true;
                }
                if ui
                    .checkbox(
                        &mut self.settings.auto_snapshot_on_start,
                        "每次启动都建快照（不看周期天数）",
                    )
                    .changed()
                {
                    changed = true;
                }
                ui.add_space(8.0);
                ui.separator();
                ui.label(RichText::new("折叠").size(13.0).strong().color(pal.text));
                ui.add_space(4.0);
                if ui
                    .checkbox(&mut self.settings.fold_enabled, "检索结果折叠低价值行")
                    .changed()
                {
                    changed = true;
                }
                if ui
                    .checkbox(&mut self.settings.fold_rare, "冷门扩展名也折叠（Rare 桶）")
                    .changed()
                {
                    changed = true;
                }
                ui.horizontal(|ui| {
                    ui.label(RichText::new("豁免扩展名").size(12.0).color(pal.text_dim));
                    if ui
                        .add(
                            egui::TextEdit::singleline(&mut self.settings.fold_keep_exts)
                                .desired_width(240.0)
                                .hint_text("逗号分隔；留空 = 用内置规则"),
                        )
                        .changed()
                    {
                        changed = true;
                        quiet = true;
                    }
                });
                ui.horizontal(|ui| {
                    ui.label(RichText::new("豁免前缀").size(12.0).color(pal.text_dim));
                    if ui
                        .add(
                            egui::TextEdit::singleline(&mut self.settings.fold_keep_prefixes)
                                .desired_width(240.0)
                                .hint_text("逗号分隔的目录前缀，如 D:\\资料"),
                        )
                        .changed()
                    {
                        changed = true;
                        quiet = true;
                    }
                });
                ui.label(
                    RichText::new(
                        "保留此目录：把目录填进「豁免前缀」，该目录整棵不折叠（例如 F:\\系统重装备份）。",
                    )
                    .size(10.5)
                    .color(pal.accent),
                );
                ui.label(
                    RichText::new("折叠开关即时生效；扩展名 / 前缀白名单由折叠逻辑（lowvalue）读取。")
                        .size(10.5)
                        .color(pal.text_faint),
                );
                ui.add_space(8.0);
                ui.separator();
                ui.label(RichText::new("托盘 / 启动").size(13.0).strong().color(pal.text));
                ui.add_space(4.0);
                if ui
                    .checkbox(
                        &mut self.settings.close_to_tray,
                        "关闭窗口时隐藏到托盘（默认开启，托盘「退出」才真退出）",
                    )
                    .changed()
                {
                    changed = true;
                }
                if ui
                    .checkbox(&mut self.settings.start_minimized, "启动时最小化")
                    .changed()
                {
                    changed = true;
                }
                ui.label(
                    RichText::new(if tray::is_ready() {
                        "托盘已就绪：双击托盘图标 = 显示主窗口。"
                    } else {
                        "托盘未启用（--shot 截图或初始化失败）：关闭窗口会直接退出。"
                    })
                    .size(10.5)
                    .color(pal.text_faint),
                );
                ui.add_space(6.0);
                ui.label(
                    RichText::new(format!(
                        "设置文件：{}（改动即时保存）",
                        settings::path().display()
                    ))
                    .size(10.0)
                    .color(pal.text_faint),
                );
            });
        self.show_settings = open;
        if pick_dir && let Some(dir) = tray::pick_folder("选择快照目录") {
            self.settings.snapshot_dir = dir;
            changed = true;
        }
        if changed {
            self.apply_settings(quiet);
            if self.snapshots_dir != dir_before {
                self.reload_snapshots(ctx);
                self.toast(format!("快照目录已切换：{}", self.snapshots_dir.display()));
            }
        }
    }

    /// 保存设置并让改动立即生效（目录 / 折叠默认态 / 注册表 SnapshotDir）。
    fn apply_settings(&mut self, quiet: bool) {
        self.settings = self.settings.clone().normalized();
        self.snapshots_dir = self.settings.effective_snapshots_dir(&self.data_root);
        settings::save(&self.settings);
        if let Err(error) =
            tray::write_snapshot_dir_registry(&self.snapshots_dir.display().to_string())
        {
            eprintln!("[settings] 写注册表 SnapshotDir 失败：{error}");
        }
        if std::env::var_os("DISKINTEL_UI_SHOW_LOW").is_none() {
            self.search.show_low_value = !self.settings.fold_enabled;
            self.search.dirty = true;
        }
        // 折叠配置改动即时生效（lowvalue::install_config 由 lead 合入后接上）。
        apply_fold_config(&self.settings);
        eprintln!(
            "[settings] 已保存：目录={} 周期={} 天 保留={} 份 托盘={} 每次启动建={} v4={}",
            self.snapshots_dir.display(),
            self.settings.auto_snapshot_every_days,
            self.settings.keep_snapshot_count,
            self.settings.close_to_tray,
            self.settings.auto_snapshot_on_start,
            self.settings.convert_snapshot_v4,
        );
        if !quiet {
            self.toast("设置已保存");
        }
    }

    /// 托盘命令：真正执行业务（显示窗口 / 建快照 / 检查更新 / 真退出）。
    fn handle_tray_commands(&mut self, ctx: &egui::Context) {
        for command in tray::poll() {
            match command {
                TrayCommand::Show => {
                    eprintln!("[tray] 显示主窗口（命令）");
                    ctx.send_viewport_cmd(egui::ViewportCommand::Visible(true));
                    ctx.send_viewport_cmd(egui::ViewportCommand::Minimized(false));
                    ctx.send_viewport_cmd(egui::ViewportCommand::Focus);
                    ctx.request_repaint();
                }
                TrayCommand::SnapshotNow => {
                    if self.build_rx.is_some() {
                        self.toast("已有建库任务在跑，稍候");
                    } else {
                        eprintln!("[tray] 立即建快照（命令）");
                        self.build_auto = false;
                        self.toast("托盘：立即建快照…");
                        self.start_rebuild(ctx);
                    }
                }
                TrayCommand::CheckUpdate => {
                    eprintln!("[tray] 检查更新（命令）");
                    self.start_update_check(ctx, true);
                }
                TrayCommand::Quit => {
                    eprintln!("[tray] 托盘「退出」：真退出");
                    self.allow_close = true;
                    tray::shutdown();
                    ctx.send_viewport_cmd(egui::ViewportCommand::Close);
                }
            }
        }
    }

    /// 点窗口关闭按钮：close_to_tray 时取消关闭并隐藏窗口，进程继续驻留托盘。
    fn handle_close_request(&mut self, ctx: &egui::Context) {
        if !ctx.input(|input| input.viewport().close_requested()) {
            return;
        }
        if self.allow_close {
            return; // 真退出：让 eframe 正常关闭，on_exit 里移除托盘图标
        }
        if !self.settings.close_to_tray {
            eprintln!("[tray] close_to_tray=false，正常退出窗口");
            return;
        }
        ctx.send_viewport_cmd(egui::ViewportCommand::CancelClose);
        ctx.send_viewport_cmd(egui::ViewportCommand::Visible(false));
        eprintln!("[tray] 拦截关闭：隐藏到托盘（进程继续，托盘图标常驻）");
        self.toast("已隐藏到托盘：双击托盘图标或右键菜单「显示主窗口」恢复；托盘「退出」才真退出");
    }
}

/// 「检查更新」窗口上的一次点击（窗口闭包只写这个枚举，动作在窗口外执行）。
enum UpdateAction {
    Open(String),
    Recheck,
}

/// 折叠配置接线点：把 settings 里的四个折叠字段交给 lowvalue 的全局配置。
///
/// w3 的 lowvalue::FoldConfig / install_config 还没合入本克隆，这里先留空；
/// lead 合并时把函数体换成：
///
/// lowvalue::install_config(Some(lowvalue::FoldConfig {
///     enabled: settings.fold_enabled,
///     rare: settings.fold_rare,
///     keep_exts: split_list(&settings.fold_keep_exts),
///     keep_prefixes: split_list(&settings.fold_keep_prefixes),
/// }));
fn apply_fold_config(settings: &settings::Settings) {
    // 检索页每行判定走 lowvalue::classify，读取的是这里安装的全局配置。
    crate::lowvalue::install_config(Some(settings.fold_config()));
}

/// 最新快照的时间（Unix 秒）：meta.created_at 与文件 mtime 取较大值（哪个新算哪个）。
fn newest_snapshot_time(snapshots: &[db::SnapshotFile]) -> Option<i64> {
    let newest = snapshots.first()?;
    let mtime = std::fs::metadata(&newest.path)
        .and_then(|meta| meta.modified())
        .ok()
        .and_then(|time| time.duration_since(std::time::UNIX_EPOCH).ok())
        .map(|elapsed| elapsed.as_secs() as i64)
        .unwrap_or(0);
    Some(newest.created_at.max(mtime))
}

fn now_unix() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|elapsed| elapsed.as_secs() as i64)
        .unwrap_or(0)
}

/// 自动快照的一次决策（纯函数，便于单测覆盖全部 5 种分支）。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum AutoSnapshot {
    /// 首次安装：没有任何快照，必建。
    FirstInstall,
    /// 设置「每次启动都建」。
    OnStart,
    /// 周期到期。
    Due,
    /// 周期未到。
    Skip,
    /// 周期自动快照关闭。
    Disabled,
}

fn auto_snapshot_decision(
    has_snapshot: bool,
    on_start: bool,
    every_days: u64,
    age_seconds: i64,
) -> AutoSnapshot {
    if !has_snapshot {
        return AutoSnapshot::FirstInstall;
    }
    if on_start {
        return AutoSnapshot::OnStart;
    }
    if every_days == 0 {
        return AutoSnapshot::Disabled;
    }
    if snapshot_due(age_seconds, every_days) {
        AutoSnapshot::Due
    } else {
        AutoSnapshot::Skip
    }
}

/// 周期自动快照是否到期：距上次至少 every_days 天。
///
/// 这是 w4 的 snapshot_policy::due 的本地替身（该模块还没合入主仓库）；
/// 合入后把这里换成 snapshot_policy::due 即可，调用点签名保持不变。
fn snapshot_due(age_seconds: i64, every_days: u64) -> bool {
    if every_days == 0 {
        return false;
    }
    age_seconds >= (every_days as i64).saturating_mul(SECONDS_PER_DAY)
}

/// 人类可读的「距上次」：不足 1 小时按分钟、不足 1 天按小时、否则按天。
fn human_age(seconds: i64) -> String {
    if seconds >= SECONDS_PER_DAY {
        format!("{} 天", seconds / SECONDS_PER_DAY)
    } else if seconds >= 3600 {
        format!("{} 小时", seconds / 3600)
    } else {
        format!("{} 分钟", (seconds / 60).max(0))
    }
}

/// 只读判断快照是不是 v4（meta.format_version = 4）；打不开 / 读不到按 v3 处理。
fn snapshot_is_v4(path: &std::path::Path) -> bool {
    let Ok(conn) = rusqlite::Connection::open_with_flags(
        path,
        rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY,
    ) else {
        return false;
    };
    conn.query_row(
        "SELECT CAST(value AS INTEGER) FROM meta WHERE key = 'format_version'",
        [],
        |row| row.get::<_, i64>(0),
    )
    .map(|version| version == 4)
    .unwrap_or(false)
}

/// 启动时先把上次留下的 <快照名>.v4tmp 原子落位。
///
/// 必须在打开快照之前调用（main.rs）：那时旧 <快照名>.sqlite 还没被任何连接打开，
/// Windows 才允许 rename 覆盖。旧文件按公约移入 <待删除> 并登记 MANIFEST。
pub fn finalize_pending_v4(snapshots_dir: &std::path::Path) -> usize {
    let Ok(entries) = std::fs::read_dir(snapshots_dir) else {
        return 0;
    };
    let mut finalized = 0;
    for entry in entries.flatten() {
        let temp = entry.path();
        if temp.extension().and_then(|ext| ext.to_str()) != Some(V4_TEMP_EXT) {
            continue;
        }
        let Some(stem) = temp
            .file_stem()
            .and_then(|stem| stem.to_str())
            .map(str::to_owned)
        else {
            continue;
        };
        let target = snapshots_dir.join(format!("{stem}.sqlite"));
        let trash_root = snapshots_dir.parent().unwrap_or(snapshots_dir);
        if target.exists() {
            match trash::move_to_trash(
                &target,
                trash_root,
                "v4 转换后替换旧 v3 快照",
                "DiskIntel 自动快照",
            ) {
                Ok(moved) => eprintln!("[v4] 旧 v3 快照移入待删除：{}", moved.display()),
                Err(error) => {
                    eprintln!("[v4] 旧 v3 快照入待删除失败，跳过本次落位：{error}");
                    continue;
                }
            }
        }
        match std::fs::rename(&temp, &target) {
            Ok(()) => {
                finalized += 1;
                eprintln!("[v4] 已落位 v4 快照：{}", target.display());
            }
            Err(error) => eprintln!("[v4] v4 落位失败：{error}"),
        }
    }
    finalized
}

impl eframe::App for App {
    /// 无 UI 的每帧逻辑：窗口隐藏到托盘时 eframe 只跑这一层（不跑 egui 绘制），
    /// 所以托盘命令、建库轮询、自动快照调度、检查更新全部放这里。
    fn logic(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        self.handle_tray_commands(ctx);
        let engine_events = self.engine.poll();
        for event in engine_events {
            if matches!(event, engine::EngineEvent::State(_)) {
                self.search.dirty = true;
            }
            self.search.on_engine_event(event);
        }
        self.poll_build(ctx);
        self.poll_convert(ctx);
        self.handle_auto_tasks(ctx);
        self.poll_update(ctx);
    }

    fn ui(&mut self, ui: &mut egui::Ui, _frame: &mut eframe::Frame) {
        self.frame_no += 1;
        if self.frame_no == 1 {
            eprintln!("[t] 第一帧开始 +{:.0}ms", self.startup_ms);
        }
        let ctx = ui.ctx().clone();
        // 关闭按钮：close_to_tray 时取消关闭 + 隐藏（进程继续驻留托盘）。
        self.handle_close_request(&ctx);
        // 启动最小化：首帧发一次命令。
        if self.frame_no == 1 && self.start_minimized_pending {
            self.start_minimized_pending = false;
            eprintln!("[tray] 启动最小化（设置 start_minimized=true）");
            ctx.send_viewport_cmd(egui::ViewportCommand::Minimized(true));
        }
        // Hero 缓动：0 = 中央大框，1 = 收回顶部工具栏；egui 负责逐帧推进，绘制侧只读进度。
        self.hero_t = ctx.animate_value_with_time(
            egui::Id::unique(HERO_ANIM_ID),
            self.hero_target(),
            crate::widgets::HERO_ANIM_SECS,
        );
        self.handle_auto_focus(&ctx);
        self.handle_shortcuts(&ctx);
        self.menubar(ui);
        self.toolbar(ui);
        self.statusbar(ui);

        let pal = self.pal;
        // 中央区矩形必须在 CentralPanel 之前取：Hero 用它算「垂直 38% 处」。
        let central = ui.available_rect_before_wrap();
        egui::CentralPanel::default()
            .frame(Frame::NONE.fill(pal.bg))
            .show(ui, |ui| {
                match self.view {
                    View::Search => {
                        // Hero 态（空查询）列表让位给中央检索框；hero_overlay 负责画检索框本身。
                        // 过渡的 220 ms 里也不画列表，避免「列表先闪一下再被遮挡」。
                        if self.hero_t >= HERO_DONE {
                            let action = self.search.show(ui, &pal, &self.snap);
                            if let Some(spec) = self.search.take_live_request() {
                                let id = self.engine.query(spec);
                                self.search.set_live_seq(id);
                            }
                            if let Some(action) = action {
                                self.handle_action(action);
                            }
                            if self.search.dirty {
                                self.search.dirty = false;
                                self.last_refresh = Instant::now();
                            }
                            // 只在真正提交生效（回车 / 防抖落地）时记历史。
                            self.record_history();
                        }
                    }
                    View::Space => {
                        self.space.show(ui, &pal, &self.snap);
                    }
                    View::Diff => {
                        self.diff.ensure_started(&self.snapshots);
                        self.diff.show(ui, &pal, &self.snapshots);
                        if self.diff.busy() {
                            ui.ctx().request_repaint_after(std::time::Duration::from_millis(120));
                        }
                    }
                    View::Snapshots => {
                        if let Some(action) = self.snapshots_view.show(ui, &pal, &self.snap) {
                            match action {
                                views::snapshots::SnapshotsAction::Switch(name) => {
                                    self.switch_snapshot(&name, &ctx);
                                }
                                views::snapshots::SnapshotsAction::Deleted(name) => {
                                    self.snapshots.retain(|file| file.name != name);
                                    self.toast(format!("已移入待删除：{name}"));
                                }
                                views::snapshots::SnapshotsAction::Toast(text) => self.toast(text),
                            }
                        }
                    }
                }
            });

        // Hero 层画在所有面板之上：检索框可以从中央一路滑进顶部工具栏，不被中央面板裁剪。
        self.hero_overlay(&ctx, central);

        self.help_window(&ctx);
        self.shortcuts_window(&ctx);
        self.update_window(&ctx);
        self.settings_window(&ctx);
        if let Some((text, at)) = &self.toast {
            if at.elapsed().as_secs_f32() < 2.6 {
                let pal = self.pal;
                egui::Area::new(egui::Id::unique("toast"))
                    .anchor(egui::Align2::RIGHT_BOTTOM, Vec2::new(-16.0, -34.0))
                    .show(&ctx, |ui| {
                        Frame::NONE
                            .fill(if pal.dark {
                                Color32::from_rgb(0x25, 0x32, 0x4a)
                            } else {
                                Color32::from_rgb(0x25, 0x32, 0x4a)
                            })
                            .corner_radius(CornerRadius::same(6))
                            .inner_margin(Margin::symmetric(12, 7))
                            .show(ui, |ui| {
                                ui.label(RichText::new(text.clone()).size(12.0).color(Color32::WHITE));
                            });
                    });
                ctx.request_repaint_after(std::time::Duration::from_millis(200));
            }
        }
        self.handle_shot(&ctx);
    }

    /// 退出：移除托盘图标（真退出 / close_to_tray=false 的正常关闭都会走到）。
    fn on_exit(&mut self, _gl: Option<&eframe::glow::Context>) {
        tray::shutdown();
    }
}

impl App {
    fn handle_action(&mut self, action: views::search::SearchAction) {
        match action {
            views::search::SearchAction::Open(path) => {
                if let Err(error) = open_path(&path) {
                    self.toast(error);
                }
            }
            views::search::SearchAction::CopyPath(path) => {
                self.toast(format!("已复制：{path}"));
            }
            views::search::SearchAction::Toast(text) => self.toast(text),
            views::search::SearchAction::Focus(path) => {
                self.view = View::Space;
                self.space.focus_on(&path, &self.snap);
            }
        }
    }
}

/// 缓动曲线：smoothstep，两端导数为 0，起落都不生硬。
fn smoothstep(t: f32) -> f32 {
    let t = t.clamp(0.0, 1.0);
    t * t * (3.0 - 2.0 * t)
}

/// Hero 检索框的宽度：窗口越宽越宽，但上限 680，窄窗口保底 360。
fn hero_width(central: egui::Rect) -> f32 {
    (central.width() - 140.0).clamp(360.0, 680.0)
}

/// Hero 检索框在过渡进度 t 处的落点与宽度。
///
/// 关键契约（--shot 稳定性的前提）：
/// 关键契约（--shot 稳定性的前提）：
///
/// - t = 0 → 中央偏上（垂直 38% 处）的 Hero 大框；
/// - t = 1 → 与顶部工具栏预留的槽位严格重合（位置、宽度都相同）。
///
/// 所以「Area 画的过渡框」与「工具栏原位画的框」在切换的那一帧完全对齐，不会跳。
fn hero_field_geometry(central: egui::Rect, slot: egui::Rect, t: f32) -> (egui::Pos2, f32) {
    let eased = smoothstep(t);
    let hero_w = hero_width(central);
    let top = egui::pos2(
        central.center().x - hero_w * 0.5,
        central.top() + central.height() * HERO_Y_RATIO - crate::widgets::HERO_FIELD_H * 0.5,
    );
    let pos = egui::pos2(
        top.x + (slot.left() - top.x) * eased,
        top.y + (slot.top() - top.y) * eased,
    );
    (pos, hero_w + (slot.width() - hero_w) * eased)
}


fn view_hint(view: View) -> String {
    match view {
        View::Search => "  Ctrl+1".to_owned(),
        View::Space => "  Ctrl+2".to_owned(),
        View::Diff => "  Ctrl+3".to_owned(),
        View::Snapshots => "  Ctrl+4".to_owned(),
    }
}

fn view_verb(view: View) -> &'static str {
    match view {
        View::Search => "search",
        View::Space => "du",
        View::Diff => "diff",
        View::Snapshots => "snapshots",
    }
}

fn snapshot_label(name: &str) -> String {
    if name == "-" {
        "无快照".to_owned()
    } else {
        name.to_owned()
    }
}

fn apply_icons(ctx: &egui::Context) {
    // 统一悬停/点击视觉反馈：不放大控件，避免行高抖动。
    ctx.all_styles_mut(|style| {
        style.visuals.widgets.hovered.expansion = 0.0;
        style.visuals.widgets.active.expansion = 0.0;
    });
}

fn open_in_explorer(path: &std::path::Path) -> Result<(), String> {
    std::process::Command::new("explorer")
        .arg(path)
        .spawn()
        .map(|_| ())
        .map_err(|e| format!("打开目录失败：{e}"))
}

fn open_path(path: &str) -> Result<(), String> {
    // 目录用 explorer 直接打开；文件用系统默认程序。
    std::process::Command::new("explorer")
        .arg(path)
        .spawn()
        .map(|_| ())
        .map_err(|e| format!("打开失败：{e}"))
}

fn rss_mb() -> f64 {
    // 进程 RSS：Windows 下读 GetProcessMemoryInfo 成本略高，用简单采样即可。
    #[allow(unused_imports)]
    use std::sync::OnceLock;
    static CACHE: OnceLock<()> = OnceLock::new();
    let _ = &CACHE;
    current_rss_mb().unwrap_or(0.0)
}

fn current_rss_mb() -> Option<f64> {
    #[cfg(windows)]
    {
        use std::ffi::c_void;
        #[repr(C)]
        #[derive(Default)]
        struct ProcessMemoryCounters {
            cb: u32,
            page_fault_count: u32,
            peak_working_set_size: usize,
            working_set_size: usize,
            quota_peak_paged_pool_usage: usize,
            quota_paged_pool_usage: usize,
            quota_peak_non_paged_pool_usage: usize,
            quota_non_paged_pool_usage: usize,
            pagefile_usage: usize,
            peak_pagefile_usage: usize,
        }
        #[link(name = "kernel32")]
        unsafe extern "system" {
            fn GetCurrentProcess() -> *mut c_void;
        }
        #[link(name = "psapi")]
        unsafe extern "system" {
            fn GetProcessMemoryInfo(
                process: *mut c_void,
                counters: *mut ProcessMemoryCounters,
                size: u32,
            ) -> i32;
        }
        unsafe {
            let mut counters = ProcessMemoryCounters {
                cb: std::mem::size_of::<ProcessMemoryCounters>() as u32,
                ..Default::default()
            };
            let ok = GetProcessMemoryInfo(
                GetCurrentProcess(),
                &mut counters,
                std::mem::size_of::<ProcessMemoryCounters>() as u32,
            );
            if ok != 0 {
                return Some(counters.working_set_size as f64 / 1_048_576.0);
            }
        }
    }
    None
}

#[cfg(test)]
mod shell_tests {
    use super::{hero_field_geometry, hero_width};

    fn rect(x: f32, y: f32, w: f32, h: f32) -> eframe::egui::Rect {
        eframe::egui::Rect::from_min_size(eframe::egui::pos2(x, y), eframe::egui::vec2(w, h))
    }

    /// t = 1 必须与工具栏槽位严格重合：这是 Hero 与列表之间「不跳、不闪」的数学前提。
    #[test]
    fn hero_end_lands_exactly_on_toolbar_slot() {
        let central = rect(0.0, 64.0, 1200.0, 672.0);
        let slot = rect(340.0, 70.0, 560.0, 28.0);
        let (pos, width) = hero_field_geometry(central, slot, 1.0);
        assert!((pos.x - slot.left()).abs() < 0.01);
        assert!((pos.y - slot.top()).abs() < 0.01);
        assert!((width - slot.width()).abs() < 0.01);
    }

    /// t = 0 必须落在「中央偏上、水平居中」的 Hero 位置。
    #[test]
    fn hero_start_is_centered_at_38_percent() {
        let central = rect(0.0, 64.0, 1200.0, 672.0);
        let slot = rect(340.0, 70.0, 560.0, 28.0);
        let (pos, width) = hero_field_geometry(central, slot, 0.0);
        let expected_w = hero_width(central);
        assert!((width - expected_w).abs() < 0.01);
        assert!((pos.x - (600.0 - expected_w * 0.5)).abs() < 0.01);
        // 垂直中心 = 中央区 38% 处
        let center_y = pos.y + crate::widgets::HERO_FIELD_H * 0.5;
        assert!((center_y - (64.0 + 672.0 * 0.38)).abs() < 0.01);
    }

    /// 过渡过程必须单调上移，且永远夹在 Hero 与工具栏之间（不会越界再弹回）。
    #[test]
    fn hero_transition_moves_monotonically_up() {
        let central = rect(0.0, 64.0, 1200.0, 672.0);
        let slot = rect(340.0, 70.0, 560.0, 28.0);
        let (start, start_w) = hero_field_geometry(central, slot, 0.0);
        let mut last_y = start.y;
        let mut last_w = start_w;
        for step in 1..=20 {
            let t = step as f32 / 20.0;
            let (pos, width) = hero_field_geometry(central, slot, t);
            assert!(pos.y <= last_y + 0.01, "y 必须单调上移：t={t}");
            assert!(width <= last_w + 0.01, "宽度必须单调收窄：t={t}");
            assert!(pos.y >= slot.top() - 0.01);
            last_y = pos.y;
            last_w = width;
        }
        assert!(last_y < start.y - 100.0, "整个过渡应完成大幅上移");
    }

    use super::{
        AutoSnapshot, SECONDS_PER_DAY, auto_snapshot_decision, finalize_pending_v4, human_age,
        snapshot_due,
    };

    /// 自动快照决策树：首次安装 → 每次启动 → 到期 / 跳过 / 关闭，五种分支都要覆盖。
    #[test]
    fn auto_snapshot_decision_covers_all_branches() {
        let day = SECONDS_PER_DAY;
        assert_eq!(
            auto_snapshot_decision(false, false, 7, 0),
            AutoSnapshot::FirstInstall,
            "没有快照时无论设置如何都要建第一份"
        );
        assert_eq!(
            auto_snapshot_decision(false, true, 7, 10 * day),
            AutoSnapshot::FirstInstall
        );
        assert_eq!(
            auto_snapshot_decision(true, true, 7, 0),
            AutoSnapshot::OnStart,
            "每次启动都建优先于周期判断"
        );
        assert_eq!(
            auto_snapshot_decision(true, false, 7, 7 * day),
            AutoSnapshot::Due
        );
        assert_eq!(
            auto_snapshot_decision(true, false, 7, 6 * day),
            AutoSnapshot::Skip
        );
        assert_eq!(
            auto_snapshot_decision(true, false, 0, 999 * day),
            AutoSnapshot::Disabled
        );
    }

    /// 周期自动快照到期判定：不足 N 天不建、达到/超过才建，0 表示关闭。
    #[test]
    fn snapshot_policy_due_thresholds() {
        assert!(!snapshot_due(0, 7));
        assert!(!snapshot_due(6 * SECONDS_PER_DAY, 7));
        assert!(snapshot_due(7 * SECONDS_PER_DAY, 7));
        assert!(snapshot_due(30 * SECONDS_PER_DAY, 7));
        // 0 天 = 关闭周期自动快照：永远不到期。
        assert!(!snapshot_due(i64::MAX, 0));
    }

    /// 「距上次」文案：分钟 / 小时 / 天三档，给 toast 用。
    #[test]
    fn human_age_formats_three_ranges() {
        assert_eq!(human_age(59), "0 分钟");
        assert_eq!(human_age(90), "1 分钟");
        assert_eq!(human_age(3 * 3600), "3 小时");
        assert_eq!(human_age(SECONDS_PER_DAY), "1 天");
        assert_eq!(human_age(3 * SECONDS_PER_DAY + 86_399), "3 天");
    }

    /// v4 落位：旧 v3 文件移入待删除 + 登记 MANIFEST，v4tmp 改名成正式快照。
    #[test]
    fn finalize_v4_replaces_v3_and_keeps_trash() {
        let root = std::env::temp_dir().join(format!(
            "diskintel-v4-finalize-{}",
            std::process::id()
        ));
        let snapshots = root.join("snapshots");
        std::fs::create_dir_all(&snapshots).unwrap();
        let name = "20260101-000000";
        std::fs::write(snapshots.join(format!("{name}.sqlite")), b"v3").unwrap();
        std::fs::write(snapshots.join(format!("{name}.v4tmp")), b"v4").unwrap();
        assert_eq!(finalize_pending_v4(&snapshots), 1);
        assert_eq!(
            std::fs::read(snapshots.join(format!("{name}.sqlite"))).unwrap(),
            b"v4"
        );
        assert!(!snapshots.join(format!("{name}.v4tmp")).exists());
        // 旧 v3 文件按公约进了 <root>/待删除 并登记 MANIFEST，不是永久删除。
        let trash = root.join("待删除");
        assert!(trash.exists(), "待删除目录应存在");
        let names: Vec<String> = std::fs::read_dir(&trash)
            .unwrap()
            .flatten()
            .map(|entry| entry.file_name().to_string_lossy().into_owned())
            .collect();
        assert!(
            names.iter().any(|item| item.contains(name)),
            "待删除里应有旧快照：{names:?}"
        );
        assert!(trash.join("MANIFEST.md").exists());
    }
}

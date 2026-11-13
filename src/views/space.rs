//! 空间视图：物理磁盘分组 → 卷卡片（实际口径环形图）→ 目录矩形树图 + 层级列表。
//!
//! 规格来源：磁盘情报系统/docs/方案-二期体验改进-20261113.md 第 1 节。
//!
//! 两条口径（严禁混用）：
//! 1. 实际口径：实际已用 = 容量 − 实时空闲（Win32 GetDiskFreeSpaceExW），
//!    环形图与所有百分比只用它；已用恒在 [0, 容量] 内，不会出现「100% / 超出容量」。
//! 2. 快照口径：快照逻辑 = 快照里该卷所有文件 size 的逻辑和，单独一行展示。
//!    它包含硬链接重复计数（WinSxS / System32）、NTFS 元数据文件（$MFT 等）、
//!    压缩/稀疏文件的逻辑大小，可能大于卷容量；差额只用脚注与 tooltip 解释，不标红。
//!
//! 分组口径：按物理磁盘分行（diskmap 模块用 Win32 FFI 取拓扑），
//! 无盘符的系统 / 恢复分区进「系统保留」折叠区（默认收起）。
//!
//! 树图口径（未变）：总量一律来自 folder_sizes；快照内严格成立
//! 「目录 size = Σ 直接子目录 size + Σ 直接文件 size」，所以聚合块面积可精确反推，
//! 树图面积总和恒等于当前目录 size；每帧只算当前目录的直接子项，下钻 / 返回命中缓存。

use std::collections::{HashMap, HashSet};
use std::sync::Arc;
use std::sync::mpsc::{self, Receiver, TryRecvError};
use std::time::{Duration, Instant};

use eframe::egui::{
    self, pos2, vec2, Align2, Color32, CornerRadius, FontId, Rect, RichText, Sense, Stroke,
    StrokeKind, Ui, Vec2,
};

use crate::db;
use crate::icons::{self, Glyph};
use crate::theme::{self, Palette};

mod colors;
mod diskmap;
mod layout;
mod pie;
mod volume;

// ---------- 布局与预算常量（单位：点） ----------

/// 顶部汇总条高度。
const SUMMARY_H: f32 = 54.0;
/// 卷卡片最小宽度（Web 版 auto-fit minmax(330px,1fr)）。
const CARD_MIN_W: f32 = 330.0;
/// 卷卡片最大宽度：一块盘只有 1~2 个卷时不让卡片无限拉宽。
const MAX_CARD_W: f32 = 520.0;
const CARD_H: f32 = 150.0;
const GRID_GAP: f32 = 8.0;
/// 磁盘行标题高度与行间距。
const DISK_TITLE_H: f32 = 22.0;
const ROW_GAP: f32 = 4.0;
/// 「系统保留」折叠条 / 展开后每行的高度。
const RESERVED_H: f32 = 20.0;
const RESERVED_ROW_H: f32 = 20.0;
/// 单目录最多逐项渲染的子目录数（其余进聚合块）。
const DIR_LIMIT: u64 = 400;
/// 单目录最多逐项渲染的直接文件数。
const FILE_LIMIT: usize = 400;
/// 一次扫表的行块大小（按键集分页，避免 OFFSET 的平方开销）。
const SCAN_CHUNK: usize = 1500;
/// 下钻时允许花的扫描预算（毫秒）：超预算即把剩余项聚合成一块，帧时间可控。
const LOAD_BUDGET_MS: f64 = 5.0;
/// 展开聚合块时的逐项上限（无预算，用户显式点击才触发）。
const FULL_DIR_LIMIT: u64 = 4000;
const FULL_FILE_LIMIT: usize = 4000;
/// 层级列表最多展开到第几层（规格 5.5：只画已展开的浅层）。
const MAX_LIST_DEPTH: usize = 4;
/// 内容缓存条数上限。
const MAX_CACHE: usize = 128;
/// 物理拓扑后台探测的等待上限：超过就按盘符单列，不卡整页。
const TOPOLOGY_TIMEOUT: Duration = Duration::from_millis(2000);
/// 探测期间的 UI 轮询间隔（线程完成时也会 request_repaint，双保险）。
const TOPOLOGY_POLL: Duration = Duration::from_millis(30);

// ---------- 数据模型 ----------

/// 树图 / 列表里的一项：目录、文件，或「其他 N 项」聚合块。
#[derive(Clone, Debug)]
pub struct Item {
    pub path: String,
    pub name: String,
    pub size: i64,
    pub is_dir: bool,
    pub file_count: i64,
    pub mtime: i64,
    /// > 0 表示这是聚合块（值为被折叠的项数）。
    pub agg_count: u64,
}

impl Item {
    fn is_aggregate(&self) -> bool {
        self.agg_count > 0
    }
}

/// 一个目录的直接子项快照（总量来自 folder_sizes，文件来自 entries）。
#[derive(Clone)]
pub struct DirContent {
    pub path: String,
    pub self_size: i64,
    pub items: Vec<Item>,
    pub dir_total: u64,
    pub file_total: u64,
    pub complete: bool,
    pub load_ms: f64,
    /// 本次实际扫描的 entries 行数（自检 / 报告数字用，界面不显示）。
    #[allow(dead_code)]
    pub scanned: u64,
}

/// 一个卷的展示数据。
///
/// 两条口径严格分开，不允许再混用：
/// - 实际口径（环形图 / 百分比）：容量 − 实时空闲，数据来自 Win32；
/// - 快照口径（单独一行）：logical 是快照里该卷所有文件 size 的逻辑和，
///   含硬链接重复计数与 NTFS 元数据文件，可能大于卷容量。
#[derive(Clone, Debug)]
pub struct VolumeInfo {
    /// 快照根路径；快照没收录时用挂载点 / 卷 GUID 作标识。
    pub path: String,
    /// 显示名：有盘符时「C:」，无盘符时「卷 9dba3ee0」。
    pub name: String,
    /// 盘符（"C:"）；无盘符为 None。
    pub drive: Option<String>,
    /// 卷标（可能为空）。
    pub label: String,
    /// 文件系统（NTFS / FAT32…）。
    pub file_system: String,
    /// 快照逻辑大小（folder_sizes.size；快照没收录时为 0）。
    pub logical: i64,
    pub file_count: i64,
    /// 卷容量（Win32）。
    pub capacity: Option<u64>,
    /// 卷总空闲（GetDiskFreeSpaceExW 的 totalNumberOfFreeBytes）。
    pub free: Option<u64>,
    /// 所属物理磁盘号；映射失败为 None。
    pub disk: Option<u32>,
    /// 快照里是否有这个卷。
    pub has_snapshot: bool,
}

impl VolumeInfo {
    /// 实际已用 = 容量 − 实时空闲。取不到就 None，绝不退回快照逻辑大小。
    pub fn actual_used(&self) -> Option<u64> {
        Some(self.capacity?.saturating_sub(self.free?))
    }

    /// 实际占用比例；恒在 [0, 1]，不会出现「超出容量」。
    pub fn actual_share(&self) -> Option<f32> {
        let capacity = self.capacity.filter(|value| *value > 0)?;
        Some((self.actual_used()? as f64 / capacity as f64).clamp(0.0, 1.0) as f32)
    }

    /// 快照逻辑占容量比例；刻意不 clamp —— 逻辑允许大于容量，由界面负责解释。
    pub fn logical_share(&self) -> Option<f32> {
        let capacity = self.capacity.filter(|value| *value > 0)?;
        Some((self.logical.max(0) as f64 / capacity as f64) as f32)
    }

    /// 快照逻辑比实际多出的字节数（正数 = 逻辑更大）。
    pub fn logical_excess(&self) -> Option<i64> {
        let actual = self.actual_used()?;
        Some(self.logical.max(0) - actual as i64)
    }

    /// 逻辑是否大于容量：此时只加脚注解释，绝不显示 100% / 超出容量，也不标红。
    pub fn logical_over_capacity(&self) -> bool {
        self.has_snapshot
            && self.logical > 0
            && self
                .capacity
                .is_some_and(|capacity| self.logical as u64 > capacity)
    }

    /// 逻辑与实际差异是否值得说明（超过容量的 5%）。没有快照数据的卷不解释。
    pub fn needs_explanation(&self) -> bool {
        if !self.has_snapshot || self.logical <= 0 {
            return false;
        }
        let Some(capacity) = self.capacity.filter(|value| *value > 0) else {
            return false;
        };
        self.logical_excess()
            .is_some_and(|excess| excess.unsigned_abs().saturating_mul(20) > capacity)
    }

    /// 容量未知时环形图的参考占比：退回「占全库快照逻辑」。
    fn fallback_share(&self, total_logical: i64) -> f32 {
        if total_logical <= 0 {
            return 0.0;
        }
        (self.logical.max(0) as f64 / total_logical as f64) as f32
    }

    /// 排序键：盘符优先，其次显示名；无盘符的排后面。
    fn sort_key(&self) -> (u8, String) {
        match &self.drive {
            Some(drive) => (0, drive.clone()),
            None => (1, self.name.clone()),
        }
    }
}

/// 一行 = 一块物理磁盘；拿不到映射的卷进 number = None 的降级行。
#[derive(Clone, Debug)]
pub struct DiskRow {
    /// 稳定行标识（「系统保留」展开状态的键）：disk-0 / fallback。
    pub key: String,
    /// 物理盘号；None = 映射失败行。
    pub number: Option<u32>,
    /// 行标题：磁盘 0 · KINGSTON … · NVMe · 953.9 GB。
    pub title: String,
    /// 行右注：3 个卷 · 实际已用 555.9 GB / 953.9 GB。
    pub note: String,
    /// 有盘符的卷（按盘符升序）。
    pub volumes: Vec<VolumeInfo>,
    /// 无盘符的系统 / 恢复分区（默认收起的折叠区）。
    pub reserved: Vec<VolumeInfo>,
}

impl DiskRow {
    fn all_volumes(&self) -> impl Iterator<Item = &VolumeInfo> {
        self.volumes.iter().chain(self.reserved.iter())
    }

}

/// 顶部汇总条的数字（全部按实际口径，逻辑大小单独一个字段）。
#[derive(Clone, Copy, Debug, Default)]
pub struct Totals {
    pub disks: usize,
    pub volumes: usize,
    pub reserved: usize,
    pub capacity: u64,
    pub actual_used: u64,
    pub logical: i64,
    pub files: i64,
}

impl Totals {
    pub fn actual_share(&self) -> Option<f32> {
        if self.capacity == 0 {
            return None;
        }
        Some((self.actual_used as f64 / self.capacity as f64).clamp(0.0, 1.0) as f32)
    }

    /// 快照逻辑比实际多出的字节数（没有逻辑数据时为 None）。
    pub fn logical_excess(&self) -> Option<i64> {
        (self.capacity > 0 && self.logical > 0).then(|| self.logical - self.actual_used as i64)
    }
}

#[derive(Clone, Debug)]
enum Action {
    Drill(String),
    Open(String),
    Toggle(String),
    Select(String),
    ExpandAgg(String),
    CollapseAgg(String),
    Up,
    Home,
}

// ---------- 视图状态 ----------

/// 物理拓扑的懒加载状态。
///
/// 启动路径（App::new）只建空状态；空间页第一次 show() 时才起后台线程，
/// 检索页永远不会触发磁盘探测，首帧不受影响。
enum LoadState {
    /// 还没开始（空间页从未显示过）。
    Idle,
    /// 后台线程探测中。
    Running {
        rx: Receiver<Vec<DiskRow>>,
        started: Instant,
    },
    /// 已结束（成功 / 失败 / 超时），rows 即最终展示数据。
    Ready,
}

pub struct SpaceState {
    /// 当前目录；空字符串 = 「所有卷」总览。
    pub current: String,
    pub roots: Vec<db::FolderRow>,
    /// 物理磁盘分组（总览用）。
    pub rows: Vec<DiskRow>,
    /// 已被用户展开的「系统保留」折叠区（键 = DiskRow::key，默认全部收起）。
    reserved_open: HashSet<String>,
    /// 自检钩子：设 DISKINTEL_SPACE_RESERVED=1 时，行数据到达后默认展开所有「系统保留」。
    reserved_selftest: bool,
    /// 物理拓扑懒加载状态。
    load: LoadState,
    cache: HashMap<String, Arc<DirContent>>,
    expanded: HashSet<String>,
    agg_expanded: HashSet<String>,
    selected: Option<String>,
    last_load_ms: f64,
    last_report: Option<String>,
    /// 自检钩子：强制把某个瓷砖画成 hover 并显示 tooltip（截图回归用，不改变正常交互）。
    selftest_hover: Option<usize>,
    /// 自检钩子：首帧按点击该瓷砖处理（验证下钻链路）。
    selftest_click: Option<usize>,
    /// 自检钩子：进入目录后自动展开前 N 个子目录（验证层级列表展开）。
    selftest_expand: Option<usize>,
}

impl SpaceState {
    pub fn new(snap: &db::Snapshot) -> Self {
        // 只做轻量初始化：物理磁盘拓扑要几十次设备 IO（USB 机械盘唤醒可达 2 s 以上），
        // 必须等空间页第一次显示时在后台线程探测，检索页不该为它等首帧。
        let roots = snap.roots_rows();
        // 自检钩子：--view space 时可用环境变量直接指定初始目录（截图 / 回归用）。
        let mut current = String::new();
        if let Ok(path) = std::env::var("DISKINTEL_SPACE_PATH") {
            let path = path.trim().to_owned();
            if !path.is_empty() {
                current = path;
            }
        }
        // 自检钩子：设 DISKINTEL_SPACE_RESERVED=1 时，等行数据到达后展开所有「系统保留」（截图回归用）。
        let reserved_selftest = std::env::var_os("DISKINTEL_SPACE_RESERVED").is_some();
        let selftest = |name: &str| -> Option<usize> {
            std::env::var(name)
                .ok()
                .and_then(|raw| raw.trim().parse::<usize>().ok())
        };
        Self {
            current,
            roots,
            rows: Vec::new(),
            reserved_open: HashSet::new(),
            reserved_selftest,
            load: LoadState::Idle,
            cache: HashMap::new(),
            expanded: HashSet::new(),
            agg_expanded: HashSet::new(),
            selected: None,
            last_load_ms: 0.0,
            last_report: None,
            selftest_hover: selftest("DISKINTEL_SPACE_HOVER"),
            selftest_click: selftest("DISKINTEL_SPACE_CLICK"),
            selftest_expand: selftest("DISKINTEL_SPACE_EXPAND"),
        }
    }

    /// 从检索视图「定位到空间」进入：目录直接进，文件进所在目录。
    pub fn focus_on(&mut self, path: &str, snap: &db::Snapshot) {
        self.roots = snap.roots_rows();
        let target = if snap.dir_info(path).is_some() {
            path.to_owned()
        } else {
            db::parent_dir(path).unwrap_or_else(|| path.to_owned())
        };
        self.current = target;
        self.expanded.clear();
        self.agg_expanded.clear();
    }

    /// 物理拓扑懒加载：空间页第一次显示时才起后台线程；之后每帧轮询结果。
    ///
    /// 主线程永远不做设备探测：首帧立即出占位，线程完成后 swap 成完整分组。
    fn ensure_loaded(&mut self, ctx: &egui::Context) {
        self.poll_topology(ctx);
        if !matches!(&self.load, LoadState::Idle) {
            return;
        }
        let roots = self.roots.clone();
        let (tx, rx) = mpsc::channel();
        let repaint = ctx.clone();
        let spawned = std::thread::Builder::new()
            .name("diskintel-space-topology".to_owned())
            .spawn(move || {
                // 后台线程里完成「设备探测 + 分组」的整套工作，主线程只做一次 swap。
                let topology = diskmap::probe();
                let rows = build_rows(&roots, &topology);
                let _ = tx.send(rows);
                repaint.request_repaint();
            });
        self.load = match spawned {
            Ok(_) => LoadState::Running {
                rx,
                started: Instant::now(),
            },
            // 线程都起不来：直接降级为「按盘符单列」，不影响整页可用。
            Err(_) => LoadState::Ready,
        };
    }

    /// 轮询后台拓扑线程；超时 / 线程异常时降级为按盘符单列。
    fn poll_topology(&mut self, ctx: &egui::Context) {
        if !matches!(&self.load, LoadState::Running { .. }) {
            return;
        }
        let LoadState::Running { rx, started } = std::mem::replace(&mut self.load, LoadState::Ready)
        else {
            return;
        };
        match rx.try_recv() {
            Ok(rows) => self.accept_rows(rows),
            Err(TryRecvError::Empty) if started.elapsed() < TOPOLOGY_TIMEOUT => {
                ctx.request_repaint_after(TOPOLOGY_POLL);
                self.load = LoadState::Running { rx, started };
            }
            Err(_) => {
                // 超时或线程异常：此时才允许主线程做一次快速降级分组（只有卷级查询）。
                let rows = build_rows(&self.roots, &diskmap::Topology::default());
                self.accept_rows(rows);
            }
        }
    }

    fn accept_rows(&mut self, rows: Vec<DiskRow>) {
        self.rows = rows;
        if self.reserved_selftest {
            self.reserved_open = self.rows.iter().map(|row| row.key.clone()).collect();
        }
    }

    fn topology_loading(&self) -> bool {
        matches!(&self.load, LoadState::Idle | LoadState::Running { .. })
    }

    /// 展开的目录若还没载入，补一次（每层只发生一次）。
    fn prefetch_expanded(&mut self, snap: &db::Snapshot) {
        let pending: Vec<String> = self
            .expanded
            .iter()
            .filter(|path| !self.cache.contains_key(*path))
            .cloned()
            .collect();
        for path in pending {
            self.ensure(snap, &path, false);
        }
    }

    /// 取目录内容：命中缓存直接返回，否则现算（预算见常量）。
    fn ensure(&mut self, snap: &db::Snapshot, path: &str, full: bool) -> Arc<DirContent> {
        if let Some(hit) = self.cache.get(path) {
            return hit.clone();
        }
        let content = Arc::new(if full {
            load_full(snap, path)
        } else {
            load(snap, path, LOAD_BUDGET_MS, DIR_LIMIT, FILE_LIMIT)
        });
        self.last_load_ms = content.load_ms;
        if self.cache.len() >= MAX_CACHE {
            self.cache.clear();
        }
        self.cache.insert(path.to_owned(), content.clone());
        content
    }

    fn up(&mut self) {
        if self.current.is_empty() {
            return;
        }
        match db::parent_dir(&self.current) {
            Some(parent) if !parent.is_empty() && parent != self.current => {
                self.current = parent;
            }
            _ => self.current.clear(),
        }
        self.selected = None;
    }

    fn apply(&mut self, action: Option<Action>, snap: &db::Snapshot) {
        match action {
            None => {}
            Some(Action::Drill(path)) => {
                self.current = path;
                self.selected = None;
            }
            Some(Action::Open(path)) => {
                if let Err(error) = open_path(&path) {
                    eprintln!("打开失败：{error}");
                }
            }
            Some(Action::Toggle(path)) => {
                if !self.expanded.remove(&path) {
                    self.expanded.insert(path);
                }
            }
            Some(Action::Select(path)) => self.selected = Some(path),
            Some(Action::ExpandAgg(path)) => {
                self.agg_expanded.insert(path.clone());
                // 聚合块的面积 = 剩余项 size 之和，展开后逐项铺满同一面积（面积守恒）。
                self.cache.remove(&path);
                let _ = self.ensure(snap, &path, true);
            }
            Some(Action::CollapseAgg(path)) => {
                self.agg_expanded.remove(&path);
                self.cache.remove(&path);
                let _ = self.ensure(snap, &path, false);
            }
            Some(Action::Up) => self.up(),
            Some(Action::Home) => {
                self.current.clear();
                self.selected = None;
            }
        }
    }

    // ---------- 入口 ----------

    pub fn show(&mut self, ui: &mut Ui, pal: &Palette, snap: &db::Snapshot) {
        if snap.is_empty() {
            crate::widgets::empty_state(
                ui,
                pal,
                "还没有任何快照",
                "点击工具栏「扫描」建立第一份索引；这里会显示各卷空间分布与目录矩形树图。",
            );
            return;
        }
        if self.roots.is_empty() {
            self.roots = snap.roots_rows();
        }
        // 只有空间页真正显示时才触发物理拓扑探测（检索页 / 首帧完全不受影响）。
        self.ensure_loaded(ui.ctx());
        if self.current.is_empty() {
            self.overview(ui, pal, snap);
        } else {
            self.detail(ui, pal, snap);
        }
    }

    // ---------- 1. 物理磁盘 / 卷总览 ----------

    fn overview(&mut self, ui: &mut Ui, pal: &Palette, snap: &db::Snapshot) {
        if self.rows.is_empty() {
            if self.topology_loading() {
                // 后台线程探测物理拓扑（毫秒级；异常慢时 2 s 后降级），先给占位不卡帧。
                crate::widgets::empty_state(
                    ui,
                    pal,
                    "正在读取磁盘信息…",
                    "物理磁盘拓扑在后台探测，完成后自动补全；其他页面不受影响。",
                );
            } else {
                crate::widgets::empty_state(
                    ui,
                    pal,
                    "没有可显示的卷",
                    "系统里没有枚举到任何卷，快照里也没有目录聚合数据；重建一次索引即可。",
                );
            }
            return;
        }
        let totals = totals(&self.rows);
        let rows = self.rows.clone();
        let mut open = self.reserved_open.clone();
        let mut clicked: Option<String> = None;
        let mut toggled: Vec<String> = Vec::new();
        egui::ScrollArea::vertical()
            .id_salt("space-overview")
            .auto_shrink([false, false])
            .show(ui, |ui| {
                let width = ui.available_width();
                let (head, _) = ui.allocate_exact_size(vec2(width, SUMMARY_H), Sense::hover());
                paint_summary(ui, pal, head, snap, &totals);
                ui.add_space(4.0);
                for (index, row) in rows.iter().enumerate() {
                    let expanded = open.contains(&row.key);
                    let height = row_height(row, width, expanded);
                    let (rect, _) = ui.allocate_exact_size(vec2(width, height), Sense::hover());
                    let (action, toggle) =
                        paint_disk_row(ui, pal, rect, row, index, expanded, totals.logical);
                    if let Some(path) = action {
                        clicked = Some(path);
                    }
                    if toggle {
                        toggled.push(row.key.clone());
                    }
                    ui.add_space(ROW_GAP);
                }
                paint_caliber_footnote(ui, pal, width, &rows);
                ui.add_space(6.0);
            });
        for key in toggled {
            if !open.remove(&key) {
                open.insert(key);
            }
        }
        self.reserved_open = open;
        if let Some(path) = clicked {
            self.current = path;
        }
    }

    // ---------- 2. 下钻：树图 + 层级列表 ----------

    fn detail(&mut self, ui: &mut Ui, pal: &Palette, snap: &db::Snapshot) {
        if ui.ctx().memory(|m| m.focused().is_none()) {
            let (back, right) = ui.input(|i| {
                (
                    i.key_pressed(egui::Key::Backspace)
                        || i.key_pressed(egui::Key::ArrowLeft),
                    i.key_pressed(egui::Key::ArrowRight),
                )
            });
            if back {
                self.up();
            } else if let Some(path) = right
                .then(|| self.selected.clone())
                .flatten()
                .filter(|path| snap.dir_info(path).is_some())
            {
                self.current = path;
            }
        }

        let path = self.current.clone();
        let full = self.agg_expanded.contains(&path);
        let content = self.ensure(snap, &path, full);
        if let Some(count) = self.selftest_expand.take() {
            for item in content.items.iter().filter(|item| item.is_dir).take(count) {
                self.expanded.insert(item.path.clone());
            }
        }
        self.prefetch_expanded(snap);

        let full_rect = ui.available_rect_before_wrap();
        ui.allocate_rect(full_rect, Sense::hover());
        let crumb_h = 34.0;
        let legend_h = 24.0;
        let gap = 8.0;
        let crumb = Rect::from_min_size(full_rect.min, vec2(full_rect.width(), crumb_h));
        let legend = Rect::from_min_max(
            pos2(full_rect.left(), full_rect.bottom() - legend_h),
            full_rect.max,
        );
        let body = Rect::from_min_max(
            pos2(full_rect.left(), crumb.bottom() + gap),
            pos2(full_rect.right(), legend.top() - 2.0),
        );
        let left_w = ((body.width() - gap) * 0.55).max(220.0);
        let tree_rect = Rect::from_min_size(body.min, vec2(left_w, body.height()));
        let list_rect = Rect::from_min_max(pos2(tree_rect.right() + gap, body.top()), body.max);

        // 三块每帧都画；动作只取第一个（点击只可能命中一处）。
        let crumb_action = self.crumb_bar(ui, pal, snap, crumb, &content);
        let tree_action = self.treemap(ui, pal, &content, tree_rect);
        let list_action = self.hierarchy(ui, pal, &content, list_rect);
        paint_legend(ui, pal, legend);
        let action = crumb_action.or(tree_action).or(list_action);
        if action.is_some() && self.selftest_click.is_some() {
            self.selftest_click = None;
        }
        self.apply(action, snap);
    }

    /// 面包屑：返回 / 所有卷 / 逐级路径 + 右侧统计。
    fn crumb_bar(
        &mut self,
        ui: &mut Ui,
        pal: &Palette,
        snap: &db::Snapshot,
        rect: Rect,
        content: &Arc<DirContent>,
    ) -> Option<Action> {
        let painter = ui.painter();
        painter.rect_filled(rect, CornerRadius::same(theme::RADIUS_SM), pal.panel);
        painter.rect_stroke(
            rect,
            CornerRadius::same(theme::RADIUS_SM),
            Stroke::new(1.0, pal.border_soft),
            StrokeKind::Inside,
        );
        let mut bar = ui.new_child(
            egui::UiBuilder::new()
                .max_rect(rect.shrink2(vec2(6.0, 0.0)))
                .layout(egui::Layout::left_to_right(egui::Align::Center)),
        );
        let mut action = None;
        if crate::widgets::icon_button(&mut bar, pal, Glyph::Left, 24.0, "返回上级（Backspace）")
            .clicked()
        {
            action = Some(Action::Up);
        }
        if crate::widgets::pill_button(&mut bar, pal, "所有卷", Some(Glyph::Disk), false).clicked() {
            action = Some(Action::Home);
        }
        crate::widgets::vsep(&mut bar, pal);

        let ancestors = snap.ancestors(&self.current);
        for (index, ancestor) in ancestors.iter().enumerate() {
            if index > 0 {
                bar.label(
                    RichText::new("›")
                        .size(12.0)
                        .color(pal.text_faint),
                );
            }
            let label = if ancestor.ends_with('\\') {
                ancestor.clone()
            } else {
                db::file_name(ancestor).to_owned()
            };
            let last = index + 1 == ancestors.len();
            let color = if last { pal.text } else { pal.text_dim };
            let galley =
                bar.painter()
                    .layout_no_wrap(label, FontId::proportional(12.5), color);
            let size = vec2(galley.size().x + 12.0, 22.0);
            let (cell, response) = bar.allocate_exact_size(size, Sense::click());
            if response.hovered() && !last {
                bar.painter()
                    .rect_filled(cell, CornerRadius::same(4), pal.hover);
            }
            bar.painter().galley(
                pos2(cell.left() + 6.0, cell.center().y - galley.size().y * 0.5),
                galley,
                color,
            );
            if response.clicked() && !last {
                action = Some(Action::Drill(ancestor.clone()));
            }
        }

        bar.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
            ui.label(
                RichText::new(format!(
                    "{} 项 · {} · 载入 {:.1} ms",
                    db::thousands(content.items.len() as u64),
                    db::human_size(content.self_size),
                    self.last_load_ms
                ))
                .size(11.0)
                .color(pal.text_faint),
            );
            if self.agg_expanded.contains(&self.current)
                && crate::widgets::pill_button(ui, pal, "收起聚合", None, false).clicked()
            {
                action = Some(Action::CollapseAgg(self.current.clone()));
            }
        });
        action
    }

    // ---------- 3. 矩形树图 ----------

    fn treemap(
        &mut self,
        ui: &mut Ui,
        pal: &Palette,
        content: &Arc<DirContent>,
        rect: Rect,
    ) -> Option<Action> {
        let painter = ui.painter();
        painter.rect_filled(rect, CornerRadius::same(theme::RADIUS_MD), pal.panel);
        painter.rect_stroke(
            rect,
            CornerRadius::same(theme::RADIUS_MD),
            Stroke::new(1.0, pal.border_soft),
            StrokeKind::Inside,
        );
        let inner = rect.shrink(1.0);
        if content.items.is_empty() {
            painter.text(
                inner.center(),
                Align2::CENTER_CENTER,
                "（此目录没有子项）",
                FontId::proportional(12.0),
                pal.text_faint,
            );
            return None;
        }

        let weights: Vec<f64> = content.items.iter().map(|item| layout::weight(item.size)).collect();
        let out = layout::layout(&weights, inner);

        // 验收 b：面积总和 == 当前目录 folder_sizes.size（误差 < 0.5%），断言并打印。
        let area_sum = out.area();
        let container = layout::rect_area(&inner).max(1.0);
        let weight_sum: f64 = weights.iter().sum();
        let expected = content.self_size as f64;
        let area_err = (area_sum as f64 - container as f64).abs() / container as f64;
        let weight_err = if expected > 0.0 {
            (weight_sum - expected).abs() / expected
        } else {
            0.0
        };
        let key = format!("{}:{}x{}", content.path, inner.width() as i32, inner.height() as i32);
        if self.last_report.as_deref() != Some(key.as_str()) {
            eprintln!(
                "树图守恒 {} · 面积和/容器 {:.6} · 权重和 {:.0} / 目录 {} · 面积误差 {:.4}% · 权重误差 {:.6}%{}",
                content.path,
                area_sum as f64 / container as f64,
                weight_sum,
                content.self_size,
                area_err * 100.0,
                weight_err * 100.0,
                if content.complete { "" } else { " ·（含聚合块）" }
            );
            debug_assert!(area_err < 0.005, "树图面积不守恒：{area_err}");
            debug_assert!(weight_err < 0.005, "树图权重与目录 size 不一致：{weight_err}");
            self.last_report = Some(key);
        }

        let seam = colors::seam(pal.dark);
        let mut action = None;
        let forced_hover = self
            .selftest_hover
            .filter(|index| *index < content.items.len());
        let forced_click = self
            .selftest_click
            .filter(|index| *index < content.items.len());
        for (index, item) in content.items.iter().enumerate() {
            let tile = out.rects[index];
            if !tile.is_positive() {
                continue;
            }
            let response = ui.interact(
                tile,
                ui.scope_id().with(("tile", index)),
                Sense::click(),
            );
            let hovered = response.hovered() || forced_hover == Some(index);
            if hovered {
                ui.ctx().set_cursor_icon(egui::CursorIcon::PointingHand);
            }
            let base = if item.is_aggregate() {
                colors::AGGREGATE
            } else {
                colors::tile(&item.path, item.is_dir)
            };
            let fill = if hovered { base.gamma_multiply(1.08) } else { base };
            let body = tile.shrink(0.5);
            ui.painter().rect_filled(body, CornerRadius::ZERO, fill);
            // 1px 白缝（规格 5.3）
            ui.painter().rect_stroke(
                body,
                CornerRadius::ZERO,
                Stroke::new(1.0, seam),
                StrokeKind::Inside,
            );
            if item.is_aggregate() {
                // 45 度斜纹 + 虚线内框（规格 5.3 / 聚合块可辨识）
                paint_stripes(ui.painter(), body, Color32::from_rgba_unmultiplied(255, 255, 255, 36));
                dashed_rect(
                    ui.painter(),
                    body.shrink(3.0),
                    Stroke::new(1.0, Color32::from_rgba_unmultiplied(255, 255, 255, 178)),
                    3.0,
                    3.0,
                );
            }
            if hovered {
                ui.painter().rect_stroke(
                    body.shrink(1.0),
                    CornerRadius::ZERO,
                    Stroke::new(2.0, Color32::from_rgba_unmultiplied(255, 255, 255, 210)),
                    StrokeKind::Inside,
                );
            }

            // 文字：name 上、明细下（规格 5.3）
            let ink = colors::ink(fill);
            let pad = 5.0;
            if body.width() >= 34.0 && body.height() >= 18.0 {
                let name = fit_chars(&item.name, 11.0, body.width() - pad * 2.0);
                ui.painter().text(
                    pos2(body.left() + pad, body.top() + 4.0),
                    Align2::LEFT_TOP,
                    name,
                    FontId::proportional(11.0),
                    ink,
                );
            }
            if body.height() >= 34.0 && body.width() >= 46.0 {
                let detail = if item.is_aggregate() {
                    format!("{} · 点击展开", db::human_size(item.size))
                } else if item.is_dir {
                    format!(
                        "{} · {} 文件",
                        db::human_size(item.size),
                        db::thousands(item.file_count.max(0) as u64)
                    )
                } else {
                    format!(
                        "{} · {}",
                        db::human_size(item.size),
                        icons::label(icons::classify(&item.path, false))
                    )
                };
                ui.painter().text(
                    pos2(body.left() + pad, body.bottom() - 4.0),
                    Align2::LEFT_BOTTOM,
                    fit_chars(&detail, 10.0, body.width() - pad * 2.0),
                    FontId::proportional(10.0),
                    ink.gamma_multiply(0.92),
                );
            }

            let response = response.on_hover_ui(|ui| tooltip(ui, pal, item, content.self_size));
            if forced_hover == Some(index) {
                // 自检：无交互桌面时也能截到 tooltip 的真实渲染。
                egui::Area::new(ui.scope_id().with("space-selftest-tip"))
                    .order(egui::Order::Tooltip)
                    .fixed_pos(body.center() + vec2(24.0, 24.0))
                    .show(ui.ctx(), |ui| {
                        egui::Frame::popup(ui.style())
                            .show(ui, |ui| tooltip(ui, pal, item, content.self_size));
                    });
            }
            if response.clicked() || forced_click == Some(index) {
                action = Some(if item.is_aggregate() {
                    Action::ExpandAgg(content.path.clone())
                } else if item.is_dir {
                    Action::Drill(item.path.clone())
                } else {
                    Action::Open(item.path.clone())
                });
            }
        }
        action
    }

    // ---------- 4. 层级列表 ----------

    fn hierarchy(
        &mut self,
        ui: &mut Ui,
        pal: &Palette,
        content: &Arc<DirContent>,
        rect: Rect,
    ) -> Option<Action> {
        let painter = ui.painter();
        painter.rect_filled(rect, CornerRadius::same(theme::RADIUS_MD), pal.panel);
        painter.rect_stroke(
            rect,
            CornerRadius::same(theme::RADIUS_MD),
            Stroke::new(1.0, pal.border_soft),
            StrokeKind::Inside,
        );
        let head_h = 26.0;
        let head = Rect::from_min_size(rect.min, vec2(rect.width(), head_h));
        painter.rect_filled(
            head,
            CornerRadius {
                nw: theme::RADIUS_MD,
                ne: theme::RADIUS_MD,
                sw: 0,
                se: 0,
            },
            pal.panel_alt,
        );
        let mut head_ui = ui.new_child(
            egui::UiBuilder::new()
                .max_rect(head.shrink2(vec2(10.0, 0.0)))
                .layout(egui::Layout::left_to_right(egui::Align::Center)),
        );
        head_ui.label(
            RichText::new("当前目录分层")
                .size(11.5)
                .strong()
                .color(pal.text),
        );
        head_ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
            let note = if content.complete {
                format!(
                    "{} 项 · 合计 {}",
                    db::thousands(content.dir_total + content.file_total),
                    db::human_size(content.self_size)
                )
            } else {
                format!(
                    "{} 目录 / {} 文件（部分）· 合计 {}",
                    db::thousands(content.dir_total),
                    db::thousands(content.file_total),
                    db::human_size(content.self_size)
                )
            };
            ui.label(RichText::new(note).size(10.5).color(pal.text_faint));
        });

        // 展开行：沿用缓存里的子目录内容
        let rows = collect_rows(content, &self.expanded, &self.cache);
        let body = Rect::from_min_max(
            pos2(rect.left() + 1.0, head.bottom()),
            pos2(rect.right() - 1.0, rect.bottom() - 1.0),
        );
        let mut body_ui = ui.new_child(
            egui::UiBuilder::new()
                .max_rect(body)
                .layout(egui::Layout::top_down(egui::Align::LEFT)),
        );
        let mut action: Option<Action> = None;
        let selected = self.selected.clone();
        egui::ScrollArea::vertical()
            .id_salt("space-hierarchy")
            .auto_shrink([false, false])
            .show(&mut body_ui, |ui| {
                for (index, (depth, parent_size, item)) in rows.iter().enumerate() {
                    let is_selected = selected.as_deref() == Some(item.path.as_str());
                    let found = row(
                        ui,
                        pal,
                        item,
                        *parent_size,
                        RowFx {
                            index,
                            depth: *depth,
                            expanded: self.expanded.contains(&item.path),
                            selected: is_selected,
                        },
                    );
                    if action.is_none() {
                        action = found;
                    }
                }
                if rows.is_empty() {
                    ui.add_space(12.0);
                    ui.label(
                        RichText::new("（此目录没有子项）")
                            .size(12.0)
                            .color(pal.text_faint),
                    );
                }
            });
        action
    }

}

// ---------- 物理磁盘分组 ----------

/// 把「快照根」和「系统卷拓扑」合成按物理盘分组的行。
///
/// 降级策略（任何一步失败都不能空屏或 panic）：
/// 1. 拓扑为空（卷枚举失败）→ 所有快照根进同一行「未识别物理盘」，按盘符单列；
/// 2. 个别卷拿不到磁盘号 → 同样进「未识别物理盘」行；
/// 3. 快照没收录的卷照样显示，只是没有快照逻辑大小与文件数。
fn build_rows(roots: &[db::FolderRow], topo: &diskmap::Topology) -> Vec<DiskRow> {
    let mut matched = vec![false; roots.len()];
    let mut volumes: Vec<VolumeInfo> = Vec::with_capacity(topo.volumes.len() + roots.len());

    for volume in &topo.volumes {
        let index = roots.iter().position(|row| root_matches(&row.path, volume));
        if let Some(index) = index {
            matched[index] = true;
        }
        volumes.push(volume_info(volume, index.map(|index| &roots[index])));
    }

    // 拓扑里找不到的快照根（旧快照 / 已拔出的盘）：按盘符单列，不丢弃。
    for (index, row) in roots.iter().enumerate() {
        if !matched[index] {
            volumes.push(orphan_volume(row));
        }
    }

    let mut numbers: Vec<u32> = volumes.iter().filter_map(|volume| volume.disk).collect();
    numbers.sort_unstable();
    numbers.dedup();

    let mut rows: Vec<DiskRow> = numbers
        .into_iter()
        .map(|number| disk_row(number, &volumes, &topo.disks))
        .collect();

    let mut loose: Vec<VolumeInfo> = volumes
        .into_iter()
        .filter(|volume| volume.disk.is_none())
        .collect();
    if !loose.is_empty() {
        loose.sort_by_key(|volume| volume.sort_key());
        let note = topo
            .note
            .as_deref()
            .unwrap_or("卷→物理盘映射失败，按盘符单列");
        rows.push(fallback_row(loose, note));
    }
    rows
}

/// 一块物理盘：标题（盘号 · 型号 · 总线 · 容量）+ 有盘符的卷 + 无盘符的系统保留。
fn disk_row(number: u32, volumes: &[VolumeInfo], disks: &[diskmap::DiskInfo]) -> DiskRow {
    let mut letters: Vec<VolumeInfo> = volumes
        .iter()
        .filter(|volume| volume.disk == Some(number) && volume.drive.is_some())
        .cloned()
        .collect();
    letters.sort_by_key(|volume| volume.sort_key());

    let mut reserved: Vec<VolumeInfo> = volumes
        .iter()
        .filter(|volume| volume.disk == Some(number) && volume.drive.is_none())
        .cloned()
        .collect();
    reserved.sort_by(|a, b| a.name.cmp(&b.name));

    let disk = disks.iter().find(|disk| disk.number == number);
    let mut title = format!("磁盘 {number}");
    let mut size = 0u64;
    if let Some(disk) = disk {
        if let Some(model) = diskmap::model_label(disk) {
            title.push_str(" · ");
            title.push_str(&model);
        }
        if let Some(bus) = diskmap::bus_label(disk.bus) {
            title.push_str(" · ");
            title.push_str(bus);
        }
        size = disk.size;
    } else {
        title.push_str(" · 型号未知");
    }
    if size == 0 {
        // 物理盘容量查询失败时，用该盘各卷容量之和兜底。
        size = letters
            .iter()
            .chain(reserved.iter())
            .filter_map(|volume| volume.capacity)
            .sum();
    }
    if size > 0 {
        title.push_str(&format!(" · {}", db::human_size(size as i64)));
    }

    let mut row = DiskRow {
        key: format!("disk-{number}"),
        number: Some(number),
        title,
        note: String::new(),
        volumes: letters,
        reserved,
    };
    row.note = row_note(&row);
    row
}

/// 卷→物理盘映射失败时的降级行：按盘符单列，绝不空白。
fn fallback_row(mut volumes: Vec<VolumeInfo>, note: &str) -> DiskRow {
    volumes.sort_by_key(|volume| volume.sort_key());
    let (letters, reserved): (Vec<VolumeInfo>, Vec<VolumeInfo>) = volumes
        .into_iter()
        .partition(|volume| volume.drive.is_some());
    let mut row = DiskRow {
        key: "fallback".to_owned(),
        number: None,
        title: "未识别物理盘 · 按盘符单列".to_owned(),
        note: String::new(),
        volumes: letters,
        reserved,
    };
    row.note = format!("{} 个卷 · {note}", row.volumes.len());
    row
}

fn row_note(row: &DiskRow) -> String {
    // 只统计有盘符的卷：行右注的「N 个卷」与容量必须描述同一集合，
    // 无盘符分区另有「系统保留」折叠行单独记账。
    let mut capacity = 0u64;
    let mut used = 0u64;
    for volume in &row.volumes {
        if let Some(total) = volume.capacity {
            capacity += total;
            used += volume.actual_used().unwrap_or(0);
        }
    }
    let mut note = format!("{} 个卷", row.volumes.len());
    if capacity > 0 {
        note.push_str(&format!(
            " · 实际已用 {} / {}",
            db::human_size(used as i64),
            db::human_size(capacity as i64)
        ));
    }
    note
}

fn volume_info(topology: &diskmap::VolumeTopo, row: Option<&db::FolderRow>) -> VolumeInfo {
    let drive = topology
        .mount_points
        .iter()
        .find_map(|point| drive_name(point));
    let name = drive
        .clone()
        .unwrap_or_else(|| volume_label(&topology.guid));
    let probe_root = drive
        .as_ref()
        .map(|drive| format!("{drive}\\"))
        .unwrap_or_else(|| topology.guid.clone());
    let space = volume::probe(&probe_root);
    let (logical, file_count, has_snapshot) = match row {
        Some(row) => (row.size, row.file_count, true),
        None => (0, 0, false),
    };
    VolumeInfo {
        path: row.map(|row| row.path.clone()).unwrap_or(probe_root),
        name,
        drive,
        label: topology.label.clone(),
        file_system: topology.file_system.clone(),
        logical,
        file_count,
        capacity: space.map(|space| space.total),
        free: space.map(|space| space.free),
        disk: topology.disk,
        has_snapshot,
    }
}

fn orphan_volume(row: &db::FolderRow) -> VolumeInfo {
    let drive = drive_name(&row.path);
    let space = volume::probe(&row.path);
    VolumeInfo {
        path: row.path.clone(),
        name: drive.clone().unwrap_or_else(|| volume_label(&row.path)),
        drive,
        label: String::new(),
        file_system: String::new(),
        logical: row.size,
        file_count: row.file_count,
        capacity: space.map(|space| space.total),
        free: space.map(|space| space.free),
        disk: None,
        has_snapshot: true,
    }
}

/// 盘符形态：C:\ / C: → C:；目录挂载点返回 None。
fn drive_name(path: &str) -> Option<String> {
    let trimmed = path.trim_end_matches(['\\', '/']);
    let mut chars = trimmed.chars();
    let letter = chars.next()?;
    if chars.next() != Some(':') || chars.next().is_some() {
        return None;
    }
    if letter.is_ascii_alphabetic() {
        Some(format!("{}:", letter.to_ascii_uppercase()))
    } else {
        None
    }
}

fn root_matches(root: &str, volume: &diskmap::VolumeTopo) -> bool {
    let normalized = root.trim_end_matches(['\\', '/']);
    if normalized.eq_ignore_ascii_case(volume.guid.trim_end_matches(['\\', '/'])) {
        return true;
    }
    volume.mount_points.iter().any(|point| {
        point
            .trim_end_matches(['\\', '/'])
            .eq_ignore_ascii_case(normalized)
    })
}

fn volume_label(path: &str) -> String {
    if let Some(rest) = path.strip_prefix(r"\\?\Volume{") {
        let short: String = rest.chars().take(8).collect();
        return format!("卷 {short}");
    }
    path.to_owned()
}

/// 顶部汇总：容量 / 实际已用只信任 Win32 口径，逻辑大小单独累计。
fn totals(rows: &[DiskRow]) -> Totals {
    let mut out = Totals::default();
    for row in rows {
        if row.number.is_some() {
            out.disks += 1;
        }
        for volume in row.all_volumes() {
            out.volumes += 1;
            if volume.drive.is_none() {
                out.reserved += 1;
            }
            if let Some(capacity) = volume.capacity {
                out.capacity += capacity;
                out.actual_used += volume.actual_used().unwrap_or(0);
            }
            if volume.has_snapshot {
                out.logical += volume.logical.max(0);
                out.files += volume.file_count.max(0);
            }
        }
    }
    out
}

/// 目录 → 子项（预算内）；超出预算的项聚合成「其他 N 项」，面积仍精确守恒。
fn load(
    snap: &db::Snapshot,
    path: &str,
    budget_ms: f64,
    dir_limit: u64,
    file_limit: usize,
) -> DirContent {
    let started = Instant::now();
    let debug = std::env::var_os("DISKINTEL_SPACE_DEBUG").is_some();
    let info = snap.dir_info(path);
    let self_size = info.as_ref().map(|row| row.size).unwrap_or(0);
    let self_files = info.as_ref().map(|row| row.file_count).unwrap_or(0);
    let t0 = Instant::now();
    let dirs = snap.child_dirs(path, dir_limit.max(1));
    let t_dirs = t0.elapsed().as_secs_f64() * 1000.0;
    let dir_total = dir_count(snap, path);
    // 所有子目录都在列表里时，file_count 之和直接由内存累加（省一次按表回查的 SUM）。
    let shown_dir_files: i64 = dirs.iter().map(|dir| dir.file_count).sum();
    let dir_files = if dir_total as usize > dirs.len() {
        dir_files_sum(snap, path)
    } else {
        shown_dir_files
    };
    let t_stats = t0.elapsed().as_secs_f64() * 1000.0 - t_dirs;
    let dir_mtimes = dir_mtimes(snap, &dirs);
    let t_mtimes = t0.elapsed().as_secs_f64() * 1000.0 - t_dirs - t_stats;
    let (files, scanned, scan_complete) = scan_files(snap, path, budget_ms, file_limit);
    if debug {
        eprintln!(
            "载入 {} · 目录查询 {t_dirs:.1} ms · 统计 {t_stats:.1} ms · 时间 {t_mtimes:.1} ms · 文件扫描 {:.1} ms · 合计 {:.1} ms（扫描 {} 行）",
            path,
            t0.elapsed().as_secs_f64() * 1000.0 - t_dirs - t_stats - t_mtimes,
            started.elapsed().as_secs_f64() * 1000.0,
            scanned
        );
    }

    let mut items: Vec<Item> = Vec::with_capacity(dirs.len() + files.len() + 1);
    for dir in dirs.iter() {
        items.push(Item {
            path: dir.path.clone(),
            name: db::file_name(&dir.path).to_owned(),
            size: dir.size,
            is_dir: true,
            file_count: dir.file_count,
            mtime: dir_mtimes.get(&dir.path).copied().unwrap_or(0),
            agg_count: 0,
        });
    }
    let shown_dirs = items.len() as u64;
    let shown_dir_sum: i64 = items.iter().map(|item| item.size).sum();
    let shown_files = files.len() as u64;
    let shown_file_sum: i64 = files.iter().map(|item| item.size).sum();
    items.extend(files);

    // 快照内严格成立：目录 size = Σ 子目录 size + Σ 直接文件 size
    let file_total_count = (self_files - dir_files).max(0) as u64;
    let residual_size = self_size - shown_dir_sum - shown_file_sum;
    let residual_count = dir_total.saturating_sub(shown_dirs)
        + file_total_count.saturating_sub(shown_files);
    let complete = scan_complete && residual_count == 0 && dir_total <= dir_limit;

    items.sort_by(|a, b| b.size.cmp(&a.size).then_with(|| a.path.cmp(&b.path)));
    if residual_count > 0 {
        items.push(Item {
            path: format!("{path}#其他"),
            name: format!("其他 {} 项", db::thousands(residual_count)),
            size: residual_size.max(0),
            is_dir: true,
            file_count: 0,
            mtime: 0,
            agg_count: residual_count,
        });
    }

    DirContent {
        path: path.to_owned(),
        self_size,
        items,
        dir_total,
        file_total: file_total_count,
        complete,
        load_ms: started.elapsed().as_secs_f64() * 1000.0,
        scanned,
    }
}

/// 无预算的完整载入（用户显式展开聚合块时才用）。
fn load_full(snap: &db::Snapshot, path: &str) -> DirContent {
    load(snap, path, 0.0, FULL_DIR_LIMIT, FULL_FILE_LIMIT)
}

/// 当前目录的直接子目录条数（纯索引扫描，不取表行）。
fn dir_count(snap: &db::Snapshot, path: &str) -> u64 {
    let Some(conn) = snap.conn() else {
        return 0;
    };
    let Ok(mut stmt) = conn
        .prepare_cached("SELECT COUNT(*) FROM folder_sizes WHERE parent_path = ?1")
    else {
        return 0;
    };
    stmt.query_row([path], |row| row.get::<_, i64>(0))
        .unwrap_or(0)
        .max(0) as u64
}

/// 全部子目录的文件数之和：仅当子目录超出展示上限时才需要（每行都要回表读 file_count）。
fn dir_files_sum(snap: &db::Snapshot, path: &str) -> i64 {
    let Some(conn) = snap.conn() else {
        return 0;
    };
    let Ok(mut stmt) = conn
        .prepare_cached("SELECT COALESCE(SUM(file_count),0) FROM folder_sizes WHERE parent_path = ?1")
    else {
        return 0;
    };
    stmt.query_row([path], |row| row.get::<_, i64>(0))
        .unwrap_or(0)
        .max(0)
}

/// 路径前缀扫描区间：[path, path 去掉尾部分隔符 + ']')，恰好覆盖整棵子树。
fn scan_bounds(path: &str) -> (String, String) {
    let trimmed = path.trim_end_matches(['\\', '/']);
    let base = if trimmed.is_empty() { path } else { trimmed };
    (path.to_owned(), format!("{base}]"))
}

/// 子目录的修改时间：folder_sizes 不存 mtime，按主键回查 entries（一次 IN，最多 400 条）。
fn dir_mtimes(snap: &db::Snapshot, dirs: &[db::FolderRow]) -> HashMap<String, i64> {
    let mut out: HashMap<String, i64> = HashMap::new();
    let Some(conn) = snap.conn() else {
        return out;
    };
    for chunk in dirs.chunks(400) {
        let placeholders = vec!["?"; chunk.len()].join(",");
        let sql = format!("SELECT path, mtime FROM entries WHERE path IN ({placeholders})");
        let Ok(mut stmt) = conn.prepare_cached(&sql) else {
            continue;
        };
        let params: Vec<&dyn rusqlite::ToSql> = chunk
            .iter()
            .map(|dir| &dir.path as &dyn rusqlite::ToSql)
            .collect();
        if let Ok(rows) = stmt.query_map(rusqlite::params_from_iter(params), |row| {
            Ok((row.get::<_, String>(0)?, row.get::<_, i64>(1)?))
        }) {
            for (dir_path, mtime) in rows.flatten() {
                out.insert(dir_path, mtime);
            }
        }
    }
    out
}

/// 在预算内按键集分页扫当前目录的直接文件。
fn scan_files(
    snap: &db::Snapshot,
    path: &str,
    budget_ms: f64,
    limit: usize,
) -> (Vec<Item>, u64, bool) {
    let mut files: Vec<Item> = Vec::new();
    let Some(conn) = snap.conn() else {
        return (files, 0, true);
    };
    let Ok(mut stmt) = conn.prepare_cached(
        "SELECT path, size, mtime, is_dir FROM entries \
         WHERE path > ?1 AND path < ?2 ORDER BY path LIMIT ?3",
    ) else {
        return (files, 0, true);
    };
    let (lo, hi) = scan_bounds(path);
    let started = Instant::now();
    let mut cursor = lo;
    let mut scanned = 0u64;
    let mut complete = false;
    loop {
        if budget_ms > 0.0 && started.elapsed().as_secs_f64() * 1000.0 > budget_ms {
            break;
        }
        let batch: Vec<(String, i64, i64, i64)> = stmt
            .query_map(rusqlite::params![cursor, hi, SCAN_CHUNK as i64], |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    row.get::<_, i64>(1)?,
                    row.get::<_, i64>(2)?,
                    row.get::<_, i64>(3)?,
                ))
            })
            .map(|iter| iter.flatten().collect())
            .unwrap_or_default();
        let full_chunk = batch.len() >= SCAN_CHUNK;
        for (row_path, size, mtime, is_dir) in batch {
            scanned += 1;
            cursor = row_path.clone();
            if db::parent_dir(&row_path).as_deref() != Some(path) {
                continue;
            }
            if is_dir == 0 {
                files.push(Item {
                    name: db::file_name(&row_path).to_owned(),
                    path: row_path,
                    size,
                    is_dir: false,
                    file_count: 0,
                    mtime,
                    agg_count: 0,
                });
                if files.len() >= limit {
                    return (files, scanned, false);
                }
            }
        }
        if !full_chunk {
            complete = true;
            break;
        }
    }
    (files, scanned, complete)
}

// ---------- 绘制：卷卡片 ----------

/// 卷卡片：环形图与百分比只表示「实际已用 = 容量 − 实时空闲」；
/// 快照逻辑大小单独一行，逻辑 > 容量时只加脚注解释，不显示 100%、不标红。
fn volume_card(
    ui: &mut Ui,
    pal: &Palette,
    rect: Rect,
    volume: &VolumeInfo,
    id: usize,
    total_logical: i64,
) -> bool {
    let response = ui.interact(
        rect,
        ui.scope_id().with(("vol-card", id)),
        Sense::click(),
    );
    let interactive = volume.has_snapshot;
    let hovered = response.hovered();
    if hovered && interactive {
        ui.ctx().set_cursor_icon(egui::CursorIcon::PointingHand);
    }
    let bg = if hovered { pal.hover } else { pal.card };
    let border = if hovered && interactive {
        pal.accent
    } else {
        pal.border_soft
    };
    let painter = ui.painter();
    painter.rect_filled(rect, CornerRadius::same(theme::RADIUS_MD), bg);
    painter.rect_stroke(
        rect,
        CornerRadius::same(theme::RADIUS_MD),
        Stroke::new(1.0, border),
        StrokeKind::Inside,
    );

    let pad = 10.0;
    let title = if volume.label.is_empty() {
        volume.name.clone()
    } else {
        format!("{} · {}", volume.name, volume.label)
    };
    painter.text(
        pos2(rect.left() + pad, rect.top() + 8.0),
        Align2::LEFT_TOP,
        fit_chars(&title, 13.0, rect.width() * 0.52),
        FontId::proportional(13.0),
        pal.text,
    );

    // 右上：实际占用百分比（唯一允许进入百分比的数字）。
    let actual_share = volume.actual_share();
    let status = match (volume.capacity, actual_share) {
        (Some(_), Some(share)) => format!("实际 {:.0}%", share * 100.0),
        (Some(_), None) => "空闲未知".to_owned(),
        (None, _) => "容量未知".to_owned(),
    };
    painter.text(
        pos2(rect.right() - pad, rect.top() + 9.0),
        Align2::RIGHT_TOP,
        status,
        FontId::proportional(11.0),
        pal.text_dim,
    );

    // 环形图：实际占比；容量未知时退回「占全库快照逻辑」并在中心写明。
    let (reference, center_caption) = match actual_share {
        Some(share) => (share, "实际已用"),
        None => (
            volume.fallback_share(total_logical).clamp(0.0, 1.0),
            "快照逻辑",
        ),
    };
    let radius = 36.0;
    let center = pos2(rect.left() + pad + radius + 2.0, rect.top() + 30.0 + radius);
    pie::donut(
        painter,
        center,
        radius,
        &[(reference, pal.accent)],
        Some(pal.border),
        colors::seam(pal.dark),
    );
    pie::punch(painter, center, radius * 0.60, bg);
    painter.text(
        pos2(center.x, center.y - 9.0),
        Align2::CENTER_CENTER,
        center_caption,
        FontId::proportional(8.5),
        pal.text_dim,
    );
    painter.text(
        pos2(center.x, center.y + 7.0),
        Align2::CENTER_CENTER,
        format!("{:.0}%", reference * 100.0),
        FontId::proportional(12.0),
        pal.text,
    );

    // 右侧明细：实际已用 / 空闲 / 容量 / 快照逻辑（快照逻辑单独一行并给出差额）。
    let legend_x = center.x + radius + 12.0;
    let legend_right = rect.right() - pad;
    let free_share = volume
        .capacity
        .filter(|capacity| *capacity > 0)
        .map(|capacity| volume.free.unwrap_or(0) as f64 / capacity as f64 * 100.0)
        .unwrap_or(0.0);
    let mut used_text = volume
        .actual_used()
        .map(|used| db::human_size(used as i64))
        .unwrap_or_else(|| "-".to_owned());
    if let Some(share) = actual_share {
        used_text.push_str(&format!(" · {:.0}%", share * 100.0));
    }
    let free_text = volume
        .free
        .map(|free| format!("{} · {free_share:.0}%", db::human_size(free as i64)))
        .unwrap_or_else(|| "-".to_owned());
    let capacity_text = volume
        .capacity
        .map(|value| db::human_size(value as i64))
        .unwrap_or_else(|| "未知".to_owned());
    let mut logical_text = if volume.has_snapshot {
        db::human_size(volume.logical.max(0))
    } else {
        "未收录".to_owned()
    };
    if let Some(excess) = volume.logical_excess().filter(|diff| *diff > 0) {
        logical_text.push_str(&format!(" · 多 {}", db::human_size(excess)));
    }
    let legend_rows: [(Option<Color32>, &str, String); 4] = [
        (Some(pal.accent), "实际已用", used_text),
        (Some(pal.border), "空闲", free_text),
        (None, "容量", capacity_text),
        (Some(pal.border_soft), "快照逻辑", logical_text),
    ];
    let mut y = rect.top() + 31.0;
    for (swatch, label, value) in legend_rows {
        if let Some(color) = swatch {
            painter.rect_filled(
                Rect::from_center_size(pos2(legend_x + 4.0, y + 6.0), Vec2::splat(8.0)),
                CornerRadius::same(2),
                color,
            );
        }
        painter.text(
            pos2(legend_x + 12.0, y),
            Align2::LEFT_TOP,
            label,
            FontId::proportional(10.5),
            pal.text_dim,
        );
        painter.text(
            pos2(legend_right, y),
            Align2::RIGHT_TOP,
            fit_chars(&value, 10.5, (legend_right - legend_x - 54.0).max(40.0)),
            FontId::proportional(10.5),
            pal.text,
        );
        y += 19.0;
    }

    // 脚注：只在需要解释时出现，不标红。
    let footnote = if volume.logical_over_capacity() {
        "快照逻辑大于卷容量：含硬链接重复计数与系统文件，非真实占用"
    } else if volume.needs_explanation() {
        "快照逻辑含硬链接与系统文件，比实际占用多"
    } else if !volume.has_snapshot {
        "快照未收录该卷，仅显示实时占用"
    } else {
        ""
    };
    if !footnote.is_empty() {
        painter.text(
            pos2(rect.left() + pad, rect.top() + 104.0),
            Align2::LEFT_TOP,
            fit_chars(footnote, 9.5, rect.width() - pad * 2.0),
            FontId::proportional(9.5),
            pal.text_faint,
        );
    }

    // 底部：文件数 + 实时空闲（与图例口径一致，便于和「容量 − 空闲」对照）。
    let mut bottom_left = if volume.has_snapshot {
        format!("{} 文件", db::thousands(volume.file_count.max(0) as u64))
    } else {
        "快照未收录".to_owned()
    };
    if let Some(free) = volume.free {
        bottom_left.push_str(&format!(" · 实际空闲 {}", db::human_size(free as i64)));
    }
    painter.text(
        pos2(rect.left() + pad, rect.bottom() - pad),
        Align2::LEFT_BOTTOM,
        fit_chars(&bottom_left, 10.0, rect.width() - pad * 2.0 - 92.0),
        FontId::proportional(10.0),
        pal.text_faint,
    );
    painter.text(
        pos2(rect.right() - pad, rect.bottom() - pad),
        Align2::RIGHT_BOTTOM,
        if interactive {
            "点击进入树图 ›"
        } else {
            "无快照数据"
        },
        FontId::proportional(10.0),
        if hovered && interactive {
            pal.accent
        } else {
            pal.text_faint
        },
    );

    let response = response.on_hover_ui(|ui| tooltip_volume(ui, pal, volume));
    interactive && response.clicked()
}

// ---------- 绘制：顶部汇总条 ----------

/// 顶部汇总条：标题 / 物理盘统计 / 实际口径一行 / 快照口径一行。
fn paint_summary(ui: &Ui, pal: &Palette, rect: Rect, snap: &db::Snapshot, totals: &Totals) {
    let painter = ui.painter();
    painter.text(
        pos2(rect.left() + 2.0, rect.top() + 2.0),
        Align2::LEFT_TOP,
        "物理磁盘 / 卷",
        FontId::proportional(16.0),
        pal.text,
    );
    let mut right = format!(
        "快照 {} · {} 块物理盘 · {} 个卷",
        snap.name, totals.disks, totals.volumes
    );
    if totals.reserved > 0 {
        right.push_str(&format!("（含 {} 个系统保留）", totals.reserved));
    }
    painter.text(
        pos2(rect.right() - 2.0, rect.top() + 6.0),
        Align2::RIGHT_TOP,
        fit_chars(&right, 11.0, rect.width() * 0.62),
        FontId::proportional(11.0),
        pal.text_faint,
    );

    let actual_line = match (totals.capacity, totals.actual_share()) {
        (0, _) => "实际已用未知 / 已识别容量未知（Win32 查询失败）".to_owned(),
        (capacity, Some(share)) => format!(
            "实际已用 {} / 已识别容量 {} · 整体 {:.0}%",
            db::human_size(totals.actual_used as i64),
            db::human_size(capacity as i64),
            share * 100.0
        ),
        (capacity, None) => format!("已识别容量 {}", db::human_size(capacity as i64)),
    };
    painter.text(
        pos2(rect.left() + 2.0, rect.top() + 24.0),
        Align2::LEFT_TOP,
        fit_chars(&actual_line, 12.5, rect.width() - 8.0),
        FontId::proportional(12.5),
        pal.text_dim,
    );

    let logical_line = if totals.logical > 0 {
        let files = db::thousands(totals.files.max(0) as u64);
        match totals.logical_excess() {
            Some(diff) if diff > 0 => format!(
                "快照逻辑 {} · 索引文件大小之和（含硬链接与系统文件），比实际多 {} · {} 个文件",
                db::human_size(totals.logical),
                db::human_size(diff),
                files
            ),
            Some(diff) if diff < 0 => format!(
                "快照逻辑 {} · 索引文件大小之和，比实际少 {} · {} 个文件",
                db::human_size(totals.logical),
                db::human_size(diff.saturating_abs()),
                files
            ),
            _ => format!(
                "快照逻辑 {} · {} 个文件",
                db::human_size(totals.logical),
                files
            ),
        }
    } else {
        "快照逻辑：此快照没有目录聚合数据".to_owned()
    };
    painter.text(
        pos2(rect.left() + 2.0, rect.top() + 40.0),
        Align2::LEFT_TOP,
        fit_chars(&logical_line, 10.5, rect.width() - 8.0),
        FontId::proportional(10.5),
        pal.text_faint,
    );
}

// ---------- 绘制：物理磁盘行 ----------

/// 行内卡片网格：(列数, 卡片宽, 行数)。窄窗口自动换行，绝不横向溢出。
fn grid(width: f32, count: usize) -> (usize, f32, usize) {
    if count == 0 {
        return (1, 0.0, 0);
    }
    let cols = (((width + GRID_GAP) / (CARD_MIN_W + GRID_GAP)).floor() as usize)
        .max(1)
        .min(count);
    let each = (width - GRID_GAP * (cols - 1) as f32) / cols as f32;
    let card_w = each.clamp(160.0, MAX_CARD_W).min(width.max(1.0));
    (cols, card_w, count.div_ceil(cols))
}

fn row_height(row: &DiskRow, width: f32, expanded: bool) -> f32 {
    let (_, _, lines) = grid(width, row.volumes.len());
    let mut height = DISK_TITLE_H;
    if lines > 0 {
        height += lines as f32 * CARD_H + (lines - 1) as f32 * GRID_GAP;
    }
    if !row.reserved.is_empty() {
        height += 4.0 + RESERVED_H;
        if expanded {
            height += row.reserved.len() as f32 * RESERVED_ROW_H;
        }
    }
    height
}

/// 一块物理盘一行：行标题 + 该盘卷卡片（自动换行）+ 系统保留折叠区。
fn paint_disk_row(
    ui: &mut Ui,
    pal: &Palette,
    rect: Rect,
    row: &DiskRow,
    index: usize,
    expanded: bool,
    total_logical: i64,
) -> (Option<String>, bool) {
    ui.painter().text(
        pos2(rect.left() + 2.0, rect.top() + 2.0),
        Align2::LEFT_TOP,
        fit_chars(&row.title, 13.0, (rect.width() - 280.0).max(140.0)),
        FontId::proportional(13.0),
        pal.text,
    );
    if !row.note.is_empty() {
        ui.painter().text(
            pos2(rect.right() - 6.0, rect.top() + 4.0),
            Align2::RIGHT_TOP,
            fit_chars(&row.note, 11.0, rect.width() * 0.55),
            FontId::proportional(11.0),
            pal.text_faint,
        );
    }

    let (cols, card_w, lines) = grid(rect.width(), row.volumes.len());
    let cards_top = rect.top() + DISK_TITLE_H;
    let mut clicked: Option<String> = None;
    for (slot, volume) in row.volumes.iter().enumerate() {
        let column = slot % cols;
        let line = slot / cols;
        let card = Rect::from_min_size(
            pos2(
                rect.left() + column as f32 * (card_w + GRID_GAP),
                cards_top + line as f32 * (CARD_H + GRID_GAP),
            ),
            vec2(card_w, CARD_H),
        );
        if volume_card(ui, pal, card, volume, index * 100 + slot, total_logical) {
            clicked = Some(volume.path.clone());
        }
    }

    let mut toggle = false;
    if !row.reserved.is_empty() {
        let y = cards_top
            + lines as f32 * CARD_H
            + lines.saturating_sub(1) as f32 * GRID_GAP
            + 4.0;
        let line = Rect::from_min_size(pos2(rect.left(), y), vec2(rect.width(), RESERVED_H));
        let response = ui.interact(
            line,
            ui.scope_id().with(("reserved", row.key.as_str())),
            Sense::click(),
        );
        if response.hovered() {
            ui.painter()
                .rect_filled(line, CornerRadius::same(theme::RADIUS_SM), pal.hover);
        }
        icons::paint_glyph(
            ui.painter(),
            Rect::from_center_size(pos2(line.left() + 10.0, line.center().y), Vec2::splat(14.0)),
            if expanded {
                Glyph::ChevronDown
            } else {
                Glyph::ChevronRight
            },
            pal.text_dim,
        );
        let summary = reserved_summary(row);
        ui.painter().text(
            pos2(line.left() + 22.0, line.center().y),
            Align2::LEFT_CENTER,
            fit_chars(&summary, 11.0, (rect.width() - 150.0).max(120.0)),
            FontId::proportional(11.0),
            pal.text_dim,
        );
        ui.painter().text(
            pos2(line.right() - 4.0, line.center().y),
            Align2::RIGHT_CENTER,
            if expanded { "点击收起" } else { "点击展开" },
            FontId::proportional(10.5),
            pal.text_faint,
        );
        if response.clicked() {
            toggle = true;
        }
        if expanded {
            let mut row_top = y + RESERVED_H;
            for (slot, volume) in row.reserved.iter().enumerate() {
                let row_rect = Rect::from_min_size(
                    pos2(rect.left() + 8.0, row_top),
                    vec2(rect.width() - 8.0, RESERVED_ROW_H),
                );
                if reserved_row(ui, pal, row_rect, volume, index * 1000 + slot) {
                    clicked = Some(volume.path.clone());
                }
                row_top += RESERVED_ROW_H;
            }
        }
    }
    (clicked, toggle)
}

fn reserved_summary(row: &DiskRow) -> String {
    let total: u64 = row
        .reserved
        .iter()
        .filter_map(|volume| volume.capacity)
        .sum();
    let files: i64 = row
        .reserved
        .iter()
        .map(|volume| volume.file_count.max(0))
        .sum();
    let mut text = format!("系统保留（{} 个）", row.reserved.len());
    if total > 0 {
        text.push_str(&format!(" · 合计 {}", db::human_size(total as i64)));
    }
    if files > 0 {
        text.push_str(&format!(" · {} 个文件", db::thousands(files as u64)));
    }
    text
}

/// 「系统保留」展开后的紧凑行：无盘符分区一行一个，不占整张卡片的高度。
fn reserved_row(ui: &mut Ui, pal: &Palette, rect: Rect, volume: &VolumeInfo, id: usize) -> bool {
    let response = ui.interact(
        rect,
        ui.scope_id().with(("reserved-row", id)),
        Sense::click(),
    );
    let hovered = response.hovered();
    if hovered {
        ui.painter()
            .rect_filled(rect, CornerRadius::same(3), pal.hover);
    }
    let enabled = volume.has_snapshot;
    let painter = ui.painter();
    let name = if volume.label.is_empty() {
        volume.name.clone()
    } else {
        format!("{} · {}", volume.name, volume.label)
    };
    painter.text(
        pos2(rect.left() + 18.0, rect.center().y),
        Align2::LEFT_CENTER,
        fit_chars(&name, 11.0, 150.0),
        FontId::proportional(11.0),
        if enabled { pal.text_dim } else { pal.text_faint },
    );
    let mut detail = String::new();
    if !volume.file_system.is_empty() {
        detail.push_str(&volume.file_system);
        detail.push_str(" · ");
    }
    match volume.capacity {
        Some(capacity) => detail.push_str(&format!("容量 {}", db::human_size(capacity as i64))),
        None => detail.push_str("容量未知"),
    }
    if let Some(free) = volume.free {
        detail.push_str(&format!(" · 空闲 {}", db::human_size(free as i64)));
    }
    if volume.has_snapshot {
        detail.push_str(&format!(
            " · 快照逻辑 {}",
            db::human_size(volume.logical.max(0))
        ));
    }
    painter.text(
        pos2(rect.left() + 176.0, rect.center().y),
        Align2::LEFT_CENTER,
        fit_chars(&detail, 10.5, (rect.width() - 330.0).max(120.0)),
        FontId::proportional(10.5),
        pal.text_faint,
    );
    painter.text(
        pos2(rect.right() - 6.0, rect.center().y),
        Align2::RIGHT_CENTER,
        if enabled { "点击进入树图 ›" } else { "快照未收录" },
        FontId::proportional(10.5),
        if enabled && hovered {
            pal.accent
        } else {
            pal.text_faint
        },
    );
    let response = response.on_hover_ui(|ui| {
        ui.label(RichText::new(volume.path.clone()).size(11.0).color(pal.text));
        ui.label(
            RichText::new(format!(
                "{} · {}",
                if volume.file_system.is_empty() {
                    "文件系统未知"
                } else {
                    volume.file_system.as_str()
                },
                if enabled {
                    "无盘符系统分区 · 快照已收录，可进入树图"
                } else {
                    "无盘符系统分区 · 快照未收录"
                }
            ))
            .size(10.5)
            .color(pal.text_dim),
        );
    });
    if enabled && hovered {
        ui.ctx().set_cursor_icon(egui::CursorIcon::PointingHand);
    }
    enabled && response.clicked()
}

// ---------- 绘制：口径脚注 ----------

/// 全页脚注：常态说明口径；有卷的逻辑大小超过容量时，明确给出三条原因。
fn paint_caliber_footnote(ui: &mut Ui, pal: &Palette, width: f32, rows: &[DiskRow]) {
    let over = rows
        .iter()
        .any(|row| row.all_volumes().any(VolumeInfo::logical_over_capacity));
    let text = if over {
        "注：快照逻辑 = 索引到的文件大小之和，可能大于卷容量 —— 硬链接重复计数、NTFS 元数据文件被当普通文件计入、压缩/稀疏文件逻辑大小大于实际占用。环形图与百分比只按「容量 − 实时空闲」。"
    } else {
        "口径：环形图与百分比 = 实际已用（容量 − 实时空闲）；快照逻辑 = 索引到的文件大小之和，含硬链接与系统文件，不等于卷占用。"
    };
    let (rect, response) = ui.allocate_exact_size(vec2(width, 18.0), Sense::hover());
    ui.painter().text(
        pos2(rect.left() + 2.0, rect.center().y),
        Align2::LEFT_CENTER,
        fit_chars(text, 10.0, rect.width() - 6.0),
        FontId::proportional(10.0),
        pal.text_faint,
    );
    response.on_hover_ui(|ui| {
        ui.label(
            RichText::new("快照逻辑与实际占用的差异从哪来")
                .size(11.5)
                .color(pal.text),
        );
        for line in [
            "① 硬链接重复计数：WinSxS / System32 里同一份数据被多个路径分别累加。",
            "② NTFS 元数据文件：$MFT 等系统文件被快照当成普通文件计入。",
            "③ 压缩 / 稀疏文件：逻辑大小大于实际占用（如浏览器权重、Android 镜像）。",
            "快照库不保存 Windows 属性位，无法还原硬链接与重解析点，所以不做修正。",
        ] {
            ui.label(RichText::new(line).size(10.5).color(pal.text_dim));
        }
        ui.label(
            RichText::new("实际已用始终 = 容量 − 实时空闲（GetDiskFreeSpaceExW）。")
                .size(10.5)
                .color(pal.accent),
        );
    });
}

// ---------- 绘制：卷 tooltip ----------

fn tooltip_volume(ui: &mut Ui, pal: &Palette, volume: &VolumeInfo) {
    let title = if volume.label.is_empty() {
        volume.name.clone()
    } else {
        format!("{} · {}", volume.name, volume.label)
    };
    ui.label(RichText::new(title).size(12.0).color(pal.text));
    ui.label(
        RichText::new(format!(
            "{} · {}",
            volume.path,
            if volume.file_system.is_empty() {
                "文件系统未知"
            } else {
                volume.file_system.as_str()
            }
        ))
        .size(10.5)
        .color(pal.text_faint),
    );
    ui.add_space(2.0);
    let actual = volume
        .actual_used()
        .map(|used| db::human_size(used as i64))
        .unwrap_or_else(|| "-".to_owned());
    let free = volume
        .free
        .map(|free| db::human_size(free as i64))
        .unwrap_or_else(|| "-".to_owned());
    let capacity = volume
        .capacity
        .map(|capacity| db::human_size(capacity as i64))
        .unwrap_or_else(|| "未知".to_owned());
    ui.label(
        RichText::new(format!("实际已用 {actual} · 实时空闲 {free} · 容量 {capacity}"))
            .size(11.0)
            .color(pal.text_dim),
    );
    if volume.has_snapshot {
        ui.label(
            RichText::new(format!(
                "快照逻辑 {} · {} 个文件",
                db::human_size(volume.logical.max(0)),
                db::thousands(volume.file_count.max(0) as u64)
            ))
            .size(11.0)
            .color(pal.text_dim),
        );
        if let Some(share) = volume.logical_share() {
            ui.label(
                RichText::new(format!("快照逻辑占容量 {:.0}%", share * 100.0))
                    .size(11.0)
                    .color(pal.text_faint),
            );
        }
        if let Some(excess) = volume.logical_excess().filter(|diff| *diff != 0) {
            ui.label(
                RichText::new(format!(
                    "逻辑比实际{} {}",
                    if excess > 0 { "多" } else { "少" },
                    db::human_size(excess.abs())
                ))
                .size(11.0)
                .color(pal.text_faint),
            );
        }
    }
    if volume.logical_over_capacity() || volume.needs_explanation() {
        ui.add_space(2.0);
        ui.label(RichText::new("差额来源").size(11.0).strong().color(pal.text));
        for line in [
            "① 硬链接重复计数：WinSxS / System32 同一份数据多个路径被分别累加。",
            "② NTFS 元数据文件：$MFT 等被快照当成普通文件计入。",
            "③ 压缩 / 稀疏文件：逻辑大小大于实际占用。",
            "快照库不保存 Windows 属性位，无法还原硬链接与重解析点。",
        ] {
            ui.label(RichText::new(line).size(10.5).color(pal.text_dim));
        }
    }
    if volume.has_snapshot {
        ui.label(
            RichText::new("点击进入树图")
                .size(10.5)
                .color(pal.accent),
        );
    }
}

// ---------- 绘制：树图辅助 ----------

fn paint_stripes(painter: &egui::Painter, rect: Rect, color: Color32) {
    let clip = painter.with_clip_rect(rect);
    let step = 12.0;
    let mut x = rect.left() - rect.height();
    while x < rect.right() {
        clip.line_segment(
            [
                pos2(x, rect.bottom()),
                pos2(x + rect.height(), rect.top()),
            ],
            Stroke::new(6.0, color),
        );
        x += step;
    }
}

fn dashed_rect(painter: &egui::Painter, rect: Rect, stroke: Stroke, dash: f32, gap: f32) {
    let corners = [
        rect.left_top(),
        rect.right_top(),
        rect.right_bottom(),
        rect.left_bottom(),
        rect.left_top(),
    ];
    for pair in corners.windows(2) {
        let (a, b) = (pair[0], pair[1]);
        let len = a.distance(b);
        if len <= 0.0 {
            continue;
        }
        let dir = (b - a) / len;
        let mut travelled = 0.0;
        while travelled < len {
            let end = (travelled + dash).min(len);
            painter.line_segment([a + dir * travelled, a + dir * end], stroke);
            travelled = end + gap;
        }
    }
}

fn tooltip(ui: &mut Ui, pal: &Palette, item: &Item, total: i64) {
    ui.label(
        RichText::new(if item.is_aggregate() {
            item.name.clone()
        } else {
            item.path.clone()
        })
        .size(11.5)
        .color(pal.text),
    );
    let detail = if item.is_aggregate() {
        "剩余项合计 · 点击展开（面积不变）".to_owned()
    } else if item.is_dir {
        format!("{} 文件", db::thousands(item.file_count.max(0) as u64))
    } else {
        icons::label(icons::classify(&item.path, false)).to_owned()
    };
    ui.label(
        RichText::new(format!(
            "{} · {} · 占当前目录 {}",
            db::human_size(item.size),
            detail,
            percent(item.size, total)
        ))
        .size(11.0)
        .color(pal.text_dim),
    );
}

// ---------- 绘制：层级列表 ----------

/// 扁平化后的列表行：(缩进层级, 父目录大小, 项)；占比按父目录算（规格 5.5）。
type Row<'a> = (usize, i64, &'a Item);

fn collect_rows<'a>(
    content: &'a DirContent,
    expanded: &HashSet<String>,
    cache: &'a HashMap<String, Arc<DirContent>>,
) -> Vec<Row<'a>> {
    let mut rows: Vec<Row<'a>> = Vec::new();
    push_rows(&mut rows, content, 0, expanded, cache);
    rows
}

fn push_rows<'a>(
    rows: &mut Vec<Row<'a>>,
    content: &'a DirContent,
    depth: usize,
    expanded: &HashSet<String>,
    cache: &'a HashMap<String, Arc<DirContent>>,
) {
    for item in content.items.iter() {
        rows.push((depth, content.self_size, item));
        if item.is_dir
            && !item.is_aggregate()
            && depth < MAX_LIST_DEPTH
            && expanded.contains(&item.path)
            && let Some(child) = cache.get(&item.path)
        {
            push_rows(rows, child, depth + 1, expanded, cache);
        }
    }
}

/// 单行的绘制状态（缩进、斑马纹序号、展开/选中的三态）。
#[derive(Clone, Copy)]
struct RowFx {
    index: usize,
    depth: usize,
    expanded: bool,
    selected: bool,
}

fn row(ui: &mut Ui, pal: &Palette, item: &Item, total: i64, fx: RowFx) -> Option<Action> {
    let RowFx {
        index,
        depth,
        expanded,
        selected,
    } = fx;
    let (rect, response) = ui.allocate_exact_size(
        vec2(ui.available_width(), theme::ROW_H),
        Sense::click(),
    );
    let hovered = response.hovered();
    let painter = ui.painter();
    let fill = pal.row_fill(index, hovered, selected);
    if fill != Color32::TRANSPARENT {
        painter.rect_filled(rect, CornerRadius::ZERO, fill);
    }
    if selected {
        painter.rect_stroke(
            rect,
            CornerRadius::ZERO,
            Stroke::new(1.0, pal.accent),
            StrokeKind::Inside,
        );
    }

    let mut action: Option<Action> = None;
    let mut x = rect.left() + 4.0 + depth as f32 * 14.0;
    if item.is_dir && !item.is_aggregate() {
        let twisty = Rect::from_center_size(pos2(x + 7.0, rect.center().y), Vec2::splat(14.0));
        let twist_response = ui.interact(
            twisty,
            ui.scope_id().with(("twisty", item.path.as_str())),
            Sense::click(),
        );
        if twist_response.hovered() {
            painter.circle_filled(twisty.center(), 7.0, pal.hover);
        }
        icons::paint_glyph(
            painter,
            twisty.shrink(2.0),
            if expanded {
                Glyph::ChevronDown
            } else {
                Glyph::ChevronRight
            },
            pal.text_dim,
        );
        if twist_response.clicked() {
            action = Some(Action::Toggle(item.path.clone()));
        }
    } else {
        painter.circle_filled(pos2(x + 7.0, rect.center().y), 1.5, pal.text_faint);
    }
    x += 16.0;

    let icon_rect = Rect::from_center_size(pos2(x + 8.0, rect.center().y), Vec2::splat(16.0));
    if item.is_aggregate() {
        icons::paint_glyph(painter, icon_rect, Glyph::Expand, pal.text_dim);
    } else {
        icons::paint_file(painter, icon_rect, icons::classify(&item.path, item.is_dir));
    }
    x += 21.0;

    let right = rect.right() - 8.0;
    let mtime_w = 92.0;
    let pct_w = 46.0;
    let size_w = 78.0;
    let mtime_x = right - mtime_w;
    let pct_x = mtime_x - 8.0 - pct_w;
    let size_x = pct_x - 8.0 - size_w;
    let name_max = (size_x - x - 8.0).max(24.0);

    let color = if item.is_aggregate() {
        pal.text_dim
    } else {
        pal.text
    };
    let name = fit_chars(&item.name, 12.0, name_max);
    let galley = painter.layout_no_wrap(name, FontId::proportional(12.0), color);
    painter.galley(
        pos2(x, rect.center().y - galley.size().y * 0.5),
        galley.clone(),
        color,
    );
    let badge_x = x + galley.size().x + 6.0;
    if !item.is_aggregate() && item.file_count > 0 && badge_x + 46.0 < size_x {
        let text = compact_count(item.file_count);
        let badge = painter.layout_no_wrap(text, FontId::proportional(10.0), pal.text_dim);
        let box_rect = Rect::from_min_size(
            pos2(badge_x, rect.center().y - 7.0),
            vec2(badge.size().x + 10.0, 14.0),
        );
        painter.rect_filled(box_rect, CornerRadius::same(7), pal.panel_alt);
        painter.galley(
            pos2(box_rect.left() + 5.0, box_rect.center().y - badge.size().y * 0.5),
            badge,
            pal.text_dim,
        );
    }

    painter.text(
        pos2(size_x + size_w, rect.center().y),
        Align2::RIGHT_CENTER,
        db::human_size(item.size),
        FontId::proportional(11.5),
        pal.text_dim,
    );
    painter.text(
        pos2(pct_x + pct_w, rect.center().y),
        Align2::RIGHT_CENTER,
        percent(item.size, total),
        FontId::proportional(11.0),
        pal.text_faint,
    );
    painter.text(
        pos2(mtime_x + mtime_w, rect.center().y),
        Align2::RIGHT_CENTER,
        if item.mtime > 0 {
            db::format_unix(item.mtime)
        } else {
            "-".to_owned()
        },
        FontId::monospace(10.5),
        pal.text_faint,
    );

    if action.is_none() {
        if response.double_clicked() {
            action = Some(if item.is_dir {
                Action::Drill(item.path.clone())
            } else {
                Action::Open(item.path.clone())
            });
        } else if response.clicked() {
            action = Some(if item.is_aggregate() {
                Action::ExpandAgg(aggregate_parent(&item.path))
            } else if item.is_dir {
                Action::Toggle(item.path.clone())
            } else {
                Action::Select(item.path.clone())
            });
        }
    }
    if hovered {
        let _ = response.on_hover_text(if item.is_aggregate() {
            "点击展开「其他 N 项」（面积不变）"
        } else {
            item.path.as_str()
        });
    } else {
        let _ = response;
    }
    action
}

/// 聚合块的 path 形如「目录 + #其他」，展开动作要作用于所属目录。
fn aggregate_parent(path: &str) -> String {
    path.split("#其他").next().unwrap_or(path).to_owned()
}

// ---------- 图例 ----------

fn paint_legend(ui: &mut Ui, pal: &Palette, rect: Rect) {
    let painter = ui.painter();
    let y = rect.center().y;
    let mut x = rect.left() + 2.0;
    for (name, color) in colors::LEGEND {
        painter.rect_filled(
            Rect::from_center_size(pos2(x + 5.0, y), Vec2::splat(10.0)),
            CornerRadius::same(2),
            color,
        );
        x += 14.0;
        let galley = painter.layout_no_wrap(name.to_owned(), FontId::proportional(11.0), pal.text_dim);
        painter.galley(pos2(x, y - galley.size().y * 0.5), galley.clone(), pal.text_dim);
        x += galley.size().x + 12.0;
    }
    let hint = "面积 = 大小 · 颜色 = 目录类别 / 文件类型";
    let galley = painter.layout_no_wrap(hint.to_owned(), FontId::proportional(11.0), pal.text_faint);
    if x + galley.size().x + 12.0 < rect.right() {
        painter.galley(
            pos2(rect.right() - 2.0 - galley.size().x, y - galley.size().y * 0.5),
            galley,
            pal.text_faint,
        );
    }
}

// ---------- 小工具 ----------

/// 省略号截断：按字符估算宽度（ASCII 0.58 em、其余 1 em），只做一次排版。
fn fit_chars(text: &str, size: f32, max_w: f32) -> String {
    if max_w <= size * 0.9 {
        return "…".to_owned();
    }
    let mut width = 0.0;
    let mut out = String::new();
    for ch in text.chars() {
        let advance = if ch.is_ascii() { size * 0.58 } else { size * 1.0 };
        if width + advance > max_w - size * 0.72 {
            out.push('…');
            break;
        }
        out.push(ch);
        width += advance;
    }
    if out.is_empty() {
        out.push('…');
    }
    out
}

fn percent(size: i64, total: i64) -> String {
    if total <= 0 {
        return "-".to_owned();
    }
    let value = size.max(0) as f64 / total as f64 * 100.0;
    if value >= 10.0 {
        format!("{value:.0}%")
    } else if value >= 0.1 {
        format!("{value:.1}%")
    } else if size > 0 {
        "<0.1%".to_owned()
    } else {
        "0%".to_owned()
    }
}

fn compact_count(value: i64) -> String {
    let value = value.max(0);
    if value >= 1_000_000 {
        format!("{:.1}M", value as f64 / 1_000_000.0)
    } else if value >= 1_000 {
        format!("{:.0}k", value as f64 / 1_000.0)
    } else {
        value.to_string()
    }
}

fn open_path(path: &str) -> Result<(), String> {
    std::process::Command::new("explorer")
        .arg(path)
        .spawn()
        .map(|_| ())
        .map_err(|error| format!("打开 {path} 失败：{error}"))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn snapshot() -> Option<db::Snapshot> {
        let root = std::env::var("DISKINTEL_DATA_ROOT")
            .unwrap_or_else(|_| String::from("D:\\disk-intel-data"));
        let path = std::path::Path::new(&root)
            .join("snapshots")
            .join("20261112-100008.sqlite");
        if !path.exists() {
            println!("跳过：{} 不存在", path.display());
            return None;
        }
        db::Snapshot::open_path(&path).ok()
    }

    // ---------- 二期：口径 / 磁盘分组 单测 ----------

    fn fake_volume(
        name: &str,
        drive: Option<&str>,
        disk: Option<u32>,
        capacity: Option<u64>,
        free: Option<u64>,
        logical: i64,
    ) -> VolumeInfo {
        VolumeInfo {
            path: format!("{name}\\"),
            name: name.to_owned(),
            drive: drive.map(str::to_owned),
            label: String::new(),
            file_system: "NTFS".to_owned(),
            logical,
            file_count: 100,
            capacity,
            free,
            disk,
            has_snapshot: logical > 0,
        }
    }

    fn drive_root(letter: &str) -> String {
        format!("{letter}:\\")
    }

    fn guid_volume(stub: &str) -> String {
        let mut out = String::from(r"\\?\Volume{");
        out.push_str(stub);
        out.push_str("}\\");
        out
    }

    fn topo_volume(guid: String, letter: Option<&str>, disk: u32) -> diskmap::VolumeTopo {
        diskmap::VolumeTopo {
            guid,
            mount_points: letter.map(|letter| vec![drive_root(letter)]).unwrap_or_default(),
            label: String::new(),
            file_system: "NTFS".to_owned(),
            disk: Some(disk),
            offset: 0,
            length: 0,
        }
    }

    fn folder_row(path: &str, size: i64) -> db::FolderRow {
        db::FolderRow {
            path: path.to_owned(),
            size,
            file_count: 10,
        }
    }

    /// 口径分离：实际已用 = 容量 − 实时空闲；快照逻辑不 clamp、不参与百分比。
    #[test]
    fn actual_usage_is_separate_from_snapshot_logical() {
        let capacity = 321_181_970_432u64;
        let free = 36_808_097_792u64;
        let logical = 344_410_443_068i64;
        let volume = VolumeInfo {
            path: drive_root("C"),
            name: "C:".to_owned(),
            drive: Some("C:".to_owned()),
            label: String::new(),
            file_system: "NTFS".to_owned(),
            logical,
            file_count: 1_891_728,
            capacity: Some(capacity),
            free: Some(free),
            disk: Some(0),
            has_snapshot: true,
        };
        let actual = volume.actual_used().expect("实际已用");
        assert_eq!(actual, capacity - free);
        let share = volume.actual_share().expect("实际占比");
        assert!((share - 0.8854).abs() < 0.002, "实际占比 {share}");
        assert!(share <= 1.0, "实际占比绝不能超过 100%");

        // 快照逻辑按原值给出，不 clamp；逻辑 > 容量只标记「需要解释」。
        let logical_share = volume.logical_share().expect("逻辑占比");
        assert!(
            logical_share > 1.07,
            "320.8 GB / 299.1 GB 应大于 107%，实际 {logical_share}"
        );
        assert!(volume.logical_over_capacity(), "逻辑 320.8 GB 应大于容量");
        assert_eq!(volume.logical_excess().unwrap(), logical - actual as i64);
        assert!(volume.needs_explanation());

        // 空闲为 0 时实际占比恰好 100%，也不因逻辑超大而「超出容量」。
        let full = VolumeInfo {
            free: Some(0),
            ..volume.clone()
        };
        assert_eq!(full.actual_share(), Some(1.0));
        assert!(full.logical_over_capacity());

        // 容量未知：不给百分比，界面退化为「容量未知」。
        let unknown = VolumeInfo {
            capacity: None,
            ..volume.clone()
        };
        assert_eq!(unknown.actual_used(), None);
        assert_eq!(unknown.actual_share(), None);
        assert_eq!(unknown.logical_excess(), None);

        // 快照没收录的卷（逻辑恒为 0）：不得误报「逻辑比实际多」，也不得加解释脚注。
        let missing = VolumeInfo {
            logical: 0,
            has_snapshot: false,
            ..volume.clone()
        };
        assert!(!missing.needs_explanation(), "无快照数据不应解释差额");
        assert!(!missing.logical_over_capacity());
        assert_eq!(missing.actual_used(), volume.actual_used());
    }

    /// 顶部汇总：容量 / 实际已用只信任 Win32 口径，逻辑大小单独累计。
    #[test]
    fn totals_use_actual_usage_not_logical() {
        let rows = vec![DiskRow {
            key: "disk-0".to_owned(),
            number: Some(0),
            title: String::new(),
            note: String::new(),
            volumes: vec![fake_volume("C:", Some("C:"), Some(0), Some(1000), Some(250), 1200)],
            reserved: vec![fake_volume("卷 9dba3ee0", None, Some(0), Some(100), Some(20), 80)],
        }];
        let totals = totals(&rows);
        assert_eq!(totals.disks, 1);
        assert_eq!(totals.volumes, 2);
        assert_eq!(totals.reserved, 1);
        assert_eq!(totals.capacity, 1100);
        assert_eq!(totals.actual_used, 750 + 80);
        assert_eq!(totals.logical, 1280);
        assert_eq!(totals.logical_excess(), Some(1280 - 830));
        let share = totals.actual_share().expect("整体占比");
        assert!((share - 830.0 / 1100.0).abs() < 1e-6);
    }

    /// 分组：三块盘各成一行；盘内按盘符升序；无盘符卷进系统保留。
    #[test]
    fn groups_volumes_by_physical_disk() {
        let roots = vec![
            folder_row(&drive_root("C"), 300),
            folder_row(&drive_root("D"), 200),
            folder_row(&drive_root("F"), 100),
            folder_row(&guid_volume("9dba3ee0-5ad5-4126-81f9-40064982c00e"), 50),
        ];
        let topo = diskmap::Topology {
            disks: vec![
                diskmap::DiskInfo {
                    number: 0,
                    vendor: String::new(),
                    product: "KINGSTON OM8PGP41024N-A0".to_owned(),
                    bus: 17,
                    size: 953_870_000_000,
                },
                diskmap::DiskInfo {
                    number: 2,
                    vendor: "Seagate".to_owned(),
                    product: "Expansion HDD".to_owned(),
                    bus: 7,
                    size: 931_500_000_000,
                },
            ],
            volumes: vec![
                topo_volume(guid_volume("c"), Some("C"), 0),
                topo_volume(guid_volume("d"), Some("D"), 0),
                topo_volume(guid_volume("e"), Some("E"), 0),
                topo_volume(guid_volume("f"), Some("F"), 2),
                topo_volume(
                    guid_volume("9dba3ee0-5ad5-4126-81f9-40064982c00e"),
                    None,
                    0,
                ),
            ],
            note: None,
        };
        let rows = build_rows(&roots, &topo);
        assert_eq!(rows.len(), 2, "只有两块盘有卷，不应凭空多行");

        // 磁盘 0：C/D/E 按盘符升序；E 未收录快照；9dba 进系统保留。
        assert_eq!(rows[0].number, Some(0));
        let drives: Vec<String> = rows[0]
            .volumes
            .iter()
            .filter_map(|volume| volume.drive.clone())
            .collect();
        assert_eq!(drives, vec!["C:", "D:", "E:"], "盘内必须按盘符升序");
        assert!(rows[0].volumes[0].has_snapshot);
        assert!(!rows[0].volumes[2].has_snapshot, "E 盘没被快照收录");
        assert_eq!(rows[0].reserved.len(), 1);
        assert_eq!(rows[0].reserved[0].name, "卷 9dba3ee0");
        assert!(rows[0].reserved[0].has_snapshot);
        assert!(rows[0].title.contains("KINGSTON"), "标题应有型号");
        assert!(rows[0].title.contains("NVMe"), "标题应有总线");

        // 磁盘 2：只有 F。
        assert_eq!(rows[1].number, Some(2));
        assert_eq!(rows[1].volumes.len(), 1);
        assert_eq!(rows[1].volumes[0].drive.as_deref(), Some("F:"));
        assert!(rows[1].title.contains("Seagate Expansion HDD"));
        assert!(rows[1].title.contains("USB"));
    }

    /// 降级：拿不到物理盘拓扑时，快照里的卷必须按盘符单列，绝不空白。
    #[test]
    fn topology_failure_falls_back_to_drive_list() {
        let roots = vec![
            folder_row(&drive_root("C"), 10),
            folder_row(&drive_root("D"), 20),
        ];
        let rows = build_rows(&roots, &diskmap::Topology::default());
        assert_eq!(rows.len(), 1, "拓扑失败时必须仍然有一行可画");
        assert_eq!(rows[0].number, None);
        assert!(rows[0].title.contains("未识别物理盘"));
        let drives: Vec<String> = rows[0]
            .volumes
            .iter()
            .filter_map(|volume| volume.drive.clone())
            .collect();
        assert_eq!(drives, vec!["C:", "D:"], "降级后按盘符单列");
        assert!(rows[0].reserved.is_empty());
        assert!(rows[0].volumes.iter().all(|volume| volume.has_snapshot));
    }

    /// 降级：个别快照根映射不到物理盘时，进降级行而不是被丢掉。
    #[test]
    fn unmapped_root_goes_to_fallback_row() {
        let roots = vec![
            folder_row(&drive_root("C"), 10),
            folder_row(&drive_root("X"), 20),
        ];
        let topo = diskmap::Topology {
            disks: vec![diskmap::DiskInfo {
                number: 0,
                vendor: String::new(),
                product: "NVMe Disk".to_owned(),
                bus: 17,
                size: 1000,
            }],
            volumes: vec![topo_volume(guid_volume("c"), Some("C"), 0)],
            note: Some("模拟部分映射失败".to_owned()),
        };
        let rows = build_rows(&roots, &topo);
        assert_eq!(rows.len(), 2);
        assert_eq!(rows[0].number, Some(0));
        assert_eq!(rows[1].number, None, "X: 应进降级行");
        assert_eq!(rows[1].volumes.len(), 1);
        assert_eq!(rows[1].volumes[0].drive.as_deref(), Some("X:"));
    }

    /// 窄窗口换行：任何宽度下卡片网格都不得横向溢出。
    #[test]
    fn card_grid_never_overflows() {
        for width in [560.0f32, 700.0, 884.0, 1188.0, 1428.0] {
            for count in 1..=5usize {
                let (cols, card_w, lines) = grid(width, count);
                assert!(cols >= 1 && cols <= count);
                let used = card_w * cols as f32 + GRID_GAP * (cols - 1) as f32;
                assert!(used <= width + 0.01, "宽 {width} / {count} 卷溢出：{used}");
                let expected_lines = count.div_ceil(cols);
                assert_eq!(lines, expected_lines);
            }
        }
    }

    /// 探测计数是进程级全局量：两个会触发 probe() 的测试必须互斥，否则并行执行会互相干扰。
    static PROBE_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

    /// 启动路径不得探测物理拓扑：SpaceState::new 必须是纯内存初始化。
    #[test]
    fn space_state_new_does_not_probe_topology() {
        let Some(snap) = snapshot() else { return };
        let _guard = PROBE_LOCK
            .lock()
            .unwrap_or_else(|poison| poison.into_inner());
        let before = diskmap::probe_count();
        let state = SpaceState::new(&snap);
        assert_eq!(
            before,
            diskmap::probe_count(),
            "SpaceState::new 不得探测物理磁盘拓扑（会拖慢首帧）"
        );
        assert!(state.rows.is_empty(), "启动时不应预构建磁盘行");
        assert!(
            matches!(&state.load, LoadState::Idle),
            "拓扑懒加载必须等到空间页第一次 show()"
        );
    }

    /// 真实环境：拓扑 + 快照必须合成不丢卷的分组。
    #[test]
    fn real_topology_keeps_every_snapshot_root() {
        let Some(snap) = snapshot() else { return };
        let _guard = PROBE_LOCK
            .lock()
            .unwrap_or_else(|poison| poison.into_inner());
        let roots = snap.roots_rows();
        let topology = diskmap::probe();
        let rows = build_rows(&roots, &topology);
        assert!(!rows.is_empty(), "至少要有可画的行");
        for root in &roots {
            let appeared = rows.iter().flat_map(|row| row.all_volumes()).any(|volume| {
                volume.path.eq_ignore_ascii_case(&root.path)
            });
            assert!(appeared, "快照根 {} 没出现在任何磁盘行里", root.path);
        }
        let mut seen: HashSet<String> = HashSet::new();
        for row in &rows {
            if let Some(number) = row.number {
                let drives: Vec<String> = row
                    .volumes
                    .iter()
                    .filter_map(|volume| volume.drive.clone())
                    .collect();
                let mut sorted = drives.clone();
                sorted.sort();
                assert_eq!(drives, sorted, "磁盘 {number} 内卷顺序必须按盘符升序");
            }
            for volume in row.all_volumes() {
                if let Some(drive) = &volume.drive {
                    assert!(seen.insert(drive.clone()), "盘符 {drive} 重复出现");
                }
            }
        }
        println!(
            "磁盘分组：{}",
            rows.iter()
                .map(|row| format!(
                    "{} → {} 卷 / {} 系统保留",
                    row.title,
                    row.volumes.len(),
                    row.reserved.len()
                ))
                .collect::<Vec<_>>()
                .join("；")
        );
    }

    /// 验收 b：树图权重和与目录 folder_sizes.size 一致，面积和等于容器。
    #[test]
    fn area_is_conserved_against_folder_sizes() {
        let Some(snap) = snapshot() else { return };
        for dir in [
            "D:\\",
            "C:\\",
            "C:\\Windows",
            "C:\\Windows\\WinSxS",
            "D:\\Softwareinstallation",
        ] {
            let content = load(&snap, dir, LOAD_BUDGET_MS, DIR_LIMIT, FILE_LIMIT);
            let weights: Vec<f64> = content
                .items
                .iter()
                .map(|item| layout::weight(item.size))
                .collect();
            let weight_sum: f64 = weights.iter().sum();
            let weight_err =
                (weight_sum - content.self_size as f64).abs() / content.self_size.max(1) as f64;
            assert!(weight_err < 0.005, "{dir} 权重和偏差 {weight_err}");

            let rect = Rect::from_min_size(pos2(0.0, 0.0), vec2(780.0, 700.0));
            let out = layout::layout(&weights, rect);
            let area_err = (out.area() - rect.width() * rect.height()).abs()
                / (rect.width() * rect.height());
            assert!(area_err < 0.005, "{dir} 面积不守恒 {area_err}");
            println!(
                "{dir}: {} 项，权重和 {weight_sum:.0} / 目录 {}，面积误差 {:.6}%",
                content.items.len(),
                content.self_size,
                area_err * 100.0
            );
        }
    }

    /// 验收 c 的状态层：下钻 / 返回 / 面包屑 / 检索定位。
    #[test]
    fn drill_and_back_navigation() {
        let Some(snap) = snapshot() else { return };
        let mut state = SpaceState::new(&snap);
        state.apply(Some(Action::Home), &snap);
        assert!(state.current.is_empty(), "总览应为空路径");

        state.apply(Some(Action::Drill(String::from("D:\\"))), &snap);
        assert_eq!(state.current, "D:\\");
        state.apply(
            Some(Action::Drill(String::from("D:\\Softwareinstallation"))),
            &snap,
        );
        assert_eq!(state.current, "D:\\Softwareinstallation");

        state.up();
        assert_eq!(state.current, "D:\\", "Backspace 应回到上级");
        state.up();
        assert!(state.current.is_empty(), "根再返回应回到所有卷总览");

        // 面包屑：直接跳到任意祖先目录
        state.apply(
            Some(Action::Drill(String::from("C:\\Windows\\System32\\drivers"))),
            &snap,
        );
        state.up();
        assert_eq!(state.current, "C:\\Windows\\System32");
        state.apply(Some(Action::Drill(String::from("C:\\Windows"))), &snap);
        assert_eq!(state.current, "C:\\Windows");

        // 检索视图「定位到空间」：文件应落到所在目录
        state.focus_on("D:\\Softwareinstallation\\README.txtx", &snap);
        assert_eq!(state.current, "D:\\Softwareinstallation");
    }

    /// 聚合块：数量与面积都必须精确（展开前后面积不变）。
    #[test]
    fn aggregate_block_is_exact() {
        let Some(snap) = snapshot() else { return };
        let content = load(&snap, "C:\\Windows\\WinSxS", LOAD_BUDGET_MS, DIR_LIMIT, FILE_LIMIT);
        let sum: i64 = content.items.iter().map(|item| item.size).sum();
        assert_eq!(sum, content.self_size, "各项 size 之和必须等于目录 size");
        if let Some(agg) = content.items.iter().find(|item| item.is_aggregate()) {
            let shown = content.items.len() as u64 - 1;
            let total = content.dir_total + content.file_total;
            assert_eq!(agg.agg_count + shown, total, "聚合块 N 必须精确");
            assert!(agg.size > 0);
        }

        // 展开（无预算全量）后：项数变多，但总面积与总权重不变
        let expanded = load_full(&snap, "C:\\Windows\\WinSxS");
        let sum_before = content.items.iter().map(|item| item.size).sum::<i64>();
        let sum_after = expanded.items.iter().map(|item| item.size).sum::<i64>();
        assert_eq!(sum_before, sum_after, "展开前后面积（size 之和）必须不变");
    }

    /// 层级列表展开：展开的子目录必须以其子项、更深的缩进出现在行列表里。
    #[test]
    fn hierarchy_expansion_flattens_children() {
        let Some(snap) = snapshot() else { return };
        let parent = load(&snap, "D:\\Softwareinstallation", LOAD_BUDGET_MS, DIR_LIMIT, FILE_LIMIT);
        let first_dir = parent
            .items
            .iter()
            .find(|item| item.is_dir && !item.is_aggregate())
            .cloned();
        let Some(first_dir) = first_dir else { return };
        let child = load(&snap, &first_dir.path, LOAD_BUDGET_MS, DIR_LIMIT, FILE_LIMIT);

        let mut cache: HashMap<String, Arc<DirContent>> = HashMap::new();
        cache.insert(first_dir.path.clone(), Arc::new(child.clone()));
        let expanded: HashSet<String> = HashSet::from([first_dir.path.clone()]);

        let collapsed = collect_rows(&parent, &HashSet::new(), &cache);
        assert!(collapsed.iter().all(|(depth, _, _)| *depth == 0));
        let expanded_rows = collect_rows(&parent, &expanded, &cache);
        assert!(expanded_rows.len() > collapsed.len(), "展开后行数应变多");
        let nested: Vec<_> = expanded_rows
            .iter()
            .filter(|(depth, _, _)| *depth == 1)
            .collect();
        assert_eq!(nested.len(), child.items.len(), "子目录的子项必须全部出现在 depth=1");
    }

    /// 性能：单目录载入耗时（报告用实测数字）。
    #[test]
    fn load_timing_report() {
        let Some(snap) = snapshot() else { return };
        for dir in [
            "D:\\",
            "C:\\",
            "D:\\Softwareinstallation",
            "C:\\Windows",
            "C:\\Windows\\WinSxS",
        ] {
            let first = load(&snap, dir, LOAD_BUDGET_MS, DIR_LIMIT, FILE_LIMIT);
            let times: Vec<f64> = (0..3)
                .map(|_| load(&snap, dir, LOAD_BUDGET_MS, DIR_LIMIT, FILE_LIMIT).load_ms)
                .collect();
            println!(
                "{dir}: 首载 {:.2} ms · 后三次 {:.2}/{:.2}/{:.2} ms · 扫描 {} 行 · {} 项{}",
                first.load_ms,
                times[0],
                times[1],
                times[2],
                first.scanned,
                first.items.len(),
                if first.complete { "" } else { "（含聚合块）" }
            );
        }
    }
}


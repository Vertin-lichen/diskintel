//! 检索视图：Everything 式表格 + 高级条件栏 + 原生虚拟滚动。
//!
//! 说明：Web 版每次重建 200 行 DOM、无虚拟化；原生版直接按可见区间查 SQLite，
//! 4.46M 行也只画屏幕上那几十行，这是「同一份数据、更少开销」的核心差异。
//!
//! 二期体验改进（T-20261113-09）：
//! · 列宽可拖拽：表头分隔条（命中区 ±4 px，悬停高亮），拖动实时重排，
//!   双击自适应内容，比例持久化到 %APPDATA%\DiskIntel\search-columns.json；
//! · 低价值结果折叠：lowvalue::classify 分类，默认**隐藏**低价值行（只显示非低价值行），
//!   底部固定一行折叠条报分类计数；点一下展开后恢复原位、淡色 + 左侧细线标记。
//!   折叠需要准确枚举显示行，所以只在「快照全量载入（命中 ≤ 20 万）」或
//!   「实时引擎已回传的那一页」上启用；快照分页窗口模式（命中 > 20 万）不折叠；
//!   白名单之外的「冷门」扩展名同样计入折叠与展开标记；
//! · 防抖与回车语义修复：输入停顿 260 ms 自动检索；回车只提交检索，
//!   只有先用 ↑/↓ 移动过选中行，回车才打开该行。

use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::Receiver;
use std::sync::Arc;
use std::time::{Duration, Instant};

use eframe::egui::{
    self, Align2, Color32, CornerRadius, CursorIcon, FontId, Galley, Rect, RichText, Sense, Stroke, Ui, Vec2,
};

use crate::db;
use crate::engine::{self, EngineEvent, EngineRow, QuerySpec, SortDir as GozDir, SortKey as GozSort};
use crate::filter;
use crate::icons;
use crate::theme::{self, Palette};

use crate::lowvalue::{classify, Bucket};

/// 一次查询在内存里保留的行数（滚动时按需换页）。
const WINDOW: u64 = 600;
/// v4 冷存储一次取的行数：路径现拼的排序很贵，一次拿够再在内存里翻页。
const V4_CHUNK: u64 = 2_000;
const DEBOUNCE_MS: u64 = 260;

// ---------- 列宽布局 ----------
/// 序号列固定宽度（不参与拖拽）。
const INDEX_W: f32 = 40.0;
/// 列间距：分隔条画在间距正中，命中区 ±4 px 永远压在间距里，不和列头点击打架。
const COL_GAP: f32 = 10.0;
const DIVIDER_GRAB: f32 = 4.0;
/// 宽度 < 1000 逻辑像素时隐藏「类型」列。
const NARROW_HIDE_TYPE: f32 = 1000.0;
/// 宽度 < 860 时把「大小 / 修改时间」压到最小宽度，剩余空间全给路径。
const NARROW_COMPRESS: f32 = 860.0;
const NAME_MIN: f32 = 120.0;
const PATH_MIN: f32 = 120.0;
const SIZE_MIN: f32 = 60.0;
const MTIME_MIN: f32 = 90.0;
const TYPE_MIN: f32 = 50.0;
/// 低价值折叠条高度。
const FOLD_BAR_H: f32 = 24.0;
/// 列宽持久化文件名（%APPDATA%\DiskIntel\search-columns.json）。
const COLUMN_FILE: &str = "search-columns.json";
/// 命中数 ≤ 此值时后台把全部结果载入内存：折叠顺序与分类计数都覆盖全量，
/// 滚动不再换页（首屏先到，其余一次性补齐）。超过则退回「按窗口」模式。
const LOAD_ALL_CAP: u64 = 200_000;

#[derive(Clone, Copy, PartialEq, Eq)]
pub enum KindChoice {
    All,
    File,
    Dir,
}

impl KindChoice {
    fn label(self) -> &'static str {
        match self {
            KindChoice::All => "全部",
            KindChoice::File => "文件",
            KindChoice::Dir => "目录",
        }
    }
}

/// 检索来源：自动（有实时引擎就用）/ 实时索引 / 快照。
#[derive(Clone, Copy, PartialEq, Eq)]
pub enum SourceMode {
    Auto,
    Live,
    Snapshot,
}

impl SourceMode {
    fn short(self) -> &'static str {
        match self {
            SourceMode::Auto => "自动",
            SourceMode::Live => "实时",
            SourceMode::Snapshot => "快照",
        }
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
pub enum SortChoice {
    Path,
    Size,
    Newest,
    Oldest,
}

impl SortChoice {
    fn label(self) -> &'static str {
        match self {
            SortChoice::Path => "路径（默认，走索引）",
            SortChoice::Size => "大小（全表扫描）",
            SortChoice::Newest => "最新修改",
            SortChoice::Oldest => "最早修改",
        }
    }
    fn short_label(self) -> &'static str {
        match self {
            SortChoice::Path => "路径",
            SortChoice::Size => "大小",
            SortChoice::Newest => "最新",
            SortChoice::Oldest => "最早",
        }
    }
    fn sort_by(self) -> db::SortBy {
        match self {
            SortChoice::Path => db::SortBy::PathAsc,
            SortChoice::Size => db::SortBy::SizeDesc,
            SortChoice::Newest => db::SortBy::MtimeDesc,
            SortChoice::Oldest => db::SortBy::MtimeAsc,
        }
    }
}

/// 一次检索的完整输入指纹：相同指纹的重复 refresh（启动阶段引擎状态变化、
/// 首帧 dirty 兜底）直接跳过，避免对同一份结果重复全量载入。
#[derive(Clone, PartialEq)]
struct QueryKey {
    query: String,
    scope: String,
    kind: KindChoice,
    sort: SortChoice,
    source: SourceMode,
    engine_ready: bool,
    engine_label: String,
    snapshot: String,
}

/// 后台线程回传的一批查询结果。
///
/// 命中数 ≤ LOAD_ALL_CAP（且结果确实由快照呈现）时：先回传首屏一页，再把其余行一次性回传
/// （offset 递增、done 收尾），折叠顺序与计数因此覆盖全量；
/// 否则只回传首屏一页（done = true，退回按窗口折叠）。
struct QueryReply {
    seq: u64,
    total: u64,
    offset: u64,
    rows: Vec<db::Row>,
    sql_ms: f64,
    count_ms: f64,
    error: Option<String>,
    done: bool,
}

#[derive(Clone, Debug)]
pub enum SearchAction {
    Open(String),
    CopyPath(String),
    /// 外壳统一弹 toast（app.rs 消费）；视图侧目前没有直接产生它。
    #[allow(dead_code)]
    Toast(String),
    Focus(String),
}

// ---------- 列宽（比例，持久化） ----------

/// 各列占「活动宽度 flex」的比例；路径列永远吃剩余空间，所以不存路径比例。
#[derive(Clone, Copy, Debug, PartialEq)]
struct ColumnWidths {
    name: f32,
    size: f32,
    mtime: f32,
    type_: f32,
}

impl Default for ColumnWidths {
    fn default() -> Self {
        // 与旧版硬编码（名称/路径 ≈ 42% / 58%，大小 92、时间 126、类型 86）等价的默认比例。
        Self {
            name: 0.300,
            size: 0.085,
            mtime: 0.115,
            type_: 0.085,
        }
    }
}

impl ColumnWidths {
    /// 把用户拖出来的比例收进合法区间：每列 3%–85%，总和不超过 85%（给路径留 15%）。
    fn sanitized(self) -> Self {
        let clamp = |value: f32| {
            if value.is_finite() && value > 0.0 {
                value.clamp(0.03, 0.85)
            } else {
                0.0
            }
        };
        let mut widths = Self {
            name: clamp(self.name),
            size: clamp(self.size),
            mtime: clamp(self.mtime),
            type_: clamp(self.type_),
        };
        let sum = widths.name + widths.size + widths.mtime + widths.type_;
        if sum > 0.85 {
            let scale = 0.85 / sum;
            widths.name *= scale;
            widths.size *= scale;
            widths.mtime *= scale;
            widths.type_ *= scale;
        }
        widths
    }

    fn is_valid(self) -> bool {
        self.name > 0.0 && self.size > 0.0 && self.mtime > 0.0 && self.type_ > 0.0
    }

    fn to_json(self) -> String {
        format!(
            "{{\"version\":1,\"name\":{:.4},\"size\":{:.4},\"mtime\":{:.4},\"type\":{:.4}}}\n",
            self.name, self.size, self.mtime, self.type_
        )
    }

    fn from_json(text: &str) -> Option<Self> {
        Some(Self {
            name: json_number(text, "name")?,
            size: json_number(text, "size")?,
            mtime: json_number(text, "mtime")?,
            type_: json_number(text, "type")?,
        })
    }
}

/// 从极简 JSON 里取一个数字字段（不引 serde，够用且零依赖）。
fn json_number(text: &str, key: &str) -> Option<f32> {
    let needle = format!("\"{key}\"");
    let start = text.find(&needle)? + needle.len();
    let rest = &text[start..];
    let colon = rest.find(':')?;
    let after = rest[colon + 1..].trim_start();
    let end = after
        .find([',', '}', '\n'])
        .unwrap_or(after.len());
    after[..end].trim().parse().ok()
}

fn columns_config_path() -> Option<PathBuf> {
    let root = std::env::var_os("APPDATA").or_else(|| std::env::var_os("HOME"))?;
    Some(PathBuf::from(root).join("DiskIntel").join(COLUMN_FILE))
}

fn load_column_widths() -> ColumnWidths {
    match columns_config_path() {
        Some(path) => load_widths_file(&path),
        None => ColumnWidths::default(),
    }
}

fn load_widths_file(path: &std::path::Path) -> ColumnWidths {
    let Ok(text) = std::fs::read_to_string(path) else {
        return ColumnWidths::default();
    };
    let widths = ColumnWidths::from_json(&text)
        .map(ColumnWidths::sanitized)
        .unwrap_or_default();
    if widths.is_valid() {
        widths
    } else {
        ColumnWidths::default()
    }
}

fn save_column_widths(widths: ColumnWidths) {
    let Some(path) = columns_config_path() else {
        return;
    };
    if let Some(dir) = path.parent() {
        let _ = std::fs::create_dir_all(dir);
    }
    if let Err(error) = save_widths_file(&path, widths) {
        eprintln!("列宽保存失败：{error}");
    }
}

fn save_widths_file(path: &std::path::Path, widths: ColumnWidths) -> std::io::Result<()> {
    std::fs::write(path, widths.sanitized().to_json())
}

// ---------- 低价值折叠 ----------

/// 折叠条分类的数量与展示顺序都直接取自 `Bucket::ALL`：以后在 lowvalue 里
/// 新增桶，这里不用再维护一份硬编码列表（旧实现漏掉「冷门」就是列表不同步）。
const BUCKET_COUNT: usize = Bucket::ALL.len();

/// 当前已加载窗口内的低价值分类计数（口径只影响展示，不影响命中总数）。
#[derive(Clone, Copy, Default)]
struct FoldCounts {
    total: u32,
    counts: [u32; BUCKET_COUNT],
}

impl FoldCounts {
    fn add(&mut self, bucket: &Bucket) {
        self.total += 1;
        if let Some(index) = Bucket::ALL.iter().position(|known| known == bucket) {
            self.counts[index] += 1;
        }
    }

    fn summary(&self) -> String {
        Bucket::ALL
            .iter()
            .zip(self.counts.iter())
            .filter(|(_, count)| **count > 0)
            .map(|(bucket, count)| format!("{} {count}", bucket.label()))
            .collect::<Vec<String>>()
            .join(" · ")
    }
}

/// 显示序：折叠态**只保留非低价值行**（低价值行不参与显示），展开态保持原顺序。
///
/// 返回值 order 的长度就是显示行数：折叠态 = 非低价值行数，展开态 = 全部行数；
/// order[d] 是第 d 个显示行对应的 rows 下标。
fn fold_order(buckets: &[Option<Bucket>], expanded: bool) -> Vec<u32> {
    let mut order: Vec<u32> = Vec::with_capacity(buckets.len());
    for (index, bucket) in buckets.iter().enumerate() {
        if expanded || bucket.is_none() {
            order.push(index as u32);
        }
    }
    order
}

/// 滚动范围 / 虚拟滚动用的显示行数。
///
/// 折叠可用时以折叠后的显示序长度为准：隐藏行不占滚动高度，也不会被 ensure 加载。
/// 只有「快照分页窗口模式」（命中数超过全量载入上限）枚举不出显示行，
/// 才退回按命中总数滚动（该模式不启用折叠，见 fold_available）。
fn display_len(fold_available: bool, total_hits: u64, visible_loaded: usize) -> usize {
    if fold_available {
        visible_loaded
    } else {
        total_hits as usize
    }
}

/// 折叠是否可用：全量载入、或实时引擎那一页已到位时可以准确枚举显示行；
/// 快照分页窗口模式（命中数超过全量载入上限）不行。
fn foldable(all_loaded: bool, live_active: bool) -> bool {
    all_loaded || live_active
}

/// 切换折叠 / 结果集变化后把选中行夹进显示范围，避免越界。
fn clamp_selected(selected: usize, visible: usize) -> usize {
    if visible == 0 {
        0
    } else {
        selected.min(visible - 1)
    }
}

// ---------- 表格布局 ----------

#[derive(Clone, Copy, PartialEq, Eq)]
enum ColKind {
    Index,
    Name,
    Path,
    Size,
    Mtime,
    Type,
}

struct Column {
    kind: ColKind,
    /// 相对表格左边缘的 x 偏移（表头与行共用，随窗口宽度移动）。
    x: f32,
    width: f32,
    numeric: bool,
    label: &'static str,
}

/// 一帧内表头与数据行共用的列布局。
struct TableLayout {
    width: f32,
    flex: f32,
    show_type: bool,
    compress: bool,
    name_px: f32,
    path_px: f32,
    size_px: f32,
    mtime_px: f32,
    type_px: f32,
    cols: Vec<Column>,
    /// (左侧列, 分隔条 x 偏移)；x 落在列间距正中。
    dividers: Vec<(ColKind, f32)>,
}

impl TableLayout {
    fn build(avail: f32, widths: &ColumnWidths) -> Self {
        let show_type = avail >= NARROW_HIDE_TYPE;
        let compress = avail < NARROW_COMPRESS;
        // 序号列恒在：带类型 6 列（5 个列间距），隐藏类型 5 列（4 个列间距）。
        let gaps = if show_type { 5.0 } else { 4.0 };
        let flex = (avail - INDEX_W - COL_GAP * gaps).max(40.0);

        let mut name = (widths.name * flex).max(NAME_MIN);
        let mut size = if compress {
            SIZE_MIN
        } else {
            (widths.size * flex).max(SIZE_MIN)
        };
        let mut mtime = if compress {
            MTIME_MIN
        } else {
            (widths.mtime * flex).max(MTIME_MIN)
        };
        let mut type_w = if show_type {
            (widths.type_ * flex).max(TYPE_MIN)
        } else {
            0.0
        };

        // 路径列吃掉剩余空间；不够最小宽度时从其它列按余量往里借。
        let mut path = flex - name - size - mtime - type_w;
        if path < PATH_MIN {
            let mut deficit = PATH_MIN - path;
            for (value, min) in [
                (&mut name, NAME_MIN),
                (&mut size, SIZE_MIN),
                (&mut mtime, MTIME_MIN),
            ] {
                let room = (*value - min).max(0.0).min(deficit);
                *value -= room;
                deficit -= room;
                if deficit <= 0.0 {
                    break;
                }
            }
            if deficit > 0.0 && show_type {
                let room = (type_w - TYPE_MIN).max(0.0).min(deficit);
                type_w -= room;
            }
            path = flex - name - size - mtime - type_w;
        }

        // 极端窄宽兜底：整体等比压缩，保证「永不溢出」。
        let sum = name + path + size + mtime + type_w;
        if sum > flex && sum > 0.0 {
            let scale = flex / sum;
            name *= scale;
            path *= scale;
            size *= scale;
            mtime *= scale;
            type_w *= scale;
        }
        path = path.max(0.0);

        let mut x = 0.0_f32;
        let mut cols: Vec<Column> = Vec::with_capacity(if show_type { 5 } else { 4 });
        let mut push = |kind: ColKind, width: f32, label: &'static str, numeric: bool| {
            cols.push(Column {
                kind,
                x,
                width,
                numeric,
                label,
            });
            x += width + COL_GAP;
        };
        push(ColKind::Index, INDEX_W, "#", true);
        push(ColKind::Name, name, "名称", false);
        push(ColKind::Path, path, "路径", false);
        push(ColKind::Size, size, "大小", true);
        push(ColKind::Mtime, mtime, "修改时间", true);
        if show_type {
            push(ColKind::Type, type_w, "类型", false);
        }

        let mut dividers = Vec::new();
        for (index, col) in cols.iter().enumerate() {
            // 序号列不拖；最后一列右侧没有分隔条。
            if index >= 1 && index + 1 < cols.len() {
                dividers.push((col.kind, col.x + col.width + COL_GAP * 0.5));
            }
        }

        Self {
            width: (x - COL_GAP).max(0.0),
            flex,
            show_type,
            compress,
            name_px: name,
            path_px: path,
            size_px: size,
            mtime_px: mtime,
            type_px: type_w,
            cols,
            dividers,
        }
    }

    fn cell(&self, origin: Rect, col: &Column) -> Rect {
        Rect::from_min_size(
            egui::pos2(origin.left() + col.x, origin.top()),
            Vec2::new(col.width, origin.height()),
        )
    }

    /// 压缩模式下只允许拖「名称」分隔条（大小/时间被钉在最小宽度）。
    fn divider_resizable(&self, kind: ColKind) -> bool {
        match kind {
            ColKind::Name => true,
            _ => !self.compress,
        }
    }
}

/// 单行绘制参数（超过 7 个参数 clippy 会报，收成一个结构体）。
struct RowView<'a> {
    index: usize,
    row: &'a db::Row,
    hovered: bool,
    selected: bool,
    low: bool,
    mark_low: bool,
}

pub struct SearchState {
    /// 后台查询结果回传通道；查询在线程里跑，界面永不卡顿。
    rx: Option<Receiver<QueryReply>>,
    pending: bool,
    seq: u64,
    pub query: String,
    pub last_applied: String,
    /// 上一帧看到的 query：只在它真正变化时才重置防抖计时（修每帧重置导致「只有回车才检索」）。
    last_seen_query: String,
    pub parsed: filter::Filter,
    pub total: u64,
    pub rows: Vec<db::Row>,
    /// true = rows 就是全部命中（全量载入完成/进行中），显示序覆盖全量；
    /// false = rows 只是当前页窗口（超大数据集退回按窗口折叠）。
    all_loaded: bool,
    pub window_start: u64,
    pub sort: SortChoice,
    pub kind: KindChoice,
    pub scope: String,
    pub sql_ms: f64,
    pub count_ms: f64,
    /// 选中行号（显示序，与折叠后的视觉顺序一致）。
    pub selected: usize,
    pub dirty: bool,
    pub pending_at: Option<Instant>,
    pub scan_warn: bool,
    /// true = 展开全部行（低价值行淡色标记）；false（默认）= 隐藏低价值行。
    pub show_low_value: bool,
    pub source: SourceMode,
    /// 用户先按过 ↑/↓ 才允许回车打开选中行。
    keyboard_nav: bool,
    /// 列宽比例（持久化到 %APPDATA%\DiskIntel\search-columns.json）。
    widths: ColumnWidths,
    /// 低价值判定缓存（与 rows 等长）与当前窗口计数。
    buckets: Vec<Option<Bucket>>,
    fold_counts: FoldCounts,
    /// 显示序 → rows 下标。
    order: Vec<u32>,
    fold_dirty: bool,
    /// 后台查询报错（目前只用于空态提示）。
    query_error: Option<String>,
    /// 上一次真正执行的查询指纹（用于跳过重复 refresh）。
    last_key: Option<QueryKey>,
    /// 取消上一轮还在跑的后台载入（切实时引擎 / 新查询时），避免白烧 CPU 和内存。
    load_cancel: Option<Arc<AtomicBool>>,
    /// 实时引擎状态（App 每帧同步进来）。
    pub engine_ready: bool,
    pub engine_label: String,
    live_pending: bool,
    live_error: Option<String>,
    live_seq: Option<u64>,
    want_live: Option<QuerySpec>,
    live_active: bool,
}

impl SearchState {
    pub fn new() -> Self {
        Self {
            rx: None,
            pending: false,
            seq: 0,
            query: String::new(),
            last_applied: String::new(),
            last_seen_query: String::new(),
            parsed: filter::Filter::default(),
            total: 0,
            rows: Vec::new(),
            all_loaded: true,
            window_start: 0,
            sort: SortChoice::Path,
            kind: KindChoice::All,
            scope: String::new(),
            sql_ms: 0.0,
            count_ms: 0.0,
            selected: 0,
            dirty: true,
            pending_at: None,
            scan_warn: false,
            show_low_value: std::env::var_os("DISKINTEL_UI_SHOW_LOW").is_some(),
            source: SourceMode::Auto,
            keyboard_nav: false,
            widths: load_column_widths(),
            buckets: Vec::new(),
            fold_counts: FoldCounts::default(),
            order: Vec::new(),
            fold_dirty: true,
            query_error: None,
            last_key: None,
            load_cancel: None,
            engine_ready: false,
            engine_label: "未探测".to_owned(),
            live_pending: false,
            live_error: None,
            live_seq: None,
            want_live: None,
            live_active: false,
        }
    }

    fn build_filter(&self) -> filter::Filter {
        let mut parsed = filter::parse(&self.query);
        if self.kind != KindChoice::All {
            parsed.kind = Some(match self.kind {
                KindChoice::File => filter::Kind::File,
                KindChoice::Dir => filter::Kind::Dir,
                KindChoice::All => unreachable!(),
            });
        }
        let scope = self.scope.trim();
        if !scope.is_empty() {
            parsed.scope = Some(scope.to_owned());
        }
        parsed
    }

    /// App 下发实时查询后回填 id，用于过滤迟到的结果。
    pub fn set_live_seq(&mut self, id: u64) {
        self.live_seq = Some(id);
        self.live_pending = true;
    }

    /// 取出待下发的实时查询（App 负责交给引擎）。
    pub fn take_live_request(&mut self) -> Option<QuerySpec> {
        self.want_live.take()
    }

    /// 引擎事件回流。
    pub fn on_engine_event(&mut self, event: EngineEvent) {
        match event {
            EngineEvent::State(state) => {
                self.engine_ready = state.is_ready();
                self.engine_label = state.label().to_owned();
            }
            EngineEvent::Query { id, result } => {
                if Some(id) != self.live_seq {
                    return;
                }
                self.live_pending = false;
                match result {
                    Ok(outcome) => {
                        self.total = outcome.total.max(outcome.rows.len() as u64);
                        let mut rows: Vec<db::Row> = outcome
                            .rows
                            .iter()
                            .map(|row: &EngineRow| db::Row {
                                path: row.path.clone(),
                                size: row.size.unwrap_or(0) as i64,
                                mtime: row.mtime().unwrap_or(0),
                                is_dir: row.is_dir,
                            })
                            .collect();
                        if self.sort == SortChoice::Size {
                            rows.sort_by_key(|row| std::cmp::Reverse(row.size));
                        }
                        self.rows = rows;
                        self.all_loaded = false;
                        self.window_start = 0;
                        self.fold_dirty = true;
                        self.sql_ms = outcome.elapsed_ms;
                        self.live_active = true;
                        self.live_error = None;
                    }
                    Err(failure) => {
                        self.live_error = Some(failure.reason);
                        self.live_active = false;
                    }
                }
            }
            EngineEvent::Idle => {}
        }
    }

    /// 构建实时查询；引擎不支持 | ! 正则，守卫拦下就回落到快照。
    fn build_live_spec(&self) -> Result<QuerySpec, String> {
        let text = self.query.trim().to_owned();
        engine::guard_query(&text)?;
        let sort = match self.sort {
            SortChoice::Path => (GozSort::Path, GozDir::Asc),
            SortChoice::Size => (GozSort::Size, GozDir::Desc),
            SortChoice::Newest => (GozSort::DateModified, GozDir::Desc),
            SortChoice::Oldest => (GozSort::DateModified, GozDir::Asc),
        };
        let mut spec = QuerySpec::new(text)
            .sorted(sort.0, sort.1)
            .page(0, Some(engine::proto::PAGE_ROWS));
        let scope = self.scope.trim();
        if !scope.is_empty() {
            spec = spec.within(scope);
        }
        Ok(spec)
    }

    /// 重新计数并加载首页窗口：查询放到后台线程，避免全表扫描时界面卡死。
    pub fn refresh(&mut self, snap: &db::Snapshot, ctx: &egui::Context) {
        // 同一份输入 + 同一个快照 + 同一个引擎状态：结果不会变，跳过重复刷新。
        let key = QueryKey {
            query: self.query.clone(),
            scope: self.scope.clone(),
            kind: self.kind,
            sort: self.sort,
            source: self.source,
            // 只要快照时引擎状态与结果无关，别让引擎事件触发重复载入。
            engine_ready: self.source != SourceMode::Snapshot && self.engine_ready,
            engine_label: self.engine_label.clone(),
            snapshot: snap.name.clone(),
        };
        if self.last_key.as_ref() == Some(&key) {
            return;
        }
        self.last_key = Some(key);
        if let Some(cancel) = self.load_cancel.take() {
            cancel.store(true, Ordering::Relaxed);
        }
        self.query_error = None;
        // 有实时索引且不是「只要快照」：优先走引擎（毫秒级）。
        let wants_live = self.source != SourceMode::Snapshot
            && self.engine_ready
            && !self.query.trim().is_empty();
        if wants_live {
            match self.build_live_spec() {
                Ok(spec) => {
                    self.last_applied = self.query.clone();
                    self.parsed = self.build_filter();
                    // 切到实时引擎：seq 自增让旧快照载入的回包全部失效，避免把两路行混在一起。
                    self.seq = self.seq.wrapping_add(1);
                    self.pending = false;
                    self.rx = None;
                    self.load_cancel = None;
                    self.want_live = Some(spec);
                    self.live_pending = true;
                    self.live_active = false;
                    self.all_loaded = false;
                    self.live_error = None;
                    self.live_seq = None; // App 下发后回填
                    return;
                }
                Err(reason) => {
                    // 引擎语法不支持：如实提示并回落到快照，不静默错答。
                    self.live_error = Some(reason);
                }
            }
        }
        self.live_active = false;
        self.fold_dirty = true;
        if snap.is_empty() {
            self.total = 0;
            self.rows.clear();
            self.all_loaded = true;
            self.last_applied = self.query.clone();
            return;
        }
        self.parsed = self.build_filter();
        self.last_applied = self.query.clone();
        self.seq = self.seq.wrapping_add(1);
        let seq = self.seq;
        let path = snap.path.clone();
        let filter = self.parsed.clone();
        let sort = self.sort.sort_by();
        // 全量载入只在「结果确实由快照呈现」时才值得做：
        // 实时引擎会接管（一页 4096 行），Auto 模式下引擎还没探明时先别烧 CPU 和内存。
        let allow_load_all = match self.source {
            SourceMode::Snapshot => true,
            SourceMode::Live => false,
            SourceMode::Auto => !self.engine_ready && self.engine_label != "未探测",
        };
        let (tx, rx) = std::sync::mpsc::channel();
        self.rx = Some(rx);
        self.pending = true;
        let cancel = Arc::new(AtomicBool::new(false));
        self.load_cancel = Some(cancel.clone());
        let ctx = ctx.clone();
        std::thread::spawn(move || {
            let Ok(snapshot) = db::Snapshot::open_path(&path) else {
                let _ = tx.send(QueryReply {
                    seq,
                    total: 0,
                    offset: 0,
                    rows: Vec::new(),
                    sql_ms: 0.0,
                    count_ms: 0.0,
                    error: Some(format!("打开快照 {} 失败", path.display())),
                    done: true,
                });
                ctx.request_repaint();
                return;
            };
            let count_started = Instant::now();
            let total = snapshot.count(&filter);
            let count_ms = count_started.elapsed().as_secs_f64() * 1000.0;
            // 命中数够小、且结果确实由快照呈现时才全量载入：
            // 折叠顺序与分类计数才能覆盖全部命中。
            let load_all = allow_load_all && total <= LOAD_ALL_CAP;
            let first_limit = if snapshot.is_v4() {
                V4_CHUNK.min(total.max(WINDOW))
            } else {
                WINDOW.min(total)
            };
            // 首屏先到，界面立即有内容；其余行再一次性取回（比一遍遍 OFFSET 分页快）。
            let (first_rows, first_ms) = snapshot.page(&filter, sort, 0, first_limit);
            let loaded = first_rows.len() as u64;
            let first_done = !load_all || loaded == 0 || loaded >= total;
            let _ = tx.send(QueryReply {
                seq,
                total,
                offset: 0,
                rows: first_rows,
                sql_ms: first_ms,
                count_ms,
                error: None,
                done: first_done,
            });
            ctx.request_repaint();
            if !first_done && !cancel.load(Ordering::Relaxed) {
                let (rest, rest_ms) = snapshot.page(&filter, sort, loaded, total - loaded);
                eprintln!(
                    "[fold] 全量载入 {} 条：首批 {:.0} ms（{} 条）+ 其余 {:.0} ms（{} 条）",
                    db::thousands(total),
                    first_ms,
                    db::thousands(loaded),
                    rest_ms,
                    db::thousands(rest.len() as u64)
                );
                let _ = tx.send(QueryReply {
                    seq,
                    total,
                    offset: loaded,
                    rows: rest,
                    sql_ms: 0.0,
                    count_ms: 0.0,
                    error: None,
                    done: true,
                });
                ctx.request_repaint();
            }
            snapshot.shrink();
        });
    }

    pub fn busy(&self) -> bool {
        self.pending || self.live_pending
    }

    /// 接收后台结果（可能分批）；每帧调用一次。
    fn poll(&mut self) {
        while let Some(rx) = self.rx.as_ref() {
            let received = rx.try_recv();
            match received {
                Ok(reply) => {
                    if reply.seq != self.seq {
                        continue;
                    }
                    if let Some(error) = reply.error {
                        self.query_error = Some(error);
                        self.total = 0;
                        self.rows.clear();
                        self.pending = false;
                        self.rx = None;
                        break;
                    }
                    if reply.offset == 0 {
                        self.total = reply.total;
                        self.all_loaded = reply.total <= LOAD_ALL_CAP;
                        self.rows = reply.rows;
                        self.window_start = 0;
                        self.sql_ms = reply.sql_ms;
                        self.count_ms = reply.count_ms;
                        self.selected = self
                            .selected
                            .min(self.total.saturating_sub(1) as usize);
                    } else {
                        self.rows.extend(reply.rows);
                    }
                    self.fold_dirty = true;
                    self.pending = !reply.done;
                    if reply.done {
                        self.rx = None;
                        break;
                    }
                }
                Err(std::sync::mpsc::TryRecvError::Empty) => break,
                Err(std::sync::mpsc::TryRecvError::Disconnected) => {
                    self.pending = false;
                    self.rx = None;
                    break;
                }
            }
        }
    }

    /// 保证 [start, end) 已在内存里；缺了就整窗换页（滚动时同步查询，走索引时是个位数毫秒）。
    ///
    /// 全量载入模式（all_loaded）下 rows 就是全部命中，滚动只做纯内存映射，不再查库。
    fn ensure(&mut self, snap: &db::Snapshot, start: u64, end: u64) {
        if self.all_loaded || self.total == 0 || self.pending || self.live_active || self.live_pending
        {
            return;
        }
        let loaded_end = self.window_start + self.rows.len() as u64;
        if start >= self.window_start && end <= loaded_end {
            return;
        }
        let limit = WINDOW.max(end.saturating_sub(start));
        let offset = start.saturating_sub(60.min(start));
        let (rows, sql_ms) = snap.page(&self.parsed, self.sort.sort_by(), offset, limit);
        self.rows = rows;
        self.window_start = offset;
        self.fold_dirty = true;
        self.sql_ms = sql_ms;
        self.scan_warn = !self.sort.sort_by().is_indexed();
    }

    fn invalidate_fold(&mut self) {
        self.fold_dirty = true;
    }

    /// 重算低价值分类、当前窗口计数与显示序（行集变化或折叠开关变化时才做）。
    fn sync_fold(&mut self) {
        if !self.fold_dirty {
            return;
        }
        self.fold_dirty = false;
        self.buckets.clear();
        self.buckets.reserve(self.rows.len());
        let mut counts = FoldCounts::default();
        for row in &self.rows {
            // 快照库不保存 Windows 属性位：目录恒 16、文件恒 0，交给 classify 兜底。
            let bucket = low_value_bucket(&row.path, row.is_dir);
            if let Some(found) = &bucket {
                counts.add(found);
            }
            self.buckets.push(bucket);
        }
        self.fold_counts = counts;
        // 折叠只在能准确枚举「显示行」时可用：全量载入、或实时引擎已拿到它那一页。
        // 快照分页窗口模式（命中 > LOAD_ALL_CAP）枚举不出显示行数，硬折叠就会出现
        //「折叠条说已折叠、表格却照样全显示」的假象，所以该模式只按原顺序显示。
        let expanded = self.show_low_value || !self.fold_available();
        self.order = fold_order(&self.buckets, expanded);
        if self.fold_available() {
            // 折叠 / 展开切换后显示行数会变，选中行必须重新夹取，不能越界。
            self.selected = clamp_selected(self.selected, self.order.len());
        }
    }

    /// 折叠是否可用：全量载入（快照 ≤ 上限）或实时引擎已回传它那一页时，
    /// 显示行都能准确枚举；快照分页窗口模式则不能。
    fn fold_available(&self) -> bool {
        foldable(self.all_loaded, self.live_active)
    }

    /// 当前显示行数（滚动范围 / 虚拟滚动窗口的总行数）。
    fn display_total(&self) -> usize {
        display_len(self.fold_available(), self.total, self.order.len())
    }

    /// 显示序下标 → rows 下标；未加载区间返回 None。
    fn db_index_for_display(&self, display: usize) -> Option<usize> {
        if self.all_loaded {
            return self.order.get(display).map(|index| *index as usize);
        }
        let start = self.window_start as usize;
        if display < start || display >= start + self.rows.len() {
            return None;
        }
        self.order.get(display - start).map(|index| *index as usize)
    }

    fn selected_row(&self) -> Option<&db::Row> {
        self.rows.get(self.db_index_for_display(self.selected)?)
    }

    fn save_columns(&self) {
        save_column_widths(self.widths);
    }

    pub fn show(&mut self, ui: &mut Ui, pal: &Palette, snap: &db::Snapshot) -> Option<SearchAction> {
        let mut action = None;
        self.poll();
        self.filter_bar(ui, pal, snap);
        ui.add_space(4.0);

        // 防抖：只在 query 相对「上一帧看到的」真正变化时重置计时；
        // 旧实现每帧都重置 pending_at，导致 due 永远不成立（只有回车才真正检索）。
        let enter = ui.input(|i| i.key_pressed(egui::Key::Enter));
        if self.query != self.last_seen_query {
            // 新输入之后，回车只提交检索，不再打开之前的选中行。
            self.keyboard_nav = false;
        }
        let due = debounce_due(
            &self.query,
            &self.last_applied,
            &mut self.last_seen_query,
            &mut self.pending_at,
            enter,
        );
        if due || self.dirty {
            self.dirty = false;
            self.pending_at = None;
            let ctx = ui.ctx().clone();
            self.refresh(snap, &ctx);
        } else if self.pending_at.is_some() {
            ui.ctx().request_repaint_after(Duration::from_millis(80));
        }

        if snap.is_empty() {
            crate::widgets::empty_state(
                ui,
                pal,
                "还没有任何快照",
                "点击右上角「扫描」建立第一份索引；或先用 Python 版 CLI 生成快照后重开本窗口。",
            );
            return action;
        }

        if self.pending && self.total == 0 {
            ui.vertical_centered(|ui| {
                ui.add_space(80.0);
                ui.spinner();
                ui.add_space(8.0);
                ui.label(RichText::new("正在检索…").size(12.5).color(pal.text_dim));
            });
            return action;
        }

        if self.total == 0 {
            if let Some(error) = &self.query_error {
                crate::widgets::empty_state(ui, pal, "查询失败", error);
            } else if self.query.trim().is_empty() {
                crate::widgets::empty_state(
                    ui,
                    pal,
                    "没有匹配结果",
                    "在工具栏输入关键词；空格分隔表示「同时满足」，输入停顿约 0.3 秒或回车即生效。",
                );
            } else {
                crate::widgets::empty_state(
                    ui,
                    pal,
                    "没有匹配结果",
                    "没有匹配项：试试缩短关键词、去掉 ext:/size: 条件，或切换快照。",
                );
            }
            return action;
        }

        // 先同步折叠计数（折叠条文案要用最新计数），列宽布局与表头不依赖显示行数。
        self.sync_fold();

        let table_width = ui.available_width();
        let layout = TableLayout::build(table_width, &self.widths);
        let (header_rect, _) =
            ui.allocate_exact_size(Vec2::new(table_width, theme::HEADER_H), Sense::hover());
        if let Some(sort) = self.paint_header(ui, pal, header_rect, &layout) {
            self.sort = sort;
            let ctx = ui.ctx().clone();
            self.refresh(snap, &ctx);
        }

        // 折叠条：固定贴在结果列表底部；计数来自 classify（全量已载入时覆盖全部命中），
        // 折叠只影响显示（隐藏 / 展现），不影响命中总数。
        self.fold_bar(ui, pal);
        // 折叠条可能刚被点击：立刻重建显示序并取显示行数，滚动范围/虚拟窗口都以显示行为准。
        self.sync_fold();
        let total_rows = self.display_total();

        if total_rows == 0 && self.fold_counts.total > 0 {
            let title = if self.pending {
                format!(
                    "正在载入结果…（已折叠 {} 项低价值结果）",
                    self.fold_counts.total
                )
            } else {
                format!(
                    "全部命中都是已折叠的低价值结果（{} 项）",
                    self.fold_counts.total
                )
            };
            crate::widgets::empty_state(
                ui,
                pal,
                &title,
                "点击下方折叠条展开，或勾选条件栏的「显示低价值」。",
            );
            return action;
        }

        let mut clicked_row: Option<usize> = None;
        let mut open_row: Option<String> = None;
        let mut context_action: Option<SearchAction> = None;

        egui::ScrollArea::vertical()
            .auto_shrink([false, false])
            .show_rows(ui, theme::ROW_H, total_rows, |ui, range| {
                self.ensure(snap, range.start as u64, range.end as u64);
                self.sync_fold();
                let row_width = ui.available_width().min(layout.width).max(1.0);
                for display in range {
                    let (rect, response) = ui.allocate_exact_size(
                        Vec2::new(row_width, theme::ROW_H),
                        Sense::click(),
                    );
                    let Some(db_index) = self.db_index_for_display(display) else {
                        continue;
                    };
                    let Some(row) = self.rows.get(db_index) else {
                        continue;
                    };
                    let low_label = self
                        .buckets
                        .get(db_index)
                        .and_then(|entry| entry.as_ref())
                        .map(|found| found.label());
                    let low = low_label.is_some();
                    let selected = self.selected == display;
                    let hovered = response.hovered();
                    let view = RowView {
                        index: display,
                        row,
                        hovered,
                        selected,
                        low,
                        mark_low: low && self.show_low_value,
                    };
                    self.paint_row(ui, pal, &layout, rect, &view);
                    let row_path = row.path.clone();
                    if !screenshot_mode() {
                        let is_dir = row.is_dir;
                        let size_text = if is_dir {
                            "—".to_owned()
                        } else {
                            db::human_size(row.size)
                        };
                        let mtime = row.mtime;
                        response.clone().on_hover_ui(|ui| {
                            ui.set_max_width(560.0);
                            ui.label(RichText::new(&row_path).size(11.5).color(pal.text));
                            ui.label(
                                RichText::new(format!(
                                    "{} 条 · {} · {}",
                                    icons::label(icons::classify(&row_path, is_dir)),
                                    size_text,
                                    db::format_unix(mtime)
                                ))
                                .size(10.5)
                                .color(pal.text_faint),
                            );
                            if let Some(label) = low_label {
                                ui.label(
                                    RichText::new(format!("低价值分类：{label}"))
                                        .size(10.5)
                                        .color(pal.warn),
                                );
                            }
                        });
                    }
                    if response.clicked() {
                        self.selected = display;
                        clicked_row = Some(display);
                    }
                    if response.double_clicked() {
                        open_row = Some(row_path.clone());
                    }
                    if response.secondary_clicked() {
                        self.selected = display;
                    }
                    response.context_menu(|ui| {
                        if ui.button("打开").clicked() {
                            context_action = Some(SearchAction::Open(row_path.clone()));
                            ui.close();
                        }
                        if ui.button("打开所在文件夹").clicked() {
                            context_action = Some(SearchAction::Open(
                                db::parent_dir(&row_path).unwrap_or_else(|| row_path.clone()),
                            ));
                            ui.close();
                        }
                        if ui.button("复制完整路径").clicked() {
                            ui.ctx().copy_text(row_path.clone());
                            context_action = Some(SearchAction::CopyPath(row_path.clone()));
                            ui.close();
                        }
                        if row.is_dir && ui.button("在空间视图中查看").clicked() {
                            context_action = Some(SearchAction::Focus(row_path.clone()));
                            ui.close();
                        }
                    });
                }
            });

        // 键盘导航：没有控件持有焦点时才响应，避免和输入框抢键。
        if ui.ctx().memory_mut(|m| m.focused()).is_none() {
            let down = ui.input(|i| i.key_pressed(egui::Key::ArrowDown));
            let up = ui.input(|i| i.key_pressed(egui::Key::ArrowUp));
            if down {
                self.selected = (self.selected + 1).min(total_rows.saturating_sub(1));
                self.keyboard_nav = true;
            }
            if up {
                self.selected = self.selected.saturating_sub(1);
                self.keyboard_nav = true;
            }
            // 回车：只有先用 ↑/↓ 选过行才打开；否则这一帧的回车只负责提交检索（已在上面处理）。
            if self.keyboard_nav
                && ui.input(|i| i.key_pressed(egui::Key::Enter))
                && let Some(row) = self.selected_row()
            {
                open_row = Some(row.path.clone());
            }
            if ui.input(|i| i.modifiers.command && i.key_pressed(egui::Key::C))
                && let Some(row) = self.selected_row()
            {
                ui.ctx().copy_text(row.path.clone());
                action = Some(SearchAction::CopyPath(row.path.clone()));
            }
        }

        if let Some(path) = open_row {
            action = Some(SearchAction::Open(path));
        } else if let Some(next) = context_action {
            action = Some(next);
        } else if clicked_row.is_some() {
            // 单击只选中，不产生动作
        }
        action
    }

    /// 底部折叠条：折叠态低价值行被隐藏，条子显示「已折叠 N 项…」，点击整行展开；
    /// 展开态显示「已展开 N 项…」。窗口模式不启用折叠，所以不显示本条。
    fn fold_bar(&mut self, ui: &mut Ui, pal: &Palette) {
        if !self.fold_available() || self.fold_counts.total == 0 {
            return;
        }
        let expanded = self.show_low_value;
        let total = self.fold_counts.total;
        let summary = self.fold_counts.summary();
        let verb = if expanded { "已展开" } else { "已折叠" };
        let action_text = if expanded { "点击折叠" } else { "点击展开" };
        let loading = if self.pending { " · 载入中…" } else { "" };
        let scope_hint = if self.all_loaded {
            format!(
                "低价值分类统计覆盖全部 {} 条命中；折叠只影响显示，不影响命中总数",
                db::thousands(self.total)
            )
        } else {
            format!(
                "实时引擎只回传一页：统计覆盖已加载的 {} 条（命中总数 {}）；折叠只影响显示",
                db::thousands(self.rows.len() as u64),
                db::thousands(self.total)
            )
        };
        let text = if summary.is_empty() {
            format!("{verb} {total} 项低价值结果 · {action_text}{loading}")
        } else {
            format!("{verb} {total} 项低价值结果（{summary}）· {action_text}{loading}")
        };

        egui::Panel::bottom("search-fold-bar")
            .exact_size(FOLD_BAR_H)
            .show_separator_line(false)
            .frame(
                egui::Frame::NONE
                    .fill(pal.panel_alt)
                    .inner_margin(egui::Margin::symmetric(10, 0)),
            )
            .show(ui, |ui| {
                let (rect, response) = ui.allocate_exact_size(
                    Vec2::new(ui.available_width(), FOLD_BAR_H),
                    Sense::click(),
                );
                let hovered = response.hovered();
                if hovered {
                    ui.painter()
                        .rect_filled(rect, CornerRadius::same(0), pal.hover);
                    ui.ctx().set_cursor_icon(CursorIcon::PointingHand);
                }
                ui.painter().hline(
                    rect.x_range(),
                    rect.top() + 0.5,
                    Stroke::new(1.0, pal.border_soft),
                );
                let glyph = if expanded {
                    icons::Glyph::ChevronUp
                } else {
                    icons::Glyph::ChevronDown
                };
                icons::paint_glyph(
                    ui.painter(),
                    Rect::from_center_size(
                        egui::pos2(rect.left() + 8.0, rect.center().y),
                        Vec2::splat(12.0),
                    ),
                    glyph,
                    pal.text_faint,
                );
                ui.painter().text(
                    egui::pos2(rect.left() + 22.0, rect.center().y),
                    Align2::LEFT_CENTER,
                    &text,
                    FontId::proportional(11.0),
                    if hovered { pal.text } else { pal.text_dim },
                );
                response.clone().on_hover_text(scope_hint.clone());
                if response.clicked() {
                    self.show_low_value = !self.show_low_value;
                    self.invalidate_fold();
                }
            });
    }

    fn filter_bar(&mut self, ui: &mut Ui, pal: &Palette, snap: &db::Snapshot) {
        let row_height = theme::FILTERBAR_H;
        let (rect, _) =
            ui.allocate_exact_size(Vec2::new(ui.available_width(), row_height), Sense::hover());
        let mut child = ui.new_child(
            egui::UiBuilder::new()
                .max_rect(rect.shrink2(Vec2::new(8.0, 4.0)))
                .layout(egui::Layout::left_to_right(egui::Align::Center)),
        );
        let _ = snap;
        child.spacing_mut().item_spacing.x = 6.0;

        child.label(RichText::new("来源").size(11.0).color(pal.text_faint));
        let before_source = self.source;
        egui::ComboBox::from_id_salt("source-filter")
            .selected_text(RichText::new(self.source.short()).size(12.0).color(pal.text))
            .width(64.0)
            .show_ui(&mut child, |ui| {
                ui.selectable_value(&mut self.source, SourceMode::Auto, "自动（有实时就用）");
                ui.selectable_value(&mut self.source, SourceMode::Live, "实时索引（goz）");
                ui.selectable_value(&mut self.source, SourceMode::Snapshot, "快照（SQLite）");
            });
        if before_source != self.source {
            self.dirty = true;
        }
        child.add_space(4.0);
        child.label(RichText::new("类型").size(11.0).color(pal.text_faint));
        egui::ComboBox::from_id_salt("kind-filter")
            .selected_text(RichText::new(self.kind.label()).size(12.0).color(pal.text))
            .width(70.0)
            .show_ui(&mut child, |ui| {
                for choice in [KindChoice::All, KindChoice::File, KindChoice::Dir] {
                    ui.selectable_value(&mut self.kind, choice, choice.label());
                }
            });
        child.add_space(6.0);
        child.label(RichText::new("排序").size(11.0).color(pal.text_faint));
        egui::ComboBox::from_id_salt("sort-filter")
            .selected_text(RichText::new(self.sort.short_label()).size(12.0).color(pal.text))
            .width(78.0)
            .show_ui(&mut child, |ui| {
                for choice in [
                    SortChoice::Path,
                    SortChoice::Size,
                    SortChoice::Newest,
                    SortChoice::Oldest,
                ] {
                    ui.selectable_value(&mut self.sort, choice, choice.label());
                }
            });
        child.add_space(6.0);
        child.label(RichText::new("范围").size(11.0).color(pal.text_faint));
        if child
            .add(
                egui::TextEdit::singleline(&mut self.scope)
                    .hint_text("如 D:\\ 或 C:\\Users")
                    .desired_width(190.0)
                    .font(FontId::proportional(12.0)),
            )
            .changed()
        {
            self.dirty = true;
        }

        child.add_space(4.0);
        // 条件芯片：把用户输入里的高级条件解析出来并可视化，AI/人都能一眼看懂
        let parsed = filter::parse(&self.query);
        for ext in parsed.exts.iter().take(4) {
            crate::widgets::tag(&mut child, pal, &format!("ext:{ext}"), pal.accent);
        }
        if let Some(size) = parsed.min_size {
            crate::widgets::tag(&mut child, pal, &format!("≥{}", db::human_size(size)), pal.ok);
        }
        if let Some(size) = parsed.max_size {
            crate::widgets::tag(&mut child, pal, &format!("≤{}", db::human_size(size)), pal.ok);
        }
        if let Some(after) = parsed.after {
            crate::widgets::tag(
                &mut child,
                pal,
                &format!("> {}", &db::format_unix(after)[..10]),
                pal.text_dim,
            );
        }
        if !self.parsed.terms.is_empty() && !self.query.trim().is_empty() {
            crate::widgets::tag(
                &mut child,
                pal,
                &format!("{} 词", self.parsed.terms.len()),
                pal.text_dim,
            );
        }

        // 窗口窄时只保留重置与徽章，去掉说明文字，避免和范围输入框叠在一起。
        let narrow = child.max_rect().width() < 1150.0;
        child.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
            if ui
                .add(egui::Button::new(RichText::new("重置").size(11.5)).corner_radius(4))
                .clicked()
            {
                self.query.clear();
                self.scope.clear();
                self.kind = KindChoice::All;
                self.sort = SortChoice::Path;
                self.keyboard_nav = false;
                self.dirty = true;
            }
            ui.add_space(6.0);
            // 「显示低价值」：勾选 = 展开全部行，不勾 = 隐藏低价值行（与底部折叠条同一开关）。
            // 窗口模式枚举不出显示行数，折叠不可用：置灰显示为「全部显示」并说明原因。
            let fold_ready = self.fold_available();
            let mut show = self.show_low_value || !fold_ready;
            let toggled = ui
                .add_enabled(
                    fold_ready,
                    egui::Checkbox::new(&mut show, RichText::new("显示低价值").size(11.0)),
                )
                .on_disabled_hover_text(
                    "命中超过全量载入上限（20 万条）：折叠需要全部结果才能准确计算显示行数",
                );
            if toggled.changed() {
                self.show_low_value = show;
                self.invalidate_fold();
            }
            ui.add_space(6.0);
            let (badge, tint) = if self.live_active {
                (format!("实时 · {} ms", self.sql_ms.round() as i64), pal.ok)
            } else if self.engine_ready {
                ("快照 · 引擎就绪未用".to_owned(), pal.text_faint)
            } else {
                (format!("快照 · 引擎{}", self.engine_label), pal.text_faint)
            };
            crate::widgets::tag(ui, pal, &badge, tint);
            ui.add_space(4.0);
            if self.scan_warn {
                ui.label(
                    RichText::new("全表扫描排序，首次约 1-3 s")
                        .size(10.5)
                        .color(pal.warn),
                );
            } else if !narrow {
                ui.label(
                    RichText::new("空格=同时满足 · ext: · size:>10m · after: · path:")
                        .size(10.5)
                        .color(pal.text_faint),
                );
            }
        });
    }

    /// 画表头与列宽分隔条；返回被点击后应当切换到的排序。
    fn paint_header(
        &mut self,
        ui: &mut Ui,
        pal: &Palette,
        rect: Rect,
        layout: &TableLayout,
    ) -> Option<SortChoice> {
        let painter = ui.painter();
        painter.rect_filled(rect, CornerRadius::same(0), pal.panel_alt);
        painter.hline(
            rect.x_range(),
            rect.bottom() - 0.5,
            Stroke::new(1.0, pal.border),
        );
        let font = FontId::proportional(11.0);
        let mut next_sort = None;
        for (index, col) in layout.cols.iter().enumerate() {
            let col_rect = layout.cell(rect, col);
            let is_sorted = match col.kind {
                ColKind::Path => self.sort == SortChoice::Path,
                ColKind::Size => self.sort == SortChoice::Size,
                ColKind::Mtime => matches!(self.sort, SortChoice::Newest | SortChoice::Oldest),
                ColKind::Index | ColKind::Type | ColKind::Name => false,
            };
            let response = ui.interact(col_rect, ui.scope_id().with(("hdr", index)), Sense::click());
            if response.hovered() {
                painter.rect_filled(col_rect, CornerRadius::same(3), pal.hover);
            }
            let align = if col.numeric {
                Align2::RIGHT_CENTER
            } else {
                Align2::LEFT_CENTER
            };
            let pos = if col.numeric {
                egui::pos2(col_rect.right() - 4.0, col_rect.center().y)
            } else {
                egui::pos2(col_rect.left() + 4.0, col_rect.center().y)
            };
            let color = if is_sorted { pal.accent } else { pal.text_dim };
            painter.text(pos, align, col.label, font.clone(), color);
            if is_sorted && !col.numeric {
                let text_width = painter
                    .layout_no_wrap(col.label.to_owned(), font.clone(), color)
                    .size()
                    .x;
                let icon_center =
                    egui::pos2(col_rect.left() + 4.0 + text_width + 9.0, col_rect.center().y);
                icons::paint_glyph(
                    painter,
                    Rect::from_center_size(icon_center, Vec2::splat(10.0)),
                    icons::Glyph::SortAsc,
                    pal.accent,
                );
            }
            if response.clicked() {
                next_sort = match col.kind {
                    ColKind::Size => Some(SortChoice::Size),
                    ColKind::Mtime => Some(if self.sort == SortChoice::Newest {
                        SortChoice::Oldest
                    } else {
                        SortChoice::Newest
                    }),
                    ColKind::Name | ColKind::Path => Some(SortChoice::Path),
                    ColKind::Index | ColKind::Type => None,
                };
            }
        }

        // 列宽分隔条：命中区 ±4 px 落在列间距里，与列头点击区完全不重叠；
        // 拖动判定用 response.dragged()（拖出表头也仍然成立），不依赖悬停。
        let mut persist = false;
        for (kind, divider_x) in layout.dividers.iter().copied() {
            let x = rect.left() + divider_x;
            let hit = Rect::from_min_max(
                egui::pos2(x - DIVIDER_GRAB, rect.top()),
                egui::pos2(x + DIVIDER_GRAB, rect.bottom()),
            );
            let response = ui.interact(
                hit,
                ui.scope_id().with(("col-divider", kind as u8)),
                Sense::click_and_drag(),
            );
            let resizable = layout.divider_resizable(kind);
            let dragging = response.dragged();
            let hot = response.hovered() || dragging;
            if hot && resizable {
                ui.ctx().set_cursor_icon(CursorIcon::ResizeHorizontal);
            }
            if dragging && resizable {
                let delta = response.drag_delta().x;
                if delta != 0.0 {
                    self.resize_columns(kind, delta, layout);
                }
            }
            if response.drag_stopped() && resizable {
                persist = true;
            }
            if resizable && response.double_clicked() {
                self.autofit_column(kind, ui, layout);
                persist = true;
            }
            let line_color = if dragging && resizable {
                pal.accent
            } else if hot && resizable {
                pal.accent.gamma_multiply(0.75)
            } else {
                pal.border_soft
            };
            let line_width = if hot && resizable { 2.0 } else { 1.0 };
            painter.rect_filled(
                Rect::from_center_size(
                    egui::pos2(x, rect.center().y),
                    Vec2::new(line_width, rect.height() - 6.0),
                ),
                CornerRadius::same(1),
                line_color,
            );
        }
        if persist {
            self.save_columns();
        }
        next_sort
    }

    /// 拖拽分隔条：左侧列 +delta、右侧列 -delta；路径列永远吃剩余空间，所以只调它的邻居。
    fn resize_columns(&mut self, kind: ColKind, delta: f32, layout: &TableLayout) {
        nudge_columns(&mut self.widths, kind, delta, layout);
    }

    /// 双击分隔条：按当前已加载行的内容自适应该列宽度（路径列通过反推名称列实现）。
    fn autofit_column(&mut self, kind: ColKind, ui: &Ui, layout: &TableLayout) {
        let flex = layout.flex.max(1.0);
        let text_font = FontId::proportional(11.5);
        let measure = |text: &str| -> f32 {
            if text.is_empty() {
                return 0.0;
            }
            ui.painter()
                .layout_no_wrap(text.to_owned(), text_font.clone(), Color32::WHITE)
                .size()
                .x
        };
        let sample = (self.rows.len() / 400).max(1);
        match kind {
            ColKind::Name => {
                let mut width = NAME_MIN;
                for row in self.rows.iter().step_by(sample) {
                    width = width.max(measure(db::file_name(&row.path)) + 38.0);
                }
                let max = (flex - layout.size_px - layout.mtime_px - layout.type_px - PATH_MIN)
                    .max(NAME_MIN);
                self.widths.name = width.clamp(NAME_MIN, max) / flex;
            }
            ColKind::Path => {
                let mut width = PATH_MIN;
                for row in self.rows.iter().step_by(sample) {
                    let parent = db::parent_dir(&row.path).unwrap_or_default();
                    width = width.max(measure(&parent) + 12.0);
                }
                let max = (flex - NAME_MIN - layout.size_px - layout.mtime_px - layout.type_px)
                    .max(PATH_MIN);
                let path = width.clamp(PATH_MIN, max);
                let name =
                    (flex - path - layout.size_px - layout.mtime_px - layout.type_px).max(NAME_MIN);
                self.widths.name = name / flex;
            }
            ColKind::Size => {
                let mut width = SIZE_MIN;
                for row in self.rows.iter().step_by(sample) {
                    let text = if row.is_dir {
                        "—".to_owned()
                    } else {
                        db::human_size(row.size)
                    };
                    width = width.max(measure(&text) + 14.0);
                }
                let max = (flex - layout.name_px - layout.mtime_px - layout.type_px - PATH_MIN)
                    .max(SIZE_MIN);
                self.widths.size = width.clamp(SIZE_MIN, max) / flex;
            }
            ColKind::Mtime => {
                let mut width = MTIME_MIN;
                for row in self.rows.iter().step_by(sample) {
                    width = width.max(measure(&db::format_unix(row.mtime)) + 14.0);
                }
                let max = (flex - layout.name_px - layout.size_px - layout.type_px - PATH_MIN)
                    .max(MTIME_MIN);
                self.widths.mtime = width.clamp(MTIME_MIN, max) / flex;
            }
            ColKind::Index | ColKind::Type => {}
        }
    }

    fn paint_row(
        &self,
        ui: &Ui,
        pal: &Palette,
        layout: &TableLayout,
        rect: Rect,
        view: &RowView<'_>,
    ) {
        let painter = ui.painter();
        let fill = pal.row_fill(view.index, view.hovered, view.selected);
        if fill != Color32::TRANSPARENT {
            painter.rect_filled(rect, CornerRadius::same(0), fill);
        }
        if view.selected {
            painter.rect_filled(
                Rect::from_min_size(rect.min, Vec2::new(2.0, rect.height())),
                CornerRadius::same(0),
                pal.accent,
            );
        }
        if view.low && view.mark_low {
            // 展开态：低价值行左侧一条细线，配合淡色文字说明「混排进来的」
            painter.rect_filled(
                Rect::from_min_size(
                    egui::pos2(rect.left(), rect.top() + 4.0),
                    Vec2::new(2.0, rect.height() - 8.0),
                ),
                CornerRadius::same(1),
                pal.text_faint.gamma_multiply(0.8),
            );
        }
        let name_font = FontId::proportional(12.5);
        let dim_font = FontId::proportional(11.5);
        let mono_font = FontId::monospace(11.5);
        let name_color = if view.low { pal.text_faint } else { pal.text };
        let path_color = if view.low { pal.text_faint } else { pal.text_dim };

        for col in layout.cols.iter() {
            let cell = layout.cell(rect, col);
            match col.kind {
                ColKind::Index => {
                    painter.text(
                        egui::pos2(cell.left() + 4.0, cell.center().y),
                        Align2::LEFT_CENTER,
                        (view.index + 1).to_string(),
                        dim_font.clone(),
                        pal.text_faint,
                    );
                }
                ColKind::Name => {
                    let kind = icons::classify(&view.row.path, view.row.is_dir);
                    let icon_rect = Rect::from_center_size(
                        egui::pos2(cell.left() + 13.0, cell.center().y),
                        Vec2::splat(18.0),
                    );
                    icons::paint_file(painter, icon_rect, kind);
                    let text_rect = Rect::from_min_max(
                        egui::pos2(cell.left() + 28.0, cell.top()),
                        egui::pos2(cell.right() - 4.0, cell.bottom()),
                    );
                    let galley = elide(
                        ui,
                        db::file_name(&view.row.path),
                        name_font.clone(),
                        text_rect.width(),
                        name_color,
                    );
                    painter.galley(
                        egui::pos2(text_rect.left(), text_rect.center().y - galley.size().y / 2.0),
                        galley,
                        name_color,
                    );
                }
                ColKind::Path => {
                    let parent = db::parent_dir(&view.row.path).unwrap_or_default();
                    let text_rect = Rect::from_min_max(
                        egui::pos2(cell.left() + 4.0, cell.top()),
                        egui::pos2(cell.right() - 4.0, cell.bottom()),
                    );
                    let galley = elide(ui, &parent, dim_font.clone(), text_rect.width(), path_color);
                    painter.galley(
                        egui::pos2(text_rect.left(), text_rect.center().y - galley.size().y / 2.0),
                        galley,
                        path_color,
                    );
                }
                ColKind::Size => {
                    let text = if view.row.is_dir {
                        "—".to_owned()
                    } else {
                        db::human_size(view.row.size)
                    };
                    painter.text(
                        egui::pos2(cell.right() - 4.0, cell.center().y),
                        Align2::RIGHT_CENTER,
                        text,
                        mono_font.clone(),
                        pal.text_dim,
                    );
                }
                ColKind::Mtime => {
                    painter.text(
                        egui::pos2(cell.right() - 4.0, cell.center().y),
                        Align2::RIGHT_CENTER,
                        db::format_unix(view.row.mtime),
                        dim_font.clone(),
                        pal.text_faint,
                    );
                }
                ColKind::Type => {
                    let kind = icons::classify(&view.row.path, view.row.is_dir);
                    painter.text(
                        egui::pos2(cell.left() + 4.0, cell.center().y),
                        Align2::LEFT_CENTER,
                        icons::label(kind),
                        dim_font.clone(),
                        icons::color(kind).gamma_multiply(0.95),
                    );
                }
            }
        }
    }
}

/// 拖拽分隔条的纯逻辑（不碰 UI，便于单测）：左侧列 +delta、右侧列 -delta；
/// 路径列永远吃剩余空间，所以拖动它的邻居时只需改一个显式列。
fn nudge_columns(widths: &mut ColumnWidths, kind: ColKind, delta: f32, layout: &TableLayout) {
    let flex = layout.flex.max(1.0);
    let mut name = layout.name_px;
    let mut size = layout.size_px;
    let mut mtime = layout.mtime_px;
    let mut type_w = layout.type_px;
    match kind {
        ColKind::Name => {
            // 名称 + 路径 的像素和是固定的（中间没有别的列），拖开名称就是挤路径。
            let max = (name + layout.path_px - PATH_MIN).max(NAME_MIN);
            name = (name + delta).clamp(NAME_MIN, max);
        }
        ColKind::Path => {
            // 路径/大小：右侧是「大小」，路径吃剩余 → 拖右边界 = 缩小大小。
            let max = (size + layout.path_px - PATH_MIN).max(SIZE_MIN);
            size = (size - delta).clamp(SIZE_MIN, max);
        }
        ColKind::Size => {
            let room = (size + mtime).max(SIZE_MIN + MTIME_MIN);
            size = (size + delta).clamp(SIZE_MIN, room - MTIME_MIN);
            mtime = room - size;
        }
        ColKind::Mtime => {
            if layout.show_type {
                let room = (mtime + type_w).max(MTIME_MIN + TYPE_MIN);
                mtime = (mtime + delta).clamp(MTIME_MIN, room - TYPE_MIN);
                type_w = room - mtime;
            }
        }
        ColKind::Index | ColKind::Type => return,
    }
    widths.name = name / flex;
    widths.size = size / flex;
    widths.mtime = mtime / flex;
    if layout.show_type {
        widths.type_ = type_w / flex;
    }
}

/// 单行省略：二分字符数，保证不超宽且保留省略号。
fn elide(ui: &Ui, text: &str, font: FontId, max_width: f32, color: Color32) -> Arc<Galley> {
    let galley = ui.painter().layout_no_wrap(text.to_owned(), font.clone(), color);
    if galley.size().x <= max_width || max_width <= 8.0 {
        return galley;
    }
    let chars: Vec<char> = text.chars().collect();
    let mut low = 0usize;
    let mut high = chars.len();
    while low < high {
        let mid = (low + high).div_ceil(2);
        let mut candidate: String = chars[..mid].iter().collect();
        candidate.push('…');
        let probe = ui.painter().layout_no_wrap(candidate, font.clone(), color);
        if probe.size().x <= max_width {
            low = mid;
        } else {
            high = mid - 1;
        }
    }
    let mut candidate: String = chars[..low].iter().collect();
    candidate.push('…');
    ui.painter().layout_no_wrap(candidate, font.clone(), color)
}

/// 输入防抖状态机：query 相对 last_seen_query 变化时才重置计时，回车立即触发。
///
/// 回归点：旧实现在 query != last_applied 的每一帧都重置 pending_at，
/// 于是计时永远走不到 DEBOUNCE_MS，只有回车才会真正检索。
fn debounce_due(
    query: &str,
    last_applied: &str,
    last_seen_query: &mut String,
    pending_at: &mut Option<Instant>,
    enter: bool,
) -> bool {
    if query != last_seen_query {
        *last_seen_query = query.to_owned();
        *pending_at = if query != last_applied {
            Some(Instant::now())
        } else {
            None
        };
    }
    let due = pending_at
        .map(|at| at.elapsed() >= Duration::from_millis(DEBOUNCE_MS))
        .unwrap_or(false);
    enter || due
}

/// 是否处于截图自检（--shot）：此时不弹行悬停提示，避免鼠标恰好压在窗口上时
/// 提示框遮挡验收截图。正常交互（人点、AI 调）不受影响。
fn screenshot_mode() -> bool {
    static FLAG: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *FLAG.get_or_init(|| std::env::args().any(|arg| arg == "--shot"))
}

/// 低价值判定适配点：唯一依赖 lowvalue 模块签名的地方。
/// 快照库不保存 Windows 属性位（目录恒 16 / 文件恒 0），交给 classify 兜底。
fn low_value_bucket(path: &str, is_dir: bool) -> Option<Bucket> {
    classify(path, is_dir, if is_dir { 16 } else { 0 }).map(|v| v.bucket)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn approx(left: f32, right: f32) -> bool {
        (left - right).abs() < 0.01
    }

    #[test]
    fn column_widths_json_roundtrip() {
        let widths = ColumnWidths {
            name: 0.31,
            size: 0.09,
            mtime: 0.12,
            type_: 0.08,
        }
        .sanitized();
        let text = widths.to_json();
        let parsed = ColumnWidths::from_json(&text).expect("能解析自己写出的 JSON");
        assert!(approx(parsed.name, widths.name));
        assert!(approx(parsed.size, widths.size));
        assert!(approx(parsed.mtime, widths.mtime));
        assert!(approx(parsed.type_, widths.type_));
        assert!(text.contains("\"version\":1"));
    }

    #[test]
    fn column_widths_sanitized_rejects_garbage() {
        let widths = ColumnWidths {
            name: f32::NAN,
            size: -3.0,
            mtime: 5.0,
            type_: 0.0,
        }
        .sanitized();
        assert!(widths.name == 0.0);
        assert!(widths.size == 0.0);
        assert!(widths.mtime <= 0.85);
        assert!(!widths.is_valid());
        assert!(ColumnWidths::from_json("{\"name\": 0.3}").is_none());
    }

    #[test]
    fn layout_hides_type_and_compresses() {
        let default = ColumnWidths::default();
        let wide = TableLayout::build(1200.0, &default);
        assert!(wide.show_type);
        assert!(!wide.compress);
        let narrow = TableLayout::build(999.0, &default);
        assert!(!narrow.show_type);
        let compressed = TableLayout::build(800.0, &default);
        assert!(compressed.compress);
        assert!(approx(compressed.size_px, SIZE_MIN));
        assert!(approx(compressed.mtime_px, MTIME_MIN));
    }

    #[test]
    fn layout_never_overflows_and_keeps_path_minimum() {
        let widths = ColumnWidths::default();
        for avail in [560.0_f32, 700.0, 860.0, 900.0, 1000.0, 1200.0, 1440.0, 2560.0] {
            let layout = TableLayout::build(avail, &widths);
            assert!(
                layout.width <= avail + 0.5,
                "宽度 {avail} 溢出：{}",
                layout.width
            );
            if avail >= 900.0 {
                assert!(
                    layout.path_px >= PATH_MIN - 0.01,
                    "宽度 {avail} 的路径列挤到了 {}",
                    layout.path_px
                );
            }
            assert!(layout.flex > 0.0);
            assert!(!layout.cols.is_empty());
        }
    }

    #[test]
    fn dividers_stay_inside_the_gap() {
        let widths = ColumnWidths::default();
        let layout = TableLayout::build(1200.0, &widths);
        for (kind, divider_x) in layout.dividers.iter().copied() {
            let Some(col) = layout.cols.iter().find(|col| col.kind == kind) else {
                panic!("分隔条找不到所属列");
            };
            // 分隔条画在列尾 + 半个间距，命中区 ±4 仍落在 10 px 间距内。
            assert!(divider_x > col.x + col.width);
            assert!(divider_x < col.x + col.width + COL_GAP);
            assert!(divider_x - DIVIDER_GRAB >= col.x + col.width - 0.001);
        }
    }

    #[test]
    fn fold_hides_low_value_rows_and_expand_shows_all() {
        let buckets = vec![
            Some(crate::lowvalue::Bucket::System),
            None,
            Some(crate::lowvalue::Bucket::Rare),
            None,
            None,
        ];
        // 折叠态：显示序只含非低价值行（1/3/4），低价值行 0/2 不参与显示。
        let folded = fold_order(&buckets, false);
        assert_eq!(folded, vec![1, 3, 4]);
        // 展开态：全部行，保持原顺序。
        let expanded = fold_order(&buckets, true);
        assert_eq!(expanded, vec![0, 1, 2, 3, 4]);
        // 显示行数 = 折叠后的显示序长度；窗口模式（未全量载入）按命中总数滚动。
        assert_eq!(display_len(true, 5, folded.len()), 3);
        assert_eq!(display_len(true, 5, expanded.len()), 5);
        assert_eq!(display_len(false, 5, folded.len()), 5);
    }

    #[test]
    fn fold_is_available_only_when_display_rows_are_enumerable() {
        // 全量载入（快照 ≤ 20 万）：可折叠。
        assert!(foldable(true, false));
        // 实时引擎已回传它那一页：可以按这一页折叠。
        assert!(foldable(false, true));
        // 快照分页窗口模式：枚举不出显示行，不折叠（否则折叠条会说谎）。
        assert!(!foldable(false, false));
        assert_eq!(display_len(foldable(false, false), 535_967, 300), 535_967);
        assert_eq!(display_len(foldable(false, true), 535_967, 300), 300);
    }

    #[test]
    fn fold_all_rows_leaves_empty_display_and_selection_in_range() {
        // 154 条 dll 全部判为低价值：折叠后显示 0 行（列表为空），底部折叠条报 154。
        let buckets = [Some(Bucket::System); 154];
        let folded = fold_order(&buckets, false);
        let expanded = fold_order(&buckets, true);
        assert!(folded.is_empty(), "全部折叠时不应有显示行");
        assert_eq!(display_len(true, 154, folded.len()), 0);
        assert_eq!(display_len(true, 154, expanded.len()), 154);

        // 切换折叠后 selected 必须被夹进新的显示范围，不能越界。
        assert_eq!(clamp_selected(37, 0), 0);
        assert_eq!(clamp_selected(37, 154), 37);
        assert_eq!(clamp_selected(37, 12), 11);
        assert_eq!(clamp_selected(153, expanded.len()), 153);
    }

    #[test]
    fn fold_counts_summary_follows_bucket_all_order() {
        let mut counts = FoldCounts::default();
        // 故意乱序加入，summary 必须按 Bucket::ALL 的顺序输出。
        for bucket in [
            Bucket::Hidden,
            Bucket::Rare,
            Bucket::Installer,
            Bucket::System,
            Bucket::Backup,
            Bucket::Cache,
            Bucket::Temp,
        ] {
            counts.add(&bucket);
        }
        assert_eq!(counts.total, 7);
        assert_eq!(
            counts.summary(),
            "临时 1 · 缓存 1 · 备份 1 · 系统 1 · 安装包 1 · 冷门 1 · 隐藏 1"
        );
        // 计数数组长度必须与 Bucket::ALL 一致：新增桶时不会静默漏计。
        assert_eq!(counts.counts.len(), Bucket::ALL.len());
    }

    #[test]
    fn debounce_does_not_reset_every_frame() {
        let mut seen = String::new();
        let mut pending: Option<Instant> = None;
        // 首次看到新输入：只安排计时，不立即检索
        assert!(!debounce_due("abc", "", &mut seen, &mut pending, false));
        assert!(pending.is_some());
        // 之后每一帧 query 不变：计时不被重置（旧实现这里会一直重置）
        std::thread::sleep(Duration::from_millis(200));
        assert!(!debounce_due("abc", "", &mut seen, &mut pending, false));
        std::thread::sleep(Duration::from_millis(120));
        assert!(debounce_due("abc", "", &mut seen, &mut pending, false));
        // 回车立即触发，不等防抖
        assert!(debounce_due("abcd", "", &mut seen, &mut pending, true));
        // 输入回到已生效的值：不需要再检索，也不能留下待触发的计时
        let mut pending: Option<Instant> = Some(Instant::now());
        assert!(!debounce_due("abc", "abc", &mut seen, &mut pending, false));
        assert!(pending.is_none());
    }

    #[test]
    fn column_widths_persist_to_disk() {
        let path = std::env::temp_dir().join(format!(
            "diskintel-search-columns-{}.json",
            std::process::id()
        ));
        let widths = ColumnWidths {
            name: 0.28,
            size: 0.09,
            mtime: 0.12,
            type_: 0.07,
        }
        .sanitized();
        save_widths_file(&path, widths).expect("写入临时列宽文件应成功");
        let loaded = load_widths_file(&path);
        assert!(approx(loaded.name, widths.name));
        assert!(approx(loaded.size, widths.size));
        assert!(approx(loaded.mtime, widths.mtime));
        assert!(approx(loaded.type_, widths.type_));
        // 收尾：写回默认值（不做删除），避免留下脏数据。
        save_widths_file(&path, ColumnWidths::default()).expect("写回默认列宽应成功");
    }

    #[test]
    fn nudge_columns_keeps_minimums_and_redistributes() {
        let mut widths = ColumnWidths::default();
        let layout = TableLayout::build(1200.0, &widths);
        let name_before = layout.name_px;
        let path_before = layout.path_px;
        nudge_columns(&mut widths, ColKind::Name, 60.0, &layout);
        let widened = TableLayout::build(1200.0, &widths);
        assert!(widened.name_px > name_before + 50.0);
        assert!(widened.path_px < path_before - 50.0);
        assert!(widened.path_px >= PATH_MIN - 0.01);

        // 一直往右拖：路径列不会低于最小宽度，总宽也不会溢出。
        let mut widths = ColumnWidths::default();
        for _ in 0..200 {
            let layout = TableLayout::build(1200.0, &widths);
            nudge_columns(&mut widths, ColKind::Name, 40.0, &layout);
        }
        let layout = TableLayout::build(1200.0, &widths);
        assert!(layout.path_px >= PATH_MIN - 0.01);
        assert!(layout.width <= 1200.5);

        // 大小/时间分隔条：两列之和不变，路径不受影响。
        let mut widths = ColumnWidths::default();
        let layout = TableLayout::build(1200.0, &widths);
        let sum_before = layout.size_px + layout.mtime_px;
        let path_before = layout.path_px;
        nudge_columns(&mut widths, ColKind::Size, 25.0, &layout);
        let after = TableLayout::build(1200.0, &widths);
        assert!(approx(after.size_px + after.mtime_px, sum_before));
        assert!(after.size_px > layout.size_px);
        assert!(approx(after.path_px, path_before));
    }

    #[test]
    fn classify_flags_temp_and_system_paths() {
        let temp = low_value_bucket(r"C:\Users\me\AppData\Local\Temp\a.tmp", false);
        assert_eq!(temp.map(|bucket| bucket.label()), Some("临时"));
        let system = low_value_bucket(r"C:\Windows\System32\kernel32.dll", false);
        assert_eq!(system.map(|bucket| bucket.label()), Some("系统"));
        let normal = low_value_bucket(r"D:\项目\报告.docx", false);
        assert!(normal.is_none());
    }

    #[test]
    fn rare_files_are_folded_with_chinese_label() {
        // 冷门扩展名必须在 UI 侧被折叠（bucket = Some），不能只靠 lowvalue 单测。
        let rare = low_value_bucket(r"D:\资料\导出.pnf", false);
        assert_eq!(rare, Some(Bucket::Rare));
        assert_eq!(rare.map(|bucket| bucket.label()), Some("冷门"));
        // 展开态的 tooltip / 淡色标记用同一个 label：这里验证文案不会退化成英文。
        assert_eq!(
            format!("低价值分类：{}", rare.unwrap().label()),
            "低价值分类：冷门"
        );
        // 无扩展名文件与目录不折叠。
        assert_eq!(low_value_bucket(r"D:\资料\README", false), None);
        assert_eq!(low_value_bucket(r"D:\资料\build.pnf", true), None);
        // 常用文档仍保持可见（「报告」查询的关键反例）。
        assert_eq!(low_value_bucket(r"D:\资料\季度报告.docx", false), None);
    }
}

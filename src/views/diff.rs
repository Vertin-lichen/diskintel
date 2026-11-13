//! 对比视图：任意两份快照的新增 / 消失 / 增长 / 缩小。
//!
//! 数据口径与 Web 版一致（diff_snapshots 的 merge 扫描）：只统计文件（is_dir=0），
//! 目录大小是聚合值、会与子文件变化重复计入，故不参与四类差异与净变化。
//! 归并在后台线程执行：rusqlite 对旧/新两库做 ATTACH（b=旧、c=新），
//! 分 5 条 SQL 走主键索引（added / removed / 共有变化计数 / 各类 Top-N 归并），
//! 实测比单条 FULL OUTER JOIN（19 s）快得多；UI 只收 mpsc 结果，对比期间保持可交互。

use std::sync::mpsc::Receiver;
use std::time::Instant;

use eframe::egui::{self, Align2, Color32, CornerRadius, FontId, Rect, RichText, Sense, Ui, Vec2};

use crate::db;
use crate::theme::{self, Palette};

/// 每次对比保留的变化最大的条数（与 Web 版 limit=300 对齐）。
pub const TOP_LIMIT: u64 = 300;

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum DiffKind {
    Added,
    Removed,
    Grown,
    Shrunk,
}

impl DiffKind {
    pub fn label(self) -> &'static str {
        match self {
            DiffKind::Added => "新增",
            DiffKind::Removed => "消失",
            DiffKind::Grown => "增长",
            DiffKind::Shrunk => "缩小",
        }
    }
    pub fn color(self, pal: &Palette) -> Color32 {
        match self {
            DiffKind::Added | DiffKind::Grown => pal.ok,
            DiffKind::Removed | DiffKind::Shrunk => pal.warn,
        }
    }
}

#[derive(Clone, Debug)]
pub struct DiffRow {
    pub kind: DiffKind,
    pub path: String,
    pub old_size: i64,
    pub new_size: i64,
}

impl DiffRow {
    pub fn delta(&self) -> i64 {
        self.new_size - self.old_size
    }
}

#[derive(Clone, Copy, Debug, Default)]
pub struct DiffCounts {
    pub added: u64,
    pub removed: u64,
    pub grown: u64,
    pub shrunk: u64,
}

impl DiffCounts {
    pub fn total(&self) -> u64 {
        self.added + self.removed + self.grown + self.shrunk
    }
}

/// 一次对比的完整结果（后台线程算完一次性送回 UI）。
#[derive(Clone, Debug)]
pub struct DiffResult {
    pub old: String,
    pub new: String,
    pub counts: DiffCounts,
    pub net_delta: i64,
    pub rows: Vec<DiffRow>,
    pub elapsed_ms: f64,
    /// 非空表示计算失败（错误信息），其余字段为默认值。
    pub error: String,
}

impl DiffResult {
    pub fn is_err(&self) -> bool {
        !self.error.is_empty()
    }
}

enum Phase {
    Idle,
    Running,
    Failed(String),
}

pub struct DiffState {
    pub old: String,
    pub new: String,
    phase: Phase,
    rx: Option<Receiver<DiffResult>>,
    pub result: Option<DiffResult>,
    /// 最近一次结果到达的时刻；其后 3 秒内保持 16fps 重绘，供截图/动画收尾。
    done_at: Option<Instant>,
}

impl DiffState {
    pub fn new(snapshots: &[db::SnapshotFile], snap: &db::Snapshot) -> Self {
        // 默认 A=上一份、B=当前/最新快照。
        let newest = snapshots.first().map(|file| file.name.clone());
        let new = if snap.name != "-" { snap.name.clone() } else { newest.unwrap_or_default() };
        // 默认 A：优先挑「来源指纹不同」的最近一份 —— 同一份 EFU 重复入库的两份快照 diff 必为零，
        // 直接拿它们做默认会让用户误以为对比坏了。
        let new_sha = snapshots
            .iter()
            .find(|file| file.name == new)
            .map(|file| file.source_sha256.clone())
            .unwrap_or_default();
        let old = snapshots
            .iter()
            .find(|file| file.name != new && !file.source_sha256.is_empty() && file.source_sha256 != new_sha)
            // 没有任何「指纹不同」的快照时，取最老的一份（差异概率最大）
            .or_else(|| snapshots.last().filter(|file| file.name != new))
            .or_else(|| snapshots.iter().find(|file| file.name != new))
            .map(|file| file.name.clone())
            .unwrap_or_default();
        let mut state = Self {
            old,
            new,
            phase: Phase::Idle,
            rx: None,
            result: None,
            done_at: None,
        };
        state
    }

    /// 首次进入对比视图时才开算：两个 1.5 GB 库的 ATTACH 不该在启动时发生。
    pub fn ensure_started(&mut self, snapshots: &[db::SnapshotFile]) {
        if matches!(self.phase, Phase::Idle) {
            self.start(snapshots);
        }
    }

    /// 是否正在计算（状态栏 / 截图自检用）。
    pub fn busy(&self) -> bool {
        matches!(self.phase, Phase::Running)
    }

    fn path_of(snapshots: &[db::SnapshotFile], name: &str) -> Option<std::path::PathBuf> {
        snapshots
            .iter()
            .find(|file| file.name == name)
            .map(|file| file.path.clone())
    }

    /// 在后台线程上跑一次对比；结果通过 mpsc 送回，UI 轮询。
    pub fn start(&mut self, snapshots: &[db::SnapshotFile]) {
        let (Some(old_path), Some(new_path)) = (
            Self::path_of(snapshots, &self.old),
            Self::path_of(snapshots, &self.new),
        ) else {
            self.phase = Phase::Failed("需要两份快照才能对比（快照不足两份）".to_owned());
            return;
        };
        if old_path == new_path {
            self.phase = Phase::Failed("A 与 B 是同一份快照，请先更换其一".to_owned());
            return;
        }
        self.result = None;
        self.phase = Phase::Running;
        let (tx, rx) = std::sync::mpsc::channel();
        let old_name = self.old.clone();
        let new_name = self.new.clone();
        std::thread::spawn(move || {
            let outcome = compute_diff(&old_path, &new_path, old_name, new_name);
            let _ = tx.send(outcome);
        });
        self.rx = Some(rx);
    }

    fn poll(&mut self) {
        let Some(rx) = &self.rx else { return };
        let mut received: Option<DiffResult> = None;
        while let Ok(result) = rx.try_recv() {
            received = Some(result);
        }
        if let Some(result) = received {
            eprintln!(
                "对比完成 {} -> {}　added {} removed {} grown {} shrunk {} net {}　top {} 行　{:.1} ms",
                result.old,
                result.new,
                result.counts.added,
                result.counts.removed,
                result.counts.grown,
                result.counts.shrunk,
                result.net_delta,
                result.rows.len(),
                result.elapsed_ms
            );
            self.rx = None;
            self.done_at = Some(Instant::now());
            if result.is_err() {
                self.phase = Phase::Failed(result.error.clone());
            }
            self.result = Some(result);
        }
    }

    pub fn show(&mut self, ui: &mut Ui, pal: &Palette, snapshots: &[db::SnapshotFile]) {
        self.poll();
        let busy = matches!(self.phase, Phase::Running)
            || self.done_at.map(|at| at.elapsed() < std::time::Duration::from_secs(3)).unwrap_or(false);
        if busy {
            ui.ctx().request_repaint_after(std::time::Duration::from_millis(60));
        }
        self.controls(ui, pal, snapshots);
        ui.add_space(6.0);

        let failed = match &self.phase {
            Phase::Failed(message) => Some(message.clone()),
            _ => None,
        };
        if let Some(message) = failed {
            crate::widgets::empty_state(ui, pal, "无法对比", &message);
            return;
        }
        if matches!(self.phase, Phase::Running) && self.result.is_none() {
            ui.add_space(28.0);
            ui.vertical_centered(|ui| {
                // 不用 ui.spinner()：它每帧强制 request_repaint，会打乱本视图的
                // 16fps 节流（截图自检靠帧计数定时，帧率必须可预期）。
                let (rect, _) = ui.allocate_exact_size(Vec2::splat(22.0), Sense::hover());
                ui.painter().circle_stroke(
                    rect.center(),
                    8.0,
                    egui::Stroke::new(2.0, pal.accent),
                );
                ui.add_space(6.0);
                ui.label(
                    RichText::new(format!("正在对比 {} → {} …", self.old, self.new))
                        .size(13.0)
                        .color(pal.text_dim),
                );
            });
            return;
        }
        let Some(result) = self.result.clone() else {
            crate::widgets::empty_state(
                ui,
                pal,
                "选择两份快照开始对比",
                "上方选 A（旧）与 B（新），点击「对比」查看新增 / 消失 / 增长 / 缩小。",
            );
            return;
        };
        self.summary(ui, pal, &result);
        ui.add_space(6.0);
        if result.rows.is_empty() {
            crate::widgets::empty_state(
                ui,
                pal,
                "两份快照没有文件级差异",
                "两次扫描之间磁盘上的文件（含大小）没有任何变化。",
            );
        } else {
            self.table(ui, pal, &result);
        }
    }

    /// 顶部控制条：A 下拉 / 交换 / B 下拉 / 对比 / 最近两份。
    fn controls(&mut self, ui: &mut Ui, pal: &Palette, snapshots: &[db::SnapshotFile]) {
        let height = theme::FILTERBAR_H + 8.0;
        let (rect, _) =
            ui.allocate_exact_size(Vec2::new(ui.available_width(), height), Sense::hover());
        let mut child = ui.new_child(
            egui::UiBuilder::new()
                .max_rect(rect.shrink2(Vec2::new(0.0, 5.0)))
                .layout(egui::Layout::left_to_right(egui::Align::Center)),
        );
        child.spacing_mut().item_spacing.x = 6.0;
        let running = matches!(self.phase, Phase::Running);

        child.label(RichText::new("A 旧").size(11.5).color(pal.text_dim));
        let mut old_pick = self.old.clone();
        egui::ComboBox::from_id_salt("diff-old")
            .selected_text(
                RichText::new(if old_pick.is_empty() { "-" } else { old_pick.as_str() }).size(12.0),
            )
            .width(158.0)
            .show_ui(&mut child, |ui| {
                for file in snapshots {
                    ui.selectable_value(&mut old_pick, file.name.clone(), file.name.as_str());
                }
            });

        let swap = crate::widgets::icon_button(
            &mut child,
            pal,
            crate::icons::Glyph::Refresh,
            24.0,
            "交换 A / B",
        );

        child.label(RichText::new("B 新").size(11.5).color(pal.text_dim));
        let mut new_pick = self.new.clone();
        egui::ComboBox::from_id_salt("diff-new")
            .selected_text(
                RichText::new(if new_pick.is_empty() { "-" } else { new_pick.as_str() }).size(12.0),
            )
            .width(158.0)
            .show_ui(&mut child, |ui| {
                for file in snapshots {
                    ui.selectable_value(&mut new_pick, file.name.clone(), file.name.as_str());
                }
            });

        let compare = crate::widgets::primary_button(
            &mut child,
            pal,
            if running {
                "对比中…"
            } else if self.result.is_some() {
                "重新对比"
            } else {
                "开始对比"
            },
            Some(crate::icons::Glyph::Play),
        );
        let latest_two = crate::widgets::pill_button(
            &mut child,
            pal,
            "最近两份",
            Some(crate::icons::Glyph::Clock),
            false,
        );
        if let Some(result) = &self.result {
            child.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                ui.label(
                    RichText::new(format!("SQL {:.2} s", result.elapsed_ms / 1000.0))
                        .size(11.0)
                        .color(pal.text_faint),
                );
            });
        }

        if swap.clicked() && !self.old.is_empty() && !self.new.is_empty() {
            std::mem::swap(&mut self.old, &mut self.new);
            self.start(snapshots);
            return;
        }
        if latest_two.clicked() && snapshots.len() >= 2 {
            self.old = snapshots[1].name.clone();
            self.new = snapshots[0].name.clone();
            self.start(snapshots);
            return;
        }
        if old_pick != self.old {
            self.old = old_pick;
        }
        if new_pick != self.new {
            self.new = new_pick;
        }
        if compare.clicked() {
            self.start(snapshots);
        }
    }

    /// 摘要条：old → new　新增 a / 消失 r / 增长 g / 缩小 s　净变化 ±X。
    fn summary(&self, ui: &mut Ui, pal: &Palette, result: &DiffResult) {
        let (rect, _) = ui.allocate_exact_size(
            Vec2::new(ui.available_width(), theme::FILTERBAR_H),
            Sense::hover(),
        );
        ui.painter()
            .rect_filled(rect, CornerRadius::same(theme::RADIUS_SM), pal.accent_weak);
        let font = FontId::proportional(12.5);
        let mono = FontId::monospace(12.0);
        let mut x = rect.left() + 10.0;
        let mid = rect.center().y;
        let mut put = |ui: &mut Ui, text: String, color: Color32, font: FontId, gap: f32| {
            let galley = ui.painter().layout_no_wrap(text, font, color);
            ui.painter()
                .galley(egui::pos2(x, mid - galley.size().y / 2.0), galley.clone(), color);
            x += galley.size().x + gap;
        };
        put(ui, format!("{} → {}", result.old, result.new), pal.text, font.clone(), 14.0);
        put(ui, "新增".to_owned(), pal.text_dim, font.clone(), 4.0);
        put(ui, db::thousands(result.counts.added), pal.ok, mono.clone(), 12.0);
        put(ui, "消失".to_owned(), pal.text_dim, font.clone(), 4.0);
        put(ui, db::thousands(result.counts.removed), pal.warn, mono.clone(), 12.0);
        put(ui, "增长".to_owned(), pal.text_dim, font.clone(), 4.0);
        put(ui, db::thousands(result.counts.grown), pal.ok, mono.clone(), 12.0);
        put(ui, "缩小".to_owned(), pal.text_dim, font.clone(), 4.0);
        put(ui, db::thousands(result.counts.shrunk), pal.warn, mono.clone(), 12.0);
        put(ui, "净变化".to_owned(), pal.text_dim, font.clone(), 4.0);
        let (sign, tint) = if result.net_delta >= 0 {
            ("+", pal.ok)
        } else {
            ("−", pal.warn)
        };
        put(
            ui,
            format!("{sign}{}", db::human_size(result.net_delta.abs())),
            tint,
            mono,
            12.0,
        );
        let tail = format!("耗时 {:.2} s", result.elapsed_ms / 1000.0);
        let galley = ui
            .painter()
            .layout_no_wrap(tail, FontId::proportional(11.0), pal.text_faint);
        ui.painter().galley(
            egui::pos2(rect.right() - 10.0 - galley.size().x, mid - galley.size().y / 2.0),
            galley,
            pal.text_faint,
        );
    }

    fn table(&mut self, ui: &mut Ui, pal: &Palette, result: &DiffResult) {
        let (header_rect, _) = ui.allocate_exact_size(
            Vec2::new(ui.available_width(), theme::HEADER_H),
            Sense::hover(),
        );
        paint_diff_header(ui, pal, header_rect);

        let rows: &Vec<DiffRow> = &result.rows;
        let total = result.counts.total();
        egui::ScrollArea::vertical()
            .auto_shrink([false, false])
            .show_rows(ui, theme::ROW_H, rows.len(), |ui, range| {
                for (vis, index) in range.enumerate() {
                    let row = &rows[index];
                    let (rect, response) = ui.allocate_exact_size(
                        Vec2::new(ui.available_width(), theme::ROW_H),
                        Sense::click(),
                    );
                    paint_diff_row(ui, pal, rect, vis, row);
                    response.context_menu(|ui| {
                        if ui.button("复制路径").clicked() {
                            ui.ctx().copy_text(row.path.clone());
                            ui.close();
                        }
                        if ui.button("打开所在目录").clicked() {
                            let dir =
                                db::parent_dir(&row.path).unwrap_or_else(|| row.path.clone());
                            let _ = std::process::Command::new("explorer").arg(&dir).spawn();
                            ui.close();
                        }
                    });
                }
            });
        ui.add_space(4.0);
        let note = if total > rows.len() as u64 {
            format!(
                "仅显示变化最大的 {} 条（共 {} 条）；完整结果请用 CLI 导出：diskintel diff {} {} --json",
                rows.len(),
                db::thousands(total),
                result.old,
                result.new,
            )
        } else {
            format!("共 {} 条变化", db::thousands(total))
        };
        ui.label(RichText::new(note).size(11.0).color(pal.text_faint));
    }
}

// ---------- 后台计算 ----------

/// 归并两库：main 为内存库，ATTACH 旧库为 b、新库为 c。
/// 四类差异只看文件（is_dir=0）；目录大小是聚合值、会与子文件变化重复计入。
/// added / removed 用反连接（NOT EXISTS，走 b/c 的 path 主键）；
/// 共有变化用内连接按主键归并；Top-N 先各类取 300 再全局按 |变化量| 排序。
fn compute_diff(
    old_path: &std::path::Path,
    new_path: &std::path::Path,
    old_name: String,
    new_name: String,
) -> DiffResult {
    let started = Instant::now();
    // 快路径：复用 json_cli 里的「两侧 ORDER BY path 游标归并」实现（实测 2.9 s vs 分步 SQL 20+ s）。
    if let Some(result) = fast_diff(old_path, &old_name, &new_name, &started) {
        return result;
    }
    let mut fail = |message: String| DiffResult {
        old: old_name.clone(),
        new: new_name.clone(),
        counts: DiffCounts::default(),
        net_delta: 0,
        rows: Vec::new(),
        elapsed_ms: started.elapsed().as_secs_f64() * 1000.0,
        error: message,
    };
    let conn = match rusqlite::Connection::open_in_memory() {
        Ok(conn) => conn,
        Err(error) => return fail(format!("打开内存库失败：{error}")),
    };
    let attach = |name: &str, path: &std::path::Path| -> Result<(), String> {
        let quoted = path.display().to_string().replace('\'', "''");
        conn.execute(&format!("ATTACH DATABASE '{quoted}' AS {name}"), [])
            .map(|_| ())
            .map_err(|error| error.to_string())
    };
    if let Err(message) = attach("b", old_path) {
        return fail(format!("ATTACH 旧库失败：{message}"));
    }
    if let Err(message) = attach("c", new_path) {
        return fail(format!("ATTACH 新库失败：{message}"));
    }
    // 两个 1.5 GB 库各自 mmap 256 MB 会把 RSS 推到 600 MB；各 64 MB 足够（实测查询耗时不变）。
    let _ = conn.execute_batch(
        "PRAGMA b.mmap_size=67108864; PRAGMA c.mmap_size=67108864; \
         PRAGMA b.cache_size=-16384; PRAGMA c.cache_size=-16384;",
    );

    let scalar2 = |sql: &str| -> (i64, i64) {
        conn.query_row(sql, [], |row| {
            Ok((row.get::<_, i64>(0)?, row.get::<_, i64>(1)?))
        })
        .unwrap_or((0, 0))
    };

    // 新增：c 有 b 无
    let (added, added_bytes) = scalar2(
        "SELECT COUNT(*), COALESCE(SUM(x.size),0) \
         FROM c.entries x \
         WHERE x.is_dir = 0 \
           AND NOT EXISTS (SELECT 1 FROM b.entries y WHERE y.path = x.path)",
    );
    // 消失：b 有 c 无
    let (removed, removed_bytes) = scalar2(
        "SELECT COUNT(*), COALESCE(SUM(x.size),0) \
         FROM b.entries x \
         WHERE x.is_dir = 0 \
           AND NOT EXISTS (SELECT 1 FROM c.entries y WHERE y.path = x.path)",
    );
    // 共有且大小变化：增长数 / 缩小数 / 净变化
    let (grown, shrunk, changed_net) = conn
        .query_row(
            "SELECT COALESCE(SUM(CASE WHEN c.size > b.size THEN 1 ELSE 0 END),0), \
                    COALESCE(SUM(CASE WHEN c.size < b.size THEN 1 ELSE 0 END),0), \
                    COALESCE(SUM(c.size - b.size),0) \
             FROM b.entries b JOIN c.entries c ON c.path = b.path \
             WHERE b.is_dir = 0 AND c.is_dir = 0 AND c.size <> b.size",
            [],
            |row| {
                Ok((
                    row.get::<_, i64>(0)?,
                    row.get::<_, i64>(1)?,
                    row.get::<_, i64>(2)?,
                ))
            },
        )
        .unwrap_or((0, 0, 0));

    let counts = DiffCounts {
        added: added.max(0) as u64,
        removed: removed.max(0) as u64,
        grown: grown.max(0) as u64,
        shrunk: shrunk.max(0) as u64,
    };
    let net_delta = added_bytes - removed_bytes + changed_net;

    // 变化幅度最大的前 N 条：各类先取 Top-N（正确性：全局 Top-N ⊆ 各类 Top-N 的并集），
    // 再全局按 |变化量| 降序取前 N。SQL 已排好序，UI 不再排序。
    let top_sql = format!(
        "SELECT k, path, old_size, new_size FROM ( \
           SELECT * FROM ( \
             SELECT 'added' AS k, x.path AS path, 0 AS old_size, x.size AS new_size \
             FROM c.entries x \
             WHERE x.is_dir = 0 \
               AND NOT EXISTS (SELECT 1 FROM b.entries y WHERE y.path = x.path) \
             ORDER BY x.size DESC LIMIT {TOP_LIMIT} \
           ) \
           UNION ALL SELECT * FROM ( \
             SELECT 'removed', x.path, x.size, 0 \
             FROM b.entries x \
             WHERE x.is_dir = 0 \
               AND NOT EXISTS (SELECT 1 FROM c.entries y WHERE y.path = x.path) \
             ORDER BY x.size DESC LIMIT {TOP_LIMIT} \
           ) \
           UNION ALL SELECT * FROM ( \
             SELECT CASE WHEN c.size > b.size THEN 'grown' ELSE 'shrunk' END, \
                    b.path, b.size, c.size \
             FROM b.entries b JOIN c.entries c ON c.path = b.path \
             WHERE b.is_dir = 0 AND c.is_dir = 0 AND c.size <> b.size \
             ORDER BY ABS(c.size - b.size) DESC LIMIT {TOP_LIMIT} \
           ) \
         ) ORDER BY ABS(new_size - old_size) DESC LIMIT {TOP_LIMIT}"
    );
    let mut rows: Vec<DiffRow> = Vec::new();
    if let Ok(mut stmt) = conn.prepare(&top_sql) {
        let mapped = stmt.query_map([], |row| {
            let kind = match row.get::<_, String>(0)?.as_str() {
                "added" => DiffKind::Added,
                "removed" => DiffKind::Removed,
                "grown" => DiffKind::Grown,
                _ => DiffKind::Shrunk,
            };
            Ok(DiffRow {
                kind,
                path: row.get(1)?,
                old_size: row.get(2)?,
                new_size: row.get(3)?,
            })
        });
        if let Ok(mapped) = mapped {
            rows.extend(mapped.flatten());
        }
    }

    DiffResult {
        old: old_name,
        new: new_name,
        counts,
        net_delta,
        rows,
        elapsed_ms: started.elapsed().as_secs_f64() * 1000.0,
        error: String::new(),
    }
}

// ---------- 表格绘制 ----------

struct DiffCol {
    x: f32,
    w: f32,
}

fn diff_columns(rect: Rect) -> Vec<DiffCol> {
    // 类型 56 / 变化量 96 / 旧 88 / 新 88 / 路径 占余量（对齐 Web 版第 4.1 节对比表）
    let gap = 10.0;
    let fixed = 56.0 + 96.0 + 88.0 + 88.0 + gap * 4.0;
    let path_w = (rect.width() - fixed).max(160.0);
    let mut x = rect.left() + 2.0;
    let widths = [56.0, 96.0, 88.0, 88.0, path_w];
    widths
        .iter()
        .map(|w| {
            let col = DiffCol { x, w: *w };
            x += w + gap;
            col
        })
        .collect()
}

fn paint_diff_header(ui: &mut Ui, pal: &Palette, rect: Rect) {
    let painter = ui.painter();
    painter.rect_filled(rect, CornerRadius::same(0), pal.panel_alt);
    painter.hline(
        rect.x_range(),
        rect.bottom() - 0.5,
        egui::Stroke::new(1.0, pal.border),
    );
    let font = FontId::proportional(11.0);
    for (col, name) in diff_columns(rect).iter().zip(["类型", "变化量", "旧大小", "新大小", "路径"]) {
        let numeric = name != "类型" && name != "路径";
        let pos = if numeric {
            egui::pos2(col.x + col.w - 4.0, rect.center().y)
        } else {
            egui::pos2(col.x + 4.0, rect.center().y)
        };
        let align = if numeric { Align2::RIGHT_CENTER } else { Align2::LEFT_CENTER };
        painter.text(pos, align, name, font.clone(), pal.text_dim);
    }
}

fn paint_diff_row(ui: &mut Ui, pal: &Palette, rect: Rect, index: usize, row: &DiffRow) {
    let painter = ui.painter();
    let fill = pal.row_fill(index, false, false);
    if fill != Color32::TRANSPARENT {
        painter.rect_filled(rect, CornerRadius::same(0), fill);
    }
    let cols = diff_columns(rect);
    let kind_color = row.kind.color(pal);
    let mono = FontId::monospace(11.5);
    let mid = rect.center().y;

    painter.text(
        egui::pos2(cols[0].x + 4.0, mid),
        Align2::LEFT_CENTER,
        row.kind.label(),
        FontId::proportional(11.5),
        kind_color,
    );
    let delta = row.delta();
    let (sign, tint) = if delta >= 0 { ("+", pal.ok) } else { ("−", pal.warn) };
    painter.text(
        egui::pos2(cols[1].x + cols[1].w - 4.0, mid),
        Align2::RIGHT_CENTER,
        format!("{sign}{}", db::human_size(delta.abs())),
        mono.clone(),
        tint,
    );
    let old_text = if row.old_size == 0 && row.kind == DiffKind::Added {
        "—".to_owned()
    } else {
        db::human_size(row.old_size)
    };
    painter.text(
        egui::pos2(cols[2].x + cols[2].w - 4.0, mid),
        Align2::RIGHT_CENTER,
        old_text,
        mono.clone(),
        pal.text_dim,
    );
    let new_text = if row.new_size == 0 && row.kind == DiffKind::Removed {
        "—".to_owned()
    } else {
        db::human_size(row.new_size)
    };
    painter.text(
        egui::pos2(cols[3].x + cols[3].w - 4.0, mid),
        Align2::RIGHT_CENTER,
        new_text,
        mono.clone(),
        pal.text_dim,
    );
    // 路径：图标 + 单行省略
    let kind = crate::icons::classify(&row.path, false);
    let icon_rect = Rect::from_center_size(egui::pos2(cols[4].x + 10.0, mid), Vec2::splat(15.0));
    crate::icons::paint_file(painter, icon_rect, kind);
    let text_rect = Rect::from_min_max(
        egui::pos2(cols[4].x + 22.0, rect.top()),
        egui::pos2(cols[4].x + cols[4].w, rect.bottom()),
    );
    let font = FontId::proportional(12.0);
    let galley = painter.layout_no_wrap(row.path.clone(), font.clone(), pal.text);
    let elided = if galley.size().x <= text_rect.width() {
        galley
    } else {
        let chars: Vec<char> = row.path.chars().collect();
        let mut low = 0usize;
        let mut high = chars.len();
        while low < high {
            let middle = (low + high + 1) / 2;
            let mut candidate: String = chars[..middle].iter().collect();
            candidate.push('…');
            if painter.layout_no_wrap(candidate, font.clone(), pal.text).size().x <= text_rect.width() {
                low = middle;
            } else {
                high = middle - 1;
            }
        }
        let mut candidate: String = chars[..low].iter().collect();
        candidate.push('…');
        painter.layout_no_wrap(candidate, font, pal.text)
    };
    painter.galley(
        egui::pos2(text_rect.left(), mid - elided.size().y / 2.0),
        elided,
        pal.text,
    );
}
/// 用 json_cli 的流式归并算一次对比；失败（老快照无 path 索引等）返回 None，交给分步 SQL 兜底。
fn fast_diff(
    old_path: &std::path::Path,
    old_name: &str,
    new_name: &str,
    started: &Instant,
) -> Option<DiffResult> {
    let data_root = old_path.parent()?.parent()?;
    let json = crate::json_cli::diff(data_root, Some(old_name), Some(new_name), TOP_LIMIT as i64, "", "all")
        .ok()?;
    let counts = json.get("counts")?;
    let int_of = |value: Option<&crate::json_cli::Json>| -> i64 {
        match value {
            Some(crate::json_cli::Json::Int(v)) => *v,
            _ => 0,
        }
    };
    let counts = DiffCounts {
        added: int_of(counts.get("added")).max(0) as u64,
        removed: int_of(counts.get("removed")).max(0) as u64,
        grown: int_of(counts.get("grown")).max(0) as u64,
        shrunk: int_of(counts.get("shrunk")).max(0) as u64,
    };
    let net_delta = int_of(json.get("total_delta"));
    let mut rows = Vec::new();
    if let Some(crate::json_cli::Json::Arr(items)) = json.get("changes") {
        for item in items {
            let kind = match item.get("kind").and_then(|v| v.as_str()) {
                Some("added") => DiffKind::Added,
                Some("removed") => DiffKind::Removed,
                Some("grown") => DiffKind::Grown,
                Some("shrunk") => DiffKind::Shrunk,
                _ => continue,
            };
            rows.push(DiffRow {
                kind,
                path: item.get("path").and_then(|v| v.as_str()).unwrap_or("").to_owned(),
                old_size: int_of(item.get("old_size")),
                new_size: int_of(item.get("new_size")),
            });
        }
    }
    Some(DiffResult {
        old: old_name.to_owned(),
        new: new_name.to_owned(),
        counts,
        net_delta,
        rows,
        elapsed_ms: started.elapsed().as_secs_f64() * 1000.0,
        error: String::new(),
    })
}
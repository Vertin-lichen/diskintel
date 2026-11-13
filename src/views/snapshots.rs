//! 快照管理：快照目录、列表、体积合计、磁盘水位、删除（移入待删除）与可编辑备注。
//!
//! 两条硬约束决定了这里的实现方式：
//! - 快照库一律只读打开，备注不能写进 meta 表，落在**快照目录**里的 notes.json（store::notes）；
//! - 项目公约不永久删除，删除 = 交给 store::prune 移入待删除并登记 MANIFEST.md。
//!
//! 快照目录可以自定义（<data_root>\snapshots 之外的盘也行）：目录由外壳通过 configure
//! 注入，没有注入时按快照文件路径反推。磁盘剩余空间用 GetDiskFreeSpaceExW 查快照所在卷。

use std::collections::HashMap;
use std::path::{Path, PathBuf};

use eframe::egui::{self, Align2, Color32, CornerRadius, FontId, Rect, RichText, Sense, Ui, Vec2};

use crate::db;
use crate::snapshot_policy;
use crate::store::{notes, prune};
use crate::theme::{self, Palette};

/// 备注最多接受的字符数：拦住粘贴整篇文档，界面上也只放得下一行。
const NOTE_MAX_CHARS: usize = 200;

/// 删除原因 / 操作者，原样写进待删除目录的 MANIFEST.md。
const DELETE_REASON: &str = "快照管理页手动删除";
const DELETE_OPERATOR: &str = "UI";

/// 快照行在列表之外补充的 meta 信息（列表扫描时一次读出）。
#[derive(Clone, Debug, Default)]
struct SnapMeta {
    note: String,
    total_size: i64,
    format_version: i64,
}

/// 行内备注的编辑态：正在编辑哪一份快照、当前的编辑缓冲、是否还要抢一次焦点。
struct NoteEdit {
    name: String,
    buffer: String,
    focus: bool,
}

/// 本帧备注编辑的结局（帧末统一落地，滚动区里只读）。
enum EditOutcome {
    Keep(String),
    Commit(String),
    Cancel,
}

/// 行动作的返回值：由外壳（app.rs）落地。
#[derive(Clone, Debug)]
pub enum SnapshotsAction {
    /// 切换当前快照（快照名）。
    Switch(String),
    /// 一份快照已经移入待删除（快照名）：外壳要把它从自己的列表里去掉并提示。
    Deleted(String),
    /// 已复制路径后的提示文案。
    Toast(String),
}

pub struct SnapshotsState {
    pub files: Vec<db::SnapshotFile>,
    meta: HashMap<String, SnapMeta>,
    pub total_bytes: u64,
    pub free_bytes: u64,
    pub disk_total: u64,
    pub current: String,
    /// 备注（快照名 → 文本），来自 <data_root>\snapshots\notes.json。
    notes: HashMap<String, String>,
    /// 正在内联编辑的备注。
    editing: Option<NoteEdit>,
    /// 点了「删除」、正等二次确认的快照名。
    pending_delete: Option<String>,
    /// 删除 / 保存备注失败的错误行（不静默）。
    error: Option<String>,
    /// 数据根目录（默认布局下待删除就放在它下面）。
    data_root: Option<PathBuf>,
    /// 当前快照目录（可自定义；没注入时按快照文件路径反推）。
    snapshot_dir: Option<PathBuf>,
    /// 策略提示（每 N 天 / 保留 K 份），只用于文案，不参与决策。
    policy_hint: Option<(u64, usize)>,
}

impl SnapshotsState {
    pub fn new(files: &[db::SnapshotFile], snap: &db::Snapshot) -> Self {
        let mut meta = HashMap::new();
        for file in files {
            meta.insert(file.name.clone(), read_meta(&file.path));
        }
        let total_bytes: u64 = files.iter().map(|file| file.bytes).sum();
        let (free_bytes, disk_total) = files
            .first()
            .map(|file| disk_free(file.path.parent().unwrap_or(&file.path)))
            .unwrap_or((0, 0));
        // 快照目录：文件所在目录（自定义目录时就是它本身）。
        let snapshot_dir = files
            .first()
            .and_then(|file| file.path.parent())
            .map(Path::to_path_buf);
        let data_root = snapshot_dir
            .as_deref()
            .and_then(Path::parent)
            .map(Path::to_path_buf);
        let loaded = snapshot_dir
            .as_deref()
            .map(notes::load_in)
            .unwrap_or_default();
        Self {
            files: files.to_vec(),
            meta,
            total_bytes,
            free_bytes,
            disk_total,
            current: snap.name.clone(),
            notes: loaded,
            editing: None,
            pending_delete: None,
            error: None,
            data_root,
            snapshot_dir,
            policy_hint: None,
        }
    }

    /// 外壳注入：数据根、快照目录（支持自定义目录）与策略文案。
    ///
    /// 应用启动时调一次即可；不调用也能工作（目录从快照文件路径反推）。
    /// 注：接线落地前只有单测调用，先关掉 dead_code 噪声。
    #[allow(dead_code)]
    pub fn configure(
        &mut self,
        data_root: &Path,
        snapshot_dir: &Path,
        every_days: u64,
        keep_count: usize,
    ) {
        self.data_root = Some(data_root.to_path_buf());
        self.snapshot_dir = Some(snapshot_dir.to_path_buf());
        self.policy_hint = if every_days > 0 && keep_count > 0 {
            Some((every_days, keep_count))
        } else {
            None
        };
        self.notes = notes::load_in(snapshot_dir);
        self.refresh_disk();
    }

    /// 实际快照目录（注入值优先，否则从列表反推）。
    fn snapshot_dir(&self) -> Option<PathBuf> {
        self.snapshot_dir.clone().or_else(|| {
            self.files
                .first()
                .and_then(|file| file.path.parent())
                .map(Path::to_path_buf)
        })
    }

    /// 待删除落点（与 store::prune 同一口径：默认布局在数据根，自定义目录就近）。
    fn trash_dir(&self) -> Option<PathBuf> {
        let snapshot_dir = self.snapshot_dir()?;
        let data_root = self
            .data_root
            .clone()
            .or_else(|| snapshot_dir.parent().map(Path::to_path_buf))?;
        Some(snapshot_policy::trash_dir(&snapshot_dir, &data_root))
    }

    /// 备注文本：sidecar 里有的（哪怕已清空）优先，没有才回落到库里只读的 meta.note。
    fn note_of(&self, name: &str, meta: &SnapMeta) -> String {
        match self.notes.get(name) {
            Some(text) => text.clone(),
            None => meta.note.clone(),
        }
    }

    /// 把内存里的备注整体刷到 sidecar（放在快照目录里，跟着快照走）。
    fn save_notes(&self) -> Result<(), String> {
        let Some(dir) = self.snapshot_dir() else {
            return Err("拿不到快照目录（快照路径异常），备注没有保存".to_owned());
        };
        notes::save_in(&dir, &self.notes)
    }

    /// 删除成功后重算磁盘剩余：锚点用快照目录（自定义目录也查得到对的卷）。
    fn refresh_disk(&mut self) {
        let anchor = self
            .snapshot_dir()
            .or_else(|| self.data_root.as_ref().map(|root| root.join("snapshots")));
        let (free_bytes, disk_total) = match anchor {
            Some(dir) => disk_free(&dir),
            None => (0, 0),
        };
        self.free_bytes = free_bytes;
        self.disk_total = disk_total;
    }

    /// 摘要条：快照份数 / 合计库体积 / 磁盘剩余 / 水位提示。
    fn summary(&self, ui: &mut Ui, pal: &Palette) {
        let (rect, _) = ui.allocate_exact_size(
            Vec2::new(ui.available_width(), theme::FILTERBAR_H),
            Sense::hover(),
        );
        ui.painter()
            .rect_filled(rect, CornerRadius::same(theme::RADIUS_SM), pal.accent_weak);
        let mut x = rect.left() + 10.0;
        let mid = rect.center().y;
        let mut put = |ui: &mut Ui, text: String, color: Color32, font: FontId, gap: f32| {
            let galley = ui.painter().layout_no_wrap(text, font, color);
            ui.painter()
                .galley(egui::pos2(x, mid - galley.size().y / 2.0), galley.clone(), color);
            x += galley.size().x + gap;
        };
        let font = FontId::proportional(12.5);
        let mono = FontId::monospace(12.0);
        put(ui, format!("快照 {} 份", self.files.len()), pal.text, font.clone(), 16.0);
        put(ui, "合计库体积".to_owned(), pal.text_dim, font.clone(), 4.0);
        put(ui, db::human_size(self.total_bytes as i64), pal.text, mono.clone(), 16.0);
        put(ui, "磁盘剩余".to_owned(), pal.text_dim, font.clone(), 4.0);
        let free_tint = if self.free_bytes < 30 * 1024 * 1024 * 1024 { pal.warn } else { pal.ok };
        put(
            ui,
            if self.free_bytes > 0 { db::human_size(self.free_bytes as i64) } else { "—".to_owned() },
            free_tint,
            mono.clone(),
            16.0,
        );
        if self.disk_total > 0 {
            let used_pct = ((self.disk_total - self.free_bytes) as f64 / self.disk_total as f64 * 100.0).round() as i64;
            let (water, tint) = if self.free_bytes < 30 * 1024 * 1024 * 1024 {
                ("水位偏低", pal.warn)
            } else {
                ("水位正常", pal.ok)
            };
            put(ui, format!("已用 {used_pct}% · {water}"), tint, font.clone(), 0.0);
        }
        // 右端：当前快照目录 + 「打开目录」（目录可自定义，用户要能看到它到底在哪）
        if let Some(dir) = self.snapshot_dir() {
            let button_width = 68.0;
            let button_rect = Rect::from_min_max(
                egui::pos2(rect.right() - 8.0 - button_width, mid - 9.0),
                egui::pos2(rect.right() - 8.0, mid + 9.0),
            );
            let dir_text = dir.display().to_string();
            let font = FontId::monospace(11.0);
            let avail = (button_rect.left() - 10.0 - x).max(0.0);
            let elided = elide_left(ui.painter(), &dir_text, font.clone(), pal.text_faint, avail);
            let galley = ui.painter().layout_no_wrap(elided, font, pal.text_faint);
            ui.painter().galley(
                egui::pos2(button_rect.left() - 10.0 - galley.size().x, mid - galley.size().y / 2.0),
                galley,
                pal.text_faint,
            );
            let response = ui.interact(
                button_rect,
                ui.scope_id().with("snap-open-dir"),
                Sense::click(),
            );
            if response.hovered() {
                ui.painter()
                    .rect_filled(button_rect, CornerRadius::same(4), pal.hover);
            }
            ui.painter().text(
                button_rect.center(),
                Align2::CENTER_CENTER,
                "打开目录",
                FontId::proportional(11.0),
                pal.text_dim,
            );
            response
                .clone()
                .on_hover_text(format!("当前快照目录：{dir_text}"));
            if response.clicked() {
                open_dir(&dir);
            }
        }
    }

    pub fn show(&mut self, ui: &mut Ui, pal: &Palette, snap: &db::Snapshot) -> Option<SnapshotsAction> {
        if snap.name != self.current {
            self.current = snap.name.clone();
        }
        let mut action = None;
        if self.files.is_empty() {
            let cadence = self
                .policy_hint
                .map(|(days, _)| format!("（自动快照开启后每 {days} 天至少一份）"))
                .unwrap_or_default();
            crate::widgets::empty_state(
                ui,
                pal,
                "还没有快照",
                &format!("点工具栏「扫描」建立第一份索引，或等待自动创建{cadence}；首次安装会立刻建一份。"),
            );
            if let Some(dir) = self.snapshot_dir() {
                ui.add_space(6.0);
                ui.vertical_centered(|ui| {
                    ui.label(
                        RichText::new(format!("快照目录：{}", dir.display()))
                            .size(11.5)
                            .color(pal.text_faint),
                    );
                    if ui.button("打开快照目录").clicked() {
                        open_dir(&dir);
                    }
                });
            }
            return action;
        }
        self.summary(ui, pal);
        ui.add_space(6.0);

        let (header_rect, _) = ui.allocate_exact_size(
            Vec2::new(ui.available_width(), theme::HEADER_H),
            Sense::hover(),
        );
        paint_header(ui, pal, header_rect);

        // 滚动区里只读：交互结果先收集到局部变量，帧末统一落地（避免和 files 的借用打架）。
        let mut delete_request: Option<String> = None;
        let mut open_edit: Option<String> = None;
        let mut edit_outcome: Option<EditOutcome> = None;
        let editing_name = self.editing.as_ref().map(|edit| edit.name.clone());
        let editing_focus = self.editing.as_ref().map(|edit| edit.focus).unwrap_or(false);
        let total_files = self.files.len();

        let compact = is_compact(header_rect.width());
        let files: &Vec<db::SnapshotFile> = &self.files;
        let rows = files.len();
        egui::ScrollArea::vertical()
            .auto_shrink([false, false])
            .show_rows(ui, theme::ROW_H, rows, |ui, range| {
                for (vis, index) in range.enumerate() {
                    let file = &files[index];
                    let meta = self.meta.get(&file.name).cloned().unwrap_or_default();
                    let is_current = file.name == self.current;
                    let (rect, response) = ui.allocate_exact_size(
                        Vec2::new(ui.available_width(), theme::ROW_H),
                        Sense::click(),
                    );
                    paint_row(ui, pal, rect, vis, file, &meta, is_current);
                    response.context_menu(|ui| {
                        if ui.button("复制完整路径").clicked() {
                            ui.ctx().copy_text(file.path.display().to_string());
                            action = Some(SnapshotsAction::Toast(format!(
                                "已复制：{}",
                                file.path.display()
                            )));
                            ui.close();
                        }
                        if ui.button("打开所在目录").clicked() {
                            if let Some(dir) = file.path.parent() {
                                let _ = std::process::Command::new("explorer").arg(dir).spawn();
                            }
                            ui.close();
                        }
                    });

                    // ---- 备注单元格：静态时单击进入编辑，编辑中放一个内联 TextEdit ----
                    let note_rect = note_cell(rect);
                    let is_editing = editing_name.as_deref() == Some(file.name.as_str());
                    if is_editing {
                        let mut buffer = self
                            .editing
                            .as_ref()
                            .map(|edit| edit.buffer.clone())
                            .unwrap_or_default();
                        // 编辑框自己画底与描边：26 px 的行里放 egui 默认输入框太重，
                        // 加一圈强调色描边才能一眼看出这一格正在编辑。
                        let edit_rect = Rect::from_center_size(
                            note_rect.center(),
                            Vec2::new((note_rect.width() - 6.0).max(60.0), 20.0),
                        );
                        ui.painter().rect_filled(edit_rect, CornerRadius::same(3), pal.panel);
                        ui.painter().rect_stroke(
                            edit_rect,
                            CornerRadius::same(3),
                            egui::Stroke::new(1.0, pal.accent),
                            egui::StrokeKind::Inside,
                        );
                        let text_rect = edit_rect.shrink2(Vec2::new(4.0, 2.0));
                        let mut child = ui.new_child(
                            egui::UiBuilder::new()
                                .max_rect(text_rect)
                                .layout(egui::Layout::left_to_right(egui::Align::Center)),
                        );
                        let output = child.add(
                            egui::TextEdit::singleline(&mut buffer)
                                .id(ui.scope_id().with(("snap-note-edit", index)))
                                .frame(egui::Frame::NONE)
                                .text_color(pal.text)
                                .font(FontId::proportional(11.5))
                                .margin(egui::Margin::symmetric(0, 0))
                                .desired_width(text_rect.width())
                                .char_limit(NOTE_MAX_CHARS)
                                .hint_text(
                                    RichText::new("备注，回车保存 / Esc 取消")
                                        .size(11.0)
                                        .color(pal.text_faint),
                                ),
                        );
                        if ui.input(|input| input.key_pressed(egui::Key::Escape)) {
                            // Esc = 放弃本次输入（TextEdit 自己不会因为 Esc 失焦）
                            edit_outcome = Some(EditOutcome::Cancel);
                        } else if output.lost_focus() {
                            // 回车（单行框会失焦）与点到别处都算保存
                            edit_outcome = Some(EditOutcome::Commit(buffer));
                        } else {
                            if editing_focus && !output.has_focus() {
                                output.request_focus();
                            }
                            edit_outcome = Some(EditOutcome::Keep(buffer));
                        }
                    } else {
                        let note = self.note_of(&file.name, &meta);
                        let truncated = paint_note(ui, pal, note_rect, &note, is_current);
                        // 单击进入编辑：复用整行的点击响应，不再叠一个覆盖控件，
                        // 这样备注列上的右键菜单（复制完整路径 / 打开目录）照常可用。
                        let over_note = response
                            .hover_pos()
                            .is_some_and(|pos| note_rect.contains(pos));
                        if over_note {
                            response.clone().on_hover_cursor(egui::CursorIcon::Text);
                            let hint = if note.is_empty() {
                                "单击添加备注".to_owned()
                            } else if truncated {
                                format!("{note}\n\n单击编辑备注")
                            } else {
                                String::new()
                            };
                            if !hint.is_empty() {
                                response.clone().on_hover_text(hint);
                            }
                            if response.clicked() {
                                open_edit = Some(file.name.clone());
                            }
                        }
                    }

                    // ---- 操作区：设为当前 / 复制路径 / 删除 ----
                    let buttons = row_buttons_rect(rect);
                    let switch_id = ui.scope_id().with(("snap-switch", index));
                    let switch_rect = egui::Rect::from_min_max(
                        egui::pos2(buttons.0, rect.top() + 2.0),
                        egui::pos2(buttons.0 + 74.0, rect.bottom() - 2.0),
                    );
                    let switch = ui.interact(switch_rect, switch_id, Sense::click());
                    if switch.hovered() {
                        ui.painter().rect_filled(switch_rect, CornerRadius::same(4), pal.hover);
                    }
                    ui.painter().text(
                        switch_rect.center(),
                        Align2::CENTER_CENTER,
                        if is_current { "当前" } else { "设为当前" },
                        FontId::proportional(11.5),
                        if is_current { pal.accent } else { pal.text_dim },
                    );
                    if !is_current && switch.clicked() {
                        action = Some(SnapshotsAction::Switch(file.name.clone()));
                    }
                    // 复制路径：窄窗口下省略（右键菜单里仍有「复制完整路径」，功能不丢）。
                    let mut next_left = switch_rect.right() + 6.0;
                    if !compact {
                        let copy_id = ui.scope_id().with(("snap-copy", index));
                        let copy_rect = egui::Rect::from_min_max(
                            egui::pos2(next_left, rect.top() + 2.0),
                            egui::pos2(next_left + 78.0, rect.bottom() - 2.0),
                        );
                        let copy = ui.interact(copy_rect, copy_id, Sense::click());
                        if copy.hovered() {
                            ui.painter().rect_filled(copy_rect, CornerRadius::same(4), pal.hover);
                        }
                        ui.painter().text(
                            copy_rect.center(),
                            Align2::CENTER_CENTER,
                            "复制路径",
                            FontId::proportional(11.5),
                            pal.text_dim,
                        );
                        if copy.clicked() {
                            ui.ctx().copy_text(file.path.display().to_string());
                            action = Some(SnapshotsAction::Toast(format!(
                                "已复制：{}",
                                file.path.display()
                            )));
                        }
                        next_left = copy_rect.right() + 6.0;
                    }
                    // 删除：当前打开的与最后一份都不可删（置灰 + tooltip），其余走二次确认。
                    let delete_rect = egui::Rect::from_min_max(
                        egui::pos2(next_left, rect.top() + 2.0),
                        egui::pos2(next_left + 46.0, rect.bottom() - 2.0),
                    );
                    let delete_id = ui.scope_id().with(("snap-delete", index));
                    let allowed = can_delete_snapshot(total_files, is_current);
                    let delete = ui.interact(
                        delete_rect,
                        delete_id,
                        if allowed { Sense::click() } else { Sense::hover() },
                    );
                    if allowed && delete.hovered() {
                        ui.painter().rect_filled(delete_rect, CornerRadius::same(4), pal.hover);
                    }
                    ui.painter().text(
                        delete_rect.center(),
                        Align2::CENTER_CENTER,
                        "删除",
                        FontId::proportional(11.5),
                        if allowed { pal.danger } else { pal.text_faint },
                    );
                    if allowed {
                        delete.clone().on_hover_text(
                            "移入待删除目录并登记 MANIFEST（程序不永久删除文件）",
                        );
                        if delete.clicked() {
                            delete_request = Some(file.name.clone());
                        }
                    } else {
                        let why = if is_current {
                            "当前打开的快照不能删除：先切换到别的快照"
                        } else {
                            "最后一份快照不能删除：删掉就没有可读的数据了"
                        };
                        delete.clone().on_hover_text(why);
                    }
                }
            });

        // ---- 帧末落地：备注编辑 → 打开别的单元格 → 删除请求 → 确认框 ----
        match edit_outcome {
            Some(EditOutcome::Keep(buffer)) => {
                if let Some(edit) = self.editing.as_mut() {
                    edit.buffer = buffer;
                    edit.focus = false;
                }
            }
            Some(EditOutcome::Commit(buffer)) => {
                if let Some(edit) = self.editing.take() {
                    self.commit_note(&edit.name, buffer);
                }
            }
            Some(EditOutcome::Cancel) => {
                self.editing = None;
            }
            None => {}
        }
        if let Some(name) = open_edit {
            let meta = self.meta.get(&name).cloned().unwrap_or_default();
            let buffer = self.note_of(&name, &meta);
            self.editing = Some(NoteEdit {
                name,
                buffer,
                focus: true,
            });
        }
        if let Some(name) = delete_request {
            self.pending_delete = Some(name);
        }

        if let Some(deleted) = self.confirm_delete(ui.ctx(), pal) {
            action = Some(SnapshotsAction::Deleted(deleted));
        }

        // 错误行：删除 / 备注保存失败都在这里明说，不静默。
        self.error_line(ui, pal);

        ui.add_space(4.0);
        let policy_text = match self.policy_hint {
            Some((days, keep)) => {
                format!(" · 自动快照每 {days} 天至少一份，保留最近 {keep} 份")
            }
            None => String::new(),
        };
        ui.label(
            RichText::new(format!(
                "合计 {} 份 · {} · 删除 = 移入待删除并登记 MANIFEST（程序不永久删除）{policy_text}",
                self.files.len(),
                db::human_size(self.total_bytes as i64),
            ))
            .size(11.0)
            .color(pal.text_faint),
        );
        action
    }

    /// 把一条备注写进 sidecar 并刷盘；失败时把原因挂到错误行。
    fn commit_note(&mut self, name: &str, buffer: String) {
        self.notes.insert(name.to_owned(), buffer);
        match self.save_notes() {
            Ok(()) => self.error = None,
            Err(error) => self.error = Some(format!("备注保存失败：{error}")),
        }
    }

    /// 删除确认框；确认后真正移入待删除区，返回被删除的快照名。
    fn confirm_delete(&mut self, ctx: &egui::Context, pal: &Palette) -> Option<String> {
        let name = self.pending_delete.clone()?;
        let bytes = self
            .files
            .iter()
            .find(|file| file.name == name)
            .map(|file| file.bytes)
            .unwrap_or(0);
        let trash_dir = self
            .trash_dir()
            .map(|dir| dir.display().to_string())
            .unwrap_or_else(|| "待删除".to_owned());
        let mut confirm = false;
        let mut cancel = false;
        let modal = egui::Modal::new(egui::Id::unique("snap-delete-confirm"))
            .backdrop_color(Color32::from_black_alpha(120))
            .frame(
                egui::Frame::NONE
                    .fill(pal.card)
                    .stroke(egui::Stroke::new(1.0, pal.border))
                    .corner_radius(CornerRadius::same(theme::RADIUS_LG))
                    .inner_margin(egui::Margin::same(16)),
            )
            .show(ctx, |ui| {
                ui.set_width(360.0);
                ui.label(
                    RichText::new("删除快照")
                        .size(14.0)
                        .strong()
                        .color(pal.text),
                );
                ui.add_space(8.0);
                ui.label(
                    RichText::new(format!("{}（{}）", name, db::human_size(bytes as i64)))
                        .size(12.5)
                        .color(pal.text),
                );
                ui.add_space(10.0);
                ui.label(
                    RichText::new(format!(
                        "文件将移入 {trash_dir}，并在该目录的 MANIFEST.md 记一行。\n程序不永久删除文件，需要彻底删除请自行清理待删除目录。"
                    ))
                    .size(11.5)
                    .color(pal.text_dim),
                );
                ui.add_space(14.0);
                ui.horizontal(|ui| {
                    if ui
                        .add(
                            egui::Button::new(RichText::new("取消").size(12.0).color(pal.text))
                                .fill(pal.panel_alt)
                                .stroke(egui::Stroke::new(1.0, pal.border))
                                .min_size(Vec2::new(78.0, 26.0)),
                        )
                        .clicked()
                    {
                        cancel = true;
                    }
                    if ui
                        .add(
                            egui::Button::new(
                                RichText::new("移入待删除").size(12.0).color(pal.accent_text),
                            )
                            .fill(pal.danger)
                            .min_size(Vec2::new(96.0, 26.0)),
                        )
                        .clicked()
                    {
                        confirm = true;
                    }
                });
            });
        if confirm {
            self.pending_delete = None;
            return self.delete_snapshot(&name);
        }
        if cancel || modal.should_close() {
            self.pending_delete = None;
        }
        None
    }

    /// 真正删除：移入待删除区（同步 index.json / notes.json）→ 从列表移除 → 重算体积与磁盘。
    fn delete_snapshot(&mut self, name: &str) -> Option<String> {
        let Some(snapshot_dir) = self.snapshot_dir() else {
            self.error = Some("拿不到快照目录（快照路径异常），未做任何改动".to_owned());
            return None;
        };
        let root = self
            .data_root
            .clone()
            .or_else(|| snapshot_dir.parent().map(Path::to_path_buf));
        let Some(root) = root else {
            self.error = Some("拿不到数据目录（快照路径异常），无法定位待删除目录，未做任何改动".to_owned());
            return None;
        };
        let Some(file) = self.files.iter().find(|file| file.name == name).cloned() else {
            self.error = Some(format!("列表里已经没有快照 {name}，未做任何改动"));
            return None;
        };
        match prune::move_to_trash_as(&file.path, &root, DELETE_REASON, DELETE_OPERATOR) {
            Ok(target) => {
                self.files.retain(|file| file.name != name);
                self.meta.remove(name);
                // 备注跟着快照走：这份快照已经移走，sidecar 里那条也一并清掉。
                if self.notes.remove(name).is_some() {
                    let _ = self.save_notes();
                }
                self.total_bytes = self.files.iter().map(|file| file.bytes).sum();
                self.refresh_disk();
                self.error = None;
                eprintln!("[快照] 已移入待删除：{}", target.display());
                Some(name.to_owned())
            }
            Err(error) => {
                self.error = Some(format!("删除 {name} 失败：{error}"));
                None
            }
        }
    }

    /// 一行错误提示（可点右侧 × 关掉）。
    fn error_line(&mut self, ui: &mut Ui, pal: &Palette) {
        let Some(error) = self.error.clone() else {
            return;
        };
        ui.add_space(4.0);
        let height = 22.0;
        let (rect, _) = ui.allocate_exact_size(
            Vec2::new(ui.available_width(), height),
            Sense::hover(),
        );
        let fill = Color32::from_rgba_unmultiplied(pal.danger.r(), pal.danger.g(), pal.danger.b(), 26);
        ui.painter().rect_filled(rect, CornerRadius::same(4), fill);
        let text = elide_left(
            ui.painter(),
            &error,
            FontId::proportional(11.5),
            pal.danger,
            rect.width() - 32.0,
        );
        ui.painter().text(
            egui::pos2(rect.left() + 8.0, rect.center().y),
            Align2::LEFT_CENTER,
            text,
            FontId::proportional(11.5),
            pal.danger,
        );
        let close_rect = Rect::from_center_size(
            egui::pos2(rect.right() - 14.0, rect.center().y),
            Vec2::splat(18.0),
        );
        let close = ui.interact(close_rect, ui.scope_id().with("snap-error-close"), Sense::click());
        if close.hovered() {
            ui.painter().circle_filled(close_rect.center(), 8.0, pal.hover);
        }
        ui.painter().text(
            close_rect.center(),
            Align2::CENTER_CENTER,
            "×",
            FontId::proportional(13.0),
            pal.danger,
        );
        close.clone().on_hover_text("关闭提示");
        if close.clicked() {
            self.error = None;
        }
    }
}

/// 用资源管理器打开目录（失败就静默：只是辅助操作，不该影响主流程）。
fn open_dir(dir: &Path) {
    let _ = std::process::Command::new("explorer").arg(dir).spawn();
}

/// 从左侧截断（保住尾部路径），用于目录这类「结尾更重要」的文本。
fn elide_left(
    painter: &eframe::egui::Painter,
    text: &str,
    font: FontId,
    color: Color32,
    max_width: f32,
) -> String {
    let full = painter.layout_no_wrap(text.to_owned(), font.clone(), color);
    if full.size().x <= max_width || max_width <= 20.0 {
        return text.to_owned();
    }
    let chars: Vec<char> = text.chars().collect();
    let mut keep = chars.len();
    while keep > 4 {
        keep -= 1;
        let candidate: String = format!("…{}", chars[chars.len() - keep..].iter().collect::<String>());
        if painter.layout_no_wrap(candidate, font.clone(), color).size().x <= max_width {
            return format!("…{}", chars[chars.len() - keep..].iter().collect::<String>());
        }
    }
    format!("…{}", chars[chars.len() - 4..].iter().collect::<String>())
}

/// 从右侧截断（保住开头），用于备注这类「开头更重要」的文本。
fn elide_right(
    painter: &eframe::egui::Painter,
    text: &str,
    font: FontId,
    color: Color32,
    max_width: f32,
) -> String {
    let galley = painter.layout_no_wrap(text.to_owned(), font.clone(), color);
    if galley.size().x <= max_width || max_width <= 8.0 {
        return text.to_owned();
    }
    let chars: Vec<char> = text.chars().collect();
    let mut low = 0usize;
    let mut high = chars.len();
    while low < high {
        let middle = (low + high).div_ceil(2);
        let mut candidate: String = chars[..middle].iter().collect();
        candidate.push('…');
        if painter.layout_no_wrap(candidate, font.clone(), color).size().x <= max_width {
            low = middle;
        } else {
            high = middle - 1;
        }
    }
    let mut candidate: String = chars[..low].iter().collect();
    candidate.push('…');
    candidate
}

/// 读一份快照的 meta（备注 / 逻辑大小 / 格式版本）。读取失败时字段留默认。
fn read_meta(path: &Path) -> SnapMeta {
    use rusqlite::{Connection, OpenFlags};
    let Ok(conn) = Connection::open_with_flags(path, OpenFlags::SQLITE_OPEN_READ_ONLY) else {
        return SnapMeta::default();
    };
    let value = |key: &str| -> Option<String> {
        conn.query_row("SELECT value FROM meta WHERE key = ?1", [key], |row| {
            row.get::<_, String>(0)
        })
        .ok()
    };
    SnapMeta {
        note: value("note").unwrap_or_default(),
        total_size: value("total_size").and_then(|v| v.trim().parse().ok()).unwrap_or(0),
        format_version: value("format_version").and_then(|v| v.trim().parse().ok()).unwrap_or(0),
    }
}

/// 卷剩余 / 总容量（字节）。非 Windows 或失败时返回 (0, 0)。
///
/// 入参必须是**目录**：实测 GetDiskFreeSpaceExW 传文件路径会返回 ERROR_DIRECTORY(267)，
/// 于是磁盘剩余整栏显示「—」。传快照所在目录（同卷）即可拿到该卷的实时空闲。
fn disk_free(path: &Path) -> (u64, u64) {
    #[cfg(windows)]
    {
        use std::os::windows::ffi::OsStrExt;
        #[repr(C)]
        struct Large {
            quad: u64,
        }
        #[link(name = "kernel32")]
        unsafe extern "system" {
            fn GetDiskFreeSpaceExW(
                directory: *const u16,
                free: *mut Large,
                total: *mut Large,
                available: *mut Large,
            ) -> i32;
        }
        let mut wide: Vec<u16> = path.as_os_str().encode_wide().collect();
        wide.push(0);
        let mut free = Large { quad: 0 };
        let mut total = Large { quad: 0 };
        let mut available = Large { quad: 0 };
        let ok = unsafe {
            GetDiskFreeSpaceExW(wide.as_ptr(), &mut free, &mut total, &mut available)
        };
        if ok != 0 {
            return (free.quad, total.quad);
        }
    }
    let _ = path;
    (0, 0)
}

// ---------- 表格绘制 ----------

/// 窄窗口阈值：窗口最小宽度就是 900，低于此值时用紧凑列宽，
/// 并隐藏行内「复制路径」按钮（右键菜单里仍有，功能不丢）。
const COMPACT_WIDTH: f32 = 1020.0;

fn is_compact(width: f32) -> bool {
    width < COMPACT_WIDTH
}

/// 表格列：名称、宽度、是否右对齐（对齐 Web 版 4.1 快照表，外加创建时间列）。
/// 宽度 0 表示备注列占余量；紧凑模式只是整体收窄，不隐藏任何列，900 px 宽也整行放得下。
fn columns(compact: bool) -> [(&'static str, f32, bool); 8] {
    if compact {
        [
            ("快照", 140.0, false),
            ("格式", 44.0, false),
            ("条目", 88.0, true),
            ("逻辑大小", 92.0, true),
            ("库体积", 82.0, true),
            ("创建时间", 110.0, true),
            ("备注", 0.0, false),
            ("操作", 126.0, false),
        ]
    } else {
        [
            ("快照", 150.0, false),
            ("格式", 52.0, false),
            ("条目", 96.0, true),
            ("逻辑大小", 100.0, true),
            ("库体积", 90.0, true),
            ("创建时间", 128.0, true),
            ("备注", 0.0, false),
            ("操作", 210.0, false),
        ]
    }
}

fn layout_cols(rect: Rect) -> Vec<Rect> {
    let cols = columns(is_compact(rect.width()));
    let gap = 10.0;
    let fixed: f32 = cols.iter().filter(|c| c.1 > 0.0).map(|c| c.1).sum::<f32>() + gap * (cols.len() as f32 - 1.0);
    // 列从 rect.left()+2 起画，减掉这 2px 才能让最后一列正好落在右边界上。
    let note_w = (rect.width() - fixed - 2.0).max(120.0);
    let mut x = rect.left() + 2.0;
    cols.iter()
        .map(|(_, w, _)| {
            let width = if *w > 0.0 { *w } else { note_w };
            let col = Rect::from_min_size(egui::pos2(x, rect.top()), Vec2::new(width, rect.height()));
            x += width + gap;
            col
        })
        .collect()
}

/// 行右侧按钮区的起始 x（操作列）。
fn row_buttons_rect(rect: Rect) -> (f32, f32) {
    let cols = layout_cols(rect);
    let last = cols[columns(is_compact(rect.width())).len() - 1];
    (last.left(), last.width())
}

/// 备注列的矩形（第 7 列）。
fn note_cell(rect: Rect) -> Rect {
    layout_cols(rect)[6]
}

/// 删除按钮是否可用：当前打开的快照不能删（正被检索/空间/对比视图用着），
/// 最后一份也不能删（删完就没有任何可读数据）。
fn can_delete_snapshot(total: usize, is_current: bool) -> bool {
    total > 1 && !is_current
}

fn paint_header(ui: &mut Ui, pal: &Palette, rect: Rect) {
    let painter = ui.painter();
    painter.rect_filled(rect, CornerRadius::same(0), pal.panel_alt);
    painter.hline(
        rect.x_range(),
        rect.bottom() - 0.5,
        egui::Stroke::new(1.0, pal.border),
    );
    let font = FontId::proportional(11.0);
    let names = columns(is_compact(rect.width()));
    for (col, (name, _, numeric)) in layout_cols(rect).iter().zip(names.iter()) {
        let pos = if *numeric {
            egui::pos2(col.right() - 4.0, rect.center().y)
        } else {
            egui::pos2(col.left() + 4.0, rect.center().y)
        };
        let align = if *numeric { Align2::RIGHT_CENTER } else { Align2::LEFT_CENTER };
        painter.text(pos, align, *name, font.clone(), pal.text_dim);
    }
}

/// 画备注格：空备注给一句占位提示；超长从右侧截断（返回是否被截断，供 tooltip 用）。
fn paint_note(ui: &mut Ui, pal: &Palette, rect: Rect, note: &str, is_current: bool) -> bool {
    let font = FontId::proportional(11.5);
    if note.is_empty() {
        let placeholder = if is_current { "（当前快照）" } else { "单击添加备注" };
        ui.painter().text(
            egui::pos2(rect.left() + 4.0, rect.center().y),
            Align2::LEFT_CENTER,
            placeholder,
            font,
            pal.text_faint,
        );
        return false;
    }
    let width = rect.width() - 8.0;
    let text = elide_right(ui.painter(), note, font.clone(), pal.text_dim, width);
    let truncated = text != note;
    ui.painter().text(
        egui::pos2(rect.left() + 4.0, rect.center().y),
        Align2::LEFT_CENTER,
        text,
        font,
        pal.text_dim,
    );
    truncated
}

fn paint_row(
    ui: &mut Ui,
    pal: &Palette,
    rect: Rect,
    index: usize,
    file: &db::SnapshotFile,
    meta: &SnapMeta,
    is_current: bool,
) {
    let painter = ui.painter();
    let fill = pal.row_fill(index, false, is_current);
    if fill != Color32::TRANSPARENT {
        painter.rect_filled(rect, CornerRadius::same(0), fill);
    }
    if is_current {
        painter.rect_stroke(
            rect,
            CornerRadius::same(0),
            egui::Stroke::new(1.0, pal.accent.gamma_multiply(0.6)),
            egui::StrokeKind::Inside,
        );
    }
    let cols = layout_cols(rect);
    let mono = FontId::monospace(11.5);
    let dim = FontId::proportional(11.5);
    let mid = rect.center().y;

    // 快照名（当前项左侧画强调色圆点）
    if is_current {
        painter.circle_filled(egui::pos2(cols[0].left() + 6.0, mid), 3.0, pal.accent);
    }
    let name_offset = if is_current { 14.0 } else { 4.0 };
    let name = elide_right(
        painter,
        &file.name,
        dim.clone(),
        pal.text,
        cols[0].width() - name_offset - 4.0,
    );
    painter.text(
        egui::pos2(cols[0].left() + name_offset, mid),
        Align2::LEFT_CENTER,
        name,
        dim.clone(),
        if is_current { pal.accent } else { pal.text },
    );
    // 层级
    let tier = if meta.format_version >= 4 { "v4" } else { "v3" };
    painter.text(
        egui::pos2(cols[1].left() + 4.0, mid),
        Align2::LEFT_CENTER,
        tier,
        FontId::proportional(11.0),
        pal.text_faint,
    );
    // 条目
    painter.text(
        egui::pos2(cols[2].right() - 4.0, mid),
        Align2::RIGHT_CENTER,
        db::thousands(file.entries),
        mono.clone(),
        pal.text_dim,
    );
    // 逻辑大小
    painter.text(
        egui::pos2(cols[3].right() - 4.0, mid),
        Align2::RIGHT_CENTER,
        if meta.total_size > 0 { db::human_size(meta.total_size) } else { "—".to_owned() },
        mono.clone(),
        pal.text_dim,
    );
    // 库体积
    painter.text(
        egui::pos2(cols[4].right() - 4.0, mid),
        Align2::RIGHT_CENTER,
        db::human_size(file.bytes as i64),
        mono.clone(),
        pal.text_dim,
    );
    // 创建时间
    painter.text(
        egui::pos2(cols[5].right() - 4.0, mid),
        Align2::RIGHT_CENTER,
        db::format_unix(file.created_at),
        dim.clone(),
        pal.text_faint,
    );
    // 备注列由 show() 里的单元格负责：静态时画文本 + tooltip，编辑时换成 TextEdit。
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::store::trash;

    fn snapshot(name: &str, bytes: u64) -> db::SnapshotFile {
        db::SnapshotFile {
            path: PathBuf::from(format!(r"D:\disk-intel-data\snapshots\{name}.sqlite")),
            name: name.to_owned(),
            bytes,
            entries: 100,
            created_at: 1_760_000_000,
            source_sha256: String::new(),
        }
    }

    /// 备注优先取 sidecar；sidecar 没有才回落到库里的 meta.note（含清空后不再回落）。
    #[test]
    fn sidecar_note_wins_over_db_meta() {
        let files = vec![snapshot("a", 10), snapshot("b", 20)];
        let mut state = SnapshotsState::new(&files, &db::Snapshot::open_empty().unwrap());
        let mut meta = SnapMeta {
            note: "库里的旧备注".to_owned(),
            total_size: 0,
            format_version: 3,
        };
        assert_eq!(state.note_of("a", &meta), "库里的旧备注");
        state.notes.insert("a".to_owned(), "界面改的新备注".to_owned());
        assert_eq!(state.note_of("a", &meta), "界面改的新备注");
        state.notes.insert("a".to_owned(), String::new());
        meta.note = "库里的旧备注".to_owned();
        assert_eq!(state.note_of("a", &meta), "", "清空后不该回落到库里");
    }

    /// 目录从快照路径反推：<data_root>\snapshots\<名>.sqlite → 快照目录 + 数据根。
    #[test]
    fn snapshot_dir_and_data_root_derived_from_path() {
        let files = vec![snapshot("20261112-100008", 123)];
        let state = SnapshotsState::new(&files, &db::Snapshot::open_empty().unwrap());
        assert_eq!(
            state.snapshot_dir().as_deref(),
            Some(Path::new(r"D:\disk-intel-data\snapshots"))
        );
        assert_eq!(
            state.data_root.as_deref(),
            Some(Path::new(r"D:\disk-intel-data"))
        );
        assert_eq!(
            notes::note_path_in(state.snapshot_dir().as_deref().unwrap()),
            PathBuf::from(r"D:\disk-intel-data\snapshots\notes.json")
        );
        assert_eq!(
            state.trash_dir().as_deref(),
            Some(Path::new(r"D:\disk-intel-data\待删除")),
            "默认布局沿用数据根下的待删除"
        );
    }

    /// 合计体积与磁盘锚点跟着列表走（删除后由 delete_snapshot 重算）。
    #[test]
    fn totals_follow_the_file_list() {
        let files = vec![snapshot("b", 30), snapshot("a", 12)];
        let state = SnapshotsState::new(&files, &db::Snapshot::open_empty().unwrap());
        assert_eq!(state.total_bytes, 42);
        assert_eq!(state.files.len(), 2);
    }

    /// 唯一临时目录（和 store 侧单测同口径：只写自己的 %TEMP% 子目录，不做永久删除）。
    fn unique_dir(tag: &str) -> PathBuf {
        use std::sync::atomic::{AtomicU64, Ordering};
        static SEQ: AtomicU64 = AtomicU64::new(0);
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0);
        let dir = std::env::temp_dir()
            .join("diskintel-snapshots-tests")
            .join(format!("{tag}-{nanos}-{}", SEQ.fetch_add(1, Ordering::Relaxed)));
        std::fs::create_dir_all(dir.join("snapshots")).expect("建临时快照目录");
        dir
    }

    /// 造一份「假快照」：只要求路径形状对（<root>snapshots<名>.sqlite），列表不校验库内容。
    fn fake_snapshot(root: &Path, name: &str, bytes: u64) -> db::SnapshotFile {
        fake_snapshot_in(&root.join("snapshots"), name, bytes)
    }

    /// 在指定快照目录里造假快照（自定义目录用）。
    fn fake_snapshot_in(snapshot_dir: &Path, name: &str, bytes: u64) -> db::SnapshotFile {
        std::fs::create_dir_all(snapshot_dir).expect("建快照目录");
        let path = snapshot_dir.join(format!("{name}.sqlite"));
        std::fs::write(&path, vec![0u8; (bytes as usize).min(64)]).expect("写假快照");
        db::SnapshotFile {
            path,
            name: name.to_owned(),
            bytes,
            entries: 3,
            created_at: 1_760_000_000,
            source_sha256: String::new(),
        }
    }

    /// 自定义快照目录：目录/数据根由外壳注入，备注跟着快照目录走，删除就近进待删除。
    #[test]
    fn configure_supports_custom_snapshot_dir() {
        let root = unique_dir("custom-dir");
        let custom = root.join("我的快照");
        let first = fake_snapshot_in(&custom, "20260101-000000", 111);
        let second = fake_snapshot_in(&custom, "20260102-000000", 222);
        let mut state = SnapshotsState::new(
            &[second, first],
            &db::Snapshot::open_empty().unwrap(),
        );
        // 没 configure 时也能按文件路径反推
        assert_eq!(state.snapshot_dir().as_deref(), Some(custom.as_path()));
        state.configure(&root, &custom, 7, 8);
        assert_eq!(state.policy_hint, Some((7, 8)));
        assert_eq!(
            state.trash_dir().as_deref(),
            Some(custom.join("待删除").as_path()),
            "自定义目录的待删除就近放，不跨盘复制"
        );

        // 备注写进自定义快照目录
        state.commit_note("20260101-000000", "自定义目录备注".to_owned());
        assert!(state.error.is_none(), "{:?}", state.error);
        assert_eq!(
            notes::load_in(&custom).get("20260101-000000").map(String::as_str),
            Some("自定义目录备注")
        );

        // 删除落点也在自定义目录里
        let deleted = state.delete_snapshot("20260101-000000").expect("删除应成功");
        assert_eq!(deleted, "20260101-000000");
        assert!(!custom.join("20260101-000000.sqlite").exists());
        assert!(custom.join("待删除").exists(), "待删除应就近创建");
        assert!(!root.join("待删除").exists(), "不该在数据根下建待删除");
    }

    /// 删除许可：当前项与最后一份都不可删。
    #[test]
    fn delete_guard_rules() {
        assert!(!can_delete_snapshot(1, false), "最后一份不能删");
        assert!(!can_delete_snapshot(5, true), "当前快照不能删");
        assert!(!can_delete_snapshot(1, true));
        assert!(can_delete_snapshot(2, false));
        assert!(can_delete_snapshot(5, false));
    }

    /// 端到端（不碰用户数据）：delete_snapshot 真的把文件移入临时数据根的待删除目录、
    /// 登记 MANIFEST，并从列表里移除、重算合计体积。
    #[test]
    fn delete_snapshot_moves_file_and_logs_manifest() {
        let root = unique_dir("delete");
        let first = fake_snapshot(&root, "20260101-000000", 111);
        let second = fake_snapshot(&root, "20260102-000000", 222);
        let mut state = SnapshotsState::new(
            &[second.clone(), first.clone()],
            &db::Snapshot::open_empty().unwrap(),
        );
        assert_eq!(state.total_bytes, 333);
        assert!(state.error.is_none());

        let deleted = state
            .delete_snapshot("20260101-000000")
            .expect("应当删除成功并回传名字");

        assert_eq!(deleted, "20260101-000000");
        assert!(!first.path.exists(), "原路径必须不存在");
        assert!(second.path.exists(), "别的快照不能被牵连");
        assert_eq!(state.files.len(), 1);
        assert_eq!(state.total_bytes, 222);
        assert!(state.error.is_none(), "不该有错误行：{:?}", state.error);

        let trash = root.join(trash::TRASH_DIR);
        let moved: Vec<PathBuf> = std::fs::read_dir(&trash)
            .expect("待删除目录应当存在")
            .flatten()
            .map(|entry| entry.path())
            .filter(|path| path.extension().and_then(|ext| ext.to_str()) == Some("sqlite"))
            .collect();
        assert_eq!(moved.len(), 1, "待删除目录里应当正好一个文件：{moved:?}");
        assert!(
            moved[0].file_name().unwrap().to_string_lossy().contains("20260101-000000"),
            "新名字要保住原名：{}",
            moved[0].display()
        );
        let manifest = std::fs::read_to_string(trash.join(trash::MANIFEST_FILE)).unwrap();
        assert!(manifest.contains("20260101-000000.sqlite"), "{manifest}");
        assert!(manifest.contains("快照管理页手动删除"), "{manifest}");
        assert!(manifest.contains("| UI |"), "{manifest}");
    }

    /// 备注落盘：commit_note 之后 sidecar 里能读回同一个值。
    #[test]
    fn commit_note_writes_sidecar() {
        let root = unique_dir("note");
        let file = fake_snapshot(&root, "20260103-000000", 5);
        let mut state = SnapshotsState::new(&[file], &db::Snapshot::open_empty().unwrap());
        state.commit_note("20260103-000000", "手动写的备注".to_owned());
        assert!(state.error.is_none(), "不该报错：{:?}", state.error);

        let snapshot_dir = root.join("snapshots");
        let path = notes::note_path_in(&snapshot_dir);
        assert!(path.exists(), "notes.json 应当已创建：{}", path.display());
        let loaded = notes::load_in(&snapshot_dir);
        assert_eq!(loaded.get("20260103-000000").map(String::as_str), Some("手动写的备注"));

        // 再改一次应覆盖而不是追加
        state.commit_note("20260103-000000", "改过的备注".to_owned());
        assert_eq!(
            notes::load_in(&snapshot_dir).get("20260103-000000").map(String::as_str),
            Some("改过的备注")
        );
    }

    /// 备注列在最小窗口（900）与 1200 下都留得下，操作列不溢出窗口。
    #[test]
    fn note_column_survives_narrow_layout() {
        for width in [900.0_f32, 1200.0] {
            let rect = Rect::from_min_size(egui::pos2(0.0, 0.0), Vec2::new(width, theme::ROW_H));
            let note = note_cell(rect);
            assert!(
                note.width() >= 120.0,
                "{width} 宽时备注列只有 {}（至少 120）",
                note.width()
            );
            let buttons = row_buttons_rect(rect);
            assert!(
                buttons.0 + buttons.1 <= rect.right(),
                "{width} 宽时操作列溢出窗口：右边界 {} > {}",
                buttons.0 + buttons.1,
                rect.right()
            );
            // 操作列要装得下最小的按钮组：设为当前 74 + 间距 6 + 删除 46
            assert_eq!(buttons.1, if is_compact(width) { 126.0 } else { 210.0 });
        }
    }

    /// 紧凑模式只在窄窗口生效，且不会把列数改掉（表头与行共用同一套列宽）。
    #[test]
    fn compact_layout_threshold() {
        assert!(is_compact(900.0));
        assert!(is_compact(COMPACT_WIDTH - 1.0));
        assert!(!is_compact(COMPACT_WIDTH));
        assert_eq!(columns(true).len(), columns(false).len());
        let compact_fixed: f32 = columns(true).iter().map(|column| column.1).sum();
        let full_fixed: f32 = columns(false).iter().map(|column| column.1).sum();
        assert!(compact_fixed < full_fixed, "紧凑列宽必须更窄");
    }
}



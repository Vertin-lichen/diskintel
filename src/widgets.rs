//! 通用控件：原生版所有视图共用的一套轻量组件。
//!
//! 原则：视觉参数全部来自 theme::Palette，不在控件里写死颜色；
//! 交互一律返回 Response，便于视图层组合右键菜单 / tooltip / 快捷键。

use eframe::egui::{self, Color32, CornerRadius, Frame, Margin, Response, Sense, Stroke, Ui, Vec2};

use crate::icons::{self, Glyph};
use crate::theme::Palette;

/// 纯图标按钮：方形、圆角、悬停底色，无文字。
pub fn icon_button(ui: &mut Ui, pal: &Palette, glyph: Glyph, size: f32, tip: &str) -> Response {
    let (rect, response) = ui.allocate_exact_size(Vec2::splat(size), Sense::click());
    let active = response.hovered() || response.is_pointer_button_down_on();
    if active {
        ui.painter().rect_filled(rect, CornerRadius::same(5), pal.hover);
    }
    let inner = rect.shrink(size * 0.24);
    let tint = if response.enabled() { pal.text_dim } else { pal.text_faint };
    icons::paint_glyph(ui.painter(), inner, glyph, tint);
    if !tip.is_empty() {
        response.clone().on_hover_text(tip);
    }
    response
}

/// 带文字与可选图标的按钮（工具栏用）。selected 时填充强调色。
pub fn pill_button(
    ui: &mut Ui,
    pal: &Palette,
    label: &str,
    glyph: Option<Glyph>,
    selected: bool,
) -> Response {
    let font = egui::FontId::proportional(13.0);
    let text_w = ui.painter().layout_no_wrap(label.to_owned(), font.clone(), pal.text).size().x;
    let icon_w = if glyph.is_some() { 18.0 } else { 0.0 };
    let pad_x = 10.0;
    let size = Vec2::new(text_w + icon_w + pad_x * 2.0, 26.0);
    let (rect, response) = ui.allocate_exact_size(size, Sense::click());
    let hovered = response.hovered();
    let bg = if selected {
        pal.accent_weak
    } else if hovered {
        pal.hover
    } else {
        Color32::TRANSPARENT
    };
    if bg != Color32::TRANSPARENT {
        ui.painter().rect_filled(rect, CornerRadius::same(6), bg);
    }
    let fg = if selected { pal.accent } else { pal.text_dim };
    let mut x = rect.left() + pad_x;
    if let Some(glyph) = glyph {
        let icon_rect = egui::Rect::from_center_size(
            egui::pos2(x + 8.0, rect.center().y),
            Vec2::splat(15.0),
        );
        icons::paint_glyph(ui.painter(), icon_rect, glyph, fg);
        x += 18.0;
    }
    ui.painter().text(
        egui::pos2(x, rect.center().y),
        egui::Align2::LEFT_CENTER,
        label,
        font,
        fg,
    );
    if selected {
        // 底部 2px 指示条，与 Web 版的 tab 下划线一致
        ui.painter().rect_filled(
            egui::Rect::from_min_max(
                egui::pos2(rect.left() + 8.0, rect.bottom() - 2.0),
                egui::pos2(rect.right() - 8.0, rect.bottom()),
            ),
            CornerRadius::same(1),
            pal.accent,
        );
    }
    response
}

/// 视图标签：更扁、更大字距，用于顶部主视图切换。
///
/// 高度 28 px 是为 36 px 的紧凑工具栏留出上下各 4 px 呼吸位（二期 E：工具栏 40 → 36）。
pub fn view_tab(ui: &mut Ui, pal: &Palette, label: &str, selected: bool) -> Response {
    let font = egui::FontId::proportional(13.5);
    let text_w = ui.painter().layout_no_wrap(label.to_owned(), font.clone(), pal.text).size().x;
    let size = Vec2::new(text_w + 26.0, 28.0);
    let (rect, response) = ui.allocate_exact_size(size, Sense::click());
    if selected {
        ui.painter().rect_filled(rect, CornerRadius::same(7), pal.panel_alt);
    } else if response.hovered() {
        ui.painter().rect_filled(rect, CornerRadius::same(7), pal.hover);
    }
    let fg = if selected { pal.text } else { pal.text_dim };
    ui.painter().text(
        rect.center(),
        egui::Align2::CENTER_CENTER,
        label,
        font,
        fg,
    );
    response
}

// ---------- 检索框：常规态 / Hero 态 / 过渡态 ----------

/// 常规态检索框高度（放进 36 px 工具栏后上下各留 4 px）。
pub const FIELD_H: f32 = 28.0;
/// Hero 态检索框高度。
pub const HERO_FIELD_H: f32 = 50.0;
/// Hero 与工具栏之间的缓动时长（秒）。
pub const HERO_ANIM_SECS: f32 = 0.22;

/// 检索框占位提示：工具栏与 Hero 共用同一句，过渡时文字不跳变。
pub const SEARCH_HINT: &str =
    "搜索文件…  ext:pdf size:>10m after:2026-01-01 kind:file path:D:\\";

/// 检索框的全局唯一 Id。
///
/// 工具栏小框与中央 Hero 大框共用同一个 Id：egui 的焦点、光标位置、文本选择都按 Id 存，
/// 所以形态切换是「同一个输入框在移动 / 缩放」，而不是「换了一个控件」。
pub fn search_id() -> egui::Id {
    egui::Id::unique("diskintel-search")
}

/// 检索框当前是否持有键盘焦点。
pub fn search_focused(ctx: &egui::Context) -> bool {
    ctx.memory(|m| m.has_focus(search_id()))
}

/// 把键盘焦点交给检索框（启动自动聚焦 / Ctrl+F / Ctrl+L）。
pub fn focus_search(ctx: &egui::Context) {
    ctx.memory_mut(|m| m.request_focus(search_id()));
}

/// 主动放弃检索框焦点（Esc 清空之后）。
pub fn blur_search(ctx: &egui::Context) {
    ctx.memory_mut(|m| m.surrender_focus(search_id()));
}

/// 检索框的视觉参数。常规态与 Hero 态只是数值不同，
/// 过渡态按 t 线性插值 —— 于是「上移缩回」是同一套绘制代码在动，不会闪。
#[derive(Clone, Copy, Debug)]
struct SearchStyle {
    height: f32,
    font: f32,
    hint_font: f32,
    radius: u8,
    stroke: f32,
    pad_left: f32,
    pad_right: f32,
    icon: f32,
    gap_y: f32,
}

impl SearchStyle {
    const FIELD: Self = Self {
        height: FIELD_H,
        font: 13.0,
        hint_font: 12.5,
        radius: 8,
        stroke: 1.0,
        pad_left: 30.0,
        pad_right: 30.0,
        icon: 15.0,
        gap_y: 3.0,
    };

    const HERO: Self = Self {
        height: HERO_FIELD_H,
        font: 16.5,
        hint_font: 15.0,
        radius: 12,
        stroke: 1.6,
        pad_left: 42.0,
        pad_right: 42.0,
        icon: 20.0,
        gap_y: 4.0,
    };

    fn lerp(a: Self, b: Self, t: f32) -> Self {
        let t = t.clamp(0.0, 1.0);
        Self {
            height: lerp_f32(a.height, b.height, t),
            font: lerp_f32(a.font, b.font, t),
            hint_font: lerp_f32(a.hint_font, b.hint_font, t),
            radius: lerp_f32(a.radius as f32, b.radius as f32, t).round() as u8,
            stroke: lerp_f32(a.stroke, b.stroke, t),
            pad_left: lerp_f32(a.pad_left, b.pad_left, t),
            pad_right: lerp_f32(a.pad_right, b.pad_right, t),
            icon: lerp_f32(a.icon, b.icon, t),
            gap_y: lerp_f32(a.gap_y, b.gap_y, t),
        }
    }
}

fn lerp_f32(a: f32, b: f32, t: f32) -> f32 {
    a + (b - a) * t
}

/// 检索框的唯一绘制实现；返回值是 TextEdit 自己的 Response。
fn search_field_styled(
    ui: &mut Ui,
    pal: &Palette,
    value: &mut String,
    hint: &str,
    width: f32,
    style: SearchStyle,
) -> Response {
    let width = width.max(96.0);
    let (outer, _) = ui.allocate_exact_size(Vec2::new(width, style.height), Sense::hover());
    // 焦点按 Id 读：Hero 与工具栏共用同一 Id，所以两种形态的描边表现一致。
    let focused = search_focused(ui.ctx());
    let radius = CornerRadius::same(style.radius);
    ui.painter().rect_filled(outer, radius, pal.panel_alt);
    ui.painter().rect_stroke(
        outer,
        radius,
        Stroke::new(style.stroke, if focused { pal.accent } else { pal.border }),
        egui::StrokeKind::Inside,
    );

    let icon_rect = egui::Rect::from_center_size(
        egui::pos2(outer.left() + style.pad_left * 0.5, outer.center().y),
        Vec2::splat(style.icon),
    );
    icons::paint_glyph(
        ui.painter(),
        icon_rect,
        Glyph::Search,
        if focused { pal.accent } else { pal.text_faint },
    );

    let edit_rect = egui::Rect::from_min_max(
        egui::pos2(outer.left() + style.pad_left, outer.top() + style.gap_y),
        egui::pos2(outer.right() - style.pad_right, outer.bottom() - style.gap_y),
    );
    let mut child = ui.new_child(
        egui::UiBuilder::new()
            .max_rect(edit_rect)
            .layout(egui::Layout::left_to_right(egui::Align::Center)),
    );
    let mut response = child.add(
        egui::TextEdit::singleline(value)
            .id(search_id())
            .frame(egui::Frame::NONE)
            .desired_width(edit_rect.width())
            .hint_text(egui::RichText::new(hint).color(pal.text_faint).size(style.hint_font))
            .text_color(pal.text)
            .font(egui::FontId::proportional(style.font))
            .margin(Margin::symmetric(0, 0)),
    );

    if !value.is_empty() {
        let hit_size = (style.height * 0.62).min(style.pad_right);
        let clear_rect = egui::Rect::from_center_size(
            egui::pos2(outer.right() - style.pad_right * 0.5, outer.center().y),
            Vec2::splat(hit_size),
        );
        let hit = ui.interact(clear_rect, search_id().with("clear"), Sense::click());
        if hit.hovered() {
            ui.painter().circle_filled(clear_rect.center(), hit_size * 0.5, pal.hover);
        }
        icons::paint_glyph(
            ui.painter(),
            clear_rect.shrink(hit_size * 0.28),
            Glyph::Close,
            if hit.hovered() { pal.text } else { pal.text_faint },
        );
        if hit.clicked() {
            value.clear();
            // 清除按钮也是「输入变化」：必须让调用方看到 changed()，否则要等下一次键入才检索。
            response.mark_changed();
            focus_search(ui.ctx());
        }
    }

    response
}

/// 搜索框：圆角、内部左侧放大镜、右侧清除按钮、聚焦描边。
///
/// 返回的是 TextEdit 自己的 Response（历史实现返回的是外框 hover Response，
/// 调用方 response.changed() 永远为 false，所以打字几乎不触发检索）：
/// changed() / has_focus() / lost_focus() 现在都可以直接用。
pub fn search_field(
    ui: &mut Ui,
    pal: &Palette,
    value: &mut String,
    hint: &str,
    width: f32,
) -> Response {
    search_field_styled(ui, pal, value, hint, width, SearchStyle::FIELD)
}

/// Hero 检索框：居中大号版本（高 50 px、字号 16.5、圆角 12、描边 1.6）。
/// 与 search_field 共用 search_id()，切换形态时文本与焦点连续。
pub fn hero_search(ui: &mut Ui, pal: &Palette, value: &mut String, width: f32) -> Response {
    search_field_styled(ui, pal, value, SEARCH_HINT, width, SearchStyle::HERO)
}

/// 过渡态检索框：t = 0 完全是 Hero，t = 1 完全是工具栏，中间按 t 插值。
/// 用于「Hero 上移缩回顶部工具栏」的 220 ms 缓动，绘制代码与两端完全一致。
pub fn search_field_animated(
    ui: &mut Ui,
    pal: &Palette,
    value: &mut String,
    width: f32,
    t: f32,
) -> Response {
    search_field_styled(
        ui,
        pal,
        value,
        SEARCH_HINT,
        width,
        SearchStyle::lerp(SearchStyle::HERO, SearchStyle::FIELD, t),
    )
}

/// 与 Web 版一致的小圆点状态灯（引擎就绪 / 未就绪）。
pub fn status_dot(ui: &mut Ui, pal: &Palette, ok: bool, label: &str) -> Response {
    let font = egui::FontId::proportional(11.5);
    let text_w = ui.painter().layout_no_wrap(label.to_owned(), font.clone(), pal.text).size().x;
    let (rect, response) = ui.allocate_exact_size(Vec2::new(text_w + 20.0, 20.0), Sense::hover());
    let dot = if ok { pal.ok } else { pal.text_faint };
    ui.painter().circle_filled(
        egui::pos2(rect.left() + 5.0, rect.center().y),
        4.0,
        dot,
    );
    if ok {
        ui.painter().circle_filled(
            egui::pos2(rect.left() + 5.0, rect.center().y),
            7.0,
            Color32::from_rgba_unmultiplied(dot.r(), dot.g(), dot.b(), 40),
        );
    }
    ui.painter().text(
        egui::pos2(rect.left() + 15.0, rect.center().y),
        egui::Align2::LEFT_CENTER,
        label,
        font,
        pal.text_dim,
    );
    response
}

/// 描边小标签（类型 / 来源 / 状态）。
pub fn tag(ui: &mut Ui, pal: &Palette, text: &str, color: Color32) -> Response {
    let font = egui::FontId::proportional(11.0);
    let text_w = ui.painter().layout_no_wrap(text.to_owned(), font.clone(), color).size().x;
    let (rect, response) = ui.allocate_exact_size(Vec2::new(text_w + 12.0, 18.0), Sense::hover());
    ui.painter().rect_filled(
        rect,
        CornerRadius::same(4),
        Color32::from_rgba_unmultiplied(color.r(), color.g(), color.b(), 28),
    );
    ui.painter().text(rect.center(), egui::Align2::CENTER_CENTER, text, font, color);
    response
}

/// 竖向分隔线。
pub fn vsep(ui: &mut Ui, pal: &Palette) {
    let (rect, _) = ui.allocate_exact_size(Vec2::new(9.0, 18.0), Sense::hover());
    ui.painter().rect_filled(
        egui::Rect::from_center_size(rect.center(), Vec2::new(1.0, 16.0)),
        CornerRadius::same(1),
        pal.border_soft,
    );
}

/// 空态提示（居中、次要色、可带一行操作提示）。
pub fn empty_state(ui: &mut Ui, pal: &Palette, title: &str, hint: &str) {
    ui.vertical_centered(|ui| {
        ui.add_space(40.0);
        ui.label(
            egui::RichText::new(title)
                .size(14.0)
                .color(pal.text_dim),
        );
        ui.add_space(6.0);
        ui.label(
            egui::RichText::new(hint)
                .size(12.0)
                .color(pal.text_faint),
        );
    });
}

/// 统一的卡片容器。
pub fn card(pal: &Palette) -> Frame {
    Frame::NONE
        .fill(pal.card)
        .stroke(Stroke::new(1.0, pal.border_soft))
        .corner_radius(CornerRadius::same(crate::theme::RADIUS_MD))
        .inner_margin(Margin::same(12))
}
/// 主按钮：强调色实底，用于「扫描」这类主操作。
pub fn primary_button(ui: &mut Ui, pal: &Palette, label: &str, glyph: Option<Glyph>) -> Response {
    let font = egui::FontId::proportional(12.5);
    let text_w = ui.painter().layout_no_wrap(label.to_owned(), font.clone(), pal.accent_text).size().x;
    let icon_w = if glyph.is_some() { 16.0 } else { 0.0 };
    let size = Vec2::new(text_w + icon_w + 22.0, 26.0);
    let (rect, response) = ui.allocate_exact_size(size, Sense::click());
    let bg = if response.is_pointer_button_down_on() {
        pal.accent.gamma_multiply(0.85)
    } else if response.hovered() {
        pal.accent.gamma_multiply(1.08)
    } else {
        pal.accent
    };
    ui.painter().rect_filled(rect, CornerRadius::same(6), bg);
    let mut x = rect.left() + 11.0;
    if let Some(glyph) = glyph {
        let icon_rect = egui::Rect::from_center_size(
            egui::pos2(x + 7.5, rect.center().y),
            Vec2::splat(15.0),
        );
        icons::paint_glyph(ui.painter(), icon_rect, glyph, pal.accent_text);
        x += 17.0;
    }
    ui.painter().text(
        egui::pos2(x, rect.center().y),
        egui::Align2::LEFT_CENTER,
        label,
        font,
        pal.accent_text,
    );
    response
}
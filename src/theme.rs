//! 设计系统：颜色、字体、间距、全局样式。
//!
//! 每个数值都对应 Web 版 app.css 的设计令牌（详见 磁盘情报系统/docs/研究-Web界面规格-20261113.md），
//! 目的：原生窗口与 Web 版是同一个产品的两种呈现，而不是两套皮肤。

use eframe::egui::{self, Color32, CornerRadius, FontData, FontDefinitions, FontFamily, FontId,
    Stroke, TextStyle, Visuals};
use eframe::egui::epaint::text::Blob;

// ---------- 间距节奏（4 px 基准） ----------
pub const ROW_H: f32 = 26.0;
pub const HEADER_H: f32 = 28.0;
pub const MENUBAR_H: f32 = 28.0;
pub const TOOLBAR_H: f32 = 36.0;
pub const FILTERBAR_H: f32 = 34.0;
pub const STATUSBAR_H: f32 = 24.0;
pub const TAB_H: f32 = 26.0;
pub const GUTTER: f32 = 8.0;
pub const RADIUS_SM: u8 = 4;
pub const RADIUS_MD: u8 = 6;
pub const RADIUS_LG: u8 = 10;

/// 一套配色。所有视图只从这里取色，不允许在函数里写死十六进制。
#[derive(Clone, Copy, Debug)]
pub struct Palette {
    pub dark: bool,
    pub bg: Color32,
    pub panel: Color32,
    pub panel_alt: Color32,
    pub card: Color32,
    pub hover: Color32,
    pub border: Color32,
    pub border_soft: Color32,
    pub text: Color32,
    pub text_dim: Color32,
    pub text_faint: Color32,
    pub accent: Color32,
    pub accent_weak: Color32,
    pub accent_text: Color32,
    pub selection: Color32,
    pub stripe: Color32,
    pub ok: Color32,
    pub warn: Color32,
    pub danger: Color32,
    pub shadow: Color32,
}

impl Palette {
    /// 深色主题：夜间/低光环境；灰阶取自 Web 版同一套明度关系。
    pub fn dark() -> Self {
        Self {
            dark: true,
            bg: Color32::from_rgb(0x12, 0x14, 0x18),
            panel: Color32::from_rgb(0x1a, 0x1d, 0x23),
            panel_alt: Color32::from_rgb(0x22, 0x26, 0x2e),
            card: Color32::from_rgb(0x1e, 0x22, 0x29),
            hover: Color32::from_rgb(0x28, 0x2f, 0x3a),
            border: Color32::from_rgb(0x30, 0x36, 0x41),
            border_soft: Color32::from_rgb(0x26, 0x2b, 0x33),
            text: Color32::from_rgb(0xe8, 0xea, 0xee),
            text_dim: Color32::from_rgb(0x9a, 0xa3, 0xb0),
            text_faint: Color32::from_rgb(0x70, 0x7a, 0x88),
            accent: Color32::from_rgb(0x5b, 0x9b, 0xff),
            accent_weak: Color32::from_rgb(0x1e, 0x2f, 0x4d),
            accent_text: Color32::from_rgb(0x0b, 0x12, 0x20),
            selection: Color32::from_rgb(0x24, 0x39, 0x5e),
            stripe: Color32::from_rgb(0x18, 0x1b, 0x21),
            ok: Color32::from_rgb(0x34, 0xc9, 0x8a),
            warn: Color32::from_rgb(0xe0, 0xa0, 0x3a),
            danger: Color32::from_rgb(0xe5, 0x64, 0x5f),
            shadow: Color32::from_rgba_premultiplied(0, 0, 0, 110),
        }
    }

    /// 浅色主题：与 Web 版逐值对齐（bg #f5f6f8 / panel #fff / accent #2f6fd0 ...）。
    pub fn light() -> Self {
        Self {
            dark: false,
            bg: Color32::from_rgb(0xf5, 0xf6, 0xf8),
            panel: Color32::from_rgb(0xff, 0xff, 0xff),
            panel_alt: Color32::from_rgb(0xf0, 0xf2, 0xf5),
            card: Color32::from_rgb(0xff, 0xff, 0xff),
            hover: Color32::from_rgb(0xe7, 0xf0, 0xfc),
            border: Color32::from_rgb(0xd9, 0xde, 0xe6),
            border_soft: Color32::from_rgb(0xee, 0xf0, 0xf4),
            text: Color32::from_rgb(0x1c, 0x22, 0x30),
            text_dim: Color32::from_rgb(0x68, 0x70, 0x7e),
            text_faint: Color32::from_rgb(0x8a, 0x92, 0x9f),
            accent: Color32::from_rgb(0x2f, 0x6f, 0xd0),
            accent_weak: Color32::from_rgb(0xe7, 0xf0, 0xfc),
            accent_text: Color32::from_rgb(0xff, 0xff, 0xff),
            selection: Color32::from_rgb(0xdb, 0xe9, 0xff),
            stripe: Color32::from_rgb(0xfa, 0xfb, 0xfc),
            ok: Color32::from_rgb(0x0f, 0x7a, 0x55),
            warn: Color32::from_rgb(0xa8, 0x45, 0x1f),
            danger: Color32::from_rgb(0xa4, 0x26, 0x2c),
            shadow: Color32::from_rgba_premultiplied(28, 34, 48, 31),
        }
    }

    pub fn toggle(&self) -> Self {
        if self.dark { Self::light() } else { Self::dark() }
    }

    /// 行底色：斑马 + 悬停 + 选中三态，全部从令牌派生。
    pub fn row_fill(&self, index: usize, hovered: bool, selected: bool) -> Color32 {
        if selected {
            self.selection
        } else if hovered {
            self.hover
        } else if index % 2 == 1 {
            self.stripe
        } else {
            Color32::TRANSPARENT
        }
    }
}

/// 系统字体栈：Segoe UI（拉丁）+ 微软雅黑 UI（CJK）+ Consolas（等宽）。
/// 在 egui 自带字体族前面插入，保留它的 emoji / 图标兜底，不破坏任何默认行为。
pub fn install_fonts(ctx: &egui::Context, scale: f32) {
    use std::sync::Arc;

    let mut fonts = FontDefinitions::default();

    let mut keys_prop: Vec<String> = Vec::new();
    let mut keys_mono: Vec<String> = Vec::new();

    let mut add = |key: &str, path: &str, index: u32| -> bool {
        let Ok(bytes) = std::fs::read(path) else {
            return false;
        };
        let blob: Blob = Arc::new(bytes);
        fonts
            .font_data
            .insert(key.to_owned(), Arc::new(FontData::from_blob(blob, index)));
        true
    };

    if add("latin", r"C:\Windows\Fonts\segoeui.ttf", 0) {
        keys_prop.push("latin".to_owned());
    }
    if add("latin_sb", r"C:\Windows\Fonts\seguisb.ttf", 0) {
        keys_prop.push("latin_sb".to_owned());
    }
    // msyh.ttc 第 0 面 Microsoft YaHei，第 1 面 Microsoft YaHei UI（界面用更紧凑）
    if add("cjk", r"C:\Windows\Fonts\msyh.ttc", 1)
        || add("cjk", r"C:\Windows\Fonts\msyh.ttc", 0)
        || add("cjk", r"C:\Windows\Fonts\simhei.ttf", 0)
    {
        keys_prop.push("cjk".to_owned());
        keys_mono.push("cjk".to_owned());
    }
    if add("mono", r"C:\Windows\Fonts\consola.ttf", 0) {
        keys_mono.insert(0, "mono".to_owned());
    }

    for family in [FontFamily::Proportional, FontFamily::Monospace] {
        let existing = fonts.families.entry(family.clone()).or_default().clone();
        let prefix: &Vec<String> = if family == FontFamily::Proportional {
            &keys_prop
        } else {
            &keys_mono
        };
        let mut merged = prefix.clone();
        merged.extend(existing);
        fonts.families.insert(family, merged);
    }

    ctx.set_fonts(fonts);

    ctx.all_styles_mut(|style| {
        style.text_styles = [
            (TextStyle::Heading, FontId::proportional(17.0 * scale)),
            (TextStyle::Body, FontId::proportional(12.5 * scale)),
            (TextStyle::Button, FontId::proportional(12.5 * scale)),
            (TextStyle::Small, FontId::proportional(11.0 * scale)),
            (TextStyle::Monospace, FontId::monospace(11.5 * scale)),
        ]
        .into();
    });
}

/// 把配色与间距灌进 egui 的全局样式（同时作用于深/浅两套主题）。
pub fn apply_style(ctx: &egui::Context, pal: &Palette, scale: f32) {
    let mut v = Visuals::dark();
    v.dark_mode = pal.dark;
    v.override_text_color = Some(pal.text);
    v.panel_fill = pal.panel;
    v.window_fill = pal.panel;
    v.window_stroke = Stroke::new(1.0, pal.border);
    v.window_corner_radius = CornerRadius::same(RADIUS_LG);
    v.menu_corner_radius = CornerRadius::same(RADIUS_MD);
    v.extreme_bg_color = pal.panel_alt;
    v.faint_bg_color = pal.stripe;
    v.code_bg_color = pal.panel_alt;
    v.hyperlink_color = pal.accent;
    v.warn_fg_color = pal.warn;
    v.error_fg_color = pal.danger;
    v.selection.bg_fill = pal.selection;
    v.selection.stroke = Stroke::new(1.0, pal.accent);
    v.button_frame = false;
    v.striped = false;
    v.indent_has_left_vline = false;

    let mk = |bg: Color32, weak: Color32, stroke_color: Color32, fg: Color32| egui::style::WidgetVisuals {
        bg_fill: bg,
        weak_bg_fill: weak,
        bg_stroke: Stroke::new(1.0, stroke_color),
        corner_radius: CornerRadius::same(RADIUS_SM),
        fg_stroke: Stroke::new(1.0, fg),
        expansion: 0.0,
    };
    v.widgets.noninteractive = mk(Color32::TRANSPARENT, Color32::TRANSPARENT, pal.border_soft, pal.text);
    v.widgets.inactive = mk(Color32::TRANSPARENT, Color32::TRANSPARENT, Color32::TRANSPARENT, pal.text);
    v.widgets.hovered = mk(pal.hover, pal.hover, pal.border_soft, pal.text);
    v.widgets.active = mk(pal.accent_weak, pal.accent_weak, pal.accent, pal.text);
    v.widgets.open = mk(pal.panel_alt, pal.panel_alt, pal.border, pal.text);

    let mut scroll = egui::style::ScrollStyle::floating();
    scroll.bar_width = 9.0;
    scroll.bar_inner_margin = 2.0;
    scroll.bar_outer_margin = 2.0;

    ctx.all_styles_mut(|style| {
        style.visuals = v.clone();
        style.spacing.item_spacing = egui::vec2(8.0, 6.0);
        style.spacing.button_padding = egui::vec2(10.0, 4.0);
        style.spacing.window_margin = egui::Margin::same(10);
        style.spacing.menu_margin = egui::Margin::same(6);
        style.spacing.indent = 16.0;
        style.spacing.interact_size.y = 24.0 * scale;
        style.spacing.scroll = scroll.clone();
    });
}

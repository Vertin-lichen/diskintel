//! 图标系统：16×16 矢量直绘（对应 Web 版 app.js 的 ICONS / iconSvg）。
//!
//! 全部用 Painter 现画，不依赖图片资源、不依赖字体图标，任意 DPI 下都清晰；
//! 颜色沿用 Web 版同一套十六进制，保证两个客户端「一眼是同一个产品」。

use eframe::egui::{self, Color32, CornerRadius, Painter, Pos2, Rect, Shape, Stroke, vec2};
use eframe::egui::epaint::PathShape;

// ---------- 类型与配色（与 Web 版逐字对齐） ----------

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Kind {
    Folder, Doc, Ppt, Sheet, Pdf, Archive, Image, Media, Code, Exe, Db, Other,
}

pub fn classify(path: &str, is_dir: bool) -> Kind {
    if is_dir {
        return Kind::Folder;
    }
    let ext = path
        .rsplit(['\\', '/'])
        .next()
        .and_then(|name| name.rsplit_once('.').map(|(_, e)| e))
        .unwrap_or("")
        .to_ascii_lowercase();
    match ext.as_str() {
        "doc" | "docx" | "rtf" | "odt" | "txt" | "wps" | "md" => Kind::Doc,
        "ppt" | "pptx" | "pps" | "odp" | "dps" => Kind::Ppt,
        "xls" | "xlsx" | "csv" | "ods" | "et" => Kind::Sheet,
        "pdf" => Kind::Pdf,
        "zip" | "rar" | "7z" | "tar" | "gz" | "bz2" | "xz" | "cab" | "iso" | "tgz" => Kind::Archive,
        "png" | "jpg" | "jpeg" | "gif" | "bmp" | "webp" | "svg" | "ico" | "tif" | "tiff" | "psd" => Kind::Image,
        "mp3" | "wav" | "flac" | "aac" | "m4a" | "mp4" | "mkv" | "avi" | "mov" | "wmv" | "flv" => Kind::Media,
        "py" | "js" | "ts" | "jsx" | "tsx" | "html" | "css" | "json" | "xml" | "yaml" | "yml" | "java"
        | "kt" | "c" | "cpp" | "h" | "go" | "rs" | "sh" | "ps1" | "sql" | "ini" | "toml" => Kind::Code,
        "exe" | "msi" | "dll" | "sys" | "bat" | "cmd" | "com" | "scr" => Kind::Exe,
        "db" | "sqlite" | "sqlite3" | "mdb" => Kind::Db,
        _ => Kind::Other,
    }
}

pub fn color(kind: Kind) -> Color32 {
    match kind {
        Kind::Folder => Color32::from_rgb(0xF0, 0xA0, 0x20),
        Kind::Doc => Color32::from_rgb(0x3B, 0x6F, 0xD4),
        Kind::Ppt => Color32::from_rgb(0xDE, 0x5A, 0x16),
        Kind::Sheet => Color32::from_rgb(0x23, 0x8A, 0x54),
        Kind::Pdf => Color32::from_rgb(0xD6, 0x3B, 0x3B),
        Kind::Archive => Color32::from_rgb(0xE0, 0x8A, 0x0C),
        Kind::Image => Color32::from_rgb(0x8E, 0x44, 0xAD),
        Kind::Media => Color32::from_rgb(0xC2, 0x25, 0x5C),
        Kind::Code => Color32::from_rgb(0x0B, 0x7C, 0x8C),
        Kind::Exe => Color32::from_rgb(0x4B, 0x55, 0x63),
        Kind::Db => Color32::from_rgb(0x0B, 0x8A, 0x6B),
        Kind::Other => Color32::from_rgb(0x7C, 0x85, 0x95),
    }
}

pub fn label(kind: Kind) -> &'static str {
    match kind {
        Kind::Folder => "文件夹",
        Kind::Doc => "文档",
        Kind::Ppt => "演示文稿",
        Kind::Sheet => "表格",
        Kind::Pdf => "PDF",
        Kind::Archive => "压缩包",
        Kind::Image => "图片",
        Kind::Media => "音视频",
        Kind::Code => "代码 / 文本",
        Kind::Exe => "程序",
        Kind::Db => "数据库",
        Kind::Other => "文件",
    }
}

fn white(alpha: u8) -> Color32 {
    Color32::from_rgba_unmultiplied(255, 255, 255, alpha)
}

// ---------- 16×16 画布 ----------

struct Canvas<'a> {
    p: &'a Painter,
    origin: Pos2,
    s: f32,
}

impl<'a> Canvas<'a> {
    fn new(p: &'a Painter, rect: Rect) -> Self {
        Self { p, origin: rect.min, s: rect.width() / 16.0 }
    }
    fn at(&self, x: f32, y: f32) -> Pos2 {
        self.origin + vec2(x * self.s, y * self.s)
    }
    fn rect(&self, x: f32, y: f32, w: f32, h: f32) -> Rect {
        Rect::from_min_size(self.at(x, y), vec2(w * self.s, h * self.s))
    }
    fn rr(&self, x: f32, y: f32, w: f32, h: f32, radius: f32, fill: Color32) {
        let r = (radius * self.s).round().max(0.0) as u8;
        self.p.rect_filled(self.rect(x, y, w, h), CornerRadius::same(r), fill);
    }
    fn poly(&self, pts: &[(f32, f32)], fill: Color32) {
        let v: Vec<Pos2> = pts.iter().map(|&(x, y)| self.at(x, y)).collect();
        self.p.add(PathShape::convex_polygon(v, fill, Stroke::NONE));
    }
    fn line(&self, pts: &[(f32, f32)], stroke: Stroke) {
        let v: Vec<Pos2> = pts.iter().map(|&(x, y)| self.at(x, y)).collect();
        self.p.add(Shape::line(v, stroke));
    }
    fn dot(&self, x: f32, y: f32, r: f32, fill: Color32) {
        self.p.circle_filled(self.at(x, y), r * self.s, fill);
    }
    fn ring(&self, x: f32, y: f32, r: f32, stroke: Stroke) {
        self.p.circle_stroke(self.at(x, y), r * self.s, stroke);
    }
}

/// 画一枚文件/文件夹图标；rect 应为正方形（建议 16px 或 18px）。
pub fn paint_file(painter: &Painter, rect: Rect, kind: Kind) {
    let c = Canvas::new(painter, rect);
    let tint = color(kind);
    let w1 = Stroke::new(1.15 * c.s, white(240));
    let w07 = Stroke::new(0.85 * c.s, white(215));

    match kind {
        Kind::Folder => {
            // 两段式文件夹：后片略深，前片亮一档，叠出层次
            c.rr(1.1, 2.5, 5.6, 3.6, 1.2, tint.gamma_multiply(0.78));
            c.rr(1.1, 4.1, 13.8, 9.6, 1.8, tint);
            c.rr(1.1, 4.1, 13.8, 0.9, 0.9, white(30));
        }
        _ => {
            let page = [(3.2, 1.2), (8.9, 1.2), (12.9, 5.2), (12.9, 14.8), (3.2, 14.8)];
            c.poly(&page, tint);
            // 折角用更亮的同色，而不是白色：小尺寸下才读得出「一页纸」而不是一个色块
            c.poly(&[(8.9, 1.2), (12.9, 5.2), (8.9, 5.2)], tint.gamma_multiply(1.55));
        }
    }

    match kind {
        Kind::Folder | Kind::Other => {}
        Kind::Doc | Kind::Pdf => {
            c.line(&[(5.6, 8.4), (11.0, 8.4)], w1);
            c.line(&[(5.6, 10.4), (11.0, 10.4)], w1);
            c.line(&[(5.6, 12.4), (9.2, 12.4)], w1);
        }
        Kind::Ppt => {
            c.rr(4.2, 7.4, 7.4, 5.4, 0.7, white(235));
            c.dot(7.9, 10.1, 1.35, tint);
        }
        Kind::Sheet => {
            c.rr(4.1, 7.4, 7.6, 5.4, 0.5, white(235));
            let g = Stroke::new(0.7 * c.s, tint);
            c.line(&[(4.1, 9.2), (11.7, 9.2)], g);
            c.line(&[(4.1, 11.0), (11.7, 11.0)], g);
            c.line(&[(6.6, 7.4), (6.6, 12.8)], g);
            c.line(&[(9.1, 7.4), (9.1, 12.8)], g);
        }
        Kind::Archive => {
            c.line(&[(5.6, 7.8), (10.4, 7.8), (10.4, 12.8), (5.6, 12.8), (5.6, 7.8)], w07);
            for y in [8.4, 9.8, 11.2, 12.2] {
                c.line(&[(7.6, y), (8.4, y)], w1);
            }
        }
        Kind::Image => {
            c.dot(6.3, 9.0, 1.0, white(235));
            c.poly(&[(4.5, 12.9), (7.2, 9.9), (8.9, 11.6), (10.4, 10.2), (12.0, 12.9)], white(235));
        }
        Kind::Media => {
            c.poly(&[(6.4, 8.4), (11.2, 10.8), (6.4, 13.2)], white(235));
        }
        Kind::Code => {
            c.line(&[(6.4, 8.8), (4.7, 10.6), (6.4, 12.4)], w1);
            c.line(&[(9.6, 8.8), (11.3, 10.6), (9.6, 12.4)], w1);
        }
        Kind::Exe => {
            c.rr(4.3, 7.6, 7.4, 5.2, 0.6, white(235));
            c.line(&[(4.3, 9.1), (11.7, 9.1)], Stroke::new(0.8 * c.s, tint));
            c.dot(5.4, 8.4, 0.4, tint);
        }
        Kind::Db => {
            let stroke = Stroke::new(0.9 * c.s, white(235));
            c.ring(8.0, 8.9, 1.0, stroke);
            c.line(&[(4.8, 8.9), (4.8, 12.0)], stroke);
            c.line(&[(11.2, 8.9), (11.2, 12.0)], stroke);
            c.line(&[(4.8, 12.0), (6.0, 12.9), (10.0, 12.9), (11.2, 12.0)], stroke);
            c.line(&[(5.4, 9.2), (10.6, 9.2)], Stroke::new(0.5 * c.s, white(120)));
        }
    }
}

// ---------- 界面动作图标 ----------

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Glyph {
    Search, ChevronDown, ChevronUp, ChevronRight, Close, Refresh, SortAsc, SortDesc,
    Sliders, Disk, FolderUp, Copy, External, Scan, Play, Check, Clock, Filter, Plus,
    Left, Right, ZoomIn, ZoomOut, Collapse, Expand,
}

/// 通用动作图标；线宽按 16 网格 1.5，视觉上接近 Web 版的 1.5px 描边。
pub fn paint_glyph(painter: &Painter, rect: Rect, glyph: Glyph, color: Color32) {
    let c = Canvas::new(painter, rect);
    let s = Stroke::new(1.5 * c.s, color);
    match glyph {
        Glyph::Search => {
            c.ring(6.9, 6.9, 3.9, s);
            c.line(&[(9.8, 9.8), (12.6, 12.6)], s);
        }
        Glyph::ChevronDown => c.line(&[(4.4, 6.4), (8.0, 10.0), (11.6, 6.4)], s),
        Glyph::ChevronUp => c.line(&[(4.4, 9.6), (8.0, 6.0), (11.6, 9.6)], s),
        Glyph::ChevronRight => c.line(&[(6.4, 4.4), (10.0, 8.0), (6.4, 11.6)], s),
        Glyph::Left => c.line(&[(9.6, 3.6), (5.6, 8.0), (9.6, 12.4)], s),
        Glyph::Right => c.line(&[(6.4, 3.6), (10.4, 8.0), (6.4, 12.4)], s),
        Glyph::Close => {
            c.line(&[(4.6, 4.6), (11.4, 11.4)], s);
            c.line(&[(11.4, 4.6), (4.6, 11.4)], s);
        }
        Glyph::Refresh | Glyph::Scan => {
            c.ring(8.0, 8.0, 4.6, s);
            c.poly(&[(10.6, 1.6), (12.9, 4.4), (9.6, 5.0)], color);
        }
        Glyph::SortAsc => {
            c.line(&[(8.0, 12.0), (8.0, 4.2)], s);
            c.line(&[(4.6, 7.4), (8.0, 4.0), (11.4, 7.4)], s);
        }
        Glyph::SortDesc => {
            c.line(&[(8.0, 4.0), (8.0, 11.8)], s);
            c.line(&[(4.6, 8.6), (8.0, 12.0), (11.4, 8.6)], s);
        }
        Glyph::Sliders => {
            for (y, x) in [(4.6, 6.6), (8.0, 9.8), (11.4, 5.4)] {
                c.line(&[(2.8, y), (13.2, y)], Stroke::new(1.3 * c.s, color));
                c.dot(x, y, 1.6, color);
            }
        }
        Glyph::Disk => {
            c.ring(8.0, 8.0, 5.2, s);
            c.ring(8.0, 8.0, 1.5, s);
        }
        Glyph::FolderUp => {
            c.line(&[(2.6, 12.4), (2.6, 5.2), (6.0, 5.2), (7.2, 6.6), (13.4, 6.6), (13.4, 12.4), (2.6, 12.4)], Stroke::new(1.3 * c.s, color));
            c.line(&[(8.0, 11.0), (8.0, 8.0)], s);
            c.line(&[(6.4, 9.4), (8.0, 7.8), (9.6, 9.4)], s);
        }
        Glyph::Copy => {
            c.line(&[(5.6, 3.6), (13.2, 3.6), (13.2, 11.4)], s);
            c.line(&[(2.8, 5.6), (10.4, 5.6), (10.4, 13.4), (2.8, 13.4), (2.8, 5.6)], s);
        }
        Glyph::External => {
            c.line(&[(9.2, 2.8), (13.2, 2.8), (13.2, 6.8)], s);
            c.line(&[(13.0, 3.0), (7.6, 8.4)], s);
            c.line(&[(11.0, 9.0), (11.0, 13.2), (2.8, 13.2), (2.8, 5.0), (7.0, 5.0)], s);
        }
        Glyph::Play => c.poly(&[(5.6, 3.6), (12.4, 8.0), (5.6, 12.4)], color),
        Glyph::Check => c.line(&[(3.2, 8.4), (6.6, 11.8), (12.8, 4.4)], s),
        Glyph::Clock => {
            c.ring(8.0, 8.0, 5.2, s);
            c.line(&[(8.0, 4.8), (8.0, 8.2), (10.8, 9.8)], s);
        }
        Glyph::Filter => c.poly(&[(2.6, 3.4), (13.4, 3.4), (9.2, 8.6), (9.2, 13.0), (6.8, 11.6), (6.8, 8.6)], color),
        Glyph::Plus => {
            c.line(&[(8.0, 3.4), (8.0, 12.6)], s);
            c.line(&[(3.4, 8.0), (12.6, 8.0)], s);
        }
        Glyph::ZoomIn => {
            c.ring(6.9, 6.9, 3.9, s);
            c.line(&[(9.8, 9.8), (12.6, 12.6)], s);
            c.line(&[(5.1, 6.9), (8.7, 6.9)], Stroke::new(1.2 * c.s, color));
            c.line(&[(6.9, 5.1), (6.9, 8.7)], Stroke::new(1.2 * c.s, color));
        }
        Glyph::ZoomOut => {
            c.ring(6.9, 6.9, 3.9, s);
            c.line(&[(9.8, 9.8), (12.6, 12.6)], s);
            c.line(&[(5.1, 6.9), (8.7, 6.9)], Stroke::new(1.2 * c.s, color));
        }
        Glyph::Collapse => {
            c.line(&[(4.4, 6.4), (8.0, 2.8), (11.6, 6.4)], s);
            c.line(&[(4.4, 9.6), (8.0, 13.2), (11.6, 9.6)], s);
        }
        Glyph::Expand => {
            c.line(&[(4.4, 2.8), (8.0, 6.4), (11.6, 2.8)], s);
            c.line(&[(4.4, 13.2), (8.0, 9.6), (11.6, 13.2)], s);
        }
    }
}

/// 品牌标识：同心圆盘 + 高亮指针，用于菜单栏左上角。
pub fn paint_logo(painter: &Painter, rect: Rect, accent: Color32, text: Color32) {
    let c = Canvas::new(painter, rect);
    c.ring(8.0, 8.0, 6.6, Stroke::new(1.6 * c.s, accent));
    c.ring(8.0, 8.0, 3.4, Stroke::new(1.2 * c.s, accent));
    c.dot(8.0, 8.0, 1.5, text);
    c.line(&[(11.6, 4.4), (13.6, 2.4)], Stroke::new(1.4 * c.s, accent));
}
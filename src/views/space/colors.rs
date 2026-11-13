//! 树图染色：10 类路径规则配色（规格 5.3）+ 墨色判定 + 聚合块色。
//!
//! 这是全局令牌的唯一例外：类别色按规格 5.3 的固定值取，因为它们是「类别语义色」，
//! 与主题无关；其余一切颜色仍来自 theme::Palette。

use eframe::egui::Color32;

use crate::icons;

/// 一条染色规则：RGB + 关键词表（按序首个命中；路径不区分大小写）。
type Rule = ((u8, u8, u8), &'static [&'static str]);

/// 10 类，色值见规格 5.3。
const CATEGORIES: [Rule; 10] = [
    ((0x3B, 0x5B, 0xDB), &["windows", "system32", "winsxs", "programdata", "drivers"]),
    ((0x0C, 0xA6, 0x78), &["program files", "node_modules", ".venv", "site-packages"]),
    ((0xF0, 0x8C, 0x00), &["documents", "desktop", "downloads", "文档", "桌面", "下载"]),
    ((0xE6, 0x49, 0x80), &["video", "music", "picture", "media", "视频", "音频", "图片", "电影"]),
    ((0x70, 0x48, 0xE8), &["src", "repo", "project", "git", "workspace", "代码", "项目"]),
    ((0xE0, 0x31, 0x31), &["cache", "temp", "tmp", "log", "缓存", "临时"]),
    ((0xFD, 0x7E, 0x14), &[".zip", ".rar", ".7z", ".iso", "archive", "备份", "镜像"]),
    ((0x08, 0x7F, 0x5B), &[".sqlite", ".db", "database", "数据库"]),
    ((0x10, 0x98, 0xAD), &[".vhdx", ".vmdk", "virtual", "vmware", "虚拟机"]),
    ((0x7C, 0x85, 0x95), &[]),
];

/// 聚合块固定色（规格 5.3）。
pub const AGGREGATE: Color32 = Color32::from_rgb(0x5C, 0x63, 0x6E);

/// 目录按路径关键词归类；无命中 → 其他。
pub fn category(path: &str) -> Color32 {
    let lower = path.to_ascii_lowercase();
    for &((r, g, b), keys) in CATEGORIES.iter() {
        for key in keys.iter() {
            if lower.contains(*key) {
                return Color32::from_rgb(r, g, b);
            }
        }
    }
    Color32::from_rgb(0x7C, 0x85, 0x95)
}

/// FNV-1a：同一路径恒得同一抖动，窗口宽高变化不换色（规格 5.3 的 seed 用路径而非坐标）。
fn fnv1a(text: &str) -> u32 {
    let mut hash: u32 = 0x811C_9DC5;
    for byte in text.as_bytes() {
        hash ^= u32::from(*byte);
        hash = hash.wrapping_mul(0x0100_0193);
    }
    hash
}

/// 抖动：RGB 乘 factor ∈ [0.82, 1.15]，让同类目录也能互相区分（规格 5.3）。
pub fn vary(base: Color32, seed: &str) -> Color32 {
    let factor = 0.82 + (fnv1a(seed) % 34) as f32 / 100.0;
    Color32::from_rgb(
        (base.r() as f32 * factor).min(255.0) as u8,
        (base.g() as f32 * factor).min(255.0) as u8,
        (base.b() as f32 * factor).min(255.0) as u8,
    )
}

/// 瓷砖底色：目录走类别色（带抖动），文件走文件类型图标色（与列表图标同源，不做抖动）。
pub fn tile(path: &str, is_dir: bool) -> Color32 {
    if is_dir {
        vary(category(path), path)
    } else {
        icons::color(icons::classify(path, false))
    }
}

/// 墨色：按底色亮度二选一（规格 5.3），深底白字、浅底深字。
pub fn ink(bg: Color32) -> Color32 {
    let lum = 0.299 * bg.r() as f32 + 0.587 * bg.g() as f32 + 0.114 * bg.b() as f32;
    if lum / 255.0 > 0.62 {
        Color32::from_rgb(0x10, 0x23, 0x3F)
    } else {
        Color32::WHITE
    }
}

/// 1px 白缝：深色主题用半透明白（形成亮网格），浅色主题用不透明白。
pub fn seam(dark: bool) -> Color32 {
    if dark {
        Color32::from_rgba_unmultiplied(255, 255, 255, 78)
    } else {
        Color32::from_rgba_unmultiplied(255, 255, 255, 225)
    }
}

/// 图例条目：类别名 + 色（供底部图例使用）。
pub const LEGEND: [(&str, Color32); 10] = [
    ("系统", Color32::from_rgb(0x3B, 0x5B, 0xDB)),
    ("程序", Color32::from_rgb(0x0C, 0xA6, 0x78)),
    ("用户文档", Color32::from_rgb(0xF0, 0x8C, 0x00)),
    ("媒体", Color32::from_rgb(0xE6, 0x49, 0x80)),
    ("开发", Color32::from_rgb(0x70, 0x48, 0xE8)),
    ("缓存临时", Color32::from_rgb(0xE0, 0x31, 0x31)),
    ("归档镜像", Color32::from_rgb(0xFD, 0x7E, 0x14)),
    ("数据库", Color32::from_rgb(0x08, 0x7F, 0x5B)),
    ("虚拟机", Color32::from_rgb(0x10, 0x98, 0xAD)),
    ("其他", Color32::from_rgb(0x7C, 0x85, 0x95)),
];

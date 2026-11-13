//! 本机设置：%APPDATA%\DiskIntel\settings.json。
//!
//! 0.3.3 起扩到「快照策略 + 托盘 + 折叠」一组开关，刻意仍不引入 serde：
//! 扁平 JSON 手写解析比多一个依赖划算；任何文件缺失 / 截断 / 乱码 / 类型不对
//! 都逐字段退回默认值并在 stderr 留一行说明，绝不 panic。
//!
//! 向后兼容：0.3.1 / 0.3.2 写出的只有前三个字段，读旧文件时新字段全部取默认值。
//!
//! 快照口径（对应机主疑问「快照是自动还是手动」）：
//! - 默认：首次安装必建（没有快照时启动 3 秒后自动建一份），以后每 7 天自动建一次；
//! - auto_snapshot_on_start：每次启动都建（不论距上次多久）；
//! - auto_snapshot_every_days = 0 表示关闭周期自动快照。

use std::path::{Path, PathBuf};

/// 旧版（0.3.1）自动建快照的最小间隔（分钟）：0.3.3 起由 auto_snapshot_every_days 接管，
/// 字段仍保留读写，避免旧设置文件被覆盖时丢信息。
pub const DEFAULT_MIN_INTERVAL_MINUTES: u64 = 60;
/// 周期自动快照默认间隔天数。
pub const DEFAULT_EVERY_DAYS: u64 = 7;
/// 默认保留快照份数（自动清理超出部分）。
pub const DEFAULT_KEEP_COUNT: u64 = 8;
/// 周期天数上下限：0 = 关闭；上限一年，防止手改出天文数字。
const MAX_EVERY_DAYS: u64 = 365;
/// 保留份数上限（0 = 不自动清理）。
const MAX_KEEP_COUNT: u64 = 500;

/// 本机设置。字段全部可选：JSON 里缺的项取默认值。
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Settings {
    // ---------- 0.3.1 / 0.3.2 已有字段（保持读写兼容） ----------
    /// 启动时自动建快照（不受周期天数限制；默认关闭）。
    pub auto_snapshot_on_start: bool,
    /// 旧版最小间隔（分钟）：仅保留兼容，0.3.3 起不再参与判断。
    pub auto_snapshot_min_interval_minutes: u64,
    /// 启动时静默检查一次更新（默认关闭；结果只在发现新版本时提示）。
    pub check_update_on_start: bool,

    // ---------- 0.3.3 新增 ----------
    /// 点窗口关闭按钮时隐藏到托盘（默认开启；托盘菜单「退出」才是真退出）。
    pub close_to_tray: bool,
    /// 启动时最小化（默认关闭）。
    pub start_minimized: bool,
    /// 周期自动快照间隔天数（0 = 关闭；默认 7 天）。
    pub auto_snapshot_every_days: u64,
    /// 自定义快照目录（空 = <数据根>\snapshots）。
    pub snapshot_dir: String,
    /// 自动保留的快照份数（0 = 不自动清理；默认 8）。
    pub keep_snapshot_count: u64,
    /// 建完快照后后台自动转 v4（默认开启；转换产物下次启动前落位）。
    pub convert_snapshot_v4: bool,
    /// 检索结果低价值折叠总开关（默认开启）。
    pub fold_enabled: bool,
    /// 冷门扩展名（Rare 桶）也折叠（默认开启）。
    pub fold_rare: bool,
    /// 折叠豁免扩展名，逗号分隔（默认空，由 lowvalue 内置规则兜底）。
    pub fold_keep_exts: String,
    /// 折叠豁免路径前缀，逗号分隔（默认空）。
    pub fold_keep_prefixes: String,
}

impl Default for Settings {
    fn default() -> Self {
        Self {
            auto_snapshot_on_start: false,
            auto_snapshot_min_interval_minutes: DEFAULT_MIN_INTERVAL_MINUTES,
            check_update_on_start: false,
            close_to_tray: true,
            start_minimized: false,
            auto_snapshot_every_days: DEFAULT_EVERY_DAYS,
            snapshot_dir: String::new(),
            keep_snapshot_count: DEFAULT_KEEP_COUNT,
            convert_snapshot_v4: false,
            fold_enabled: true,
            fold_rare: true,
            fold_keep_exts: String::new(),
            fold_keep_prefixes: String::new(),
        }
    }
}

impl Settings {
    /// 把数值夹到合理区间、清理文本字段，防止手改文件写出越界值。
    pub fn normalized(mut self) -> Self {
        self.auto_snapshot_min_interval_minutes =
            self.auto_snapshot_min_interval_minutes.clamp(1, 10_080);
        self.auto_snapshot_every_days = self.auto_snapshot_every_days.min(MAX_EVERY_DAYS);
        self.keep_snapshot_count = self.keep_snapshot_count.min(MAX_KEEP_COUNT);
        self.snapshot_dir = self.snapshot_dir.trim().to_owned();
        self.fold_keep_exts = normalize_list(&self.fold_keep_exts);
        self.fold_keep_prefixes = normalize_list(&self.fold_keep_prefixes);
        self
    }

    /// 把设置翻译成检索页折叠策略：用户清单叠加在内置默认清单之上。
    ///
    /// `fold_keep_exts` 是「额外保留」而不是「替换默认」：默认清单本身是产品口径
    /// （文档/图片/镜像等），用户再加的条目只会让更多文件可见。
    pub fn fold_config(&self) -> crate::lowvalue::FoldConfig {
        let mut cfg = crate::lowvalue::FoldConfig::default();
        cfg.enabled = self.fold_enabled;
        cfg.fold_rare = self.fold_rare;
        for ext in Self::split_list(&self.fold_keep_exts) {
            let ext = ext.trim_start_matches('.').to_ascii_lowercase();
            if !ext.is_empty() && !cfg.keep_exts.iter().any(|known| known.eq_ignore_ascii_case(&ext)) {
                cfg.keep_exts.push(ext);
            }
        }
        cfg.keep_prefixes = Self::split_list(&self.fold_keep_prefixes);
        cfg
    }

    /// 用户清单按逗号/分号/换行拆开（路径里不会出现这些分隔符）。
    fn split_list(raw: &str) -> Vec<String> {
        raw.split(|ch| ch == ',' || ch == ';' || ch == '\n' || ch == '\r')
            .map(str::trim)
            .filter(|part| !part.is_empty())
            .map(str::to_owned)
            .collect()
    }

    /// 实际生效的快照目录：snapshot_dir 为空时用 <数据根>\snapshots。
    pub fn effective_snapshots_dir(&self, data_root: &Path) -> PathBuf {
        if self.snapshot_dir.trim().is_empty() {
            data_root.join("snapshots")
        } else {
            PathBuf::from(self.snapshot_dir.trim())
        }
    }

    /// 是否配置了自定义快照目录（决定建库后要不要搬运文件）。
    pub fn has_custom_snapshot_dir(&self) -> bool {
        !self.snapshot_dir.trim().is_empty()
    }
}

/// 逗号分隔列表归一化：去空白、去空项、去重（保持原顺序）。
fn normalize_list(raw: &str) -> String {
    let mut parts: Vec<&str> = Vec::new();
    for part in raw.split(',') {
        let part = part.trim();
        if !part.is_empty() && !parts.iter().any(|known| known.eq_ignore_ascii_case(part)) {
            parts.push(part);
        }
    }
    parts.join(",")
}

/// 设置文件路径：%APPDATA%\DiskIntel\settings.json。
///
/// 取不到 APPDATA 时退到临时目录，保证「读不到就用默认值」而不是报错退出。
pub fn path() -> PathBuf {
    let base = std::env::var_os("APPDATA")
        .map(PathBuf::from)
        .unwrap_or_else(std::env::temp_dir);
    base.join("DiskIntel").join("settings.json")
}

/// 读取设置；文件不存在或内容损坏都返回默认值。
pub fn load() -> Settings {
    load_from(&path())
}

/// 写回设置（父目录不存在会先建）；失败只记日志，不影响界面。
pub fn save(settings: &Settings) {
    save_to(&path(), settings);
}

fn load_from(path: &Path) -> Settings {
    let Ok(text) = std::fs::read_to_string(path) else {
        return Settings::default();
    };
    parse(&text)
}

fn save_to(path: &Path, settings: &Settings) {
    if let Some(dir) = path.parent() {
        let _ = std::fs::create_dir_all(dir);
    }
    if let Err(error) = std::fs::write(path, render(settings)) {
        eprintln!("[settings] 写入失败：{error}");
    }
}

/// 序列化成单行扁平 JSON（字段固定，方便人肉查看与手改）。
fn render(settings: &Settings) -> String {
    format!(
        concat!(
            "{{\"auto_snapshot_on_start\":{},\"auto_snapshot_min_interval_minutes\":{},",
            "\"check_update_on_start\":{},\"close_to_tray\":{},\"start_minimized\":{},",
            "\"auto_snapshot_every_days\":{},\"snapshot_dir\":\"{}\",",
            "\"keep_snapshot_count\":{},\"convert_snapshot_v4\":{},",
            "\"fold_enabled\":{},\"fold_rare\":{},\"fold_keep_exts\":\"{}\",",
            "\"fold_keep_prefixes\":\"{}\"}}\n"
        ),
        settings.auto_snapshot_on_start,
        settings.auto_snapshot_min_interval_minutes,
        settings.check_update_on_start,
        settings.close_to_tray,
        settings.start_minimized,
        settings.auto_snapshot_every_days,
        escape_json(&settings.snapshot_dir),
        settings.keep_snapshot_count,
        settings.convert_snapshot_v4,
        settings.fold_enabled,
        settings.fold_rare,
        escape_json(&settings.fold_keep_exts),
        escape_json(&settings.fold_keep_prefixes),
    )
}

/// JSON 字符串转义（只覆盖设置里可能出现的字符）。
fn escape_json(text: &str) -> String {
    let mut out = String::with_capacity(text.len() + 2);
    for ch in text.chars() {
        match ch {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            c if (c as u32) < 0x20 => out.push_str(&format!("\\u{:04x}", c as u32)),
            c => out.push(c),
        }
    }
    out
}

/// 解析设置：从默认值出发，能认出来的字段覆盖，认不出来的一律忽略。
fn parse(text: &str) -> Settings {
    let mut settings = Settings::default();
    if let Some(value) = bool_field(text, "auto_snapshot_on_start") {
        settings.auto_snapshot_on_start = value;
    }
    if let Some(value) = u64_field(text, "auto_snapshot_min_interval_minutes") {
        settings.auto_snapshot_min_interval_minutes = value;
    }
    if let Some(value) = bool_field(text, "check_update_on_start") {
        settings.check_update_on_start = value;
    }
    if let Some(value) = bool_field(text, "close_to_tray") {
        settings.close_to_tray = value;
    }
    if let Some(value) = bool_field(text, "start_minimized") {
        settings.start_minimized = value;
    }
    if let Some(value) = u64_field(text, "auto_snapshot_every_days") {
        settings.auto_snapshot_every_days = value;
    }
    if let Some(value) = str_field(text, "snapshot_dir") {
        settings.snapshot_dir = value;
    }
    if let Some(value) = u64_field(text, "keep_snapshot_count") {
        settings.keep_snapshot_count = value;
    }
    if let Some(value) = bool_field(text, "convert_snapshot_v4") {
        settings.convert_snapshot_v4 = value;
    }
    if let Some(value) = bool_field(text, "fold_enabled") {
        settings.fold_enabled = value;
    }
    if let Some(value) = bool_field(text, "fold_rare") {
        settings.fold_rare = value;
    }
    if let Some(value) = str_field(text, "fold_keep_exts") {
        settings.fold_keep_exts = value;
    }
    if let Some(value) = str_field(text, "fold_keep_prefixes") {
        settings.fold_keep_prefixes = value;
    }
    settings.normalized()
}

/// 找到某个键名后面冒号之后的内容（去掉前后空白）。
fn after_key<'a>(text: &'a str, key: &str) -> Option<&'a str> {
    let needle = format!("\"{key}\"");
    let start = text.find(&needle)? + needle.len();
    let rest = text[start..].trim_start();
    let rest = rest.strip_prefix(':')?.trim_start();
    Some(rest)
}

fn bool_field(text: &str, key: &str) -> Option<bool> {
    let rest = after_key(text, key)?;
    if rest.starts_with("true") {
        Some(true)
    } else if rest.starts_with("false") {
        Some(false)
    } else {
        None
    }
}

fn u64_field(text: &str, key: &str) -> Option<u64> {
    let rest = after_key(text, key)?;
    let digits: String = rest.chars().take_while(char::is_ascii_digit).collect();
    if digits.is_empty() {
        None
    } else {
        digits.parse().ok()
    }
}

/// 读一个 JSON 字符串字段；类型不对（数字 / 对象 / 未闭合）返回 None 走默认值。
fn str_field(text: &str, key: &str) -> Option<String> {
    let rest = after_key(text, key)?;
    let rest = rest.strip_prefix('"')?;
    let mut out = String::new();
    let mut chars = rest.chars();
    loop {
        match chars.next()? {
            '"' => return Some(out),
            '\\' => match chars.next()? {
                '"' => out.push('"'),
                '\\' => out.push('\\'),
                '/' => out.push('/'),
                'b' => out.push('\u{0008}'),
                'f' => out.push('\u{000c}'),
                'n' => out.push('\n'),
                'r' => out.push('\r'),
                't' => out.push('\t'),
                'u' => {
                    let mut code = 0u32;
                    for _ in 0..4 {
                        let Some(digit) = chars.next().and_then(|c| c.to_digit(16)) else {
                            return Some(out);
                        };
                        code = code * 16 + digit;
                    }
                    out.push(char::from_u32(code).unwrap_or('\u{fffd}'));
                }
                other => out.push(other),
            },
            ch => out.push(ch),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{
        DEFAULT_EVERY_DAYS, DEFAULT_KEEP_COUNT, Settings, load_from, parse, render, save_to,
    };
    use std::path::Path;

    /// 文件缺失：返回默认值，不报错、不 panic。
    #[test]
    fn missing_file_yields_defaults() {
        let path = std::env::temp_dir().join(format!(
            "diskintel-settings-missing-{}.json",
            std::process::id()
        ));
        assert!(!path.exists());
        assert_eq!(load_from(&path), Settings::default());
    }

    /// 损坏文件（乱码 / 截断 / 空文件）都退回默认值。
    #[test]
    fn corrupted_file_yields_defaults() {
        let path = std::env::temp_dir().join(format!(
            "diskintel-settings-broken-{}.json",
            std::process::id()
        ));
        for garbage in [
            "",
            "{",
            "not json at all",
            "{\"close_to_tray\":",
            "{\"close_to_tray\":\"yes\"}",
            "{\"snapshot_dir\":123}",
            "\u{0000}\u{0001}\u{0002}",
        ] {
            save_to(&path, &Settings::default());
            std::fs::write(&path, garbage).unwrap();
            assert_eq!(
                load_from(&path),
                Settings::default(),
                "损坏内容应退回默认值：{garbage:?}"
            );
        }
    }

    /// 旧版本（0.3.1/0.3.2）只有三个字段的 JSON：旧值保留、新字段取默认。
    #[test]
    fn old_json_migrates_with_new_defaults() {
        let old = "{\"auto_snapshot_on_start\":true,\"auto_snapshot_min_interval_minutes\":180,\"check_update_on_start\":true}";
        let parsed = parse(old);
        assert!(parsed.auto_snapshot_on_start);
        assert_eq!(parsed.auto_snapshot_min_interval_minutes, 180);
        assert!(parsed.check_update_on_start);
        // 新字段全部是默认值。
        assert!(parsed.close_to_tray);
        assert!(!parsed.start_minimized);
        assert_eq!(parsed.auto_snapshot_every_days, DEFAULT_EVERY_DAYS);
        assert_eq!(parsed.snapshot_dir, "");
        assert_eq!(parsed.keep_snapshot_count, DEFAULT_KEEP_COUNT);
        // v4 读层目前只有 GUI 支持（--json / MCP 仍走 v3 SQL），默认不自动转换。
        assert!(!parsed.convert_snapshot_v4);
        assert!(parsed.fold_enabled);
        assert!(parsed.fold_rare);
    }

    /// 渲染 → 解析往返：所有字段原样回来（含 Windows 路径与中文）。
    #[test]
    fn roundtrip_keeps_all_fields() {
        let settings = Settings {
            auto_snapshot_on_start: true,
            auto_snapshot_min_interval_minutes: 180,
            check_update_on_start: true,
            close_to_tray: false,
            start_minimized: true,
            auto_snapshot_every_days: 3,
            snapshot_dir: r"D:\快照 备份\disk\\snap".to_owned(),
            keep_snapshot_count: 12,
            convert_snapshot_v4: false,
            fold_enabled: false,
            fold_rare: false,
            fold_keep_exts: "psd,blend,7z".to_owned(),
            fold_keep_prefixes: r"D:\工程,D:\素材".to_owned(),
        };
        let roundtrip = parse(&render(&settings));
        assert_eq!(roundtrip, settings);
    }

    /// 未知字段忽略；类型不对（数字写进 bool、字符串写进数字）逐字段回退默认。
    #[test]
    fn unknown_fields_and_wrong_types_are_ignored() {
        let json = concat!(
            "{\"close_to_tray\":\"yes\",\"start_minimized\":1,",
            "\"auto_snapshot_every_days\":\"soon\",\"snapshot_dir\":42,",
            "\"keep_snapshot_count\":-3,\"future_field\":[1,2,3]}"
        );
        let parsed = parse(json);
        assert_eq!(parsed, Settings::default(), "错误类型与未知字段都应回退默认值");
    }

    /// 数值字段越界按上下限夹住（周期天数 ≤365、保留份数 ≤500、旧间隔 1..10080）。
    #[test]
    fn numeric_fields_are_clamped() {
        let huge = parse(
            "{\"auto_snapshot_every_days\":9999,\"keep_snapshot_count\":100000,\"auto_snapshot_min_interval_minutes\":0}",
        );
        assert_eq!(huge.auto_snapshot_every_days, 365);
        assert_eq!(huge.keep_snapshot_count, 500);
        assert_eq!(huge.auto_snapshot_min_interval_minutes, 1);
    }

    /// 自定义快照目录：空走默认 <数据根>\snapshots；非空按填写值。
    #[test]
    fn effective_snapshots_dir_follows_setting() {
        let root = Path::new(r"D:\disk-intel-data");
        let mut settings = Settings::default();
        assert_eq!(
            settings.effective_snapshots_dir(root),
            root.join("snapshots")
        );
        assert!(!settings.has_custom_snapshot_dir());
        settings.snapshot_dir = r"E:\快照".to_owned();
        assert_eq!(
            settings.effective_snapshots_dir(root),
            Path::new(r"E:\快照")
        );
        assert!(settings.has_custom_snapshot_dir());
    }

    /// 列表字段归一化：去空白 / 去重 / 保留顺序。
    #[test]
    fn comma_lists_are_normalized() {
        let parsed = parse("{\"fold_keep_exts\":\" PSD, blend ,psd,, 7z \"}");
        assert_eq!(parsed.fold_keep_exts, "PSD,blend,7z");
    }

    /// 真实落盘一次：父目录自动创建、特殊字符原样读回。
    #[test]
    fn file_roundtrip_through_disk() {
        let path = std::env::temp_dir().join(format!(
            "diskintel-settings-test-{}-nested/settings.json",
            std::process::id()
        ));
        let settings = Settings {
            auto_snapshot_on_start: true,
            snapshot_dir: "E:\\带\"引号\"的目录".to_owned(),
            ..Settings::default()
        };
        save_to(&path, &settings);
        assert_eq!(load_from(&path), settings);
    }
}

//! 本机设置：%APPDATA%\DiskIntel\settings.json。
//!
//! 目前只有三项（快照策略 + 启动检查更新），刻意不引入 serde：
//! 扁平 JSON 手写解析不到三十行，比多一个依赖划算；任何文件缺失 / 截断 /
//! 乱码都退回默认值并在 stderr 留一行说明，绝不 panic。
//!
//! 口径（对应机主疑问「快照是自动还是手动」）：
//! - 默认手动：工具栏「扫描」/ 文件菜单「重建索引」随时可建；
//! - 打开 auto_snapshot_on_start 后，启动 3 秒由外壳在后台跑一次，
//!   但最新快照距今不足 auto_snapshot_min_interval_minutes 就跳过，避免频繁建库。

use std::path::{Path, PathBuf};

/// 自动建快照的默认最小间隔（分钟）：一小时一次足够，手工扫描不受限制。
pub const DEFAULT_MIN_INTERVAL_MINUTES: u64 = 60;
/// 间隔下限：0 会让每次启动都全盘扫描，按 1 分钟兜底。
const MIN_INTERVAL_MINUTES: u64 = 1;
/// 间隔上限：一周。再大就没有「自动」的意义了。
const MAX_INTERVAL_MINUTES: u64 = 10_080;

/// 本机设置。字段全部可选：JSON 里缺的项取默认值。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Settings {
    /// 启动时自动建快照（默认关闭，避免开机就吃满磁盘 IO）。
    pub auto_snapshot_on_start: bool,
    /// 自动建快照的最小间隔（分钟）：最新快照比它新就跳过。
    pub auto_snapshot_min_interval_minutes: u64,
    /// 启动时静默检查一次更新（默认关闭；结果只在发现新版本时提示）。
    pub check_update_on_start: bool,
}

impl Default for Settings {
    fn default() -> Self {
        Self {
            auto_snapshot_on_start: false,
            auto_snapshot_min_interval_minutes: DEFAULT_MIN_INTERVAL_MINUTES,
            check_update_on_start: false,
        }
    }
}

impl Settings {
    /// 把间隔夹到合理区间，防止手改文件写出 0 或天文数字。
    fn normalized(mut self) -> Self {
        self.auto_snapshot_min_interval_minutes = self
            .auto_snapshot_min_interval_minutes
            .clamp(MIN_INTERVAL_MINUTES, MAX_INTERVAL_MINUTES);
        self
    }
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
        "{{\"auto_snapshot_on_start\":{},\"auto_snapshot_min_interval_minutes\":{},\"check_update_on_start\":{}}}\n",
        settings.auto_snapshot_on_start,
        settings.auto_snapshot_min_interval_minutes,
        settings.check_update_on_start,
    )
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

#[cfg(test)]
mod tests {
    use super::{
        DEFAULT_MIN_INTERVAL_MINUTES, Settings, load_from, parse, render, save_to,
    };

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
            "{\"auto_snapshot_on_start\":",
            "{\"auto_snapshot_on_start\":\"yes\"}",
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

    /// 渲染 → 解析往返，三个字段都能原样回来（包括 true 与非默认间隔）。
    #[test]
    fn roundtrip_keeps_all_fields() {
        let settings = Settings {
            auto_snapshot_on_start: true,
            auto_snapshot_min_interval_minutes: 180,
            check_update_on_start: true,
        };
        assert_eq!(parse(&render(&settings)), settings);
    }

    /// 缺字段 / 多字段：缺的取默认，多的忽略。
    #[test]
    fn partial_and_unknown_fields() {
        let parsed = parse("{\"check_update_on_start\":true,\"future_field\":123}");
        assert_eq!(
            parsed,
            Settings {
                auto_snapshot_on_start: false,
                auto_snapshot_min_interval_minutes: DEFAULT_MIN_INTERVAL_MINUTES,
                check_update_on_start: true,
            }
        );
    }

    /// 间隔字段越界（0 / 过亿）按上下限夹住，避免每次启动全盘扫描。
    #[test]
    fn interval_is_clamped() {
        let zero = parse(
            "{\"auto_snapshot_on_start\":false,\"auto_snapshot_min_interval_minutes\":0}",
        );
        assert_eq!(zero.auto_snapshot_min_interval_minutes, 1);
        let huge = parse(
            "{\"auto_snapshot_on_start\":false,\"auto_snapshot_min_interval_minutes\":999999999}",
        );
        assert_eq!(huge.auto_snapshot_min_interval_minutes, 10_080);
    }

    /// 真实落盘一次：父目录自动创建、UTF-8 内容原样读回。
    #[test]
    fn file_roundtrip_through_disk() {
        let path = std::env::temp_dir().join(format!(
            "diskintel-settings-test-{}-nested/settings.json",
            std::process::id()
        ));
        let settings = Settings {
            auto_snapshot_on_start: true,
            auto_snapshot_min_interval_minutes: 30,
            check_update_on_start: false,
        };
        save_to(&path, &settings);
        assert_eq!(load_from(&path), settings);
    }
}

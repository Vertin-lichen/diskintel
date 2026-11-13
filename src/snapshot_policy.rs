//! 快照策略（纯逻辑）：该不该建、留几份、待删除放哪。
//!
//! 产品口径的唯一出处，本模块不碰文件系统，全部可以单测：
//! - due：距上一份快照是否已到期限（没有快照 = 首次安装，恒为到期）；
//! - prune_plan：按保留份数算出该移入待删除的旧快照（保留最新 N 份）；
//! - trash_dir：待删除目录落点（默认布局沿用 <data_root>\待删除，自定义目录就近放）；
//! - should_build_on_start：启动时的总判断（首次必建 + 自动开关 + 天数期限）。
//!
//! 机主要求：每 7 天至少一份、首次安装必须有一份、保留最近 N 份、快照目录可自定义。
//! 落盘执行（真正移动文件 / 调 v4 转换）在 store::prune，本模块只回答「该做什么」。
//!
//! 说明：这里的 API 由外壳（app.rs，w6/lead 接线）调用；接线落地前除了单测还没有生产调用者，
//! 先关掉 dead_code 噪声，接线后可以删掉下面这行。
#![allow(dead_code)]

use std::path::{Path, PathBuf};

use crate::store::trash::TRASH_DIR;

/// 默认自动间隔（天）。
pub const DEFAULT_AUTO_EVERY_DAYS: u64 = 7;
/// 默认保留份数（含最新一份）。
pub const DEFAULT_KEEP_COUNT: usize = 8;
/// 默认快照目录名（相对数据根）。
pub const SNAPSHOT_DIR_NAME: &str = "snapshots";
/// 一天的秒数（策略换算用）。
const SECONDS_PER_DAY: i64 = 24 * 60 * 60;

/// 快照策略。字段全部公开：设置层（settings.json）解析后直接填，本模块不依赖设置模块。
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Policy {
    /// 自定义快照目录；None 表示默认的 <data_root>\snapshots。
    pub snapshot_dir: Option<PathBuf>,
    /// 自动快照的最小间隔（天）。0 = 不设间隔（每次启动都建），设置层应夹到 1..=365。
    pub auto_every_days: u64,
    /// 是否允许「启动时自动建快照」。首次安装（一份快照都没有）不受此开关限制。
    pub auto_on_start: bool,
    /// 保留份数（含最新一份）：多于这个数的旧快照会被移入待删除；0 视为 1，绝不删光。
    pub keep_count: usize,
    /// 建库成功后自动把新快照转 v4，并把 v3 原文件移入待删除。
    ///
    /// 默认 **false**：v4 目前只有 GUI 读层支持，--json / MCP 仍按 v3 SQL 查询，
    /// 自动转换会让命令行/AI 通道看不见新快照，所以本期做成手动勾选项
    /// （设置项建议命名 convert_snapshot_v4，默认 false）。
    pub v4_after_create: bool,
}

impl Default for Policy {
    fn default() -> Self {
        Self {
            snapshot_dir: None,
            auto_every_days: DEFAULT_AUTO_EVERY_DAYS,
            auto_on_start: false,
            keep_count: DEFAULT_KEEP_COUNT,
            v4_after_create: false,
        }
    }
}

impl Policy {
    /// 显式构造（设置层映射用）：构造时就把极端值夹到安全区间。
    pub fn new(
        snapshot_dir: Option<PathBuf>,
        auto_every_days: u64,
        auto_on_start: bool,
        keep_count: usize,
        v4_after_create: bool,
    ) -> Self {
        Self {
            snapshot_dir,
            auto_every_days,
            auto_on_start,
            keep_count,
            v4_after_create,
        }
        .normalized()
    }

    /// 夹取极端值：天数 1..=365，保留份数至少 1（保留策略永远不把历史删光）。
    pub fn normalized(mut self) -> Self {
        self.auto_every_days = self.auto_every_days.clamp(1, 365);
        self.keep_count = self.keep_count.max(1);
        self
    }

    /// 生效的自动间隔天数（未归一化的结构体也不会退化成 0）。
    pub fn every_days(&self) -> u64 {
        self.auto_every_days.clamp(1, 365)
    }

    /// 生效的保留份数（至少 1）。
    pub fn retained_count(&self) -> usize {
        self.keep_count.max(1)
    }

    /// 实际快照目录：自定义优先，否则 <data_root>\snapshots。
    pub fn resolve_snapshot_dir(&self, data_root: &Path) -> PathBuf {
        match &self.snapshot_dir {
            Some(dir) => dir.clone(),
            None => data_root.join(SNAPSHOT_DIR_NAME),
        }
    }

    /// 是否使用默认布局（<data_root>\snapshots）。
    pub fn uses_default_dir(&self, data_root: &Path) -> bool {
        self.snapshot_dir.is_none()
            || self
                .snapshot_dir
                .as_deref()
                .is_some_and(|dir| same_path(dir, &data_root.join(SNAPSHOT_DIR_NAME)))
    }
}

/// 距离上一份快照是否已到期限。
///
/// - 没有快照（首次安装 / 空目录）→ 恒 true；
/// - every_days = 0 → true（不设间隔；设置层会夹到 1 天，这里保持纯函数语义）；
/// - 恰好到达间隔 → true（到点就建，不等到超时）；
/// - 时钟回拨（上一份比现在还新）→ false：系统时间异常时不反复全盘建库。
pub fn due(last_created_unix: Option<i64>, now_unix: i64, every_days: u64) -> bool {
    let Some(last) = last_created_unix else {
        return true;
    };
    if every_days == 0 {
        return true;
    }
    let interval = (every_days.min(i64::MAX as u64) as i64).saturating_mul(SECONDS_PER_DAY);
    let elapsed = now_unix.saturating_sub(last);
    if elapsed < 0 {
        return false;
    }
    elapsed >= interval
}

/// 启动时是否应该建一份快照。
///
/// 首次（一份快照都没有）恒 true —— 机主要求「首次安装必须建一份」，不受自动开关限制；
/// 其余情况：自动开关打开，且距上一份已满 auto_every_days 天。
pub fn should_build_on_start(
    policy: &Policy,
    last_created_unix: Option<i64>,
    now_unix: i64,
) -> bool {
    match last_created_unix {
        None => true,
        Some(last) => policy.auto_on_start && due(Some(last), now_unix, policy.every_days()),
    }
}

/// 保留策略：返回应移入待删除的快照名（保留最新 keep_count 份，不足则空）。
///
/// 排序口径：创建时间降序，同一时刻用名字降序兜底（快照名是时间戳，可重复时仍稳定）；
/// 返回顺序从最旧开始 —— 先入待删除的该是更早的观测。
/// keep_count = 0 按 1 处理（永远保留最新一份，绝不把历史删光）。
pub fn prune_plan(existing: &[(String, i64, u64)], keep_count: usize) -> Vec<String> {
    let keep = keep_count.max(1);
    if existing.len() <= keep {
        return Vec::new();
    }
    let mut order: Vec<usize> = (0..existing.len()).collect();
    order.sort_by(|&a, &b| {
        existing[b]
            .1
            .cmp(&existing[a].1)
            .then_with(|| existing[b].0.cmp(&existing[a].0))
    });
    order[keep..]
        .iter()
        .rev()
        .map(|&index| existing[index].0.clone())
        .collect()
}

/// 待删除目录落点。
///
/// - 默认布局（<data_root>\snapshots）→ <data_root>\待删除，沿用既有 MANIFEST 口径；
/// - 自定义快照目录 → <snapshot_dir>\待删除，与快照同卷，移动是 rename，不用跨盘复制 GB 级文件；
/// - 兜底：拿不准时也放在快照目录旁边，宁可目录多一个，也不做跨盘大复制。
pub fn trash_dir(snapshot_dir: &Path, data_root: &Path) -> PathBuf {
    if same_path(snapshot_dir, &data_root.join(SNAPSHOT_DIR_NAME)) {
        data_root.join(TRASH_DIR)
    } else {
        snapshot_dir.join(TRASH_DIR)
    }
}

/// 两个路径是否指向同一位置（Windows 路径大小写不敏感，末尾分隔符不算差异）。
pub fn same_path(a: &Path, b: &Path) -> bool {
    normalize_path(a) == normalize_path(b)
}

fn normalize_path(path: &Path) -> String {
    let text = path.as_os_str().to_string_lossy().replace('/', "\\");
    text.trim_end_matches('\\').to_lowercase()
}

/// 两个路径是否在同一个卷 / 盘符上（决定移动是 rename 还是要复制）。
pub fn same_volume(a: &Path, b: &Path) -> bool {
    volume_of(a) == volume_of(b)
}

fn volume_of(path: &Path) -> String {
    match path.components().next() {
        Some(std::path::Component::Prefix(prefix)) => {
            prefix.as_os_str().to_string_lossy().replace('/', "\\").to_lowercase()
        }
        Some(std::path::Component::RootDir) => "\\".to_owned(),
        // 相对路径没有卷信息：视为同一卷，避免误判成跨盘。
        _ => String::new(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn entries(items: &[(&str, i64, u64)]) -> Vec<(String, i64, u64)> {
        items
            .iter()
            .map(|(name, created, bytes)| ((*name).to_owned(), *created, *bytes))
            .collect()
    }

    fn names(items: &[&str]) -> Vec<String> {
        items.iter().map(|name| (*name).to_owned()).collect()
    }

    /// 默认值就是产品口径：默认目录 / 7 天 / 自动关闭 / 保留 8 份 / 默认不转 v4。
    #[test]
    fn defaults_match_product_policy() {
        let policy = Policy::default();
        assert_eq!(policy.snapshot_dir, None);
        assert_eq!(policy.auto_every_days, 7);
        assert!(!policy.auto_on_start);
        assert_eq!(policy.keep_count, 8);
        assert!(
            !policy.v4_after_create,
            "v4 只有 GUI 读层支持，默认手动（convert_snapshot_v4=false）"
        );
    }

    /// 首次安装（一份快照都没有）：恒到期，不管间隔设多大。
    #[test]
    fn first_install_is_always_due() {
        assert!(due(None, 1_800_000_000, 7));
        assert!(due(None, 1_800_000_000, 0));
        assert!(due(None, 0, 365));
    }

    /// 未到间隔不建；恰好到点就建。
    #[test]
    fn interval_boundary() {
        let now = 1_800_000_000;
        let week = 7 * SECONDS_PER_DAY;
        assert!(!due(Some(now - week + 1), now, 7), "差 1 秒不该建");
        assert!(due(Some(now - week), now, 7), "恰好 7 天该建");
        assert!(due(Some(now - week * 2), now, 7));
        assert!(!due(Some(now), now, 7), "刚建完不该再建");
    }

    /// 0 天 = 不设间隔（纯函数语义；设置层会把 0 夹到 1 天）。
    #[test]
    fn zero_days_means_no_interval() {
        let now = 1_800_000_000;
        assert!(due(Some(now), now, 0));
        assert!(due(Some(now + 100), now, 0));
    }

    /// 时钟回拨（上一份比现在还新）不触发建库，避免系统时间异常时反复全盘扫描。
    #[test]
    fn clock_rollback_is_not_due() {
        let now = 1_800_000_000;
        assert!(!due(Some(now + SECONDS_PER_DAY), now, 7));
        assert!(!due(Some(now + SECONDS_PER_DAY * 400), now, 7));
    }

    /// 上一份是 1970 年（last=0）也算早已到期。
    #[test]
    fn epoch_timestamp_is_due() {
        assert!(due(Some(0), 1_800_000_000, 7));
    }

    /// 启动判断：首次必建（即使自动开关关着），其余要求开关打开且已到期。
    #[test]
    fn should_build_on_start_rules() {
        let now = 1_800_000_000;
        let mut policy = Policy::default();
        assert!(should_build_on_start(&policy, None, now), "首次安装必须建一份");
        assert!(
            !should_build_on_start(&policy, Some(now - 1), now),
            "开关默认关闭时不自动建"
        );
        policy.auto_on_start = true;
        assert!(!should_build_on_start(&policy, Some(now - 1), now));
        assert!(should_build_on_start(&policy, Some(now - 7 * SECONDS_PER_DAY), now));
    }

    /// 保留 8 份：10 份里把最旧的 2 份挑出来，且从最旧开始。
    #[test]
    fn prune_keeps_newest_eight() {
        let items: Vec<(String, i64, u64)> = (1..=10)
            .map(|day| (format!("202601{day:02}-000000"), day as i64, 100))
            .collect();
        let plan = prune_plan(&items, 8);
        // 返回顺序：从最旧开始（先入待删除的该是更早的观测）
        assert_eq!(plan, names(&["20260101-000000", "20260102-000000"]));
    }

    /// 份数不足（或刚好等于）保留数：不需要淘汰。
    #[test]
    fn prune_insufficient_returns_empty() {
        let three = entries(&[("a", 1, 10), ("b", 2, 10), ("c", 3, 10)]);
        assert!(prune_plan(&three, 8).is_empty());
        assert!(prune_plan(&three, 3).is_empty());
        assert!(prune_plan(&[], 8).is_empty());
    }

    /// keep_count = 1：只留最新一份，其余全部进待删除（从旧到新）。
    #[test]
    fn prune_keep_one_only_newest() {
        let items = entries(&[("a", 1, 10), ("c", 3, 10), ("b", 2, 10)]);
        assert_eq!(prune_plan(&items, 1), names(&["a", "b"]));
    }

    /// keep_count = 0 按 1 处理：任何情况下都不把历史删光。
    #[test]
    fn prune_keep_zero_never_deletes_everything() {
        let items = entries(&[("a", 1, 10), ("b", 2, 10), ("c", 3, 10)]);
        assert_eq!(prune_plan(&items, 0), names(&["a", "b"]));
    }

    /// 创建时间全部相等：用名字（时间戳）降序稳定排序，结果确定。
    #[test]
    fn prune_equal_timestamps_are_stable() {
        let items = entries(&[
            ("20260101-000000", 5, 1),
            ("20260102-000000", 5, 1),
            ("20260103-000000", 5, 1),
            ("20260104-000000", 5, 1),
        ]);
        assert_eq!(
            prune_plan(&items, 2),
            names(&["20260101-000000", "20260102-000000"])
        );
        // 输入顺序不同，结果一致
        let mut reversed = items.clone();
        reversed.reverse();
        assert_eq!(prune_plan(&reversed, 2), prune_plan(&items, 2));
    }

    /// 默认布局：待删除放在数据根下（沿用既有 MANIFEST）；自定义目录就近放在快照目录里。
    #[test]
    fn trash_dir_follows_layout() {
        let data_root = Path::new(r"D:\disk-intel-data");
        let default_dir = data_root.join("snapshots");
        assert_eq!(
            trash_dir(&default_dir, data_root),
            PathBuf::from(r"D:\disk-intel-data\待删除")
        );
        let custom = Path::new(r"E:\我的快照");
        assert_eq!(trash_dir(custom, data_root), PathBuf::from(r"E:\我的快照\待删除"));
        // 自定义目录名叫 snapshots 但在别的盘，也不会被误判成默认布局
        let other = Path::new(r"E:\snapshots");
        assert_eq!(trash_dir(other, data_root), PathBuf::from(r"E:\snapshots\待删除"));
    }

    /// 自定义目录解析与默认目录识别。
    #[test]
    fn resolve_snapshot_dir_honours_override() {
        let data_root = Path::new(r"D:\disk-intel-data");
        assert_eq!(
            Policy::default().resolve_snapshot_dir(data_root),
            PathBuf::from(r"D:\disk-intel-data\snapshots")
        );
        assert!(Policy::default().uses_default_dir(data_root));
        let custom = Policy::new(Some(PathBuf::from(r"E:\我的快照")), 7, true, 8, false);
        assert_eq!(custom.resolve_snapshot_dir(data_root), PathBuf::from(r"E:\我的快照"));
        assert!(!custom.uses_default_dir(data_root));
    }

    /// 卷判断：同盘 true、跨盘 false（用于解释待删除落点）。
    #[test]
    fn volumes_are_compared_by_prefix() {
        assert!(same_volume(Path::new(r"D:\a\b"), Path::new(r"D:\c")));
        assert!(!same_volume(Path::new(r"D:\a"), Path::new(r"E:\a")));
        assert!(same_volume(Path::new(r"d:\a"), Path::new(r"D:\b")), "盘符大小写不敏感");
    }

    /// 归一化：天数夹到 1..=365，保留份数至少 1。
    #[test]
    fn normalized_clamps_extremes() {
        let zero = Policy::new(None, 0, false, 0, true);
        assert_eq!(zero.auto_every_days, 1);
        assert_eq!(zero.keep_count, 1);
        assert_eq!(zero.every_days(), 1);
        assert_eq!(zero.retained_count(), 1);
        let huge = Policy::new(None, 9999, false, 999, true);
        assert_eq!(huge.auto_every_days, 365);
        assert_eq!(huge.keep_count, 999);
        // 手改结构体（绕过 new）也不会退化成 0 天 / 0 份
        let raw = Policy {
            auto_every_days: 0,
            keep_count: 0,
            ..Policy::default()
        };
        assert_eq!(raw.every_days(), 1);
        assert_eq!(raw.retained_count(), 1);
    }

    /// 路径比较：末尾分隔符与大小写不影响判定。
    #[test]
    fn same_path_ignores_case_and_trailing_separator() {
        assert!(same_path(Path::new(r"D:\DATA\snapshots\"), Path::new(r"d:\data\snapshots")));
        assert!(!same_path(Path::new(r"D:\data\snapshots"), Path::new(r"D:\data\snapshots2")));
    }
}

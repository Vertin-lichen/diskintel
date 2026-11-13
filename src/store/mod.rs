//! v4 字典化快照：共享工具与类型。
//!
//! 规范见 docs/研究-快照存储优化分析-20261113.md 第 11 节：
//! 一份快照里不再写完整路径，只写「父目录 id + 名字 id」，路径在查询时现拼。
//! 本模块只放与具体表无关的路径 / 内存工具；表结构与读写见 v4 子模块。
//!
//! 与 v3（crate::db）的关系：方法名与返回形状保持一致，
//! 但 v4 没有 entries.path / ext 列，所有路径类条件都要先落到
//! names.name / dir_paths.path 上，再按 id 取行。

pub mod compact;
pub mod notes;
pub mod trash;
pub mod v4;

/// delta 记录的操作码：目标快照里新出现的路径。
pub const OP_ADDED: i64 = 1;
/// 目标快照里消失的路径（内容取旧快照，即消失前的值）。
pub const OP_REMOVED: i64 = 2;
/// 仍然存在，但 size / mtime / is_dir 至少一项变了（内容取新快照的值）。
pub const OP_CHANGED: i64 = 3;

/// delta 文件在 meta.format_version 里声明的格式版本。
pub const DELTA_FORMAT_VERSION: &str = "4-delta";

#[allow(unused_imports)]
pub use v4::{Query, V4Snapshot, V4Stats, convert_v3_to_v4};

/// 父路径 + 名字 → 完整路径。
///
/// 父路径以分隔符结尾就直接接，否则补一个反斜杠（等价 Python 侧 paths.join_path）。
/// 卷根（C:\、\\?\Volume{GUID}\）靠这条规则才不会丢分隔符。
pub fn join_path(parent: &str, name: &str) -> String {
    let mut out = String::with_capacity(parent.len() + name.len() + 1);
    out.push_str(parent);
    if !parent.is_empty() && !parent.ends_with('\\') && !parent.ends_with('/') {
        out.push('\\');
    }
    out.push_str(name);
    out
}

/// 从完整路径里剥出名字（parent 必须是 path 的父目录）。
///
/// v3 的 4,461,929 行已逐行校验满足 path == join_path(parent_path, name)，
/// 所以这里只做一次切片，不做字符串查找；数据异常时退回按分隔符切，绝不 panic。
pub fn name_of<'a>(path: &'a str, parent: &str) -> &'a str {
    if parent.is_empty() {
        return path;
    }
    if path.starts_with(parent) {
        let extra = if parent.ends_with('\\') || parent.ends_with('/') {
            0
        } else {
            1
        };
        let index = parent.len() + extra;
        if index <= path.len() && !path[index..].is_empty() {
            return &path[index..];
        }
    }
    let trimmed = path.trim_end_matches(['\\', '/']);
    trimmed.rsplit(['\\', '/']).next().unwrap_or(trimmed)
}

/// 扩展名：最后一个点之后、小写、不带点；没有点则为空串。
///
/// 与 v3 建库器口径一致：.gitignore → gitignore，a.tar.gz → gz，
/// LICENSE → 空串。v4 不落盘这一列，过滤时在 names 上现算。
pub fn extension_of(name: &str) -> String {
    let base = name.rsplit(['\\', '/']).next().unwrap_or(name);
    match base.rsplit_once('.') {
        Some((_, ext)) if !ext.is_empty() => ext.to_lowercase(),
        _ => String::new(),
    }
}

/// LIKE 转义：与 src/filter.rs 同一口径（转义符是反斜杠）。
pub fn escape_like(value: &str) -> String {
    value
        .replace('\\', "\\\\")
        .replace('%', "\\%")
        .replace('_', "\\_")
}

/// escape_like(scope) 之后要补的后缀：字面反斜杠 + 任意后缀。
pub const LIKE_SEP_SUFFIX: &str = "\\\\%";

/// 当前进程工作集字节数（Windows；其它平台返回 0）。
pub fn rss_bytes() -> u64 {
    memory().0
}

/// 当前进程峰值工作集字节数（Windows；其它平台返回 0）。
pub fn peak_rss_bytes() -> u64 {
    memory().1
}

#[cfg(windows)]
fn memory() -> (u64, u64) {
    #[repr(C)]
    struct ProcessMemoryCounters {
        cb: u32,
        page_fault_count: u32,
        peak_working_set_size: usize,
        working_set_size: usize,
        quota_peak_paged_pool_usage: usize,
        quota_paged_pool_usage: usize,
        quota_peak_non_paged_pool_usage: usize,
        quota_non_paged_pool_usage: usize,
        pagefile_usage: usize,
        peak_pagefile_usage: usize,
    }

    unsafe extern "system" {
        fn GetCurrentProcess() -> isize;
        fn K32GetProcessMemoryInfo(
            process: isize,
            counters: *mut ProcessMemoryCounters,
            cb: u32,
        ) -> i32;
    }

    let mut counters = ProcessMemoryCounters {
        cb: std::mem::size_of::<ProcessMemoryCounters>() as u32,
        page_fault_count: 0,
        peak_working_set_size: 0,
        working_set_size: 0,
        quota_peak_paged_pool_usage: 0,
        quota_paged_pool_usage: 0,
        quota_peak_non_paged_pool_usage: 0,
        quota_non_paged_pool_usage: 0,
        pagefile_usage: 0,
        peak_pagefile_usage: 0,
    };
    let ok = unsafe { K32GetProcessMemoryInfo(GetCurrentProcess(), &mut counters, counters.cb) };
    if ok == 0 {
        (0, 0)
    } else {
        (
            counters.working_set_size as u64,
            counters.peak_working_set_size as u64,
        )
    }
}

#[cfg(not(windows))]
fn memory() -> (u64, u64) {
    (0, 0)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn join_path_keeps_root_separator() {
        assert_eq!(join_path("C:\\", "Windows"), "C:\\Windows");
        assert_eq!(
            join_path("C:\\Windows", "notepad.exe"),
            "C:\\Windows\\notepad.exe"
        );
        assert_eq!(join_path("D:\\资料", "a.txt"), "D:\\资料\\a.txt");
    }

    #[test]
    fn name_of_matches_join_path() {
        for (parent, name) in [
            ("C:\\", "Windows"),
            ("C:\\Windows", "notepad.exe"),
            ("D:\\资料", "报告.docx"),
            ("", "C:\\"),
        ] {
            let path = join_path(parent, name);
            assert_eq!(name_of(&path, parent), name, "parent={parent} path={path}");
        }
    }

    #[test]
    fn extension_follows_v3_builder() {
        assert_eq!(extension_of(".gitignore"), "gitignore");
        assert_eq!(extension_of("a.tar.gz"), "gz");
        assert_eq!(extension_of("LICENSE"), "");
        assert_eq!(extension_of("A.DLL"), "dll");
        assert_eq!(extension_of("trailing."), "");
    }
}
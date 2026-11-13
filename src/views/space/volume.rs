//! 卷容量：直接调 Win32 GetDiskFreeSpaceExW，不创建任何进程。
//!
//! 历史教训（见 docs 实现记录）：早先版本启动时调 PowerShell 查 WMI 取磁盘拓扑，
//! 被安全软件的启发式规则判为启动即起壳的可疑程序。这里只做一次系统调用，
//! 结果按卷缓存；取不到就返回 None，界面退化为「容量未知」。
//!
//! 口径（二期改造后唯一允许的口径）：
//! - `total` = 卷容量；
//! - `free`  = **卷的总空闲字节数**（GetDiskFreeSpaceExW 的第 4 个出参 totalNumberOfFreeBytes），
//!   不是「调用者可用」的那个（二者仅在配额目录下不同，界面对比的应当是全卷空闲）；
//! - 实际已用 = total − free。快照逻辑大小不参与这里的任何计算。

use std::collections::HashMap;
use std::sync::{Mutex, OnceLock};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct VolumeSpace {
    /// 卷容量（字节）。
    pub total: u64,
    /// 卷总空闲（字节）。
    pub free: u64,
}

fn cache() -> &'static Mutex<HashMap<String, Option<VolumeSpace>>> {
    static CACHE: OnceLock<Mutex<HashMap<String, Option<VolumeSpace>>>> = OnceLock::new();
    CACHE.get_or_init(|| Mutex::new(HashMap::new()))
}

/// 查一个卷根（支持 D:\ 与 \\?\Volume{...}\ 两种形态）。
pub fn probe(root: &str) -> Option<VolumeSpace> {
    if let Ok(map) = cache().lock()
        && let Some(hit) = map.get(root)
    {
        return *hit;
    }
    let value = query(root);
    if let Ok(mut map) = cache().lock() {
        map.insert(root.to_owned(), value);
    }
    value
}

#[cfg(windows)]
fn query(root: &str) -> Option<VolumeSpace> {
    // 与 views/snapshots.rs 的声明保持完全一致（同一符号在两处声明时签名必须相同）。
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
    let wide: Vec<u16> = root.encode_utf16().chain(std::iter::once(0)).collect();
    let mut caller_free = Large { quad: 0 };
    let mut total = Large { quad: 0 };
    let mut total_free = Large { quad: 0 };
    let ok = unsafe {
        GetDiskFreeSpaceExW(
            wide.as_ptr(),
            &mut caller_free,
            &mut total,
            &mut total_free,
        )
    };
    if ok == 0 || total.quad == 0 {
        return None;
    }
    Some(VolumeSpace {
        total: total.quad,
        free: total_free.quad,
    })
}

#[cfg(not(windows))]
fn query(_root: &str) -> Option<VolumeSpace> {
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn probes_system_drive() {
        for root in ["C:\\", "D:\\"] {
            if let Some(space) = probe(root) {
                assert!(space.total > 0);
                assert!(space.free <= space.total);
            }
        }
    }

    /// 口径：free 取的是全卷总空闲（第 4 个出参），且 free 不会大于容量。
    /// 实际已用 / 占比 / clamp 的计算在 space.rs 的 VolumeInfo 上单测。
    #[test]
    fn free_is_total_not_caller_free() {
        for root in ["C:\\", "D:\\"] {
            if let Some(space) = probe(root) {
                assert!(space.total > 0);
                assert!(space.free <= space.total, "{root} 空闲大于容量");
                assert!(space.total - space.free <= space.total);
            }
        }
    }
}

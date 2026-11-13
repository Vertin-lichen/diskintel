//! 物理磁盘拓扑：卷枚举 / 卷→磁盘映射 / 磁盘型号与总线。
//!
//! 只用 Win32 FFI，不新增依赖、不起任何子进程（历史教训：启动即起壳会被安全软件盯上）。
//! 每一步失败都只降级那一条信息，绝不 panic：
//! - 卷枚举失败   → 返回空拓扑，上层按快照根逐卷单列；
//! - 卷→磁盘失败  → 该卷 disk = None，上层归入「未识别物理盘」行；
//! - 型号/总线失败 → 该盘标题退化成「磁盘 N · 容量」。
//!
//! 不做进程级缓存：只由空间页在首次显示时于后台线程调用一次（F5 / 切快照
//! 由新的视图实例重新探测）。构建过程只用毫秒级的卷级查询——
//! 刻意不调 IOCTL_DISK_GET_DRIVE_GEOMETRY_EX：实测 USB 机械盘上该 IOCTL
//! 要 2.3 s（设备唤醒），磁盘容量改用各卷磁盘范围末端估算（差值为未分配尾部）。

// ---------- 公开数据结构 ----------

/// 一块物理磁盘的静态信息。
#[derive(Clone, Debug, Default)]
pub struct DiskInfo {
    pub number: u32,
    /// 厂商串（NVMe 板卡常为空）。
    pub vendor: String,
    /// 型号串。
    pub product: String,
    /// STORAGE_BUS_TYPE 原始值（17 = NVMe，7 = USB，11 = SATA…）。
    pub bus: u32,
    /// 磁盘容量（字节；0 表示未知）。
    pub size: u64,
}

/// 一个卷的拓扑信息（纯系统侧，与快照无关）。
#[derive(Clone, Debug, Default)]
pub struct VolumeTopo {
    /// `\\?\Volume{...}\`
    pub guid: String,
    /// 挂载点：盘符形如 `C:\`，也可能是目录挂载点。
    pub mount_points: Vec<String>,
    pub label: String,
    pub file_system: String,
    /// 所在物理磁盘号（拿不到映射时为 None）。
    pub disk: Option<u32>,
    /// 卷在物理盘上的起始偏移（字节）。
    pub offset: u64,
    /// 卷长度（字节）。
    pub length: u64,
}

/// 一次系统拓扑快照。
#[derive(Clone, Debug, Default)]
pub struct Topology {
    pub disks: Vec<DiskInfo>,
    pub volumes: Vec<VolumeTopo>,
    /// 降级说明（给自检 / 报告用，界面不直接展示）。
    pub note: Option<String>,
}

/// 探测一次物理磁盘拓扑（每次调用都重新枚举；调用方负责放到后台线程）。
pub fn probe() -> Topology {
    build()
}

/// 测试用：本进程已执行的探测次数（用于断言启动路径没有探测拓扑）。
#[cfg(test)]
pub fn probe_count() -> usize {
    PROBES.load(std::sync::atomic::Ordering::Relaxed)
}

#[cfg(test)]
static PROBES: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);

/// 总线类型 → 中文/通用短名；未知返回 None（标题里直接省略）。
pub fn bus_label(bus: u32) -> Option<&'static str> {
    Some(match bus {
        1 => "SCSI",
        2 => "ATAPI",
        3 => "ATA",
        4 => "1394",
        7 => "USB",
        8 => "RAID",
        9 => "iSCSI",
        10 => "SAS",
        11 => "SATA",
        12 => "SD",
        13 => "eMMC",
        14 | 15 => "虚拟",
        16 => "存储空间",
        17 => "NVMe",
        19 => "UFS",
        _ => return None,
    })
}

/// 磁盘型号：厂商串 + 型号串，二者都空时返回 None。
pub fn model_label(disk: &DiskInfo) -> Option<String> {
    let vendor = disk.vendor.trim();
    let product = disk.product.trim();
    match (vendor.is_empty(), product.is_empty()) {
        (true, true) => None,
        (true, false) => Some(product.to_owned()),
        (false, true) => Some(vendor.to_owned()),
        (false, false) => Some(format!("{vendor} {product}")),
    }
}

// ---------- 非 Windows：明确返回空拓扑 ----------

#[cfg(not(windows))]
fn build() -> Topology {
    Topology {
        note: Some("非 Windows 平台：无法查询物理磁盘拓扑".to_owned()),
        ..Topology::default()
    }
}

#[cfg(windows)]
fn build() -> Topology {
    #[cfg(test)]
    PROBES.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    sys::build()
}

#[cfg(windows)]
mod sys {
    use super::{DiskInfo, Topology, VolumeTopo};
    use std::ffi::c_void;

    // ---------- Win32 常量 ----------

    const FILE_SHARE_READ: u32 = 0x0000_0001;
    const FILE_SHARE_WRITE: u32 = 0x0000_0002;
    const OPEN_EXISTING: u32 = 3;

    /// _IOCTL_VOLUME_BASE(0x56) · 0 · METHOD_BUFFERED · FILE_ANY_ACCESS
    const IOCTL_VOLUME_GET_VOLUME_DISK_EXTENTS: u32 = 0x0056_0000;
    /// _IOCTL_STORAGE_BASE(0x2D) · 0x0500 · METHOD_BUFFERED · FILE_ANY_ACCESS
    const IOCTL_STORAGE_QUERY_PROPERTY: u32 = 0x002D_1400;

    const VOLUME_NAME_MAX: usize = 260;
    const STORAGE_DESCRIPTOR_BYTES: usize = 2048;

    type Handle = *mut c_void;

    #[link(name = "kernel32")]
    unsafe extern "system" {
        fn FindFirstVolumeW(name: *mut u16, len: u32) -> Handle;
        fn FindNextVolumeW(handle: Handle, name: *mut u16, len: u32) -> i32;
        fn FindVolumeClose(handle: Handle) -> i32;
        fn GetVolumePathNamesForVolumeNameW(
            volume: *const u16,
            paths: *mut u16,
            len: u32,
            needed: *mut u32,
        ) -> i32;
        fn GetVolumeInformationW(
            root: *const u16,
            label: *mut u16,
            label_len: u32,
            serial: *mut u32,
            max_component: *mut u32,
            flags: *mut u32,
            fs_name: *mut u16,
            fs_len: u32,
        ) -> i32;
        fn CreateFileW(
            name: *const u16,
            access: u32,
            share: u32,
            security: *mut c_void,
            disposition: u32,
            flags: u32,
            template: Handle,
        ) -> Handle;
        fn DeviceIoControl(
            handle: Handle,
            code: u32,
            in_buf: *mut c_void,
            in_len: u32,
            out_buf: *mut c_void,
            out_len: u32,
            returned: *mut u32,
            overlapped: *mut c_void,
        ) -> i32;
        fn CloseHandle(handle: Handle) -> i32;
    }

    // ---------- 小工具 ----------

    fn wide(text: &str) -> Vec<u16> {
        text.encode_utf16().chain(std::iter::once(0)).collect()
    }

    fn utf16_to_string(buf: &[u16]) -> String {
        let end = buf.iter().position(|&c| c == 0).unwrap_or(buf.len());
        String::from_utf16_lossy(&buf[..end])
    }

    fn parse_multi_sz(buf: &[u16]) -> Vec<String> {
        let mut out = Vec::new();
        let mut start = 0usize;
        for (index, &code) in buf.iter().enumerate() {
            if code != 0 {
                continue;
            }
            if index == start {
                break; // 双 NUL 结束
            }
            out.push(String::from_utf16_lossy(&buf[start..index]));
            start = index + 1;
        }
        out
    }

    fn read_u32(buf: &[u8], offset: usize) -> u32 {
        buf.get(offset..offset + 4)
            .and_then(|slice| <[u8; 4]>::try_from(slice).ok())
            .map(u32::from_le_bytes)
            .unwrap_or(0)
    }

    fn read_u64(buf: &[u8], offset: usize) -> u64 {
        buf.get(offset..offset + 8)
            .and_then(|slice| <[u8; 8]>::try_from(slice).ok())
            .map(u64::from_le_bytes)
            .unwrap_or(0)
    }

    /// 从 STORAGE_DEVICE_DESCRIPTOR 的字符串偏移处读 ANSI 串。
    fn read_descriptor_cstr(buf: &[u8], offset: usize) -> String {
        if offset == 0 || offset >= buf.len() {
            return String::new();
        }
        let end = buf[offset..]
            .iter()
            .position(|&byte| byte == 0)
            .map(|pos| offset + pos)
            .unwrap_or(buf.len());
        String::from_utf8_lossy(&buf[offset..end]).trim().to_owned()
    }

    // ---------- 设备句柄 ----------

    fn open_device(path: &str) -> Option<Handle> {
        let name = wide(path);
        let handle = unsafe {
            CreateFileW(
                name.as_ptr(),
                0,
                FILE_SHARE_READ | FILE_SHARE_WRITE,
                std::ptr::null_mut(),
                OPEN_EXISTING,
                0,
                std::ptr::null_mut(),
            )
        };
        let invalid = handle.is_null() || handle as isize == -1;
        if invalid { None } else { Some(handle) }
    }

    fn close_device(handle: Handle) {
        unsafe {
            CloseHandle(handle);
        }
    }

    /// 发出一个无输入缓冲、指定输出长度的 DeviceIoControl。
    fn ioctl(handle: Handle, code: u32, out: &mut [u8]) -> Option<u32> {
        let mut returned = 0u32;
        let ok = unsafe {
            DeviceIoControl(
                handle,
                code,
                std::ptr::null_mut(),
                0,
                out.as_mut_ptr().cast(),
                out.len() as u32,
                &mut returned,
                std::ptr::null_mut(),
            )
        };
        if ok == 0 { None } else { Some(returned) }
    }

    fn ioctl_with_input(handle: Handle, code: u32, input: &[u8], out: &mut [u8]) -> Option<u32> {
        let mut returned = 0u32;
        let ok = unsafe {
            DeviceIoControl(
                handle,
                code,
                input.as_ptr() as *mut c_void,
                input.len() as u32,
                out.as_mut_ptr().cast(),
                out.len() as u32,
                &mut returned,
                std::ptr::null_mut(),
            )
        };
        if ok == 0 { None } else { Some(returned) }
    }

    // ---------- 1. 枚举卷 ----------

    fn volume_names() -> Vec<String> {
        let mut names = Vec::new();
        let mut buf = vec![0u16; VOLUME_NAME_MAX];
        let handle = unsafe { FindFirstVolumeW(buf.as_mut_ptr(), buf.len() as u32) };
        if handle.is_null() || handle as isize == -1 {
            return names;
        }
        loop {
            let name = utf16_to_string(&buf);
            if name.is_empty() {
                break;
            }
            names.push(name);
            let next = unsafe { FindNextVolumeW(handle, buf.as_mut_ptr(), buf.len() as u32) };
            if next == 0 {
                break;
            }
        }
        unsafe {
            FindVolumeClose(handle);
        }
        names
    }

    fn mount_points(volume: &str) -> Vec<String> {
        let name = wide(volume);
        let mut needed = 0u32;
        let mut buf = vec![0u16; 512];
        let mut ok = unsafe {
            GetVolumePathNamesForVolumeNameW(
                name.as_ptr(),
                buf.as_mut_ptr(),
                buf.len() as u32,
                &mut needed,
            )
        };
        if ok == 0 {
            let required = needed as usize;
            if required <= buf.len() || required > 64 * 1024 {
                return Vec::new();
            }
            buf = vec![0u16; required];
            ok = unsafe {
                GetVolumePathNamesForVolumeNameW(
                    name.as_ptr(),
                    buf.as_mut_ptr(),
                    buf.len() as u32,
                    &mut needed,
                )
            };
            if ok == 0 {
                return Vec::new();
            }
        }
        parse_multi_sz(&buf)
    }

    fn volume_information(volume: &str) -> (String, String) {
        let root = wide(volume);
        let mut label = vec![0u16; 256];
        let mut fs = vec![0u16; 64];
        let ok = unsafe {
            GetVolumeInformationW(
                root.as_ptr(),
                label.as_mut_ptr(),
                label.len() as u32,
                std::ptr::null_mut(),
                std::ptr::null_mut(),
                std::ptr::null_mut(),
                fs.as_mut_ptr(),
                fs.len() as u32,
            )
        };
        if ok == 0 {
            return (String::new(), String::new());
        }
        (utf16_to_string(&label), utf16_to_string(&fs))
    }

    // ---------- 2. 卷 → 物理盘 ----------

    /// IOCTL_VOLUME_GET_VOLUME_DISK_EXTENTS：返回（盘号, 起始偏移, 卷长度）。
    fn disk_extents_path(path: &str) -> Option<(u32, u64, u64)> {
        let handle = open_device(path)?;
        let mut out = vec![0u8; 8 + 24 * 8];
        let result = ioctl(handle, IOCTL_VOLUME_GET_VOLUME_DISK_EXTENTS, &mut out);
        close_device(handle);
        result?;
        let count = read_u32(&out, 0);
        if count == 0 {
            return None;
        }
        // 一个卷跨多块盘时只取第一段（消费级设备与系统卷都是单段）。
        Some((read_u32(&out, 8), read_u64(&out, 16), read_u64(&out, 24)))
    }

    fn disk_extents(volume: &VolumeTopo) -> Option<(u32, u64, u64)> {
        let guid = volume.guid.trim_end_matches('\\');
        if let Some(extents) = disk_extents_path(guid) {
            return Some(extents);
        }
        // 降级：用盘符形态再试一次（\\.\C:）。
        for point in &volume.mount_points {
            let trimmed = point.trim_end_matches('\\');
            if let Some(letter) = drive_letter(trimmed) {
                let path = format!(r"\\.\{letter}");
                if let Some(extents) = disk_extents_path(&path) {
                    return Some(extents);
                }
            }
        }
        None
    }

    fn drive_letter(path: &str) -> Option<char> {
        let mut chars = path.chars();
        let letter = chars.next()?;
        if chars.next() != Some(':') || chars.next().is_some() {
            return None;
        }
        if letter.is_ascii_alphabetic() {
            Some(letter.to_ascii_uppercase())
        } else {
            None
        }
    }

    // ---------- 3. 磁盘型号 / 总线 / 容量 ----------

    /// STORAGE_DEVICE_DESCRIPTOR：厂商、型号、总线类型。
    fn storage_descriptor(number: u32) -> Option<(String, String, u32)> {
        let handle = open_device(&format!(r"\\.\PhysicalDrive{number}"))?;
        // STORAGE_PROPERTY_QUERY：PropertyId=StorageDeviceProperty(0)，QueryType=PropertyStandardQuery(0)
        let input = [0u8; 12];
        let mut out = vec![0u8; STORAGE_DESCRIPTOR_BYTES];
        let result = ioctl_with_input(handle, IOCTL_STORAGE_QUERY_PROPERTY, &input, &mut out);
        close_device(handle);
        result?;
        let vendor = read_descriptor_cstr(&out, read_u32(&out, 12) as usize);
        let product = read_descriptor_cstr(&out, read_u32(&out, 16) as usize);
        let bus = read_u32(&out, 28);
        Some((vendor, product, bus))
    }

    // ---------- 组装 ----------

    pub(super) fn build() -> Topology {
        let names = volume_names();
        let mut note = None;
        if names.is_empty() {
            note = Some("卷枚举失败：找不到任何卷".to_owned());
        }

        let mut volumes = Vec::with_capacity(names.len());
        for guid in names {
            let points = mount_points(&guid);
            let (label, file_system) = volume_information(&guid);
            let mut volume = VolumeTopo {
                guid,
                mount_points: points,
                label,
                file_system,
                disk: None,
                offset: 0,
                length: 0,
            };
            if let Some((disk, offset, length)) = disk_extents(&volume) {
                volume.disk = Some(disk);
                volume.offset = offset;
                volume.length = length;
            } else if note.is_none() {
                note = Some(format!("卷 {} 无法映射到物理盘", volume.guid));
            }
            volumes.push(volume);
        }

        // 每块盘：型号 + 总线 + 容量。
        //
        // 容量用该盘所有卷的「起始偏移 + 长度」最大值估算，不走
        // IOCTL_DISK_GET_DRIVE_GEOMETRY_EX：实测 USB 机械盘上这个 IOCTL
        // 要 2324 ms（唤醒自旋盘），会拖垮启动/首帧；差额只是未分配尾部，
        // 本机 NVMe 实测 953.9 GB → 显示 953.2 GB，机械盘无尾部差异。
        let mut numbers: Vec<u32> = volumes.iter().filter_map(|volume| volume.disk).collect();
        numbers.sort_unstable();
        numbers.dedup();

        let mut disks = Vec::with_capacity(numbers.len());
        for number in numbers {
            let (vendor, product, bus) =
                storage_descriptor(number).unwrap_or_else(|| (String::new(), String::new(), 0));
            let size = volumes
                .iter()
                .filter(|volume| volume.disk == Some(number))
                .map(|volume| volume.offset.saturating_add(volume.length))
                .max()
                .unwrap_or(0);
            disks.push(DiskInfo {
                number,
                vendor,
                product,
                bus,
                size,
            });
            if disks.last().is_some_and(|disk| disk.bus == 0) && note.is_none() {
                note = Some(format!("磁盘 {number} 型号/总线查询失败"));
            }
        }

        Topology {
            disks,
            volumes,
            note,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bus_labels_cover_common_hardware() {
        assert_eq!(bus_label(17), Some("NVMe"));
        assert_eq!(bus_label(7), Some("USB"));
        assert_eq!(bus_label(11), Some("SATA"));
        assert_eq!(bus_label(0), None);
        assert_eq!(bus_label(99), None);
    }

    #[test]
    fn model_label_handles_empty_parts() {
        let mut disk = DiskInfo {
            number: 0,
            vendor: String::new(),
            product: "KINGSTON OM8PGP41024N-A0".to_owned(),
            bus: 17,
            size: 1,
        };
        assert_eq!(
            model_label(&disk).as_deref(),
            Some("KINGSTON OM8PGP41024N-A0")
        );
        disk.vendor = "Seagate".to_owned();
        disk.product = "Expansion HDD".to_owned();
        assert_eq!(model_label(&disk).as_deref(), Some("Seagate Expansion HDD"));
        disk.vendor.clear();
        disk.product.clear();
        assert_eq!(model_label(&disk), None);
    }

    /// 真实环境：拿不到拓扑也不能 panic（虚拟机 / 受限权限下允许为空）。
    #[test]
    fn probe_never_panics() {
        let topo = probe();
        println!(
            "拓扑：{} 块盘 / {} 个卷{}",
            topo.disks.len(),
            topo.volumes.len(),
            topo.note
                .as_ref()
                .map(|note| format!("（{note}）"))
                .unwrap_or_default()
        );
        for volume in &topo.volumes {
            assert!(volume.guid.starts_with(r"\\?\Volume{"), "卷名形态异常");
            if let Some(disk) = volume.disk {
                assert!(topo.disks.iter().any(|info| info.number == disk));
            }
        }
        for disk in &topo.disks {
            assert!(disk.number < 256, "盘号异常");
        }
    }
}

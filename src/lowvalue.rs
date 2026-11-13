//! 低价值结果分类：按路径 / 文件名 / 扩展名给检索结果打「低价值」标签。
//!
//! 纯逻辑：不依赖 egui、不读数据库、不做 IO。输入只有路径、是否目录与
//! Windows 文件属性位，输出 `Verdict { bucket, reason }`，供检索页默认折叠。
//!
//! 口径见 docs/方案-二期体验改进-20261113.md 第 3 节，落地原则是
//! **宁可少判，不可误杀**：
//!
//! - 只有命中下表里明确的低价值形态才返回 `Some`，其余一律 `None`；
//! - 常见可用文件（`COMMON_EXTS` 白名单：文档 / 图片 / 音视频 / 压缩包 /
//!   程序链接 / 代码配置）不会因为扩展名被折叠；
//! - 白名单之外的扩展名归入「冷门」（`Bucket::Rare`），只影响 UI 折叠，
//!   AI/JSON 通道（--json / MCP）不读取判定结果；
//! - 无扩展名的文件不按扩展名判定；目录只看路径（目录名与所在目录）规则，
//!   不看扩展名；
//! - `.git` 子树整体豁免：路径任一段等于 `.git` 时直接不算低价值，
//!   `.gitignore` / `.gitattributes` 这类同名前缀文件不受影响；
//! - 每条判定都带一句稳定的中文原因，展开后可淡色标注或悬停解释。
//!
//! 判定顺序（先命中先返回，越具体的规则越靠前）：
//! Temp → Cache → Backup → Installer → System → Rare → Hidden。所以
//! `AppData\Local\Temp\a.dll` 是「临时」、回收站里的 dll 是「备份」、
//! `SoftwareDistribution\Download\update.cab` 是「缓存」，而不是被后置的
//! 系统 / 安装包 / 冷门 / 隐藏规则抢走；`.dll` / `.sys` 也因系统规则更靠前，
//! 仍归「系统」而不是「冷门」。
//!
//! ```ignore
//! let verdict = lowvalue::classify(r"C:\Users\me\AppData\Local\Temp\~$周报.docx", false, 0);
//! assert_eq!(verdict.unwrap().bucket, lowvalue::Bucket::Temp);
//! ```
//!
//! 路径比较统一先归一化（`/` → `\`、ASCII 小写），只用 `split` /
//! `contains` / `starts_with` / `ends_with` / `matches!` 判断，不引入正则表达式。

#![allow(dead_code)] // 判定小工具与常量保留完整实现：部分仅供单测或将来实时扫描使用。

/// `FILE_ATTRIBUTE_HIDDEN`：文件或目录带隐藏属性。
const ATTR_HIDDEN: i64 = 0x0000_0002;
/// `FILE_ATTRIBUTE_SYSTEM`：操作系统自用的文件。
const ATTR_SYSTEM: i64 = 0x0000_0004;

/// 临时文件扩展名。
const TEMP_EXTS: [&str; 6] = ["tmp", "temp", "part", "partial", "crdownload", "download"];
/// 备份文件扩展名。
const BACKUP_EXTS: [&str; 6] = ["bak", "backup", "old", "wbk", "asd", "swn"];
/// 系统文件扩展名。
const SYSTEM_EXTS: [&str; 12] = [
    "dll", "sys", "drv", "mui", "cpl", "ocx", "tlb", "pdb", "cat", "manifest", "etl", "dmp",
];
/// 只有在 Windows 系统目录里才算低价值的数据文件扩展名。
const SYSTEM_DATA_EXTS: [&str; 4] = ["dat", "bin", "db", "sqlite"];
/// 安装包扩展名。
const INSTALLER_EXTS: [&str; 6] = ["msi", "msix", "cab", "wim", "appx", "appxbundle"];
/// 用户常用、应保持可见的扩展名白名单（归一化后的 ASCII 小写）。
///
/// 只有「文件有扩展名、且不在本表」才判「冷门」；无扩展名文件与目录不参与。
/// 更靠前的具体规则（Temp / Cache / Backup / Installer / System）优先，
/// 所以 `.msi` 仍归安装包、`.dll` / `.sys` 仍归系统。
const COMMON_EXTS: &[&str] = &[
    // 文档
    "doc", "docx", "xls", "xlsx", "ppt", "pptx", "pdf", "txt", "md", "rtf", "odt", "csv", "epub",
    // 图片
    "jpg", "jpeg", "png", "gif", "bmp", "webp", "svg", "heic", "tif", "tiff", "ico",
    // 音视频
    "mp4", "mkv", "avi", "mov", "wmv", "flv", "webm", "mp3", "wav", "flac", "aac", "m4a",
    // 压缩包
    "zip", "rar", "7z", "tar", "gz", "bz2", "xz", "iso",
    // 程序 / 链接
    "exe", "msi", "lnk", "url",
    // 代码 / 配置
    "py", "rs", "js", "ts", "jsx", "tsx", "java", "c", "cpp", "h", "hpp", "cs", "go", "rb", "php",
    "sh", "bat", "cmd", "ps1", "sql", "html", "htm", "css", "json", "yaml", "yml", "toml", "ini",
    "xml",
];
/// 缓存目录名（归一化后按整段比较）。
const CACHE_DIRS: [&str; 6] = [
    "cache",
    "caches",
    "gpucache",
    "code cache",
    "service worker",
    "cachestorage",
];

/// 低价值结果的六个分类桶。`ALL` 的顺序就是检索页筛选区的展示顺序。
#[derive(Clone, Copy, PartialEq, Eq, Debug, Hash)]
pub enum Bucket {
    /// 临时文件：Office/WPS 锁文件、.tmp 之类、Temp/tmp 目录。
    Temp,
    /// 缓存：浏览器与应用的 Cache、日志、.cache、Windows 更新下载缓存。
    Cache,
    /// 备份：备份扩展名、带日期的备份名、回收站正文（$R）。
    Backup,
    /// 系统：dll/sys 等系统文件扩展名，以及 Windows 目录下的数据文件。
    System,
    /// 安装包：msi/msix/cab/wim/appx 安装包与 setup/install 类 exe。
    Installer,
    /// 冷门：有扩展名但不在 `COMMON_EXTS` 白名单里的文件（.d/.mui/.pdb/.etl/.dat 等）。
    /// 只影响 UI 折叠；.dll/.sys 等更具体的系统规则先命中，仍归 `System`。
    Rare,
    /// 隐藏：点开头的文件（.git 除外）、AppData / ProgramData / $Extend /
    /// System Volume Information，以及隐藏属性。
    Hidden,
}

impl Bucket {
    /// 七个桶，顺序即展示顺序（检索页折叠条直接遍历本数组，新增桶不会漏展示）。
    pub const ALL: [Bucket; 7] = [
        Bucket::Temp,
        Bucket::Cache,
        Bucket::Backup,
        Bucket::System,
        Bucket::Installer,
        Bucket::Rare,
        Bucket::Hidden,
    ];

    /// 界面上的中文名。
    pub fn label(self) -> &'static str {
        match self {
            Bucket::Temp => "临时",
            Bucket::Cache => "缓存",
            Bucket::Backup => "备份",
            Bucket::System => "系统",
            Bucket::Installer => "安装包",
            Bucket::Rare => "冷门",
            Bucket::Hidden => "隐藏",
        }
    }
}

/// 一条判定结果：桶 + 稳定的中文原因（供展开后的淡色标注 / tooltip 使用）。
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Verdict {
    /// 命中的分类桶。
    pub bucket: Bucket,
    /// 命中原因，静态字符串，可长期展示。
    pub reason: &'static str,
}

/// 判定一条检索结果是否属于低价值，命中时返回桶与原因。
///
/// - `path`：快照里的完整路径（反斜杠或正斜杠分隔都可）；
/// - `is_dir`：是否目录。目录不会按扩展名判定；
/// - `attributes`：Windows 文件属性位（`FILE_ATTRIBUTE_*`）。快照当前对目录
///   恒写 16、文件恒写 0，传 16/0 不会触发属性规则，只作将来实时扫描的兜底。
///
/// 判定顺序见模块文档；不命中任何规则时返回 `None`。
pub fn classify(path: &str, is_dir: bool, attributes: i64) -> Option<Verdict> {
    if path.trim().is_empty() {
        return None;
    }
    let normalized = normalize(path);
    let view = PathView::new(&normalized, is_dir);
    // .git 是开发者仓库：整棵子树都不折叠（.gitignore / .gitattributes 这类
    // 同名前缀文件不在此列，仍按点开头规则处理）。
    if view.segments.contains(&".git") {
        return None;
    }
    temp_verdict(&view)
        .or_else(|| cache_verdict(&view))
        .or_else(|| backup_verdict(&view))
        .or_else(|| installer_verdict(&view))
        .or_else(|| system_verdict(&view))
        .or_else(|| rare_verdict(&view))
        .or_else(|| hidden_verdict(&view, attributes))
}

/// 只判是否低价值，等价于 `classify(..).is_some()`。
pub fn is_low_value(path: &str, is_dir: bool, attributes: i64) -> bool {
    classify(path, is_dir, attributes).is_some()
}

// ---------------------------------------------------------------------------
// 各桶规则
// ---------------------------------------------------------------------------

fn temp_verdict(view: &PathView<'_>) -> Option<Verdict> {
    let name = view.name();
    if name.starts_with("~$") {
        return Some(Verdict {
            bucket: Bucket::Temp,
            reason: "Office/WPS 临时锁文件（~$ 开头）",
        });
    }
    if !view.is_dir && has_ext(name, &TEMP_EXTS) {
        return Some(Verdict {
            bucket: Bucket::Temp,
            reason: "临时文件扩展名（.tmp/.temp/.part/.partial/.crdownload/.download）",
        });
    }
    if view.has_folder("temp") || view.has_folder("tmp") {
        return Some(Verdict {
            bucket: Bucket::Temp,
            reason: "临时目录（Temp/tmp）",
        });
    }
    None
}

fn cache_verdict(view: &PathView<'_>) -> Option<Verdict> {
    for folder in view.folders() {
        if CACHE_DIRS.contains(folder) {
            return Some(Verdict {
                bucket: Bucket::Cache,
                reason: "缓存目录（Cache/Caches/GPUCache/Code Cache/Service Worker/CacheStorage）",
            });
        }
        if *folder == ".cache" {
            return Some(Verdict {
                bucket: Bucket::Cache,
                reason: "应用缓存目录（.cache）",
            });
        }
        if matches!(*folder, "log" | "logs") {
            return Some(Verdict {
                bucket: Bucket::Cache,
                reason: "日志目录（Logs/log）",
            });
        }
    }
    if view.has_folder_chain(&["softwaredistribution", "download"]) {
        return Some(Verdict {
            bucket: Bucket::Cache,
            reason: "Windows 更新下载缓存（SoftwareDistribution\\Download）",
        });
    }
    None
}

fn backup_verdict(view: &PathView<'_>) -> Option<Verdict> {
    let name = view.name();
    if !view.is_dir && has_ext(name, &BACKUP_EXTS) {
        return Some(Verdict {
            bucket: Bucket::Backup,
            reason: "备份文件扩展名（.bak/.backup/.old/.wbk/.asd/.swn）",
        });
    }
    if (name.contains("备份") || name.contains("backup")) && date_like(name) {
        return Some(Verdict {
            bucket: Bucket::Backup,
            reason: "备份文件名（含「备份/backup」与日期）",
        });
    }
    if name.contains(".~lock.") {
        return Some(Verdict {
            bucket: Bucket::Backup,
            reason: "LibreOffice 锁文件（.~lock.）",
        });
    }
    if view.has_folder("#recycle") {
        return Some(Verdict {
            bucket: Bucket::Backup,
            reason: "回收站（#recycle）",
        });
    }
    if view.has_folder("$recycle.bin") && is_recycle_body(name) {
        return Some(Verdict {
            bucket: Bucket::Backup,
            reason: "回收站正文文件（$R）",
        });
    }
    None
}

fn installer_verdict(view: &PathView<'_>) -> Option<Verdict> {
    if view.is_dir {
        return None;
    }
    let name = view.name();
    if has_ext(name, &INSTALLER_EXTS) {
        return Some(Verdict {
            bucket: Bucket::Installer,
            reason: "安装包扩展名（.msi/.msix/.cab/.wim/.appx/.appxbundle）",
        });
    }
    let (stem, ext) = split_name(name);
    if ext == "exe" {
        if stem.contains("setup") {
            return Some(Verdict {
                bucket: Bucket::Installer,
                reason: "安装程序（文件名含 setup 的 exe）",
            });
        }
        if stem.contains("uninstall") {
            return Some(Verdict {
                bucket: Bucket::Installer,
                reason: "卸载程序（文件名含 uninstall 的 exe）",
            });
        }
        if stem.contains("install") {
            return Some(Verdict {
                bucket: Bucket::Installer,
                reason: "安装程序（文件名含 install 的 exe）",
            });
        }
    }
    None
}

fn system_verdict(view: &PathView<'_>) -> Option<Verdict> {
    if view.is_dir {
        return None;
    }
    let name = view.name();
    if has_ext(name, &SYSTEM_EXTS) {
        return Some(Verdict {
            bucket: Bucket::System,
            reason: "Windows 系统文件扩展名（.dll/.sys/.drv/.mui/.cpl/.ocx/.tlb/.pdb/.cat/.manifest/.etl/.dmp）",
        });
    }
    // 快照里 .db 之类也可能是用户自己的数据库，只有落在 Windows 系统目录才算系统文件。
    if view.has_folder("windows") && has_ext(name, &SYSTEM_DATA_EXTS) {
        return Some(Verdict {
            bucket: Bucket::System,
            reason: "Windows 系统目录下的数据文件（.dat/.bin/.db/.sqlite）",
        });
    }
    None
}

/// 「冷门」：有扩展名、但扩展名不在 `COMMON_EXTS` 白名单里。
///
/// 放在 System 之后、Hidden 之前：更具体的系统 / 安装包规则先命中，点开头
/// 文件（无扩展名）与路径类隐藏规则则留给后面的 Hidden。
fn rare_verdict(view: &PathView<'_>) -> Option<Verdict> {
    if view.is_dir {
        return None;
    }
    let (stem, ext) = split_name(view.name());
    if stem.is_empty() || ext.is_empty() {
        return None;
    }
    if COMMON_EXTS.contains(&ext) {
        return None;
    }
    Some(Verdict {
        bucket: Bucket::Rare,
        reason: "扩展名极少使用",
    })
}

fn hidden_verdict(view: &PathView<'_>, attributes: i64) -> Option<Verdict> {
    // 点开头的段（文件或目录）；.git 是开发者仓库，按需求整体豁免。
    if view
        .segments
        .iter()
        .any(|segment| is_hidden_segment(segment))
    {
        return Some(Verdict {
            bucket: Bucket::Hidden,
            reason: "隐藏文件/目录（点开头）",
        });
    }
    // AppData\Local\Programs 放的是用户自己安装的程序，折叠了会误伤，明确豁免。
    let in_local_programs = view.has_folder_chain(&["appdata", "local", "programs"]);
    for folder in view.folders() {
        match *folder {
            "appdata" if !in_local_programs => {
                return Some(Verdict {
                    bucket: Bucket::Hidden,
                    reason: "应用数据目录（AppData）",
                });
            }
            "programdata" => {
                return Some(Verdict {
                    bucket: Bucket::Hidden,
                    reason: "程序数据目录（ProgramData）",
                });
            }
            "$extend" => {
                return Some(Verdict {
                    bucket: Bucket::Hidden,
                    reason: "NTFS 扩展元数据（$Extend）",
                });
            }
            "system volume information" => {
                return Some(Verdict {
                    bucket: Bucket::Hidden,
                    reason: "系统卷影信息（System Volume Information）",
                });
            }
            _ => {}
        }
    }
    // 属性位兜底：快照恒 0/16 时不会触发；将来接实时扫描时生效。
    if attributes & ATTR_SYSTEM != 0 {
        return Some(Verdict {
            bucket: Bucket::System,
            reason: "系统文件属性（FILE_ATTRIBUTE_SYSTEM）",
        });
    }
    if attributes & ATTR_HIDDEN != 0 {
        return Some(Verdict {
            bucket: Bucket::Hidden,
            reason: "隐藏文件属性（FILE_ATTRIBUTE_HIDDEN）",
        });
    }
    None
}

// ---------------------------------------------------------------------------
// 小工具
// ---------------------------------------------------------------------------

/// 归一化路径：正斜杠统一成反斜杠，ASCII 转小写（中文不受影响）。
fn normalize(path: &str) -> String {
    path.chars()
        .map(|ch| match ch {
            '/' => '\\',
            other => other.to_ascii_lowercase(),
        })
        .collect()
}

/// 归一化路径的只读视图：一次切段，供各规则复用。
struct PathView<'a> {
    segments: Vec<&'a str>,
    is_dir: bool,
}

impl<'a> PathView<'a> {
    fn new(path: &'a str, is_dir: bool) -> Self {
        let segments: Vec<&'a str> = path
            .split('\\')
            .filter(|segment| !segment.is_empty())
            .collect();
        Self { segments, is_dir }
    }

    /// 最后一段：文件或目录自身的名字（卷根退化成 `c:`，不命中任何规则）。
    fn name(&self) -> &'a str {
        self.segments.last().copied().unwrap_or("")
    }

    /// 所在目录的各段；目录自身也算一段（`C:\ProgramData` 判 Hidden）。
    fn folders(&self) -> &[&'a str] {
        let end = if self.is_dir {
            self.segments.len()
        } else {
            self.segments.len().saturating_sub(1)
        };
        &self.segments[..end]
    }

    fn has_folder(&self, wanted: &str) -> bool {
        self.folders().contains(&wanted)
    }

    /// 连续出现的目录链，如 `appdata\local\programs`。
    fn has_folder_chain(&self, chain: &[&str]) -> bool {
        let folders = self.folders();
        if chain.is_empty() || folders.len() < chain.len() {
            return false;
        }
        folders
            .windows(chain.len())
            .any(|window| window.iter().zip(chain).all(|(left, right)| left == right))
    }
}

/// 拆扩展名；无扩展名或点开头的隐藏文件名（`.gitignore`）都返回空扩展名。
fn split_name(name: &str) -> (&str, &str) {
    match name.rsplit_once('.') {
        Some((stem, ext)) if !stem.is_empty() => (stem, ext),
        _ => (name, ""),
    }
}

fn has_ext(name: &str, wanted: &[&str]) -> bool {
    let (stem, ext) = split_name(name);
    !stem.is_empty() && wanted.contains(&ext)
}

/// 回收站正文文件：`$R` 开头；`$I` 索引不算，回收站根目录自身也不算。
fn is_recycle_body(name: &str) -> bool {
    name.len() > 2 && name.starts_with("$r") && !name.starts_with("$recycle.bin")
}

/// 点开头的段（`.`/`..` 除外；`.git` 目录除外）。
fn is_hidden_segment(segment: &str) -> bool {
    segment.starts_with('.') && !matches!(segment, "." | ".." | ".git")
}

/// 名字里是否带日期样式：`20261113`、`2026-11-13`、`2026_11_13`、
/// `2026.11.13`、`2026年11月13日` 等；只有`备份`/`backup`同时出现时才用它。
fn date_like(text: &str) -> bool {
    let chars: Vec<char> = text.chars().collect();
    let mut run = 0usize;
    for ch in &chars {
        if ch.is_ascii_digit() {
            run += 1;
            if run >= 8 {
                return true;
            }
        } else {
            run = 0;
        }
    }
    let mut index = 0usize;
    while index < chars.len() {
        if !chars[index].is_ascii_digit() {
            index += 1;
            continue;
        }
        let start = index;
        while index < chars.len() && chars[index].is_ascii_digit() {
            index += 1;
        }
        if index - start == 4 && has_two_numbers_after(&chars, index) {
            return true;
        }
    }
    false
}

/// 从 `index` 起是否还有两段 1–2 位数字（允许各带一个分隔符，如 `-`、`年`）。
fn has_two_numbers_after(chars: &[char], mut index: usize) -> bool {
    for _ in 0..2 {
        if index < chars.len() && !chars[index].is_ascii_digit() {
            index += 1;
        }
        let start = index;
        while index < chars.len() && chars[index].is_ascii_digit() {
            index += 1;
        }
        let digits = index - start;
        if !(1..=2).contains(&digits) {
            return false;
        }
    }
    true
}

#[cfg(test)]
mod tests {
    use super::*;

    fn bucket_of(path: &str, is_dir: bool) -> Option<Bucket> {
        classify(path, is_dir, 0).map(|verdict| verdict.bucket)
    }

    fn bucket_with_attrs(path: &str, is_dir: bool, attributes: i64) -> Option<Bucket> {
        classify(path, is_dir, attributes).map(|verdict| verdict.bucket)
    }

    fn assert_bucket(path: &str, expected: Bucket) {
        let verdict = classify(path, false, 0)
            .unwrap_or_else(|| panic!("应判为低价值（{expected:?}）：{path}"));
        assert_eq!(
            verdict.bucket, expected,
            "路径 {path} 命中了：{}",
            verdict.reason
        );
        assert!(!verdict.reason.is_empty(), "原因字符串不能为空：{path}");
    }

    fn assert_dir_bucket(path: &str, expected: Bucket) {
        let verdict = classify(path, true, 0)
            .unwrap_or_else(|| panic!("目录应判为低价值（{expected:?}）：{path}"));
        assert_eq!(
            verdict.bucket, expected,
            "目录 {path} 命中了：{}",
            verdict.reason
        );
    }

    fn assert_kept(path: &str) {
        assert_eq!(bucket_of(path, false), None, "不应判为低价值：{path}");
    }

    fn assert_dir_kept(path: &str) {
        assert_eq!(bucket_of(path, true), None, "目录不应判为低价值：{path}");
    }

    #[test]
    fn office_lock_files_are_temp() {
        assert_bucket(r"C:\Users\me\Desktop\~$季度报告.docx", Bucket::Temp);
        assert_bucket(r"D:\资料\~$预算.xlsx", Bucket::Temp);
        assert_bucket(r"C:\Users\me\AppData\Local\Temp\~$锁.docx", Bucket::Temp);
    }

    #[test]
    fn temp_extensions_are_temp() {
        for ext in ["tmp", "temp", "part", "partial", "crdownload", "download"] {
            assert_bucket(&format!(r"D:\下载\未命名.{ext}"), Bucket::Temp);
            assert_bucket(
                &format!(r"D:\下载\UNNAMED.{}", ext.to_ascii_uppercase()),
                Bucket::Temp,
            );
        }
    }

    #[test]
    fn temp_directories_are_temp() {
        assert_bucket(r"C:\Users\me\AppData\Local\Temp\cache.txt", Bucket::Temp);
        assert_bucket(r"D:\tmp\build\output.log", Bucket::Temp);
        assert_bucket(r"C:\Windows\Temp\error.txt", Bucket::Temp);
        assert_dir_bucket(r"C:\Users\me\AppData\Local\Temp", Bucket::Temp);
        assert_dir_bucket(r"D:\项目\tmp", Bucket::Temp);
    }

    #[test]
    fn browser_caches_are_cache() {
        assert_bucket(
            r"C:\Users\me\AppData\Local\Google\Chrome\User Data\Default\Cache\data_1",
            Bucket::Cache,
        );
        assert_bucket(
            r"C:\Users\me\AppData\Local\Google\Chrome\User Data\Default\GPUCache\data_0",
            Bucket::Cache,
        );
        assert_bucket(
            r"C:\Users\me\AppData\Local\Google\Chrome\User Data\Default\Code Cache\js\index",
            Bucket::Cache,
        );
        assert_bucket(
            r"C:\Users\me\AppData\Local\Microsoft\Edge\User Data\Default\Service Worker\CacheStorage\abc\meta",
            Bucket::Cache,
        );
        assert_bucket(
            r"C:\Users\me\AppData\Local\Packages\App\LocalState\Caches\c.dat",
            Bucket::Cache,
        );
    }

    #[test]
    fn cache_and_log_directories_are_cache() {
        assert_bucket(r"D:\app\Logs\20261113.txt", Bucket::Cache);
        assert_bucket(r"D:\app\log\run.txt", Bucket::Cache);
        assert_bucket(r"C:\Users\me\.cache\pip\http\abc", Bucket::Cache);
        assert_dir_bucket(r"C:\Users\me\.cache", Bucket::Cache);
        assert_bucket(
            r"C:\Windows\SoftwareDistribution\Download\update.cab",
            Bucket::Cache,
        );
        assert_dir_bucket(r"C:\Windows\SoftwareDistribution\Download", Bucket::Cache);
    }

    #[test]
    fn backup_extensions_are_backup() {
        for ext in ["bak", "backup", "old", "wbk", "asd", "swn"] {
            assert_bucket(&format!(r"D:\资料\年度总结.{ext}"), Bucket::Backup);
        }
    }

    #[test]
    fn backup_names_with_dates_are_backup() {
        assert_bucket(r"D:\资料\项目备份20261113.docx", Bucket::Backup);
        assert_bucket(r"D:\资料\backup_2026-11-13.zip", Bucket::Backup);
        assert_bucket(r"D:\资料\BACKUP.2026.11.13.tar", Bucket::Backup);
        assert_dir_bucket(r"D:\备份\照片备份2026年11月13日", Bucket::Backup);
    }

    #[test]
    fn backup_names_need_a_date() {
        assert_kept(r"D:\资料\我的备份.docx");
        assert_kept(r"D:\资料\backup.zip");
        assert_kept(r"D:\资料\backup2026.docx");
        assert_dir_kept(r"D:\资料\备份");
    }

    #[test]
    fn recycle_bin_body_is_backup() {
        assert_bucket(
            r"C:\$Recycle.Bin\S-1-5-21-1004336348-1177238915-682003330-1001\$RABCDEF.docx",
            Bucket::Backup,
        );
        // $I 只是索引，不算正文；回收站根目录本身也不折叠。
        assert_kept(r"C:\$Recycle.Bin\S-1-5-21-1004336348-1177238915-682003330-1001\$IABCDEF.docx");
        assert_dir_kept(r"C:\$Recycle.Bin");
    }

    #[test]
    fn synology_recycle_is_backup() {
        assert_bucket(r"\\NAS\share\#recycle\photo.jpg", Bucket::Backup);
        assert_bucket(r"\\NAS\share\#recycle\报告.docx", Bucket::Backup);
    }

    #[test]
    fn libreoffice_lock_is_backup() {
        assert_bucket(r"D:\资料\.~lock.年度报告.docx#", Bucket::Backup);
    }

    #[test]
    fn windows_system_extensions_are_system() {
        for ext in [
            "dll", "sys", "drv", "mui", "cpl", "ocx", "tlb", "pdb", "cat", "manifest", "etl", "dmp",
        ] {
            assert_bucket(
                &format!(r"C:\Windows\System32\payload.{ext}"),
                Bucket::System,
            );
        }
    }

    #[test]
    fn winsxs_dll_is_system() {
        assert_bucket(r"C:\Windows\System32\kernel32.dll", Bucket::System);
        assert_bucket(
            r"C:\Windows\WinSxS\amd64_microsoft-windows-kernel32_31bf3856ad364e35\kernel32.dll",
            Bucket::System,
        );
    }

    #[test]
    fn windows_data_files_are_system() {
        assert_bucket(r"C:\Windows\System32\config\SRUDB.dat", Bucket::System);
        assert_bucket(
            r"C:\Windows\System32\catroot2\{F750E6C3-38EE-11D1-85E5-00C04FC295EE}\catdb.bin",
            Bucket::System,
        );
        assert_bucket(r"C:\Windows\System32\Windows.db", Bucket::System);
        assert_bucket(r"C:\Windows\System32\index.sqlite", Bucket::System);
    }

    #[test]
    fn user_documents_databases_are_rare_not_system() {
        // 用户目录里的 .db/.sqlite/.dat 不是系统文件；但它们也不在常用白名单里，
        // 因此按新规则归「冷门」（只影响 UI 折叠，不改变 AI/JSON 结果）。
        assert_eq!(
            bucket_of(r"C:\Users\me\Documents\data.db", false),
            Some(Bucket::Rare)
        );
        assert_eq!(
            bucket_of(r"D:\资料\项目\data.sqlite", false),
            Some(Bucket::Rare)
        );
        assert_eq!(
            bucket_of(r"C:\Users\me\Desktop\mine.dat", false),
            Some(Bucket::Rare)
        );
        // 反例：Windows 目录下的同名数据文件仍是「系统」。
        assert_bucket(r"C:\Windows\System32\config\SRUDB.dat", Bucket::System);
    }

    #[test]
    fn installer_packages_are_installer() {
        for ext in ["msi", "msix", "cab", "wim", "appx", "appxbundle"] {
            assert_bucket(&format!(r"D:\下载\安装包.{ext}"), Bucket::Installer);
        }
        assert_bucket(r"C:\Windows\Installer\1a2b3c.msi", Bucket::Installer);
    }

    #[test]
    fn installer_exe_names_are_installer() {
        assert_bucket(r"D:\下载\setup.exe", Bucket::Installer);
        assert_bucket(r"D:\下载\VSCodeSetup.exe", Bucket::Installer);
        assert_bucket(r"D:\下载\install.exe", Bucket::Installer);
        assert_bucket(r"D:\下载\uninstall.exe", Bucket::Installer);
        assert_kept(r"D:\下载\setup.txt");
        assert_kept(r"D:\资料\安装笔记.md");
    }

    #[test]
    fn hidden_dot_entries_are_hidden() {
        assert_bucket(r"C:\Users\me\Desktop\.secret", Bucket::Hidden);
        assert_bucket(r"D:\proj\.gitignore", Bucket::Hidden);
        assert_bucket(r"D:\proj\.env", Bucket::Hidden);
        assert_bucket(r"C:\Users\me\.ssh\id_rsa", Bucket::Hidden);
        assert_dir_bucket(r"D:\proj\.idea", Bucket::Hidden);
    }

    #[test]
    fn git_repository_files_are_kept() {
        assert_dir_kept(r"D:\proj\.git");
        assert_kept(r"D:\proj\.git\config");
        assert_kept(r"D:\proj\.git\HEAD");
        assert_kept(r"D:\proj\.git\objects\ab\cdef0123456789");
        assert_kept(r"D:\proj\src\main.rs");
    }

    #[test]
    fn git_subtree_is_never_low_value() {
        // 整棵 .git 子树直接豁免，不靠"点开头"那一条；大小写不敏感。
        assert_kept(r"D:\proj\.git\config");
        assert_kept(r"C:\Users\me\project\.GIT\config");
        assert_dir_kept(r"D:\proj\.git");
    }

    #[test]
    fn git_logs_do_not_become_cache() {
        // .git\logs 里的 reflog 不会因为 logs 段被误判成缓存。
        assert_kept(r"D:\proj\.git\logs\HEAD");
        assert_kept(r"D:\proj\.git\logs\refs\heads\main");
        // 同名前缀的普通文件不受豁免影响：.gitignore 仍是点开头的隐藏文件。
        assert_bucket(r"D:\proj\.gitignore", Bucket::Hidden);
    }

    #[test]
    fn appdata_and_programdata_are_hidden() {
        assert_bucket(
            r"C:\Users\me\AppData\Roaming\SomeApp\state.json",
            Bucket::Hidden,
        );
        assert_bucket(r"C:\ProgramData\SomeVendor\License.txt", Bucket::Hidden);
        // 白名单之外的扩展名先在 Rare 命中：仍是折叠，只是桶不同。
        assert_bucket(r"C:\ProgramData\SomeVendor\License.dat", Bucket::Rare);
        assert_dir_bucket(r"C:\Users\me\AppData", Bucket::Hidden);
        assert_dir_bucket(r"C:\ProgramData", Bucket::Hidden);
    }

    #[test]
    fn installed_programs_under_appdata_are_kept() {
        assert_kept(r"C:\Users\me\AppData\Local\Programs\Microsoft VS Code\Code.exe");
        assert_kept(r"C:\Users\me\AppData\Local\Programs\Discord\Update.exe");
        assert_dir_kept(r"C:\Users\me\AppData\Local\Programs");
    }

    #[test]
    fn extend_and_system_volume_information_are_hidden() {
        assert_bucket(r"C:\$Extend\$UsnJrnl:$J", Bucket::Hidden);
        assert_bucket(
            r"C:\System Volume Information\{3808876b-c176-4e48-b7ae-04046e6cc752}",
            Bucket::Hidden,
        );
        assert_dir_bucket(r"C:\System Volume Information", Bucket::Hidden);
    }

    #[test]
    fn attribute_bits_are_respected() {
        assert_eq!(
            bucket_with_attrs(r"D:\资料\a.txt", false, 0x2),
            Some(Bucket::Hidden)
        );
        assert_eq!(
            bucket_with_attrs(r"D:\资料\a.txt", false, 0x4),
            Some(Bucket::System)
        );
        // 系统属性优先于隐藏属性。
        assert_eq!(
            bucket_with_attrs(r"D:\资料\a.txt", false, 0x6),
            Some(Bucket::System)
        );
        // 目录属性 16 不是低价值信号。
        assert_eq!(bucket_with_attrs(r"D:\资料", true, 0x10), None);
        // 具体规则永远先于属性兜底。
        assert_eq!(
            bucket_with_attrs(r"D:\下载\a.msi", false, 0x2),
            Some(Bucket::Installer)
        );
    }

    #[test]
    fn directories_ignore_extensions() {
        assert_dir_kept(r"D:\资料\归档.bak");
        assert_dir_kept(r"D:\资料\setup.exe");
        assert_dir_kept(r"D:\资料\kernel32.dll");
        assert_dir_kept(r"D:\资料\安装包.msi");
        assert_dir_kept(r"D:\资料\冷门.pnf");
        assert_dir_kept(r"D:\资料\build.d");
    }

    #[test]
    fn directories_follow_path_rules() {
        assert_dir_bucket(r"C:\Users\me\AppData\Local\Temp", Bucket::Temp);
        assert_dir_bucket(
            r"C:\Users\me\AppData\Local\Google\Chrome\User Data\Default\Cache",
            Bucket::Cache,
        );
        assert_dir_bucket(r"C:\Users\me\.cache", Bucket::Cache);
        assert_dir_bucket(r"D:\proj\.idea", Bucket::Hidden);
        assert_dir_bucket(r"C:\ProgramData", Bucket::Hidden);
    }

    #[test]
    fn common_documents_on_desktop_are_kept() {
        for ext in [
            "doc", "docx", "xls", "xlsx", "ppt", "pptx", "pdf", "jpg", "jpeg", "png", "mp4", "zip",
            "7z", "rar", "py", "rs", "md", "txt", "csv", "json",
        ] {
            assert_kept(&format!(r"C:\Users\me\Desktop\report.{ext}"));
            assert_kept(&format!(r"D:\资料\报告.{ext}"));
        }
    }

    #[test]
    fn common_files_are_kept_in_any_case() {
        assert_kept(r"C:\Users\me\Desktop\REPORT.DOCX");
        assert_kept(r"C:\Users\me\Desktop\Photo.JPG");
        assert_kept(r"D:\资料\项目.ZIP");
        assert_kept(r"C:\Users\me\Desktop\main.rs");
    }

    #[test]
    fn rare_extensions_are_rare() {
        // 需求清单里的冷门后缀逐条核对：没有更靠前的规则命中时归「冷门」。
        for path in [
            r"D:\资料\a.d",
            r"D:\资料\a.obj",
            r"D:\资料\a.lib",
            r"D:\资料\a.ilk",
            r"D:\资料\a.pch",
            r"D:\资料\a.ipdb",
            r"D:\资料\a.map",
            r"D:\资料\a.exp",
            r"D:\资料\a.mum",
            r"D:\资料\a.cdf-ms",
            r"D:\资料\a.evtx",
            r"D:\资料\a.edb",
            r"D:\资料\a.pnf",
            r"D:\资料\a.log",
            r"D:\资料\a.dat",
            r"D:\资料\a.bin",
            r"D:\资料\a.cache",
            r"D:\资料\a.pak",
        ] {
            assert_bucket(path, Bucket::Rare);
        }
        // 白名单 / 空扩展名：不折叠（.h 在代码白名单，.ini 在配置白名单）。
        assert_kept(r"D:\资料\a.h");
        assert_kept(r"D:\资料\a.ini");
        assert_kept(r"D:\资料\README");
        assert_kept(r"D:\资料\a.");
    }

    #[test]
    fn rare_respects_more_specific_buckets() {
        // System 规则先于 Rare：这些扩展名不会因为「不常见」被改成冷门。
        for ext in [
            "pdb", "manifest", "cat", "mui", "ocx", "tlb", "cpl", "drv", "sys", "etl", "dmp",
        ] {
            assert_bucket(
                &format!(r"C:\Windows\System32\payload.{ext}"),
                Bucket::System,
            );
        }
        // Temp / Backup 同样先命中。
        assert_bucket(r"D:\下载\未命名.tmp", Bucket::Temp);
        assert_bucket(r"D:\资料\年度总结.bak", Bucket::Backup);
    }

    #[test]
    fn common_exts_whitelist_is_kept() {
        // 需求清单共 77 个常用扩展名；少一个 / 多一个都会在这里失败。
        assert_eq!(COMMON_EXTS.len(), 77, "COMMON_EXTS 数量与需求清单不一致");
        for ext in COMMON_EXTS {
            let path = format!(r"D:\资料\report.{ext}");
            if *ext == "msi" {
                // .msi 被更靠前的安装包规则收走，但仍不算冷门。
                assert_eq!(bucket_of(&path, false), Some(Bucket::Installer), "{path}");
            } else {
                assert_eq!(bucket_of(&path, false), None, "白名单扩展名不应折叠：{path}");
            }
        }
        // 抽样确认各分组没有整段漏掉。
        for ext in [
            "docx", "epub", "heic", "ico", "mkv", "m4a", "7z", "iso", "lnk", "url", "rs", "tsx",
            "ps1", "yaml", "xml",
        ] {
            assert!(COMMON_EXTS.contains(&ext), "白名单缺少 {ext}");
        }
    }

    #[test]
    fn files_without_extensions_are_not_rare() {
        for path in [
            r"D:\资料\README",
            r"D:\资料\Makefile",
            r"D:\proj\LICENSE",
            r"D:\资料\带空格 无后缀",
            r"C:\Windows\System32\drivers\etc\hosts",
        ] {
            assert_eq!(bucket_of(path, false), None, "无扩展名文件不应判冷门：{path}");
        }
        // 点开头文件（无扩展名）仍按隐藏规则处理，而不是冷门。
        assert_bucket(r"D:\proj\.gitignore", Bucket::Hidden);
    }

    #[test]
    fn rare_precedes_hidden_but_not_system() {
        // 隐藏目录里的冷门后缀：Rare 先于 Hidden 命中，但仍然是折叠。
        assert_bucket(r"C:\Users\me\AppData\Roaming\SomeApp\cache.dat", Bucket::Rare);
        assert_bucket(r"C:\ProgramData\SomeVendor\License.dat", Bucket::Rare);
        // 点开头文件没有扩展名，归 Hidden。
        assert_bucket(r"D:\proj\.env", Bucket::Hidden);
        // .dat 在 Windows 目录下仍归 System。
        assert_bucket(r"C:\Windows\System32\config\SRUDB.dat", Bucket::System);
    }

    #[test]
    fn precedence_is_specific_to_general() {
        assert_bucket(r"C:\Users\me\AppData\Local\Temp\a.dll", Bucket::Temp);
        assert_bucket(r"C:\$Recycle.Bin\S-1-5-21-1\$R1.dll", Bucket::Backup);
        assert_bucket(
            r"C:\Windows\SoftwareDistribution\Download\update.cab",
            Bucket::Cache,
        );
        assert_bucket(
            r"C:\Users\me\AppData\Local\Google\Chrome\User Data\Default\Cache\data_0",
            Bucket::Cache,
        );
        assert_bucket(r"C:\Windows\Installer\1a2b3c.msi", Bucket::Installer);
        assert_bucket(r"C:\Windows\System32\a.dll", Bucket::System);
    }

    #[test]
    fn bucket_labels_and_all_are_stable() {
        let labels: Vec<&str> = Bucket::ALL.iter().map(|bucket| bucket.label()).collect();
        assert_eq!(
            labels,
            vec!["临时", "缓存", "备份", "系统", "安装包", "冷门", "隐藏"]
        );
        assert_eq!(Bucket::ALL.len(), 7);
        let samples = [
            (r"C:\Temp\a.txt", Bucket::Temp),
            (r"D:\app\Cache\a.bin", Bucket::Cache),
            (r"D:\资料\a.bak", Bucket::Backup),
            (r"C:\Windows\System32\a.dll", Bucket::System),
            (r"D:\下载\a.msi", Bucket::Installer),
            (r"D:\资料\a.pnf", Bucket::Rare),
            (r"C:\Users\me\AppData\Roaming\a.txt", Bucket::Hidden),
        ];
        let mut seen = Vec::new();
        for (path, expected) in samples {
            let bucket = bucket_of(path, false).unwrap_or_else(|| panic!("应判为低价值：{path}"));
            assert_eq!(bucket, expected, "{path}");
            seen.push(bucket);
        }
        // 六个桶都在样本里出现一次。
        assert_eq!(seen, Bucket::ALL.to_vec());
    }

    #[test]
    fn is_low_value_agrees_with_classify() {
        let cases = [
            r"C:\Windows\System32\kernel32.dll",
            r"D:\资料\report.docx",
            r"D:\proj\.git\config",
            r"C:\Users\me\AppData\Local\Temp\x.tmp",
            r"C:\ProgramData\SomeVendor\state.dat",
            r"\\NAS\share\#recycle\a.jpg",
        ];
        for path in cases {
            assert_eq!(
                is_low_value(path, false, 0),
                classify(path, false, 0).is_some(),
                "{path}"
            );
        }
        assert!(!is_low_value("", false, 0));
        assert!(is_low_value(r"C:\Windows\System32\a.dll", false, 0));
    }

    #[test]
    fn odd_and_empty_paths_do_not_panic() {
        assert_eq!(bucket_of("", false), None);
        assert_eq!(bucket_of("   ", false), None);
        assert_dir_kept(r"C:\");
        assert_dir_kept(r"\\?\Volume{9dba3ee0-0000-0000-0000-000000000000}\");
        assert_bucket(r"\\?\C:\Windows\System32\a.dll", Bucket::System);
    }

    #[test]
    fn forward_slashes_are_normalized() {
        assert_bucket("C:/Windows/System32/a.dll", Bucket::System);
        assert_bucket("C:/Users/me/AppData/Local/Temp/a.txt", Bucket::Temp);
        assert_bucket("C:/Users/me/AppData/Roaming/a.txt", Bucket::Hidden);
    }

    #[test]
    fn date_like_forms() {
        assert!(date_like("备份20261113"));
        assert!(date_like("备份2026-11-13"));
        assert!(date_like("backup_2026_11_13"));
        assert!(date_like("备份2026.11.3"));
        assert!(date_like("备份2026年11月13日"));
        assert!(!date_like("备份2026"));
        assert!(!date_like("backup"));
        assert!(!date_like("2026"));
    }

    #[test]
    fn reason_texts_mention_their_marker() {
        let pairs = [
            (r"C:\Users\me\Desktop\~$a.docx", "~$"),
            (r"D:\资料\a.bak", ".bak"),
            (r"C:\$Recycle.Bin\S-1-5-21-1\$R1.docx", "$R"),
            (r"\\NAS\share\#recycle\a.txt", "#recycle"),
            (r"C:\Windows\System32\a.dll", ".dll"),
            (r"D:\下载\a.msi", ".msi"),
            (r"D:\资料\a.pnf", "扩展名极少使用"),
            (r"C:\ProgramData\a.txt", "ProgramData"),
            (r"D:\proj\.gitignore", "点开头"),
        ];
        for (path, marker) in pairs {
            let verdict =
                classify(path, false, 0).unwrap_or_else(|| panic!("应判为低价值：{path}"));
            assert!(
                verdict.reason.contains(marker),
                "{path} 的原因应包含 {marker}，实际：{}",
                verdict.reason
            );
        }
    }
}

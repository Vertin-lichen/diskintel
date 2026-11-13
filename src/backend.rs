//! 后端桥：只在「建库」这一低频动作上调用 Python 版 CLI。
//!
//! 设计边界——Rust 进程永远不依赖 Python 常驻：
//! - 浏览 / 检索 / 排序：Rust 直读 SQLite，纯本地，零 IPC；
//! - 建库（scan → snap）：临时拉起 `diskintel` 一次，跑完即退。
//!
//! 这样安装包里 Python 只是「建库器」，不是运行时。

use std::io::Read;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::mpsc::Sender;
use std::time::Instant;

#[derive(Clone, Debug)]
pub enum BuildEvent {
    Stage(String),
    Finished(Result<String, String>),
}

pub struct Backend {
    pub exe: PathBuf,
    pub data_root: PathBuf,
    /// 随包分发的 goz 客户端；有值时通过 `DISKINTEL_GOZ` 传给建库器，
    /// 免去用户手工配置 PATH 或依赖本机既有安装。
    pub goz: Option<PathBuf>,
}

impl Backend {
    /// 按「显式配置 → 同目录 → PATH → 开发路径」的顺序找建库器。
    pub fn locate(data_root: &Path) -> Option<Self> {
        let mut candidates: Vec<PathBuf> = Vec::new();
        if let Ok(value) = std::env::var("DISKINTEL_CLI") {
            candidates.push(PathBuf::from(value));
        }
        if let Ok(exe) = std::env::current_exe() {
            if let Some(dir) = exe.parent() {
                candidates.push(dir.join("DiskIntelCLI.exe"));
                candidates.push(dir.join("diskintel.exe"));
            }
        }
        candidates.push(PathBuf::from(r"D:\disk-intel-data\bin\DiskIntelCLI.exe"));
        // 开发机上的构建产物；发布时随安装包放在同目录，这一项自然落空。
        candidates.push(PathBuf::from(
            r"C:\Users\<user>\DeskBox\项目文件夹\工作路径\项目\AI协同通用工作路径\磁盘情报系统\dist\DiskIntelCLI.exe",
        ));

        let exe = candidates.into_iter().find(|path| path.is_file())?;
        let dir = exe.parent()?.to_path_buf();
        let goz = [
            dir.join("goz.exe"),
            dir.join("goz").join("goz.exe"),
            dir.join("bin").join("goz.exe"),
        ]
        .into_iter()
        .find(|path| path.is_file());

        // 建库器的 `--data-root` 要的是父目录：它内部会自己拼 `snapshots/`。
        // 窗口这边拿到的往往是快照目录本身，这里统一退一级，避免建库写进
        // `snapshots\snapshots\`。
        let data_root = if data_root
            .file_name()
            .and_then(|name| name.to_str())
            .is_some_and(|name| name.eq_ignore_ascii_case("snapshots"))
        {
            data_root
                .parent()
                .map(Path::to_path_buf)
                .unwrap_or_else(|| data_root.to_path_buf())
        } else {
            data_root.to_path_buf()
        };

        Some(Self {
            exe,
            data_root,
            goz,
        })
    }

    /// 同步跑一次快照对比，返回原始 JSON（仅供 `--diff` 自检使用）。
    pub fn run_diff(&self, old: &str, new: &str, limit: usize) -> Option<String> {
        let limit_text = limit.to_string();
        run_json(
            &self.exe,
            &self.data_root,
            self.goz.as_deref(),
            &["diff", "--old", old, "--new", new, "--limit", &limit_text],
        )
    }

    /// 在后台线程里跑 scan → snap；进度通过 channel 回传，UI 不阻塞。
    pub fn spawn_rebuild(&self, tx: Sender<BuildEvent>, repaint: impl Fn() + Send + 'static) {
        let exe = self.exe.clone();
        let root = self.data_root.clone();
        let goz = self.goz.clone();
        std::thread::spawn(move || {
            let started = Instant::now();
            let _ = tx.send(BuildEvent::Stage("正在扫描全盘（goz 全量导出）…".into()));
            repaint();

            let Some(scan) = run_json(&exe, &root, goz.as_deref(), &["scan"]) else {
                let _ = tx.send(BuildEvent::Finished(Err("scan 启动失败或输出不可解析".into())));
                repaint();
                return;
            };
            if !scan.contains("\"ok\": true") {
                let _ = tx.send(BuildEvent::Finished(Err(format!(
                    "scan 失败：{}",
                    first_error(&scan)
                ))));
                repaint();
                return;
            }
            let efu_mb = extract_number(&scan, "bytes")
                .map(|b| b as f64 / (1024.0 * 1024.0))
                .unwrap_or(0.0);
            let _ = tx.send(BuildEvent::Stage(format!(
                "扫描完成（{efu_mb:.0} MB），正在建立索引库…"
            )));
            repaint();

            let note = format!("Rust 界面重建 {}", now_stamp());
            let Some(snap) = run_json(&exe, &root, goz.as_deref(), &["snap", "--note", &note]) else {
                let _ = tx.send(BuildEvent::Finished(Err("snap 启动失败或输出不可解析".into())));
                repaint();
                return;
            };
            if !snap.contains("\"ok\": true") {
                let _ = tx.send(BuildEvent::Finished(Err(format!(
                    "snap 失败：{}",
                    first_error(&snap)
                ))));
                repaint();
                return;
            }
            let id = extract_string(&snap, "snapshot_id").unwrap_or_else(|| "-".into());
            let seconds = started.elapsed().as_secs_f64();
            let _ = tx.send(BuildEvent::Finished(Ok(format!(
                "快照 {id} 建立完成，用时 {seconds:.0} s"
            ))));
            repaint();
        });
    }
}

fn run_json(
    exe: &Path,
    root: &Path,
    goz: Option<&Path>,
    args: &[&str],
) -> Option<String> {
    let mut command = Command::new(exe);
    command
        .arg("--json")
        .arg("--data-root")
        .arg(root)
        .args(args)
        .stdout(Stdio::piped())
        .stderr(Stdio::null());
    if let Some(goz) = goz {
        // 随包分发的 goz 放在子目录时，通过环境变量告知建库器。
        command.env("DISKINTEL_GOZ", goz);
    }
    let output = command.output().ok()?;
    let mut text = String::new();
    output.stdout.as_slice().read_to_string(&mut text).ok()?;
    Some(text)
}

fn extract_string(text: &str, key: &str) -> Option<String> {
    let needle = format!("\"{key}\": \"");
    let start = text.find(&needle)? + needle.len();
    let rest = &text[start..];
    let end = rest.find('"')?;
    Some(rest[..end].to_owned())
}

fn extract_number(text: &str, key: &str) -> Option<u64> {
    let needle = format!("\"{key}\": ");
    let start = text.find(&needle)? + needle.len();
    let rest = &text[start..];
    let end = rest
        .find(|c: char| !c.is_ascii_digit())
        .unwrap_or(rest.len());
    rest[..end].parse().ok()
}

fn first_error(text: &str) -> String {
    extract_string(text, "message").unwrap_or_else(|| text.chars().take(160).collect())
}

fn now_stamp() -> String {
    chrono::Local::now().format("%Y-%m-%d %H:%M").to_string()
}

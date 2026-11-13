//! 磁盘情报系统 · 原生窗口。
//!
//! 纯 GPU 直绘（egui/glow），单进程、无 WebView、无 JS 运行时；
//! 数据侧只读 SQLite 快照（v3 全量 / v4 增量链，后者见 data 层）。

#![windows_subsystem = "windows"]

mod app;
mod backend;
mod db;
mod engine;
mod filter;
mod icons;
mod json_cli;
mod history;
mod lowvalue;
mod mcp;
mod settings;
mod snapshot_policy;
mod store;
mod tray;
mod update;
mod theme;
mod views;
mod widgets;

use std::path::PathBuf;
use std::time::Instant;

/// 命令行参数。GUI 之外的所有能力（截图自检 / 探针 / 对比）都从这里分流，
/// 保证「人能点、AI 能调、CI 能验」是同一条实现路径。
#[derive(Clone, Default)]
pub struct Cli {
    pub data_root: PathBuf,
    pub snapshot: Option<String>,
    pub shot: Option<PathBuf>,
    pub frames: u32,
    pub view: Option<String>,
    pub query: Option<String>,
    pub size: Option<(f32, f32)>,
    pub theme: Option<String>,
    pub source: Option<String>,
    pub diff_old: Option<String>,
    pub convert_v4: Option<(PathBuf, PathBuf)>,
    pub diff_new: Option<String>,
    pub probe: bool,
    pub bench: bool,
    pub diff: Option<(String, String)>,
}

fn parse_args() -> Cli {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let mut cli = Cli {
        data_root: std::env::var("DISKINTEL_DATA_ROOT")
            .map(PathBuf::from)
            .unwrap_or_else(|_| PathBuf::from(r"D:\disk-intel-data")),
        frames: 26,
        ..Default::default()
    };
    let mut index = 0;
    while index < args.len() {
        let arg = args[index].as_str();
        let mut next = || {
            index += 1;
            args.get(index).cloned().unwrap_or_default()
        };
        match arg {
            "--data-root" => cli.data_root = PathBuf::from(next()),
            "--snapshot" => cli.snapshot = Some(next()),
            "--shot" => cli.shot = Some(PathBuf::from(next())),
            "--frames" => cli.frames = next().parse().unwrap_or(26),
            "--view" => cli.view = Some(next()),
            "--query" | "-q" => cli.query = Some(next()),
            "--size" => {
                let raw = next();
                if let Some((w, h)) = raw.split_once('x') {
                    if let (Ok(w), Ok(h)) = (w.parse(), h.parse()) {
                        cli.size = Some((w, h));
                    }
                }
            }
            "--theme" => cli.theme = Some(next()),
            "--source" => cli.source = Some(next()),
            "--convert-v4" => {
                let source = next();
                let target = next();
                cli.convert_v4 = Some((PathBuf::from(source), PathBuf::from(target)));
            }
            "--diff-old" => cli.diff_old = Some(next()),
            "--diff-new" => cli.diff_new = Some(next()),
            "--probe" => cli.probe = true,
            "--bench" => cli.bench = true,
            "--diff" => {
                let old = next();
                let new = next();
                cli.diff = Some((old, new));
            }
            _ if arg.starts_with("--") => {}
            _ => cli.data_root = PathBuf::from(arg),
        }
        index += 1;
    }
    cli
}

fn main() -> eframe::Result<()> {
    // AI / 脚本通道：--mcp 与 --json 在建窗之前分流，进程不进 GUI。
    if let Some(code) = json_cli::run_if_requested() {
        std::process::exit(code);
    }
    if let Some(code) = mcp::run_if_requested() {
        std::process::exit(code);
    }
    // 检查更新（--check-update）：只读 GitHub Releases，不进 GUI。
    if let Some(code) = update::run_if_requested() {
        std::process::exit(code);
    }
    // 托盘自检（--tray-selftest）：建图标 → 立即移除，证据用于自动化验收。
    if std::env::args().any(|arg| arg == "--tray-selftest") {
        return match tray::selftest() {
            Ok(()) => {
                println!("TRAY SELFTEST OK");
                Ok(())
            }
            Err(error) => {
                println!("TRAY SELFTEST FAIL：{error}");
                std::process::exit(1);
            }
        };
    }

    let started = Instant::now();
    let cli = parse_args();
    // 设置里的自定义快照目录对「打开快照 / 探针 / 自检」立即生效。
    let initial_settings = settings::load();
    let snapshots_dir = initial_settings.effective_snapshots_dir(&cli.data_root);

    let argv: Vec<String> = std::env::args().skip(1).collect();
    if let Some(index) = argv.iter().position(|arg| arg == "--compact") {
        let args: Vec<&String> = argv.iter().skip(index + 1).collect();
        if args.len() < 3 {
            println!("用法: --compact <base.sqlite> <target.sqlite> <out.delta.sqlite>");
            return Ok(());
        }
        let (base, target, out) = (
            PathBuf::from(args[0]),
            PathBuf::from(args[1]),
            PathBuf::from(args[2]),
        );
        match store::compact::write_delta(&base, &target, &out) {
            Ok(stats) => println!(
                "delta 完成：{} → {}（{}）新增 {} / 消失 {} / 变化 {}，体积 {}（占源 {:.4}%），耗时 {:.2} s，输出 {}",
                stats.base_id,
                stats.target_id,
                stats.strategy.label(),
                db::thousands(stats.added),
                db::thousands(stats.removed),
                db::thousands(stats.changed),
                db::human_size(stats.delta_bytes as i64),
                if stats.base_bytes + stats.target_bytes > 0 {
                    stats.delta_bytes as f64 * 100.0 / (stats.base_bytes + stats.target_bytes) as f64
                } else {
                    0.0
                },
                stats.elapsed_ms / 1000.0,
                out.display()
            ),
            Err(error) => {
                println!("压缩失败：{error}");
                std::process::exit(1);
            }
        }
        return Ok(());
    }

    if let Some(index) = argv.iter().position(|arg| arg == "--materialize") {
        let args: Vec<&String> = argv.iter().skip(index + 1).collect();
        if args.len() < 3 {
            println!("用法: --materialize <base.sqlite> <delta.sqlite> [更多 delta…] <out.sqlite>");
            return Ok(());
        }
        let base = PathBuf::from(args[0]);
        let out = PathBuf::from(args[args.len() - 1]);
        let deltas: Vec<PathBuf> = args[1..args.len() - 1].iter().map(|p| PathBuf::from(p)).collect();
        match store::compact::materialize(&base, &deltas, &out) {
            Ok(stats) => println!(
                "物化完成：base {} + {} 份 delta → {}（{} 行 / {} 目录，total_size {}，完整性 {}，耗时 {:.1} s，输出 {}）",
                stats.base_id,
                deltas.len(),
                stats.target_id,
                db::thousands(stats.rows),
                db::thousands(stats.dir_count),
                db::human_size(stats.total_size),
                stats.integrity,
                (stats.copy_ms + stats.apply_ms + stats.folder_ms) / 1000.0,
                out.display()
            ),
            Err(error) => {
                println!("物化失败：{error}");
                std::process::exit(1);
            }
        }
        return Ok(());
    }

    if let Some((source, target)) = cli.convert_v4.clone() {
        println!("v3 → v4 转换：{}", source.display());
        let started = std::time::Instant::now();
        match store::v4::convert_v3_to_v4(&source, &target) {
            Ok(stats) => {
                println!(
                    "完成 {:.2} s｜v3 {:.1} MB → v4 {:.1} MB（{:.2}×，省 {:.1}%）",
                    stats.seconds,
                    stats.v3_bytes as f64 / 1_048_576.0,
                    stats.out_bytes as f64 / 1_048_576.0,
                    stats.ratio,
                    (1.0 - stats.out_bytes as f64 / stats.v3_bytes.max(1) as f64) * 100.0
                );
                println!(
                    "行数 {}｜目录 {}｜名字 {}｜total_size {}｜校验 {}",
                    stats.rows, stats.dirs, stats.names, stats.total_size, stats.verified
                );
                for note in &stats.notes {
                    println!("  · {note}");
                }
                println!("输出：{}（耗时 {:.1} s）", target.display(), started.elapsed().as_secs_f64());
            }
            Err(message) => println!("转换失败：{message}"),
        }
        return Ok(());
    }

    if std::env::args().any(|arg| arg == "--engine-probe") {
        engine::print_probe();
        return Ok(());
    }

    if let Some(path) = std::env::args().position(|a| a == "--write-test").and_then(|i| std::env::args().nth(i + 1)) {
        match std::fs::write(&path, b"write-test-ok") {
            Ok(()) => println!("WRITE OK {path}"),
            Err(error) => println!("WRITE FAIL {path}: {error}"),
        }
        if let Ok(dir) = std::fs::read_dir(std::env::temp_dir()) {
            println!("TEMP readable, {} entries", dir.count());
        }
        return Ok(());
    }

    if cli.probe {
        println!("数据根: {}", cli.data_root.display());
        println!("快照目录: {}", snapshots_dir.display());
        match backend::Backend::locate(&snapshots_dir) {
            Some(found) => {
                println!("建库器: {}", found.exe.display());
                match &found.goz {
                    Some(goz) => println!("goz: {}", goz.display()),
                    None => println!("goz: 未找到（建库时回落到 PATH / 内置默认）"),
                }
            }
            None => println!("建库器: 未找到（重建索引不可用，浏览与检索不受影响）"),
        }
        let files = db::list_snapshots(&snapshots_dir);
        println!("快照: {} 份", files.len());
        for file in files.iter().take(10) {
            println!(
                "  {}　{:.2} GB　{} 条",
                file.name,
                file.bytes as f64 / 1_073_741_824.0,
                db::thousands(file.entries)
            );
        }
        return Ok(());
    }

    if std::env::args().any(|arg| arg == "--engine-probe") {
        engine::print_probe();
        return Ok(());
    }

    if let Some((old, new)) = cli.diff.clone() {
        let Some(found) = backend::Backend::locate(&snapshots_dir) else {
            println!("未找到建库器，无法执行对比");
            return Ok(());
        };
        let timer = Instant::now();
        match found.run_diff(&old, &new, 5) {
            Some(raw) => {
                println!("对比 {old} → {new}　耗时 {:.1} s", timer.elapsed().as_secs_f64());
                println!("{}", raw.chars().take(900).collect::<String>());
            }
            None => println!("对比调用失败"),
        }
        return Ok(());
    }

    eprintln!("[t] 参数解析完成 +{:.0}ms", started.elapsed().as_secs_f64() * 1000.0);
    // 上次建库留下的 v4 转换产物在打开快照前落位：此时旧 <快照名>.sqlite 还没被任何连接打开，
    // Windows 才允许 rename 覆盖（打开之后就只能等下次启动）。
    let finalized = app::finalize_pending_v4(&snapshots_dir);
    if finalized > 0 {
        eprintln!("[v4] 启动前落位 {finalized} 份 v4 快照");
    }
    let snapshot = match cli.snapshot.clone() {
        Some(id) => {
            let path = snapshots_dir.join(format!("{id}.sqlite"));
            db::Snapshot::open_path(&path)
        }
        None => db::Snapshot::open_latest(&snapshots_dir),
    };
    let snapshot = match snapshot {
        Ok(snapshot) => snapshot,
        Err(message) => {
            eprintln!("{message}");
            // 无快照时仍然打开窗口，让用户看到空态与扫描入口，而不是双击无反应。
            db::Snapshot::open_empty().unwrap_or_else(|_| std::process::exit(2))
        }
    };
    eprintln!("[t] 快照打开完成 +{:.0}ms", started.elapsed().as_secs_f64() * 1000.0);
    eprintln!(
        "快照 {}　条目 {}　打开 {:.1} ms",
        snapshot.name, db::thousands(snapshot.total_entries), snapshot.open_ms
    );

    let (width, height) = cli.size.unwrap_or((1360.0, 860.0));
    let mut viewport = eframe::egui::ViewportBuilder::default()
        .with_inner_size([width, height])
        .with_min_inner_size([900.0, 520.0])
        .with_title("磁盘情报系统");
    if let Ok(icon) = eframe::icon_data::from_png_bytes(include_bytes!("../assets/icon.png")) {
        viewport = viewport.with_icon(std::sync::Arc::new(icon));
    }
    let options = eframe::NativeOptions {
        viewport,
        ..Default::default()
    };

    eprintln!("[t] 进入 run_native +{:.0}ms", started.elapsed().as_secs_f64() * 1000.0);
    eframe::run_native(
        "磁盘情报系统",
        options,
        Box::new(move |cc| Ok(Box::new(app::App::new(cc, cli, snapshot, started)))),
    )
}
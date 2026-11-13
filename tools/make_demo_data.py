"""生成演示用快照数据（v3 schema），用于公开截图与演示，不包含任何真实文件信息。"""
import sqlite3, random, time, os, sys, json

OUT = sys.argv[1] if len(sys.argv) > 1 else r"D:\diskintel-demo"
random.seed(20260101)

def ensure(d):
    os.makedirs(d, exist_ok=True)

def build_tree():
    """返回 [(path, parent, size, mtime, is_dir)]"""
    rows = []
    now = 1767225600  # 2026-01-01

    def add(path, parent, size, is_dir, mtime=None):
        rows.append((path, parent, size, mtime or (now + random.randint(-86400 * 300, 0)), is_dir))

    roots = ["C:\\", "D:\\", "E:\\", "F:\\"]
    for r in roots:
        add(r, "", 0, 1)

    def dir_tree(parent, name, size, is_dir=1):
        path = parent + name if parent.endswith("\\") else parent + "\\" + name
        add(path, parent, size, is_dir)
        return path

    # C:\Users\demo
    users = dir_tree("C:\\", "Users", 0)
    demo = dir_tree(users, "demo", 0)
    desktop = dir_tree(demo, "Desktop", 0)
    docs = dir_tree(demo, "Documents", 0)
    reports = dir_tree(docs, "报告", 0)
    downloads = dir_tree(demo, "Downloads", 0)
    pictures = dir_tree(demo, "Pictures", 0)
    appdata = dir_tree(demo, "AppData", 0)
    local = dir_tree(appdata, "Local", 0)
    temp = dir_tree(local, "Temp", 0)
    cache = dir_tree(local, "Cache", 0)

    files = [
        (desktop, "项目计划.docx", 182_000), (desktop, "会议纪要.md", 12_400),
        (desktop, "截图-2026.png", 640_000), (desktop, "预算表.xlsx", 96_000),
        (desktop, "演示文稿.pptx", 1_480_000), (desktop, "~/报告.docx", 20_480),
        (reports, "季度报告-2026Q1.docx", 512_000), (reports, "总结.md", 8_200),
        (reports, "数据导出.csv", 2_400_000), (reports, "图纸.pdf", 3_100_000),
        (downloads, "安装包.exe", 24_000_000), (downloads, "setup.msi", 18_000_000),
        (downloads, "演示视频.mp4", 88_000_000), (downloads, "素材.zip", 12_000_000),
        (pictures, "IMG_0001.jpg", 2_100_000), (pictures, "IMG_0002.jpg", 1_900_000),
        (pictures, "封面.png", 780_000),
        (temp, "~$报告.docx", 162), (temp, "安装日志.log", 240_000),
        (temp, "a.tmp", 4_096), (temp, "b.tmp", 8_192),
        (cache, "缓存块.dat", 32_000_000), (cache, "index.bin", 5_000_000),
        (cache, "数据.db", 900_000),
    ]
    for parent, name, size in files:
        add(parent + ("" if parent.endswith("\\") else "\\") + name, parent, size, 0)

    # C:\Windows 系统目录（演示折叠：dll/mui/sys/dat 等）
    windows = dir_tree("C:\\", "Windows", 0)
    system32 = dir_tree(windows, "System32", 0)
    winsxs = dir_tree(windows, "WinSxS", 0)
    for i in range(260):
        ext = [".dll", ".mui", ".sys", ".dat", ".etl", ".pdb", ".cat", ".manifest"][i % 8]
        name = f"system-file-{i:03d}{ext}"
        add(system32 + "\\" + name, system32, random.randint(4_000, 900_000), 0)
    for i in range(120):
        add(winsxs + f"\\amd64_demo_{i:03d}.dll", winsxs, random.randint(4_000, 400_000), 0)

    # C:\Program Files
    pf = dir_tree("C:\\", "Program Files", 0)
    app = dir_tree(pf, "DemoApp", 0)
    appcache = dir_tree(app, "cache", 0)
    for name, size in [("DemoApp.exe", 12_000_000), ("DemoApp.dll", 1_200_000), ("readme.txt", 3_000)]:
        add(app + "\\" + name, app, size, 0)
    for i in range(30):
        add(appcache + f"\\chunk-{i:02d}.cache", appcache, random.randint(1_000, 2_000_000), 0)

    # D:\Projects
    projects = dir_tree("D:\\", "Projects", 0)
    repo = dir_tree(projects, "diskintel", 0)
    src = dir_tree(repo, "src", 0)
    for name, size in [("main.rs", 12_000), ("app.rs", 68_000), ("search.rs", 84_000),
                       ("lowvalue.rs", 42_000), ("db.rs", 20_000)]:
        add(src + "\\" + name, src, size, 0)
    add(repo + "\\Cargo.toml", repo, 1_800, 0)
    add(repo + "\\README.md", repo, 12_000, 0)
    add(repo + "\\架构图.png", repo, 420_000, 0)
    build = dir_tree(repo, "build", 0)
    add(build + "\\output.pdb", build, 2_400_000, 0)
    add(build + "\\report.d", build, 8_000, 0)
    node_modules = dir_tree(repo, "node_modules", 0)
    pkg = dir_tree(node_modules, "demo-package", 0)
    for i in range(40):
        add(pkg + f"\\file-{i:02d}.js", pkg, random.randint(2_000, 90_000), 0)
    docs_d = dir_tree(projects, "docs", 0)
    add(docs_d + "\\设计说明.md", docs_d, 22_000, 0)

    # E:\Media
    media = dir_tree("E:\\", "Media", 0)
    photos = dir_tree(media, "Photos", 0)
    videos = dir_tree(media, "Videos", 0)
    for i in range(60):
        add(photos + f"\\IMG_{i:04d}.jpg", photos, random.randint(800_000, 4_000_000), 0)
    for i in range(20):
        add(videos + f"\\clip-{i:02d}.mp4", videos, random.randint(20_000_000, 200_000_000), 0)

    # F:\Games（演示大体量）
    games = dir_tree("F:\\", "Games", 0)
    g1 = dir_tree(games, "DemoGame", 0)
    for i in range(50):
        add(g1 + f"\\pak-{i:02d}.pak", g1, random.randint(50_000_000, 900_000_000), 0)

    # 冷门扩展名样本（演示「冷门」折叠）
    misc = dir_tree(demo, "Misc", 0)
    rare = [".d", ".ilk", ".obj", ".pch", ".lib", ".exp", ".map", ".cdf-ms", ".mum", ".pnf", ".edb", ".evtx"]
    for i, ext in enumerate(rare):
        add(misc + f"\\artifact-{i:02d}{ext}", misc, random.randint(1_000, 300_000), 0)
    return rows

def folder_sizes(rows):
    dirs = {}
    for path, parent, size, mtime, is_dir in rows:
        if is_dir:
            dirs.setdefault(path, [0, 0])
    for path, parent, size, mtime, is_dir in rows:
        if is_dir:
            continue
        p = parent
        while p:
            if p in dirs:
                dirs[p][0] += size
                dirs[p][1] += 1
            parent_of = next((r[1] for r in rows if r[0] == p), "")
            p = parent_of
    out = []
    for path, (size, count) in dirs.items():
        parent = next(r[1] for r in rows if r[0] == path)
        out.append((path, parent, size, count))
    return out

def make_snapshot(path, rows, note):
    if os.path.exists(path):
        os.remove(path)
    conn = sqlite3.connect(path)
    conn.execute("PRAGMA journal_mode=DELETE")
    conn.execute("CREATE TABLE entries (path TEXT PRIMARY KEY, parent_path TEXT, size INTEGER, mtime INTEGER, is_dir INTEGER, attributes INTEGER, ext TEXT) WITHOUT ROWID")
    conn.execute("CREATE TABLE folder_sizes (path TEXT PRIMARY KEY, parent_path TEXT, size INTEGER, file_count INTEGER) WITHOUT ROWID")
    conn.execute("CREATE TABLE meta (key TEXT PRIMARY KEY, value TEXT)")
    conn.executemany("INSERT INTO entries VALUES (?,?,?,?,?,?,?)", [
        (p, par, s, m, d, 16 if d else 0, (os.path.splitext(p)[1][1:].lower() if not d else ""))
        for p, par, s, m, d in rows
    ])
    fs = folder_sizes([(p, par, s, m, d) for p, par, s, m, d in rows])
    conn.executemany("INSERT INTO folder_sizes VALUES (?,?,?,?)", fs)
    files = sum(1 for r in rows if not r[4]); dirs = sum(1 for r in rows if r[4])
    total = sum(r[2] for r in rows if not r[4])
    meta = {
        "format_version": "3", "file_count": str(files), "dir_count": str(dirs),
        "total_size": str(total), "created_at": str(int(time.time())),
        "note": note, "source_efu": "demo", "layout": "v3",
    }
    conn.executemany("INSERT INTO meta VALUES (?,?)", list(meta.items()))
    conn.commit(); conn.close()
    return files, dirs, total

def main():
    snap_dir = os.path.join(OUT, "snapshots")
    ensure(snap_dir)
    base = build_tree()
    f1, d1, t1 = make_snapshot(os.path.join(snap_dir, "20260101-000000.sqlite"), base, "演示数据 A")
    # 第二份：制造差异（新增/消失/增长）
    rows2 = []
    for p, par, s, m, d in base:
        if p.endswith("演示视频.mp4"):
            continue  # 消失
        if p.endswith("季度报告-2026Q1.docx"):
            s = int(s * 1.6)  # 增长
        rows2.append((p, par, s, m, d))
    extra_dir = "C:\\Users\\demo\\Desktop\\新项目"
    rows2.append((extra_dir, "C:\\Users\\demo\\Desktop", 0, 1767225600 + 86400, 1))
    rows2.append((extra_dir + "\\方案.docx", extra_dir, 240_000, 1767225600 + 86400, 0))
    rows2.append((extra_dir + "\\数据.csv", extra_dir, 96_000, 1767225600 + 86400, 0))
    rows2.append(("C:\\Users\\demo\\Downloads\\大文件.iso", "C:\\Users\\demo\\Downloads", 4_600_000_000, 1767225600 + 86400, 0))
    f2, d2, t2 = make_snapshot(os.path.join(snap_dir, "20260102-000000.sqlite"), rows2, "演示数据 B")
    print(json.dumps({"root": OUT, "a": {"files": f1, "dirs": d1, "total": t1},
                      "b": {"files": f2, "dirs": d2, "total": t2}}, ensure_ascii=False, indent=2))

if __name__ == "__main__":
    main()

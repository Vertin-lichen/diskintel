//! 系统托盘：纯 Win32（Shell_NotifyIconW + 隐藏窗口 + 托盘线程消息循环），零第三方 crate。
//!
//! 为什么是「独立托盘线程 + 自己 DoMessage 循环」而不是主线程每帧 PeekMessage：
//! - egui/eframe 的主线程已由 winit 事件循环驱动；把 TrackPopupMenu（内部是模态消息循环）
//!   塞进 winit 的 DispatchMessage 重入路径，等于在主线程里再跑一层消息泵，容易踩 winit 状态；
//! - 托盘线程用 GetMessageW 阻塞等待，UI 线程永远不阻塞，符合「非阻塞」要求；
//! - Python 参考实现（磁盘情报系统/src/diskintel/tray.py）就是同一套隐藏窗口 + 托盘线程方案，
//!   原生版与之一一对应，行为更容易对齐。
//!
//! 主线程与托盘线程之间只通过线程安全队列 + egui::Context::request_repaint 交互：
//! 菜单/双击只入队并请求重绘，真正的业务（建快照/显示窗口/退出）由 App::logic 每帧 poll 处理。
//!
//! 图标：优先取主窗口当前图标（winit 用 WM_SETICON 设置的 assets/icon.png），
//! 取不到时回退系统 IDI_APPLICATION —— 项目没有 .ico 资源，这是最省事且不丢品牌的做法。

use std::ffi::c_void;
use std::ptr;
use std::sync::Mutex;
use std::sync::atomic::{AtomicBool, AtomicIsize, Ordering};
use std::sync::mpsc::{Receiver, channel};
use std::time::Duration;

/// 主窗口标题（main.rs 里 ViewportBuilder::with_title 同一个字符串）。
const MAIN_TITLE: &str = "磁盘情报系统";
/// 隐藏消息窗口的类名（自动化测试按它 FindWindowW）。
pub const CLASS_NAME: &str = "DiskIntelTrayWindow";
/// 托盘回调消息：Shell_NotifyIcon 把鼠标事件投递到隐藏窗口。
const WM_TRAY: u32 = 0x8001; // WM_APP + 1
/// 让托盘线程退出消息循环（App::on_exit 用）。
const WM_APP_QUIT: u32 = 0x8002; // WM_APP + 2
const WM_COMMAND: u32 = 0x0111;
const WM_DESTROY: u32 = 0x0002;
const WM_NULL: u32 = 0x0000;
const WM_LBUTTONDBLCLK: u32 = 0x0203;
const WM_RBUTTONUP: u32 = 0x0205;

const NIM_ADD: u32 = 0;
const NIM_MODIFY: u32 = 1;
const NIM_DELETE: u32 = 2;
const NIF_MESSAGE: u32 = 0x1;
const NIF_ICON: u32 = 0x2;
const NIF_TIP: u32 = 0x4;

const MF_STRING: u32 = 0;
const MF_SEPARATOR: u32 = 0x800;
const TPM_LEFTALIGN: u32 = 0x0;
const TPM_RIGHTBUTTON: u32 = 0x2;
const TPM_RETURNCMD: u32 = 0x100;

const WM_GETICON: u32 = 0x007F;
const ICON_SMALL: usize = 0;
const IDI_APPLICATION: usize = 32512;
const WS_EX_TOOLWINDOW: u32 = 0x80;
const WS_POPUP: u32 = 0x8000_0000;
const SW_SHOW: i32 = 5;
const SW_RESTORE: i32 = 9;
const BIF_RETURNONLYFSDIRS: u32 = 0x1;
const BIF_EDITBOX: u32 = 0x10;
const BIF_NEWDIALOGSTYLE: u32 = 0x40;
const REG_SZ: u32 = 1;
const KEY_WRITE: u32 = 0x0002_0006;
/// HKEY_CURRENT_USER = 0x80000001（x64 上按有符号指针等值为 -2147483647）。
const HKEY_CURRENT_USER: isize = -2_147_483_647;

/// 托盘菜单命令 ID（与 Python 版不同值域，纯内部约定）。
const CMD_SHOW: u32 = 1001;
const CMD_SNAPSHOT: u32 = 1002;
const CMD_UPDATE: u32 = 1003;
const CMD_QUIT: u32 = 1004;

/// 托盘菜单 / 双击产生的一次请求；由 App 在 logic() 里每帧取走执行。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TrayCommand {
    /// 显示主窗口（从托盘还原）。
    Show,
    /// 立即建快照。
    SnapshotNow,
    /// 打开「检查更新」窗口。
    CheckUpdate,
    /// 真退出（托盘菜单「退出」）。
    Quit,
}

type Hwnd = isize;
type Hicon = isize;
type Hmenu = isize;
type Hinstance = isize;
type Wparam = usize;
type Lparam = isize;
type Lresult = isize;

#[repr(C)]
struct NotifyIconDataW {
    cb_size: u32,
    hwnd: Hwnd,
    uid: u32,
    uflags: u32,
    callback_message: u32,
    hicon: Hicon,
    tip: [u16; 128],
    state: u32,
    state_mask: u32,
    info: [u16; 256],
    timeout: u32,
    info_title: [u16; 64],
    info_flags: u32,
    guid_item: [u8; 16],
    balloon_icon: Hicon,
}

#[repr(C)]
struct WndClassExW {
    cb_size: u32,
    style: u32,
    wnd_proc: Option<unsafe extern "system" fn(Hwnd, u32, Wparam, Lparam) -> Lresult>,
    cls_extra: i32,
    wnd_extra: i32,
    instance: Hinstance,
    icon: Hicon,
    cursor: isize,
    background: isize,
    menu_name: *const u16,
    class_name: *const u16,
    icon_small: Hicon,
}

#[repr(C)]
#[derive(Clone, Copy, Default)]
struct Point {
    x: i32,
    y: i32,
}

#[repr(C)]
struct Msg {
    hwnd: Hwnd,
    message: u32,
    wparam: Wparam,
    lparam: Lparam,
    time: u32,
    pt: Point,
    private: u32,
}

#[repr(C)]
struct BrowseInfoW {
    owner: Hwnd,
    root: *const c_void,
    display_name: *mut u16,
    title: *const u16,
    flags: u32,
    callback: Option<unsafe extern "system" fn(Hwnd, u32, Lparam, Lparam) -> i32>,
    lparam: Lparam,
    image: i32,
}

#[link(name = "user32")]
unsafe extern "system" {
    fn RegisterClassExW(class: *const WndClassExW) -> u16;
    fn UnregisterClassW(class_name: *const u16, instance: Hinstance) -> i32;
    fn CreateWindowExW(
        ex_style: u32,
        class_name: *const u16,
        window_name: *const u16,
        style: u32,
        x: i32,
        y: i32,
        width: i32,
        height: i32,
        parent: Hwnd,
        menu: Hmenu,
        instance: Hinstance,
        param: *mut c_void,
    ) -> Hwnd;
    fn DestroyWindow(hwnd: Hwnd) -> i32;
    fn DefWindowProcW(hwnd: Hwnd, msg: u32, wparam: Wparam, lparam: Lparam) -> Lresult;
    fn GetMessageW(msg: *mut Msg, hwnd: Hwnd, filter_min: u32, filter_max: u32) -> i32;
    fn TranslateMessage(msg: *const Msg) -> i32;
    fn DispatchMessageW(msg: *const Msg) -> Lresult;
    fn PostQuitMessage(exit_code: i32);
    fn PostMessageW(hwnd: Hwnd, msg: u32, wparam: Wparam, lparam: Lparam) -> i32;
    fn SendMessageW(hwnd: Hwnd, msg: u32, wparam: Wparam, lparam: Lparam) -> Lresult;
    fn CreatePopupMenu() -> Hmenu;
    fn AppendMenuW(menu: Hmenu, flags: u32, id: usize, text: *const u16) -> i32;
    fn DestroyMenu(menu: Hmenu) -> i32;
    fn TrackPopupMenu(
        menu: Hmenu,
        flags: u32,
        x: i32,
        y: i32,
        reserved: i32,
        owner: Hwnd,
        rect: *const c_void,
    ) -> i32;
    fn GetCursorPos(point: *mut Point) -> i32;
    fn SetForegroundWindow(hwnd: Hwnd) -> i32;
    fn ShowWindow(hwnd: Hwnd, cmd: i32) -> i32;
    fn IsIconic(hwnd: Hwnd) -> i32;
    fn IsWindowVisible(hwnd: Hwnd) -> i32;
    fn GetWindowTextW(hwnd: Hwnd, buffer: *mut u16, max: i32) -> i32;
    fn GetWindowThreadProcessId(hwnd: Hwnd, pid: *mut u32) -> u32;
    fn EnumWindows(
        callback: Option<unsafe extern "system" fn(Hwnd, Lparam) -> i32>,
        lparam: Lparam,
    ) -> i32;
    fn LoadIconW(instance: Hinstance, name: *const u16) -> Hicon;
}

#[link(name = "shell32")]
unsafe extern "system" {
    fn Shell_NotifyIconW(message: u32, data: *const NotifyIconDataW) -> i32;
    fn SHBrowseForFolderW(info: *mut BrowseInfoW) -> *mut c_void;
    fn SHGetPathFromIDListW(pidl: *const c_void, path: *mut u16) -> i32;
}

#[link(name = "kernel32")]
unsafe extern "system" {
    fn GetModuleHandleW(module: *const u16) -> Hinstance;
    fn GetCurrentProcessId() -> u32;
    fn GetLastError() -> u32;
}

#[link(name = "ole32")]
unsafe extern "system" {
    fn CoTaskMemFree(memory: *const c_void);
}

#[link(name = "advapi32")]
unsafe extern "system" {
    fn RegCreateKeyExW(
        key: isize,
        sub_key: *const u16,
        reserved: u32,
        class: *const u16,
        options: u32,
        access: u32,
        security: *const c_void,
        out_key: *mut isize,
        disposition: *mut u32,
    ) -> i32;
    fn RegSetValueExW(
        key: isize,
        value_name: *const u16,
        reserved: u32,
        value_type: u32,
        data: *const u8,
        data_size: u32,
    ) -> i32;
    fn RegCloseKey(key: isize) -> i32;
}

// ---------- 全局状态（托盘线程 ↔ 主线程） ----------

/// 托盘线程当前隐藏窗口句柄（0 = 未创建）。
static HIDDEN_HWND: AtomicIsize = AtomicIsize::new(0);
/// 主窗口句柄（0 = 还没找到）。
static MAIN_HWND: AtomicIsize = AtomicIsize::new(0);
static READY: AtomicBool = AtomicBool::new(false);
static THREAD: Mutex<Option<std::thread::JoinHandle<()>>> = Mutex::new(None);
static COMMANDS: Mutex<Vec<TrayCommand>> = Mutex::new(Vec::new());
static REPAINT: Mutex<Option<eframe::egui::Context>> = Mutex::new(None);
static LAST_ERROR: Mutex<String> = Mutex::new(String::new());

/// 启动托盘（幂等：已在跑就直接返回 true）。
///
/// 传 None 用于纯 Win32 自检（不依赖 egui）；正常路径传 egui 上下文，
/// 托盘线程会用它请求重绘，让 App::logic 尽快取走命令。
pub fn start(ctx: Option<eframe::egui::Context>) -> bool {
    if READY.load(Ordering::SeqCst) {
        return true;
    }
    *REPAINT.lock().unwrap() = ctx;
    set_last_error("");
    let (tx, rx): (_, Receiver<Result<(), String>>) = channel();
    match std::thread::Builder::new()
        .name("diskintel-tray".to_owned())
        .spawn(move || run_tray_thread(tx))
    {
        Ok(handle) => {
            *THREAD.lock().unwrap() = Some(handle);
        }
        Err(error) => {
            let message = format!("托盘线程创建失败：{error}");
            set_last_error(&message);
            eprintln!("[tray] {message}");
            return false;
        }
    }
    match rx.recv_timeout(Duration::from_secs(3)) {
        Ok(Ok(())) => {
            READY.store(true, Ordering::SeqCst);
            true
        }
        Ok(Err(error)) => {
            set_last_error(&error);
            eprintln!("[tray] 初始化失败：{error}");
            false
        }
        Err(_) => {
            let message = "托盘线程 3 秒内未就绪".to_owned();
            set_last_error(&message);
            eprintln!("[tray] {message}");
            false
        }
    }
}

/// 取走托盘线程攒下的命令（App::logic 每帧调用，非阻塞）。
pub fn poll() -> Vec<TrayCommand> {
    let mut guard = COMMANDS.lock().unwrap();
    std::mem::take(&mut *guard)
}

/// 托盘是否已就绪（图标存在）。
pub fn is_ready() -> bool {
    READY.load(Ordering::SeqCst)
}

/// 最近一次失败原因（没失败过就是空串）。
pub fn last_error() -> String {
    LAST_ERROR.lock().unwrap().clone()
}

/// 结束托盘：移除图标、销毁隐藏窗口、让托盘线程退出并 join。
///
/// 幂等；App::on_exit 与真退出路径都会调用。
pub fn shutdown() {
    let handle = THREAD.lock().unwrap().take();
    let hwnd = HIDDEN_HWND.load(Ordering::SeqCst);
    if hwnd != 0 {
        unsafe {
            PostMessageW(hwnd, WM_APP_QUIT, 0, 0);
        }
    }
    if let Some(handle) = handle {
        let _ = handle.join();
    }
    READY.store(false, Ordering::SeqCst);
}

/// 纯 Win32 自检：建图标 → 立即移除。给自动化 / CI 用，不驻留。
pub fn selftest() -> Result<(), String> {
    if !start(None) {
        return Err(last_error());
    }
    let result = if is_ready() {
        Ok(())
    } else {
        Err(last_error())
    };
    shutdown();
    result
}

/// 把快照目录写进 HKCU\Software\DiskIntel\SnapshotDir（供卸载器读取）。
/// 失败只返回错误文本，调用方记日志即可，不影响运行。
pub fn write_snapshot_dir_registry(dir: &str) -> Result<(), String> {
    let sub_key = wide(r"Software\DiskIntel");
    let value_name = wide("SnapshotDir");
    let data = wide(dir);
    unsafe {
        let mut key: isize = 0;
        let mut disposition: u32 = 0;
        let status = RegCreateKeyExW(
            HKEY_CURRENT_USER,
            sub_key.as_ptr(),
            0,
            ptr::null(),
            0,
            KEY_WRITE,
            ptr::null(),
            &mut key,
            &mut disposition,
        );
        if status != 0 {
            return Err(format!("RegCreateKeyExW 返回 {status}"));
        }
        let bytes = data.len() * std::mem::size_of::<u16>();
        let status = RegSetValueExW(
            key,
            value_name.as_ptr(),
            0,
            REG_SZ,
            data.as_ptr() as *const u8,
            bytes as u32,
        );
        RegCloseKey(key);
        if status != 0 {
            return Err(format!("RegSetValueExW 返回 {status}"));
        }
    }
    eprintln!("[tray] 已写入 HKCU\\Software\\DiskIntel\\SnapshotDir = {dir}");
    Ok(())
}

/// 系统文件夹选择框（SHBrowseForFolderW，纯 Win32、无 COM）。
pub fn pick_folder(title: &str) -> Option<String> {
    let title_w = wide(title);
    let mut display = [0u16; 260];
    let mut info = BrowseInfoW {
        owner: main_window(),
        root: ptr::null(),
        display_name: display.as_mut_ptr(),
        title: title_w.as_ptr(),
        flags: BIF_RETURNONLYFSDIRS | BIF_EDITBOX | BIF_NEWDIALOGSTYLE,
        callback: None,
        lparam: 0,
        image: 0,
    };
    unsafe {
        let pidl = SHBrowseForFolderW(&mut info);
        if pidl.is_null() {
            return None;
        }
        let mut buffer = [0u16; 32_768];
        let ok = SHGetPathFromIDListW(pidl, buffer.as_mut_ptr());
        CoTaskMemFree(pidl);
        if ok == 0 {
            None
        } else {
            Some(from_wide(&buffer))
        }
    }
}

// ---------- 托盘线程 ----------

fn run_tray_thread(tx: std::sync::mpsc::Sender<Result<(), String>>) {
    unsafe {
        let instance = GetModuleHandleW(ptr::null());
        let class_name = wide(CLASS_NAME);
        let title = wide("diskintel tray");
        let mut class: WndClassExW = std::mem::zeroed();
        class.cb_size = std::mem::size_of::<WndClassExW>() as u32;
        class.wnd_proc = Some(wnd_proc);
        class.instance = instance;
        class.class_name = class_name.as_ptr();
        if RegisterClassExW(&class) == 0 {
            let code = GetLastError();
            // 1410 = ERROR_CLASS_ALREADY_EXISTS（重复初始化 / 自检后再次启动）
            if code != 1410 {
                let message = format!("RegisterClassExW 失败：{code}");
                let _ = tx.send(Err(message));
                return;
            }
        }
        let hwnd = CreateWindowExW(
            WS_EX_TOOLWINDOW,
            class_name.as_ptr(),
            title.as_ptr(),
            WS_POPUP,
            0,
            0,
            0,
            0,
            0,
            0,
            instance,
            ptr::null_mut(),
        );
        if hwnd == 0 {
            let message = format!("CreateWindowExW 失败：{}", GetLastError());
            let _ = tx.send(Err(message));
            return;
        }
        HIDDEN_HWND.store(hwnd, Ordering::SeqCst);

        // 先用系统默认图标建出托盘（不依赖 UI 线程，避免 App::new 等就绪时互相等待）；
        // 就绪后再把图标换成主窗口的品牌图标（assets/icon.png 转出来的 HICON）。
        // LoadIcon 返回共享图标，不能 DestroyIcon。
        let fallback = LoadIconW(0, IDI_APPLICATION as *const u16);
        let mut data: NotifyIconDataW = std::mem::zeroed();
        data.cb_size = std::mem::size_of::<NotifyIconDataW>() as u32;
        data.hwnd = hwnd;
        data.uid = 1;
        data.uflags = NIF_MESSAGE | NIF_ICON | NIF_TIP;
        data.callback_message = WM_TRAY;
        data.hicon = fallback;
        copy_wide("磁盘情报系统 · 双击显示主窗口", &mut data.tip);
        let added = Shell_NotifyIconW(NIM_ADD, &data);
        if added == 0 {
            let message = format!(
                "Shell_NotifyIconW(NIM_ADD) 失败：{}（资源管理器未运行？）",
                GetLastError()
            );
            DestroyWindow(hwnd);
            UnregisterClassW(class_name.as_ptr(), instance);
            HIDDEN_HWND.store(0, Ordering::SeqCst);
            let _ = tx.send(Err(message));
            return;
        }
        eprintln!(
            "[tray] 托盘图标已创建：Shell_NotifyIconW={added} hwnd=0x{hwnd:x} icon=0x{fallback:x}"
        );
        // 先让 App::new 拿到就绪信号，再问主窗口要品牌图标（SendMessage 会等 UI 线程处理）。
        let _ = tx.send(Ok(()));
        let main = find_main_window();
        MAIN_HWND.store(main, Ordering::SeqCst);
        if main != 0 {
            let mut branded = SendMessageW(main, WM_GETICON, ICON_SMALL, 0) as Hicon;
            if branded == 0 {
                branded = SendMessageW(main, WM_GETICON, 1, 0) as Hicon; // ICON_BIG
            }
            if branded != 0 {
                let mut update: NotifyIconDataW = std::mem::zeroed();
                update.cb_size = std::mem::size_of::<NotifyIconDataW>() as u32;
                update.hwnd = hwnd;
                update.uid = 1;
                update.uflags = NIF_ICON;
                update.hicon = branded;
                if Shell_NotifyIconW(NIM_MODIFY, &update) != 0 {
                    eprintln!("[tray] 已换用主窗口图标 hicon=0x{branded:x}（main=0x{main:x}）");
                }
            }
        }

        // 标准 Win32 消息循环：只在托盘线程里阻塞，UI 线程不受影响。
        let mut msg: Msg = std::mem::zeroed();
        while GetMessageW(&mut msg, 0, 0, 0) > 0 {
            TranslateMessage(&msg);
            DispatchMessageW(&msg);
        }
        // 清理：图标 → 隐藏窗口 → 窗口类。
        Shell_NotifyIconW(NIM_DELETE, &data);
        DestroyWindow(hwnd);
        UnregisterClassW(class_name.as_ptr(), instance);
        HIDDEN_HWND.store(0, Ordering::SeqCst);
        READY.store(false, Ordering::SeqCst);
        eprintln!("[tray] 托盘已移除，消息循环结束");
    }
}

unsafe extern "system" fn wnd_proc(hwnd: Hwnd, msg: u32, wparam: Wparam, lparam: Lparam) -> Lresult {
    match msg {
        WM_TRAY => {
            let event = (lparam as usize & 0xFFFF) as u32;
            match event {
                WM_LBUTTONDBLCLK => dispatch_command(CMD_SHOW),
                WM_RBUTTONUP => unsafe { show_context_menu(hwnd) },
                _ => {}
            }
            0
        }
        // 自动化测试用：PostMessageW(hwnd, WM_COMMAND, 命令 ID, 0) 与点菜单等价。
        WM_COMMAND => {
            dispatch_command((wparam & 0xFFFF) as u32);
            0
        }
        WM_APP_QUIT => {
            unsafe { PostQuitMessage(0) };
            0
        }
        WM_DESTROY => {
            unsafe { PostQuitMessage(0) };
            0
        }
        _ => unsafe { DefWindowProcW(hwnd, msg, wparam, lparam) },
    }
}

unsafe fn show_context_menu(hwnd: Hwnd) {
    unsafe {
        let menu = CreatePopupMenu();
        if menu == 0 {
            return;
        }
        append_item(menu, CMD_SHOW, "显示主窗口");
        append_item(menu, CMD_SNAPSHOT, "立即建快照");
        append_item(menu, CMD_UPDATE, "检查更新");
        AppendMenuW(menu, MF_SEPARATOR, 0, ptr::null());
        append_item(menu, CMD_QUIT, "退出");
        let mut point = Point::default();
        GetCursorPos(&mut point);
        // 隐藏窗口先抢前台，否则点菜单外部不会关闭（系统托盘菜单的经典写法）。
        SetForegroundWindow(hwnd);
        let chosen = TrackPopupMenu(
            menu,
            TPM_LEFTALIGN | TPM_RIGHTBUTTON | TPM_RETURNCMD,
            point.x,
            point.y,
            0,
            hwnd,
            ptr::null(),
        );
        PostMessageW(hwnd, WM_NULL, 0, 0);
        DestroyMenu(menu);
        if chosen != 0 {
            dispatch_command(chosen as u32);
        }
    }
}

unsafe fn append_item(menu: Hmenu, id: u32, text: &str) {
    let text = wide(text);
    unsafe {
        AppendMenuW(menu, MF_STRING, id as usize, text.as_ptr());
    }
}

/// 命令分发：还原窗口 + 入队；托盘线程不做任何业务。
fn dispatch_command(id: u32) {
    match id {
        CMD_SHOW => {
            restore_main_window();
            push_command(TrayCommand::Show);
        }
        CMD_SNAPSHOT => push_command(TrayCommand::SnapshotNow),
        CMD_UPDATE => push_command(TrayCommand::CheckUpdate),
        CMD_QUIT => push_command(TrayCommand::Quit),
        _ => {}
    }
}

fn push_command(command: TrayCommand) {
    {
        let mut guard = COMMANDS.lock().unwrap();
        // 右键菜单可能被连点：同一条命令未处理完前不重复入队。
        if guard.len() < 8 {
            guard.push(command);
        }
    }
    wake_ui();
}

fn wake_ui() {
    if let Some(ctx) = REPAINT.lock().unwrap().as_ref() {
        ctx.request_repaint();
    }
}

fn restore_main_window() {
    let hwnd = main_window();
    if hwnd == 0 {
        return;
    }
    unsafe {
        if IsIconic(hwnd) != 0 {
            ShowWindow(hwnd, SW_RESTORE);
        } else {
            ShowWindow(hwnd, SW_SHOW);
        }
        SetForegroundWindow(hwnd);
    }
    eprintln!("[tray] 还原主窗口 hwnd=0x{hwnd:x}");
}

/// 主窗口句柄：优先按标题精确匹配，退而求其次取本进程第一个可见顶层窗口。
fn main_window() -> Hwnd {
    let cached = MAIN_HWND.load(Ordering::SeqCst);
    if cached != 0 && unsafe { IsWindowVisible(cached) != 0 } {
        return cached;
    }
    let found = find_main_window();
    if found != 0 {
        MAIN_HWND.store(found, Ordering::SeqCst);
    }
    found
}

fn find_main_window() -> Hwnd {
    unsafe extern "system" fn callback(hwnd: Hwnd, lparam: Lparam) -> i32 {
        let slot = lparam as *mut Hwnd;
        let mut pid = 0u32;
        unsafe { GetWindowThreadProcessId(hwnd, &mut pid) };
        if pid != unsafe { GetCurrentProcessId() } {
            return 1;
        }
        let mut buffer = [0u16; 260];
        let len = unsafe { GetWindowTextW(hwnd, buffer.as_mut_ptr(), 260) };
        let visible = unsafe { IsWindowVisible(hwnd) } != 0;
        // 精确标题不要求可见：App::new 阶段 eframe 还没把窗口显示出来，
        // 但标题已经设好，靠它才能第一时间拿到主窗口句柄（图标 / 还原都用）。
        if len > 0 {
            let title = from_wide(&buffer[..len as usize]);
            if title == MAIN_TITLE {
                unsafe { *slot = hwnd };
                return 0; // 找到精确标题，停止枚举
            }
        }
        // 非精确匹配只接受可见窗口，继续找；枚举结束后若没找到就用它。
        if visible && len > 0 && unsafe { (*slot) == 0 } {
            unsafe { *slot = hwnd };
        }
        1
    }
    let mut slot: Hwnd = 0;
    unsafe {
        EnumWindows(Some(callback), &mut slot as *mut Hwnd as Lparam);
    }
    slot
}

// ---------- 小工具 ----------

fn wide(text: &str) -> Vec<u16> {
    text.encode_utf16().chain(std::iter::once(0)).collect()
}

fn from_wide(buffer: &[u16]) -> String {
    let end = buffer.iter().position(|unit| *unit == 0).unwrap_or(buffer.len());
    String::from_utf16_lossy(&buffer[..end])
}

/// 把字符串按 UTF-16 写入定长缓冲区，超出截断、始终 NUL 结尾。
fn copy_wide(text: &str, target: &mut [u16]) {
    let units: Vec<u16> = text.encode_utf16().collect();
    let take = units.len().min(target.len().saturating_sub(1));
    target[..take].copy_from_slice(&units[..take]);
    target[take] = 0;
}

fn set_last_error(message: &str) {
    *LAST_ERROR.lock().unwrap() = message.to_owned();
}

#[cfg(test)]
mod tests {
    use super::{TrayCommand, copy_wide, from_wide, wide};

    #[test]
    fn wide_roundtrip_keeps_chinese() {
        let units = wide("磁盘情报系统");
        assert_eq!(*units.last().unwrap(), 0);
        assert_eq!(from_wide(&units), "磁盘情报系统");
    }

    #[test]
    fn copy_wide_truncates_and_terminates() {
        let mut buffer = [1u16; 8];
        copy_wide("abcdefghijk", &mut buffer);
        assert_eq!(from_wide(&buffer), "abcdefg");
        assert_eq!(buffer[7], 0);
    }

    #[test]
    fn tray_commands_are_copy_and_comparable() {
        let commands = [
            TrayCommand::Show,
            TrayCommand::SnapshotNow,
            TrayCommand::CheckUpdate,
            TrayCommand::Quit,
        ];
        let copied = commands;
        assert_eq!(copied, commands);
    }
}

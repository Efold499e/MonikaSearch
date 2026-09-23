//! 系统原生右键菜单：对任意路径弹出与资源管理器完全一致的上下文菜单
//! （含第三方 shell 扩展，如 7-Zip / 杀毒 / Git），选中命令交给系统执行。
//!
//! 实现：命令线程上 SHParseDisplayName → IContextMenu → TrackPopupMenu
//! （TPM_RETURNCMD）→ InvokeCommand。消息只窗口把菜单钉在光标处。
//!
//! ## 病态第三方扩展
//!
//! 网盘 / 社交客户端的 shell 扩展会在 QueryContextMenu 里同步等待自家主程序，
//! 主程序没运行时就永远不返回（本机实测百度网盘 YunShellExt 对目录 >130s 仍不返回），
//! 连带把整个菜单拖死。Windows 只提供全局的屏蔽开关
//! （`Shell Extensions\Blocked`，会连资源管理器一起改掉），没有"只在本进程跳过"的接口。
//!
//! 但 COM 允许进程用 CoRegisterClassObject 注册自己的类工厂，且 CoCreateInstance
//! 优先使用本 apartment 已注册的类对象。于是给这些 CLSID 注册一个 CreateInstance
//! 直接失败的空工厂，shell 就会丢弃该扩展继续构建下一个菜单项 ——
//! 菜单其余部分仍是系统原生结果（其他第三方扩展 + 全部系统动词），
//! 且完全不影响资源管理器自己。
//!
//! 跳过名单 = 内置种子 + `%LOCALAPPDATA%\MonikaSearch\menu_skip.txt`（每行一个
//! CLSID，`#` 开头为注释）+ 环境变量 MONIKA_CTX_SUPPRESS（逗号分隔，供自动诊断用）。

use std::sync::atomic::{AtomicBool, Ordering};

use windows::core::PCSTR;
use windows::core::PCWSTR;
use windows::Win32::Foundation::{HWND, LPARAM, LRESULT, POINT, WPARAM};
use windows::Win32::System::Com::{CoInitializeEx, CoTaskMemFree, CoUninitialize, COINIT_APARTMENTTHREADED};
use windows::Win32::System::LibraryLoader::GetModuleHandleW;
use windows::Win32::UI::Shell::Common::ITEMIDLIST;
use windows::Win32::UI::Shell::{
    SHBindToParent, SHParseDisplayName, IContextMenu, IShellFolder,
    CMINVOKECOMMANDINFO, CMF_ITEMMENU, CMF_NORMAL,
};
use windows::Win32::UI::WindowsAndMessaging::{
    CreatePopupMenu, CreateWindowExW, DefWindowProcW, DestroyMenu, DestroyWindow,
    GetCursorPos, PostMessageW, RegisterClassW, SetForegroundWindow, TrackPopupMenu,
    SW_SHOWNORMAL, TPM_LEFTBUTTON, TPM_RETURNCMD, TPM_RIGHTBUTTON, WINDOW_EX_STYLE,
    WINDOW_STYLE, WM_NULL, WNDCLASSW, WS_POPUP,
};

pub static MENU_OPEN: AtomicBool = AtomicBool::new(false);

pub fn is_open() -> bool {
    MENU_OPEN.load(Ordering::SeqCst)
}

/// 已知会让菜单死等的扩展（CLSID）。
/// 百度网盘 YunShellExtContextMenu：对目录右键时同步等待未运行的网盘主程序，
/// 2026-09-22 本机实测 QueryContextMenu 超过 130s 不返回。
/// 可在设置里清空跳过列表；也可以用 menu_skip.txt 追加。
const BUILTIN_SKIP: &[&str] = &["{6D85624F-305A-491d-8848-C1927AA0D790}"];

pub fn builtin_skip() -> &'static [&'static str] {
    BUILTIN_SKIP
}

fn data_dir() -> Option<std::path::PathBuf> {
    std::env::var_os("LOCALAPPDATA").map(|d| std::path::PathBuf::from(d).join("MonikaSearch"))
}

pub fn skip_file() -> Option<std::path::PathBuf> {
    data_dir().map(|d| d.join("menu_skip.txt"))
}

/// 调试日志（发布版控制台不可见，写文件排查用）
fn log(msg: &str) {
    use std::io::Write;
    if let Some(dir) = data_dir() {
        let _ = std::fs::create_dir_all(&dir);
        let p = dir.join("ctxmenu.log");
        if let Ok(mut f) = std::fs::OpenOptions::new().create(true).append(true).open(p) {
            let _ = writeln!(f, "[{:?}] {}", std::time::SystemTime::now(), msg);
        }
    }
}

/// 阶段汇报：父进程/自动探测靠它区分"卡死 / 崩溃 / 菜单已建好"，
/// 否则只能靠一个对用户来说过长的超时来瞎猜。
/// 必须【追加】而不是覆盖 —— 后面的 "done" 若覆盖掉 "ready"，读取方就会
/// 因为慢一拍而误判成失败（探测阶段曾因此把无辜扩展误判为元凶）。
fn status(msg: &str) {
    if let Ok(p) = std::env::var("MONIKA_CTX_STATUS") {
        use std::io::Write;
        if let Ok(mut f) = std::fs::OpenOptions::new().create(true).append(true).open(&p) {
            let _ = writeln!(f, "{msg}");
        }
    }
    log(msg);
}

/// 需要在本进程内屏蔽的 CLSID 列表。
/// MONIKA_CTX_SUPPRESS_EXCLUSIVE=1 时只用 MONIKA_CTX_SUPPRESS（自动探测阶段用，
/// 保证探测结果只反映传入的集合，不叠加内置种子和跳过文件）。
fn skip_clsids() -> Vec<String> {
    let exclusive = std::env::var("MONIKA_CTX_SUPPRESS_EXCLUSIVE").is_ok();
    let mut v: Vec<String> = Vec::new();
    if !exclusive {
        v.extend(BUILTIN_SKIP.iter().map(|s| (*s).to_string()));
    }
    if let Ok(extra) = std::env::var("MONIKA_CTX_SUPPRESS") {
        v.extend(
            extra
                .split(',')
                .map(str::trim)
                .filter(|s| !s.is_empty())
                .map(str::to_string),
        );
    }
    if !exclusive {
        if let Some(p) = skip_file() {
            if let Ok(text) = std::fs::read_to_string(p) {
                for line in text.lines() {
                    let l = line.split('#').next().unwrap_or("").trim();
                    if l.is_empty() {
                        continue;
                    }
                    v.push(l.to_string());
                }
            }
        }
    }
    v.sort();
    v.dedup();
    v
}

// ─────────────────────────────────────────────────────────────────────────────
// 空类工厂：CreateInstance 直接返回 E_FAIL，让 shell 跳过这个扩展
// ─────────────────────────────────────────────────────────────────────────────

#[repr(C)]
#[derive(Clone, Copy, PartialEq, Eq)]
struct Guid16 {
    d1: u32,
    d2: u16,
    d3: u16,
    d4: [u8; 8],
}

const IID_IUNKNOWN: Guid16 = Guid16 { d1: 0, d2: 0, d3: 0, d4: [0xC0, 0, 0, 0, 0, 0, 0, 0x46] };
const IID_ICLASSFACTORY: Guid16 = Guid16 { d1: 1, d2: 0, d3: 0, d4: [0xC0, 0, 0, 0, 0, 0, 0, 0x46] };

const S_OK_HR: i32 = 0;
const E_NOINTERFACE_HR: i32 = 0x8000_4002u32 as i32;
const E_FAIL_HR: i32 = 0x8000_4005u32 as i32;
const E_POINTER_HR: i32 = 0x8000_4003u32 as i32;

#[repr(C)]
struct StubVtbl {
    query_interface: unsafe extern "system" fn(*mut core::ffi::c_void, *const Guid16, *mut *mut core::ffi::c_void) -> i32,
    add_ref: unsafe extern "system" fn(*mut core::ffi::c_void) -> u32,
    release: unsafe extern "system" fn(*mut core::ffi::c_void) -> u32,
    create_instance: unsafe extern "system" fn(*mut core::ffi::c_void, *mut core::ffi::c_void, *const Guid16, *mut *mut core::ffi::c_void) -> i32,
    lock_server: unsafe extern "system" fn(*mut core::ffi::c_void, i32) -> i32,
}

unsafe extern "system" fn stub_qi(
    this: *mut core::ffi::c_void,
    iid: *const Guid16,
    out: *mut *mut core::ffi::c_void,
) -> i32 {
    if out.is_null() || iid.is_null() {
        return E_POINTER_HR;
    }
    let id = *iid;
    if id == IID_IUNKNOWN || id == IID_ICLASSFACTORY {
        *out = this;
        S_OK_HR
    } else {
        *out = std::ptr::null_mut();
        E_NOINTERFACE_HR
    }
}

unsafe extern "system" fn stub_add_ref(_this: *mut core::ffi::c_void) -> u32 {
    2
}

unsafe extern "system" fn stub_release(_this: *mut core::ffi::c_void) -> u32 {
    1
}

unsafe extern "system" fn stub_create(
    _this: *mut core::ffi::c_void,
    _outer: *mut core::ffi::c_void,
    _iid: *const Guid16,
    out: *mut *mut core::ffi::c_void,
) -> i32 {
    if !out.is_null() {
        *out = std::ptr::null_mut();
    }
    E_FAIL_HR
}

unsafe extern "system" fn stub_lock(_this: *mut core::ffi::c_void, _lock: i32) -> i32 {
    S_OK_HR
}

static STUB_VTBL: StubVtbl = StubVtbl {
    query_interface: stub_qi,
    add_ref: stub_add_ref,
    release: stub_release,
    create_instance: stub_create,
    lock_server: stub_lock,
};

#[repr(C)]
struct StubFactory {
    vtbl: *const StubVtbl,
}

unsafe impl Sync for StubFactory {}
unsafe impl Send for StubFactory {}

static STUB_FACTORY: StubFactory = StubFactory { vtbl: &STUB_VTBL };

// ole32：直接声明，避免为 windows-sys 再开 feature
#[link(name = "ole32")]
extern "system" {
    fn CoRegisterClassObject(
        rclsid: *const Guid16,
        punk: *mut core::ffi::c_void,
        dwclscontext: u32,
        flags: u32,
        lpdwregister: *mut u32,
    ) -> i32;
}

const CLSCTX_INPROC_SERVER_V: u32 = 1;
const REGCLS_MULTIPLEUSE_V: u32 = 1;

fn parse_guid(s: &str) -> Option<Guid16> {
    let t: String = s.chars().filter(|c| c.is_ascii_hexdigit()).collect();
    if t.len() != 32 {
        return None;
    }
    let byte = |i: usize| u8::from_str_radix(&t[i..i + 2], 16).ok();
    Some(Guid16 {
        d1: u32::from_str_radix(&t[0..8], 16).ok()?,
        d2: u16::from_str_radix(&t[8..12], 16).ok()?,
        d3: u16::from_str_radix(&t[12..16], 16).ok()?,
        d4: [
            byte(16)?, byte(18)?, byte(20)?, byte(22)?,
            byte(24)?, byte(26)?, byte(28)?, byte(30)?,
        ],
    })
}

/// 在本进程内屏蔽一个 shell 扩展。必须在线程 COM 初始化之后调用。
pub fn suppress_clsid(clsid: &str) -> bool {
    let Some(g) = parse_guid(clsid) else {
        log(&format!("suppress: CLSID 解析失败 {clsid}"));
        return false;
    };
    let mut cookie: u32 = 0;
    let hr = unsafe {
        CoRegisterClassObject(
            &g as *const Guid16,
            &STUB_FACTORY as *const StubFactory as *mut core::ffi::c_void,
            CLSCTX_INPROC_SERVER_V,
            REGCLS_MULTIPLEUSE_V,
            &mut cookie,
        )
    };
    let ok = hr >= 0;
    if !ok {
        log(&format!("suppress {clsid} 失败 hr={hr:#x}"));
    }
    ok
}

/// 弹出上下文菜单。返回是否执行了某个命令。
/// MONIKA_CTX_STEPS=1 仅解析+绑定（诊断）；2 加到 QueryContextMenu；3 完整（默认）
pub fn run_context_menu(path: &str) -> Result<bool, String> {
    let max_step: u32 = std::env::var("MONIKA_CTX_STEPS")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(3);
    MENU_OPEN.store(true, Ordering::SeqCst);
    status(&format!("start steps<={max_step} {path}"));
    let result = run_context_menu_inner(path, max_step);
    match &result {
        Ok(a) => status(&format!("done acted={a}")),
        Err(e) => status(&format!("error {e}")),
    }
    MENU_OPEN.store(false, Ordering::SeqCst);
    result
}

fn run_context_menu_inner(path: &str, max_step: u32) -> Result<bool, String> {
    unsafe {
        // 命令线程需要自己的 COM apartment（STA，shell 对象要求）
        let hr = CoInitializeEx(None, COINIT_APARTMENTTHREADED);
        if hr.is_err() {
            return Err(format!("CoInitializeEx 失败: {hr:?}"));
        }
        for clsid in skip_clsids() {
            suppress_clsid(&clsid);
        }
        let result = context_menu_impl(path, max_step);
        CoUninitialize();
        result
    }
}

fn context_menu_impl(path: &str, max_step: u32) -> Result<bool, String> {
    unsafe {
        // 1. 隐藏的顶层弹出窗口（菜单宿主）——不能用 message-only 窗口：
        //    SetForegroundWindow 对其无效，TrackPopupMenu 菜单无法正常交互
        let hinstance = GetModuleHandleW(PCWSTR::null()).unwrap_or_default().into();
        let class_name: PCWSTR = windows::core::w!("MonikaCtxMenuHost");
        let wc = WNDCLASSW {
            lpfnWndProc: Some(host_wndproc),
            hInstance: hinstance,
            lpszClassName: class_name,
            ..Default::default()
        };
        RegisterClassW(&wc);
        let host = CreateWindowExW(
            WINDOW_EX_STYLE(0),
            class_name,
            class_name,
            WINDOW_STYLE(WS_POPUP.0), // 顶层弹出窗口，保持隐藏（不 ShowWindow）
            0,
            0,
            0,
            0,
            None,
            None,
            Some(hinstance),
            None,
        )
        .map_err(|e| format!("创建菜单宿主窗口失败: {e}"))?;

        // 2. 路径 → PIDL → IContextMenu
        let wide: Vec<u16> = path.encode_utf16().chain(std::iter::once(0)).collect();
        let mut pidl: *mut ITEMIDLIST = std::ptr::null_mut();
        SHParseDisplayName(PCWSTR::from_raw(wide.as_ptr()), None::<&windows::Win32::System::Com::IBindCtx>, &mut pidl, 0, None)
            .map_err(|e| {
                DestroyWindow(host).ok();
                format!("路径无法解析（文件可能已不存在）: {e}")
            })?;
        status("bound");

        // 关键：GetUIObjectOf 需要的是 folder 的【相对】子 PIDL（SHBindToParent 的
        // ppidllast 输出），而不是完整绝对 PIDL —— 传错会 E_INVALIDARG
        let mut child: *mut ITEMIDLIST = std::ptr::null_mut();
        let folder: IShellFolder = match SHBindToParent(pidl, Some(&mut child)) {
            Ok(f) => f,
            Err(e) => {
                CoTaskMemFree(Some(pidl as *const core::ffi::c_void));
                DestroyWindow(host).ok();
                return Err(format!("SHBindToParent 失败: {e}"));
            }
        };

        // 把真实宿主窗口交给扩展（不是 NULL）——需要 hwnd 的扩展（自绘/弹子菜单）
        // 才有正确的属主窗口。
        let ctx: IContextMenu = match folder.GetUIObjectOf(
            host,
            &[child as *const ITEMIDLIST],
            None,
        ) {
            Ok(c) => c,
            Err(e) => {
                CoTaskMemFree(Some(pidl as *const core::ffi::c_void));
                DestroyWindow(host).ok();
                return Err(format!("获取 IContextMenu 失败: {e}"));
            }
        };
        // 注意：不主动 CoTaskMemFree child/pidl —— child 是 pidl 分配块的内部指针，
        // 单独释放会造成堆损坏（0xC0000374）。ctxhost 为秒级进程，交由 OS 回收。
        let _ = (&child, &pidl);

        if max_step < 2 {
            return Ok(false);
        }

        // 3. 填充菜单。
        // 注意：不做 IContextMenu2/3 的 owner-draw 子类转发——该路径在此环境下
        // 触发 0xC000041D（窗口回调致命异常）。代价是菜单项无自定义图标，功能不变。
        let hmenu = CreatePopupMenu().unwrap_or_default();
        // QueryContextMenu 返回 HRESULT（低 16 位是加入的菜单项数，非 S_OK 属正常）
        let hr = ctx.QueryContextMenu(hmenu, 0, 1, 0x7FFF, CMF_NORMAL | CMF_ITEMMENU);
        if hr.is_err() {
            DestroyMenu(hmenu).ok();
            DestroyWindow(host).ok();
            return Err(format!("QueryContextMenu 失败: {hr:?}"));
        }
        status(&format!("ready items={}", (hr.0 as u32) & 0xFFFF));

        if max_step < 3 {
            return Ok(false);
        }

        // 4. 光标处弹出（前台必须设为宿主，否则点击外部菜单不消失）
        let mut pt = POINT::default();
        let _ = GetCursorPos(&mut pt);
        let _ = SetForegroundWindow(host);
        let chosen = TrackPopupMenu(
            hmenu,
            TPM_RETURNCMD | TPM_LEFTBUTTON | TPM_RIGHTBUTTON,
            pt.x,
            pt.y,
            None,
            host,
            None,
        );
        status(&format!("chosen={}", chosen.0));
        // KB135788：菜单结束后归还原前台状态
        let _ = PostMessageW(Some(host), WM_NULL, WPARAM(0), LPARAM(0));

        // 5. 执行选中命令（TPM_RETURNCMD 返回值即菜单项 id，作为 lpVerb 偏移）
        let mut acted = false;
        let cmd = chosen.0 as usize;
        if cmd != 0 {
            let mut ici = CMINVOKECOMMANDINFO::default();
            ici.cbSize = std::mem::size_of::<CMINVOKECOMMANDINFO>() as u32;
            ici.hwnd = host;
            ici.lpVerb = PCSTR::from_raw(cmd as *const u8);
            ici.nShow = SW_SHOWNORMAL.0;
            match ctx.InvokeCommand(&ici) {
                Ok(()) => acted = true,
                Err(e) => log(&format!("InvokeCommand 失败: {e}")),
            }
        }

        // 6. 清理
        DestroyMenu(hmenu).ok();
        DestroyWindow(host).ok();
        Ok(acted)
    }
}

/// 宿主窗口过程（DefWindowProcW 生成包装非 system ABI，需自带转接）
unsafe extern "system" fn host_wndproc(hwnd: HWND, msg: u32, wparam: WPARAM, lparam: LPARAM) -> LRESULT {
    DefWindowProcW(hwnd, msg, wparam, lparam)
}

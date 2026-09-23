//! Tauri 命令：搜索、打开/定位、状态、配置、AI、自启。

use std::os::windows::process::CommandExt;
use std::process::Command;

use ep_core::engine::{Hit, SearchQuery};
use tauri::{AppHandle, Emitter, Manager, State};
use windows::core::PCWSTR;
use windows::Win32::Foundation::HWND;

use crate::ai::{run_ai_search, AiResult};
use crate::state::{AppState, Config, STATE_READY, STATE_SCANNING};

const CREATE_NO_WINDOW: u32 = 0x0800_0000;

/// 主进程侧的"外部菜单打开中"标记：ctxhost 子进程弹菜单期间会抢走前台，
/// 主窗口失焦自动隐藏必须在此期间禁用（子进程自己的状态对主进程不可见）。
pub static HOST_MENU_OPEN: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

pub fn is_host_menu_open() -> bool {
    HOST_MENU_OPEN.load(std::sync::atomic::Ordering::SeqCst)
}

fn to_wide(s: &str) -> Vec<u16> {
    s.encode_utf16().chain(std::iter::once(0)).collect()
}

#[tauri::command]
pub fn search(state: State<AppState>, q: SearchQuery) -> Vec<Hit> {
    let eg = state.engine.read().unwrap();
    eg.search(&q)
}

#[derive(serde::Serialize)]
pub struct StatusInfo {
    pub state: String,
    pub total: u64,
    pub dirs: u64,
    pub scanned: u64,
    pub error: String,
    pub volumes: Vec<char>,
}

#[tauri::command]
pub fn get_status(state: State<AppState>) -> StatusInfo {
    let s = state.status.state.load(std::sync::atomic::Ordering::Relaxed);
    let eg = state.engine.read().unwrap();
    let stats = eg.stats();
    StatusInfo {
        state: match s {
            STATE_SCANNING => "scanning".into(),
            STATE_READY => "ready".into(),
            _ => "empty".into(),
        },
        total: stats.total_alive,
        dirs: stats.total_dirs,
        scanned: state.status.scanned.load(std::sync::atomic::Ordering::Relaxed),
        error: state.status.last_error.lock().unwrap().clone(),
        volumes: eg.volume_states().iter().map(|v| v.letter).collect(),
    }
}

#[tauri::command]
pub fn rebuild(state: State<AppState>) -> Result<(), String> {
    state.tx.send(crate::state::EngineMsg::Rebuild).map_err(|e| e.to_string())
}

#[tauri::command]
pub fn open_path(path: String) -> Result<(), String> {
    Command::new("explorer.exe")
        .arg(&path)
        .creation_flags(CREATE_NO_WINDOW)
        .spawn()
        .map_err(|e| e.to_string())?;
    Ok(())
}

#[tauri::command]
/// 在资源管理器中定位并选中文件/文件夹。
/// 主路径 SHOpenFolderAndSelectItems（原生 API，能复用已打开的资源管理器窗口）；
/// 失败时回退 explorer.exe /select —— 必须用 raw_arg 传单一参数，否则 Rust 的
/// argv 转义会破坏引号，explorer 解析不到路径就退化成只打开"此电脑"。
pub fn reveal_path(path: String) -> Result<(), String> {
    use windows::Win32::System::Com::{
        CoInitializeEx, CoTaskMemFree, CoUninitialize, COINIT_APARTMENTTHREADED, IBindCtx,
    };
    use windows::Win32::UI::Shell::Common::ITEMIDLIST;
    use windows::Win32::UI::Shell::{
        SHBindToParent, SHOpenFolderAndSelectItems, SHParseDisplayName, IShellFolder,
    };

    let file = to_wide(&path);
    unsafe {
        let hr = CoInitializeEx(None, COINIT_APARTMENTTHREADED);
        let com_ok = hr.is_ok();
        let r = (|| -> Result<(), String> {
            let mut file_pidl: *mut ITEMIDLIST = std::ptr::null_mut();
            SHParseDisplayName(
                PCWSTR::from_raw(file.as_ptr()),
                None::<&IBindCtx>,
                &mut file_pidl,
                0,
                None,
            )
            .map_err(|e| {
                CoTaskMemFree(Some(file_pidl as *const core::ffi::c_void));
                format!("路径解析失败: {e}")
            })?;

            // child 是 file_pidl 分配块【内部】的相对 PIDL，只能随 file_pidl 一起
            // 释放；单独 CoTaskMemFree(child) 会破坏堆（0xC0000374）。
            let mut child: *mut ITEMIDLIST = std::ptr::null_mut();
            let _folder: IShellFolder = SHBindToParent(file_pidl, Some(&mut child)).map_err(|e| {
                CoTaskMemFree(Some(file_pidl as *const core::ffi::c_void));
                format!("绑定父目录失败: {e}")
            })?;

            let dir = std::path::Path::new(&path)
                .parent()
                .map(|p| p.to_string_lossy().to_string())
                .ok_or_else(|| "无父目录".to_string())?;
            let dirw = to_wide(&dir);
            let mut dir_pidl: *mut ITEMIDLIST = std::ptr::null_mut();
            let parsed = SHParseDisplayName(
                PCWSTR::from_raw(dirw.as_ptr()),
                None::<&IBindCtx>,
                &mut dir_pidl,
                0,
                None,
            );
            if parsed.is_err() {
                CoTaskMemFree(Some(file_pidl as *const core::ffi::c_void));
                return Err(format!("父目录解析失败: {parsed:?}"));
            }

            let hr2 = SHOpenFolderAndSelectItems(dir_pidl, Some(&[child as *const ITEMIDLIST]), 0);
            CoTaskMemFree(Some(dir_pidl as *const core::ffi::c_void));
            CoTaskMemFree(Some(file_pidl as *const core::ffi::c_void));
            hr2.map_err(|e| format!("定位失败: {e}"))
        })();
        if com_ok {
            CoUninitialize();
        }
        if r.is_err() {
            // 回退到 explorer /select：整段作为单一原始参数传入，引号不被转义
            if Command::new("explorer.exe")
                .raw_arg(format!("/select,\"{path}\""))
                .creation_flags(CREATE_NO_WINDOW)
                .spawn()
                .is_ok()
            {
                return Ok(());
            }
        }
        r
    }
}

#[tauri::command]
pub fn hide_window(app: AppHandle) {
    if let Some(w) = app.get_webview_window("main") {
        let _ = w.hide();
    }
}

#[tauri::command]
pub fn get_config(state: State<AppState>) -> Config {
    state.config.lock().unwrap().clone()
}

#[tauri::command]
pub fn set_config(state: State<AppState>, cfg: Config) -> Result<(), String> {
    *state.config.lock().unwrap() = cfg.clone();
    crate::state::save_config(&state.config_path(), &cfg).map_err(|e| e.to_string())
}

#[tauri::command]
pub fn ai_search(state: State<AppState>, text: String) -> Result<AiResult, String> {
    let cfg = state.config.lock().unwrap().clone();
    let eg = state.engine.read().unwrap();
    run_ai_search(&cfg, &text, &eg)
}

#[tauri::command]
pub fn get_icon(
    state: State<AppState>,
    ext: String,
) -> Option<String> {
    let key = ext.to_lowercase();
    if let Some(v) = state.icons.lock().unwrap().get(&key) {
        return Some(v.clone());
    }
    let url = crate::icons::icon_data_url(&key)?;
    state.icons.lock().unwrap().insert(key, url.clone());
    Some(url)
}

#[tauri::command]
pub fn image_thumb(path: String) -> Option<String> {
    crate::icons::image_thumbnail(&path)
}

/// 弹出系统原生右键菜单（与资源管理器一致）。返回是否执行了命令。
/// 宿主是独立进程 ctxhost.exe，第三方 shell 扩展的崩溃/卡死被隔离在外面。
///
/// 卡死发生在 QueryContextMenu 阶段（菜单弹出之前），所以这里分段等待：
///   阶段一 等宿主把菜单建好（超时即判定某个扩展卡死 → HOST_HANG）
///   阶段二 菜单已弹出，等用户操作
/// 这样既能立刻识别卡死，又不会把"用户正在浏览菜单"误判成卡死。
///
/// 失败时前端回退内置菜单，同时后台自动诊断：逐候选探测定位是哪个扩展，
/// 记进跳过列表；之后右键就是完整系统菜单并且毫秒级弹出。
#[tauri::command]
pub async fn show_context_menu(app: AppHandle, path: String) -> Result<bool, String> {
    use std::sync::atomic::Ordering;
    tauri::async_runtime::spawn_blocking(move || {
        HOST_MENU_OPEN.store(true, Ordering::SeqCst);
        let result = run_menu_host(&path);
        HOST_MENU_OPEN.store(false, Ordering::SeqCst);
        if let Err(reason) = &result {
            if reason == "HOST_HANG" || reason == "HOST_CRASHED" {
                crate::shellex::start_background_diagnosis(app.clone(), path.clone());
            }
        }
        result
    })
    .await
    .map_err(|e| e.to_string())?
}

/// 菜单建好（QueryContextMenu 返回）的等待上限
const MENU_READY_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(6);/// 菜单已弹出后的兜底上限（宿主卡住时回收）
const MENU_HARD_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(600);

fn run_menu_host(path: &str) -> Result<bool, String> {
    use std::time::{Duration, Instant};

    let stamp = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    let status = std::env::temp_dir().join(format!("monika_ctx_{}_{stamp}.txt", std::process::id()));
    let _ = std::fs::remove_file(&status);

    let mut child = Command::new("ctxhost.exe")
        .env("MONIKA_CTX_PATH", path)
        .env("MONIKA_CTX_STATUS", &status)
        .creation_flags(CREATE_NO_WINDOW)
        .spawn()
        .map_err(|e| format!("启动菜单宿主失败: {e}"))?;

    // 阶段一：等菜单建好
    let ready_deadline = Instant::now() + MENU_READY_TIMEOUT;
    loop {
        if crate::shellex::status_has(&status, "ready") {
            break;
        }
        if crate::shellex::status_has(&status, "error") {
            let msg = std::fs::read_to_string(&status).unwrap_or_default();
            let detail = msg
                .lines()
                .find(|l| l.starts_with("error"))
                .map(|l| l.trim_start_matches("error").trim().to_string())
                .unwrap_or_else(|| "宿主报错（详见 ctxmenu.log）".into());
            let _ = child.kill();
            let _ = std::fs::remove_file(&status);
            return Err(detail);
        }
        match child.try_wait() {
            Ok(Some(code)) => {
                let _ = std::fs::remove_file(&status);
                return Err(match code.code() {
                    Some(2) => "宿主参数错误".to_string(),
                    Some(3) => "宿主报错（详见 ctxmenu.log）".to_string(),
                    _ => "HOST_CRASHED".to_string(),
                });
            }
            Ok(None) => {}
            Err(e) => {
                let _ = std::fs::remove_file(&status);
                return Err(e.to_string());
            }
        }
        if Instant::now() > ready_deadline {
            let _ = child.kill();
            let _ = std::fs::remove_file(&status);
            return Err("HOST_HANG".into());
        }
        std::thread::sleep(Duration::from_millis(30));
    }

    // 阶段二：菜单已弹出，等用户选择（宿主退出即结束）
    let hard_deadline = Instant::now() + MENU_HARD_TIMEOUT;
    loop {
        match child.try_wait() {
            Ok(Some(code)) => {
                let _ = std::fs::remove_file(&status);
                return Ok(code.code() == Some(1));
            }
            Ok(None) => {}
            Err(e) => {
                let _ = std::fs::remove_file(&status);
                return Err(e.to_string());
            }
        }
        if Instant::now() > hard_deadline {
            let _ = child.kill();
            let _ = std::fs::remove_file(&status);
            return Ok(false);
        }
        std::thread::sleep(Duration::from_millis(120));
    }
}

/// 当前被跳过的第三方右键扩展（内置种子 + 自动诊断结果）
#[tauri::command]
pub fn get_menu_skip() -> Vec<String> {
    crate::shellex::current_skip()
}

/// 清空跳过列表（内置种子保留），下次右键重新自动检测
#[tauri::command]
pub fn clear_menu_skip() -> Result<(), String> {
    crate::shellex::clear_skip().map_err(|e| e.to_string())
}

/// 弹出文件属性对话框（shell 原生）。
/// 坑：SHOP_TYPE 的取值是 SHOP_PRINTERNAME=1 / SHOP_FILEPATH=2 / SHOP_VOLUMENAME=3，
/// 传 1 会被当成打印机名去找一个叫 "C:\..." 的打印机，于是"属性"毫无反应。
/// 属性表要求 STA，独立线程里自己初始化 COM，顺带不阻塞 Tauri 的主循环。
#[tauri::command]
pub fn show_properties(path: String) -> Result<(), String> {
    use windows::Win32::System::Com::{CoInitializeEx, CoUninitialize, COINIT_APARTMENTTHREADED};
    use windows::Win32::UI::Shell::{SHObjectProperties, SHOP_FILEPATH};

    std::thread::Builder::new()
        .name("props-dialog".into())
        .spawn(move || {
            let file = to_wide(&path);
            let page = to_wide("");
            let hr = unsafe { CoInitializeEx(None, COINIT_APARTMENTTHREADED) };
            let ok = unsafe {
                SHObjectProperties(
                    None,
                    SHOP_FILEPATH,
                    PCWSTR::from_raw(file.as_ptr()),
                    PCWSTR::from_raw(page.as_ptr()),
                )
            };
            if hr.is_ok() {
                unsafe { CoUninitialize() };
            }
            if !ok.as_bool() {
                eprintln!("SHObjectProperties 失败: {path}");
            }
        })
        .map(|_| ())
        .map_err(|e| e.to_string())
}

/// 删除到回收站（系统确认进度，FOF_ALLOWUNDO）
#[tauri::command]
pub fn delete_path(path: String) -> Result<(), String> {
    use windows::Win32::UI::Shell::{SHFileOperationW, SHFILEOPSTRUCTW, FOF_ALLOWUNDO};
    let mut from = to_wide(&path);
    from.push(0); // 双 \0 结尾的文件列表
    unsafe {
        let mut op = SHFILEOPSTRUCTW {
            hwnd: HWND::default(),
            wFunc: 3, // FO_DELETE
            pFrom: PCWSTR::from_raw(from.as_ptr()),
            pTo: PCWSTR::null(),
            fFlags: FOF_ALLOWUNDO.0 as u16,
            fAnyOperationsAborted: Default::default(),
            hNameMappings: std::ptr::null_mut(),
            lpszProgressTitle: PCWSTR::null(),
        };
        let r = SHFileOperationW(&mut op);
        if r != 0 {
            return Err(format!("删除失败（代码 {r}）"));
        }
    }
    Ok(())
}

#[tauri::command]
pub fn get_autostart(app: AppHandle) -> bool {
    let out = Command::new("schtasks")
        .args(["/Query", "/TN", "MonikaSearch"])
        .creation_flags(CREATE_NO_WINDOW)
        .output();
    matches!(out, Ok(o) if o.status.success())
}

/// requireAdministrator 程序无法可靠地通过 Run 注册表键自启，
/// 改用登录触发的计划任务（最高权限运行）。
#[tauri::command]
pub fn set_autostart(app: AppHandle, enabled: bool) -> Result<(), String> {
    let _ = app;
    if enabled {
        let exe = std::env::current_exe().map_err(|e| e.to_string())?;
        let task = format!("\"{}\" --hidden", exe.display());
        let out = Command::new("schtasks")
            .args(["/Create", "/TN", "MonikaSearch", "/TR", &task,
                   "/SC", "ONLOGON", "/RL", "HIGHEST", "/F"])
            .creation_flags(CREATE_NO_WINDOW)
            .output()
            .map_err(|e| e.to_string())?;
        if !out.status.success() {
            return Err(format!("创建计划任务失败: {}", String::from_utf8_lossy(&out.stderr)));
        }
        Ok(())
    } else {
        let out = Command::new("schtasks")
            .args(["/Delete", "/TN", "MonikaSearch", "/F"])
            .creation_flags(CREATE_NO_WINDOW)
            .output()
            .map_err(|e| e.to_string())?;
        if !out.status.success() {
            return Err(format!("删除计划任务失败: {}", String::from_utf8_lossy(&out.stderr)));
        }
        Ok(())
    }
}

/// 供托盘“设置”菜单调用：显示窗口并通知前端打开设置面板
pub fn open_settings(app: &AppHandle) {
    if let Some(w) = app.get_webview_window("main") {
        let _ = w.show();
        let _ = w.set_focus();
        let _ = app.emit("open-settings", ());
    }
}

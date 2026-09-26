#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

mod ai;
mod commands;
mod ctxmenu;
mod icons;
mod shellex;
mod state;

use std::sync::{mpsc, Arc, Mutex, RwLock};

use tauri::{
    AppHandle, Emitter, Manager, PhysicalPosition, WindowEvent,
};
use tauri::menu::{Menu, MenuItem};
use tauri::tray::{MouseButton, MouseButtonState, TrayIconBuilder, TrayIconEvent};
use tauri_plugin_global_shortcut::ShortcutState;

use ep_core::engine::Engine;
use state::{AppState, Status};

fn main() {
    tauri::Builder::default()
        .plugin(tauri_plugin_single_instance::init(|app, args, _cwd| {
            // 二次启动：唤起已有实例的搜索窗口；带路径参数（右键菜单）则限定搜索范围
            match launch_path_from_args(&args) {
                Some(lp) => {
                    if let Some(st) = app.try_state::<state::AppState>() {
                        *st.launch_path.lock().unwrap() = Some(lp.clone());
                    }
                    show_main(app);
                    let _ = app.emit("open-with-path", lp);
                }
                None => show_main(app),
            }
        }))        .plugin(
            tauri_plugin_global_shortcut::Builder::new()
                .with_handler(|app, _shortcut, event| {
                    if event.state == ShortcutState::Pressed {
                        toggle_main(app);
                    }
                })
                .build(),
        )
        .invoke_handler(tauri::generate_handler![
            commands::search,
            commands::get_status,
            commands::rebuild,
            commands::open_path,
            commands::reveal_path,
            commands::hide_window,
            commands::get_config,
            commands::set_config,
            commands::ai_search,
            commands::get_autostart,
            commands::set_autostart,
            commands::get_icon,
            commands::image_thumb,
            commands::show_context_menu,
            commands::get_menu_skip,
            commands::clear_menu_skip,
            commands::show_properties,
            commands::delete_path,
            commands::take_launch_path
        ])
        .on_window_event(|window, event| {
            // 点关闭 = 隐藏，进程留在托盘
            if let WindowEvent::CloseRequested { api, .. } = event {
                api.prevent_close();
                let _ = window.hide();
            }
            // 失焦自动隐藏（下拉浮层行为）；右键菜单宿主运行期间例外
            if let WindowEvent::Focused(false) = event {
                if !commands::is_host_menu_open() && !ctxmenu::is_open() {
                    let _ = window.hide();
                }
            }
        })
        .setup(|app| {
            // 数据目录：%LOCALAPPDATA%\MonikaSearch（自动从 EverythingPlus 旧目录迁移）
            let data_dir = state::data_root();
            std::fs::create_dir_all(&data_dir).ok();

            let config = state::load_config(&data_dir.join("config.json"));
            let (tx, rx) = mpsc::channel();
            let engine = Arc::new(RwLock::new(Engine::empty()));
            let status = Status::new();

            app.manage(AppState {
                engine: engine.clone(),
                status: status.clone(),
                tx,
                config: Mutex::new(config),
                data_dir: data_dir.clone(),
                icons: Mutex::new(std::collections::HashMap::new()),
                launch_path: Mutex::new(None),
            });

            // 冷启动带路径参数（右键"用 MonikaSearch 搜索"且实例未运行）：
            // 显示窗口并通知前端限定目录/按文件名搜索
            let args: Vec<String> = std::env::args().skip(1).collect();
            if let Some(lp) = launch_path_from_args(&args) {
                if let Some(st) = app.try_state::<state::AppState>() {
                    *st.launch_path.lock().unwrap() = Some(lp.clone());
                }
                let handle = app.handle();
                show_main(handle);
                let _ = handle.emit("open-with-path", lp);
            }

            // 后台引擎线程
            std::thread::Builder::new()
                .name("ep-engine".into())
                .spawn(move || state::run_engine(data_dir, engine, status, rx))
                .expect("启动引擎线程失败");

            build_tray(app)?;

            // 逐个注册热键：某个被占用（如 Alt+Space 被 PowerToys 等占用）只跳过，不崩
            {
                use tauri_plugin_global_shortcut::GlobalShortcutExt;
                let gs = app.global_shortcut();
                for sc in ["alt+e", "alt+space"] {
                    if let Err(e) = gs.register(sc) {
                        eprintln!("热键 {sc} 注册失败（可能被占用）: {e}");
                    }
                }
            }
            Ok(())
        })
        .run(tauri::generate_context!())
        .expect("MonikaSearch 运行失败");
}

fn build_tray(app: &tauri::App) -> tauri::Result<()> {
    let show = MenuItem::with_id(app, "show", "显示搜索 (Alt+E)", true, None::<&str>)?;
    let rebuild = MenuItem::with_id(app, "rebuild", "重建索引", true, None::<&str>)?;
    let settings = MenuItem::with_id(app, "settings", "设置…", true, None::<&str>)?;
    let quit = MenuItem::with_id(app, "quit", "退出", true, None::<&str>)?;
    let menu = Menu::with_items(app, &[&show, &rebuild, &settings, &quit])?;

    let _tray = TrayIconBuilder::with_id("main-tray")
        .icon(app.default_window_icon().expect("缺少图标").clone())
        .tooltip("MonikaSearch — Alt+E / Alt+Space 搜索")
        .menu(&menu)
        .show_menu_on_left_click(false)
        .on_menu_event(|app, event| match event.id().as_ref() {
            "show" => show_main(app),
            "rebuild" => {
                if let Some(state) = app.try_state::<AppState>() {
                    let _ = state.tx.send(state::EngineMsg::Rebuild);
                }
            }
            "settings" => commands::open_settings(app),
            "quit" => {
                // 退出前保存索引缓存
                if let Some(state) = app.try_state::<AppState>() {
                    let _ = state.engine.read().unwrap().save_cache(&state.cache_path());
                }
                app.exit(0);
            }
            _ => {}
        })
        .on_tray_icon_event(|tray, event| {
            if let TrayIconEvent::Click {
                button: MouseButton::Left,
                button_state: MouseButtonState::Up,
                ..
            } = event
            {
                toggle_main(tray.app_handle());
            }
        })
        .build(app)?;
    Ok(())
}

/// 从启动参数里找"存在的路径"（跳过 -- 开关和 exe 自身），用于右键菜单启动
fn launch_path_from_args(args: &[String]) -> Option<state::LaunchPath> {
    let exe = std::env::current_exe().ok();
    args.iter()
        .filter(|a| !a.is_empty() && !a.starts_with('-'))
        .find(|a| {
            if let Some(e) = &exe {
                if a.eq_ignore_ascii_case(&e.to_string_lossy()) {
                    return false;
                }
            }
            std::path::Path::new(a).exists()
        })
        .map(|p| {
            let is_dir = std::path::Path::new(p).is_dir();
            state::LaunchPath { path: p.clone(), is_dir }
        })
}

fn toggle_main(app: &AppHandle) {
    if let Some(w) = app.get_webview_window("main") {
        if w.is_visible().unwrap_or(false) {
            let _ = w.hide();
        } else {
            show_window_top_center(&w);
            let _ = w.show();
            let _ = w.set_focus();
            let _ = app.emit("window-shown", ());
        }
    }
}

fn show_main(app: &AppHandle) {
    if let Some(w) = app.get_webview_window("main") {
        show_window_top_center(&w);
        let _ = w.show();
        let _ = w.set_focus();
        let _ = app.emit("window-shown", ());
    }
}

/// 浮层定位：屏幕顶部水平居中
fn show_window_top_center(w: &tauri::WebviewWindow) {
    if let (Ok(Some(monitor)), Ok(size)) = (w.current_monitor(), w.outer_size()) {
        let ms = monitor.size();
        let x = (ms.width.saturating_sub(size.width)) / 2;
        let y = (ms.height * 3 / 16).max(40);
        let _ = w.set_position(PhysicalPosition::new(x as i32, y as i32));
    }
}

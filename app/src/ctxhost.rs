//! 右键菜单宿主子进程：隔离 shell 扩展的崩溃/卡死风险。
//! 用法：MONIKA_CTX_PATH=<路径> ctxhost.exe   （argv[1] 作为手动调试兜底）
//! 退出码：0=未执行命令 1=执行了命令 2=参数错误 3=出错（原因见 MONIKA_CTX_STATUS 文件）
//! 选中"复制路径/在文件夹中显示"这类自定义菜单项时退出码仍为 1，具体动作通过
//! MONIKA_CTX_STATUS 文件里的 "action <名>" 行回传，由主进程执行。
#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

#[path = "ctxmenu.rs"]
mod ctxmenu;

fn main() {
    // 主进程经 MONIKA_CTX_PATH 环境变量传入；argv[1] 作为手动调试兜底
    let path = std::env::var("MONIKA_CTX_PATH")
        .ok()
        .filter(|p| !p.is_empty())
        .or_else(|| std::env::args().nth(1))
        .unwrap_or_default();
    if path.is_empty() {
        std::process::exit(2);
    }
    match ctxmenu::run_context_menu(&path) {
        Ok(acted) => std::process::exit(if acted { 1 } else { 0 }),
        Err(_) => std::process::exit(3),
    }
}

//! 自动定位会让右键菜单卡死/崩溃的第三方 shell 扩展。
//!
//! 思路：把注册表里登记的 ContextMenuHandlers 收成候选集（只留第三方，即 DLL 不在
//! `\Windows\` 下的），然后让 ctxhost 以"只跑到 QueryContextMenu、不弹菜单"的探测模式
//! 逐个试：在已确认要屏蔽的集合之上再屏蔽候选 c，如果菜单就能建完，说明 c 就是元凶。
//!
//! 命中的 CLSID 写进 `%LOCALAPPDATA%\MonikaSearch\menu_skip.txt`，之后 ctxhost 每次
//! 弹菜单都会在自己进程内屏蔽它们 —— 菜单仍是系统原生构建的（只少了那个坏扩展），
//! 也完全不动资源管理器。诊断只做一次，之后右键都是毫秒级。

use std::os::windows::process::CommandExt;
use std::process::Command;
use std::time::{Duration, Instant};

use windows::core::{PCWSTR, PWSTR};
use windows::Win32::System::Registry::{
    RegCloseKey, RegEnumKeyExW, RegOpenKeyExW, RegQueryValueExW, HKEY, HKEY_CLASSES_ROOT, KEY_READ,
};

use crate::ctxmenu::skip_file;

const CREATE_NO_WINDOW: u32 = 0x0800_0000;
const ERROR_SUCCESS: u32 = 0;
/// 单个探测进程的等待上限：超时即视为"这个扩展卡死"
const PROBE_TIMEOUT: Duration = Duration::from_secs(3);
/// 最多定位这么多个坏扩展，避免病态情况下后台跑太久
const MAX_BAD: usize = 5;

fn to_wide(s: &str) -> Vec<u16> {
    s.encode_utf16().chain(std::iter::once(0)).collect()
}

/// ctxhost 的状态文件是追加写入的，这里按行找标记（只匹配行首，避免
/// "error" 之类的字样出现在别的行里造成误判）。
pub fn status_has(status_file: &std::path::Path, prefix: &str) -> bool {
    match std::fs::read_to_string(status_file) {
        Ok(text) => text.lines().any(|l| l.starts_with(prefix)),
        Err(_) => false,
    }
}

/// HKCR 下某个键的默认值（REG_SZ）
fn default_value(path: &str) -> Option<String> {
    let p = to_wide(path);
    unsafe {
        let mut hkey = HKEY::default();
        if RegOpenKeyExW(HKEY_CLASSES_ROOT, PCWSTR(p.as_ptr()), None, KEY_READ, &mut hkey).0 != ERROR_SUCCESS {
            return None;
        }
        let mut buf = vec![0u16; 1024];
        let mut len: u32 = (buf.len() * 2) as u32;
        let r = RegQueryValueExW(
            hkey,
            PCWSTR::null(),
            None,
            None,
            Some(buf.as_mut_ptr() as *mut u8),
            Some(&mut len),
        );
        let _ = RegCloseKey(hkey);
        if r.0 != ERROR_SUCCESS {
            return None;
        }
        let n = ((len as usize) / 2).saturating_sub(1).min(buf.len());
        Some(String::from_utf16_lossy(&buf[..n]))
    }
}

/// 列出 `<base>` 下所有子键名及其默认值
fn subkey_values(base: &str) -> Vec<(String, String)> {
    let mut out = Vec::new();
    let p = to_wide(base);
    unsafe {
        let mut hkey = HKEY::default();
        if RegOpenKeyExW(HKEY_CLASSES_ROOT, PCWSTR(p.as_ptr()), None, KEY_READ, &mut hkey).0 != ERROR_SUCCESS {
            return out;
        }
        let mut idx = 0u32;
        loop {
            let mut name = vec![0u16; 512];
            let mut nlen = name.len() as u32;
            let r = RegEnumKeyExW(
                hkey,
                idx,
                Some(PWSTR(name.as_mut_ptr())),
                &mut nlen,
                None,
                None,
                None,
                None,
            );
            if r.0 != ERROR_SUCCESS {
                break;
            }
            idx += 1;
            let sub = String::from_utf16_lossy(&name[..nlen as usize]);
            if let Some(v) = default_value(&format!("{base}\\{sub}")) {
                let v = v.trim().to_string();
                if !v.is_empty() {
                    out.push((sub, v));
                }
            }
        }
        let _ = RegCloseKey(hkey);
    }
    out
}

/// 系统自带扩展不参与探测。
/// 判定依据是 InprocServer32 的 DLL 位置：Windows 自己的处理器要么直接写
/// `C:\Windows\...`，要么写 `%SystemRoot%\...`（REG_EXPAND_SZ，读出来是未展开的），
/// 所以两种写法都要认。没有 InprocServer32（外进程/未安装）的也跳过。
fn is_system_handler(clsid: &str) -> bool {
    match default_value(&format!("CLSID\\{clsid}\\InprocServer32")) {
        Some(dll) => {
            let d = dll.to_lowercase();
            d.is_empty()
                || d.contains("\\windows\\")
                || d.contains("%systemroot%")
                || d.contains("%windir%")
        }
        None => true,
    }
}

/// 该路径可能加载的第三方 ContextMenuHandler：(CLSID, 显示名)
pub fn candidates(path: &str) -> Vec<(String, String)> {
    let mut bases: Vec<String> = vec![
        "*\\shellex\\ContextMenuHandlers".into(),
        "AllFilesystemObjects\\shellex\\ContextMenuHandlers".into(),
        "Folder\\shellex\\ContextMenuHandlers".into(),
        "Directory\\shellex\\ContextMenuHandlers".into(),
        "Directory\\Background\\shellex\\ContextMenuHandlers".into(),
        "Drive\\shellex\\ContextMenuHandlers".into(),
    ];
    let p = std::path::Path::new(path);
    if p.is_file() {
        if let Some(ext) = p.extension().and_then(|e| e.to_str()) {
            bases.push(format!(".{ext}\\shellex\\ContextMenuHandlers"));
            bases.push(format!("SystemFileAssociations\\.{ext}\\shellex\\ContextMenuHandlers"));
            if let Some(progid) = default_value(&format!(".{ext}")) {
                let progid = progid.trim().to_string();
                if !progid.is_empty() && !progid.starts_with('.') {
                    bases.push(format!("{progid}\\shellex\\ContextMenuHandlers"));
                }
            }
        }
    }

    let mut seen: std::collections::BTreeMap<String, String> = std::collections::BTreeMap::new();
    for b in bases {
        for (name, clsid) in subkey_values(&b) {
            let up = clsid.trim_matches('"').trim().to_uppercase();
            if !up.starts_with('{') || !up.ends_with('}') {
                continue;
            }
            if is_system_handler(&up) {
                continue;
            }
            seen.entry(up).or_insert(name);
        }
    }
    seen.into_iter().collect()
}

/// 跑一次 ctxhost 探测：只到 QueryContextMenu，不弹菜单。返回菜单是否成功建完。
/// suppress_exclusive 保证探测只应用传入的集合，不叠加内置种子/跳过文件。
pub fn probe(path: &str, suppress: &[String]) -> bool {
    let stamp = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    let status_file = std::env::temp_dir().join(format!("monika_probe_{}_{stamp}.txt", std::process::id()));
    let _ = std::fs::remove_file(&status_file);

    let spawned = Command::new("ctxhost.exe")
        .env("MONIKA_CTX_PATH", path)
        .env("MONIKA_CTX_STEPS", "2")
        .env("MONIKA_CTX_SUPPRESS", suppress.join(","))
        .env("MONIKA_CTX_SUPPRESS_EXCLUSIVE", "1")
        .env("MONIKA_CTX_STATUS", &status_file)
        .creation_flags(CREATE_NO_WINDOW)
        .spawn();
    let mut child = match spawned {
        Ok(c) => c,
        Err(_) => return false,
    };

    let deadline = Instant::now() + PROBE_TIMEOUT;
    let mut ok = false;
    loop {
        if status_has(&status_file, "ready") {
            ok = true;
            break;
        }
        if status_has(&status_file, "error") {
            break;
        }
        match child.try_wait() {
            Ok(Some(_)) => break, // 提前退出：崩了
            Ok(None) => {}
            Err(_) => break,
        }
        if Instant::now() > deadline {
            break; // 卡死
        }
        std::thread::sleep(Duration::from_millis(25));
    }
    let _ = child.kill();
    let _ = std::fs::remove_file(&status_file);
    ok
}

pub struct Outcome {
    pub candidates: usize,
    pub bad: Vec<(String, String)>,
    pub note: String,
}

/// 逐候选探测，找出必须屏蔽的扩展。只做一次，结果持久化。
pub fn diagnose(path: &str) -> Outcome {
    let cands = candidates(path);
    if cands.is_empty() {
        return Outcome {
            candidates: 0,
            bad: Vec::new(),
            note: "没有发现第三方右键扩展".into(),
        };
    }

    let mut suppressed: Vec<String> = Vec::new();
    let mut bad: Vec<(String, String)> = Vec::new();

    // 关键：必须先确认"当前屏蔽集合还不够"，才继续找下一个元凶。
    // 否则元凶一旦被屏蔽，后面随便再屏蔽谁都能探测成功，会把无辜扩展也记进来。
    while bad.len() < MAX_BAD && !probe(path, &suppressed) {
        let mut found: Option<(String, String)> = None;
        for (clsid, name) in &cands {
            if suppressed.iter().any(|s| s == clsid) {
                continue;
            }
            let mut trial = suppressed.clone();
            trial.push(clsid.clone());
            if probe(path, &trial) {
                suppressed.push(clsid.clone());
                found = Some((clsid.clone(), name.clone()));
                break;
            }
        }
        match found {
            Some(b) => bad.push(b),
            None => break,
        }
    }

    let note = if bad.is_empty() {
        format!(
            "排查了 {} 个第三方扩展，没能定位到单个元凶（可能是多个扩展共同作用或系统处理器问题）",
            cands.len()
        )
    } else {
        format!("在 {} 个第三方扩展中定位到 {} 个会卡死菜单的扩展", cands.len(), bad.len())
    };
    Outcome {
        candidates: cands.len(),
        bad,
        note,
    }
}

/// 把新发现的坏扩展追加进跳过文件（幂等）
pub fn persist_bad(bad: &[(String, String)]) -> std::io::Result<()> {
    let Some(f) = skip_file() else {
        return Err(std::io::Error::new(std::io::ErrorKind::Other, "LOCALAPPDATA 未设置"));
    };
    if let Some(dir) = f.parent() {
        std::fs::create_dir_all(dir)?;
    }
    let mut text = if f.exists() {
        std::fs::read_to_string(&f)?
    } else {
        String::from(
            "# MonikaSearch 右键菜单跳过列表\n\
             # 下面这些 shell 扩展在 QueryContextMenu 阶段会卡死或崩溃，本程序弹出系统菜单时\n\
             # 只在【自己进程内】屏蔽它们（资源管理器不受影响）。删掉本文件即可重新自动检测。\n",
        )
    };
    for (clsid, name) in bad {
        if !text.to_uppercase().contains(&clsid.to_uppercase()) {
            text.push_str(&format!("{clsid}   # {name}\n"));
        }
    }
    std::fs::write(&f, text)
}

/// 当前跳过列表（含内置种子），给设置面板展示
pub fn current_skip() -> Vec<String> {
    let mut v: Vec<String> = crate::ctxmenu::builtin_skip().iter().map(|s| (*s).to_string()).collect();
    if let Some(f) = skip_file() {
        if let Ok(text) = std::fs::read_to_string(f) {
            for line in text.lines() {
                let l = line.split('#').next().unwrap_or("").trim();
                if l.is_empty() {
                    continue;
                }
                v.push(l.to_string());
            }
        }
    }
    // CLSID 大小写不敏感，按大写去重但保留首个写法
    let mut seen: std::collections::BTreeSet<String> = std::collections::BTreeSet::new();
    v.retain(|c| seen.insert(c.to_uppercase()));
    v.sort();
    v
}

/// 清空跳过文件（保留内置种子），下次右键会重新自动检测
pub fn clear_skip() -> std::io::Result<()> {
    if let Some(f) = skip_file() {
        if f.exists() {
            std::fs::remove_file(f)?;
        }
    }
    Ok(())
}

// ── 后台自动诊断 ──────────────────────────────────────────────────────────────

use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};

static DIAGNOSING: AtomicBool = AtomicBool::new(false);
static LAST_DIAG: AtomicU64 = AtomicU64::new(0);
/// 两次诊断的最小间隔，避免连续失败的右键把后台占满
const DIAG_COOLDOWN: u64 = 120;

/// 菜单弹不出来时在后台跑一次诊断，定位并记住该屏蔽的扩展。
/// 结果通过 `menu-diagnosis` 事件回给前端。
pub fn start_background_diagnosis(app: tauri::AppHandle, path: String) {
    use tauri::Emitter;

    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    if now.saturating_sub(LAST_DIAG.load(Ordering::SeqCst)) < DIAG_COOLDOWN {
        return;
    }
    if DIAGNOSING.swap(true, Ordering::SeqCst) {
        return;
    }
    LAST_DIAG.store(now, Ordering::SeqCst);

    let started = std::thread::Builder::new()
        .name("menu-diag".into())
        .spawn(move || {
            let outcome = diagnose(&path);
            let names: Vec<String> = outcome.bad.iter().map(|(c, n)| format!("{n} ({c})")).collect();
            if !outcome.bad.is_empty() {
                if let Err(e) = persist_bad(&outcome.bad) {
                    eprintln!("写入跳过列表失败: {e}");
                }
            }
            DIAGNOSING.store(false, Ordering::SeqCst);
            let _ = app.emit("menu-diagnosis", (names, outcome.note));
        });
    if started.is_err() {
        DIAGNOSING.store(false, Ordering::SeqCst);
    }
}


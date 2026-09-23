//! 应用状态：后台引擎线程（加载缓存/全量扫描/USN 监听）、配置读写。

use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, AtomicU8, Ordering};
use std::sync::mpsc::Sender;
use std::sync::{Arc, Mutex, RwLock};
use std::time::{Duration, Instant};

use ep_core::engine::Engine;
use ep_core::volume::{fixed_ntfs_volumes, Volume};
use serde::{Deserialize, Serialize};

pub const STATE_EMPTY: u8 = 0;
pub const STATE_SCANNING: u8 = 1;
pub const STATE_READY: u8 = 2;

#[derive(Clone, Serialize, Deserialize)]
pub struct Config {
    pub ai_endpoint: String,
    pub ai_key: String,
    pub ai_model: String,
    /// 兼容模式：右键用内置五项菜单，不加载任何第三方 shell 扩展。
    /// 默认【关】——完整系统菜单已可用（会卡死的扩展被自动屏蔽，
    /// 见 ctxmenu.rs / shellex.rs）。旧配置里显式写的 true 仍然生效。
    #[serde(default)]
    pub compat_menu: bool,
}

impl Default for Config {
    fn default() -> Self {
        Config {
            ai_endpoint: String::new(),
            ai_key: String::new(),
            ai_model: String::new(),
            compat_menu: false,
        }
    }
}

pub enum EngineMsg {
    Rebuild,
}

pub struct Status {
    pub state: AtomicU8,
    pub scanned: AtomicU64,
    pub last_error: Mutex<String>,
}

impl Status {
    pub fn new() -> Arc<Status> {
        Arc::new(Status {
            state: AtomicU8::new(STATE_EMPTY),
            scanned: AtomicU64::new(0),
            last_error: Mutex::new(String::new()),
        })
    }
}

pub struct AppState {
    pub engine: Arc<RwLock<Engine>>,
    pub status: Arc<Status>,
    pub tx: Sender<EngineMsg>,
    pub config: Mutex<Config>,
    pub data_dir: PathBuf,
    /// 扩展名 -> 图标 data URL 缓存
    pub icons: Mutex<std::collections::HashMap<String, String>>,
}

impl AppState {
    pub fn cache_path(&self) -> PathBuf {
        self.data_dir.join("index.bin")
    }
    pub fn config_path(&self) -> PathBuf {
        self.data_dir.join("config.json")
    }
}

/// 数据目录：%LOCALAPPDATA%\MonikaSearch；首次运行自动从 EverythingPlus 旧目录迁移
pub fn data_root() -> PathBuf {
    let base = std::env::var_os("LOCALAPPDATA")
        .map(PathBuf::from)
        .expect("LOCALAPPDATA 未设置");
    let new_dir = base.join("MonikaSearch");
    let old_dir = base.join("EverythingPlus");
    if !new_dir.exists() && old_dir.exists() {
        if std::fs::rename(&old_dir, &new_dir).is_err() {
            let _ = std::fs::create_dir_all(&new_dir);
            let _ = std::fs::copy(old_dir.join("index.bin"), new_dir.join("index.bin"));
            let _ = std::fs::copy(old_dir.join("config.json"), new_dir.join("config.json"));
        }
    }
    new_dir
}

pub fn load_config(path: &PathBuf) -> Config {
    let mut cfg: Config = std::fs::read_to_string(path)
        .ok()
        .and_then(|s| serde_json::from_str(&s).ok())
        .unwrap_or_default();
    // 环境变量优先（凭据不进源码）
    if let Ok(k) = std::env::var("MONIKA_AI_KEY").or_else(|_| std::env::var("DEEPSEEK_API_KEY")) {
        cfg.ai_key = k;
    }
    if let Ok(v) = std::env::var("MONIKA_AI_ENDPOINT") {
        cfg.ai_endpoint = v;
    }
    if let Ok(v) = std::env::var("MONIKA_AI_MODEL") {
        cfg.ai_model = v;
    }
    cfg
}

pub fn save_config(path: &PathBuf, cfg: &Config) -> std::io::Result<()> {
    if let Some(p) = path.parent() {
        std::fs::create_dir_all(p)?;
    }
    std::fs::write(path, serde_json::to_string_pretty(cfg).unwrap())
}

/// 引擎主循环：加载缓存（或全量扫描）→ 就绪 → USN 监听增量。
/// 收到 Rebuild 或 journal 失效时回到全量扫描。
pub fn run_engine(data_dir: PathBuf, engine: Arc<RwLock<Engine>>, status: Arc<Status>, rx: std::sync::mpsc::Receiver<EngineMsg>) {
    std::fs::create_dir_all(&data_dir).ok();
    let cache = data_dir.join("index.bin");
    loop {
        let letters = fixed_ntfs_volumes();
        if letters.is_empty() {
            *status.last_error.lock().unwrap() = "没有检测到 NTFS 固定卷".into();
            std::thread::sleep(Duration::from_secs(30));
            continue;
        }
        // 阶段一：加载缓存或全量扫描
        {
            let journals: Vec<(char, _)> = letters
                .iter()
                .filter_map(|&l| {
                    Volume::open(l)
                        .and_then(|v| v.query_journal().map(|j| (l, j)))
                        .ok()
                })
                .collect();
            let mut eg = engine.write().unwrap();
            let ok = eg.load_cache(&cache, Some(&journals));
            if !ok {
                status.state.store(STATE_SCANNING, Ordering::Relaxed);
                status.scanned.store(0, Ordering::Relaxed);
                for &l in &letters {
                    let st = status.clone();
                    let r = eg.scan_volume(l, &mut |n| {
                        st.scanned.store(n, Ordering::Relaxed);
                    });
                    if let Err(e) = r {
                        *status.last_error.lock().unwrap() = format!("卷 {l} 扫描失败: {e}");
                    }
                }
                let _ = eg.save_cache(&cache);
            }
        }
        status.state.store(STATE_READY, Ordering::Relaxed);
        *status.last_error.lock().unwrap() = String::new();

        // 阶段二：USN 增量监听
        let handles: Vec<(usize, Volume)> = {
            let eg = engine.read().unwrap();
            eg.volume_states()
                .iter()
                .enumerate()
                .filter_map(|(i, v)| Volume::open(v.letter).ok().map(|h| (i, h)))
                .collect()
        };
        let mut dirty = false;
        let mut last_save = Instant::now();
        'tail: loop {
            while let Ok(msg) = rx.try_recv() {
                match msg {
                    EngineMsg::Rebuild => {
                        let _ = engine.read().unwrap().save_cache(&cache);
                        status.state.store(STATE_SCANNING, Ordering::Relaxed);
                        let mut eg = engine.write().unwrap();
                        *eg = Engine::empty();
                        let _ = std::fs::remove_file(&cache);
                        break 'tail; // 回到外层全量扫描
                    }
                }
            }
            let mut applied = false;
            for (vi, vol) in &handles {
                let (id, start) = {
                    let eg = engine.read().unwrap();
                    let vs = &eg.volume_states()[*vi];
                    (vs.journal_id, vs.next_usn)
                };
                match vol.read_journal(id, start) {
                    Ok((next, recs)) => {
                        if !recs.is_empty() {
                            let mut eg = engine.write().unwrap();
                            eg.apply_usn_records(*vi as u8, recs);
                            eg.set_next_usn(*vi as u8, next);
                            applied = true;
                            dirty = true;
                        }
                    }
                    Err(e) => {
                        // journal 被删除/ID 变化 → 全量重建
                        *status.last_error.lock().unwrap() = format!("USN journal 失效: {e}");
                        break 'tail;
                    }
                }
            }
            if dirty && last_save.elapsed() > Duration::from_secs(300) {
                let _ = engine.read().unwrap().save_cache(&cache);
                dirty = false;
                last_save = Instant::now();
            }
            if !applied {
                std::thread::sleep(Duration::from_millis(600));
            }
        }
    }
}

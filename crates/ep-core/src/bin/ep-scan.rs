//! ep-scan：命令行验证工具——扫盘测速、搜索测试、缓存验证。

use ep_core::engine::{Engine, SearchQuery};
use ep_core::volume::{fixed_ntfs_volumes, Volume};
use std::path::PathBuf;
use std::time::Instant;

fn cache_path() -> PathBuf {
    let base = std::env::var_os("LOCALAPPDATA")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("."));
    let new_dir = base.join("MonikaSearch");
    let old_dir = base.join("EverythingPlus");
    if !new_dir.exists() && old_dir.exists() {
        let _ = std::fs::rename(&old_dir, &new_dir);
    }
    new_dir.join("index.bin")
}

fn main() {
    let letters = fixed_ntfs_volumes();
    println!("NTFS 固定卷: {letters:?}");
    if letters.is_empty() {
        eprintln!("没有找到 NTFS 固定卷");
        std::process::exit(1);
    }

    let cache = cache_path();
    // 查询当前各卷 journal 状态
    let journals: Vec<(char, _)> = letters
        .iter()
        .filter_map(|&l| Volume::open(l).and_then(|v| v.query_journal().map(|j| (l, j))).ok())
        .collect();

    let mut engine = Engine::empty();
    let t0 = Instant::now();
    if engine.load_cache(&cache, Some(&journals)) {
        println!("缓存加载成功: {:.2}s, {} 条目", t0.elapsed().as_secs_f64(), engine.stats().total_alive);
    } else {
        println!("缓存不可用，全量扫描…");
        for &l in &letters {
            let t = Instant::now();
            let mut last = 0u64;
            let r = engine.scan_volume(l, &mut |n| {
                if n - last >= 500_000 {
                    println!("  {l}: 已扫描 {n}");
                    last = n;
                }
            });
            match r {
                Ok(()) => println!("  卷 {l}: 扫描完成 {:.2}s", t.elapsed().as_secs_f64()),
                Err(e) => println!("  卷 {l}: 扫描失败: {e}"),
            }
        }
        let stats = engine.stats();
        println!("总条目: {} (目录 {}), 耗时 {:.2}s", stats.total_alive, stats.total_dirs, t0.elapsed().as_secs_f64());
        engine.save_cache(&cache).expect("保存缓存");
        println!("缓存已保存: {}", cache.display());
    }

    // 搜索测试
    for text in ["cargo", "desktop", "README", ".gitconfig"] {
        let t = Instant::now();
        let hits = engine.search(&SearchQuery {
            text: text.into(),
            name_only: true,
            limit: 5,
            ..Default::default()
        });
        println!("搜索 '{text}': {} 条(限5) {:.1}ms", hits.len(), t.elapsed().as_secs_f64() * 1000.0);
        for h in hits.iter().take(3) {
            println!("   {} [{}]", h.path, if h.last_write > 0 { fmt_local(h.last_write) } else { "—".into() });
        }
    }

    // 增量监听测试（10 秒）
    println!("监听 USN 增量 10 秒（可在期间创建/删除文件）…");
    for v in engine.volume_states().iter() {
        if let (Ok(h), Ok(j)) = (Volume::open(v.letter), Volume::open(v.letter).and_then(|h| h.query_journal())) {
            println!(
                "  [诊断] 卷 {}: 缓存next_usn={} 实时next_usn={} lowest_valid={} id_match={}",
                v.letter, v.next_usn, j.next_usn, j.lowest_valid_usn, v.journal_id == j.usn_journal_id
            );
        }
    }
    let mut vol_handles: Vec<(usize, Volume)> = Vec::new();
    for (i, v) in engine.volume_states().iter().enumerate() {
        if let Ok(h) = Volume::open(v.letter) {
            vol_handles.push((i, h));
        }
    }
    let t = Instant::now();
    let mut total_records = 0usize;
    while t.elapsed().as_secs() < 10 {
        let mut any = false;
        for (vi, h) in &vol_handles {
            let id = engine.volume_states()[*vi].journal_id;
            let start = engine.volume_states()[*vi].next_usn;
            match h.read_journal(id, start) {
                Ok((next, recs)) => {
                    if !recs.is_empty() {
                        any = true;
                        total_records += recs.len();
                        println!("  卷 {}: {} 条变更 (示例: {})", engine.volume_states()[*vi].letter, recs.len(), recs[0].name);
                        engine.apply_usn_records(*vi as u8, recs);
                        engine.set_next_usn(*vi as u8, next);
                    }
                }
                Err(e) => {
                    any = true;
                    println!("  卷 {} read_journal 错误: {e}", engine.volume_states()[*vi].letter);
                }
            }
        }
        if !any {
            std::thread::sleep(std::time::Duration::from_millis(500));
        }
    }
    println!("10 秒内增量记录: {total_records} 条");
    let stats = engine.stats();
    println!("最终条目: {}", stats.total_alive);
}

fn fmt_local(unix: i64) -> String {
    // 简单的 UTC+8 本地时间格式化
    let days = unix.div_euclid(86400);
    let secs = unix.rem_euclid(86400);
    // 从 1970-01-01 起的日期换算（足够测试用）
    let (y, m, d) = civil_from_days(days);
    format!("{y:04}-{m:02}-{d:02} {:02}:{:02}:{:02}", secs / 3600, (secs % 3600) / 60, secs % 60)
}

fn civil_from_days(z: i64) -> (i64, i64, i64) {
    let z = z + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36524 - doe / 146_096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    (if m <= 2 { y + 1 } else { y }, m, d)
}

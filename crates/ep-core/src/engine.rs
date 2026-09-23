//! 索引引擎：内存索引 + USN 实时增量 + 子串/路径/类型搜索。

use std::collections::HashMap;
use std::path::Path;

use rayon::prelude::*;
use serde::{Deserialize, Serialize};

use crate::volume::{
    RawRecord, Volume, UsnJournalData, USN_REASON_FILE_CREATE, USN_REASON_FILE_DELETE,
    USN_REASON_RENAME_NEW_NAME, USN_REASON_RENAME_OLD_NAME, FILE_ATTRIBUTE_DIRECTORY,
};

#[derive(Clone, Serialize, Deserialize)]
pub struct Entry {
    pub vol: u8,          // 盘符下标（对应 Engine.volumes）
    pub frn: u64,
    pub parent_frn: u64,
    pub name: String,     // 原始大小写的文件名
    pub is_dir: bool,
    pub last_write: i64,  // unix 秒
    pub path_lower: String, // 全小写完整路径（含盘符），搜索用
    #[serde(skip)]
    pub dead: bool,       // 墓碑：USN 删除后标记，避免索引位移
}

#[derive(Clone, Serialize, Deserialize)]
pub struct VolumeState {
    pub letter: char,
    pub root_frn: u64,
    pub journal_id: u64,
    pub next_usn: i64,
}

pub struct EngineStats {
    pub total_alive: u64,
    pub total_dirs: u64,
}

pub struct Engine {
    pub volumes: Vec<VolumeState>,
    pub entries: Vec<Entry>,
    /// (vol, frn) -> entries 下标
    by_frn: HashMap<(u8, u64), u32>,
    /// (vol, parent_frn) -> 子项 entries 下标
    children: HashMap<(u8, u64), Vec<u32>>,
}

const PATH_SER_VERSION: u32 = 1;

#[derive(Serialize, Deserialize)]
struct PersistData {
    version: u32,
    volumes: Vec<VolumeState>,
    entries: Vec<EntryLite>,
}

#[derive(Serialize, Deserialize)]
struct EntryLite {
    vol: u8,
    frn: u64,
    parent_frn: u64,
    name: String,
    is_dir: bool,
    last_write: i64,
}

/// 搜索查询
#[derive(Clone, Default, serde::Deserialize)]
#[serde(default)]
pub struct SearchQuery {
    pub text: String,               // 小写子串
    pub name_only: bool,            // 只匹配文件名段
    pub path_prefix: Option<String>,// 小写路径前缀（目录范围）
    pub exts: Vec<String>,          // 小写扩展名（不含点）
    pub kind: String,               // all | file | dir
    pub modified_within_days: Option<i64>,
    pub limit: usize,
}

#[derive(serde::Serialize, Clone)]
pub struct Hit {
    pub name: String,
    pub path: String,     // 原始大小写完整路径
    pub is_dir: bool,
    pub last_write: i64,
}

impl Engine {
    pub fn empty() -> Engine {
        Engine {
            volumes: Vec::new(),
            entries: Vec::new(),
            by_frn: HashMap::new(),
            children: HashMap::new(),
        }
    }

    /// 全量扫描一个卷。重复调用同一盘符会先清掉旧数据。
    pub fn scan_volume(&mut self, letter: char, progress: &mut dyn FnMut(u64)) -> std::io::Result<()> {
        let vol = Volume::open(letter)?;
        vol.ensure_journal().ok(); // 已存在会返回错误，忽略
        let journal: UsnJournalData = vol.query_journal()?;

        let vol_idx = match self.volumes.iter().position(|v| v.letter == letter) {
            Some(i) => {
                // 清空该卷旧数据（重建）
                for e in self.entries.iter_mut() {
                    if e.vol == i as u8 {
                        e.dead = true;
                    }
                }
                i
            }
            None => {
                self.volumes.push(VolumeState {
                    letter,
                    root_frn: 0,
                    journal_id: journal.usn_journal_id,
                    next_usn: journal.next_usn,
                });
                self.volumes.len() - 1
            }
        };

        let mut raw: Vec<(u64, u64, String, bool, i64)> = Vec::new();
        vol.enumerate_all(|r| {
            raw.push((r.frn, r.parent_frn, r.name, r.attrs & FILE_ATTRIBUTE_DIRECTORY != 0, r.ts_unix));
        })?;

        self.ingest_raw(vol_idx as u8, raw, progress);
        let root_frn = self.find_root_frn(vol_idx as u8);
        self.volumes[vol_idx].root_frn = root_frn;
        self.volumes[vol_idx].journal_id = journal.usn_journal_id;
        self.volumes[vol_idx].next_usn = journal.next_usn;
        self.rebuild_all_paths();
        // 扫描期间可能有新变更落在本轮 journal 游标之前，回退一点重新追
        Ok(())
    }

    fn ingest_raw(&mut self, vol: u8, raw: Vec<(u64, u64, String, bool, i64)>, progress: &mut dyn FnMut(u64)) {
        self.by_frn.reserve(raw.len());
        let mut count = 0u64;
        for (frn, parent_frn, name, is_dir, ts) in raw {
            let idx = self.entries.len() as u32;
            self.entries.push(Entry {
                vol, frn, parent_frn, name, is_dir, last_write: ts,
                path_lower: String::new(), dead: false,
            });
            self.by_frn.insert((vol, frn), idx);
            let ck = self.children.entry((vol, parent_frn)).or_default();
            ck.push(idx);
            count += 1;
            if count % 100_000 == 0 {
                progress(count);
            }
        }
        progress(count);
    }

    /// 找到卷根目录 FRN（父引用指向自身的那个目录记录）
    fn find_root_frn(&self, vol: u8) -> u64 {
        for e in &self.entries {
            if e.vol == vol && !e.dead && e.is_dir && e.parent_frn == e.frn {
                return e.frn;
            }
        }
        // 兜底：MFT 记录 5
        0x0005_0000_0000_0005
    }

    /// 重建全部 path_lower（初次扫描/加载缓存后调用）
    fn rebuild_all_paths(&mut self) {
        // 清空 children（可能含失效项），重建
        self.children.clear();
        for (i, e) in self.entries.iter().enumerate() {
            if !e.dead {
                self.children.entry((e.vol, e.parent_frn)).or_default().push(i as u32);
            }
        }
        for vi in 0..self.volumes.len() {
            let root_frn = self.volumes[vi].root_frn;
            if root_frn == 0 {
                continue;
            }
            let vol = vi as u8;
            let prefix_lower = format!("{}:\\", self.volumes[vi].letter).to_lowercase();
            // 根目录的孩子们起 DFS
            let kids = self.children.get(&(vol, root_frn)).cloned().unwrap_or_default();
            let mut stack: Vec<(u32, String)> = kids
                .into_iter()
                .map(|i| (i, prefix_lower.clone()))
                .collect();
            while let Some((idx, prefix)) = stack.pop() {
                let (name_lower, is_dir, parent_display) = {
                    let e = &self.entries[idx as usize];
                    (e.name.to_lowercase(), e.is_dir, e.parent_frn)
                };
                let _ = parent_display;
                let path = if prefix.ends_with('\\') {
                    format!("{prefix}{name_lower}")
                } else {
                    format!("{prefix}\\{name_lower}")
                };
                if is_dir {
                    if let Some(kids) = self.children.get(&(vol, self.entries[idx as usize].frn)) {
                        let child_prefix = format!("{path}\\");
                        for &k in kids {
                            stack.push((k, child_prefix.clone()));
                        }
                    }
                }
                self.entries[idx as usize].path_lower = path;
            }
        }
    }

    /// 从磁盘加载缓存。journals=Some 时严格校验（主程序）；None 时信任缓存
    /// （MCP 等无管理员权限的只读消费者，无法 query_journal）。
    /// 返回 false 表示缓存不可用（需要全量扫描）。
    pub fn load_cache(&mut self, path: &Path, journals: Option<&[(char, UsnJournalData)]>) -> bool {
        let Ok(bytes) = std::fs::read(path) else { return false };
        let Ok(data) = bincode::deserialize::<PersistData>(&bytes) else { return false };
        if data.version != PATH_SER_VERSION {
            return false;
        }
        if let Some(journals) = journals {
            // 校验每个卷的 journal 仍然有效：id 相同且 next_usn 未被覆盖
            for v in &data.volumes {
                let Some((_, j)) = journals.iter().find(|(l, _)| *l == v.letter) else {
                    return false;
                };
                if j.usn_journal_id != v.journal_id || v.next_usn < j.lowest_valid_usn {
                    return false;
                }
            }
        }
        self.volumes = data.volumes;
        self.entries = data
            .entries
            .into_iter()
            .map(|e| Entry {
                vol: e.vol, frn: e.frn, parent_frn: e.parent_frn, name: e.name,
                is_dir: e.is_dir, last_write: e.last_write, path_lower: String::new(),
                dead: false,
            })
            .collect();
        // 重建 by_frn 索引（children 由 rebuild_all_paths 重建）
        self.by_frn.clear();
        for (i, e) in self.entries.iter().enumerate() {
            self.by_frn.insert((e.vol, e.frn), i as u32);
        }
        self.rebuild_all_paths();
        true
    }

    pub fn save_cache(&self, path: &Path) -> std::io::Result<()> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let data = PersistData {
            version: PATH_SER_VERSION,
            volumes: self.volumes.clone(),
            entries: self
                .entries
                .iter()
                .filter(|e| !e.dead)
                .map(|e| EntryLite {
                    vol: e.vol, frn: e.frn, parent_frn: e.parent_frn, name: e.name.clone(),
                    is_dir: e.is_dir, last_write: e.last_write,
                })
                .collect(),
        };
        let bytes = bincode::serialize(&data).expect("bincode serialize");
        let tmp = path.with_extension("tmp");
        std::fs::write(&tmp, &bytes)?;
        std::fs::rename(&tmp, path)
    }

    pub fn volume_states(&self) -> &[VolumeState] {
        &self.volumes
    }

    /// 应用一段 USN 增量记录（来自 read_journal）
    pub fn apply_usn_records(&mut self, vol_idx: u8, records: Vec<RawRecord>) {
        for r in records {
            let is_dir = r.attrs & FILE_ATTRIBUTE_DIRECTORY != 0;
            let rename_old = r.reason & USN_REASON_RENAME_OLD_NAME != 0;
            let deleted = r.reason & USN_REASON_FILE_DELETE != 0 && !rename_old;
            let created_or_renamed = r.reason & (USN_REASON_FILE_CREATE | USN_REASON_RENAME_NEW_NAME) != 0;
            // 纯属性/时间戳变更：仅更新时间
            if !created_or_renamed && !deleted && !rename_old {
                if let Some(&idx) = self.by_frn.get(&(vol_idx, r.frn)) {
                    self.entries[idx as usize].last_write = r.ts_unix;
                }
                continue;
            }
            if deleted || rename_old {
                if let Some(&idx) = self.by_frn.get(&(vol_idx, r.frn)) {
                    let e = &self.entries[idx as usize];
                    let hit_old = e.parent_frn == r.parent_frn && e.name.eq_ignore_ascii_case(&r.name);
                    if hit_old || deleted {
                        self.remove_subtree(vol_idx, r.frn);
                    }
                }
                if rename_old {
                    // 旧名删除后等待 RENAME_NEW_NAME 记录重建
                    continue;
                }
                continue;
            }
            // created / renamed_new：插入或更新
            self.upsert(vol_idx, r.frn, r.parent_frn, r.name.clone(), is_dir, r.ts_unix);
        }
    }

    /// 更新 next_usn（tail 循环每轮调用）
    pub fn set_next_usn(&mut self, vol_idx: u8, next_usn: i64) {
        self.volumes[vol_idx as usize].next_usn = next_usn;
    }

    fn remove_subtree(&mut self, vol: u8, frn: u64) {
        let Some(&idx) = self.by_frn.get(&(vol, frn)) else { return };
        // 收集子树（迭代 DFS，借用结束后再改）
        let mut to_kill: Vec<u32> = Vec::new();
        let mut stack = vec![idx];
        while let Some(i) = stack.pop() {
            to_kill.push(i);
            let e = &self.entries[i as usize];
            if let Some(kids) = self.children.get(&(e.vol, e.frn)) {
                stack.extend(kids.iter().copied());
            }
        }
        for i in to_kill {
            let e = &mut self.entries[i as usize];
            e.dead = true;
            self.by_frn.remove(&(e.vol, e.frn));
            if let Some(v) = self.children.get_mut(&(e.vol, e.parent_frn)) {
                v.retain(|&x| x != i);
            }
        }
    }

    fn upsert(&mut self, vol: u8, frn: u64, parent_frn: u64, name: String, is_dir: bool, ts: i64) {
        if let Some(&idx) = self.by_frn.get(&(vol, frn)) {
            let e = &mut self.entries[idx as usize];
            let old_parent = e.parent_frn;
            let old_name_lower = e.name.to_lowercase();
            e.parent_frn = parent_frn;
            e.name = name;
            e.is_dir = is_dir;
            e.last_write = ts;
            e.dead = false;
            if old_parent != parent_frn {
                if let Some(v) = self.children.get_mut(&(vol, old_parent)) {
                    v.retain(|&x| x != idx);
                }
                self.children.entry((vol, parent_frn)).or_default().push(idx);
                self.rebuild_subtree_paths(vol, frn);
            } else {
                // 同目录改名：路径也要刷新
                let new_name_lower = e.name.to_lowercase();
                if new_name_lower != old_name_lower || e.path_lower.is_empty() {
                    self.rebuild_subtree_paths(vol, frn);
                }
            }
            return;
        }
        // 新建
        let idx = self.entries.len() as u32;
        self.entries.push(Entry {
            vol, frn, parent_frn, name, is_dir, last_write: ts,
            path_lower: String::new(), dead: false,
        });
        self.by_frn.insert((vol, frn), idx);
        self.children.entry((vol, parent_frn)).or_default().push(idx);
        self.rebuild_subtree_paths(vol, frn);
    }

    /// 重算某个 FRN 自身及全部后代的 path_lower
    fn rebuild_subtree_paths(&mut self, vol: u8, frn: u64) {
        let Some(&idx) = self.by_frn.get(&(vol, frn)) else { return };
        // vol 即 self.volumes 的下标；先把需要的状态拷出来避免交叉借用
        let vol_letter = self.volumes[vol as usize].letter;
        let root_frn = self.volumes[vol as usize].root_frn;
        let parent_frn = self.entries[idx as usize].parent_frn;
        let prefix_lower = if parent_frn == root_frn {
            format!("{vol_letter}:\\")
        } else if let Some(&p) = self.by_frn.get(&(vol, parent_frn)) {
            let pl = self.entries[p as usize].path_lower.clone();
            if pl.is_empty() {
                return; // 父链未就绪，等下一轮记录
            }
            pl
        } else {
            return; // 父不存在（或被过滤），不可达
        };
        let mut stack: Vec<(u32, String)> = vec![(idx, prefix_lower)];
        while let Some((i, prefix)) = stack.pop() {
            let (name_lower, is_dir, my_frn) = {
                let e = &self.entries[i as usize];
                (e.name.to_lowercase(), e.is_dir, e.frn)
            };
            let path = if prefix.ends_with('\\') {
                format!("{prefix}{name_lower}")
            } else {
                format!("{prefix}\\{name_lower}")
            };
            if is_dir {
                if let Some(kids) = self.children.get(&(vol, my_frn)) {
                    let child_prefix = format!("{path}\\");
                    for &k in kids {
                        stack.push((k, child_prefix.clone()));
                    }
                }
            }
            self.entries[i as usize].path_lower = path;
        }
    }

    pub fn stats(&self) -> EngineStats {
        let mut alive = 0u64;
        let mut dirs = 0u64;
        for e in &self.entries {
            if !e.dead {
                alive += 1;
                if e.is_dir {
                    dirs += 1;
                }
            }
        }
        EngineStats { total_alive: alive, total_dirs: dirs }
    }

    /// 原始大小写完整路径（按需重建，仅用于结果展示）
    pub fn display_path(&self, idx: u32) -> String {
        let vol = self.entries[idx as usize].vol as usize;
        let root_frn = self.volumes[vol].root_frn;
        let mut parts: Vec<&str> = Vec::new();
        let mut cur = idx as usize;
        loop {
            let e = &self.entries[cur];
            if e.frn == root_frn {
                break;
            }
            parts.push(&e.name);
            if e.parent_frn == root_frn {
                break;
            }
            match self.by_frn.get(&(e.vol, e.parent_frn)) {
                Some(&p) => cur = p as usize,
                None => break,
            }
        }
        let mut path = format!("{}:", self.volumes[vol].letter);
        for p in parts.iter().rev() {
            path.push('\\');
            path.push_str(p);
        }
        path
    }

    pub fn search(&self, q: &SearchQuery) -> Vec<Hit> {
        let text = q.text.to_lowercase();
        let exts: Vec<String> = q.exts.iter().map(|e| e.trim_start_matches('.').to_lowercase()).collect();
        let path_prefix_lower = q.path_prefix.as_ref().map(|p| p.to_lowercase());
        let cutoff = q.modified_within_days.map(|d| {
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|t| t.as_secs() as i64 - d * 86400)
                .unwrap_or(0)
        });
        let limit = if q.limit == 0 { 500 } else { q.limit };
        let empty = text.is_empty();

        let mut scored: Vec<(i32, u32)> = self
            .entries
            .par_iter()
            .enumerate()
            .filter(|(_, e)| !e.dead && !e.path_lower.is_empty())
            .filter(|(_, e)| match q.kind.as_str() {
                "file" => !e.is_dir,
                "dir" => e.is_dir,
                _ => true,
            })
            .filter(|(_, e)| {
                if let Some(pre) = &path_prefix_lower {
                    if !e.path_lower.starts_with(pre.as_str()) {
                        return false;
                    }
                }
                true
            })
            .filter(|(_, e)| {
                if exts.is_empty() {
                    return true;
                }
                if e.is_dir {
                    return false;
                }
                let name_lower = e.path_lower.rsplit('\\').next().unwrap_or("");
                match name_lower.rfind('.') {
                    Some(d) if d + 1 < name_lower.len() => {
                        exts.iter().any(|x| &name_lower[d + 1..] == x.as_str())
                    }
                    _ => false,
                }
            })
            .filter(|(_, e)| {
                if let Some(c) = cutoff {
                    if e.last_write < c {
                        return false;
                    }
                }
                true
            })
            .filter_map(|(i, e)| {
                if empty {
                    return Some((3, i as u32));
                }
                let name_seg = e.path_lower.rsplit('\\').next().unwrap_or("");
                if q.name_only {
                    if let Some(pos) = name_seg.find(&text) {
                        return Some((if pos == 0 { 0 } else { 1 }, i as u32));
                    }
                    return None;
                }
                if let Some(pos) = e.path_lower.find(&text) {
                    let score = if pos as usize >= e.path_lower.len() - name_seg.len()
                        && name_seg.starts_with(&text) { 0 }
                        else if e.path_lower.ends_with(name_seg) && name_seg.contains(&text) { 1 }
                        else { 2 };
                    return Some((score, i as u32));
                }
                None
            })
            .collect();

        // 排序：分数 → 路径短 → 时间新
        scored.sort_by(|a, b| {
            let ea = &self.entries[a.1 as usize];
            let eb = &self.entries[b.1 as usize];
            a.0.cmp(&b.0)
                .then(ea.path_lower.len().cmp(&eb.path_lower.len()))
                .then(eb.last_write.cmp(&ea.last_write))
        });
        scored.truncate(limit);

        scored
            .iter()
            .map(|&(_, i)| {
                let e = &self.entries[i as usize];
                Hit {
                    name: e.name.clone(),
                    path: self.display_path(i),
                    is_dir: e.is_dir,
                    last_write: e.last_write,
                }
            })
            .collect()
    }
}

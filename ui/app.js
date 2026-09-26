// EverythingPlus 前端逻辑
const invoke = window.__TAURI__.core.invoke;
const listen = window.__TAURI__.event.listen;

// ── 类型分组 ──
const TYPE_GROUPS = [
  { id: 'doc',    label: '文档',   exts: ['pdf','doc','docx','xls','xlsx','ppt','pptx','txt','md','csv','rtf','one','xmind'] },
  { id: 'img',    label: '图片',   exts: ['png','jpg','jpeg','gif','bmp','webp','svg','ico','tif','tiff','heic','psd'] },
  { id: 'video',  label: '视频',   exts: ['mp4','mkv','avi','mov','wmv','flv','webm','m4v','ts'] },
  { id: 'audio',  label: '音频',   exts: ['mp3','wav','flac','aac','ogg','m4a','wma','opus'] },
  { id: 'arch',   label: '压缩包', exts: ['zip','rar','7z','tar','gz','bz2','xz','iso'] },
  { id: 'code',   label: '代码',   exts: ['rs','py','js','ts','jsx','tsx','html','css','c','cpp','h','hpp','java','go','rb','php','sh','bat','ps1','json','xml','yaml','yml','toml','sql','vue','lua'] },
];

const ICONS = {
  doc: '📄', img: '🖼️', video: '🎬', audio: '🎵', arch: '🗜️', code: '📝',
  exe: '⚙️', folder: '📁',
};

const IMAGE_EXTS = new Set(['png','jpg','jpeg','gif','bmp','webp','ico','tif','tiff']);
const iconCache = { '__folder__': null };   // ext -> dataURL
const thumbCache = new Map();               // path -> dataURL（上限 300）

// ── 状态 ──
let hits = [];
let sel = -1;
let activeTypes = new Set();
let pathPrefix = null;   // { display, value }
let debounceTimer = null;
let engineReady = false;
let aiBusy = false;
// 搜索序号：命令已异步化，先发的可能后到，旧结果必须丢弃
let searchSeq = 0;

const $ = (id) => document.getElementById(id);
const input = $('q');
const resultsEl = $('results');
const chipsEl = $('type-chips');
const pathSlot = $('path-chip-slot');
const aiBanner = $('ai-banner');
const statusBar = $('status-bar');

function extOf(name) {
  const i = name.lastIndexOf('.');
  return i > 0 ? name.slice(i + 1).toLowerCase() : '';
}

function iconFor(hit) {
  if (hit.is_dir) return ICONS.folder;
  const ext = extOf(hit.name);
  for (const g of TYPE_GROUPS) if (g.exts.includes(ext)) return ICONS[g.id];
  if (['exe','msi','bat','lnk'].includes(ext)) return ICONS.exe;
  return '📃';
}

async function ensureIcons(list) {
  const exts = new Set();
  for (const h of list.slice(0, 200)) {
    if (h.is_dir) continue;
    const e = extOf(h.name) || '__none__';
    if (!(e in iconCache)) exts.add(e);
  }
  if (!iconCache['__folder__']) exts.add('__folder__');
  await Promise.all([...exts].map(async (e) => {
    try {
      const arg = e === '__folder__' ? '' : (e === '__none__' ? '__none__' : e);
      const url = await invoke('get_icon', { ext: arg });
      if (url) iconCache[e] = url;
    } catch (err) { /* ignore */ }
  }));
  return exts.size;
}

function rowIcon(hit) {
  if (hit.is_dir) return iconCache['__folder__'] ? `<img src="${iconCache['__folder__']}">` : ICONS.folder;
  const e = extOf(hit.name);
  if (IMAGE_EXTS.has(e) && thumbCache.has(hit.path)) {
    return `<img class="thumb" src="${thumbCache.get(hit.path)}">`;
  }
  const key = e || '__none__';
  if (iconCache[key]) return `<img src="${iconCache[key]}">`;
  return iconFor(hit);
}

async function loadThumbs(list, seq) {
  const targets = [];
  list.slice(0, 60).forEach((h, i) => {
    if (!h.is_dir && IMAGE_EXTS.has(extOf(h.name)) && !thumbCache.has(h.path)) targets.push(i);
  });
  if (!targets.length) return;
  // 并行取缩略图（后端也已线程池化），就绪一张就地替换一张，不再整表重渲染
  let next = 0;
  const workers = Array.from({ length: Math.min(6, targets.length) }, async () => {
    while (seq === searchSeq && next < targets.length) {
      const i = targets[next++];
      const h = list[i];
      if (thumbCache.size > 300) thumbCache.clear();
      try {
        const url = await invoke('image_thumb', { path: h.path });
        thumbCache.set(h.path, url || '');
        if (url && seq === searchSeq) patchRowIcon(i);
      } catch (err) { thumbCache.set(h.path, ''); }
    }
  });
  await Promise.all(workers);
}

function fmtDate(unix) {
  if (!unix || unix <= 0) return '';
  const d = new Date(unix * 1000);
  const p = (n) => String(n).padStart(2, '0');
  return `${d.getFullYear()}-${p(d.getMonth() + 1)}-${p(d.getDate())}`;
}

// ── AI 模式识别：以 ->ai 结尾 ──
function isAiMode() {
  return /->\s*ai\s*$/i.test(input.value);
}
function aiText() {
  return input.value.replace(/->\s*ai\s*$/i, '').trim();
}

// ── 搜索 ──
function buildQuery() {
  const q = {
    text: input.value.trim(),
    name_only: false,
    path_prefix: pathPrefix ? pathPrefix.value : null,
    exts: [...activeTypes].flatMap(id => TYPE_GROUPS.find(g => g.id === id).exts),
    kind: 'all',
    modified_within_days: null,
    limit: 300,
  };
  return q;
}

async function doSearch() {
  if (isAiMode() || aiBusy) { renderChips(); return; }
  const seq = ++searchSeq;
  const q = buildQuery();
  try {
    const r = await invoke('search', { q });
    if (seq !== searchSeq) return;
    hits = r;
    sel = hits.length ? 0 : -1;
    render();
    const fresh = await ensureIcons(hits);
    if (seq !== searchSeq) return;
    if (fresh > 0) patchAllIcons();
    loadThumbs(hits, seq);
  } catch (e) {
    console.error(e);
  }
  renderChips();
}

async function doAiSearch() {
  const text = aiText();
  if (!text) return;
  aiBusy = true;
  const seq = ++searchSeq;
  aiBanner.classList.remove('hidden', 'error');
  aiBanner.textContent = `⏳ AI 正在理解「${text}」…`;
  try {
    const r = await invoke('ai_search', { text });
    if (seq !== searchSeq) { aiBusy = false; return; }
    hits = r.hits;
    sel = hits.length ? 0 : -1;
    const kws = r.keywords.map(k => `<span class="kw">${esc(k)}</span>`).join('');
    const days = r.days ? `<span class="kw">近 ${r.days} 天</span>` : '';
    aiBanner.innerHTML = `🤖 ${esc(r.explain || '已转换为搜索条件')}${kws}${days}`;
    render();
    const fresh = await ensureIcons(hits);
    if (seq !== searchSeq) { aiBusy = false; return; }
    if (fresh > 0) patchAllIcons();
    loadThumbs(hits, seq);
  } catch (e) {
    if (seq !== searchSeq) { aiBusy = false; return; }
    aiBanner.classList.add('error');
    aiBanner.textContent = `⚠ ${e}`;
    hits = []; sel = -1;
    render();
  }
  aiBusy = false;
}

function esc(s) {
  return String(s).replace(/[&<>"]/g, c => ({ '&': '&amp;', '<': '&lt;', '>': '&gt;', '"': '&quot;' }[c]));
}

function highlight(name, text) {
  if (!text) return esc(name);
  const lower = name.toLowerCase();
  const idx = lower.indexOf(text.toLowerCase());
  if (idx < 0) return esc(name);
  return esc(name.slice(0, idx)) + '<b>' + esc(name.slice(idx, idx + text.length)) + '</b>' + esc(name.slice(idx + text.length));
}

// ── 渲染 ──
function render() {
  if (!hits.length) {
    const tip = engineReady ? '没有匹配结果' : '索引构建中，请稍候…';
    resultsEl.innerHTML = `<div class="empty-tip">${tip}</div>`;
    return;
  }
  const text = isAiMode() ? '' : input.value.trim();
  const frag = [];
  const max = Math.min(hits.length, 200);
  for (let i = 0; i < max; i++) {
    const h = hits[i];
    frag.push(`<div class="row${i === sel ? ' sel' : ''}" data-i="${i}">
      <span class="ico">${rowIcon(h)}</span>
      <span class="mid">
        <div class="name">${highlight(h.name, text)}</div>
        <div class="path">${esc(h.path)}</div>
      </span>
      <span class="date">${fmtDate(h.last_write)}</span>
    </div>`);
  }
  if (hits.length > max) frag.push(`<div class="empty-tip">已显示前 ${max} 条（共 ${hits.length} 条命中）</div>`);
  resultsEl.innerHTML = frag.join('');
}

function renderChips() {
  // 类型 chips
  const t = TYPE_GROUPS.map(g =>
    `<span class="chip${activeTypes.has(g.id) ? ' active' : ''}" data-type="${g.id}">${ICONS[g.id]} ${g.label}</span>`
  ).join('');
  chipsEl.innerHTML = t;
  // 路径 chip
  pathSlot.innerHTML = pathPrefix
    ? `<span class="chip path-chip" title="${esc(pathPrefix.value)}">📁 ${esc(pathPrefix.display)} <span class="x" data-clear="1">✕</span></span>`
    : '';
  // AI 徽标
  $('mode-badge').classList.toggle('hidden', !isAiMode());
}

function scrollSel() {
  const el = resultsEl.querySelector(`.row[data-i="${sel}"]`);
  if (el) el.scrollIntoView({ block: 'nearest' });
}

// 就地更新选中行：方向键不再整表 innerHTML 重建（200 行的重建是可感知的卡顿）
function setSel(i) {
  if (i === sel) { scrollSel(); return; }
  const prev = resultsEl.querySelector(`.row[data-i="${sel}"]`);
  if (prev) prev.classList.remove('sel');
  sel = i;
  const el = resultsEl.querySelector(`.row[data-i="${i}"]`);
  if (el) {
    el.classList.add('sel');
    el.scrollIntoView({ block: 'nearest' });
  }
}

// 图标缓存就绪后只替换 .ico 节点，不重建整表
function patchAllIcons() {
  for (const row of resultsEl.querySelectorAll('.row')) {
    const h = hits[+row.dataset.i];
    if (h) patchRowIcon(+row.dataset.i);
  }
}

function patchRowIcon(i) {
  const h = hits[i];
  const ico = resultsEl.querySelector(`.row[data-i="${i}"] .ico`);
  if (h && ico) ico.innerHTML = rowIcon(h);
}

// ── 事件 ──
input.addEventListener('input', () => {
  if (!isAiMode()) aiBanner.classList.add('hidden');
  clearTimeout(debounceTimer);
  debounceTimer = setTimeout(doSearch, 100);
});

input.addEventListener('keydown', (e) => {
  if (e.key === 'ArrowDown' || e.key === 'ArrowUp') {
    e.preventDefault();
    if (!hits.length) return;
    setSel(e.key === 'ArrowDown' ? Math.min(sel + 1, hits.length - 1) : Math.max(sel - 1, 0));
  } else if (e.key === 'Enter') {
    e.preventDefault();
    if (isAiMode()) { if (!aiBusy) doAiSearch(); return; }
    if (e.ctrlKey) {
      if (hits[sel]) invoke('reveal_path', { path: hits[sel].path });
    } else if (hits[sel]) {
      invoke('open_path', { path: hits[sel].path });
    }
  } else if (e.key === 'Tab') {
    e.preventDefault();
    if (hits[sel]) {
      const p = hits[sel].path;
      const dir = p.slice(0, p.lastIndexOf('\\'));
      if (dir) {
        pathPrefix = { display: dir.split('\\').pop() + '\\', value: dir + '\\' };
        doSearch();
      }
    }
  } else if (e.key === 'Escape') {
    if (input.value || pathPrefix || activeTypes.size) {
      input.value = '';
      pathPrefix = null;
      activeTypes.clear();
      aiBanner.classList.add('hidden');
      doSearch();
    } else {
      invoke('hide_window');
    }
  }
});

// chips 点击（事件委托）
$('chips').addEventListener('click', (e) => {
  const clear = e.target.closest('[data-clear]');
  if (clear) { pathPrefix = null; doSearch(); return; }
  const chip = e.target.closest('[data-type]');
  if (chip) {
    const id = chip.dataset.type;
    activeTypes.has(id) ? activeTypes.delete(id) : activeTypes.add(id);
    doSearch();
  }
});

// 结果点击
resultsEl.addEventListener('click', (e) => {
  const row = e.target.closest('.row');
  if (row) invoke('open_path', { path: hits[+row.dataset.i].path });
});
resultsEl.addEventListener('dblclick', (e) => {
  const row = e.target.closest('.row');
  if (row) invoke('reveal_path', { path: hits[+row.dataset.i].path });
});

// 右键：默认弹系统原生菜单（隔离子进程）；勾选兼容模式或宿主卡死/崩溃时回退内置菜单，
// 同时在后台自动排查是哪个第三方扩展的问题（见 menu-diagnosis 事件）。
let compatMenu = false; // 兼容模式缓存（get_config 后被真实值覆盖）

let menuNoteTimer = null;
function menuNote(text, isError) {
  const el = $('menu-note');
  el.classList.remove('hidden');
  el.classList.toggle('error', !!isError);
  el.innerHTML = text;
  clearTimeout(menuNoteTimer);
  menuNoteTimer = setTimeout(() => el.classList.add('hidden'), 12000);
}

function openContextMenuFor(path) {
  invoke('get_config').then((cfg) => {
    compatMenu = cfg.compat_menu !== false;
    if (compatMenu) {
      showFallbackMenu(path);
      return;
    }
    return invoke('show_context_menu', { path }).then((acted) => {
      if (acted) invoke('hide_window');
    }).catch(async (e) => {
      const msg = String(e);
      await showFallbackMenu(path);
      if (msg.includes('HOST_HANG')) {
        menuNote('⚠ 某个第三方右键扩展卡住了菜单，已用内置菜单代替。正在后台排查，稍后再右键即可用完整系统菜单。', true);
      } else if (msg.includes('HOST_CRASHED')) {
        menuNote('⚠ 某个第三方右键扩展让菜单崩溃，已用内置菜单代替。正在后台排查，稍后再右键即可用完整系统菜单。', true);
      } else {
        menuNote('⚠ 系统菜单不可用：' + esc(msg) + '（已用内置菜单代替）', true);
      }
    });
  }).catch(() => showFallbackMenu(path));
}

// 后台诊断结果：已定位并跳过导致卡死的扩展
listen('menu-diagnosis', (ev) => {
  const [names, note] = ev.payload || [];
  if (names && names.length) {
    menuNote('✅ 已定位并跳过会卡死右键菜单的扩展：' + names.map(esc).join('、') +
      '<br>再右键一次就能看到完整系统菜单了（资源管理器不受影响）。',
      false);
    renderMenuSkip();
  } else {
    menuNote('ℹ ' + esc(note || '未定位到具体扩展'), false);
  }
});

async function showFallbackMenu(path) {
  closeFallbackMenu();
  const menu = document.createElement('div');
  menu.id = 'fallback-menu';
  const items = [
    { icon: '📂', label: '打开', act: () => invoke('open_path', { path }).then(() => invoke('hide_window')) },
    { icon: '📁', label: '在文件夹中显示', act: () => invoke('reveal_path', { path }).then(() => invoke('hide_window')) },
    { icon: '📋', label: '复制路径', act: () => copyText(path) },
    { icon: 'ℹ️', label: '属性', act: () => invoke('show_properties', { path }) },
    { sep: true },
    { icon: '🗑️', label: '删除（移到回收站）', act: () => invoke('delete_path', { path }).then(() => doSearch()) },
  ];
  for (const it of items) {
    if (it.sep) {
      const hr = document.createElement('div');
      hr.className = 'fb-sep';
      menu.appendChild(hr);
      continue;
    }
    const row = document.createElement('div');
    row.className = 'fb-item';
    row.innerHTML = `<span class="fb-ico">${it.icon}</span>${it.label}`;
    row.addEventListener('click', () => {
      closeFallbackMenu();
      // 不要在用户面前静默失败（属性/定位这类失败以前是无声的）
      it.act().catch(e => menuNote('⚠ ' + it.label + '失败：' + esc(e), true));
    });
    menu.appendChild(row);
  }
  document.getElementById('app').appendChild(menu);
  // 关闭时机：点击菜单外任意处 / Esc
  setTimeout(() => {
    window.addEventListener('click', closeFallbackMenu, { once: true });
    window.addEventListener('blur', closeFallbackMenu, { once: true });
    input.addEventListener('keydown', function escClose(ev) {
      if (ev.key === 'Escape') { closeFallbackMenu(); input.removeEventListener('keydown', escClose); }
    });
  }, 0);
}

function closeFallbackMenu() {
  document.getElementById('fallback-menu')?.remove();
}

async function copyText(text) {
  const ta = document.createElement('textarea');
  ta.value = text;
  ta.style.position = 'fixed';
  ta.style.opacity = '0';
  document.body.appendChild(ta);
  ta.select();
  document.execCommand('copy');
  ta.remove();
}
resultsEl.addEventListener('contextmenu', (e) => {
  e.preventDefault();
  const row = e.target.closest('.row');
  if (!row) return;
  setSel(+row.dataset.i);
  openContextMenuFor(hits[sel].path);
});
// 键盘菜单键 / Shift+F10：对选中项弹菜单
input.addEventListener('keyup', (e) => {
  if (e.key === 'ContextMenu' || (e.key === 'F10' && e.shiftKey)) {
    e.preventDefault();
    if (hits[sel]) openContextMenuFor(hits[sel].path);
  }
});
document.addEventListener('contextmenu', (e) => {
  // 非 results 区域（如输入框）保留系统编辑菜单，其余禁用默认
  if (!resultsEl.contains(e.target)) e.preventDefault();
});

// ── 设置面板 ──
const settingsEl = $('settings');
$('gear').addEventListener('click', openSettings);

async function openSettings() {
  const cfg = await invoke('get_config');
  $('cfg-endpoint').value = cfg.ai_endpoint || '';
  $('cfg-key').value = cfg.ai_key || '';
  $('cfg-model').value = cfg.ai_model || '';
  try { $('cfg-autostart').checked = await invoke('get_autostart'); } catch (e) { /* ignore */ }
  $('cfg-compat').checked = cfg.compat_menu !== false;
  settingsEl.classList.remove('hidden');
  refreshStats();
  renderMenuSkip();
}

// ── 右键扩展跳过列表（自动诊断结果） ──
async function renderMenuSkip() {
  const el = $('cfg-menu-skip');
  if (!el) return;
  try {
    const list = await invoke('get_menu_skip');
    el.innerHTML = list.length
      ? '已跳过的右键扩展（只在本程序内跳过，资源管理器不受影响）：<br>' + list.map(c => esc(c)).join('<br>')
      : '没有跳过任何右键扩展。';
  } catch (e) {
    el.textContent = '';
  }
}

async function refreshStats() {
  const s = await invoke('get_status');
  const state = s.state === 'ready' ? '✅ 就绪' : s.state === 'scanning' ? '⏳ 索引构建中' : '…';
  $('cfg-stats').innerHTML =
    `索引状态：${state} ${s.error ? '（' + esc(s.error) + '）' : ''}<br>` +
    `文件总数：${s.total.toLocaleString()}（目录 ${s.dirs.toLocaleString()}）<br>` +
    `已收录卷：${(s.volumes || []).join(' ')}`;
  if (s.state === 'scanning') setTimeout(() => { if (!settingsEl.classList.contains('hidden')) refreshStats(); }, 1500);
}

$('btn-close-settings').addEventListener('click', async () => {
  const cfg = {
    ai_endpoint: $('cfg-endpoint').value.trim(),
    ai_key: $('cfg-key').value.trim(),
    ai_model: $('cfg-model').value.trim(),
    compat_menu: $('cfg-compat').checked,
  };
  compatMenu = cfg.compat_menu;
  await invoke('set_config', { cfg });
  settingsEl.classList.add('hidden');
});

$('cfg-autostart').addEventListener('change', async (e) => {
  try {
    await invoke('set_autostart', { enabled: e.target.checked });
  } catch (err) {
    alert('设置自启失败: ' + err);
    e.target.checked = !e.target.checked;
  }
});

$('btn-rebuild').addEventListener('click', async () => {
  await invoke('rebuild');
  engineReady = false;
  refreshStats();
});

$('btn-clear-skip').addEventListener('click', async () => {
  try {
    await invoke('clear_menu_skip');
    await renderMenuSkip();
    menuNote('已清空跳过列表。下次右键会重新自动检测（若又卡住会自动回退内置菜单）。', false);
  } catch (e) {
    menuNote('⚠ 清空失败：' + esc(e), true);
  }
});

// ── 状态轮询 ──
async function pollStatus() {
  try {
    const s = await invoke('get_status');
    const scanning = s.state === 'scanning';
    if (scanning) {
      statusBar.classList.remove('hidden');
      statusBar.innerHTML = `<span class="dot"></span>索引构建中… 已收录 ${s.scanned.toLocaleString()} 个文件` +
        (s.error ? ` <span style="color:var(--danger)">${esc(s.error)}</span>` : '');
    } else {
      statusBar.classList.add('hidden');
    }
    const nowReady = s.state === 'ready';
    if (nowReady && !engineReady) doSearch(); // 就绪瞬间刷新一次
    engineReady = nowReady;
  } catch (e) { /* ignore */ }
}
setInterval(pollStatus, 1500);
pollStatus();

// ── 窗口事件 ──
listen('window-shown', () => {
  input.focus();
  input.select();
  settingsEl.classList.add('hidden');
});
listen('open-settings', openSettings);

// 初始渲染
renderChips();
doSearch();

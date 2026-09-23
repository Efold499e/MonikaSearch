# MonikaSearch

后台常驻的极速文件搜索（Windows / NTFS）。Everything 的闭源替代：MFT 全量扫描 + USN Journal 实时增量 + AI 自然语言搜索。UI 主题：Monika 壁纸定制版。

## 特性

- **托盘常驻，开机自启**：索引一直在内存里实时更新，`Alt+E`（或 `Alt+Space`）呼出即搜，永不重新扫盘
- **自研索引引擎**：`FSCTL_ENUM_USN_DATA` 全量枚举 + `FSCTL_READ_USN_JOURNAL` 增量监听（参考 [ultrasearch](https://github.com/Dicklesworthstone/ultrasearch) / [usn-journal-rs](https://github.com/wangfu91/usn-journal-rs) 的思路，不依赖 Everything）
- **搜索方式**：文件名/路径子串；类型 chips（文档/图片/视频/音频/压缩包/代码）；Tab 限定到结果所在目录
- **预览**：结果列表显示系统真实文件类型图标；图片文件直接显示缩略图
- **AI 自然语言搜索**：输入 `上周的报账表格->ai`，回车后由任意 OpenAI 兼容 API 转结构化查询在本地索引执行（接口与密钥在设置里自行填写）
- **键盘优先**：↑↓ 选择 · Enter 打开 · Ctrl+Enter 资源管理器定位 · Esc 清空/隐藏
- **完整右键菜单（默认开启）**：右键即弹完整 Windows 原生上下文菜单（含第三方扩展，在独立宿主进程 `ctxhost.exe` 中隔离运行）；设置里的"右键兼容模式"可退回内置五项菜单（打开/在文件夹中显示/复制路径/属性/删除到回收站）
- **卡死扩展自动屏蔽**：网盘/社交客户端的 shell 扩展常在 `QueryContextMenu` 里同步等待自家主程序，主程序没运行时就永远不返回（如百度网盘 `YunShellExt`，对目录可 >130s 不返回），把整个菜单拖死。程序在**自己进程内**给这类扩展的 CLSID 注册一个 CreateInstance 直接失败的空 COM 工厂，shell 就会跳过它继续构建其余菜单项——菜单仍是系统原生结果（其他第三方扩展 + 全部系统动词），**资源管理器完全不受影响**。内置种子见 `ctxmenu.rs` 的 `BUILTIN_SKIP`；未知的坏扩展会在菜单弹不出来时**后台自动定位**（逐个候选探测），结果写进 `%LOCALAPPDATA%\MonikaSearch\menu_skip.txt`，设置面板可查看/清空重测
- **MCP 服务器**：内置 AI 调用接口，任何支持 MCP 的 AI 客户端（ZCode/Claude 等）可直接全盘搜索

## 结构

```
crates/ep-core   索引引擎（MFT/USN FFI、内存索引、搜索、持久化）
app              Tauri 2 应用（托盘、热键、命令、AI 对接）
ui               前端（原生 HTML/CSS/JS 下拉浮层）
```

## 开发

构建环境：

- Rust stable-msvc（rustup）
- MSVC 工具链：VS 2022 BuildTools（示例版本 `14.44.35207`）
- Windows SDK 10.0.26100：从 NuGet 官方包 `Microsoft.Windows.SDK.CPP*` 解压到任意目录（下文以 `D:\WinSDK-26100` 为例），并把注册表 `KitsRoot10` 指向它

Git Bash 下构建需把 MSVC bin 放 PATH 最前（避免 `/usr/bin/link` 抢占）：

```bash
export PATH="/d/WinSDK-26100/x-cpp/c/bin/10.0.26100.0/x64:/c/Program Files (x86)/Microsoft Visual Studio/2022/BuildTools/VC/Tools/MSVC/14.44.35207/bin/Hostx64/x64:$HOME/.cargo/bin:$PATH"
```

常用命令：

```bash
cargo build --release -p ep-core          # 引擎
cargo run --release -p ep-core --bin ep-scan  # 引擎真机验证（扫盘/搜索/增量监听）
cargo build --release -p monikasearch     # 应用 → target/release/monikasearch.exe
```

## 发布（制作发行包）

```bash
powershell -NoProfile -ExecutionPolicy Bypass -File scripts/make-dist.ps1            # 便携 zip + NSIS 安装包
powershell -NoProfile -ExecutionPolicy Bypass -File scripts/make-dist.ps1 -SkipNsis  # 只要便携 zip
```

产出：

- `dist/MonikaSearch-<版本>-portable.zip` —— 绿色便携版：`monikasearch.exe` +
  `ctxhost.exe`（右键菜单宿主，必须与主程序同目录）+ `ep-mcp.exe` + LICENSE +
  PRIVACY.md + `add-startmenu-shortcut.ps1`（双击/右键运行即建开始菜单快捷方式）
- `target/release/bundle/nsis/*.exe` —— NSIS 安装包（自动创建开始菜单快捷方式；
  需要 node/npm，首次会联网下载 tauri-cli 和 NSIS）

**分发须知**：版权与第三方组件条款见 `LICENSE`，数据流向与本地存储见 `PRIVACY.md`。
主题素材（Monika 壁纸/图标）为 DDLC 同人创作，按 Team Salvato 的 Fan Content
Guidelines **只能免费分发**，商业发行前必须整体替换素材并更名。

数据目录：`%LOCALAPPDATA%\MonikaSearch\`（旧 EverythingPlus 目录启动时自动迁移）。`config.json` 存 AI 配置（endpoint/key/model，key 也可用环境变量 `MONIKA_AI_KEY` / `DEEPSEEK_API_KEY` 覆盖，凭据不进源码）。索引缓存 `index.bin` 大小随文件总数增长（百万级文件约数百 MB）。性能量级：百万级文件全量扫描约半分钟，索引常驻内存后搜索毫秒级返回，USN 增量实时捕获。

## MCP 服务器（AI 调用）

`ep-mcp.exe` 是标准 MCP 服务器（stdio，newline-delimited JSON-RPC，协议 2024-11-05），加载索引缓存为快照，无需管理员权限。

工具：
- `search_files(query, name_only?, path_prefix?, exts?, kind?, modified_within_days?, limit?)` — 全盘搜索
- `get_index_status()` — 索引规模/卷
- `open_file(path)` / `reveal_file(path)` — 打开 / 资源管理器定位

ZCode 用户级配置（`~/.zcode/cli/config.json`）：

```json
"mcp": { "servers": { "monikasearch": {
  "command": "C:\\path\\to\\MonikaSearch\\ep-mcp.exe",
  "args": []
} } }
```

Claude Desktop / 其他客户端同理（command 指向 ep-mcp.exe）。索引为快照：想刷新结果，呼出一次 MonikaSearch 让 USN 增量跑一会儿即可（主程序常驻则始终实时）。

## 注意

- MFT/USN 访问需要管理员权限（exe 清单已声明 requireAdministrator）
- 仅索引固定 NTFS 卷；FAT32/exFAT/网络盘暂不支持（后续可加目录遍历兜底）
- v0.1 内存优化空间较大（每文件约存 name + path_lower 两个 String），百万级文件约 300-600MB，后续可换 arena/压缩存储

## Roadmap

- [ ] 内存占用优化（arena 存储 / 路径压缩；当前百万级文件约 1GB 量级）
- [ ] 文件大小过滤（USN 记录不含大小，需读 MFT $STANDARD_INFORMATION 或按需 stat）
- [ ] FAT/exFAT 目录遍历兜底
- [ ] AI 结果重排（相关性模型）
- [x] NSIS 安装包 + 便携 zip（`scripts/make-dist.ps1`；npm 走不通时加 `--registry=https://registry.npmmirror.com`，NSIS 工具包首次联网下载，之后缓存在 `%LOCALAPPDATA%\tauri\NSIS`）
- [ ] 安装为 Windows 服务（去掉登录 UAC）

//! ep-mcp：MonikaSearch 的 MCP 服务器，让 AI 直接调用本地文件索引。
//!
//! 传输：stdio，newline-delimited JSON-RPC 2.0（MCP 规范 2024-11-05）。
//! 启动时加载 %LOCALAPPDATA%\MonikaSearch\index.bin 索引缓存（需先运行过
//! 主程序一次）；无管理员权限要求，索引为加载时刻的快照。
//!
//! 工具：
//!   search_files      — 关键词/路径/扩展名/时间/类型 过滤搜索
//!   get_index_status  — 索引规模与卷
//!   open_file         — 用系统关联程序打开文件
//!   reveal_file       — 在资源管理器中定位文件

use std::io::{BufRead, Write};
use std::path::PathBuf;
use std::sync::Mutex;

use ep_core::engine::{Engine, SearchQuery};
use serde_json::{json, Value};

struct Ctx {
    engine: Mutex<Option<Engine>>,
    cache_path: PathBuf,
}

fn main() {
    let cache_path = std::env::var_os("LOCALAPPDATA")
        .map(PathBuf::from)
        .unwrap_or_default()
        .join("MonikaSearch")
        .join("index.bin");

    let mut engine_opt: Option<Engine> = None;
    let mut eng = Engine::empty();
    if eng.load_cache(&cache_path, None) {
        engine_opt = Some(eng);
    }

    let ctx = Ctx {
        engine: Mutex::new(engine_opt),
        cache_path,
    };

    let stdin = std::io::stdin();
    let stdout = std::io::stdout();
    let mut out = stdout.lock();
    for line in stdin.lock().lines() {
        let Ok(line) = line else { break };
        if line.trim().is_empty() {
            continue;
        }
        let Ok(msg) = serde_json::from_str::<Value>(&line) else {
            let _ = writeln!(out, "{}", json!({
                "jsonrpc": "2.0", "id": null,
                "error": {"code": -32700, "message": "Parse error"}
            }));
            let _ = out.flush();
            continue;
        };
        let id = msg.get("id").cloned().unwrap_or(Value::Null);
        let method = msg.get("method").and_then(|m| m.as_str()).unwrap_or("");
        if method.starts_with("notifications/") {
            continue; // 通知不回复
        }
        let params = msg.get("params").cloned().unwrap_or_else(|| json!({}));
        let response = match method {
            "initialize" => json!({
                "jsonrpc": "2.0", "id": id,
                "result": {
                    "protocolVersion": "2024-11-05",
                    "capabilities": {"tools": {}},
                    "serverInfo": {"name": "monikasearch", "version": "0.2.0"}
                }
            }),
            "ping" => json!({"jsonrpc": "2.0", "id": id, "result": {}}),
            "tools/list" => json!({"jsonrpc": "2.0", "id": id, "result": {"tools": tool_specs()}}),
            "tools/call" => {
                let name = params["name"].as_str().unwrap_or("").to_string();
                let args = params
                    .get("arguments")
                    .and_then(|v| v.as_object())
                    .map(|o| Value::Object(o.clone()))
                    .unwrap_or_else(|| json!({}));
                match call_tool(&ctx, &name, &args) {
                    Ok(text) => json!({
                        "jsonrpc": "2.0", "id": id,
                        "result": {"content": [{"type": "text", "text": text}], "isError": false}
                    }),
                    Err(e) => json!({
                        "jsonrpc": "2.0", "id": id,
                        "result": {"content": [{"type": "text", "text": e}], "isError": true}
                    }),
                }
            }
            _ => json!({
                "jsonrpc": "2.0", "id": id,
                "error": {"code": -32601, "message": format!("Method not found: {method}")}
            }),
        };
        let _ = writeln!(out, "{response}");
        let _ = out.flush();
    }
}

fn tool_specs() -> Vec<Value> {
    vec![
        json!({
        "name": "search_files",
        "description": "在 Windows 全盘文件索引中搜索文件（数百万文件毫秒级）。支持文件名/完整路径子串匹配，可按扩展名组、路径范围、文件/文件夹类型、修改时间过滤。",
        "inputSchema": {
            "type": "object",
            "properties": {
                "query": {"type": "string", "description": "搜索关键词（匹配文件名或完整路径，大小写不敏感；空串=列出全部）"},
                "name_only": {"type": "boolean", "description": "true=只匹配文件名（默认 false，同时匹配路径）"},
                "path_prefix": {"type": "string", "description": "限定路径前缀，如 D:\\projects"},
                "exts": {"type": "array", "items": {"type": "string"}, "description": "扩展名过滤，如 [\"pdf\",\"docx\"]"},
                "kind": {"type": "string", "enum": ["all", "file", "dir"], "description": "只搜文件/只搜文件夹/全部（默认 all）"},
                "modified_within_days": {"type": "integer", "description": "只返回最近 N 天内修改的文件"},
                "limit": {"type": "integer", "description": "返回条数上限（默认 50，最大 500）"}
            },
            "required": ["query"]
        }
    }),
    json!({
        "name": "get_index_status",
        "description": "查询本地文件索引状态（文件总数、收录的磁盘卷、缓存路径）。",
        "inputSchema": {"type": "object", "properties": {}}
    }),
    json!({
        "name": "open_file",
        "description": "用 Windows 系统关联程序打开一个文件或文件夹。",
        "inputSchema": {
            "type": "object",
            "properties": {"path": {"type": "string", "description": "文件完整路径"}},
            "required": ["path"]
        }
    }),
    json!({
        "name": "reveal_file",
        "description": "在 Windows 资源管理器中定位并选中一个文件。",
        "inputSchema": {
            "type": "object",
            "properties": {"path": {"type": "string", "description": "文件完整路径"}},
            "required": ["path"]
        }
    }),
    ]
}

fn call_tool(ctx: &Ctx, name: &str, args: &Value) -> Result<String, String> {
    match name {
        "get_index_status" => {
            let eg = ctx.engine.lock().unwrap();
            match eg.as_ref() {
                Some(e) => {
                    let s = e.stats();
                    Ok(serde_json::to_string_pretty(&json!({
                        "total_files": s.total_alive,
                        "total_dirs": s.total_dirs,
                        "volumes": e.volume_states().iter().map(|v| v.letter.to_string()).collect::<Vec<_>>(),
                        "cache_path": ctx.cache_path,
                        "note": "索引为上次主程序运行时的快照"
                    })).unwrap())
                }
                None => Err("索引缓存不可用：请先运行一次 MonikaSearch 主程序完成首次扫描".into()),
            }
        }
        "search_files" => {
            let query = args["query"].as_str().unwrap_or("").to_string();
            let limit = args["limit"].as_u64().unwrap_or(50).clamp(1, 500) as usize;
            let eg = ctx.engine.lock().unwrap();
            let Some(e) = eg.as_ref() else {
                return Err("索引缓存不可用：请先运行一次 MonikaSearch 主程序完成首次扫描".into());
            };
            let q = SearchQuery {
                text: query,
                name_only: args["name_only"].as_bool().unwrap_or(false),
                path_prefix: args["path_prefix"].as_str().map(String::from),
                exts: args["exts"]
                    .as_array()
                    .map(|a| a.iter().filter_map(|v| v.as_str().map(String::from)).collect())
                    .unwrap_or_default(),
                kind: args["kind"].as_str().unwrap_or("all").to_string(),
                modified_within_days: args["modified_within_days"].as_i64(),
                limit,
            };
            let hits = e.search(&q);
            Ok(serde_json::to_string_pretty(&json!({
                "count": hits.len(),
                "results": hits,
            })).unwrap())
        }
        "open_file" => {
            let path = args["path"].as_str().ok_or("缺少 path 参数")?;
            std::process::Command::new("explorer.exe")
                .arg(path)
                .spawn()
                .map_err(|e| format!("打开失败: {e}"))?;
            Ok("已用系统默认程序打开".into())
        }
        "reveal_file" => {
            let path = args["path"].as_str().ok_or("缺少 path 参数")?;
            let dir = std::path::Path::new(path)
                .parent()
                .map(|p| p.to_path_buf())
                .ok_or("无父目录")?;
            std::process::Command::new("explorer.exe")
                .arg(format!("/select,\"{}\"", path.replace('/', "\\")))
                .spawn()
                .map_err(|e| format!("定位失败: {e}"))?;
            let _ = dir;
            Ok("已在资源管理器中定位".into())
        }
        _ => Err(format!("未知工具: {name}")),
    }
}

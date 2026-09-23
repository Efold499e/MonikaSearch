//! AI 自然语言搜索：把 "xxx->ai" 交给 OpenAI 兼容 API，转结构化查询后在本地索引执行。

use serde_json::{json, Value};

use ep_core::engine::{Engine, Hit, SearchQuery};

use crate::state::Config;

const SYSTEM_PROMPT: &str = r#"你是 Windows 文件搜索助手。把用户的自然语言搜索意图转换为 JSON 查询。
只输出一个 JSON 对象，不要输出任何其他文字或代码块标记。字段：
{
  "keywords": ["文件名关键词数组，小写，通常1-3个，可为空数组"],
  "name_only": true,
  "path_contains": "路径中应包含的目录名小写，或 null",
  "exts": ["扩展名数组（不含点），如 [\"pdf\",\"docx\"]，或空数组"],
  "kind": "file 或 dir 或 all",
  "modified_within_days": 30,
  "explain": "一句话中文解释你的理解"
}
不确定的字段用空值（null / 空数组 / all / 0）。"#;

#[derive(serde::Serialize)]
pub struct AiResult {
    pub explain: String,
    pub hits: Vec<Hit>,
    pub keywords: Vec<String>,
    pub exts: Vec<String>,
    pub days: Option<i64>,
}

pub fn run_ai_search(cfg: &Config, text: &str, engine: &Engine) -> Result<AiResult, String> {
    if cfg.ai_endpoint.trim().is_empty() || cfg.ai_key.trim().is_empty() {
        return Err("AI 未配置：点击右下角 ⚙ 填写 API 地址和密钥".into());
    }
    let model = if cfg.ai_model.trim().is_empty() { "gpt-4o-mini".to_string() } else { cfg.ai_model.clone() };

    let body = json!({
        "model": model,
        "temperature": 0,
        "messages": [
            {"role": "system", "content": SYSTEM_PROMPT},
            {"role": "user", "content": format!("用户想找的文件：{text}")}
        ]
    });

    let resp: Value = ureq::post(cfg.ai_endpoint.trim())
        .timeout(std::time::Duration::from_secs(45))
        .set("Authorization", format!("Bearer {}", cfg.ai_key.trim()).as_str())
        .set("Content-Type", "application/json")
        .send_json(body)
        .map_err(|e| format!("AI 请求失败: {e}"))?
        .into_json()
        .map_err(|e| format!("AI 响应解析失败: {e}"))?;

    let content = resp["choices"][0]["message"]["content"]
        .as_str()
        .ok_or("AI 响应缺少 content")?
        .to_string();

    let parsed = extract_json(&content).ok_or("AI 未返回有效 JSON")?;

    let keywords: Vec<String> = parsed["keywords"]
        .as_array()
        .map(|a| a.iter().filter_map(|v| v.as_str().map(String::from)).collect())
        .unwrap_or_default();
    let exts: Vec<String> = parsed["exts"]
        .as_array()
        .map(|a| a.iter().filter_map(|v| v.as_str().map(String::from)).collect())
        .unwrap_or_default();
    let days = parsed["modified_within_days"].as_i64().filter(|&d| d > 0);
    let kind = parsed["kind"].as_str().unwrap_or("all").to_string();
    let path_contains = parsed["path_contains"].as_str().map(String::from);
    let name_only = parsed["name_only"].as_bool().unwrap_or(true);
    let explain = parsed["explain"].as_str().unwrap_or("").to_string();

    // 多关键词：逐个搜索合并，命中越多排越前
    let mut merged: Vec<(Hit, u8)> = Vec::new();
    let effective: Vec<String> = if keywords.is_empty() {
        vec![text.to_lowercase()]
    } else {
        keywords.iter().map(|k| k.to_lowercase()).collect()
    };
    for kw in &effective {
        let hits = engine.search(&SearchQuery {
            text: kw.clone(),
            name_only,
            path_prefix: path_contains.clone(),
            exts: exts.clone(),
            kind: kind.clone(),
            modified_within_days: days,
            limit: 300,
        });
        for h in hits {
            if let Some(slot) = merged.iter_mut().find(|(x, _)| x.path == h.path) {
                slot.1 += 1;
            } else {
                merged.push((h, 1));
            }
        }
    }
    merged.sort_by(|a, b| b.1.cmp(&a.1).then(a.0.path.len().cmp(&b.0.path.len())));
    merged.truncate(300);

    Ok(AiResult {
        explain,
        keywords: effective,
        exts,
        days,
        hits: merged.into_iter().map(|(h, _)| h).collect(),
    })
}

/// 从模型回复中抠出 JSON 对象（容忍 ```json 包裹和前后废话）
fn extract_json(content: &str) -> Option<Value> {
    let trimmed = content.trim();
    if let Ok(v) = serde_json::from_str::<Value>(trimmed) {
        if v.is_object() {
            return Some(v);
        }
    }
    // 找第一个 { 到最后一个 }
    if let (Some(s), Some(e)) = (content.find('{'), content.rfind('}')) {
        if s < e {
            if let Ok(v) = serde_json::from_str(&content[s..=e]) {
                return Some(v);
            }
        }
    }
    None
}

use crate::tokens;
use crate::types::RequestFeatures;
use serde_json::Value;

pub struct Extracted {
    pub messages: Vec<Value>,
    pub tools_json: Option<String>,
    pub tool_count: usize,
    pub last_user_text: String,
    pub first_user_text: String,
    pub system_text: String,
}

pub fn extract(body: &Value) -> Extracted {
    let messages: Vec<Value> = body
        .get("messages")
        .and_then(|v| v.as_array())
        .cloned()
        .unwrap_or_default();

    let tools_json = body.get("tools").map(|t| t.to_string());
    let tool_count = body.get("tools").and_then(|t| t.as_array()).map(|a| a.len()).unwrap_or(0);

    let mut last_user_text = String::new();
    let mut first_user_text = String::new();
    let mut system_text = String::new();
    for m in &messages {
        let role = m.get("role").and_then(|r| r.as_str()).unwrap_or("");
        match role {
            "system" => {
                if let Some(t) = tokens::content_text(m) {
                    system_text.push_str(&t);
                }
            }
            "user" => {
                if let Some(t) = tokens::content_text(m) {
                    if first_user_text.is_empty() {
                        first_user_text = t.clone();
                    }
                    last_user_text = t;
                }
            }
            _ => {}
        }
    }

    Extracted { messages, tools_json, tool_count, last_user_text, first_user_text, system_text }
}

pub fn features(extracted: &Extracted, est_input_tokens: u64) -> RequestFeatures {
    let sample: String = extracted
        .last_user_text
        .chars()
        .chain(extracted.system_text.chars().take(2000))
        .collect();
    let total = sample.chars().count().max(1) as f32;
    let mut code = 0f32;
    let mut cjk = 0f32;
    for ch in sample.chars() {
        if tokens::is_code_char_pub(ch) {
            code += 1.0;
        }
        if tokens::is_cjk_pub(ch) {
            cjk += 1.0;
        }
    }
    // 视觉要求只看**最后一条 user 消息**（2026-10-18 用户裁决）：历史里的图
    // 不需要"识别"，只要求模型能处理文本上下文；对 text-only 模型转发时由
    // relay 把历史图片替换成占位文本（否则上游 400 content.type 非法）。
    let mut has_images = false;
    for m in &extracted.messages {
        if m.get("role").and_then(|r| r.as_str()) != Some("user") {
            continue;
        }
        has_images = false;
        if let Some(Value::Array(parts)) = m.get("content") {
            for p in parts {
                if p.get("type").and_then(|t| t.as_str()) == Some("image_url")
                    || p.get("type").and_then(|t| t.as_str()) == Some("image")
                {
                    has_images = true;
                }
            }
        }
    }
    let user_turns = extracted
        .messages
        .iter()
        .filter(|m| m.get("role").and_then(|r| r.as_str()) == Some("user"))
        .count();

    RequestFeatures {
        est_input_tokens,
        user_text_chars: extracted.last_user_text.chars().count(),
        code_density: (code / total).min(1.0),
        tool_count: extracted.tool_count,
        tool_ratio: (extracted.tool_count as f32 / 20.0).min(1.0),
        has_images,
        turn_count: user_turns,
        cjk_ratio: (cjk / total).min(1.0),
    }
}

/// 这一轮请求是不是**工具流程的延续**（上一轮的工具调用返回后继续跑），
/// 而不是一条新的用户输入。粘性复用只在这里成立（用户裁决 2026-10-15）：
/// 同一会话里用户再发一条新消息 = 新任务，必须重新判定——否则会继承上一条
/// 低难任务的判定与选型（质量风险）。
///
/// 兼容两种协议形态：
/// - OpenAI：末条 `{role:"tool", ...}`
/// - Anthropic：末条 `{role:"user", content:[{type:"tool_result",...}]}`
///   （工具返回在 Anthropic 里就是 user 角色，靠 content part 区分）
/// 末条若是纯文本 user = 用户新输入 → 不是延续；assistant（prefill/续写）也不算。
pub fn continues_tool_flow(messages: &[Value]) -> bool {
    let Some(last) = messages.last() else { return false };
    match last.get("role").and_then(|r| r.as_str()) {
        Some("tool") | Some("function") => true,
        Some("user") => match last.get("content") {
            Some(Value::Array(parts)) => parts.iter().any(|p| {
                p.get("type").and_then(|t| t.as_str()) == Some("tool_result")
            }),
            _ => false,
        },
        _ => false,
    }
}

#[cfg(test)]
mod tool_flow_tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn vision_requirement_looks_only_at_last_user_turn() {
        // 历史图不要求识别（2026-10-18）：最后一条 user 纯文本 → has_images=false
        let body = serde_json::json!({"messages":[
            {"role":"user","content":[
                {"type":"text","text":"看图说明"},
                {"type":"image_url","image_url":{"url":"data:x"}}
            ]},
            {"role":"assistant","content":"ok"},
            {"role":"user","content":"接着处理，别管那张图"}
        ]});
        let ex = extract(&body);
        assert!(!features(&ex, 100).has_images);

        // 最后一条 user 带图 → true
        let body2 = serde_json::json!({"messages":[
            {"role":"user","content":"先看这个"},
            {"role":"assistant","content":"ok"},
            {"role":"user","content":[
                {"type":"image_url","image_url":{"url":"data:x"}}
            ]}
        ]});
        let ex2 = extract(&body2);
        assert!(features(&ex2, 100).has_images);
    }

    #[test]
    fn detects_openai_tool_continuation() {
        let msgs = vec![
            json!({"role":"user","content":"跑个测试"}),
            json!({"role":"assistant","content":null,"tool_calls":[{"id":"1"}]}),
            json!({"role":"tool","tool_call_id":"1","content":"ok"}),
        ];
        assert!(continues_tool_flow(&msgs));
    }

    #[test]
    fn detects_anthropic_tool_result() {
        let msgs = vec![
            json!({"role":"user","content":"跑个测试"}),
            json!({"role":"assistant","content":[{"type":"tool_use","id":"1","name":"bash"}]}),
            json!({"role":"user","content":[{"type":"tool_result","tool_use_id":"1","content":"ok"}]}),
        ];
        assert!(continues_tool_flow(&msgs));
    }

    #[test]
    fn new_user_message_is_not_a_continuation() {
        let msgs = vec![
            json!({"role":"assistant","content":"done"}),
            json!({"role":"user","content":"再帮我做一件完全不同的事"}),
        ];
        assert!(!continues_tool_flow(&msgs));
        // 空消息列表 / assistant 结尾 / Anthropic 纯文本 user 同样不算
        assert!(!continues_tool_flow(&[]));
        assert!(!continues_tool_flow(&[json!({"role":"assistant","content":"x"})]));
        assert!(!continues_tool_flow(&[json!({"role":"user","content":"纯文本"})]));
    }
}

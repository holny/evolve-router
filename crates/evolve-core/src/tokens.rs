/// 中文字符的 token 系数（基础值）。实测两类偏差相反：
/// - 纯中文日常文本：真实约 **0.58 tokens/char**（旧 0.75 反而估高 ~1.3×）
/// - 大会话（大量代码/JSON/工具输出）：真实比估算**高 1.4–1.5×**
/// 所以单一常数无法同时校准；**按模型观测**修正（`ModelTelemetry::token_ratio`，
/// 由成功请求的 `prompt_tokens / est_tokens` 中位数得到）在引擎的硬过滤里生效。
pub const CJK_TOKENS_PER_CHAR: f32 = 0.75;
pub const LATIN_CHARS_PER_TOKEN: f32 = 4.0;
pub const CODE_CHARS_PER_TOKEN: f32 = 3.4;

pub fn estimate_text(text: &str) -> u64 {
    if text.is_empty() {
        return 0;
    }
    let mut cjk = 0usize;
    let mut code = 0usize;
    let mut other = 0usize;
    for ch in text.chars() {
        if is_cjk(ch) {
            cjk += 1;
        } else if is_code_char(ch) {
            code += 1;
        } else {
            other += 1;
        }
    }
    let est = cjk as f32 * CJK_TOKENS_PER_CHAR
        + code as f32 / CODE_CHARS_PER_TOKEN
        + other as f32 / LATIN_CHARS_PER_TOKEN;
    (est.max(1.0)) as u64
}

fn is_cjk(ch: char) -> bool {
    matches!(ch as u32,
        0x4E00..=0x9FFF | 0x3400..=0x4DBF | 0x3000..=0x303F | 0xFF00..=0xFFEF | 0x3040..=0x30FF | 0xAC00..=0xD7AF)
}

fn is_code_char(ch: char) -> bool {
    matches!(ch, '{' | '}' | '(' | ')' | '[' | ']' | ';' | '=' | '<' | '>' | '|' | '&' | '\\' | '/' | '*' | '+' | '-' | '#' | '!' | ':' | '.' | '"')
        || ch.is_ascii_digit()
}

pub fn estimate_messages(messages: &[serde_json::Value], tools_json: Option<&str>) -> u64 {
    let mut total: u64 = 0;
    for m in messages {
        total += 4;
        if let Some(content) = content_text(m) {
            total += estimate_text(&content);
        }
        if let Some(tcs) = m.get("tool_calls").and_then(|v| v.as_array()) {
            for tc in tcs {
                if let Some(args) = tc.get("function").and_then(|f| f.get("arguments")).and_then(|v| v.as_str()) {
                    total += estimate_text(args);
                }
            }
        }
    }
    if let Some(t) = tools_json {
        total += estimate_text(t);
    }
    total
}

pub fn content_text(message: &serde_json::Value) -> Option<String> {
    match message.get("content") {
        Some(serde_json::Value::String(s)) => Some(s.clone()),
        Some(serde_json::Value::Array(parts)) => {
            let mut out = String::new();
            for p in parts {
                if let Some(t) = p.get("text").and_then(|v| v.as_str()) {
                    out.push_str(t);
                }
            }
            if out.is_empty() { None } else { Some(out) }
        }
        _ => None,
    }
}

pub fn is_cjk_pub(ch: char) -> bool {
    is_cjk(ch)
}

pub fn is_code_char_pub(ch: char) -> bool {
    is_code_char(ch) || matches!(ch, ';' | '{' | '}' | '=')
}

pub fn tokens_band(tokens: u64) -> i32 {
    if tokens == 0 {
        return 0;
    }
    (64 - (tokens as f64).ln() as i32).max(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 基础系数（0.75/char）保持不变：纯中文实测约 0.58，基础值略偏保守；
    /// 代码/JSON 类内容的低估由**按模型观测**的 `token_ratio` 在校验时修正。
    #[test]
    fn cjk_base_ratio_stays_conservative() {
        let est = estimate_text(&"中".repeat(1000));
        assert!((700..=900).contains(&est), "cjk base est = {est}/1000");
        let en = estimate_text(&"a".repeat(4000));
        assert!((900..=1100).contains(&en), "latin est = {en}");
    }
}

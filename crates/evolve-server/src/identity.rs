//! Per-request client identity: which agent is calling and which session it
//! belongs to. Extraction order is configurable (custom header first), then
//! builtin logic derived from each agent's actual wire behavior:
//! - codex: `originator` header (codex_cli_rs…) + `session_id` header (uuid)
//! - claude code: UA `claude-cli/x.y.z` + body `metadata.user_id` tail after `_session_`
//! - opencode: UA `opencode/x.y.z` + `x-opencode-session-id`（v2.0.3x 原生）
//!   （部分定制客户端会改名该头，一并识别）；均缺时退前缀链重建
//! - pi: UA `pi/…` (no stable session header → hash fallback)
use axum::http::HeaderMap;
use serde_json::Value;

fn header_of(headers: &HeaderMap, name: &str) -> Option<String> {
    headers.get(name).and_then(|v| v.to_str().ok()).map(|s| s.to_string())
}

fn trunc(s: String, n: usize) -> String {
    s.chars().take(n).collect()
}

/// Canonical agent name from known User-Agent shapes, else raw UA truncated.
pub fn agent_from_ua(ua: &str) -> String {
    let l = ua.to_lowercase();
    if l.starts_with("claude-cli") || l.contains("claude-code") {
        "claude-code".into()
    } else if l.contains("codex") {
        "codex".into()
    } else if l.contains("opencode") {
        "opencode".into()
    } else if l.starts_with("pi/") || l.contains("earendil") {
        "pi".into()
    } else {
        trunc(ua.to_string(), 40)
    }
}

pub fn agent_identity(headers: &HeaderMap, agent_header: &str) -> String {
    if let Some(v) = header_of(headers, agent_header).filter(|v| !v.is_empty()) {
        return trunc(v, 40);
    }
    if let Some(o) = header_of(headers, "originator") {
        let l = o.to_lowercase();
        if l.contains("codex") {
            return "codex".into();
        }
        return trunc(o, 40);
    }
    match header_of(headers, "user-agent") {
        Some(ua) if !ua.is_empty() => agent_from_ua(&ua),
        _ => "-".into(),
    }
}

/// Session id from custom/known headers, then claude code's body metadata.
/// opencode 头族（2026-10-23 实证）：v2.0.3x 原生发 `x-opencode-session-id`
/// （带 -id 后缀）；部分定制客户端会将该头改名——三种写法一并识别。此前网关只认
/// `x-opencode-session`（不带 -id）→ 0/1708 条命中，全部退化成内容哈希兜底。
pub fn session_identity(
    headers: &HeaderMap,
    parsed: &Value,
    session_header: &str,
) -> Option<String> {
    if let Some(v) = header_of(headers, session_header).filter(|v| !v.is_empty()) {
        return Some(v);
    }
    if let Some(v) = header_of(headers, "session_id").filter(|v| !v.is_empty()) {
        return Some(v);
    }
    // opencode 头族：v2.0.3x 原生 / 定制改名 / 旧版原生
    for name in ["x-opencode-session-id", "x-opencode-session"] {
        if let Some(v) = header_of(headers, name).filter(|v| !v.is_empty()) {
            return Some(v);
        }
    }
    if let Some(v) = header_of(headers, "x-session-id")
        .or_else(|| header_of(headers, "x-session"))
        .filter(|v| !v.is_empty())
    {
        return Some(v);
    }
    // claude code: metadata.user_id = "user_<hash>_account_<uuid>_session_<uuid>"
    if let Some(uid) = parsed
        .get("metadata")
        .and_then(|m| m.get("user_id"))
        .and_then(|v| v.as_str())
        && let Some(pos) = uid.find("_session_")
    {
        let tail = &uid[pos + "_session_".len()..];
        if !tail.is_empty() {
            return Some(tail.to_string());
        }
    }
    None
}

// ── 会话前缀链重建（2026-10-23，零插件）────────────────────────────────
// 客户端不发会话头时（opencode 实测 0/1708 条带 x-opencode-session），利用
// LLM 对话的**追加式**特性重建会话身份：同一会话每轮请求携带全量历史，
// 上一轮的 messages 数组是这一轮的**前缀**（长度只增不减，直到 compaction
// 改写旧消息——那一刻前缀断链，视为新会话，可接受）。
// 匹配策略：按 (agent, 首条消息哈希) 粗筛 → 校验 messages[0..k] 的完整
// 前缀哈希 → 命中即同一会话。内存态（重启清零，重建只花一轮）。

use std::collections::HashMap;
use std::hash::{Hash, Hasher};
use std::sync::Mutex;

/// 链条在无后续请求时保留的时长（超过即剔除，防长尾泄漏）
const CHAIN_TTL_MS: u64 = 6 * 3600 * 1000;
/// 同一 (agent, 首条哈希) 下最多保留的候选链（正常恒为 1）
const CHAIN_CANDIDATES_CAP: usize = 4;
/// 全表容量上限（超出丢最旧）
const CHAIN_GLOBAL_CAP: usize = 512;

#[derive(Debug, Clone)]
struct ChainEntry {
    /// 上次请求时的 messages 条数
    msg_count: usize,
    /// 上次请求全量 messages 的序列化哈希（前缀校验基准）
    prefix_hash: u64,
    /// 该链的会话键（首个请求的内容哈希，跨轮次稳定）
    session_key: String,
    last_seen_ms: u64,
}

fn content_hash(v: &serde_json::Value) -> u64 {
    let mut h = std::collections::hash_map::DefaultHasher::new();
    // to_string 再喂哈希：Value 的 Hash 不保证字段序稳定（内部 BTree 化差异），
    // 序列化字节才是线上真实比较口径
    if let Ok(bytes) = serde_json::to_vec(v) {
        bytes.hash(&mut h);
    }
    h.finish()
}

fn chain_store() -> &'static Mutex<HashMap<(String, u64), Vec<ChainEntry>>> {
    static STORE: std::sync::LazyLock<Mutex<HashMap<(String, u64), Vec<ChainEntry>>>> =
        std::sync::LazyLock::new(|| Mutex::new(HashMap::new()));
    &STORE
}

/// 前缀链会话重建：命中返回既有键（并刷新链），未命中播种 fallback_key。
/// `messages` 为空/缺失时直接返回 fallback_key（不建链）。
pub fn chain_session(agent: &str, messages: &[serde_json::Value], fallback_key: &str) -> String {
    if messages.is_empty() {
        return fallback_key.to_string();
    }
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0);
    let first_hash = content_hash(&messages[0]);
    let full_hash = content_hash(&serde_json::Value::Array(messages.to_vec()));
    let m = messages.len();
    let mut store = chain_store().lock().expect("chain store poisoned");
    // 全表容量守卫：丢最旧
    if store.len() > CHAIN_GLOBAL_CAP {
        if let Some(oldest) = store
            .iter()
            .map(|(k, v)| (v.iter().map(|e| e.last_seen_ms).max().unwrap_or(0), k.clone()))
            .min_by_key(|(t, _)| *t)
            .map(|(_, k)| k)
        {
            store.remove(&oldest);
        }
    }
    let key = (agent.to_string(), first_hash);
    let entries = store.entry(key).or_default();
    entries.retain(|e| now.saturating_sub(e.last_seen_ms) < CHAIN_TTL_MS);
    // 前缀链命中：len 只增 + messages[0..count] 哈希一致
    let hit = entries
        .iter_mut()
        .filter(|e| e.msg_count <= m)
        .find(|e| {
            content_hash(&serde_json::Value::Array(messages[..e.msg_count].to_vec()))
                == e.prefix_hash
        });
    if let Some(e) = hit {
        let key = e.session_key.clone();
        e.msg_count = m;
        e.prefix_hash = full_hash;
        e.last_seen_ms = now;
        return key;
    }
    // 未命中：播种新链；同键候选超上限丢最旧
    if entries.len() >= CHAIN_CANDIDATES_CAP {
        if let Some(oldest) = entries.iter().min_by_key(|e| e.last_seen_ms).map(|e| e.session_key.clone()) {
            entries.retain(|e| e.session_key != oldest);
        }
    }
    entries.push(ChainEntry {
        msg_count: m,
        prefix_hash: full_hash,
        session_key: fallback_key.to_string(),
        last_seen_ms: now,
    });
    fallback_key.to_string()
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn h(pairs: &[(&str, &str)]) -> HeaderMap {
        let mut m = HeaderMap::new();
        for (k, v) in pairs {
            m.insert(
                axum::http::HeaderName::from_bytes(k.as_bytes()).unwrap(),
                v.parse().unwrap(),
            );
        }
        m
    }

    #[test]
    fn codex_via_originator_and_session_header() {
        let headers = h(&[("originator", "codex_cli_rs"), ("session_id", "abc-123")]);
        assert_eq!(agent_identity(&headers, "x-ev-client"), "codex");
        assert_eq!(
            session_identity(&headers, &json!({}), "x-ev-session"),
            Some("abc-123".to_string())
        );
    }

    /// opencode 头族（2026-10-23 实证）：v2.0.3x 发 `x-opencode-session-id`、
    /// 定制改名头（x-*-session 族）照常识别——此前只认 `x-opencode-session`
    /// 导致 0/1708 条命中
    #[test]
    fn opencode_session_header_family() {
        let v2 = h(&[("x-opencode-session-id", "ses_abc123")]);
        assert_eq!(session_identity(&v2, &json!({}), "x-ev-session"), Some("ses_abc123".into()));
        let legacy = h(&[("x-opencode-session", "ses_old")]);
        assert_eq!(session_identity(&legacy, &json!({}), "x-ev-session"), Some("ses_old".into()));
        // 自定义头优先级最高
        let custom = h(&[("x-ev-session", "ses_custom"), ("x-opencode-session-id", "ses_native")]);
        assert_eq!(session_identity(&custom, &json!({}), "x-ev-session"), Some("ses_custom".into()));
    }

    /// 前缀链重建（零插件兜底）：同一会话每轮全量历史追加 → 上一轮是本轮前缀
    #[test]
    fn chain_session_tracks_appending_conversation() {
        let m1 = json!([{"role":"user","content":"hello"}]);
        let m2 = json!([{"role":"user","content":"hello"},{"role":"assistant","content":"hi"},{"role":"user","content":"more"}]);
        let m3 = json!([{"role":"user","content":"hello"},{"role":"assistant","content":"hi"},{"role":"user","content":"more"},{"role":"assistant","content":"ok"},{"role":"user","content":"again"}]);
        let k1 = chain_session("opencode", m1.as_array().unwrap(), "h_seed");
        assert_eq!(k1, "h_seed", "first request seeds the chain");
        let k2 = chain_session("opencode", m2.as_array().unwrap(), "h_other");
        assert_eq!(k2, "h_seed", "appended history matches the prefix chain");
        let k3 = chain_session("opencode", m3.as_array().unwrap(), "h_other");
        assert_eq!(k3, "h_seed", "chain keeps tracking as history grows");
        // 不同首条消息 → 另一条链
        let other = json!([{"role":"user","content":"different project"}]);
        let k4 = chain_session("opencode", other.as_array().unwrap(), "h_new");
        assert_eq!(k4, "h_new");
        // agent 隔离：同首条消息不同 agent 不串链
        let ka = chain_session("claude-code", m1.as_array().unwrap(), "h_cc");
        assert_eq!(ka, "h_cc", "same content but different agent = separate chain");
    }

    /// compaction（改写旧消息）断链 → 新会话键（诚实：历史已不可比）
    #[test]
    fn chain_session_breaks_on_compaction() {
        let m1 = json!([{"role":"user","content":"a"},{"role":"assistant","content":"b"},{"role":"user","content":"c"}]);
        let k1 = chain_session("opencode", m1.as_array().unwrap(), "h_1");
        // 压缩后首条消息被改写（摘要替换）→ 前缀与首条哈希全断
        let m2 = json!([{"role":"user","content":"[compacted summary]"},{"role":"user","content":"c"},{"role":"user","content":"d"}]);
        let k2 = chain_session("opencode", m2.as_array().unwrap(), "h_2");
        assert_ne!(k1, k2, "compaction rewrites history → new chain");
    }

    #[test]
    fn claude_code_via_ua_and_body_metadata() {
        let headers = h(&[("user-agent", "claude-cli/2.1.0 (external, cli)")]);
        assert_eq!(agent_identity(&headers, "x-ev-client"), "claude-code");
        let body = json!({"metadata": {"user_id": "user_abc123_account_def-uuid_session_9f8e7d6c"}});
        assert_eq!(
            session_identity(&headers, &body, "x-ev-session"),
            Some("9f8e7d6c".into())
        );
    }

    #[test]
    fn opencode_ua_and_custom_session_header() {
        let headers = h(&[("user-agent", "opencode/1.2.3"), ("x-ev-session", "ses_x")]);
        assert_eq!(agent_identity(&headers, "x-ev-client"), "opencode");
        assert_eq!(
            session_identity(&headers, &json!({}), "x-ev-session"),
            Some("ses_x".into())
        );
    }

    #[test]
    fn custom_header_overrides_builtin() {
        let headers = h(&[("x-my-sess", "custom-1"), ("session_id", "codex-2")]);
        assert_eq!(
            session_identity(&headers, &json!({}), "x-my-sess"),
            Some("custom-1".into())
        );
        assert_eq!(
            session_identity(&headers, &json!({}), "x-ev-session"),
            Some("codex-2".into())
        );
    }

    #[test]
    fn unknown_ua_passes_through() {
        let headers = h(&[("user-agent", "curl/8.4.0")]);
        assert_eq!(agent_identity(&headers, "x-ev-client"), "curl/8.4.0");
        assert_eq!(session_identity(&headers, &json!({}), "x-ev-session"), None);
    }
}

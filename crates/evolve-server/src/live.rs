//! 在飞请求登记（含正在进行的 SSE 流）——面板/网页"重启前提示"的依据。
//!
//! 语义要点（事故 a301ad5 的后续）：`inflight` 必须覆盖**流的整个生命期**，
//! 而不是"返回响应头之前"——否则重启提示永远显示 0 个在飞，等于没有保护。
//! 除计数外还登记元信息（模型/会话/开始时刻），让提示能说清"是谁在飞"。

use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use std::sync::{LazyLock, Mutex};

/// 在飞请求数（/api/live 的 `inflight`）
pub static LIVE_INFLIGHT: AtomicUsize = AtomicUsize::new(0);

static REGISTRY: LazyLock<Mutex<HashMap<u64, Entry>>> =
    LazyLock::new(|| Mutex::new(HashMap::new()));
static SEQ: AtomicU64 = AtomicU64::new(1);

struct Entry {
    model: String,
    session: String,
    agent: String,
    started_ms: u64,
}

/// 在飞令牌：创建即计数，Drop 即注销（流式响应由流持有 → 直到流结束）
pub struct Guard {
    id: Option<u64>,
    /// 令牌创建时刻（请求真正开始），describe 时作为 age 起点
    created_ms: u64,
}

impl Guard {
    /// 仅计数（请求刚进网关：此时还不知道模型/会话）
    pub fn new() -> Self {
        LIVE_INFLIGHT.fetch_add(1, Ordering::Relaxed);
        Self { id: None, created_ms: now_ms() }
    }

    /// 补充/更新元信息（知道会话后调一次、选出模型后再调一次）。
    /// 让"在飞"提示不只是一个数字：能说清是谁在飞、飞了多久
    pub fn describe(&mut self, model: &str, session: &str, agent: &str) {
        let id = *self.id.get_or_insert_with(|| SEQ.fetch_add(1, Ordering::Relaxed));
        if let Ok(mut g) = REGISTRY.lock() {
            let started_ms = g.get(&id).map(|e| e.started_ms).unwrap_or(self.created_ms);
            g.insert(
                id,
                Entry {
                    model: if model.is_empty() { "(routing)".into() } else { model.to_string() },
                    session: session.to_string(),
                    agent: agent.to_string(),
                    started_ms,
                },
            );
        }
    }

    /// 带元信息登记（流式响应用：模型/会话/agent/开始时刻）
    pub fn with_meta(model: &str, session: &str, agent: &str, started_ms: u64) -> Self {
        LIVE_INFLIGHT.fetch_add(1, Ordering::Relaxed);
        let id = SEQ.fetch_add(1, Ordering::Relaxed);
        if let Ok(mut g) = REGISTRY.lock() {
            g.insert(
                id,
                Entry {
                    model: model.to_string(),
                    session: session.to_string(),
                    agent: agent.to_string(),
                    started_ms,
                },
            );
        }
        Self { id: Some(id), created_ms: now_ms() }
    }
}

impl Default for Guard {
    fn default() -> Self {
        Self::new()
    }
}

impl Drop for Guard {
    fn drop(&mut self) {
        LIVE_INFLIGHT.fetch_sub(1, Ordering::Relaxed);
        if let Some(id) = self.id
            && let Ok(mut g) = REGISTRY.lock()
        {
            g.remove(&id);
        }
    }
}

/// 在飞明细（按开始时间升序）：`[{model, session, agent, age_ms}]`
pub fn inflight_detail() -> Vec<serde_json::Value> {
    let Ok(g) = REGISTRY.lock() else { return Vec::new() };
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0);
    let mut rows: Vec<_> = g
        .values()
        .map(|e| {
            serde_json::json!({
                "model": e.model,
                "session": e.session,
                "agent": e.agent,
                "age_ms": now.saturating_sub(e.started_ms),
            })
        })
        .collect();
    rows.sort_by_key(|v| std::cmp::Reverse(v["age_ms"].as_u64().unwrap_or(0)));
    rows
}

fn now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

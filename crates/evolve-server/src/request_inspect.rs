//! 请求检查器（2026-10-21）：为最近 50 次路由保留"原始请求 → 转换/转发 →
//! 上游响应（含流式分片）"的完整现场，供 Web 实时决策流点击查看。
//!
//! 隐私与体积红线：
//! - `telemetry.redact = true` 时**不捕获任何 body**（只记元信息与尺寸）
//! - authorization / x-api-key 等**鉴权头一律打码**
//! - 每 body 上限 256KB、流式分片头 64KB + 尾 128KB，超出写截断标记
//! - 环形缓冲 50 条，新请求挤掉最旧
//!
//! Dashboard：实时决策流行点击 → `GET /api/inspect?decision_id=…` → 折叠的
//! JSON pretty 块（原始请求 / 转换后上游请求 / 响应头 / 响应体 / 流分片）。

use axum::extract::State;
use axum::response::IntoResponse;
use serde_json::json;
use std::collections::VecDeque;
use std::sync::{Arc, Mutex, LazyLock};

const RING_CAP: usize = 50;
pub const BODY_CAP: usize = 256 * 1024;
const STREAM_HEAD_CAP: usize = 64 * 1024;
const STREAM_TAIL_CAP: usize = 128 * 1024;

#[derive(Debug, Clone, Default, serde::Serialize)]
pub struct InspectEntry {
    pub decision_id: String,
    pub session: String,
    pub agent: String,
    pub model: String,
    #[serde(rename = "ts_ms")]
    pub ts_ms: u64,
    pub status: u16,
    pub total_ms: Option<u64>,
    pub stream: bool,
    /// redact=true 时 body 一律不捕获（只记尺寸）
    pub redacted: bool,
    // ── 原始（客户端 → 网关）──
    pub client_headers: Option<String>,
    pub client_body: Option<String>,
    pub client_body_bytes: u64,
    // ── 转发（网关 → 上游，最后一次尝试）──
    pub upstream_url: Option<String>,
    pub upstream_headers: Option<String>,
    pub upstream_body: Option<String>,
    /// 转发 body 与客户端原始 body 完全一致（无有效转换）→ UI 只提示不重复展示
    pub upstream_same: bool,
    pub translated: bool,
    // ── 响应（上游 → 网关 → 客户端）──
    pub resp_status: Option<u16>,
    pub resp_headers: Option<String>,
    pub resp_body: Option<String>,
    pub stream_head: Option<String>,
    pub stream_tail: Option<String>,
    pub stream_bytes: u64,
    pub stream_done: bool,
    pub truncated: Vec<String>,
}

type Ring = VecDeque<Arc<Mutex<InspectEntry>>>;

/// 流式分片的捕获句柄：telemetry_body 的流包装器每收到一段上游字节就
/// 调 capture_stream_chunk 写入检查条目（头 64KB + 尾 128KB 截断）
#[derive(Clone)]
pub struct Tap(pub Option<Arc<Mutex<InspectEntry>>>);

impl Tap {
    pub fn capture(&self, chunk: &[u8]) {
        if let Some(e) = &self.0 {
            capture_stream_chunk(e, chunk);
        }
    }
    pub fn done(&self) {
        if let Some(e) = &self.0 {
            mark_stream_done(e);
        }
    }
}

static RING: LazyLock<Mutex<Ring>> = LazyLock::new(|| Mutex::new(VecDeque::new()));

/// 请求进入时创建检查条目并压入环形缓冲（挤掉最旧）
pub fn begin(entry: InspectEntry) -> Arc<Mutex<InspectEntry>> {
    let arc = Arc::new(Mutex::new(entry));
    if let Ok(mut ring) = RING.lock() {
        ring.push_back(arc.clone());
        while ring.len() > RING_CAP {
            if ring.pop_front().is_some_and(|old| Arc::strong_count(&old) <= 2) {
                break;
            }
        }
    }
    arc
}

pub(crate) fn with_entry<F>(entry: &Arc<Mutex<InspectEntry>>, f: F)
where
    F: FnOnce(&mut InspectEntry),
{
    if let Ok(mut g) = entry.lock() {
        f(&mut g);
    }
}

/// 鉴权头打码：authorization / x-api-key 等只留"存在 + 长度"
pub(crate) fn header_json(headers: &axum::http::HeaderMap) -> String {
    let mut map = serde_json::Map::new();
    for (name, value) in headers.iter() {
        let k = name.as_str().to_string();
        let v = value.to_str().unwrap_or("<binary>");
        let masked = matches!(
            k.as_str(),
            "authorization" | "x-api-key" | "proxy-authorization" | "cookie"
        ) || k.contains("key");
        map.insert(
            k,
            json!(if masked { format!("«masked len={}>", v.len()) } else { v.to_string() }),
        );
    }
    serde_json::to_string_pretty(&map).unwrap_or_default()
}

/// body 捕获：尽量按 JSON pretty；超限截断并记标记
pub(crate) fn cap_body(raw: &[u8], cap: usize, truncated: &mut Vec<String>, what: &str) -> Option<String> {
    if raw.is_empty() {
        return None;
    }
    let text = String::from_utf8_lossy(raw);
    if raw.len() <= cap {
        return Some(
            serde_json::from_str::<serde_json::Value>(&text)
                .ok()
                .and_then(|v| serde_json::to_string_pretty(&v).ok())
                .unwrap_or_else(|| text.into_owned()),
        );
    }
    truncated.push(what.to_string());
    let cut = &text[..cap];
    let cut = serde_json::from_str::<serde_json::Value>(cut)
        .ok()
        .and_then(|v| serde_json::to_string_pretty(&v).ok())
        .unwrap_or_else(|| cut.to_string());
    Some(format!("{cut}\n\n…[truncated, total {} bytes]…", raw.len()))
}

/// 流式分片捕获：头 64KB 全留，其后只留最新 128KB 尾部
pub fn capture_stream_chunk(entry: &Arc<Mutex<InspectEntry>>, chunk: &[u8]) {
    const HEAD_CAP: usize = STREAM_HEAD_CAP;
    const TAIL_CAP: usize = STREAM_TAIL_CAP;
    if let Ok(mut g) = entry.lock() {
        g.stream_bytes += chunk.len() as u64;
        let head_len = g.stream_head.as_ref().map(|s| s.len()).unwrap_or(0);
        if head_len < HEAD_CAP {
            let head = g.stream_head.get_or_insert_with(String::new);
            let take = (HEAD_CAP - head.len()).min(chunk.len());
            head.push_str(&String::from_utf8_lossy(&chunk[..take]));
            if take < chunk.len() {
                g.stream_tail
                    .get_or_insert_with(String::new)
                    .push_str(&String::from_utf8_lossy(&chunk[take..]));
            }
        } else {
            let tail = g.stream_tail.get_or_insert_with(String::new);
            tail.push_str(&String::from_utf8_lossy(chunk));
            let overflow = tail.len().saturating_sub(TAIL_CAP);
            if overflow > 0 {
                *tail = tail[overflow..].to_string();
                if !g.truncated.iter().any(|t| t == "stream_tail") {
                    g.truncated.push("stream_tail".into());
                }
            }
        }
    }
}

pub fn mark_stream_done(entry: &Arc<Mutex<InspectEntry>>) {
    if let Ok(mut g) = entry.lock() {
        g.stream_done = true;
    }
}

/// GET /api/inspect → 最近请求的元信息列表（新→旧）
pub async fn api_inspect_list() -> axum::response::Response {
    let ring = RING.lock().map(|r| {
        r.iter()
            .map(|e| {
                let g = e.lock().unwrap_or_else(|p| p.into_inner());
                json!({
                    "decision_id": g.decision_id, "model": g.model, "status": g.status,
                    "ts_ms": g.ts_ms, "total_ms": g.total_ms, "stream": g.stream,
                    "stream_bytes": g.stream_bytes, "redacted": g.redacted,
                    "has_client_body": g.client_body.is_some(),
                    "has_upstream_body": g.upstream_body.is_some(),
                    "has_resp_body": g.resp_body.is_some(),
                    "translated": g.translated,
                })
            })
            .collect::<Vec<_>>()
    }).unwrap_or_default();
    (axum::Json(json!({ "entries": ring }))).into_response()
}

/// GET /api/inspect?decision_id=… → 单条完整现场
pub async fn api_inspect_detail(
    axum::extract::Query(params): axum::extract::Query<std::collections::HashMap<String, String>>,
) -> axum::response::Response {
    let Some(id) = params.get("decision_id") else {
        return (
            axum::http::StatusCode::BAD_REQUEST,
            axum::Json(json!({"error": {"message": "decision_id required"}})),
        )
            .into_response();
    };
    let found = RING.lock().ok().and_then(|r| {
        r.iter()
            .find(|e| e.lock().map(|g| g.decision_id == *id).unwrap_or(false))
            .cloned()
    });
    let Some(entry) = found else {
        return (
            axum::http::StatusCode::NOT_FOUND,
            axum::Json(json!({"error": {"message": "decision_id not in the inspect ring (cap 50 / restarted)"}})),
        )
            .into_response();
    };
    let g = entry.lock().unwrap_or_else(|p| p.into_inner());
    (axum::Json(serde_json::to_value(&*g).unwrap_or_default())).into_response()
}

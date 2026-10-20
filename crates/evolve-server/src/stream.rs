use futures::stream::{Stream, StreamExt};
use evolve_core::types::ResponseQuality;
use evolve_memory::{EventLog, Flywheel, QuotaLedger, SessionStore};
use serde_json::{json, Map, Value};
use std::collections::BTreeMap;
use std::pin::Pin;
use std::sync::{Arc, Mutex};
use std::task::{Context, Poll};
use std::time::Instant;

pub struct Telemetry {
    pub decision_id: String,
    pub session: String,
    pub chosen: String,
    pub upstream_model: String,
    pub est_tokens: u64,
    pub sticky: bool,
    pub stream: bool,
    pub started: Instant,
    pub ttft_ms: Option<u128>,
    pub bytes: u64,
    pub status: u16,
    pub usage: Option<Value>,
    pub est_cost_usd: Option<f64>,
    pub translated: Option<String>,
    pub extra: Option<Value>,
    /// 来源客户端：优先 x-ev-client，回退 User-Agent（截断），供面板展示
    pub agent: Option<String>,
    /// 订阅方案 key（baseUrl 匹配注册表）——预算记账用
    pub plan_key: Option<String>,
    /// 网关预处理耗时（解析/特征提取/身份识别）
    pub preprocess_ms: Option<u64>,
    /// 路由决策耗时（决策模型 LLM + 规则引擎融合）
    pub decision_ms: Option<u64>,
    /// 其中任务判定阶段耗时
    pub judge_ms: Option<u64>,
    /// 本轮请求的**裸** token 估算（未会话校准）——流式收尾时与实测
    /// prompt_tokens 一起入会话账本（非流式路径在 relay 内自记）
    pub est_raw: Option<u64>,
}

impl Telemetry {
    pub fn event_value(&self) -> Value {
        let total_ms = self.started.elapsed().as_millis() as u64;
        let mut v = json!({
            "kind": "chat",
            "decision_id": self.decision_id,
            "session": self.session,
            "chosen": self.chosen,
            "upstream_model": self.upstream_model,
            "est_tokens": self.est_tokens,
            "sticky": self.sticky,
            "stream": self.stream,
            "status": self.status,
            "ttft_ms": self.ttft_ms.map(|t| t as u64),
            "total_ms": total_ms,
            "bytes": self.bytes,
            "usage": self.usage,
            "agent": self.agent,
            "plan_key": self.plan_key,
            "preprocess_ms": self.preprocess_ms,
            "decision_ms": self.decision_ms,
            "judge_ms": self.judge_ms,
        });
        if let Some(u) = &self.usage {
            let (p, c, cached, cwrite) = extract_usage_fields(u);
            v["prompt_tokens"] = json!(p);
            v["completion_tokens"] = json!(c);
            v["cached_tokens"] = json!(cached);
            v["cache_write_tokens"] = json!(cwrite);
        }
        if let Some(extra) = &self.extra
            && let (Some(obj), Some(ex)) = (v.as_object_mut(), extra.as_object()) {
                for (k, val) in ex {
                    obj.insert(k.clone(), val.clone());
                }
            }
        v
    }
}

type SharedTelem = Arc<Mutex<Telemetry>>;

/// Finalize: write event, feed flywheel, broadcast to dashboard subscribers.
pub fn finalize_event(
    events: &EventLog,
    flywheel: &Flywheel,
    quota: &QuotaLedger,
    bus: &tokio::sync::broadcast::Sender<Value>,
    telem: &Telemetry,
) {
    let event = telem.event_value();
    // 订阅方案用量记账（积分/美元折算的数据源）
    if telem.plan_key.is_some()
        && let Some(u) = &telem.usage
    {
        // extract_usage_fields 返回 (prompt, completion, cached_read, cache_write)
        let (i, cached, o) = {
            let (p, comp, cached, _cw) = extract_usage_fields(u);
            (p, cached, comp)
        };
        if let (Some(i), Some(o)) = (i, o) {
            quota.note_plan_usage(
                &telem.chosen,
                i as f64,
                cached.unwrap_or(0) as f64,
                o as f64,
                quota_now_ms(),
            );
        }
    }
    let event = event;
    events.record(event.clone());
    flywheel.observe(&event);
    let _ = bus.send(event);
}

fn quota_now_ms() -> u64 {
    std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_millis() as u64).unwrap_or(0)
}

/// Wraps an upstream byte stream, capturing TTFT/bytes/usage/content/tool
/// 流结束的性质（路由健康学习用）：
/// - `Ok`：看到终止帧（正常结束）
/// - `IncompleteUpstream`：上游**自己**结束了但没有 `finish_reason`（静默断流/截断）
///   —— "200 + 无 usage + 无终止帧"绝不能被记成成功。实测事故：mimo-v2.6-flash 在
///   24 万 token 上下文下 50s 首字节、347s 后只吐 840 字节就断流，客户端整轮空转
/// - `ClientAborted`：响应体在流结束前被丢弃（客户端断开/进程收尾）——不惩罚上游
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StreamEnd {
    Ok,
    IncompleteUpstream,
    ClientAborted,
}

pub fn classify_stream_end(finish_reason: Option<&str>, upstream_eof: bool) -> StreamEnd {
    if !upstream_eof {
        return StreamEnd::ClientAborted;
    }
    match finish_reason {
        Some(_) => StreamEnd::Ok,
        None => StreamEnd::IncompleteUpstream,
    }
}

/// calls while forwarding chunks untouched. At stream end (or drop) runs
/// response-quality analysis, feeds the flywheel, and records pending tool
/// calls for L3 session-loop matching.
#[allow(clippy::too_many_arguments)]
pub fn telemetry_body<E>(
    inner: impl Stream<Item = Result<bytes::Bytes, E>> + Send + 'static,
    telem: SharedTelem,
    events: EventLog,
    flywheel: Flywheel,
    sessions: SessionStore,
    quota: std::sync::Arc<QuotaLedger>,
    bus: tokio::sync::broadcast::Sender<Value>,
    request: Value,
    anthropic_mode: bool,
    health: Option<evolve_memory::health::HealthRegistry>,
) -> futures::stream::BoxStream<'static, Result<bytes::Bytes, axum::Error>>
where
    E: std::error::Error + Send + Sync + 'static,
{
    let health_model = telem.lock().map(|t| t.chosen.clone()).unwrap_or_default();
    let inner = futures::TryStreamExt::map_err(inner, |e: E| axum::Error::new(e));
    let pinned: StateInner = Box::pin(inner);
    let state = (
        pinned,
        Some(Finalizer {
            telem: telem.clone(),
            events: Some(events),
            flywheel: Some(flywheel),
            sessions: Some(sessions),
            quota: Some(quota),
            bus: Some(bus),
            request,
            acc: StreamAcc::new(anthropic_mode),
            upstream_eof: false,
            tail_done: false,
            model_id: health_model.clone(),
            health: health.clone(),
            // 流的整个生命期都算"在飞"（流结束/被丢弃时释放），并登记模型/会话
            _inflight: {
                let (model, session, agent, started) = telem
                    .lock()
                    .map(|t| {
                        (
                            t.chosen.clone(),
                            t.session.clone(),
                            t.agent.clone().unwrap_or_default(),
                            std::time::SystemTime::now()
                                .duration_since(std::time::UNIX_EPOCH)
                                .map(|d| d.as_millis() as u64)
                                .unwrap_or(0)
                                .saturating_sub(t.started.elapsed().as_millis() as u64),
                        )
                    })
                    .unwrap_or_default();
                crate::live::Guard::with_meta(&model, &session, &agent, started)
            },
        }),
    );
    TelemetryStream { inner: Box::new(state) }.boxed()
}

#[derive(Default)]
struct StreamAcc {
    content: String,
    content_capped: bool,
    tool_calls: BTreeMap<u32, (String, String, String)>, // idx -> (id, name, args)
    finish_reason: Option<String>,
    usage: Option<Value>,
    // byte-level line buffer: decode only complete lines so multi-byte
    // UTF-8 chars split across HTTP chunks survive (BUG-6)
    byte_buf: Vec<u8>,
    /// true when accumulating upstream anthropic SSE (cross-protocol)
    anthropic_mode: bool,
    // anthropic accumulation
    ant_tool_ids: BTreeMap<u32, String>,
    ant_tool_names: BTreeMap<u32, String>,
    ant_tool_args: BTreeMap<u32, String>,
}

const CONTENT_CAP: usize = 1024 * 1024; // review#10：64KB 截断会让 agent 大参数（write_file）产出非法 JSON——质量误判+双倍计费；1MB 覆盖常态

impl StreamAcc {
    fn new(anthropic_mode: bool) -> Self {
        Self {
            anthropic_mode,
            ..Default::default()
        }
    }

    fn push_chunk(&mut self, chunk: &[u8]) {
        self.byte_buf.extend_from_slice(chunk);
        while let Some(pos) = self.byte_buf.iter().position(|&b| b == b'\n') {
            let line_bytes: Vec<u8> = self.byte_buf.drain(..=pos).collect();
            let line = String::from_utf8_lossy(&line_bytes);
            self.process_line(line.trim_end());
        }
    }

    fn process_line(&mut self, line: &str) {
        if self.anthropic_mode {
            self.process_anthropic_line(line);
        } else {
            self.process_openai_line(line);
        }
    }

    fn process_anthropic_line(&mut self, line: &str) {
        let Some(data) = line.strip_prefix("data:") else { return };
        let data = data.trim();
        if data.is_empty() {
            return;
        }
        let Ok(v) = serde_json::from_str::<Value>(data) else { return };
        match v.get("type").and_then(|t| t.as_str()) {
            Some("message_start") => {
                if let Some(u) = v.pointer("/message/usage").filter(|u| u.is_object()) {
                    self.usage = Some(u.clone());
                }
            }
            Some("content_block_start") => {
                let idx = v.get("index").and_then(|i| i.as_u64()).unwrap_or(0) as u32;
                if let Some(block) = v.get("content_block")
                    && block.get("type").and_then(|t| t.as_str()) == Some("tool_use") {
                        if let Some(id) = block.get("id").and_then(|i| i.as_str()) {
                            self.ant_tool_ids.insert(idx, id.to_string());
                        }
                        if let Some(n) = block.get("name").and_then(|n| n.as_str()) {
                            self.ant_tool_names.insert(idx, n.to_string());
                        }
                    }
            }
            Some("content_block_delta") => {
                let idx = v.get("index").and_then(|i| i.as_u64()).unwrap_or(0) as u32;
                if let Some(delta) = v.get("delta") {
                    match delta.get("type").and_then(|t| t.as_str()) {
                        Some("text_delta") => {
                            if let Some(t) = delta.get("text").and_then(|t| t.as_str())
                                && self.content.len() < CONTENT_CAP
                            {
                                self.content.push_str(t);
                            }
                        }
                        Some("input_json_delta") => {
                            if let Some(a) = delta.get("partial_json").and_then(|a| a.as_str())
                                && self.ant_tool_args.entry(idx).or_default().len() < CONTENT_CAP
                            {
                                self.ant_tool_args.get_mut(&idx).unwrap().push_str(a);
                            }
                        }
                        _ => {}
                    }
                }
            }
            Some("message_delta") => {
                if let Some(u) = v.get("usage").filter(|u| u.is_object()) {
                    let merged = match &mut self.usage {
                        Some(prev) => {
                            if let (Some(p), Some(n)) = (prev.as_object_mut(), u.as_object()) {
                                for (k, val) in n {
                                    p.insert(k.clone(), val.clone());
                                }
                            }
                            prev.clone()
                        }
                        None => u.clone(),
                    };
                    self.usage = Some(merged);
                }
                if let Some(sr) = v.pointer("/delta/stop_reason").and_then(|s| s.as_str()) {
                    self.finish_reason = Some(match sr {
                        "end_turn" => "stop".into(),
                        "max_tokens" => "length".into(),
                        "tool_use" => "tool_calls".into(),
                        other => other.to_string(),
                    });
                }
            }
            _ => {}
        }
    }

    fn process_openai_line(&mut self, line: &str) {
        let Some(data) = line.strip_prefix("data:") else { return };
        let data = data.trim();
        if data.is_empty() || data == "[DONE]" {
            return;
        }
        let Ok(v) = serde_json::from_str::<Value>(data) else { return };
        if let Some(u) = v.get("usage").filter(|u| u.is_object()) {
            self.usage = Some(u.clone());
        }
        let Some(choice) = v.get("choices").and_then(|c| c.as_array()).and_then(|c| c.first()) else {
            return;
        };
        if let Some(fr) = choice.get("finish_reason").and_then(|f| f.as_str())
            && !fr.is_empty() && fr != "null" {
                self.finish_reason = Some(fr.to_string());
            }
        if let Some(delta) = choice.get("delta") {
            if let Some(t) = delta.get("content").and_then(|c| c.as_str()) {
                if self.content.len() < CONTENT_CAP {
                    self.content.push_str(t);
                } else {
                    self.content_capped = true;
                }
            }
            if let Some(tcs) = delta.get("tool_calls").and_then(|t| t.as_array()) {
                for tc in tcs {
                    let idx = tc.get("index").and_then(|i| i.as_u64()).unwrap_or(0) as u32;
                    let entry = self.tool_calls.entry(idx).or_default();
                    if let Some(id) = tc.get("id").and_then(|i| i.as_str()) {
                        entry.0 = id.to_string();
                    }
                    if let Some(name) = tc
                        .get("function")
                        .and_then(|f| f.get("name"))
                        .and_then(|n| n.as_str())
                    {
                        entry.1.push_str(name);
                    }
                    if let Some(args) = tc
                        .get("function")
                        .and_then(|f| f.get("arguments"))
                        .and_then(|a| a.as_str())
                        && entry.2.len() < CONTENT_CAP {
                            entry.2.push_str(args);
                        }
                }
            }
        }
    }

    fn build_response(&self) -> Value {
        let mut source: Vec<(String, String, String)> = self
            .tool_calls
            .values()
            .cloned()
            .collect();
        for (idx, id) in &self.ant_tool_ids {
            source.push((
                id.clone(),
                self.ant_tool_names.get(idx).cloned().unwrap_or_default(),
                self.ant_tool_args.get(idx).cloned().unwrap_or_default(),
            ));
        }
        let tool_calls: Vec<Value> = source
            .iter()
            .map(|(id, name, args)| {
                json!({"id": id, "type": "function",
                       "function": {"name": name, "arguments": args}})
            })
            .collect();
        let mut message = Map::new();
        message.insert("role".into(), json!("assistant"));
        message.insert("content".into(), json!(self.content));
        if !tool_calls.is_empty() {
            message.insert("tool_calls".into(), json!(tool_calls));
        }
        json!({
            "choices": [{
                "finish_reason": self.finish_reason.clone().unwrap_or_else(|| "stop".into()),
                "message": Value::Object(message),
            }]
        })
    }
}

struct Finalizer {
    /// 在飞令牌：**随流一起存活**，直到流结束才 release（重启提示据此可见"谁在飞"）
    _inflight: crate::live::Guard,
    telem: SharedTelem,
    events: Option<EventLog>,
    flywheel: Option<Flywheel>,
    sessions: Option<SessionStore>,
    quota: Option<std::sync::Arc<QuotaLedger>>,
    bus: Option<tokio::sync::broadcast::Sender<Value>>,
    request: Value,
    acc: StreamAcc,
    /// 是否看到了上游的正常结束（poll_next → None）：用于区分"上游截断"与"客户端放弃"
    upstream_eof: bool,
    /// 上游异常收尾时已注入底层现象的 error 帧（截断/transport 错误各一拍）
    tail_done: bool,
    /// 判定用的模型 id（截断时给健康注册表记账 → 该模型冷却，路由避开）
    model_id: String,
    health: Option<evolve_memory::health::HealthRegistry>,
}

const TAIL_CAP: usize = 16 * 1024;

impl Finalizer {
    fn push_chunk(&mut self, chunk: &[u8]) {
        self.acc.push_chunk(chunk);
        if self.acc.byte_buf.len() > TAIL_CAP {
            self.acc.byte_buf.clear();
        }
    }

    /// 给客户端一个"最底层现象"的 error 帧 + [DONE]——上游截断/断流时客户端
    /// 以前只见无声断连，容易误判成网关自身的故障。入站为 anthropic 协议时
    /// 用 anthropic 的 error 形状（event: error + type/error 字段）
    fn error_tail(&mut self, message: &str) -> bytes::Bytes {
        let body = if self.acc.anthropic_mode {
            let payload = serde_json::json!({
                "type": "error",
                "error": {"type": "api_error", "message": message}
            });
            format!("event: error\ndata: {}\n\n", payload)
        } else {
            let payload = serde_json::json!({
                "error": {"message": message, "type": "upstream_error", "code": "upstream_error"}
            });
            format!("data: {}\n\ndata: [DONE]\n\n", payload)
        };
        bytes::Bytes::from(body)
    }
}

impl Drop for Finalizer {
    fn drop(&mut self) {
        let Some(events) = self.events.take() else { return };
        // 结束性质判定：上游截断必须**如实记成失败**（否则飞轮的可靠性把断流当成功学习）
        let verdict = classify_stream_end(self.acc.finish_reason.as_deref(), self.upstream_eof);
        // 锁中毒也要落事件（曾经 `let Ok(..) else { return }` 会**静默丢失整条事件**：
        // 事故排查时"网关没有这条请求的记录"就是这么来的）
        let mut t = match self.telem.lock() {
            Ok(g) => g,
            Err(poisoned) => {
                tracing::warn!("telemetry lock poisoned at stream finalize; recording anyway");
                poisoned.into_inner()
            }
        };
        t.usage = self.acc.usage.clone();
        // 会话真实上下文入账（2026-10-20）：流式请求的 usage 在这里才落定——
        // 此前只有非流式路径入账本，流式会话永远无锚点 → 校准堆叠永远生效
        if let Some(u) = &t.usage
            && let Some(raw) = t.est_raw
        {
            let pt = u.get("prompt_tokens").and_then(|x| x.as_u64()).unwrap_or(0);
            crate::relay::record_session_tokens(&t.session.clone(), raw, pt);
        }
        if self.acc.finish_reason.is_some() || !self.acc.content.is_empty() || !self.acc.tool_calls.is_empty() {
            let pseudo = self.acc.build_response();
            let mut q: ResponseQuality = crate::quality::analyze_response(&self.request, &pseudo);
            if self.acc.content_capped {
                q.flags.push("content_capped".into());
            }
            // L3: remember pending tool calls for next-request matching
            if let Some(sessions) = &self.sessions {
                let ids: Vec<String> =
                    self.acc.tool_calls.values().map(|(id, _, _)| id.clone()).collect();
                // 与 relay 的 sticky_key 同构：agent 前缀隔离跨 agent 同名会话
                let pkey = format!("{}\u{1f}{}", t.agent.clone().unwrap_or_default(), t.session);
                sessions.set_pending(&pkey, &t.chosen, ids);
            }
            t.extra = match t.extra.take() {
                Some(mut ex) => {
                    ex["quality"] = serde_json::to_value(&q).unwrap_or_default();
                    let mut names: Vec<String> =
                        self.acc.tool_calls.values().map(|(_, n, _)| n.clone()).collect();
                    names.sort();
                    names.dedup();
                    names.truncate(5);
                    if !names.is_empty() {
                        ex["tools"] = serde_json::json!(names);
                    }
                    Some(ex)
                }
                None => Some(json!({"quality": q})),
            };
        }
        match verdict {
            StreamEnd::IncompleteUpstream => {
                t.status = 599;
                let total_ms = t.started.elapsed().as_millis() as u64;
                tracing::warn!(
                    model = %self.model_id,
                    bytes = t.bytes,
                    total_ms,
                    "upstream stream ended without a finish_reason (truncated) — marking the model failed"
                );
                if let Some(h) = &self.health {
                    h.mark_failure(
                        &self.model_id,
                        evolve_memory::health::Failure {
                            kind: evolve_core::types::HealthKind::Transient,
                            message: "stream truncated (no finish_reason)".into(),
                            until_epoch_ms: None,
                        },
                    );
                }
                t.extra = match t.extra.take() {
                    Some(mut ex) => {
                        ex["stream_incomplete"] = json!(true);
                        Some(ex)
                    }
                    None => Some(json!({"stream_incomplete": true})),
                };
            }
            StreamEnd::ClientAborted => {
                // 客户端/进程在流结束前放弃：不是上游的错，不记账、不改状态
                t.extra = match t.extra.take() {
                    Some(mut ex) => {
                        ex["client_aborted"] = json!(true);
                        Some(ex)
                    }
                    None => Some(json!({"client_aborted": true})),
                };
            }
            StreamEnd::Ok => {}
        }
        // 统一收尾（与非流式同路径）：含订阅方案用量记账（quota 缺席时退化为仅落盘）
        if let (Some(fw), Some(q)) = (&self.flywheel, &self.quota) {
            finalize_event(&events, fw, q, self.bus.as_ref().expect("bus"), &t);
            return;
        }
        let event = t.event_value();
        events.record(event.clone());
        if let Some(fw) = &self.flywheel {
            fw.observe(&event);
        }
        if let Some(bus) = &self.bus {
            let _ = bus.send(event);
        }
    }
}

type StateInner =
    Pin<Box<dyn Stream<Item = Result<bytes::Bytes, axum::Error>> + Send>>;

struct TelemetryStream {
    inner: Box<(StateInner, Option<Finalizer>)>,
}

impl Stream for TelemetryStream {
    type Item = Result<bytes::Bytes, axum::Error>;

    fn poll_next(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        let this = &mut *self.inner;
        match this.0.as_mut().poll_next(cx) {
            Poll::Ready(Some(Ok(chunk))) => {
                if let Some(f) = this.1.as_mut() {
                    f.push_chunk(&chunk);
                    if let Ok(mut t) = f.telem.lock() {
                        if t.ttft_ms.is_none() {
                            t.ttft_ms = Some(t.started.elapsed().as_millis());
                        }
                        t.bytes += chunk.len() as u64;
                    }
                }
                Poll::Ready(Some(Ok(chunk)))
            }
            Poll::Ready(Some(Err(e))) => {
                // 中途 transport 错误：向客户端透传**原样**给了个无声断连，用户只能看到
                // 连接崩却不知道是谁的错 → 改为注入底层现象的 error 帧后干净收束，
                // 报错口径 = "连的是上游，断的是上游"（错误原文照贴，不再包装）
                if let Some(f) = this.1.as_mut() {
                    if f.tail_done {
                        // 已注入过 error 帧：彻底收尾（重复的 Err 不再透出）
                        this.1 = None; // triggers Finalizer::drop
                        return Poll::Ready(None);
                    }
                    let msg = format!("upstream connection broke mid-stream: {e}");
                    let bytes = f.error_tail(&msg);
                    f.tail_done = true;
                    if let Ok(mut t) = f.telem.lock() {
                        t.status = 599;
                    }
                    return Poll::Ready(Some(Ok(bytes)));
                }
                Poll::Ready(Some(Err(axum::Error::new(e))))
            }
            Poll::Ready(None) => {
                // 先标记"上游正常结束"，Drop 里据此区分截断 vs 客户端放弃
                if let Some(f) = this.1.as_mut() {
                    f.upstream_eof = true;
                    // 截断：追加底层现象 error 帧（客户端收到的是"上游没发完"，而不是
                    // 一个看似干净的响应终点）
                    if !f.tail_done && classify_stream_end(f.acc.finish_reason.as_deref(), f.upstream_eof)
                        == StreamEnd::IncompleteUpstream
                    {
                        f.tail_done = true;
                        let msg = "upstream stream truncated (ended without a finish_reason; the \
                                   provider did not complete the response)"
                            .to_string();
                        return Poll::Ready(Some(Ok(f.error_tail(&msg))));
                    }
                    this.1 = None; // triggers Finalizer::drop
                }
                Poll::Ready(None)
            }
            Poll::Pending => Poll::Pending,
        }
    }
}

pub fn extract_usage_fields(usage: &Value) -> (Option<u64>, Option<u64>, Option<u64>, Option<u64>) {
    // openai 形状优先；anthropic 原生形状（跨协议流式上游直回）兜底
    let prompt = usage
        .get("prompt_tokens")
        .and_then(|v| v.as_u64())
        .or_else(|| usage.get("input_tokens").and_then(|v| v.as_u64()));
    let completion = usage
        .get("completion_tokens")
        .and_then(|v| v.as_u64())
        .or_else(|| usage.get("output_tokens").and_then(|v| v.as_u64()));
    let cached = usage
        .get("prompt_tokens_details")
        .and_then(|d| d.get("cached_tokens"))
        .and_then(|v| v.as_u64())
        .or_else(|| usage.get("prompt_cache_hit_tokens").and_then(|v| v.as_u64()))
        .or_else(|| usage.get("cache_read_input_tokens").and_then(|v| v.as_u64()));
    // 缓存写：zhipu cache_write_tokens / anthropic cache_creation_input_tokens
    let cache_write = usage
        .get("prompt_tokens_details")
        .and_then(|d| d.get("cache_write_tokens"))
        .and_then(|v| v.as_u64())
        .or_else(|| usage.get("cache_creation_input_tokens").and_then(|v| v.as_u64()));
    (prompt, completion, cached, cache_write)
}

#[cfg(test)]
mod stream_end_tests {
    use super::{classify_stream_end, StreamEnd};

    /// "上游自己结束但没有终止帧"必须判为截断——这类响应（200 + 无 usage + 无 finish_reason）
    /// 曾被当成成功，飞轮学不到"这个模型会断流"（实测：mimo-v2.6-flash 24 万上下文下断流）
    #[test]
    fn stream_end_classification() {
        assert_eq!(classify_stream_end(Some("stop"), true), StreamEnd::Ok);
        assert_eq!(classify_stream_end(Some("tool_calls"), true), StreamEnd::Ok);
        assert_eq!(
            classify_stream_end(None, true),
            StreamEnd::IncompleteUpstream,
            "upstream EOF without a finish_reason is a truncation"
        );
        // 客户端/进程在流结束前放弃：不惩罚上游
        assert_eq!(classify_stream_end(None, false), StreamEnd::ClientAborted);
        assert_eq!(classify_stream_end(Some("stop"), false), StreamEnd::ClientAborted);
    }
}


use crate::rewrite::rewrite_model_field;
use crate::state::AppState;
use axum::body::Bytes;
use axum::extract::State;
use axum::http::{HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use evolve_core::engine::RoutingInput;
use evolve_core::features as feat;
use evolve_core::tokens as tok;
use evolve_core::types::*;
use evolve_core::types::HealthKind;
use evolve_memory::health::{classify_failure, Failure};
use futures::StreamExt;
use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use std::time::Instant;

const MAX_ERROR_BODY: usize = 8 * 1024;

/// Health-map key separator: model id + unit-sep + key index.
const KEY_SEP: char = '\u{1f}';

// ── 实时状态（/api/live，菜单栏图标/面板的"忙碌"数据源）──
// in-flight = 请求进入 chat/messages 处理器到返回的全周期（含路由决策段），
// RAII 守卫覆盖所有提前 return 路径（缓存命中/参数错误/级联降级等）。
// 在飞登记迁到 `crate::live`（含元信息，供"重启前提示在飞的流"）；
// 保留旧名以免改动散落的调用点
pub use crate::live::{Guard as LiveGuard, LIVE_INFLIGHT};

pub async fn chat_completions(
    State(st): State<AppState>,
    headers: HeaderMap,
    body: Bytes,
) -> Response {
    let started = Instant::now();
    let mut live = LiveGuard::new();
    let agent_hdr = crate::identity::agent_identity(&headers, &st.config.telemetry.agent_header);
    let Ok(parsed) = serde_json::from_slice::<Value>(&body) else {
        return json_error(StatusCode::BAD_REQUEST, "invalid json body");
    };



    let model_field = parsed.get("model").and_then(|v| v.as_str()).unwrap_or("auto").to_string();
    let target = match resolve_target(&st, &model_field) {
        Ok(t) => t,
        Err(resp) => return resp,
    };
    // 请求检查器（2026-10-21）：原始请求头/体入检查环（redact=true 不存体，
    // authorization 类头打码）；decision_id/model 在路由后回填
    let redact_on = st.config.decision.redact;
    let inspect = crate::request_inspect::begin(crate::request_inspect::InspectEntry {
        decision_id: String::new(),
        session: String::new(),
        agent: agent_hdr.clone(),
        model: model_field.clone(),
        ts_ms: evolve_memory::health::now(),
        status: 0,
        total_ms: None,
        stream: false,
        redacted: redact_on,
        client_headers: Some(crate::request_inspect::header_json(&headers)),
        client_body: if redact_on {
            None
        } else {
            crate::request_inspect::cap_body(
                &body,
                crate::request_inspect::BODY_CAP,
                &mut Vec::new(),
                "client_body",
            )
        },
        client_body_bytes: body.len() as u64,
        upstream_url: None,
        upstream_headers: None,
        upstream_body: None,
        upstream_same: false,
        translated: false,
        resp_status: None,
        resp_headers: None,
        resp_body: None,
        stream_head: None,
        stream_tail: None,
        stream_bytes: 0,
        stream_done: false,
        truncated: Vec::new(),
    });
    let session_key = session_key_of(&st, &agent_hdr, &headers, &parsed);
    // 在飞登记：先记"(routing)/会话"，选出模型后再更新（面板重启提示可读）
    live.describe("", &session_key, &agent_hdr);
    crate::request_inspect::with_entry(&inspect, |g| {
        g.session = session_key.clone();
    });
    // 内部状态（粘性/L3 反馈）按 agent 隔离：跨 agent 同名会话绝不共享状态
    let sticky_key = format!("{}\u{1f}{}", agent_hdr, session_key);
    let header_policy = headers
        .get("x-ev-policy")
        .and_then(|v| v.to_str().ok())
        .and_then(PolicyProfile::parse);

    let extracted = feat::extract(&parsed);
    let est_raw = tok::estimate_messages(&extracted.messages, extracted.tools_json.as_deref());
    // 会话真实上下文：上一轮 usage 实测 × 本轮估算增幅（同一估算器 → 偏差抵消），
    // 见 SESSION_TOKENS。命中账本的会话 est 即真值，引擎侧跳过校准堆叠
    let (est, est_observed) = session_calibrated_est(&session_key, est_raw);
    let features = feat::features(&extracted, est);
    let tools_sig = tools_signature(&parsed);
    let digest = build_digest(&extracted, features.turn_count);

    // A3 completion cache（FrugalGPT Strategy 2）：逻辑内容相同（去掉 model/
    // stream 字段后指纹一致）的非流式请求直接返回缓存响应——重复查询零成本
    // 零延迟。本地内存、TTL/容量受限、x-ev-no-cache 头可绕过。
    let is_stream_early = parsed.get("stream").and_then(|v| v.as_bool()).unwrap_or(false);
    let cache_key = if is_stream_early
        || headers.get("x-ev-no-cache").is_some()
        || std::env::var("EVO_CACHE").map(|v| v == "0").unwrap_or(false)
    {
        None
    } else {
        cache_fingerprint(&parsed)
    };
    if let Some(key) = &cache_key
        && let Some((model_id, content_type, bytes)) = st.ccache.lock().ok().and_then(|mut c| c.get(*key))
    {
        tracing::info!(model = %model_id, "completion cache hit");
        st.events.record(json!({
            "kind": "chat", "chosen": model_id, "upstream_model": model_id,
            "cache": true, "status": 200, "session": session_key,
            "agent": agent_hdr, "total_ms": started.elapsed().as_millis() as u64,
        }));
        // 缓存命中也是一次"已服务请求"——喂给飞轮（样本环 + 小时桶），否则面板
        // 的窗口请求数会少于最近请求列表，今天/昨天统计漏计缓存命中的请求
        let usage = serde_json::from_slice::<serde_json::Value>(&bytes)
            .ok()
            .and_then(|v| v.get("usage").cloned())
            .unwrap_or(Value::Null);
        let num = |v: &Value, k: &str| v.get(k).and_then(|x| x.as_u64()).unwrap_or(0);
        st.flywheel.observe(&json!({
            "kind": "chat", "chosen": model_id, "status": 200,
            "ts": evolve_memory::health::now(),
            "ttft_ms": started.elapsed().as_millis() as u64,
            "total_ms": started.elapsed().as_millis() as u64,
            "prompt_tokens": num(&usage, "prompt_tokens"),
            "completion_tokens": num(&usage, "completion_tokens"),
            "cached_tokens": usage
                .get("prompt_tokens_details")
                .and_then(|d| d.get("cached_tokens"))
                .and_then(|v| v.as_u64())
                .unwrap_or(0),
        }));
        return Response::builder()
            .status(StatusCode::OK)
            .header("content-type", content_type)
            .header("x-ev-cache", "hit")
            .header("x-ev-model", model_id)
            .header("x-ev-reason", "completion cache hit (identical logical request)")
            .body(axum::body::Body::from(bytes))
            .unwrap();
    }

    let decision_started = std::time::Instant::now();
    let mut decision = match &target {
        Target::Auto(alias_policy) => {
            let sticky = st.sessions.get(&sticky_key);
            let health = st.health.snapshot();
            let telemetry = st.flywheel.telemetry_snapshot();
            let mut quota_view = st.quota.best_remaining_by_models();
            let plan_pressure = st.plan_pressure_map();
            // S2 价格层标签表（自动推导 + 用户覆盖）→ 引擎难度→价位硬闸
            let price_tiers = crate::meta::price_tier_map(&st);
            // 活跃集（最近发布/最近使用前 50 + 手动开关）
            let active_ids = crate::meta::active_id_set(&st);
            // provider 级降级（网络/鉴权；连续失败达标才进）→ 不进主池
            let degraded: std::collections::HashSet<String> =
                st.quota.provider_degraded().into_iter().collect();

            // w2 provider 配额闸门：QUOTA_LIMITED 账号的模型配额视图清零
            // （引擎按模型硬滤 "quota remaining < est"——比 plan_pressure=1.5
            // 粗粒度方案精确：同 plan_key 多账号不连带受罚）；探测窗口到的
            // 无 API provider 豁免本轮（放一间探测单验证真实状态）
            let now_gate = evolve_memory::quota::now_ms();
            let probe_provider = st.quota.gate_pick_probe(now_gate);
            for provider in st.quota.gate_blocked() {
                if Some(provider.as_str()) == probe_provider.as_deref() {
                    continue;
                }
                for mid in st.engine.catalog_snapshot().iter()
                    // 免费直通车豁免（2026-10-23 用户追问实证）：免费模型不烧
                    // 方案配额，账号级配额闸（weekly/5h 耗尽）不连带——否则
                    // 付费窗耗尽时免费模型也被一并清零，违背直通车裁决
                    .filter(|m| m.provider == provider && !m.is_free())
                    .map(|m| m.id.clone()) {
                    quota_view.insert(mid, 0);
                }
            }

            // L3 session-loop: did the previous turn's tool calls come back
            // executed as role=tool messages? (gateway-side semantic signal)
            if let Some(pending) = st.sessions.take_pending(&sticky_key) {
                let ids: Vec<&str> = parsed
                    .get("messages")
                    .and_then(|m| m.as_array())
                    .map(|msgs| {
                        msgs.iter()
                            .filter(|m| m.get("role").and_then(|r| r.as_str()) == Some("tool"))
                            .filter_map(|m| m.get("tool_call_id").and_then(|i| i.as_str()))
                            .collect()
                    })
                    .unwrap_or_default();
                let total = pending.call_ids.len() as u64;
                let matched = pending.call_ids.iter().filter(|id| ids.contains(&id.as_str())).count() as u64;
                if total > 0 {
                    st.flywheel.observe_semantic(&pending.model, matched, total);
                    st.events.record(json!({
                        "kind": "semantic", "model": pending.model,
                        "matched": matched, "total": total, "session": session_key,
                    }));
                }
            }

            let input = RoutingInput {
                session_key: &session_key,
                features,
                digest: &digest,
                tools_sig,
                continues_tools: evolve_core::features::continues_tool_flow(&extracted.messages),
                max_output_req: parsed.get("max_tokens").and_then(|v| v.as_u64()),
                policy: header_policy.or(*alias_policy),
                sticky,
                health: &health,
                telemetry: &telemetry,
                quota: &quota_view,
                plan_pressure: &plan_pressure,
                redact: st.config.decision.redact,
                price_tier_of: &price_tiers,
                degraded_providers: &degraded,
                token_ratio_backfill: &st.token_ratio_backfill,
                est_observed,
                active: &active_ids,
            };
            let d = st.engine.decide(input);
            crate::state::note_judge(&st, d.judgment.judge_source);
            live.describe(&d.chosen, &session_key, &agent_hdr);
            // 粘性的度：仅非粘性的重新判定重置轮次；粘性延续扣减——
            // 否则每轮 put 满额轮次，turns_left 永不触 0，粘性无度
            let turns_left = if d.sticky {
                st.sessions
                    .get(&sticky_key)
                    .map(|s| s.turns_left.saturating_sub(1))
                    .unwrap_or(st.config.policy.sticky_turns)
            } else {
                st.config.policy.sticky_turns
            };
            st.sessions.put(
                &sticky_key,
                StickyState {
                    chosen: d.chosen.clone(),
                    est_tokens_band: tok::tokens_band(est),
                    turns_left,
                    tools_sig,
                    domain: d.judgment.domain,
                    difficulty: d.difficulty_eff,
                    flags: evolve_core::types::JudgmentFlags::from_judgment(&d.judgment),
                    est_tokens: est,
                },
            );
            d
        }
        Target::Direct(dec) => dec.clone(),
    };
    decision.est_input_tokens = est;
    let decision_ms = decision_started.elapsed().as_millis() as u64;
    let preprocess_ms = decision_started
        .saturating_duration_since(started)
        .as_millis() as u64;

    let original_choice = decision.chosen.clone();
    let is_stream = parsed.get("stream").and_then(|v| v.as_bool()).unwrap_or(false);

    // 胜出队列（用户裁决 2026-10）：最终决策的 chain 即胜出队列——按序尝试，
    // 失败顺延下一个；不可重试错误（配额/协议不支持）当场剔除该队列项（配额走
    // dead_providers 账号级熔断，协议不支持走 exclude_models 整体剔除）。队列尝试
    // 上限 = min(fallback_depth, WINNER_QUEUE_MAX)；队列整体失败或为空后，才尝试
    // 队列外模型（sweep 段，评分序 + 目录长尾，防单点大面积不可用）。
    const WINNER_QUEUE_MAX: usize = 5;
    let (mut attempts, sweep_from): (Vec<String>, usize) = match &target {
        Target::Auto(_) => {
            let queue_max =
                (st.config.policy.fallback_depth.max(3) as usize).min(WINNER_QUEUE_MAX);
            let mut list = winner_queue(&decision.chain, &decision.scores, queue_max);
            // 链外兜底（用户裁决）：胜出队列（链+评分补位）全灭时，目录里其余
            // provider 的模型仍一试（冷却中的 zhipu / 未入评分的 MiniMax 也纳入）——
            // 队尾追加即天然只在全灭路径被尝试；上限 6 个防长尾延迟。自指 base_url
            // 的不试（无凭据模型与主链同权照试——本地上游可免鉴权）。
            let mut seen: std::collections::HashSet<String> = list.iter().cloned().collect();
            let sweep_from = list.len();
            let cat = st.engine.catalog_snapshot();
            let self_addr = format!("{}:{}", st.config.server.host, st.config.server.port);
            // 兜底候选（2026-10 修复）：旧实现是**盲取"目录前 6 个"**（不看窗口/
            // 健康/供应商）——实测事故：一个 27 万 token 的请求被拖进 200k 窗口的
            // 模型白试 429/404/400，且 opencode-go-github / MiniMax 全程没轮到
            // （6 个名额被目录顺序靠前的供应商吃满）。现改为：**硬约束先过**
            // （窗口/视觉）+ **按 provider 轮转**（每家先取 1 个再取第 2 个）+
            // 无冷却的 provider 优先——真正"还有地方可试"。
            let max_out = parsed
                .get("max_tokens")
                .and_then(|v| v.as_u64())
                .unwrap_or(4096)
                .min(65_536);
            for id in sweep_candidates(
                &cat,
                &seen,
                est,
                max_out,
                &decision.judgment,
                &self_addr,
                &st,
            ) {
                seen.insert(id.clone());
                list.push(id);
            }
            (list, sweep_from)
        }
        Target::Direct(_) => (vec![decision.chosen.clone()], usize::MAX),
    };

    // 价位闸临近模型预放行（用户裁决 2026-10-20）：价位闸从"掐死"降级为
    let mut skipped: Vec<String> = Vec::new();
    // 候选遍历游标 + 评分榜续补游标（2026-10-23）：初始补 scored 前 12 名，
    // 队列走完后从第 13 名继续续补（跳过冷却/已入列）——拉黑完成后自动
    // 往下走榜单 = 用最新失败信息重新决策
    let mut cand_idx = 0usize;
    let mut top_up_cursor = 12usize;
    // "压排序"——每 provider 一个最临近的代表补到链尾，任何请求的候选集
    // 都覆盖全部 provider；精确档全灭时直接落到位，不用先空转一轮等救援
    let price_near = price_gated_retries(&decision.filtered, &decision.scores, &attempts, 6);
    if !price_near.is_empty() {
        tracing::info!(models = ?price_near, "price-gated near models appended to chain tail");
        attempts.extend(price_near);
    }

    // 全灭兜底（用户裁决 2026-10）：配额被硬滤（剩余不足/耗尽）的模型排
    // 最后——主链+清扫全灭后再试一轮（学习到的配额可能过期/已重置，
    // 死马当活马医，延迟换成功率）。去重、上限 6。
    if !attempts.is_empty() {
        let mut retry: Vec<String> = Vec::new();
        for f in &decision.filtered {
            if f.cause.starts_with("quota remaining")
                && !attempts.iter().any(|a| a == &f.model)
                && !retry.contains(&f.model)
                && retry.len() < 6
            {
                retry.push(f.model.clone());
            }
        }
        if !retry.is_empty() {
            tracing::info!(models = ?retry, "quota-filtered models appended as last-resort");
            attempts.extend(retry);
        }
        // 上下文闸放宽（2026-10-19）：need 的余量 stacking（校准×比值中位×1.1+output）
        // 会把"窗口真实够用"的模型挤出局——实证：683K 真实上下文被算成 1.01M need，
        // 1M 窗口的 glm-5.3-flash 差 0.8% 被滤、1.048M 的 kimi-k3 差 14% 被滤，
        // 唯二幸存候选又恰好月配额耗尽 → 会话全灭。救命轮按 **窗口 ≥ 裸 est** 重判
        // （裸 est 是不被校准污染的诚实下界），按窗口上界降序补队尾。
        let ctx_retry = context_gated_retries(&decision.filtered, &attempts, est, 4);
        if !ctx_retry.is_empty() {
            tracing::info!(models = ?ctx_retry, "context-gated models appended as last-resort");
            attempts.extend(ctx_retry);
        }
        // 链耗尽续跑（2026-10-23 实证：top-3 全灭时 zhipu glm-5.3-flash 排在
        // 榜单第 8 名从未被试到，而同池直连它是 200）：**评分榜 top-12 强制
        // 入列**——首个版本用"总尝试数 ≤12"做预算，但价格邻近/上下文救命
        // 扩展先行占满了名额，扩展自身变成零贡献。改为按榜计数：scored 前
        // 12 名（去重后）无论如何都在队列里，故障转移真正走完榜单。
        // 本轮 walk 中刚被标记冷却的模型不再入列（用户定：明确不可重试的
        // 不进重试队列）——全部入列后仍无可走时保留原列表兜底。
        {
            const SCORED_TOP_N: usize = 12;
            let now = evolve_memory::health::now();
            let health = st.health.snapshot();
            let mut counted = 0usize;
            for sc in decision.scored.iter() {
                if counted >= SCORED_TOP_N {
                    break;
                }
                counted += 1;
                if attempts.contains(&sc.model_id) {
                    continue;
                }
                // 冷却中（含本轮 walk 刚拉黑的）→ 跳过，不排进重试队列
                if health
                    .get(&sc.model_id)
                    .map(|h| !h.available(now))
                    .unwrap_or(false)
                {
                    skipped.push(format!("{}(blacklisted: cooling after failure)", sc.model_id));
                    continue;
                }
                attempts.push(sc.model_id.clone());
            }
        }
    }

    // 会话级拉黑剔除（2026-10）：该会话历史与某模型不兼容（请求形状类 4xx）→
    // 本会话不再试它；**其它会话不受影响**。全部被拉黑时保留原列表（宁可再试
    // 一次也不让会话彻底无路可走），并把剔除原因写进 skipped。
    {
        let now_q = evolve_memory::health::now();
        if let Ok(mut q) = st.session_quarantine.lock() {
            if let Some(per_model) = q.get_mut(&session_key) {
                per_model.retain(|_, (_, until)| *until > now_q);
                if per_model.is_empty() {
                    q.remove(&session_key);
                } else {
                    let blocked: std::collections::HashMap<String, String> = per_model
                        .iter()
                        .map(|(m, (r, _))| (m.clone(), r.clone()))
                        .collect();
                    let kept: Vec<String> = attempts
                        .iter()
                        .filter(|id| !blocked.contains_key(*id))
                        .cloned()
                        .collect();
                    if kept.is_empty() {
                        tracing::warn!(session = %session_key, "all candidates are session-quarantined; retrying anyway");
                    } else {
                        for (id, r) in &blocked {
                            if attempts.contains(id) {
                                skipped.push(format!("{id}(session-quarantined: {r})"));
                            }
                        }
                        attempts = kept;
                    }
                }
            }
        }
    }

    let mut last_error: Option<(StatusCode, Bytes)> = None;
    // 主因追踪（2026-10 用户裁决）：最终失败状态按**主因优先级**上报，而不是
    // 兜底段最后一个尝试的状态码。实测误导：整链配额耗尽 + 兜底段撞一个 400
    // （如 deepseek 的 reasoning_content）时，客户端只看到 400，误以为请求格式错。
    // 优先级：配额/付款 5 > 上下文超限 4 > 鉴权 3 > 网络 2 > 瞬态 1 > 其它 0
    let mut primary: Option<(u8, StatusCode, String)> = None;
    // ⑤ 质量级联兜底：成功但质量差的响应暂存，链耗尽时返回（降质好过硬报错）
    let mut quality_fallback: Option<(StatusCode, axum::http::HeaderMap, Bytes)> = None;
    // context overflow on one candidate means the session needs MORE window:
    // later candidates smaller than the est are pointless, skip them
    let mut min_context_needed: Option<u64> = None;

    // Cross-protocol: OpenAI ingress -> Anthropic upstream (Switchyard IR)
    // provider 账户级熔断：配额/余额/鉴权/限流是 provider 级共享资源，
    // 同 provider 的其他模型不再重复尝试（本请求内）
    let mut dead_providers: std::collections::HashSet<String> = std::collections::HashSet::new();
    // 网络级熔断（用户裁决）：transport 错误是 baseUrl 级故障（同一条网络
    // 路径），换模型重试毫无意义——按 base_url 熔断
    let mut dead_routes: std::collections::HashSet<String> = std::collections::HashSet::new();
    // 自递归防护（审查 M-6）：候选 base_url 指向自身监听地址时出局——
    // 不依赖用户给网关条目起什么名字（命名约定不可靠）
    let self_addr = format!("{}:{}", st.config.server.host, st.config.server.port);
    'cands: while cand_idx < attempts.len() {
        // 队列走完 → 评分榜续补下一批（跳过冷却/已入列）：拉黑完成后自动
        // 往下走榜单 = 用最新失败信息重新决策（榜单即决策）。补尽即止。
        if cand_idx >= attempts.len() {
            let now = evolve_memory::health::now();
            let health = st.health.snapshot();
            let mut added = 0usize;
            while top_up_cursor < decision.scored.len()
                && added < 6
                && attempts.len() < 36
            {
                let sc = &decision.scored[top_up_cursor];
                top_up_cursor += 1;
                if health
                    .get(&sc.model_id)
                    .map(|h| !h.available(now))
                    .unwrap_or(false)
                {
                    continue; // 冷却中（本轮刚拉黑）→ 不入列
                }
                if attempts.contains(&sc.model_id) {
                    continue;
                }
                attempts.push(sc.model_id.clone());
                added += 1;
            }
            if cand_idx >= attempts.len() {
                break 'cands;
            }
        }
        let this_idx = cand_idx; // body 内索引语义保持（quality 级联/sweep 判定用）
        cand_idx += 1;
        let cand = &attempts[this_idx];
        let Some(record) = st.catalog_get(cand) else { continue };
        if record.base_url.contains(&self_addr) || record.base_url.contains("127.0.0.1:8787") {
            skipped.push(format!("{cand}(self-loop: points back at this gateway)"));
            continue;
        }
        if dead_providers.contains(&record.provider) {
            skipped.push(format!("{cand}(provider {} account-level failure)", record.provider));
            continue;
        }
        // 路由粒度 = (base_url, 账号)：同 baseURL 的双订阅（不同 apiKey）是
        // 独立账号——一个账号的传输失败不该连坐另一个（2026-10-20 实证：一次
        // 传输失败把两个订阅的免费模型全拉黑 → 只剩欠费户 → 全灭）
        let route_key = format!("{}|{}", record.base_url, record.provider);
        if dead_routes.contains(&route_key) {
            skipped.push(format!(
                "{cand}(route {} network failure, account {})",
                record.base_url, record.provider
            ));
            continue;
        }
        if let Some(need) = min_context_needed
            && record.context_window.map(|w| w < need).unwrap_or(true)
        {
            skipped.push(format!("{cand}(window < {need})"));
            continue;
        }
        let to_anthropic = record.protocol == Protocol::Anthropic;
        let keys = record.key_values();
        let key_len = keys.len().max(1);
        let key_start = st.key_start(cand, key_len);
        let health_snap = st.health.snapshot();
        let now_ms = evolve_memory::health::now();

        // multi-key pool: try each key in rotation before falling to the
        // next candidate (per model×key health bookkeeping)
        for key_off in 0..key_len {
        let key_idx = (key_start + key_off) % key_len;
        let key_value = keys.get(key_idx).cloned();
        let health_id = if keys.is_empty() {
            cand.clone()
        } else {
            format!("{cand}{KEY_SEP}{key_idx}")
        };
        if let Some(h) = health_snap.get(&health_id)
            && !h.available(now_ms)
        {
            // 链外兜底段（cand_idx >= sweep_from）冷却不再拦——全灭好过硬报错，
            // 单次尝试不会形成打爆（失败会重新记账冷却）
            let sweep = this_idx >= sweep_from;
            if !sweep {
                let remaining = h
                    .cooldown_remaining_ms(now_ms)
                    .map(|ms| format!(" for {}s", ms / 1000))
                    .unwrap_or_default();
                skipped.push(format!("{cand}[{key_idx}]({}){remaining}", h.kind.label()));
                continue;
            }
            skipped.push(format!("{cand}[{key_idx}](last-resort, {} cooling)", h.kind.label()));
        }
        // text-only 上游 + 本轮不需要识图（历史图片不要求识别）→ 转发前把
        // 历史消息里的图片替换成占位文本，否则上游按非法 content.type 拒收
        // （zhipu `messages.content.type 参数非法，取值范围 ['text']` 实证）
        let strip_historical_images = record.tiers.vision < evolve_core::thresholds::VISION
            && decision.judgment.needs_vision <= evolve_core::thresholds::VISION;
        let (fwd_body, url) = if to_anthropic {
            let mut openai_value: Value = serde_json::from_slice(&body).unwrap_or(Value::Null);
            if let Some(obj) = openai_value.as_object_mut() {
                obj.insert("model".into(), json!(record.upstream_model));
                obj.entry("max_tokens").or_insert(json!(4096));
            }
            // text-only 上游 + 本轮不需要识图 → 历史图片对上游非法（zhipu 400 实证）
            if strip_historical_images {
                strip_image_parts(&mut openai_value);
            }
            let translated = crate::translate::Translator::global()
                .request_openai_to_anthropic(&openai_value);
            match translated {
                Ok(v) => (v.to_string().into_bytes(), format!("{}/messages", record.base_url)),
                Err(e) => {
                    tracing::warn!(model = %cand, error = %e, "openai->anthropic translation failed");
                    skipped.push(format!("{cand}(translate)"));
                    last_error = Some((StatusCode::BAD_GATEWAY, Bytes::new()));
                    continue;
                }
            }
        } else {
            let rewritten = if strip_historical_images {
                rewrite_model_field_images_stripped(&body, &record.upstream_model)
            } else {
                rewrite_model_field(&body, &record.upstream_model)
            };
            (rewritten, format!("{}/chat/completions", record.base_url))
        };
        let cross = to_anthropic;

        // 检查器：最后一次尝试的转发现场（URL / 上游头 / 转换或重写后的 body）
        {
            let upstream_headers = if to_anthropic {
                json!({"content-type": "application/json", "anthropic-version": "2023-06-01",
                       "x-api-key": key_value.as_deref().map(|k| format!("«masked len={}>", k.len()))})
            } else {
                let mut h = json!({"content-type": "application/json"});
                if record.base_url.contains("opencode.ai/zen") {
                    h["x-opencode-session"] = json!(session_key);
                }
                if let Some(k) = key_value.as_deref() {
                    h["authorization"] = json!(format!("«masked len={}>", k.len()));
                }
                h
            };
            crate::request_inspect::with_entry(&inspect, |g| {
                g.upstream_url = Some(url.clone());
                g.upstream_headers = Some(serde_json::to_string_pretty(&upstream_headers).unwrap_or_default());
                // 去掉 model 字段后比较：仅 model 名不同 = "语义一致"（只是名字映射），
                // 结构性变化（图片剥离 / 协议转换 / 字段增删）才是真转换。
                // 语义一致 → 不存重复体（只记标记），UI 提示"仅 model 名改写"
                let model_name_only = match (
                    serde_json::from_slice::<serde_json::Value>(&body).ok(),
                    serde_json::from_slice::<serde_json::Value>(&fwd_body).ok(),
                ) {
                    (Some(client), Some(fwd)) => {
                        let strip = |v: &serde_json::Value| {
                            let mut c = v.clone();
                            if let Some(o) = c.as_object_mut() { o.remove("model"); }
                            c
                        };
                        strip(&client) == strip(&fwd)
                    }
                    _ => false,
                };
                g.upstream_body = if !model_name_only && !redact_on {
                    Some(String::from_utf8_lossy(&fwd_body).into_owned())
                } else {
                    None
                };
                g.upstream_same = model_name_only;
                g.translated = to_anthropic;
            });
        }

        let mut req = st.http.post(&url).header("content-type", "application/json");
        if to_anthropic {
            req = req.header("anthropic-version", "2023-06-01");
            if let Some(key) = key_value.as_deref() {
                req = req.header("x-api-key", key);
            }
        } else {
            // request shaping: opencode zen upstreams require a session header
            if record.base_url.contains("opencode.ai/zen") {
                req = req.header("x-opencode-session", &session_key);
            }
            if let Some(key) = key_value.as_deref() {
                req = req.bearer_auth(key);
            }
        }

        let upstream = req.body(fwd_body).send().await;
        let resp = match upstream {
            Ok(r) => r,
            Err(e) => {
                tracing::warn!(model = %cand, error = %e, "upstream transport failure");
                st.health.mark_failure(
                    cand,
                    Failure { kind: HealthKind::Transient, message: "transport error".into(), until_epoch_ms: None },
                );
                // provider 级（连续 3 次才降级；降级后定时探活，不永久沉默）
                note_provider_issue(&st, &record.provider, &record.base_url, evolve_memory::quota::ProviderIssue::Network);
                // 端点被兄弟账号证明仍可达（近 2 分钟内有 200）→ 这次传输失败是
                // 账号/模型级抖动，不拉黑整条路由（否则同账号其他模型也被连坐）
                if !endpoint_recently_ok(&record.base_url) {
                    dead_routes.insert(format!("{}|{}", record.base_url, record.provider));
                }
                skipped.push(format!("{cand}(transport)"));
                if primary.as_ref().map(|(p, _, _)| 2 > *p).unwrap_or(true) {
                    primary = Some((2, StatusCode::BAD_GATEWAY, format!("network unreachable ({cand})")));
                }
                last_error = Some((StatusCode::BAD_GATEWAY, Bytes::new()));
                continue;
            }
        };

        let status = resp.status();

        // 检查器：上游响应状态 + 头（最后一次尝试为准）
        crate::request_inspect::with_entry(&inspect, |g| {
            g.status = status.as_u16();
            g.resp_status = Some(status.as_u16());
            let rh: serde_json::Map<String, Value> = resp
                .headers()
                .iter()
                .map(|(k, v)| {
                    (k.as_str().to_string(),
                     serde_json::Value::String(
                         v.to_str().map(|s| s.to_string()).unwrap_or("<binary>".into())))
                })
                .collect();
            g.resp_headers = Some(serde_json::to_string_pretty(&rh).unwrap_or_default());
        });

        // 端点级存活记录（2026-10-20）：同 baseURL 的多账号（同 plan 双订阅）
        // 共享端点命运——任一账号 200 即证明端点可达，兄弟账号的传输失败
        // 不应升级成 provider 降级（见 note_provider_issue 的豁免检查）
        if status.is_success() && !record.base_url.is_empty() {
            record_endpoint_ok(&record.base_url);
            // 真流量自愈（2026-10-21）：200 本身证明账号+端点都活着——该 provider
            // 若还挂着 Network 降级（误伤残留），立即清除，不等探活周期
            if st
                .quota
                .provider_issue_of(&record.provider)
                .map(|(i, _, _)| i == evolve_memory::quota::ProviderIssue::Network)
                .unwrap_or(false)
            {
                st.quota.provider_issue_probe_result(&record.provider, true, evolve_memory::quota::now_ms());
                st.events.record(json!({
                    "kind": "provider_state_changed",
                    "provider": record.provider, "state": "ok", "source": "real traffic 200",
                }));
                tracing::info!(provider = %record.provider, "network degradation cleared by real traffic 200");
            }
        }

        if fallback_eligible(status.as_u16()) {
            // 失败路径也捕获 provider 配额头（用户裁决：429/402 的 reset/remaining
            // 是权威信号——须在 bytes() 消耗 resp 之前读取头部）
            let failure_windows = evolve_memory::QuotaLedger::parse_headers(resp.headers());
            // 重置时间提取：retry-after(秒) 优先，回退 x-ratelimit-reset-*（纪元 ms 或时长 ms）
            let retry_after_ms = resp
                .headers()
                .get("retry-after")
                .and_then(|v| v.to_str().ok())
                .and_then(|v| v.parse::<u64>().ok())
                .map(|s| s * 1000)
                .or_else(|| {
                    ["x-ratelimit-reset-requests", "x-ratelimit-reset-tokens", "x-ratelimit-reset"]
                        .iter()
                        .find_map(|h| resp.headers().get(*h))
                        .and_then(|v| v.to_str().ok())
                        .and_then(|v| v.parse::<u64>().ok())
                        .map(|n| {
                            // review#20：秒级 epoch（1e9–1e12）会被旧阈值当 ms 用成 ~0——
                            // 分档判断：>1e12 为 ms；1e9–1e12 为秒 ×1000；更小为相对秒
                            if n > 1_000_000_000_000 {
                                n
                            } else if n > 1_000_000_000 {
                                n * 1000
                            } else {
                                n
                            }
                        })
                });
            let err_body = resp.bytes().await.unwrap_or_default();
            let snippet =
                String::from_utf8_lossy(&err_body.slice(..err_body.len().min(MAX_ERROR_BODY))).into_owned();
            // 400 响应体入日志——上游拒绝原因需要可见（用户裁决：不可黑盒）
            // review#4：按字符截断——字节切片会在多字节字符（中文错误体）中间 panic
            if status.as_u16() == 400 {
                let head: String = snippet.chars().take(300).collect();
                tracing::warn!(model = %cand, status = 400, body = %head, "upstream 400 — request rejected");
            }
            let mut failure = classify_failure(status.as_u16(), &snippet, retry_after_ms);
            // 协议不支持（Responses-only 等）**且用户未显式配置**的模型：进程内
            // 整体剔除（用户裁决 2026-10）——目录/路由/面板/探活一并消失，不落冷却；
            // 仅存内存，网关重启复位（Source::User 的用户显式配置保留，走下方 5h
            // 冷却路径）。此类错误按模型不可重试——换 key 无意义，直接跳出 key 池。
            if status.as_u16() == 400
                && failure.kind == HealthKind::Unsupported
                && record.source != evolve_core::types::Source::User
            {
                let reason = format!("protocol unsupported ({})", record.upstream_model);
                tracing::info!(model = %cand, "protocol-unsupported model excluded (in-process)");
                st.events.record(json!({
                    "kind": "model_excluded",
                    "model": cand,
                    "upstream_model": record.upstream_model,
                    "reason": reason,
                    "session": session_key,
                }));
                st.exclude_models(vec![(cand.clone(), reason)]);
                skipped.push(format!("{cand}(protocol-unsupported, excluded)"));
                last_error = Some((map_status(status), err_body));
                break; // 该模型（含其它 key）整体剔除，不再重试
            }
            // w3 404 语义二分（用户裁决）：
            //  - 模型仍在 provider 的模型列表里 → 临时不可用，5h 冷却后自动重试
            //  - 列表里没有（上游已移除）→ 永久黑名单（until=哨兵），仅手动恢复
            if status.as_u16() == 404 || failure.kind == HealthKind::Unsupported {
                if crate::meta::provider_lists_model(&st, &record.provider, &record.upstream_model) {
                    failure = Failure {
                        kind: HealthKind::Unsupported,
                        message: if status.as_u16() == 404 {
                            format!("{} not serving now (still listed; retry in 5h)", record.upstream_model)
                        } else {
                            format!("{} protocol unsupported (still listed; retry in 5h)", record.upstream_model)
                        },
                        until_epoch_ms: Some(evolve_memory::quota::now_ms() + 5 * 3600 * 1000),
                    };
                } else {
                    failure = Failure {
                        kind: HealthKind::Unsupported,
                        message: format!("{} removed upstream (manual restore required)", record.upstream_model),
                        until_epoch_ms: Some(evolve_memory::health::HealthRegistry::PERMANENT_UNTIL),
                    };
                }
            }
            // w3 配额类（API 型 provider）：既不采信上游长 retry-after，也不落
            // 健康冷却/探测周期（用户裁决：会周期性查 API 就不需要探活）——
            // 当场拉一次官方配额 API 定夺：API 说满 → gate 关（账号级窗口）；
            // API 说有余量 → 保持可用（不因单次 429 误伤）
            let api_backed_quota = failure.kind == HealthKind::QuotaExhausted
                && crate::meta::provider_is_api_backed(&st, &record.base_url);
            // 无 API 型：信任上游给的恢复期（唯一可信信号——月窗 9d9h 如实采用，
            // 用户裁决）；探活由 watchdog 独立周期探测兜底（不受此冷却阻挡）
            // provider 级故障：鉴权/区域被拒（与配额、模型级问题分开记账）
            match failure.kind {
                HealthKind::AuthFailed => note_provider_issue(
                    &st, &record.provider, &record.base_url, evolve_memory::quota::ProviderIssue::Auth,
                ),
                _ => {}
            }
            tracing::warn!(model = %cand, status = status.as_u16(), kind = failure.kind.label(), "upstream failure, marking health");
            st.events.record(json!({
                "kind": "upstream_error",
                "model": cand,
                "upstream_model": record.upstream_model,
                "status": status.as_u16(),
                "health": failure.kind.label(),
                "message": failure.message,
                "session": session_key,
            }));
            let health_id = if keys.is_empty() {
                cand.clone()
            } else {
                format!("{cand}{KEY_SEP}{key_idx}")
            };
            if failure.kind == HealthKind::ContextOverflow {
                min_context_needed = Some(est.max(1));
            }
            if matches!(
                failure.kind,
                HealthKind::QuotaExhausted
                    | HealthKind::PaymentRequired
                    | HealthKind::AuthFailed
                    | HealthKind::RateLimited
            ) {
                dead_providers.insert(record.provider.clone());
            }
            // 失败响应同样捕获 provider 配额头（用户裁决：429/402 的 reset/remaining
            // 是权威信号，丢弃会导致前端展示与真实状态脱钩）
            {
                if !failure_windows.is_empty() {
                    st.quota.observe(&format!("{cand}{KEY_SEP}{key_idx}"), failure_windows.clone());
                    st.quota.observe_provider(&record.provider, &failure_windows);
                }
            }
            // w2 gate：上游配额信号 → 账号级关闸（面板状态与路由同步）
            // （置于 mark_failure 之前——Failure 非 Copy，先取用再做健康记账）
            if matches!(failure.kind, HealthKind::QuotaExhausted | HealthKind::PaymentRequired) {
                let provider_name = &record.provider;
                if !provider_name.is_empty() {
                    let pk_gate = evolve_core::plans::plan_key_for(&record.base_url).unwrap_or_default();
                    let api_backed = matches!(
                        pk_gate,
                        "minimax-coding" | "zhipu-coding" | "zai-devpack" | "opencode-go"
                    );
                    let reset_ms = failure_windows
                        .iter()
                        .filter_map(|w| w.reset_epoch_ms)
                        // body 内嵌重置时刻（OpenRouter 429 的 metadata 头）也计入——
                        // 免费日限的重置只在 body 里，HTTP 头上没有
                        .chain(failure.until_epoch_ms)
                        .max();
                    if st.quota.gate_get(provider_name).is_none() {
                        st.events.record(json!({
                            "kind": "quota_state_changed",
                            "provider": provider_name, "state": "quota_limited",
                            "source": if api_backed { "api" } else { "upstream" },
                        }));
                    }
                    st.quota.gate_mark_limited(provider_name, api_backed, reset_ms, evolve_memory::quota::now_ms());
                }
            }
            // skip 标签：api-backed 配额型失败不落健康冷却（watchdog/API 复核
            // 接管）——标签改用失败类别 + api 标注，避免空括号
            let kind_label = if api_backed_quota {
                format!("{} (api-backed)", failure.kind.label())
            } else {
                st.health.mark_failure(&health_id, failure.clone());
                st.health
                    .snapshot()
                    .get(&health_id)
                    .map(|h| h.kind.label().to_string())
                    .unwrap_or_default()
            };
            if api_backed_quota {
                // 立即用官方 API 复核（异步；gate 由 API 数据接管，不落健康冷却）
                let st2 = st.clone();
                let key_name = record.provider.clone();
                let bu = record.base_url.clone();
                let pk_now = evolve_core::plans::plan_key_for(&bu).unwrap_or_default().to_string();
                tokio::spawn(async move {
                    let _ = crate::meta::refresh_observed(
                        &st2, &key_name, &bu, &pk_now, None, evolve_memory::quota::now_ms(),
                    ).await;
                });
            }
            // 会话级拉黑（2026-10）：400 且非配额/鉴权/上下文/不支持类 →
            // 1210 类"只收纯文本"拒收（2026-10-23 实证：zhipuai glm-5.3
            // `code 1210 messages.content.type 参数非法，取值范围 ['text']`）：
            // 能力卡声称支持视觉但端点实测拒收非文本 → 能力自修正
            //（vision → 0，内存态，重启后由首个 400 重学习）：后续请求
            // ①历史图片预剥离（strip 条件读 tiers.vision）②真视觉请求被
            // 视觉闸正确排除。本会话不再隔离——修正后纯文本历史仍可用。
            let text_only_rejection = status.as_u16() == 400
                && (snippet.contains("1210")
                    || (snippet.contains("content.type") && snippet.contains("'text'")));
            if text_only_rejection {
                let mut models = st.engine.catalog_snapshot();
                let mut flipped = false;
                if let Some(m) = models.iter_mut().find(|m| m.id == *cand)
                    && m.tiers.vision >= evolve_core::thresholds::VISION
                {
                    m.tiers.vision = 0.0;
                    flipped = true;
                }
                if flipped {
                    st.engine.replace_catalog(models);
                    tracing::warn!(model = %cand, "capability self-correction: endpoint rejects non-text content — vision tier zeroed (pre-strip + vision-gate)");
                    st.events.record(json!({
                        "kind": "capability_corrected", "model": cand,
                        "field": "vision", "to": 0.0, "source": "upstream text-only 400",
                    }));
                }
            }
            // 属"请求形状"问题（典型 deepseek `reasoning_content` 必须回传），
            // 该会话历史与这个模型不兼容 → 本会话拉黑 30min，别再反复白试。
            // （1210 纯文本拒收不隔离：能力已自修正，见上）
            if status.as_u16() == 400
                && !text_only_rejection
                && !matches!(
                    failure.kind,
                    HealthKind::QuotaExhausted
                        | HealthKind::PaymentRequired
                        | HealthKind::AuthFailed
                        | HealthKind::ContextOverflow
                        | HealthKind::Unsupported
                )
            {
                const QUARANTINE_MS: u64 = 30 * 60 * 1000;
                let until = evolve_memory::health::now() + QUARANTINE_MS;
                let reason = format!("request-shape rejected by {} (http 400)", record.upstream_model);
                if let Ok(mut q) = st.session_quarantine.lock() {
                    // 顺手清过期项 + 上限（会话数可能很多；无清理会无界增长）。
                    // `until` = now + QUARANTINE_MS，反推 now 作为过期阈值
                    let now_cut = until.saturating_sub(QUARANTINE_MS);
                    q.retain(|_, m| {
                        m.retain(|_, (_, u)| *u > now_cut);
                        !m.is_empty()
                    });
                    if q.len() > 200 && !q.contains_key(&session_key) {
                        // 超上限时丢掉一个任意旧会话（近似 LRU：HashMap 无法取最旧）
                        if let Some(k) = q.keys().next().cloned() {
                            q.remove(&k);
                        }
                    }
                    q.entry(session_key.clone())
                        .or_default()
                        .insert(cand.clone(), (reason.clone(), until));
                }
                st.events.record(json!({
                    "kind": "session_quarantined",
                    "session": session_key, "model": cand,
                    "reason": reason, "until_ms": until,
                }));
                tracing::warn!(model = %cand, session = %session_key,
                    "model quarantined for this session only (request-shape 400)");
            }
            skipped.push(format!("{cand}[{key_idx}]({kind_label})"));
            {
                let prio = match failure.kind {
                    HealthKind::QuotaExhausted | HealthKind::PaymentRequired => 5u8,
                    HealthKind::ContextOverflow => 4,
                    HealthKind::AuthFailed => 3,
                    HealthKind::Transient => 1,
                    _ => 0,
                };
                if primary.as_ref().map(|(p, _, _)| prio > *p).unwrap_or(true) {
                    primary = Some((prio, map_status(status), kind_label.clone()));
                }
            }
            last_error = Some((map_status(status), err_body));
            continue; // next key in pool, then next candidate
        }

        // quota window learning from success headers (before body consumed)
        let windows = evolve_memory::QuotaLedger::parse_headers(resp.headers());
        st.quota.observe(&format!("{cand}{KEY_SEP}{key_idx}"), windows.clone());
        // provider 实时配额（panel 配额展示以此为准，不靠本地 token 累加估算）
        st.quota.observe_provider(&record.provider, &windows);
        // success path（review#15：仅 2xx 记健康——409/422/451 等非 fallback
        // 状态码此前 mark_ok 会清零冷却并掩盖上游异常）
        let success_health_id = if keys.is_empty() {
            cand.clone()
        } else {
            format!("{cand}{KEY_SEP}{key_idx}")
        };
        if status.is_success() {
            st.health.mark_ok(&success_health_id);
            if st.quota.provider_success(&record.provider) {
                st.events.record(json!({
                    "kind": "provider_state_changed",
                    "provider": record.provider, "state": "ok", "source": "routed_success",
                }));
            }
        }
        // w2 gate：被路由到且成功 = 真实状态的反证——闸门立即开，状态不再显示
        // 配额受限；API 型当场强制刷新官方配额（充值/窗口重置即时可见）
        if status.is_success() && !record.provider.is_empty() {
            let gated = st.quota.gate_get(&record.provider).map(|g| !g.windows_full.is_empty()).unwrap_or(false);
            if gated {
                st.quota.gate_probe_success(&record.provider, evolve_memory::quota::now_ms());
                st.events.record(json!({
                    "kind": "quota_state_changed",
                    "provider": record.provider, "state": "ok", "source": "routed_success",
                }));
                let pk_ok = evolve_core::plans::plan_key_for(&record.base_url).unwrap_or_default();
                if matches!(
                    pk_ok,
                    "minimax-coding" | "zhipu-coding" | "zai-devpack" | "opencode-go"
                ) {
                    let st2 = st.clone();
                    let bu = record.base_url.clone();
                    let key_name = record.provider.clone();
                    tokio::spawn(async move {
                        let _ = crate::meta::refresh_observed(
                            &st2, &key_name, &bu, &pk_ok, None, evolve_memory::quota::now_ms(),
                        ).await;
                    });
                }
            }
        }
        decision.chosen = cand.clone();
        decision.upstream_model = record.upstream_model.clone();
        decision.id = format!("{}-{}", decision.id, skipped.len());

        // 检查器：回填 decision_id / 最终模型 / 本次判定的量级
        crate::request_inspect::with_entry(&inspect, |g| {
            g.decision_id = decision.id.clone();
            g.model = decision.chosen.clone();
        });

        // 响应头到达时刻 = 非流式 TTFT / 流式首块前基准
        let head_ms = started.elapsed().as_millis() as u64;
        let telem = std::sync::Arc::new(std::sync::Mutex::new(crate::stream::Telemetry {
            decision_id: decision.id.clone(),
            session: session_key.clone(),
            chosen: decision.chosen.clone(),
            upstream_model: record.upstream_model.clone(),
            est_tokens: est,
            sticky: decision.sticky,
            stream: is_stream,
            started,
            ttft_ms: Some(head_ms as u128),
            bytes: 0,
            status: status.as_u16(),
            usage: None,
            est_cost_usd: None,
            translated: cross.then(|| "anthropic->openai".to_string()),
            agent: Some(agent_hdr.clone()),
            plan_key: evolve_core::plans::plan_key_for(&record.base_url).map(|k| k.to_string()),
            preprocess_ms: Some(preprocess_ms),
            decision_ms: Some(decision_ms),
            judge_ms: Some(decision.judge_ms),
            est_raw: Some(est_raw),
            extra: {
                let mut ex = json!({
                    "reason": decision.reason,
                    "scores": decision.scores,
                    "chain": decision.chain,
                    "judge": decision.judgment.judge_source,
                    "judgment": decision.judgment,
                    "scoreboard": decision.scored,
                    "weights": decision.weights,
                    "difficulty_eff": decision.difficulty_eff,
                    "filtered": decision.filtered,
                    "funnel": decision.funnel,
                    "explore_gate": decision.explore_gate,
                    // ⑥ regret proxy（ACRouter regret-style metrics）：
                    // 公式最优分 − 实际选择分 = 本次路由的代价（0 = 无悔）
                    "regret": decision.scored.first().and_then(|top| {
                        decision.scored.iter()
                            .find(|s| s.model_id == decision.chosen)
                            .map(|c| (top.score - c.score) as f64)
                    }).unwrap_or(0.0),
                });
                // 决策时点的模型打标（能力+价位）：请求与标签同帧落库，
                // 事后标签演进不影响历史请求的可解释性（用户裁决）
                {
                    let snap = st.engine.catalog_snapshot();
                    let tags = crate::meta::tags_for_catalog(&st, &snap);
                    let cap_label = tags.get(&decision.chosen).map(|(cap, _)| cap.label());
                    ex["model_tags"] = match tags.get(&decision.chosen) {
                        Some((cap, price)) => json!({
                            "capability": cap.label(), "price_tier": price.label(),
                        }),
                        None => json!(null),
                    };
                    // 派生标签（难度/域/特征/判定来源/匹配）单一事实源：面板与 Web 只渲染
                    ex["tags"] = crate::decision_tags::derive(
                        &decision.judgment,
                        cap_label,
                        decision.sticky,
                    );
                }
                if cross {
                    ex["translated"] = json!("anthropic->openai");
                }
                Some(ex)
            },
        }));

        let mut out = Response::builder().status(map_status(status));
        let h = out.headers_mut().unwrap();
        append_decision_headers(h, &decision, est);
        if !skipped.is_empty() {
            insert_header(h, "x-ev-fallback-from", &original_choice);
            insert_header(h, "x-ev-skipped", &skipped.join(","));
        }

        if !status.is_success() {
            let err_body = resp.bytes().await.unwrap_or_default();
            crate::stream::finalize_event(&st.events, &st.flywheel.clone(), &st.quota, &st.bus, &telem.lock().unwrap());
            insert_header(h, "x-ev-upstream-status", status.as_str());
            return out.body(axum::body::Body::from(err_body)).unwrap();
        }

        if is_stream {
            let ct = resp
                .headers()
                .get("content-type")
                .cloned()
                .unwrap_or_else(|| "text/event-stream".parse().unwrap());
            h.insert("content-type", ct);
            if cross {
                // telemetry sees the RAW anthropic stream (parser has an
                // anthropic mode); translation wraps telemetry output so the
                // client still receives openai chunks. Order matters: parse
                // pre-translation or quality/usage signals are lost.
                let telem_body = crate::stream::telemetry_body(
                    resp.bytes_stream(),
                    telem,
                    Some(crate::request_inspect::Tap(Some(inspect.clone()))),
                    st.events.clone(),
                    st.flywheel.clone(),
                    st.sessions.clone(),
                    st.quota.clone(),
                    st.bus.clone(),
                    parsed.clone(),
                    true,
                    Some(st.health.clone()),
                );
                let translated =
                    crate::translate::AnthropicToOpenaiStream::new(telem_body).boxed();
                return out.body(axum::body::Body::from_stream(translated)).unwrap();
            }
            let stream = crate::stream::telemetry_body(
                resp.bytes_stream(),
                telem,
                Some(crate::request_inspect::Tap(Some(inspect.clone()))),
                st.events.clone(),
                st.flywheel.clone(),
                st.sessions.clone(),
                st.quota.clone(),
                st.bus.clone(),
                parsed.clone(),
                false,
                Some(st.health.clone()),
            );
            return out.body(axum::body::Body::from_stream(stream)).unwrap();
        }

        let _resp_ct = resp
            .headers()
            .get("content-type")
            .cloned()
            .unwrap_or_else(|| "application/json".parse().unwrap());
        let bytes = resp.bytes().await.unwrap_or_default();
        let mut response_value: Option<Value> = serde_json::from_slice::<Value>(&bytes).ok();
        if cross {
            // anthropic -> openai response shape for the caller; quality
            // analysis and pending tool-call extraction run on openai form
            if let Some(v) = &response_value {
                match crate::translate::Translator::global().response_anthropic_to_openai(v) {
                    Ok(openai_v) => {
                        response_value = Some(openai_v);
                        insert_header(h, "x-ev-translated", "anthropic->openai");
                    }
                    Err(e) => {
                        tracing::warn!(error = %e, "anthropic->openai response translation failed");
                    }
                }
            }
        }
        {
            let Ok(mut t) = telem.lock() else {
                return json_error(StatusCode::INTERNAL_SERVER_ERROR, "telemetry lock poisoned");
            };
            t.bytes = bytes.len() as u64;
            if t.ttft_ms.is_none() {
                t.ttft_ms = Some(started.elapsed().as_millis());
            }
            if let Some(v) = &response_value {
                if let Some(u) = v.get("usage").filter(|u| u.is_object()) {
                    t.usage = Some(u.clone());
                    // 会话真实上下文入账：下一轮估算的精确基准（2026-10-20）
                    record_session_tokens(
                        &session_key,
                        est_raw,
                        u.get("prompt_tokens").and_then(|x| x.as_u64()).unwrap_or(0),
                    );
                }
                if let (Some(cost), Some(u)) = (record.cost, t.usage.as_ref()) {
                    let pt = u.get("prompt_tokens").and_then(|x| x.as_f64()).unwrap_or(0.0);
                    let ct = u.get("completion_tokens").and_then(|x| x.as_f64()).unwrap_or(0.0);
                    t.est_cost_usd =
                        Some((pt / 1e6) * cost.input as f64 + (ct / 1e6) * cost.output as f64);
                }
                let q = crate::quality::analyze_response(&parsed, v);
                // A3a 响应摘要落盘（opt-in）：backtest 的 LLM-as-a-Judge 需要
                // 响应正文评分。redact=true 模式正文永不落盘（隐私红线）；
                // 截取前 400 字符，足够 judge 判断"是否充分回答"
                if st.config.telemetry.response_digest
                    && !st.config.decision.redact
                    && status.is_success()
                {
                    let digest_text: String = v
                        .get("choices")
                        .and_then(|c| c.get(0))
                        .and_then(|c| c.get("message"))
                        .and_then(|m| m.get("content"))
                        .and_then(|c| c.as_str())
                        .map(|s| s.chars().take(400).collect())
                        .unwrap_or_default();
                    if !digest_text.is_empty() && let Some(ex) = t.extra.as_mut() {
                        ex["resp_digest"] = json!(digest_text);
                    }
                }
                if let Some(ex) = t.extra.as_mut() {
                    ex["quality"] = serde_json::to_value(&q).unwrap_or_default();
                } else {
                    t.extra = Some(json!({"quality": q}));
                }
                // ⑤ 质量级联（FrugalGPT/ACRouter）：响应成功但质量差——退化输出/
                // 空响应/工具调用损坏——留痕后升级到链上下一个候选。
                // A2 链位置异质严格度（FrugalGPT τ_i 思想：链首阈值 0.96 严、
                // 链中 0.37 宽）：链首宁枉勿纵，全部质量信号触发级联；链中已
                // 降级一次，只对最恶劣信号（退化/空响应）升级，工具调用损坏
                // 容忍——避免无谓耗尽链上名额。
                // 流式不级联（内容已发出无法重试）；truncated 不触发（max_tokens
                // 问题换模型无益且双倍计费）；refusal 不触发（可能正当拒绝）。
                // 链耗尽则回退返回此响应（降质好过硬报错）。
                let quality_poor = if this_idx == 0 {
                    q.degenerate
                        || q.empty_response
                        || (q.tool_calls_total > 0 && !q.syntactic_ok())
                } else {
                    q.degenerate || q.empty_response
                };
                if quality_poor && this_idx + 1 < attempts.len() {
                    crate::stream::finalize_event(&st.events, &st.flywheel.clone(), &st.quota, &st.bus, &t);
                    tracing::warn!(model = %cand, flags = ?q.flags, "quality cascade: escalating to next candidate");
                    quality_fallback = Some((map_status(status), h.clone(), bytes.clone()));
                    skipped.push(format!("{cand}(poor quality: {})", q.flags.join(",")));
                    continue 'cands;
                }
                // A3：高质量响应写入缓存（供后续相同逻辑请求命中）。
                // review#1 修复：cross（OpenAI 入口 → Anthropic 上游）时必须缓存
                // 翻译后的 openai 形状——否则命中会把 anthropic JSON 返给
                // OpenAI 客户端（跨协议投毒）。
                if !quality_poor
                    && status.is_success()
                    && let Some(key) = cache_key
                    && let Ok(mut c) = st.ccache.lock()
                {
                    let cache_body: Vec<u8> = if cross {
                        response_value
                            .as_ref()
                            .and_then(|v| serde_json::to_vec(v).ok())
                            .unwrap_or_else(|| bytes.to_vec())
                    } else {
                        bytes.to_vec()
                    };
                    c.put(key, cand.clone(), "application/json".to_string(), cache_body.into());
                }
                let ids: Vec<String> = v
                    .get("choices")
                    .and_then(|c| c.as_array())
                    .and_then(|c| c.first())
                    .and_then(|c| c.get("message"))
                    .and_then(|m| m.get("tool_calls"))
                    .and_then(|t| t.as_array())
                    .map(|tcs| {
                        tcs.iter()
                            .filter_map(|tc| {
                                tc.get("id").and_then(|i| i.as_str()).map(|s| s.to_string())
                            })
                            .collect::<Vec<String>>()
                    })
                    .unwrap_or_default();
                let chosen = t.chosen.clone();
                st.sessions.set_pending(&sticky_key, &chosen, ids);
            }
        } // guard dropped before finalize (which locks telem itself)

        if let Some(v) = &response_value {
            crate::stream::finalize_event(&st.events, &st.flywheel.clone(), &st.quota, &st.bus, &telem.lock().unwrap());
            h.insert("content-type", "application/json".parse().unwrap());
            let body_bytes = serde_json::to_vec(v).unwrap_or_default();
            return out.body(axum::body::Body::from(body_bytes)).unwrap();
        }
        crate::stream::finalize_event(&st.events, &st.flywheel.clone(), &st.quota, &st.bus, &telem.lock().unwrap());
        h.insert("content-type", "application/json".parse().unwrap());
        return out.body(axum::body::Body::from(bytes)).unwrap();
        } // key_off loop (keys exhausted for this candidate)
    } // candidate loop

    // every candidate failed: explicit selections get the raw upstream
    // error back; auto routing gets a summary of what was skipped and why
    // 链耗尽但持有质量级联兜底：返回它（带标记头），不发全灭事件
    if let Some((status, headers, body)) = quality_fallback {
        let mut b = Response::builder()
            .status(status)
            .header("x-ev-quality-cascade", "1");
        for (k, v) in headers.iter() {
            b = b.header(k, v);
        }
        return b.body(axum::body::Body::from(body)).unwrap();
    }
    // 全灭也落事件（含 x-ev-skipped 明细），否则面板回看不到这次失败
    {
        let status = last_error.as_ref().map(|(s, _)| s.as_u16()).unwrap_or(502);
        let filtered_note: Vec<String> = decision
            .filtered
            .iter()
            .take(4)
            .map(|f| format!("{} ({})", f.model, f.cause))
            .collect();
        let mut detail = if skipped.is_empty() {
            "all routed upstreams failed".to_string()
        } else {
            format!("all routed upstreams failed: {}", skipped.join("; "))
        };
        if !filtered_note.is_empty() {
            detail.push_str(&format!(" | filtered: {}", filtered_note.join("; ")));
        }
        let telem = crate::stream::Telemetry {
            decision_id: decision.id.clone(),
            session: session_key.clone(),
            chosen: original_choice.clone(),
            upstream_model: String::new(),
            est_tokens: decision.est_input_tokens,
            sticky: decision.sticky,
            stream: false,
            started,
            ttft_ms: None,
            bytes: 0,
            status,
            usage: None,
            est_cost_usd: None,
            translated: None,
            extra: Some(json!({
                "reason": detail,
                "skipped": skipped,
                "filtered": decision.filtered,
                "attempted": decision.chain,  // 全灭时降级链内容：让面板看清试了哪些
                "primary_cause": primary.as_ref().map(|(_, _, l)| l.clone()),  // 主因（非最后一个尝试）
                "scored": decision.scored,      // 各候选总分（不是无路可走，是都被熔断）
            })),
            agent: Some(crate::identity::agent_identity(&headers, &st.config.telemetry.agent_header)),
            plan_key: None,
            preprocess_ms: None,
            decision_ms: None,
            judge_ms: None,
            est_raw: None,
        };
        crate::stream::finalize_event(&st.events, &st.flywheel.clone(), &st.quota, &st.bus, &telem);
    }
    let direct_raw = matches!(target, Target::Direct(_)) && last_error.is_some();
    let (status, err_body) = last_error.unwrap_or((StatusCode::BAD_GATEWAY, Bytes::new()));
    // 上下文真超限（2026-10-20）：目录里没有任何窗口装得下 est+output 预留时，
    // 返回业界标准的 context_length_exceeded（客户端 opencode/qwen-code/pi 解析
    // 后**自动压缩**），取代语焉不详的"全灭"——盲目重试永远不可能成功
    let ctx_overflow = {
        let out_reserve = parsed.get("max_tokens").and_then(|v| v.as_u64());
        let max_catalog_window = st
            .engine
            .catalog_snapshot()
            .iter()
            .filter_map(|m| m.context_window)
            .max()
            .unwrap_or(0);
        context_overflow(est, out_reserve, &decision.filtered, max_catalog_window)
    };
    // 全灭时：状态码取**主因**（配额/上下文 > 兜底段个别 400）；原因写进 message，
    // 让调用方一眼看到"真正为什么不通"，而不是最后一个被试模型的状态码
    let (status, err_body) = if direct_raw {
        (status, err_body)
    } else if let Some((budget, ctx_est)) = ctx_overflow {
        // OpenAI 形态：opencode / qwen-code / 通用 SDK 靠 code/message 触发压缩
        let body = json!({
            "error": {
                "message": format!(
                    "This model's maximum context length is {budget} tokens. However, your messages resulted in {ctx_est} tokens. Please reduce the length of the messages."
                ),
                "type": "invalid_request_error",
                "param": "messages",
                "code": "context_length_exceeded",
                "evolve_max_input_tokens": budget,
                "evolve_est_tokens": ctx_est,
            }
        })
        .to_string()
        .into_bytes();
        (StatusCode::BAD_REQUEST, Bytes::from(body))
    } else if decision.judgment.needs_vision > evolve_core::thresholds::VISION
        && !st.engine.catalog_snapshot().iter().any(|m| {
            m.has_credential() && m.tiers.vision >= evolve_core::thresholds::VISION
        }) {
        // 请求带图但全池没有任何可用多模态模型 → 显式可行动报错
        // （用户裁决 2026-10-23："返回报错说候选模型不支持多模态识别"），
        // 不发语焉不详的全灭——客户端该做的是去图重试或配置多模态模型
        let body = json!({
            "error": {
                "message": "request contains image input, but no vision-capable model is currently available in the pool — 候选模型不支持多模态识别（remove images and retry with text only, or restore/configure a multimodal model）",
                "type": "invalid_request_error",
                "code": "no_vision_model_available",
                "evolve_est_tokens": est,
            }
        })
        .to_string()
        .into_bytes();
        (StatusCode::BAD_REQUEST, Bytes::from(body))
    } else if let Some((_, pstatus, plabel)) = primary.clone() {
        // 配额/上下文类主因的语义状态码优先于兜底段的 4xx
        // 容量类主因（欠费 no credit / 配额枯竭 quota exhausted）→ 统一用 **429**
        // 表达"此刻全池无容量，请退避后重试"：上游的 402 是**不可重试**的硬错误，
        // Agent 框架会直接判死整轮（2026-10-20 实证：OCR 评审因 402 no credit
        // 四连败 → 整轮失败）；429 是各家都会指数退避重试的信号，额度恢复后自愈。
        // 显式点名路径不走这里（direct_raw 分支保留上游原状态码，语义才诚实）。
        let capacity_exhausted = plabel == "no credit" || plabel == "quota exhausted";
        let status = if capacity_exhausted {
            StatusCode::TOO_MANY_REQUESTS
        } else if pstatus.as_u16() == 502 {
            status
        } else {
            pstatus
        };
        let body = json!({
            "error": {
                "message": format!(
                    "all {} candidate route(s) failed — primary cause: {} (no upstream capacity right now; retry with backoff)",
                    decision.chain.len().max(1), plabel
                ),
                "type": if capacity_exhausted { "rate_limit_error" } else { "evolve_all_failed" },
                "primary_cause": plabel,
                "tried": skipped,
            }
        })
        .to_string()
        .into_bytes();
        (status, Bytes::from(body))
    } else {
        (status, err_body)
    };
    let mut resp = if direct_raw {
        let mut r = Response::builder().status(status)
            .header("content-type", "application/json")
            .body(axum::body::Body::from(err_body))
            .unwrap();
        insert_header(r.headers_mut(), "x-ev-model", &original_choice);
        insert_header(r.headers_mut(), "x-evolve-decision-id", &decision.id);
        r
    } else {
        {
            // 主因优先：有主因就用主因生成的 body（含 primary_cause），
            // 否则回落到通用文案（老行为）
            let mut resp = if !err_body.is_empty() {
                Response::builder()
                    .status(map_status(status))
                    .header("content-type", "application/json")
                    .body(axum::body::Body::from(err_body))
                    .unwrap()
            } else {
                json_error(
                    map_status(status),
                    "all routed upstreams failed; see x-ev-skipped for per-model reasons",
                )
            };
            if let Some(sec) = skipped.iter()
                .filter_map(|s| s.split('(').last().and_then(|t| t.split('s').next()).and_then(|n| n.parse::<u64>().ok()))
                .max()
                .map(|s| s.min(300)) // 封顶 5 分钟：退避超过单轮预算只会让整轮被掐
            {
                if let Ok(hv) = axum::http::HeaderValue::from_str(&sec.to_string()) {
                    resp.headers_mut().insert("retry-after", hv);
                }
            }
            resp
        }
    };
    // 引导调用方退避：按链上模型已知的最短冷却给 Retry-After，
    // 让 opencode 等客户端按此退避而不是立即重撞
    {
        let snap = st.health.snapshot();
        let now_ms = evolve_memory::health::now();
        let mut ra: Option<u64> = None;
    for cand in &attempts {
            for (k, h) in &snap {
                if k.split(KEY_SEP).next() == Some(cand.as_str())
                    && let Some(ms) = h.cooldown_remaining_ms(now_ms)
                {
                    ra = Some(ra.map_or(ms, |x: u64| x.min(ms)));
                }
            }
        }
        if let Some(secs) = ra {
            insert_header(resp.headers_mut(), "retry-after", &(secs.div_ceil(1000)).to_string());
        }
    }
    if !skipped.is_empty() {
        insert_header(resp.headers_mut(), "x-ev-skipped", &skipped.join(","));
    }
    resp
}

fn fallback_eligible(status: u16) -> bool {
    // review#6：补 413（上下文超限→ContextOverflow 换模型）与 451（上游 WAF
    // 地域拦截——此前走"成功"路径被 mark_ok，实际上游根本不可用）
    // 2026-10-23 实证：opencode zen 免费 MiniMax 对超大请求回 410（0 字节，
    // ttft 7.3s）——此前不在白单里走"成功"路径透传给客户端，会话直接死
    // （客户端收 410 空体即放弃，无重试）；实际该模型常态 200，410 属
    // 上游瞬时拒绝 → 换模型失败转移（Transient 短冷却）
    matches!(status, 400 | 401 | 402 | 403 | 404 | 408 | 410 | 413 | 429 | 451) || status >= 500
}

/// 端点级最近成功记录（baseURL → 最后一次 200 的时刻）：同 baseURL 多账号
/// （同 plan 双订阅，如 opencode-go / opencode-go-github）共享端点命运——
/// 兄弟账号刚 200 过 = 端点明确可达，另一账号的传输失败属于瞬时/连接级抖动，
/// 不应升级成 provider 降级（否则一次网络抖动会把双订阅之一流放 30-60 分钟，
/// 而兄弟账号全程 200 的实证 2026-10-20）。
static ENDPOINT_LAST_OK: std::sync::LazyLock<
    std::sync::Mutex<std::collections::HashMap<String, u64>>,
> = std::sync::LazyLock::new(|| std::sync::Mutex::new(std::collections::HashMap::new()));

const ENDPOINT_OK_FRESH_MS: u64 = 120 * 1000;

fn record_endpoint_ok(base_url: &str) {
    let now = evolve_memory::health::now();
    if let Ok(mut m) = ENDPOINT_LAST_OK.lock() {
        m.insert(base_url.to_string(), now);
    }
}

/// 同 baseURL 的兄弟账号近期有成功响应 → 端点明确可达
fn endpoint_recently_ok(base_url: &str) -> bool {
    let now = evolve_memory::health::now();
    let Ok(m) = ENDPOINT_LAST_OK.lock() else { return false };
    m.get(base_url)
        .map(|ts| now.saturating_sub(*ts) < ENDPOINT_OK_FRESH_MS)
        .unwrap_or(false)
}

/// provider 级故障记账（网络/鉴权）：达阈值才降级并发出事件（面板据此显示
/// `degraded · next probe`）；降级只影响"是否进主池"，兜底段与探活都会再给它机会。
/// Network 类失败有**兄弟豁免**：同 baseURL 的另一账号 2 分钟内有 200 → 端点
/// 明确可达 → 传输失败不计入降级（2026-10-20 实证：睡眠唤醒抖动把双订阅之一
/// 流放 30+ 分钟，兄弟账号全程 200）。
fn note_provider_issue(
    st: &AppState,
    provider: &str,
    base_url: &str,
    issue: evolve_memory::quota::ProviderIssue,
) {
    // 无凭据（还没填 key）不是"provider 故障"——不降级、不探活，交给 needs_key 提示
    let has_credential = st
        .engine
        .catalog_snapshot()
        .iter()
        .any(|m| m.provider == provider && m.has_credential());
    if !has_credential {
        return;
    }
    if issue == evolve_memory::quota::ProviderIssue::Network && endpoint_recently_ok(base_url) {
        tracing::info!(
            provider = %provider,
            "transport failure but sibling account on the same endpoint is serving — skipping network degradation"
        );
        return;
    }
    let now = evolve_memory::health::now();
    if st.quota.provider_failure(provider, issue, now) {
        let (_, probe_at, fails) = st
            .quota
            .provider_issue_of(provider)
            .unwrap_or((issue, 0, 0));
        st.events.record(json!({
            "kind": "provider_state_changed",
            "provider": provider, "state": "degraded",
            "issue": issue.label(), "consecutive_fails": fails,
            "next_probe_ms": probe_at.saturating_sub(now),
            "source": "upstream_failure",
        }));
    }
}

/// 兜底 sweep 候选（2026-10 修复，实测事故驱动）。
///
/// 旧实现盲取"目录里前 6 个模型"——不看请求硬约束、不看健康、不看供应商，
/// 于是：
/// - 27 万 token 的请求被拖进窗口只有 200k 的模型，白试一堆 429/404/400；
/// - 6 个名额被目录顺序靠前的供应商吃满，`opencode-go-github`（独立额度）
///   与 MiniMax（1M 窗口）**全程没轮到**，最终回一个误导性的 400。
///
/// 现规则（按序）：
/// 1. **硬约束先过**：窗口放不下（`scoring::context_fits`）或有视觉需求但无
///    视觉能力 → 不进兜底（不再拿注定失败的模型去撞）
/// 2. **按 provider 轮转**：每家先取 1 个，再取第 2 个……直到取满 `SWEEP_MAX`
///    ——保证跨供应商都有机会（单家网络/额度问题不再吃掉全部名额）
/// 3. **无冷却的 provider 优先**：provider 内有模型在冷却 → 排后面（仍保留
///    "全灭也要试"的兜底语义，只是次序靠后）
pub(crate) fn sweep_candidates(
    cat: &[evolve_core::types::ModelRecord],
    seen: &std::collections::HashSet<String>,
    est: u64,
    max_output: u64,
    j: &evolve_core::types::JudgmentSet,
    self_addr: &str,
    st: &AppState,
) -> Vec<String> {
    const SWEEP_MAX: usize = 6;
    let health = st.health.snapshot();
    let now = evolve_memory::health::now();
    let needs_vision = j.needs_vision > evolve_core::thresholds::VISION;
    // provider → 通过硬约束的候选（BTreeMap 保证遍历顺序稳定、可测）
    let mut per: std::collections::BTreeMap<&str, Vec<&evolve_core::types::ModelRecord>> =
        std::collections::BTreeMap::new();
    for m in cat {
        if seen.contains(&m.id) || m.base_url.contains(self_addr) {
            continue;
        }
        // 无凭据模型不进兜底：默认接入的 provider 未填 key 时去撞 401 只会把它
        // 误标成 provider 级降级（真实原因是"还没填 key"，由 needs_key 提示）
        if !m.has_credential() {
            continue;
        }
        if needs_vision && m.tiers.vision < evolve_core::thresholds::VISION {
            continue;
        }
        if evolve_core::scoring::context_fits(m, est, max_output).is_err() {
            continue;
        }
        per.entry(m.provider.as_str()).or_default().push(m);
    }
    let mut providers: Vec<(&str, &Vec<&evolve_core::types::ModelRecord>)> =
        per.iter().map(|(k, v)| (*k, v)).collect();
    providers.sort_by_key(|(k, models)| {
        let cooling = models
            .iter()
            .filter(|m| {
                health
                    .get(&m.id)
                    .map(|h| !h.available(now))
                    .unwrap_or(false)
            })
            .count();
        (cooling, *k)
    });
    let mut out: Vec<String> = Vec::new();
    for round in 0.. {
        let mut took = false;
        for (_, models) in &providers {
            if let Some(m) = models.get(round) {
                took = true;
                if out.len() >= SWEEP_MAX {
                    return out;
                }
                out.push(m.id.clone());
            }
        }
        if !took {
            break;
        }
    }
    out
}

/// 胜出队列（用户裁决 2026-10）：最终决策的 chain 去重后为主序列，不足
/// `queue_max` 时按评分序（降序）补位；总量以 `queue_max` 封顶。队列整体
/// 失败或为空才走队列外的 sweep 段（见调用点）。
pub(crate) fn winner_queue(
    chain: &[String],
    scores: &std::collections::BTreeMap<String, f32>,
    queue_max: usize,
) -> Vec<String> {
    let mut seen = std::collections::HashSet::new();
    let mut list: Vec<String> = chain
        .iter()
        .filter(|c| seen.insert((*c).clone()))
        .take(queue_max)
        .cloned()
        .collect();
    if list.len() < queue_max && !scores.is_empty() {
        let mut ranked: Vec<(String, f32)> = scores.iter().map(|(k, v)| (k.clone(), *v)).collect();
        ranked.sort_by(|a, b| b.1.partial_cmp(&a.1).unwrap_or(std::cmp::Ordering::Equal));
        for (m, _) in ranked {
            if list.len() >= queue_max {
                break;
            }
            if seen.insert(m.clone()) {
                list.push(m);
            }
        }
    }
    list
}

/// 价位闸临近模型（用户裁决 2026-10-20）：从决策 `filtered` 里挑被**价格层闸**
/// 挡掉的模型，**每 provider 至多 1 个**（该家离允许档最近的代表，按评分降序），
/// 排除自指别名（evo-router/auto 绕回本网关必 400）与已在 attempts 里的——
/// 直接补到链尾，让"没命中格子的 provider 也以临近模型进候选集"，精确档
/// 全灭时不必先空转一轮再等救援。价位闸语义从"掐死"降级为"压排序"。
pub(crate) fn price_gated_retries(
    filtered: &[evolve_core::types::FilteredOut],
    scores: &std::collections::BTreeMap<String, f32>,
    attempts: &[String],
    max: usize,
) -> Vec<String> {
    let mut gated: Vec<&str> = filtered
        .iter()
        .filter(|f| f.cause.starts_with("price tier "))
        .map(|f| f.model.as_str())
        .collect();
    gated.sort_by(|a, b| {
        let sa = scores.get(*a).copied().unwrap_or(0.0);
        let sb = scores.get(*b).copied().unwrap_or(0.0);
        sb.partial_cmp(&sa).unwrap_or(std::cmp::Ordering::Equal)
    });
    let mut out: Vec<String> = Vec::new();
    let mut per_provider: std::collections::HashSet<&str> = std::collections::HashSet::new();
    for m in gated {
        if out.len() >= max {
            break;
        }
        if m == "evo-router/auto" {
            continue; // 自指别名：路由回本网关必 400，占位无意义
        }
        if attempts.iter().any(|a| a == m) || out.iter().any(|a| a == m) {
            continue;
        }
        // 每 provider 至多 1 个临近代表（候选集要的是 provider 覆盖面）
        let prov = m.split('/').next().unwrap_or(m);
        if !per_provider.insert(prov) {
            continue;
        }
        out.push(m.to_string());
    }
    out
}

/// 上下文真超限判定（2026-10-20）：目录里没有任何窗口能容纳 `est + output 预留`
/// 时返回 `(预算, est)`，调用方构造**业界标准形态**的上下文超限错误——
/// opencode / qwen-code / pi 等客户端解析后触发自动压缩，而不是对着
/// "全灭"盲目重试。
/// 预算 = 可用最大窗口 − output 预留。预留取 **20K**（claude-code 实测值
/// SUMMARY_RESERVE = COMPACT_MAX_OUTPUT_TOKENS，qwen-code 设计文档全文沿用：
/// "模型受 maxOutputTokens 硬限制约束，输出不可能超出 20K，reserve 不需要
/// 额外 safety margin"）；请求显式要了更大 max_tokens 时预留随之放大。
pub(crate) fn context_overflow(
    est: u64,
    max_output_req: Option<u64>,
    filtered: &[evolve_core::types::FilteredOut],
    max_catalog_window: u64,
) -> Option<(u64, u64)> {
    let out_reserve = max_output_req.unwrap_or(20_000).clamp(20_000, 65_536);
    let mut max_window = max_catalog_window;
    for f in filtered {
        if let Some((_, bound)) = context_gate_numbers(&f.cause) {
            max_window = max_window.max(bound);
        }
    }
    if max_window == 0 {
        return None;
    }
    let budget = max_window.saturating_sub(out_reserve).max(1024);
    (est > budget).then_some((budget, est))
}

/// 上下文闸 cause → (need_strict, 窗口上界)。两种形态：
///   "context window too small (need 1137090 tok, window 1000000)"
///   "unknown context window (proven max accepted 1012384 tok, need 1007973 tok)"
fn context_gate_numbers(cause: &str) -> Option<(u64, u64)> {
    let need: u64 = cause.split("need ").nth(1)?.split(" tok").next()?.trim().parse().ok()?;
    // ⚠ 锚点必须带前缀：cause 里 "context window"/"unknown context window" 自身
    // 都含 "window "，裸锚会劈中标题
    let bound: u64 = if cause.starts_with("unknown context window") {
        cause.split("proven max accepted ").nth(1)?.split(" tok").next()?.trim().parse().ok()?
    } else {
        cause.split(", window ").nth(1)?.split(" tok").next()?.trim_end_matches(')').trim().parse().ok()?
    };
    Some((need, bound))
}

/// 上下文闸放宽的救命轮（2026-10-19）：`bound × 1.1 ≥ need_strict` 的被滤模型
/// 按窗口上界降序补队尾——need 的余量 stacking（校准×比值×1.1+output）偶尔把
/// 窗口真实够用的模型挤出局，全军覆没边缘给 10% 宽限再试，错过成本只是一次
/// 上游 400，收益是把"明明装得下"的会话救活。
/// 上下文闸救命轮（2026-10-20 重构）：被上下文闸挡掉的模型里，**窗口 ≥ 裸 est**
/// 的按窗口降序补队尾。
/// 为什么不用 per-model need（= est×校准×比值×1.1+output）：冷启动会话没有
/// 真值锚点时，比值堆叠会把 need 顶到 1.2-1.5M（实证：裸 est 552K 被算成
/// 1.4M），连同窗口真实够用的模型一起滤光 → 唯一候选是欠费户 → 失败 → 依旧
/// 无锚点 → 死循环。裸 est 是不被校准污染的诚实下界；窗口 ≥ 它的模型值得一
/// 次尝试（最坏一次上游 400，最好直接救活会话并锚定账本）。窗口降序 = 最大的
/// 先上，成功概率最高。真正超限的会话（裸 est 都超预算）由 context_overflow
/// 返回压缩信号——两条路径互补，没有中间死区。
fn context_gated_retries(
    filtered: &[evolve_core::types::FilteredOut],
    attempts: &[String],
    raw_est: u64,
    max: usize,
) -> Vec<String> {
    let mut gated: Vec<(&str, u64)> = filtered
        .iter()
        .filter(|f| f.cause.starts_with("context window too small")
            || f.cause.starts_with("unknown context window"))
        .filter_map(|f| {
            context_gate_numbers(&f.cause)
                .map(|(_, bound)| (f.model.as_str(), bound))
                .filter(|(_, bound)| *bound >= raw_est)
        })
        .collect();
    gated.sort_by(|a, b| b.1.cmp(&a.1).then(a.0.cmp(b.0)));
    let mut out: Vec<String> = Vec::new();
    for (m, _) in gated {
        if out.len() >= max {
            break;
        }
        if attempts.iter().any(|a| a == m) || out.iter().any(|a| a == m) {
            continue;
        }
        out.push(m.to_string());
    }
    out
}

/// 把历史消息里的图片 part 替换成占位文本（**最后一条 user 消息保留**）：
/// text-only 上游收到图片会按非法 content.type 拒收；本轮不需要识图时，历史
/// 图对模型只是"看不见的上下文"，占位文本即可（2026-10-18 用户裁决）。
fn strip_image_parts(v: &mut Value) {
    let Some(msgs) = v.get_mut("messages").and_then(|m| m.as_array_mut()) else { return };
    let last_user = msgs
        .iter()
        .rposition(|m| m.get("role").and_then(|r| r.as_str()) == Some("user"));
    for (i, m) in msgs.iter_mut().enumerate() {
        if Some(i) == last_user {
            continue;
        }
        let Some(parts) = m.get_mut("content").and_then(|c| c.as_array_mut()) else { continue };
        for p in parts.iter_mut() {
            let t = p.get("type").and_then(|x| x.as_str());
            if matches!(t, Some("image_url") | Some("image") | Some("input_image")) {
                *p = json!({
                    "type": "text",
                    "text": "[image omitted: routed model does not accept image input]"
                });
            }
        }
    }
}

/// model 重写 + 历史图片剥离（text-only 上游的转发整形；放弃逐字节 passthrough）
fn rewrite_model_field_images_stripped(body: &[u8], new_model: &str) -> Vec<u8> {
    let Ok(mut v) = serde_json::from_slice::<Value>(body) else {
        return body.to_vec();
    };
    if let Some(obj) = v.as_object_mut() {
        obj.insert("model".into(), json!(new_model));
    }
    strip_image_parts(&mut v);
    v.to_string().into_bytes()
}

fn insert_header(h: &mut axum::http::HeaderMap, k: &'static str, v: &str) {    if let Ok(val) = axum::http::HeaderValue::from_str(&v.chars().map(|c| if c.is_ascii() { c } else { ' ' }).collect::<String>()) {
        h.insert(k, val);
    }
}

#[allow(clippy::large_enum_variant)]
enum Target {
    Auto(Option<PolicyProfile>),
    Direct(Decision),
}

/// `model` 字段归一（容错）：面板「复制网关配置」输出的人读写法
/// `Auto (smart routing)` 与 `auto` 等价。忽略大小写、首尾及**内部多余空白**。
pub(crate) fn auto_request(model_field: &str) -> bool {
    let norm = model_field
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
        .to_ascii_lowercase();
    matches!(norm.as_str(), "auto" | "auto (smart routing)" | "auto (smart)" | "smart routing")
}

#[allow(clippy::result_large_err)]
fn resolve_target(st: &AppState, model_field: &str) -> Result<Target, Response> {
    if let Some(alias) = model_field.strip_prefix("auto:") {
        let policy = PolicyProfile::parse(alias).ok_or_else(|| {
            json_error(StatusCode::NOT_FOUND, &format!("unknown policy alias '{alias}'"))
        })?;
        return Ok(Target::Auto(Some(policy)));
    }
    if auto_request(model_field) {
        return Ok(Target::Auto(None));
    }
    if let Some(m) = st.catalog_get(model_field) {
        let dec = Decision {
            id: format!("direct-{}", evolve_memory::now_millis().unwrap_or(0)),
            chosen: m.id.clone(),
            upstream_model: m.upstream_model.clone(),
            chain: vec![m.id.clone()],
            reason: format!("explicit model selection: {}", m.id),
            scores: Default::default(),
            judgment: JudgmentSet {
                domain: Domain::Other,
                domain_confidence: 1.0,
                difficulty: 0.0,
                difficulty_confidence: 1.0,
                needs_vision: 0.0,
                is_trivial: 0.0,
                tool_heavy: 0.0,
                high_stakes: 0.0,
                session_relevance: 0.0,
                session_depth: 0.0,
                judge_source: JudgeSource::Explicit,
            route_recommendation: None,
            route_recommendation_confidence: 0.0,
            },
            filtered: vec![],
            funnel: [0, 0, 0, 0],
            scored: vec![],
            weights: [0.35, 0.15, 0.25, 0.15, 0.10],
            judge_ms: 0,
            explore_gate: None,
            sticky: false,
            est_input_tokens: 0,
            difficulty_eff: 0.0,
        };
        return Ok(Target::Direct(dec));
    }
    Err(json_error(
        StatusCode::NOT_FOUND,
        &format!("model '{model_field}' is neither 'auto' nor a configured catalog id"),
    ))
}

fn build_digest(extracted: &feat::Extracted, _turns: usize) -> DigestSignals {
    let overlap = overlap_ratio(&extracted.last_user_text, &extracted.first_user_text);
    let has_deixis = ["这个", "那个", "它", "刚才", "继续", "上面", "接着", "再改"]
        .iter()
        .any(|w| extracted.last_user_text.contains(w));
    let topic_shift_marker = ["另外", "顺便", "换个话题", "新任务"]
        .iter()
        .any(|w| extracted.last_user_text.contains(w));
    DigestSignals {
        first_user_text: extracted.first_user_text.clone(),
        last_user_text: extracted.last_user_text.clone(),
        overlap_ratio: overlap,
        has_deixis,
        topic_shift_marker,
        session_tools_seen: extracted.tool_count,
    }
}

fn overlap_ratio(a: &str, b: &str) -> f32 {
    if a.is_empty() || b.is_empty() {
        return 0.0;
    }
    let norm = |s: &str| -> std::collections::HashSet<String> {
        s.to_lowercase()
            .split(|c: char| c.is_whitespace() || ",。.;;!?、".contains(c))
            .filter(|w| w.chars().count() > 1)
            .map(|w| w.to_string())
            .collect()
    };
    let (sa, sb) = (norm(a), norm(b));
    if sa.is_empty() || sb.is_empty() {
        return 0.0;
    }
    let inter = sa.intersection(&sb).count() as f32;
    let union = sa.union(&sb).count() as f32;
    inter / union
}

fn tools_signature(parsed: &Value) -> u64 {
    let tools = parsed.get("tools").map(|t| t.to_string()).unwrap_or_default();
    let hash = Sha256::digest(tools.as_bytes());
    u64::from_be_bytes(hash[0..8].try_into().unwrap())
}

fn session_key_of(st: &AppState, agent: &str, headers: &HeaderMap, parsed: &Value) -> String {
    if let Some(v) = crate::identity::session_identity(headers, parsed, &st.config.telemetry.session_header) {
        return v;
    }
    let fallback = {
        let mut hasher = Sha256::new();
        if let Some(msgs) = parsed.get("messages").and_then(|v| v.as_array()) {
            if let Some(first) = msgs.first() {
                hasher.update(first.to_string().as_bytes());
            }
        }
        if let Some(sys) = parsed.get("system").or_else(|| {
            parsed.get("messages").and_then(|v| v.as_array())
                .and_then(|msgs| msgs.iter().find(|m| m.get("role").and_then(|r| r.as_str()) == Some("system")))
        }) {
            hasher.update(sys.to_string().as_bytes());
        }
        format!("h{}", hex::encode(&hasher.finalize()[..8]))
    };
    // 前缀链重建（零插件）：同一会话每轮携带全量历史，上一轮 messages 是
    // 本轮前缀——命中即跨轮稳定同一会话键（compaction 断链视为新会话）
    match parsed.get("messages").and_then(|v| v.as_array()) {
        Some(msgs) if !msgs.is_empty() => crate::identity::chain_session(agent, msgs, &fallback),
        _ => fallback,
    }
}

fn append_decision_headers(h: &mut axum::http::HeaderMap, decision: &Decision, est: u64) {
    insert_header(h, "x-ev-model", &decision.chosen);
    insert_header(h, "x-evolve-decision-id", &decision.id);
    insert_header(h, "x-ev-sticky", &decision.sticky.to_string());
    insert_header(h, "x-ev-est-tokens", &est.to_string());
    insert_header(h, "x-ev-reason", &decision.header_reason());
}

fn map_status(s: reqwest::StatusCode) -> StatusCode {
    StatusCode::from_u16(s.as_u16()).unwrap_or(StatusCode::BAD_GATEWAY)
}

fn json_error(status: StatusCode, msg: &str) -> Response {
    (
        status,
        axum::Json(json!({
            "error": { "message": msg, "type": "evolve_error" }
        })),
    )
        .into_response()
}

/// A3 completion cache（FrugalGPT Strategy 2 / completion cache）：
/// 逻辑请求指纹 → 响应的本地内存缓存。只缓存非流式成功响应；
/// TTL/容量受限（env 可调：EVO_CACHE_TTL_SECS / EVO_CACHE_MAX，EVO_CACHE=0 停用）。
pub struct CompletionCache {
    map: std::collections::HashMap<[u8; 32], (std::time::Instant, String, String, Bytes)>,
    order: std::collections::VecDeque<[u8; 32]>,
    max: usize,
    ttl: std::time::Duration,
}

impl CompletionCache {
    pub fn new() -> Self {
        let ttl = std::time::Duration::from_secs(
            std::env::var("EVO_CACHE_TTL_SECS").ok().and_then(|v| v.parse().ok()).unwrap_or(3600),
        );
        let max = std::env::var("EVO_CACHE_MAX").ok().and_then(|v| v.parse().ok()).unwrap_or(256);
        Self { map: std::collections::HashMap::new(), order: std::collections::VecDeque::new(), max, ttl }
    }

    pub fn get(&mut self, key: [u8; 32]) -> Option<(String, String, Bytes)> {
        let (at, model, ct, body) = self.map.get(&key)?;
        if at.elapsed() > self.ttl {
            self.map.remove(&key);
            return None;
        }
        Some((model.clone(), ct.clone(), body.clone()))
    }

    pub fn put(&mut self, key: [u8; 32], model: String, content_type: String, body: Bytes) {
        if body.len() > 512 * 1024 {
            return; // 超大响应不缓存（内存保护）
        }
        if !self.order.contains(&key) {
            self.order.push_back(key);
        }
        self.map.insert(key, (std::time::Instant::now(), model, content_type, body));
        while self.order.len() > self.max {
            if let Some(old) = self.order.pop_front() {
                self.map.remove(&old);
            }
        }
    }
}

/// 会话级真实上下文账本：value = (本轮 body_est, 上游实测 prompt_tokens, 更新时刻)。
/// 上一轮 usage.prompt_tokens 是历史上下文的**精确值**（客户端每轮传全量历史）；
/// 下一轮 est = 实测 × 本轮估算增幅——估算偏差在同一估算器下完全抵消，无需
/// 校准系数（用户裁决 2026-10-20）。容量 1024 / TTL 6h，超限淘汰最旧。
static SESSION_TOKENS: std::sync::LazyLock<
    std::sync::Mutex<std::collections::HashMap<String, (u64, u64, u64)>>,
> = std::sync::LazyLock::new(|| std::sync::Mutex::new(std::collections::HashMap::new()));

/// 命中账本 → (实测基准 × 增幅, true)；未命中 → (裸估算, false)。
/// 下限 = 裸估算（压缩/清史后真实值骤降交给裸估算托底），上限 ×3 防异常。
fn session_calibrated_est(session: &str, body_est: u64) -> (u64, bool) {
    const TTL_MS: u64 = 6 * 3600 * 1000;
    let now = evolve_memory::health::now();
    let Ok(mut map) = SESSION_TOKENS.lock() else { return (body_est, false) };
    map.retain(|_, (_, _, ts)| now.saturating_sub(*ts) < TTL_MS);
    let Some((est_last, real_last, _)) = map.get(session).copied() else {
        return (body_est, false);
    };
    if est_last == 0 || real_last == 0 {
        return (body_est, false);
    }
    let grown = (real_last as f64 * (body_est as f64 / est_last as f64)) as u64;
    (grown.clamp(body_est, body_est.saturating_mul(3)), true)
}

pub(crate) fn record_session_tokens(session: &str, body_est: u64, real_prompt: u64) {
    const CAP: usize = 1024;
    if body_est == 0 || real_prompt == 0 {
        return;
    }
    // 锚点可信度护栏（2026-10-20 实证）：实测与估算差出 20 倍 = 假数据
    // （mock 固定 usage=123 vs est≈4，30 倍）→ 不入账——一条毒锚点会把
    // 后续轮次的 est 顶出粘性护栏（est ≤ 首轮×8），无端断掉工具续跑
    if (real_prompt as f64) > (body_est as f64) * 20.0
        || (real_prompt as f64) * 8.0 < (body_est as f64)
    {
        return;
    }
    let now = evolve_memory::health::now();
    let Ok(mut map) = SESSION_TOKENS.lock() else { return };
    if map.len() >= CAP && !map.contains_key(session) {
        if let Some(oldest) =
            map.iter().min_by_key(|(_, (_, _, ts))| *ts).map(|(k, _)| k.clone())
        {
            map.remove(&oldest);
        }
    }
    map.insert(session.to_string(), (body_est, real_prompt, now));
}

/// 逻辑请求指纹：去掉 model（路由可换）与 stream 后的整体 JSON 哈希。
/// 精确匹配（不引入 embedding 依赖）；messages/tools/参数完全一致才命中。
fn cache_fingerprint(parsed: &Value) -> Option<[u8; 32]> {
    use sha2::{Digest, Sha256};
    let mut logic = parsed.clone();
    if let Some(obj) = logic.as_object_mut() {
        obj.remove("model");
        obj.remove("stream");
    }
    let canonical = serde_json::to_vec(&logic).ok()?;
    Some(Sha256::digest(&canonical).into())
}

#[cfg(test)]
mod winner_queue_tests {
    use super::{
        context_gate_numbers, context_gated_retries, context_overflow, price_gated_retries,
        strip_image_parts, winner_queue,
    };
    use evolve_core::types::FilteredOut;
    use std::collections::BTreeMap;

    fn scores(pairs: &[(&str, f32)]) -> BTreeMap<String, f32> {
        pairs.iter().map(|(k, v)| (k.to_string(), *v)).collect()
    }

    #[test]
    fn chain_first_then_score_fill() {
        let chain = vec!["a".to_string(), "b".to_string()];
        let s = scores(&[("c", 0.9), ("d", 0.8), ("b", 0.7)]);
        let q = winner_queue(&chain, &s, 5);
        // chain 先行（去重）；再用评分序补位，上限 5
        assert_eq!(q, vec!["a", "b", "c", "d"]);
    }

    #[test]
    fn queue_is_capped() {
        let chain: Vec<String> = (0..8).map(|i| format!("m{i}")).collect();
        let q = winner_queue(&chain, &scores(&[]), 5);
        assert_eq!(q.len(), 5);
        assert_eq!(q[0], "m0");
        assert_eq!(q[4], "m4");
    }

    #[test]
    fn dedupes_chain_and_scores() {
        let chain = vec!["a".to_string(), "a".to_string(), "b".to_string()];
        let s = scores(&[("a", 0.5), ("b", 0.9), ("c", 0.1)]);
        let q = winner_queue(&chain, &s, 5);
        assert_eq!(q, vec!["a", "b", "c"]);
    }

    #[test]
    fn empty_chain_falls_back_to_scores() {
        let q = winner_queue(&[], &scores(&[("x", 0.2), ("y", 0.8)]), 5);
        assert_eq!(q, vec!["y", "x"]);
    }

    fn filtered(pairs: &[(&str, &str)]) -> Vec<FilteredOut> {
        pairs
            .iter()
            .map(|(m, c)| FilteredOut { model: m.to_string(), cause: c.to_string() })
            .collect()
    }

    #[test]
    fn price_gated_models_become_last_resort_by_score() {
        // 价位闸只压排序：被闸模型按评分降序补到链尾
        let f = filtered(&[
            ("cheap-tier/ok", "quota remaining low"),
            ("premA/a", "price tier 2 above allowed 1 (difficulty 0.6)"),
            ("premB/b", "price tier 2 above allowed 1 (difficulty 0.6)"),
        ]);
        let s = scores(&[("premA/a", 0.5), ("premB/b", 0.9)]);
        let retry = price_gated_retries(&f, &s, &["seed/model".to_string()], 6);
        assert_eq!(retry, vec!["premB/b", "premA/a"]);
    }

    #[test]
    fn price_gated_near_models_take_one_per_provider_and_skip_self() {
        // 每 provider 至多 1 个临近代表；自指别名（evo-router/auto）剔除
        let f = filtered(&[
            ("premA/a", "price tier 2 above allowed 1 (difficulty 0.6)"),
            ("premA/b", "price tier 2 above allowed 1 (difficulty 0.6)"),
            ("evo-router/auto", "price tier 2 above allowed 1 (difficulty 0.6)"),
            ("premB/c", "price tier 2 above allowed 1 (difficulty 0.6)"),
        ]);
        let s = scores(&[("premA/a", 0.9), ("premA/b", 0.8), ("evo-router/auto", 0.7), ("premB/c", 0.6)]);
        let retry = price_gated_retries(&f, &s, &[], 6);
        assert_eq!(retry, vec!["premA/a", "premB/c"], "premA 只留最高分的 a；auto 剔除");
    }

    #[test]
    fn context_overflow_reports_when_no_window_fits() {
        let f = filtered(&[
            ("big/flash", "context window too small (need 1007973 tok, window 1000000)"),
        ]);
        // 预算 = 最大窗口 − output 预留；est 超预算 → Some((预算, est))
        assert_eq!(
            context_overflow(1_007_973, Some(65_536), &f, 1_000_000),
            Some((934_464, 1_007_973))
        );
        // est 在预算内 → None（正常路由，不报上下文超限）
        assert_eq!(context_overflow(500_000, Some(65_536), &f, 1_000_000), None);
        // 目录与实证都没有任何窗口 → None（无从构造有意义的预算）
        let f0 = filtered(&[("x/y", "unknown context window (proven max accepted 0 tok, need 557827 tok)")]);
        assert_eq!(context_overflow(557_827, None, &f0, 0), None);
    }

    #[test]
    fn context_gated_models_become_last_resort_by_window() {
        // 上下文闸救命轮：窗口 ≥ 裸 est 即放行（不被校准污染的诚实下界），窗口降序
        let f = filtered(&[
            // 窗口 1M，膨胀 need 1.007M——照样放行（冷启动会话的真值可能只有 810K）
            ("big/flash", "context window too small (need 1007973 tok, window 1000000)"),
            // unknown-window 形态：proven 1012384 ≥ est → 放行
            ("big/proven", "unknown context window (proven max accepted 1012384 tok, need 1007973 tok)"),
            // 窗口 1.048M ≥ est → 放行，且窗口最大排最前
            ("mid/k3", "context window too small (need 1195593 tok, window 1048576)"),
            // 窗口 < 裸 est → 不放行（真装不下）
            ("small/na", "context window too small (need 527287 tok, window 200000)"),
            // 非上下文闸原因不入选
            ("price/x", "price tier 2 above allowed 0 (difficulty 0.6)"),
        ]);
        let retry = context_gated_retries(&f, &["seed/model".to_string()], 552_746, 4);
        assert_eq!(retry, vec!["mid/k3", "big/proven", "big/flash"]);
        // 已试过的不重复入队
        let retry2 = context_gated_retries(
            &f,
            &["mid/k3".to_string(), "big/proven".to_string()],
            552_746,
            4,
        );
        assert_eq!(retry2, vec!["big/flash"]);
    }

    #[test]
    fn strips_images_from_history_but_keeps_the_last_user_turn() {
        let mut v = serde_json::json!({"model":"m","messages":[
            {"role":"user","content":[
                {"type":"text","text":"看图"},
                {"type":"image_url","image_url":{"url":"data:1"}}
            ]},
            {"role":"assistant","content":"ok"},
            {"role":"user","content":[
                {"type":"image_url","image_url":{"url":"data:2"}}
            ]}
        ]});
        strip_image_parts(&mut v);
        let msgs = v["messages"].as_array().unwrap();
        let old = msgs[0]["content"].as_array().unwrap();
        assert_eq!(old[0]["type"], "text");
        assert_eq!(old[1]["type"], "text", "histor. image must become text");
        assert!(old[1]["text"].as_str().unwrap().contains("image omitted"));
        // 最后一条 user 的图保留（本轮意图不被静默丢弃）
        let last = msgs[2]["content"].as_array().unwrap();
        assert_eq!(last[0]["type"], "image_url");
    }

    #[test]
    fn price_gated_retries_skip_already_tried_and_cap() {
        let mut pairs: Vec<(String, String)> = (0..8)
            .map(|i| (format!("p{i}"), "price tier 3 above allowed 0 (difficulty 0.1)".into()))
            .collect();
        pairs[0].1 = "context window too small".into(); // 非价格闸原因不入选
        let owned: Vec<(&str, &str)> = pairs.iter().map(|(a, b)| (a.as_str(), b.as_str())).collect();
        let f = filtered(&owned);
        let tried = vec!["p1".to_string()];
        let retry = price_gated_retries(&f, &scores(&[]), &tried, 3);
        assert_eq!(retry.len(), 3);
        assert!(!retry.contains(&"p1".to_string()));
        assert!(!retry.contains(&"p0".to_string()));
    }
}

#[cfg(test)]
mod auto_alias_tests {
    use super::{auto_request, fallback_eligible};

    #[test]
    fn accepts_human_readable_auto_wiring() {
        // 面板「复制网关配置」输出 + 大小写/空白变体
        assert!(auto_request("auto"));
        assert!(auto_request("Auto"));
        assert!(auto_request("Auto (smart routing)"));
        assert!(auto_request("  auto (smart routing) "));
        assert!(auto_request("AUTO (SMART ROUTING)"));
        assert!(auto_request("auto  (smart   routing)"));
        assert!(auto_request("auto (smart)"));
    }

    #[test]
    fn rejects_real_model_ids() {
        assert!(!auto_request("deepseek/deepseek-v4-pro"));
        assert!(!auto_request("opencode-go/glm-5.3"));
        assert!(!auto_request(""));
        assert!(!auto_request("autopilot"));
    }

    /// 410 必须走失败转移（2026-10-23 实证：opencode zen 免费 MiniMax 对超大
    /// 请求回 410 空体——透传会让客户端会话直接死，且模型被误标 OK）
    #[test]
    fn upstream_410_is_fallback_eligible() {
        assert!(fallback_eligible(410));
        assert!(fallback_eligible(413));
        assert!(fallback_eligible(502));
        assert!(!fallback_eligible(200));
        assert!(!fallback_eligible(204));
    }
}

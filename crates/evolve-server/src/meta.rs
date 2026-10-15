use crate::state::AppState;
use axum::extract::State;
use axum::http::StatusCode;
use axum::response::sse::{Event, KeepAlive, Sse};
use axum::response::{IntoResponse, Response};
use futures::stream::Stream;
use serde_json::{json, Value};
use std::convert::Infallible;
use std::time::Duration;
use tokio_stream::wrappers::BroadcastStream;
use futures::StreamExt;

pub const DASHBOARD_HTML: &str = include_str!("dashboard.html");

pub async fn dashboard() -> impl IntoResponse {
    (
        [(axum::http::header::CONTENT_TYPE, "text/html; charset=utf-8")],
        DASHBOARD_HTML,
    )
}

pub async fn list_models(State(st): State<AppState>) -> Response {
    // advertised window for auto = largest context in the eligible pool;
    // dynamic-catalog agents (hermes/openrouter-style) read this directly
    let max_window = st
        .engine
        .catalog_snapshot()
        .iter()
        .filter_map(|m| m.context_window)
        .max()
        .unwrap_or(0);
    let mut data = vec![
        json!({"id": "auto", "object": "model", "owned_by": "evolve", "source": "router",
               "description": "intelligent routing (default policy)",
               "context_length": max_window}),
        json!({"id": "auto:cost", "object": "model", "owned_by": "evolve", "source": "router",
               "description": "intelligent routing, cost-optimized policy",
               "context_length": max_window}),
        json!({"id": "auto:quality", "object": "model", "owned_by": "evolve", "source": "router",
               "description": "intelligent routing, quality-optimized policy",
               "context_length": max_window}),
    ];
    for m in &st.engine.catalog_snapshot() {
        data.push(json!({
            "id": m.id,
            "object": "model",
            "owned_by": m.provider,
            "source": m.source.label(),
            "context_window": m.context_window,
            "cost_per_mtok": m.cost.as_ref().map(|c| json!({"input": c.input, "output": c.output})),
            "tiers": m.tiers,
            "weight": m.weight,
            "note": m.source_note,
        }));
    }
    (axum::Json(json!({"object": "list", "data": data}))).into_response()
}

pub async fn healthz() -> &'static str {
    "ok"
}

/// Live health status of every model seen by the router.
pub async fn api_health(State(st): State<AppState>) -> Response {
    let snap = st.health.snapshot();
    let mut models = serde_json::Map::new();
    for (id, e) in snap {
        models.insert(
            id.clone(),
            json!({
                "kind": e.kind,
                "available": e.available(evolve_memory::health::now()),
                "cooldown_remaining_ms": e.cooldown_remaining_ms(evolve_memory::health::now()),
                "hits": e.hits,
                "message": e.message,
            }),
        );
    }
    (axum::Json(json!({ "models": models }))).into_response()
}

/// Flywheel aggregates + learned telemetry + link status + dynamic score.
/// 窗口聚合：把模型近期样本环按窗口过滤后折算成与累计口径同形的聚合值
/// 解析绝对窗口（依赖本地时区的日历口径——由客户端算好 epoch 传入）：
/// `since:<epoch_ms>` → (start, +∞)；`range:<start>-<end>` → (start, end)。
fn parse_absolute_window(window: &str) -> Option<(u64, u64)> {
    if let Some(rest) = window.strip_prefix("since:") {
        let start = rest.parse::<u64>().ok()?;
        return Some((start, u64::MAX));
    }
    if let Some(rest) = window.strip_prefix("range:") {
        let mut it = rest.split('-');
        let s = it.next().and_then(|v| v.parse::<u64>().ok())?;
        let e = it.next().and_then(|v| v.parse::<u64>().ok())?;
        return Some((s, e));
    }
    None
}

fn windowed_view(
    s: &evolve_core::types::ModelTelemetry,
    window: &str,
) -> Option<serde_json::Value> {
    use evolve_core::types::ReqSample;
    let samples: Vec<ReqSample> = s.recent.clone().unwrap_or_default();
    if samples.is_empty() {
        return None;
    }
    let now = evolve_memory::health::now();
    // (cutoff, before)：cutoff = 起始回溯量（含），before = 结束回溯量（**不含**，
    // 用于"昨天"这类有上下界的日历窗口——只给起点会把今天的流量也算进昨天）
    let (cutoff_ms, before_ms): (Option<u64>, Option<u64>) = match parse_absolute_window(window) {
        Some((start, end)) => (
            Some(now.saturating_sub(start)),
            (end != u64::MAX).then(|| now.saturating_sub(end)),
        ),
        None => match window {
            "7d" => (Some(7 * 24 * 3600 * 1000), None),
            "24h" => (Some(24 * 3600 * 1000), None),
            // 5h = 订阅配额窗口（Coding/Go/Agent plan）
            "5h" => (Some(5 * 3600 * 1000), None),
            "1h" => (Some(3600 * 1000), None),
            "10m" => (Some(10 * 60 * 1000), None),
            _ => (None, None),
        },
    };
    let take_n: Option<usize> = match window {
        "last30" => Some(30),
        "last10" => Some(10),
        _ => None,
    };
    let mut sel: Vec<ReqSample> = samples
        .iter()
        .copied()
        .filter(|r| {
            let age = now.saturating_sub(r.ts);
            match cutoff_ms {
                Some(ms) if age > ms => return false,
                _ => {}
            }
            match before_ms {
                Some(ms) if age <= ms => return false,
                _ => {}
            }
            true
        })
        .collect();
    if let Some(n) = take_n {
        sel = sel.split_off(sel.len().saturating_sub(n));
    }

    if sel.is_empty() {
        return None;
    }
    fn avg_of(sel: &[ReqSample], f: impl Fn(&ReqSample) -> u64) -> u64 {
        (sel.iter().map(|r| f(r) as f64).sum::<f64>() / sel.len() as f64).round() as u64
    }
    let avg = |f: fn(&ReqSample) -> u64| avg_of(&sel, f);
    let ttft = avg(|r| r.ttft_ms);
    let total = avg(|r| r.total_ms);
    let in_tok: u64 = sel.iter().map(|r| r.in_tok).sum();
    let cached: u64 = sel.iter().map(|r| r.cached_tok).sum();
    let out_tok: u64 = sel.iter().map(|r| r.out_tok).sum();
    let ok = sel.iter().filter(|r| r.ok).count();
    let gen_ms: u64 = sel.iter().map(|r| r.total_ms.saturating_sub(r.ttft_ms)).sum();
    let rate = if gen_ms > 0 { (out_tok as f64 * 1000.0 / gen_ms as f64).round() as u64 } else { 0 };
    let tools_total: u32 = sel.iter().map(|r| r.tools_total).sum();
    let tools_ok: u32 = sel.iter().map(|r| r.tools_ok).sum();
    Some(json!({
        "requests": sel.len(),
        "success": ok,
        "avg_ttft_ms": ttft,
        "avg_total_ms": total,
        "avg_rate_tok_s": rate,
        "in_tok": in_tok,
        "cached_tok": cached,
        "cache_hit_rate": if in_tok > 0 { Some(cached as f32 / in_tok as f32) } else { None },
        "tool_calls": {"total": tools_total, "valid_json": tools_ok},
        "last_total_ms": sel.last().map(|r| r.total_ms),
        "last_ttft_ms": sel.last().map(|r| r.ttft_ms),
        "last_rate_tok_s": rate,
        "window": window,
    }))
}

/// 窗口聚合覆盖：把窗口视图写回 ModelStats 聚合字段（仪表盘渲染口径不变）
fn apply_windowed(s: &mut evolve_memory::flywheel::ModelStats, view: &serde_json::Value) {
    let g = |k: &str| view.get(k).and_then(|v| v.as_u64()).unwrap_or(0);
    s.requests = g("requests");
    s.success = g("success");
    // 窗口内无样本 → view 为空对象 → 全部归零。**不得保留累计值充数**：否则
    // "今日/5h 内 0 次"的模型会显示成历史累计（此前 5h=1229 vs 24h=107 的自相
    // 矛盾即源于此：5h 窗口空 → 保留累计，24h 有样本 → 覆盖成窗口值）
    match view.get("avg_ttft_ms").and_then(|v| v.as_u64()) {
        Some(v) => {
            s.ttft_ms_sum = v;
            s.ttft_n = 1;
        }
        None => {
            s.ttft_ms_sum = 0;
            s.ttft_n = 0;
        }
    }
    match view.get("avg_total_ms").and_then(|v| v.as_u64()) {
        Some(v) => {
            s.total_ms_sum = v;
            s.total_ms_n = 1;
        }
        None => {
            s.total_ms_sum = 0;
            s.total_ms_n = 0;
        }
    }
    s.last_rate_tok_s = g("avg_rate_tok_s");
    s.cached_tokens = g("cached_tok");
    s.prompt_tokens = g("in_tok");
    // review 修复：此处原错映射为 avg_rate_tok_s（tok/s 速率）——24h 视图
    // tokens 显示成个位数；窗口视图已输出 out_tok 合计，直接对齐
    s.completion_tokens = g("out_tok");
    s.tc_total = view
        .get("tool_calls")
        .and_then(|t| t.get("total"))
        .and_then(|v| v.as_u64())
        .unwrap_or(0);
    s.tc_valid_json = view
        .get("tool_calls")
        .and_then(|t| t.get("valid_json"))
        .and_then(|v| v.as_u64())
        .unwrap_or(0);
}

/// 请求处理耗时分段（流水线瀑布）：预处理/决策/上游首字/流式传输
pub async fn api_stats_query(
    State(st): State<AppState>,
    axum::extract::Query(params): axum::extract::Query<std::collections::HashMap<String, String>>,
) -> Response {
    let window = params.get("window").cloned().unwrap_or_else(|| "overview".into());
    api_stats_inner(&st, &window).await
}

pub async fn api_stats(State(st): State<AppState>) -> Response {
    api_stats_inner(&st, "overview").await
}

/// /api/live：菜单栏伴侣的轻量实时状态（1s 轮询）——in-flight 请求计数。
/// 计数由 relay/anthropic 入口的 RAII LiveGuard 维护，覆盖全部 return 路径。
/// `decision.status`：决策模型可用性（面板/网页横幅定时检测依据，每次调用现读 env）。
/// - ok: 蒸馏器在 = 启动时已按 decision_model/hybrid 建 backend；或显式 heuristic； laya 有 env
/// - stale: 网络起后 env 才补上——重启网关生效
/// - missing: 未配置
pub async fn api_live(State(st): State<AppState>) -> Response {
    let cfg = st.config.decision.backend.clone();
    // w2 槽位链 + 评审 #5：distiller 是**启动时**快照——env 现在有但启动时没建成
    // 后端时报 "stale"（面板/网页提示"已配置，重启生效"），避免误报 ok
    let started_active = st.distiller.is_some();
    let env_now = evolve_decision::slots::any_slot_configured();
    let status = if cfg == "heuristic" || started_active {
        "ok"
    } else if env_now {
        "stale"
    } else {
        "missing"
    };
    axum::Json(json!({
        "inflight": crate::relay::LIVE_INFLIGHT.load(std::sync::atomic::Ordering::Relaxed),
        "ts": evolve_memory::health::now(),
        // 版本号（编译期常量）：面板与 web 面板已在轮询本端点 → 顺带下发，前端展示
        "version": env!("CARGO_PKG_VERSION"),
        "decision": { "backend": cfg, "status": status },
    }))
    .into_response()
}

async fn api_stats_inner(st: &AppState, window: &str) -> Response {
    let mut stats = st.flywheel.stats();
    // 难度分布观测（2026-10）：raw vs 校准后三档占比 + 目标——验证 7:2:1 收敛
    let (dsnap, dtarget, dcal) = st.engine.difficulty_snapshot();
    let telemetry = st.flywheel.telemetry_snapshot();
    // 窗口模式：用近期样本环折算聚合，覆盖累计口径（总览保持原样）。
    // 窗口内**无样本的模型一律归零**（否则会保留累计值充数——曾出现同一模型
    // 5h=1229 而 24h=107 的自相矛盾）。
    // 绝对窗口（今天/昨天等日历口径）走**持久化小时桶**：不受样本环 cap 300
    // 限制、重启不丢——面板的日历数据必须准确。
    if window != "overview" {
        let absolute = parse_absolute_window(window);
        if let Some((start_ms, end_ms)) = absolute {
            let hourly = st.flywheel.hourly_window(start_ms, end_ms);
            for (id, s) in stats.iter_mut() {
                let view = hourly.get(id).cloned().unwrap_or_else(|| json!({}));
                apply_windowed(s, &view);
            }
        } else {
            for (id, s) in stats.iter_mut() {
                let view = telemetry.get(id).and_then(|tm| windowed_view(tm, window));
                apply_windowed(s, &view.unwrap_or_else(|| json!({})));
            }
        }
    }
    let overlays = st.engine.tier_overrides();
    let health_snap = st.health.snapshot();
    let now = evolve_memory::health::now();
    let mut models = serde_json::Map::new();
    // iterate the CATALOG (not just traffic stats) so every known model
    // shows up, ranked by dynamic priority; top 50 returned
    let mut ranked: Vec<(String, serde_json::Value)> = Vec::new();
    for m in &st.engine.catalog_snapshot() {
        let id = &m.id;
        let empty = Default::default();
        let s = stats.get(id).unwrap_or(&empty);
        let t = telemetry.get(id).cloned().unwrap_or_default();
        // link status from health ledger (any key ok = up)
        let key_ids: Vec<String> = {
            let keys = m.key_values();
            if keys.is_empty() {
                vec![id.clone()]
            } else {
                (0..keys.len()).map(|i| format!("{id}\u{1f}{i}")).collect()
            }
        };
        let link = {
            let any_ok = key_ids.iter().any(|k| {
                health_snap.get(k).map(|h| h.available(now)).unwrap_or(true)
            });
            let any_entry = key_ids.iter().any(|k| health_snap.contains_key(k));
            if !any_entry { "unknown" } else if any_ok { "up" } else { "down" }
        };
        // dynamic priority: benchmark tiers avg x reliability/speed x weight
        let (tiers, conf) = overlays.get(id).cloned().unwrap_or({
            (m.tiers, 0.0)
        });
        let bench_avg = (tiers.coding + tiers.reasoning + tiers.agentic) / 3.0;
        // 性价比优先度（用户修正）：能力不是全部——按量计费模型看每次请求
        // 的真实花费；coding plan 订阅模型看 5h/7d 窗口余量（好钢用在刀刃上）
        let dyn_score = {
            let rel = t.reliability.unwrap_or(0.7);
            let spd = t.speed_obs.unwrap_or(0.5);
            let cost_eff = if m.plan {
                // 订阅套餐：配额内边际成本≈0，用掉才值
                1.0_f32
            } else {
            match m.cost {
                // 按量：以一次典型 4k-in/0.5k-out 请求为基准，与目录最便宜
                // 模型比价；无价模型（订阅摊薄）按 0.8 中性偏优
                Some(c) => {
                    let typical = (4_000.0 / 1e6) * c.input as f64 + (500.0 / 1e6) * c.output as f64;
                    let cheapest = st
                        .engine
                        .catalog_snapshot()
                        .iter()
                        .filter_map(|x| {
                            x.cost.map(|cc| {
                                (4_000.0 / 1e6) * cc.input as f64
                                    + (500.0 / 1e6) * cc.output as f64
                            })
                        })
                        .fold(f64::MAX, f64::min);
                    ((cheapest / typical) as f32).clamp(0.05, 1.0)
                }
                None => 0.8,
            }
            };
            let quota_factor = st
                .quota
                .model_remaining_tokens(id)
                .map(|rem| ((rem as f32) / 4_000.0).min(1.0))
                .unwrap_or(1.0);
            let raw = 0.40 * bench_avg + 0.25 * rel + 0.15 * spd + 0.20 * cost_eff;
            let w = m.weight.unwrap_or(1.0).clamp(0.2, 3.0);
            let bias = t.learned_bias.unwrap_or(1.0).clamp(0.7, 1.3);
            (raw * (w * bias).sqrt().clamp(0.4, 1.8) * quota_factor).clamp(0.0, 1.0) * 100.0
        };
        let success_rate = (s.requests > 0).then(|| s.success as f32 / s.requests as f32);
        let cache_hit_rate = (s.prompt_tokens > 0)
            .then(|| s.cached_tokens as f32 / s.prompt_tokens as f32);
        let cat_model = st.catalog_get(id);
        let plan_kind = cat_model
            .as_ref()
            .and_then(|m| evolve_core::plans::plan_for(&m.base_url))
            .map(|p| p.plan_kind.to_string());
        let currency = cat_model.as_ref().map(|m| m.currency.clone()).unwrap_or_default();
        let tiers = cat_model.as_ref().map(|m| {
            json!({
                "coding": ((m.tiers.coding as f64) * 100.0).round() / 100.0,
                "reasoning": ((m.tiers.reasoning as f64) * 100.0).round() / 100.0,
                "agentic": ((m.tiers.agentic as f64) * 100.0).round() / 100.0,
            })
        });
        let source = cat_model.as_ref().map(|m| m.source.label());
        let est_cost: Option<f32> = match cat_model.as_ref().map(|m| (m.plan, m.cost)) {
            Some((true, _)) | Some((false, None)) => Some(0.0),
            Some((false, Some(c))) => Some(
                (s.prompt_tokens as f32 / 1e6) * c.input
                    + (s.completion_tokens as f32 / 1e6) * c.output,
            ),
            _ => None,
        };
        ranked.push((
            id.clone(),
            json!({
                "link": link,
                "dynamic_score": (dyn_score * 10.0).round() / 10.0,
                "bench_confidence": conf,
                "requests": s.requests,
                "success": s.success,
                "failures": s.failures,
                "avg_ttft_ms": (s.ttft_n > 0).then(|| s.ttft_ms_sum / s.ttft_n),
                "avg_total_ms": (s.total_ms_n > 0).then(|| s.total_ms_sum / s.total_ms_n),
                "prompt_tokens": s.prompt_tokens,
                "completion_tokens": s.completion_tokens,
                "cached_tokens": s.cached_tokens,
                "cache_write_tokens": s.cache_write_tokens,
                "tool_calls": {"total": s.tc_total, "valid_json": s.tc_valid_json,
                               "known_name": s.tc_known_name, "schema_ok": s.tc_schema_ok},
                "truncations": s.truncations,
                "degenerate": s.degenerate,
                "empty_responses": s.empty_responses,
                "semantic": {"matched": s.sem_matched, "total": s.sem_total},
                "feedback": {"ok": s.fb_ok, "total": s.fb_total},
                "success_rate": success_rate,
                "cache_hit_rate": cache_hit_rate,
                "est_cost": est_cost,
                "currency": currency,
                "plan_kind": plan_kind,
                "tiers": tiers,
                "source": source,
                "samples": s.requests,
                "last_seen_ms": (s.last_seen_ms > 0).then_some(s.last_seen_ms),
                "last_total_ms": (s.last_total_ms > 0).then_some(s.last_total_ms),
                "last_ttft_ms": (s.last_ttft_ms > 0).then_some(s.last_ttft_ms),
                // 平均吐字速率：完成 tokens ÷ 生成窗口（总耗时-TTFT 累计）；
                // f64 域取整避免 f32 序列化毛刺
                "avg_rate_tok_s": (s.total_ms_sum > s.ttft_ms_sum).then(|| {
                    let gen_s = (s.total_ms_sum - s.ttft_ms_sum) as f32 / 1000.0;
                    ((s.completion_tokens as f64 / gen_s as f64) * 10.0).round() / 10.0
                }),
                "last_rate_tok_s": (s.last_rate_tok_s > 0).then_some(s.last_rate_tok_s),
                "telemetry": t,
                "user_weight": st
                    .engine
                    .weight_override(id)
                    .or(m.weight)
                    .map(|w| ((w as f64) * 100.0).round() / 100.0),
            }),
        ));
    }
    // 模型动态排序（用户规则）：有流量的固定前排，内部按 流量(请求数) 降序、
    // 再按路由优先度降序；无流量的殿后，纯按路由优先度降序
    ranked.sort_by(|a, b| {
        let get = |v: &serde_json::Value, k: &str| {
            v.get(k).and_then(|x| x.as_f64()).unwrap_or(0.0)
        };
        let (ra, rb) = (get(&a.1, "requests"), get(&b.1, "requests"));
        let (da, db) = (get(&a.1, "dynamic_score"), get(&b.1, "dynamic_score"));
        let ta = if ra > 0.0 { 0 } else { 1 };
        let tb = if rb > 0.0 { 0 } else { 1 };
        ta.cmp(&tb)
            .then(rb.partial_cmp(&ra).unwrap_or(std::cmp::Ordering::Equal))
            .then(db.partial_cmp(&da).unwrap_or(std::cmp::Ordering::Equal))
    });
    ranked.truncate(15);
    for (id, entry) in ranked {
        models.insert(id, entry);
    }
    // 难度分布观测（raw vs 校准 + 目标）——前端/巡检据此验证 7:2:1 收敛
    let difficulty = json!({
        "samples": dsnap.samples,
        "enabled": dcal,
        "target": { "l1": dtarget[0], "l2": dtarget[1], "l3": dtarget[2] },
        "raw":  { "l1": dsnap.raw_l1, "l2": dsnap.raw_l2, "l3": dsnap.raw_l3 },
        "calibrated": { "l1": dsnap.cal_l1, "l2": dsnap.cal_l2, "l3": dsnap.cal_l3 },
    });
    let (rsnap, rtarget, rband, renabled) = st.engine.ratio_snapshot();
    let ratio = json!({
        "samples": rsnap.samples, "enabled": renabled,
        "target": {"cheap": rtarget[0], "medium": rtarget[1], "premium": rtarget[2]},
        "band": rband,
        "observed": {"cheap": rsnap.cheap, "medium": rsnap.medium, "premium": rsnap.premium},
        "penalties": {"cheap": rsnap.penalties[0], "medium": rsnap.penalties[1], "premium": rsnap.penalties[2]},
    });
    let recent100 = recent_distribution(st, 100);
    // 统计起点（flywheel 首次记录时刻）：面板顶部"总览"卡标注"自 …起统计"
    (axum::Json(json!({
        "models": models,
        "since_ms": st.flywheel.since_ms(),
        "difficulty": difficulty,
        "last100": recent100,
        "ratio": ratio
    })))
    .into_response()
}

/// Benchmark feed status + currently applied tier overlay.
pub async fn api_benchmarks(State(st): State<AppState>) -> Response {
    let overlay = st.engine.tier_overrides();
    let dir = if st.config.data.dir.starts_with("~/") {
        std::env::var("HOME")
            .map(|h| std::path::PathBuf::from(h).join(&st.config.data.dir[2..]))
            .unwrap_or_else(|_| std::path::PathBuf::from(&st.config.data.dir))
    } else {
        std::path::PathBuf::from(&st.config.data.dir)
    };
    let snapshot = std::fs::read_to_string(dir.join("benchmarks.json"))
    .ok()
    .and_then(|t| serde_json::from_str::<serde_json::Value>(&t).ok())
    .unwrap_or(serde_json::json!({}));
    let mut models = serde_json::Map::new();
    for (id, (tiers, conf)) in overlay {
        models.insert(id, json!({ "tiers": tiers, "confidence": conf }));
    }
    (axum::Json(json!({ "sources": snapshot, "applied": models }))).into_response()
}

/// Recent routing events for the dashboard decision feed.
pub async fn api_events(
    State(st): State<AppState>,
    axum::extract::Query(params): axum::extract::Query<std::collections::HashMap<String, String>>,
) -> Response {
    let limit: usize = params
        .get("limit")
        .and_then(|v| v.parse().ok())
        .unwrap_or(30);
    let dir = shellexpand_dir(&st.config.data.dir);
    let path = dir.join("events.jsonl");
    let mut events = Vec::new();
    if let Ok(content) = tokio::fs::read_to_string(&path).await {
        for line in content.lines().rev() {
            if events.len() >= limit { break; }
            if let Ok(v) = serde_json::from_str::<serde_json::Value>(line) {
                events.push(v);
            }
        }
        events.reverse(); // oldest first, so prepend works for feed
    }
    (axum::Json(serde_json::Value::Array(events))).into_response()
}

pub(crate) fn shellexpand_dir(dir: &str) -> std::path::PathBuf {
    if dir.starts_with("~/") {
        std::env::var("HOME")
            .map(|h| std::path::PathBuf::from(h).join(&dir[2..]))
            .unwrap_or_else(|_| std::path::PathBuf::from(dir))
    } else {
        std::path::PathBuf::from(dir)
    }
}

/// Quota windows learned from upstream rate-limit headers.
pub async fn api_quota(State(st): State<AppState>) -> Response {
    (axum::Json(json!({ "windows": st.quota.snapshot() }))).into_response()
}

/// Plugin-reported explicit feedback (L4 ground truth from agents that
/// run the evolve adapter).
pub async fn api_feedback(
    State(st): State<AppState>,
    axum::Json(body): axum::Json<serde_json::Value>,
) -> Response {
    let ok = body.get("ok").and_then(|o| o.as_bool());
    let session = body.get("session").and_then(|s| s.as_str()).unwrap_or("");
    let agent = body
        .get("agent")
        .and_then(|a| a.as_str())
        .map(|s| s.to_string())
        .unwrap_or_else(|| "-".into());
    // plugins don't know which model served the session; the gateway does
    let model = body
        .get("model")
        .and_then(|m| m.as_str())
        .map(|s| s.to_string())
        .or_else(|| st.sessions.get_by_session(&[agent], session).map(|s| s.chosen))
        .unwrap_or_default();
    if model.is_empty() || ok.is_none() {
        return (
            StatusCode::BAD_REQUEST,
            axum::Json(json!({"error": {"message": "model (or a known session) and ok (bool) are required"}})),
        )
            .into_response();
    }
    st.flywheel.observe_feedback(&model, ok.unwrap());
    st.events.record(json!({
        "kind": "feedback",
        "model": model,
        "ok": ok.unwrap(),
        "tool": body.get("tool").cloned().unwrap_or(Value::Null),
        "call_id": body.get("call_id").cloned().unwrap_or(Value::Null),
        "detail": body.get("detail").cloned().unwrap_or(Value::Null),
        "session": session,
    }));
    (axum::Json(json!({"status": "ok"}))).into_response()
}

/// 面板权重调控：热生效（engine overlay）+ overrides.json 持久化
pub async fn api_weight(
    State(st): State<AppState>,
    axum::Json(body): axum::Json<serde_json::Value>,
) -> Response {
    let Some(model) = body.get("model").and_then(|m| m.as_str()).map(|s| s.to_string()) else {
        return (StatusCode::BAD_REQUEST, axum::Json(json!({"error": {"message": "model is required"}}))).into_response();
    };
    let Some(w) = body.get("weight").and_then(|w| w.as_f64()) else {
        return (StatusCode::BAD_REQUEST, axum::Json(json!({"error": {"message": "weight (number 0.2-3.0) is required"}}))).into_response();
    };
    let w = (w as f32).clamp(0.2, 3.0);
    st.engine.set_weight_override(&model, Some(w));
    let dir = &st.config.data.dir;
    let mut ov = crate::state::load_overrides(dir);
    ov.models.insert(model.clone(), w);
    crate::state::save_overrides(dir, &ov);
    (axum::Json(json!({"status": "ok", "model": model, "weight": w}))).into_response()
}

/// 当前生效的公式权重（override > 默认 profile）
fn effective_weights(st: &AppState) -> evolve_core::types::PolicyWeights {
    st.engine.weights_override().unwrap_or_else(|| {
        evolve_core::types::PolicyProfile::parse(&st.config.policy.default)
            .unwrap_or(evolve_core::types::PolicyProfile::Balanced)
            .weights()
    })
}

pub async fn api_policy_weights_get(State(st): State<AppState>) -> Response {
    let w = effective_weights(&st);
    let over = st.engine.weights_override().is_some();
    (
        axum::Json(json!({
            "weights": {"quality": w.quality, "speed": w.speed, "cost": w.cost,
                        "stability": w.stability, "headroom": w.headroom},
            "overridden": over,
            "profile": st.config.policy.default,
        })),
    )
        .into_response()
}

/// 公式权重调控：任意子集，缺省沿用当前值；归一化后热生效并持久化
pub async fn api_policy_weights_set(
    State(st): State<AppState>,
    axum::Json(body): axum::Json<serde_json::Value>,
) -> Response {
    let cur = effective_weights(&st);
    let pick = |k: &str, fallback: f32| -> Result<f32, (StatusCode, serde_json::Value)> {
        match body.get(k) {
            Some(v) => {
                let f = v.as_f64().unwrap_or(-1.0) as f32;
                if !(0.0..=2.0).contains(&f) || !f.is_finite() {
                    Err((StatusCode::BAD_REQUEST, json!({"error": {"message": format!("{k} must be within 0.0-2.0")}})))
                } else {
                    Ok(f)
                }
            }
            None => Ok(fallback),
        }
    };
    let quality = match pick("quality", cur.quality) { Ok(v) => v, Err((code, msg)) => return (code, axum::Json(msg)).into_response() };
    let speed = match pick("speed", cur.speed) { Ok(v) => v, Err((code, msg)) => return (code, axum::Json(msg)).into_response() };
    let cost = match pick("cost", cur.cost) { Ok(v) => v, Err((code, msg)) => return (code, axum::Json(msg)).into_response() };
    let stability = match pick("stability", cur.stability) { Ok(v) => v, Err((code, msg)) => return (code, axum::Json(msg)).into_response() };
    let headroom = match pick("headroom", cur.headroom) { Ok(v) => v, Err((code, msg)) => return (code, axum::Json(msg)).into_response() };
    let w = evolve_core::types::PolicyWeights { quality, speed, cost, stability, headroom };
    let sum = w.quality + w.speed + w.cost + w.stability + w.headroom;
    if sum <= 0.01 {
        return (StatusCode::BAD_REQUEST, axum::Json(json!({"error": {"message": "weights sum must be > 0"}}))).into_response();
    }
    st.engine.set_weights_override(Some(w.clone()));
    let dir = &st.config.data.dir;
    let mut ov = crate::state::load_overrides(dir);
    ov.weights = Some([
        w.quality, w.speed, w.cost, w.stability, w.headroom,
    ]);
    let normalized = w.normalized();
    crate::state::save_overrides(dir, &ov);
    (
        axum::Json(json!({
            "status": "ok",
            "normalized": {"quality": normalized[0], "speed": normalized[1], "cost": normalized[2], "stability": normalized[3], "headroom": normalized[4]},
        })),
    )
        .into_response()
}

/// 趋势图数据源：每模型近期请求样本序列（ts/ttft/total/速率/缓存/工具）
pub async fn api_trends(
    State(st): State<AppState>,
    axum::extract::Query(params): axum::extract::Query<std::collections::HashMap<String, String>>,
) -> Response {
    let window_ms: u64 = match params.get("window").map(|s| s.as_str()) {
        Some("7d") => 7 * 24 * 3600 * 1000,
        Some("1h") => 3600 * 1000,
        Some("10m") => 10 * 60 * 1000,
        _ => 24 * 3600 * 1000, // 默认 24h
    };
    let limit: usize = params
        .get("models")
        .and_then(|v| v.parse().ok())
        .unwrap_or(6);
    let now = evolve_memory::health::now();
    let telemetry = st.flywheel.telemetry_snapshot();
    // 只取有流量的模型，按样本数排序
    let mut entries: Vec<(String, Vec<evolve_core::types::ReqSample>)> = telemetry
        .iter()
        .filter_map(|(id, tm)| {
            let rec: Vec<evolve_core::types::ReqSample> = tm
                .recent
                .clone()
                .unwrap_or_default()
                .into_iter()
                .filter(|r| now.saturating_sub(r.ts) <= window_ms)
                .collect();
            if rec.is_empty() {
                None
            } else {
                Some((id.clone(), rec))
            }
        })
        .collect();
    entries.sort_by_key(|(_, rec)| std::cmp::Reverse(rec.len()));
    entries.truncate(limit);
    let series: Vec<serde_json::Value> = entries
        .iter()
        .map(|(id, rec)| {
            json!({
                "model": id,
                "points": rec.iter().map(|r| json!({
                    "ts": r.ts,
                    "ttft": r.ttft_ms,
                    "total": r.total_ms,
                    "tok_s": if r.total_ms > r.ttft_ms {
                        (r.out_tok as f64 * 1000.0 / (r.total_ms - r.ttft_ms) as f64).round() as u64
                    } else { 0 },
                    "cache_pct": (r.cached_tok * 100).checked_div(r.in_tok).unwrap_or(0) as u8,
                    "tools_pct": (r.tools_ok * 100).checked_div(r.tools_total).unwrap_or(0) as u8,
                })).collect::<Vec<_>>(),
            })
        })
        .collect();
    (axum::Json(json!({"window_ms": window_ms, "series": series}))).into_response()
}

/// 全局扫描：后台任务执行（agent 配置发现 + 逐 provider 远程 /models 重拉），
/// 进度写 st.scan_progress，面板轮询 /api/providers/scan-progress 展示。
pub async fn api_providers_scan(State(st): State<AppState>) -> Response {
    {
        let Ok(mut p) = st.scan_progress.lock() else {
            return (StatusCode::INTERNAL_SERVER_ERROR, axum::Json(json!({"error": {"message": "lock poisoned"}}))).into_response();
        };
        if p.running {
            return (axum::Json(json!({"status": "already_running"}))).into_response();
        }
        *p = crate::state::ScanProgress { running: true, done: false, log: Vec::new() };
    }
    let st2 = st.clone();
    tokio::spawn(async move {
        let push = |s: String| {
            if let Ok(mut p) = st2.scan_progress.lock() {
                p.log.push(s);
            }
        };
        let finish = |st: &AppState, tail: String| {
            if let Ok(mut p) = st.scan_progress.lock() {
                p.log.push(tail);
                p.running = false;
                p.done = true;
            }
        };

        // ① agent 配置发现（opencode 等）
        push("\u{2460} 扫描 agent 配置 …".into());
        st2.rescan_discovery();

        // ② 逐 provider 强制重拉（删缓存文件绕过 1h TTL）
        let cache = evolve_discovery::remote::cache_path(&st2.config.data.dir);
        let _ = std::fs::remove_file(&cache);
        let records: Vec<evolve_core::types::ModelRecord> = st2.engine.catalog_snapshot();
        let self_origin = format!("http://127.0.0.1:{}", st2.config.server.port);
        let removed: std::collections::HashSet<String> =
            crate::state::load_overrides(&st2.config.data.dir).removed_base_urls.into_iter().collect();
        let groups: Vec<_> = evolve_discovery::remote::provider_groups_ex(&records, Some(&self_origin))
            .into_iter().filter(|g| !removed.contains(&g.base_url)).collect();
        let total = groups.len();
        push(format!("\u{2461} 远程重拉 {} 个 provider …", total));
        let mut ok_cnt = 0usize;
        let mut err_cnt = 0usize;
        for (i, g) in groups.iter().enumerate() {
            let hint = g.provider_hint.clone();
            let host = g.base_url.split("://").nth(1).unwrap_or(&g.base_url).split('/').next().unwrap_or("").to_string();
            push(format!("  [{}/{}] {}（{}）…", i + 1, total, hint, host));
            match evolve_discovery::remote::fetch_provider_models(&st2.http, &g.base_url, g.key.as_deref()).await {
                Ok(mut models) => {
                    let fetched = models.len();
                    for m in &mut models {
                        // id 用 hint 前缀（与已发现条目同族，同 discover_remote_blocking）
                        m.id = format!("{hint}/{}", m.upstream_model);
                        m.provider = hint.clone();
                    }
                    let mut added = 0usize;
                    let mut cur = st2.engine.catalog_snapshot();
                    for r in models {
                        if !cur.iter().any(|x| x.id == r.id) {
                            cur.push(r);
                            added += 1;
                        }
                    }
                    if added > 0 {
                        st2.engine.replace_catalog(cur);
                    }
                    ok_cnt += 1;
                    push(format!("  \u{2713} {}：拉到 {} models，新增 {}", hint, fetched, added));
                }
                Err(e) => {
                    err_cnt += 1;
                    push(format!("  \u{2717} {}：{}", hint, e));
                }
            }
        }
        let catalog_size = st2.engine.catalog_snapshot().len();
        finish(&st2, format!("完成：成功 {} / 失败 {}，catalog 共 {} models", ok_cnt, err_cnt, catalog_size));
    });
    (axum::Json(json!({"status": "started"}))).into_response()
}

/// 扫描进度轮询（面板 500ms 一次，done=true 停止）
pub async fn api_providers_scan_progress(State(st): State<AppState>) -> Response {
    match st.scan_progress.lock() {
        Ok(p) => axum::Json(json!({
            "running": p.running, "done": p.done, "log": p.log,
        })).into_response(),
        Err(_) => (StatusCode::INTERNAL_SERVER_ERROR,
            axum::Json(json!({"error": {"message": "lock poisoned"}}))).into_response(),
    }
}

/// Provider 实时状态（每 5s 轮询）：health、最后成功时间、当前使用率
/// 近 5h 每 Provider 积分用量：事件回扫（plan_credits_used 折算，与配额执行
/// 同口径）。plan_usage_sums 为内存账本、重启即清零——展示口径改走事件回扫，
/// 重启不丢、与 tokens_24h 同源。美元制套餐（如 OpenCode Go）折算仍为 0（v2 TODO）。
fn usage_5h_by_provider(
    snap: &[evolve_core::types::ModelRecord],
    events: &evolve_memory::events::EventLog,
    now_ms: u64,
) -> std::collections::HashMap<String, f64> {
    let mut out = std::collections::HashMap::new();
    let prov_of_model: std::collections::HashMap<&str, &str> =
        snap.iter().map(|m| (m.id.as_str(), m.provider.as_str())).collect();
    let Some(path) = events.path() else { return out };
    let Ok(content) = std::fs::read_to_string(path) else { return out };
    let cutoff = (now_ms as i64) - 5 * 3600 * 1000;
    for line in content.lines().rev() {
        let Ok(v) = serde_json::from_str::<serde_json::Value>(line) else { continue };
        if (v.get("ts").and_then(|x| x.as_i64()).unwrap_or(0)) < cutoff {
            break;
        }
        if v.get("kind").and_then(|x| x.as_str()) != Some("chat") {
            continue;
        }
        let Some(chosen) = v.get("chosen").and_then(|x| x.as_str()) else { continue };
        let Some(prov) = prov_of_model.get(chosen) else { continue };
        let Some(mm) = snap.iter().find(|m| m.id == chosen) else { continue };
        let pt = v.get("prompt_tokens").and_then(|x| x.as_f64()).unwrap_or(0.0);
        let ct = v.get("completion_tokens").and_then(|x| x.as_f64()).unwrap_or(0.0);
        let cached = v.get("cached_tokens").and_then(|x| x.as_f64()).unwrap_or(0.0);
        *out.entry(prov.to_string()).or_insert(0.0) +=
            evolve_core::plans::plan_credits_used(&mm.base_url, chosen, pt, cached, ct);
    }
    out
}

/// 近 24h 每 Provider token 用量：事件回扫（单文件倒序，越过 24h 界即停；
/// events.jsonl.1 轮转历史不计——本地量级下单文件通常远超 24h 覆盖）。
/// /api/providers 与 /api/providers/status 共用，保证两个消费端同源。
/// 返回 (各 Provider 近 24h tokens, 各 Provider 最近一次使用 ts_ms)
fn tokens_24h_by_provider(
    snap: &[evolve_core::types::ModelRecord],
    events: &evolve_memory::events::EventLog,
    now_ms: u64,
) -> (std::collections::HashMap<String, u64>, std::collections::HashMap<String, i64>) {
    let mut out = std::collections::HashMap::new();
    let mut last_used: std::collections::HashMap<String, i64> = std::collections::HashMap::new();
    let prov_of_model: std::collections::HashMap<&str, &str> =
        snap.iter().map(|m| (m.id.as_str(), m.provider.as_str())).collect();
    let Some(path) = events.path() else { return (out, last_used) };
    let Ok(content) = std::fs::read_to_string(path) else { return (out, last_used) };
    let cutoff = (now_ms as i64) - 24 * 3600 * 1000;
    for line in content.lines().rev() {
        let Ok(v) = serde_json::from_str::<serde_json::Value>(line) else { continue };
        if (v.get("ts").and_then(|x| x.as_i64()).unwrap_or(0)) < cutoff {
            break;
        }
        if v.get("kind").and_then(|x| x.as_str()) != Some("chat") {
            continue;
        }
        let Some(chosen) = v.get("chosen").and_then(|x| x.as_str()) else { continue };
        let Some(prov) = prov_of_model.get(chosen) else { continue };
        let pt = v.get("prompt_tokens").and_then(|x| x.as_f64()).unwrap_or(0.0);
        let ct = v.get("completion_tokens").and_then(|x| x.as_f64()).unwrap_or(0.0);
        *out.entry(prov.to_string()).or_insert(0) += (pt + ct) as u64;
        let ts = v.get("ts")
                .and_then(|x| x.as_i64().or_else(|| x.as_f64().map(|f| f as i64)))
                .unwrap_or(0);
        let e = last_used.entry(prov.to_string()).or_insert(0);
        if ts > *e {
            *e = ts;
        }
    }
    (out, last_used)
}

/// 账号（provider）级健康聚合：返回 (最紧冷却项, 永久黑名单模型名列表)。
/// 冷却项 = (剩余ms, model_id, kind, message)。`/api/providers` 与
/// `/api/providers/status` 共用——单一事实源，避免两端各自聚合口径漂移。
#[allow(clippy::type_complexity)]
fn provider_health(
    health_snap: &evolve_core::types::HealthMap,
    snap: &[evolve_core::types::ModelRecord],
    provider: &str,
    now_ms: u64,
) -> (Option<(u64, String, String, String)>, Vec<String>) {
    let mut coolest: Option<(u64, String, String, String)> = None;
    let mut blocked: Vec<String> = Vec::new();
    for (k, h) in health_snap.iter() {
        let mid = k.split('\u{1f}').next().unwrap_or("");
        if !snap.iter().any(|mm| mm.id == mid && mm.provider == provider) {
            continue;
        }
        // 永久黑名单（404 且不在模型列表）：不进冷却数字，单独暴露给面板/网页做恢复入口
        if h.until_epoch_ms == Some(evolve_memory::health::HealthRegistry::PERMANENT_UNTIL) {
            blocked.push(mid.split('/').last().unwrap_or(mid).to_string());
            continue;
        }
        if let Some(remain) = h.cooldown_remaining_ms(now_ms)
            && coolest.as_ref().map(|(r, _, _, _)| remain > *r).unwrap_or(true)
        {
            coolest = Some((remain, mid.to_string(), h.kind.label().to_string(), h.message.clone()));
        }
    }
    (coolest, blocked)
}

/// 配额窗口派生（单一事实源）：官方 observed 实测优先，本地 5h 用量估算兜底。
/// 返回 `(逐窗, 绑定窗口)`。窗口固定 5h/weekly/monthly；weekly 兼容 `7d` 键名
/// （Anthropic 系把周窗写成 7d）。绑定窗口 = 用量最高者（全 0% 时取首个，
/// 保证只要声明过额度就有 scope，客户端不会回落 "payg" 文案）；有窗口实测耗尽时
/// exhausted=true、reset 取耗尽窗口里最长的恢复时间。
/// 客户端（macOS 面板 / web 面板）只渲染，不再各自计算——此前两端各写一份，
/// 已多次出现口径漂移。
fn derive_quota(observed: &Value, windows: &Value, used_5h: f64) -> (Value, Value) {
    let mut per = serde_json::Map::new();
    let obs_at = |scope: &str| -> Option<&Value> {
        observed
            .get(scope)
            .filter(|v| !v.is_null())
            .or_else(|| if scope == "weekly" { observed.get("7d").filter(|v| !v.is_null()) } else { None })
    };
    for (scope, sim) in [("5h", 1.0f64), ("weekly", 7.0), ("monthly", 30.0)] {
        if let Some(o) = obs_at(scope)
            && let (Some(limit), Some(remaining)) = (
                o.get("limit").and_then(|v| v.as_f64()),
                o.get("remaining").and_then(|v| v.as_f64()),
            )
            && limit > 0.0
        {
            let used_pct = (((limit - remaining) / limit) * 100.0).round().clamp(0.0, 100.0);
            per.insert(scope.to_string(), json!({
                "used_pct": used_pct, "limit": limit, "remaining": remaining,
                "reset_epoch_ms": o.get("reset_epoch_ms").cloned().unwrap_or(Value::Null),
                "source": "observed",
            }));
            continue;
        }
        // 估算：本地 5h 用量 × 窗口系数 ÷ 声明额度。**有声明额度即出窗**（用量为 0
        // 时显示 0%）——窗口本身标识"这是订阅方案"，缺了它面板只能回落到 "payg"
        // 文案，把 plan 误标成按量（volcengine 这类仅有响应头、无官方端点的 provider
        // 一旦被配额闸挡下，观测值 5 分钟即过期 → 曾因此整块显示成 payg）。
        let allowance = windows.get(scope).and_then(|v| v.as_f64()).filter(|a| *a > 0.0);
        if let Some(a) = allowance {
            let used = used_5h * sim;
            per.insert(scope.to_string(), json!({
                "used_pct": ((used / a) * 100.0).round().clamp(0.0, 100.0),
                "limit": a, "remaining": (a - used).max(0.0),
                "reset_epoch_ms": Value::Null, "source": "estimate",
            }));
        }
    }
    // 绑定窗口 = 用量最高者；初值 -1 保证"有窗就选一个"（全 0% 时取首个=5h），
    // 否则 scope 为 null → 客户端回落 "payg" 文案。有实测耗尽时 exhausted=true、
    // reset 取耗尽窗口里最长的恢复时间。
    let mut b_used = -1.0f64;
    let mut b_scope = Value::Null;
    let mut exhausted = false;
    let mut b_reset: Option<f64> = None;
    for (scope, w) in &per {
        if let Some(used) = w.get("used_pct").and_then(|v| v.as_f64())
            && used > b_used
        {
            b_used = used;
            b_scope = json!(scope);
        }
        if w.get("source").and_then(|v| v.as_str()) == Some("observed")
            && w.get("remaining").and_then(|v| v.as_f64()).unwrap_or(1.0) <= 0.0
        {
            exhausted = true;
            if let Some(r) = w.get("reset_epoch_ms").and_then(|v| v.as_f64()) {
                b_reset = Some(b_reset.map_or(r, |c: f64| c.max(r)));
            }
        }
    }
    let binding = json!({
        // 无任何窗口时 b_used 停在初值 -1（用于"有窗就选一个"），对外必须归零
        "scope": b_scope, "used_pct": b_used.max(0.0), "exhausted": exhausted,
        "reset_epoch_ms": b_reset,
    });
    (Value::Object(per), binding)
}

pub async fn api_providers_status(State(st): State<AppState>) -> Response {
    let now_ms = evolve_memory::health::now();
    let health_snap = st.health.snapshot();
    let sums = st.quota.plan_usage_sums(now_ms, 5 * 3600 * 1000);
    let soft_pct = st.config.policy.plan_soft_pct;
    let ov = crate::state::load_overrides(&st.config.data.dir);
    let mut out: Vec<Value> = Vec::new();
    let snap = st.engine.catalog_snapshot();
    let (tok24, last_used) = tokens_24h_by_provider(&snap, &st.events, now_ms);
    use std::hash::{Hash, Hasher};
    let key_fp = |k: &str| {
        let mut h = std::collections::hash_map::DefaultHasher::new();
        k.hash(&mut h);
        format!("{:016x}", h.finish())
    };
    let group_fp = |provider: &str| -> String {
        snap.iter()
            .find(|x| x.provider == provider)
            .and_then(|x| x.key_values().first().cloned())
            .map(|k| key_fp(&k))
            .unwrap_or_default()
    };
    // 身份遮蔽（与 /api/providers 同规则）：baseUrl+apiKey 指纹。
    // 含 disabled 条目——禁用行仍要展示（可恢复），其发现型孪生必须继续被遮蔽
    let manual_idents: std::collections::HashSet<(String, String)> = ov.providers.iter()
        .map(|(k, p)| {
            let fp = p.api_key.as_deref().map(|s| key_fp(s))
                .or_else(|| p.key_fp.clone())
                .unwrap_or_else(|| group_fp(k));
            (p.base_url.clone(), fp)
        })
        .collect();
    // 用量按账号（provider）归属：同端点多账号不互串
    let mut used_by_provider: std::collections::HashMap<String, f64> = std::collections::HashMap::new();
    let mut used_by_url: std::collections::HashMap<String, f64> = std::collections::HashMap::new();
    for m in &snap {
        if !m.plan { continue; }
        if let Some((i, c, o)) = sums.get(&m.id) {
            let credits = evolve_core::plans::plan_credits_used(&m.base_url, &m.id, *i, *c, *o);
            *used_by_provider.entry(m.provider.clone()).or_insert(0.0) += credits;
            *used_by_url.entry(m.base_url.clone()).or_insert(0.0) += credits;
        }
    }
    for (key, pd) in &ov.providers {
        // 禁用行仍展示（带 disabled 标记）——否则面板/网页的"恢复"入口永不出现
        let has_named = snap.iter().any(|m| &m.provider == key);
        let used = if has_named {
            used_by_provider.get(key).copied().unwrap_or(0.0)
        } else {
            used_by_url.get(&pd.base_url).copied().unwrap_or(0.0)
        };
        let allowance = pd.window_5h.or_else(|| {
            let pk = evolve_core::plans::plan_for(&pd.base_url).map(|p| p.key).unwrap_or("");
            evolve_core::plans::tier_allowance_by_key(pk, &pd.tier, 0)
        });
        let pressure = allowance.filter(|a| *a > 0.0).map(|a| used / a);
        let (coolest, blocked) = provider_health(&health_snap, &snap, key, now_ms);
        let cool = coolest.as_ref().map(|(r, _, _, _)| *r);
        let cool_model = coolest.as_ref().map(|(_, m, _, _)| m.clone());
        let cool_msg = coolest.as_ref().map(|(_, _, _, msg)| msg.clone()).unwrap_or_default();
        let cool_kind = coolest.as_ref().map(|(_, _, kd, _)| kd.clone()).unwrap_or_default();
        out.push(json!({
            "key": key, "source": "manual",
            "base_url": pd.base_url, "tier": pd.tier, "plan_kind": pd.plan_kind,
            "disabled": pd.disabled,
            "allowance_5h": allowance,
            "used_5h": if used > 0.0 { (used * 100.0).round() / 100.0 } else { 0.0 },
            "pressure": pressure,
            "soft_pct": soft_pct,
            "cooldown_remaining_ms": cool,
            "cooldown_model": cool_model,
            "message": cool_msg,
            "kind": cool_kind,
            "blocked_models": blocked,
            "available": cool.is_none() && blocked.is_empty(),
            "tokens_24h": tok24.get(key).copied().unwrap_or(0),
            "last_used_ms": last_used.get(key).copied().unwrap_or(0),
            "fetched_models_count": pd.fetched_models.len(),
        }));
    }
    use std::collections::HashMap;
    let mut grouped: HashMap<String, Value> = HashMap::new();
    for m in &snap {
        if manual_idents.contains(&(m.base_url.clone(), group_fp(&m.provider))) { continue; }
        let profile = evolve_core::plans::plan_for(&m.base_url);
        let plan_key = profile.map(|p| p.key.to_string()).unwrap_or_default();
        let entry = grouped.entry(m.provider.clone()).or_insert_with(|| json!({
            "key": m.provider.clone(),
            "source": "scanned",
            "disabled": false,
            "plan_kind": profile.map(|p| p.plan_kind).unwrap_or("api"),
            "docs_url": profile.map(|p| p.docs_url).unwrap_or(""),
            "model_count": 0u64,
            "models": Vec::<Value>::new(),
            "allowance_5h": ov.providers.get(&plan_key).and_then(|p| p.window_5h),
            "tokens_24h": tok24.get(m.provider.as_str()).copied().unwrap_or(0),
            "last_used_ms": last_used.get(m.provider.as_str()).copied().unwrap_or(0),
        }));
        if let Some(arr) = entry.get_mut("models").and_then(|v| v.as_array_mut()) {
            arr.push(serde_json::Value::String(m.id.clone()));
        }
        if let Some(o) = entry.as_object_mut() {
            let cur = o.get("model_count").and_then(|v| v.as_u64()).unwrap_or(0);
            o.insert("model_count".into(), serde_json::json!(cur + 1));
        }
    }
    for (provider, mut e) in grouped {
        let used: f64 = e.get("models").and_then(|v| v.as_array())
            .map(|arr| arr.iter().filter_map(|id| id.as_str()).filter_map(|id| {
                let mm = snap.iter().find(|m| m.id == id)?;
                sums.get(id).map(|(i, c, o)| evolve_core::plans::plan_credits_used(&mm.base_url, &mm.id, *i, *c, *o))
            }).sum())
            .unwrap_or(0.0);
        let allowance = e.get("allowance_5h").and_then(|v| v.as_f64());
        let pressure = allowance.filter(|a| *a > 0.0).map(|a| used / a);
        let (coolest, blocked) = provider_health(&health_snap, &snap, &provider, now_ms);
        let cool = coolest.as_ref().map(|(r, _, _, _)| *r);
        let cool_model = coolest.as_ref().map(|(_, m, _, _)| m.clone());
        let cool_msg = coolest.as_ref().map(|(_, _, _, msg)| msg.clone()).unwrap_or_default();
        let cool_kind = coolest.as_ref().map(|(_, _, kd, _)| kd.clone()).unwrap_or_default();
        e.as_object_mut().map(|o| {
            o.insert("used_5h".into(), json!((used * 100.0).round() / 100.0));
            o.insert("pressure".into(), json!(pressure));
            o.insert("cooldown_remaining_ms".into(), json!(cool));
            o.insert("cooldown_model".into(), json!(cool_model));
            o.insert("message".into(), json!(cool_msg));
            o.insert("kind".into(), json!(cool_kind));
            o.insert("blocked_models".into(), json!(blocked));
            o.insert("available".into(), json!(cool.is_none() && blocked.is_empty()));
        });
        out.push(e);
    }
    // 契约：全接口统一 snake_case（与 /api/health、/api/stats、/api/model-tags 一致）
    // ——web 面板与 macOS 面板共用同一字段名，禁止在此做 camelCase 重映射（曾因
    // 重映射只改服务端、web 未跟随而整块 Provider 卡失效）。
    (axum::Json(json!({"providers": out, "soft_pct": soft_pct, "now": now_ms})))
        .into_response()
}

/// Provider 管理列表：内置注册表 + 扫描发现 + 手动定义，合并展示
/// 按量 provider 余额刷新（deepseek /user/balance）：缓存 >60s 视为过期，后台拉。
/// 面板轮询 `/api/providers` 与 90s watchdog 共用——**watchdog 让启动即预热**，
/// 否则网关刚起来时面板取不到余额 → 回落到字面 "payg" 文案，几秒后又变回余额
/// （用户观测到的"余额/payg 闪烁"）。
fn refresh_payg_balances_if_stale(st: &AppState) {
    let pb = st.payg_balance.lock().map(|b| b.clone()).unwrap_or_default();
    let now0 = evolve_memory::health::now();
    let stale = |k: &str| {
        pb.get(k)
            .and_then(|v| v["ts"].as_u64())
            .map(|t| now0.saturating_sub(t) > 60 * 1000)
            .unwrap_or(true)
    };
    if stale("deepseek") {
        let st2 = st.clone();
        tokio::spawn(async move {
            let now_ms = evolve_memory::health::now();
            if let Some(mut bal) = deepseek_balance(&st2, "deepseek", None).await {
                bal["ts"] = json!(now_ms);
                if let Ok(mut pb) = st2.payg_balance.lock() {
                    pb.insert("deepseek".into(), bal);
                }
            }
        });
    }
}

pub async fn api_providers(State(st): State<AppState>) -> Response {
    let ov = crate::state::load_overrides(&st.config.data.dir);
    // 积分制官方配额刷新：observed 过期（>60s）后台并行拉取（面板轮询自然驱动；
    // 另有 90s watchdog 兜底——面板不开也在刷）。配额指纹源自官方 API，
    // 无官方端点的 provider 用响应头/估算，消费侧按 quota_source 标注
    tokio::spawn(quota_refresh_tick(st.clone(), 60 * 1000));
    // 按量 provider 余额（deepseek）：缓存过期（>60s）→ 后台拉。启动即预热，
    // 免得网关刚起来时面板取不到余额而回落到字面 "payg"（随后又变回余额=闪烁）
    refresh_payg_balances_if_stale(&st);
    let budgets = st.plan_budgets.lock().map(|b| b.clone()).unwrap_or_default();
    let soft_pct = st.config.policy.plan_soft_pct;
    let health_snap = st.health.snapshot();
    let now_ms = evolve_memory::health::now();
    let mut out: Vec<Value> = Vec::new();
    // 判重指纹（用户裁决）：apiKey+baseUrl 唯一标识 provider。指纹哈希进响应仅用于判重。
    use std::hash::{Hash, Hasher};
    let key_fp = |k: &str| {
        let mut h = std::collections::hash_map::DefaultHasher::new();
        k.hash(&mut h);
        format!("{:016x}", h.finish())
    };
    // 1. 手动定义的（最优先——用户自己加的）
    let snap_all = st.engine.catalog_snapshot();
    let (tok24, last_used) = tokens_24h_by_provider(&snap_all, &st.events, now_ms);
    let credits5h = usage_5h_by_provider(&snap_all, &st.events, now_ms);
    for (key, pd) in &ov.providers {
        // 禁用行仍展示（带 disabled 标记）——其发现型孪生已被身份遮蔽抑制，
        // 用户可在面板/网页一键恢复
        // 档位联动：显式窗口额度 > 注册表档位额度（选档位后配额条立即生效）
        let plan_key = evolve_core::plans::plan_for(&pd.base_url).map(|p| p.key).unwrap_or("");
        let tier_allow = |w: usize| evolve_core::plans::tier_allowance_by_key(plan_key, &pd.tier, w);
        // 手动也查用量：按账号归属——目录里有同名 provider（编辑自发现条的
        // overlay）按 provider 过滤；纯自定义条目退回 base_url 匹配
        let has_named = snap_all.iter().any(|m| &m.provider == key);
        let used = if has_named {
            credits5h.get(key).copied().unwrap_or(0.0)
        } else {
            // 纯自定义条目：同 base_url 各 provider 求和
            snap_all.iter()
                .filter(|m| m.base_url == pd.base_url)
                .filter_map(|m| credits5h.get(&m.provider))
                .sum::<f64>()
        };
        out.push(json!({
            "key": key, "source": "manual",
            "base_url": pd.base_url, "protocol": pd.protocol,
            "disabled": pd.disabled,
            "plan_key": plan_key,
            "plan_kind": pd.plan_kind, "docs_url": pd.docs_url,
            "tier": pd.tier,
            "windows": {
                "5h": pd.window_5h.or_else(|| tier_allow(0)),
                "weekly": pd.window_weekly.or_else(|| tier_allow(1)),
                "monthly": pd.window_monthly.or_else(|| tier_allow(2)),
            },
            "model_rates": pd.model_rates,
            "fetched_models": pd.fetched_models,
            "observed": observed_for_provider(&st.quota, key, now_ms),
            "has_key": pd.api_key.is_some(),
            "key_fp": pd.api_key.as_deref().map(key_fp).unwrap_or_default(),
            "used_5h": if used > 0.0 { (used * 100.0).round() / 100.0 } else { 0.0 },
            "tokens_24h": tok24.get(key).copied().unwrap_or(0),
            "last_used_ms": last_used.get(key).copied().unwrap_or(0),
            "soft_pct": soft_pct,
        }));
    }
    // 2. 目录里实际存在的（扫描+内置合并）。遮蔽按身份（baseUrl+apiKey 指纹，
    // 用户裁决）判断：手动条目与发现组同账号 → 已由手动行展示；同端点不同
    // key 是不同账号（如 opencode-go 与 opencode-go-github）各自展示
    let group_fp = |provider: &str| -> String {
        snap_all.iter()
            .find(|x| x.provider == provider)
            .and_then(|x| x.key_values().first().cloned())
            .map(|k| key_fp(&k))
            .unwrap_or_default()
    };
    let manual_idents: std::collections::HashSet<(String, String)> = ov.providers.iter()
        .map(|(k, p)| {
            // 无显式 key 时回退：编辑自发现 provider 的 overlay 条目按同组现有 key 指纹回填
            let fp = p.api_key.as_deref().map(|s| key_fp(s))
                .or_else(|| p.key_fp.clone())
                .unwrap_or_else(|| group_fp(k));
            (p.base_url.clone(), fp)
        })
        .collect();
    let mut seen_providers = std::collections::HashSet::new();
    for m in &snap_all {
        if !seen_providers.insert(m.provider.clone()) { continue; }
        if manual_idents.contains(&(m.base_url.clone(), group_fp(&m.provider))) { continue; }
        // review：网关自身（self endpoint）不是上游——不出现在 Provider 管理
        let self_ep = format!("127.0.0.1:{}", st.config.server.port);
        let self_ep2 = format!("localhost:{}", st.config.server.port);
        if m.base_url.contains(&self_ep) || m.base_url.contains(&self_ep2) { continue; }
        let profile = evolve_core::plans::plan_for(&m.base_url);
        let plan_key = profile.map(|p| p.key.to_string()).unwrap_or_default();
        let pov = ov.plans.get(&plan_key);
        // tier 回退链：面板档位设置 → 同 plan_key 的手动条目档位（同方案多账号
        // 档位一致，如 opencode-go 与 -github）→ 空（无配额展示，诚实）
        let tier = pov.and_then(|p| p.tier.clone())
            .or_else(|| ov.providers.iter()
                .filter(|(_, p)| !p.disabled && !p.tier.is_empty()
                    && evolve_core::plans::plan_for(&p.base_url).map(|pp| pp.key.to_string()).unwrap_or_default() == plan_key)
                .map(|(_, p)| p.tier.clone())
                .next())
            .unwrap_or_default();
        let tier_str = tier.clone();
        let allowance_5h = budgets.get(&plan_key).copied()
            .or_else(|| evolve_core::plans::tier_allowance_by_key(&plan_key, &tier_str, 0));
        let allowance_weekly = evolve_core::plans::tier_allowance_by_key(&plan_key, &tier_str, 1);
        let allowance_monthly = evolve_core::plans::tier_allowance_by_key(&plan_key, &tier_str, 2);
        // 用量：该 provider（账号）全部模型的 5h 积分合计——同端点多账号按
        // provider 归属，不按 base_url（会把别家账号的用量算进来）
        let models_at_url: Vec<&evolve_core::types::ModelRecord> = snap_all.iter()
            .filter(|x| x.provider == m.provider)
            .collect();
        let used: f64 = credits5h.get(m.provider.as_str()).copied().unwrap_or(0.0);
        // 下次窗口重置：组内模型的健康冷却取最近
        let next_reset = health_snap.iter()
            .filter(|(k, _h)| {
                let km = k.split('\u{1f}').next().unwrap_or(k);
                models_at_url.iter().any(|mm| mm.id == *km)
            })
            .filter(|(_, h)| matches!(h.kind, evolve_core::types::HealthKind::QuotaExhausted | evolve_core::types::HealthKind::RateLimited))
            .filter_map(|(_, h)| h.until_epoch_ms.filter(|u| *u > now_ms))
            .min();
        let model_ids: Vec<&str> = models_at_url.iter().map(|mm| mm.id.as_str()).collect();
        out.push(json!({
            "key": m.provider.clone(), "source": "discovered",
            "disabled": false,
            "base_url": m.base_url, "protocol": if m.protocol == evolve_core::types::Protocol::Anthropic { "anthropic" } else { "openai" },
            "plan_key": plan_key,
            "plan_kind": profile.map(|p| p.plan_kind).unwrap_or("api"),
            "docs_url": profile.map(|p| p.docs_url).unwrap_or(""),
            "tier": tier,
            "windows": {
                "5h": allowance_5h,
                "weekly": allowance_weekly,
                "monthly": allowance_monthly,
            },
            "model_rates": profile.map(|p| p.model_rates).unwrap_or(""),
            "observed": observed_for_provider(&st.quota, &m.provider, now_ms),
            "model_count": model_ids.len(),
            "models": model_ids,
            "has_key": m.has_credential(),
            "key_fp": m.key_values().first().map(|k| key_fp(k)).unwrap_or_default(),
            "used_5h": if used > 0.0 { (used * 100.0).round() / 100.0 } else { 0.0 },
            "tokens_24h": tok24.get(m.provider.as_str()).copied().unwrap_or(0),
            "last_used_ms": last_used.get(m.provider.as_str()).copied().unwrap_or(0),
            "soft_pct": soft_pct,
            "next_reset_epoch_ms": next_reset,
        }));
    }
    // 同套餐归一（用户裁决 10-12）：同 plan_key 的多账号条目合并为一行展示——
    // 用量/24h tokens 求和、模型并集、windows 逐窗取首个非空、标量取首个非空值。
    // plan_key 为空（无套餐画像的自定义端点）保持独立。前端（面板/托盘）零适配。
    let mut merged: Vec<Value> = Vec::new();
    let mut merged_idx: std::collections::HashMap<String, usize> = std::collections::HashMap::new();
    for e in out {
        // 合并键：API key 指纹（不同账号不同行；同账号聚合多 provider 名）
        let pk = e.get("key_fp").and_then(|v| v.as_str()).unwrap_or("").to_string();
        if pk.is_empty() {
            // 无 key 的发现型（未自动登记）保持独立行
            merged.push(e);
            continue;
        }
        match merged_idx.get(&pk).copied() {
            None => {
                let mut e = e;
                if let Some(o) = e.as_object_mut() {
                    let k0 = o.get("key").cloned().unwrap_or(json!(""));
                    o.insert("merged_from".into(), json!([k0]));
                    // 行名归一为套餐名（plan_key），同套餐多账号聚合后不再以账号名为主键
                        o.insert("key".into(), json!(k0.as_str().unwrap_or(&pk).to_string()));
                }
                merged_idx.insert(pk, merged.len());
                merged.push(e);
            }
            Some(i) => {
                if let (Some(dst), Some(src)) = (merged[i].as_object_mut(), e.as_object()) {
                // idempotent used: last_used 取 max
                if let (Some(dl), Some(sl)) = (
                    dst.get("last_used_ms").and_then(|v| v.as_i64()),
                    src.get("last_used_ms").and_then(|v| v.as_i64()),
                ) {
                    if sl > dl {
                        dst.insert("last_used_ms".into(), json!(sl));
                    }
                }
                // 求和字段：used_5h 浮点（两位舍入），tokens/count 整数
                for k in ["used_5h", "tokens_24h", "model_count"] {
                    if k == "used_5h" {
                        let s = dst.get(k).and_then(|v| v.as_f64()).unwrap_or(0.0)
                            + src.get(k).and_then(|v| v.as_f64()).unwrap_or(0.0);
                        dst.insert(k.into(), json!((s * 100.0).round() / 100.0));
                    } else {
                        let s = dst.get(k).and_then(|v| v.as_u64()).unwrap_or(0)
                            + src.get(k).and_then(|v| v.as_u64()).unwrap_or(0);
                        dst.insert(k.into(), json!(s));
                    }
                }
                // 数组并集（保持序）；merged_from 源无值时回退其 key
                for k in ["models", "fetched_models", "merged_from"] {
                    let mut arr = dst.get(k).and_then(|v| v.as_array()).cloned().unwrap_or_default();
                    let src_arr = src.get(k).and_then(|v| v.as_array()).cloned()
                        .or_else(|| {
                            if k == "merged_from" {
                                src.get("key").cloned().map(|k0| vec![k0])
                            } else {
                                None
                            }
                        });
                    if arr.is_empty() {
                        if let Some(sv) = src_arr {
                            arr = sv;
                        }
                    } else if let Some(sv) = src_arr {
                        for x in sv {
                            if !arr.contains(&x) {
                                arr.push(x);
                            }
                        }
                    }
                    dst.insert(k.into(), json!(arr));
                }
                    // windows 逐窗取首个非空
                    if let (Some(dw), Some(sw)) = (dst.get_mut("windows").and_then(|v| v.as_object_mut()),
                                                   src.get("windows").and_then(|v| v.as_object())) {
                        for (wk, wv) in sw {
                            if !wv.is_null() && dw.get(wk).map(|c| c.is_null()).unwrap_or(true) {
                                dw.insert(wk.clone(), wv.clone());
                            }
                        }
                    }
                    // 其余标量：目标空值时回填来源非空值（tier/docs_url/has_key/next_reset 等）
                    for (k, sv) in src {
                        if k == "key" || k == "key_fp" || k == "source" {
                            continue;
                        }
                        let cur_empty = dst.get(k).map(|c| {
                            c.is_null() || c.as_str().map(|s| s.is_empty()).unwrap_or(false)
                        }).unwrap_or(true);
                        if cur_empty && !sv.is_null() {
                            dst.insert(k.clone(), sv.clone());
                        }
                    }
                }
            }
        }
    }
    let mut out = merged;
    // 判重标记：同 (base_url, key指纹) 的第二处 → duplicate_of=先出现的 provider 名。
    // 同 base_url 不同 key 是两个 provider（如 opencode-go 与 opencode-go-github），不算重复。
    {
        let mut seen: std::collections::HashMap<String, String> = std::collections::HashMap::new();
        for e in out.iter_mut() {
            let b = e.get("base_url").and_then(|v| v.as_str()).unwrap_or("").to_string();
            let fp = e.get("key_fp").and_then(|v| v.as_str()).unwrap_or("").to_string();
            let ident = format!("{}\u{1f}{}", b, fp);
            let name = e.get("key").and_then(|v| v.as_str()).unwrap_or("").to_string();
            if let Some(first) = seen.get(&ident) {
                e.as_object_mut().map(|o| o.insert("duplicate_of".into(), json!(first)));
            } else {
                seen.insert(ident, name);
            }
            e.as_object_mut().map(|o| o.remove("key_fp"));
        }
    }
    out.sort_by(|a, b| {
        let ka = a.get("key").and_then(|v| v.as_str()).unwrap_or("");
        let kb = b.get("key").and_then(|v| v.as_str()).unwrap_or("");
        ka.cmp(kb)
    });
    // 附加按量 provider 余额（payg_balance 缓存；deepseek 等）
    {
        let pb = st.payg_balance.lock().map(|b| b.clone()).unwrap_or_default();
        for row in out.iter_mut() {
            if let Some(k) = row["key"].as_str() {
                if let Some(bal) = pb.get(k) {
                    row["payg_balance"] = json!({
                        "currency": bal["currency"], "total": bal["total"],
                        "is_available": bal["is_available"],
                    });
                }
            }
        }
    }
    // w2 信息源标注：配额数字从哪来？"official" = 官方 API 端点/官方响应头
    // （observed store 5min 内新鲜，或官方余额实测）；"estimated" = 网关 credits
    // 估算兜底；"" = 无配额概念（api/payg 型，不涉及窗口）
    {
        let now_q = evolve_memory::health::now();
        let has_observed = |provider: &str| ["5h", "7d", "weekly", "monthly"].iter().any(|sc| st.quota.observed_window(provider, sc, now_q, 5 * 60 * 1000).is_some());
        for row in out.iter_mut() {
            let pk = row.get("key").and_then(|v| v.as_str()).unwrap_or("");
            let payg = row.get("payg_balance").is_some();
            let src = if payg || has_observed(pk) {
                "official"
            } else if row.get("windows").map(|w| !w.as_object().map(|m| m.is_empty()).unwrap_or(true)).unwrap_or(false) {
                "official_headers"
            } else if row.get("used_5h").and_then(|v| v.as_f64()).map(|u| u > 0.0).unwrap_or(false) {
                "estimated"
            } else {
                ""
            };
            if let Some(o) = row.as_object_mut() { o.insert("quota_source".into(), json!(src)); }
        }
    }
    // 派生视图（单一事实源）：配额窗口 + 绑定窗口 + 冷却/黑名单/可用性——macOS 面板
    // 与 web 面板只渲染，不再各自计算（此前两端各写一份，已多次出现口径漂移）。
    // 冷却/黑名单按 merged_from 跨来源账号聚合（本端点合并行，status 端不合并）。
    {
        let now_d = evolve_memory::health::now();
        for row in out.iter_mut() {
            let keys: Vec<String> = row
                .get("merged_from")
                .and_then(|v| v.as_array())
                .map(|a| a.iter().filter_map(|x| x.as_str().map(str::to_string)).collect())
                .unwrap_or_else(|| {
                    row.get("key").and_then(|v| v.as_str()).map(|k| vec![k.to_string()]).unwrap_or_default()
                });
            let mut cool: Option<(u64, String, String, String)> = None;
            let mut blocked: Vec<String> = Vec::new();
            for k in &keys {
                let (c, b) = provider_health(&health_snap, &snap_all, k, now_d);
                if let Some(c) = c
                    && cool.as_ref().map(|x| c.0 > x.0).unwrap_or(true)
                {
                    cool = Some(c);
                }
                blocked.extend(b);
            }
            blocked.sort();
            blocked.dedup();
            let disabled = row.get("disabled").and_then(|v| v.as_bool()).unwrap_or(false);
            let available = cool.is_none() && blocked.is_empty() && !disabled;
            let observed = row.get("observed").cloned().unwrap_or_else(|| json!({}));
            let windows = row.get("windows").cloned().unwrap_or_else(|| json!({}));
            let used_5h = row.get("used_5h").and_then(|v| v.as_f64()).unwrap_or(0.0);
            let (quota, binding) = derive_quota(&observed, &windows, used_5h);
            if let Some(o) = row.as_object_mut() {
                o.insert("quota".into(), quota);
                o.insert("binding".into(), binding);
                o.insert("blocked_models".into(), json!(blocked));
                o.insert("available".into(), json!(available));
                o.insert(
                    "cooling".into(),
                    match cool {
                        Some((ms, model, kind, cause)) => json!({
                            "remaining_ms": ms, "model": model, "kind": kind, "cause": cause,
                        }),
                        None => Value::Null,
                    },
                );
            }
        }
    }
    // 契约：snake_case（与 /api/providers 同源同命名，勿重映射）
    (axum::Json(json!({"providers": out})))
        .into_response()
}

/// Provider 测实时配额（每次响应头到达即更新，含失败响应 429 reset/remaining）——
/// 面板 quota bars 的权威源（用户裁决：以 Provider 测为准）
/// w2 provider 键控：账号级 observed store（同 plan_key 多账号不互串）
fn observed_for_provider(quota: &std::sync::Arc<evolve_memory::quota::QuotaLedger>, provider: &str, now_ms: u64) -> Value {
    if provider.is_empty() { return json!({}) };
    let mut out = serde_json::Map::new();
    for scope in ["5h", "7d", "tokens", "requests", "weekly", "monthly"] {
        // 常规 5 分钟新鲜度；**额外**放宽到"耗尽且 reset 未到"的陈旧观测——
        // 仅有响应头、无官方端点可查的 provider（如 volcengine）一旦被配额闸挡下就
        // 不再发请求 → 永无新响应头 → 5 分钟后观测过期，面板会忘掉"该窗口已耗尽"
        // （反馈闭环）。reset 一到即自然失效（那时也确实该重新探活）。
        let fresh = quota.observed_window(provider, scope, now_ms, 5 * 60 * 1000);
        let stale_exhausted = || {
            quota.observed_window(provider, scope, now_ms, u64::MAX).filter(|w| {
                w.remaining == Some(0)
                    && w.reset_epoch_ms.map(|r| r > now_ms).unwrap_or(false)
            })
        };
        if let Some(w) = fresh.or_else(stale_exhausted) {
            out.insert(scope.into(), json!({
                // remaining=0 且 reset 未到 → 窗口对抗（provider 明示该窗口耗尽）
                "exhausted": w.remaining == Some(0),
                "remaining": w.remaining, "limit": w.limit,
                "reset_epoch_ms": w.reset_epoch_ms, "updated_epoch_ms": w.updated_epoch_ms,
            }));
        }
    }
    json!(out)
}

/// w3：该 provider 的模型列表是否仍包含此上游模型（404 语义二分）
/// 来源优先级：手动 provider 的 fetched_models（/models 拉取结果）→ 目录记录
pub fn provider_lists_model(st: &AppState, provider: &str, upstream_model: &str) -> bool {
    let ov = crate::state::load_overrides(&st.config.data.dir);
    if let Some(pd) = ov.providers.get(provider)
        && !pd.fetched_models.is_empty()
    {
        return pd.fetched_models.iter().any(|m| m == upstream_model || m.ends_with(upstream_model));
    }
    st.engine.catalog_snapshot().iter().any(|m| {
        m.provider == provider
            && (m.upstream_model == upstream_model || m.id.ends_with(upstream_model))
    })
}

/// w3：官方配额端点可查询（minimax/zhipu/zai/opencode-go）——配额类失败不采信
/// 上游长 retry-after，交给 API 刷新确认（用户裁决）
pub fn provider_is_api_backed(_st: &AppState, base_url: &str) -> bool {
    evolve_core::plans::quota_api_for(base_url).map(|k| k.window_based()).unwrap_or(false)
}

/// w3 模型手动恢复：POST {provider? | model?}
/// 清健康条目（永久黑名单 / 任意冷却）——provider 传参时清该 provider 全部模型
pub async fn api_health_clear(
    State(st): State<AppState>,
    axum::Json(body): axum::Json<serde_json::Value>,
) -> Response {
    let provider = body.get("provider").and_then(|v| v.as_str()).unwrap_or("").to_string();
    let model = body.get("model").and_then(|v| v.as_str()).unwrap_or("").to_string();
    let mut cleared: Vec<String> = Vec::new();
    if !model.is_empty() {
        st.health.clear(std::slice::from_ref(&model));
        cleared.push(model.clone());
    } else if !provider.is_empty() {
        for m in st.engine.catalog_snapshot().iter().filter(|m| m.provider == provider) {
            st.health.clear(std::slice::from_ref(&m.id));
            cleared.push(m.id.clone());
        }
    }
    st.events.record(json!({
        "kind": "health_cleared",
        "provider": provider, "model": model, "cleared": cleared.len(),
    }));
    (axum::Json(json!({"status": "ok", "cleared": cleared}))).into_response()
}

/// 模型标签（2026-10）：默认自动推导 + 用户覆盖（持久到 overrides.json）
/// GET → 全模型标签清单（含来源 auto|user）
pub async fn api_model_tags_get(State(st): State<AppState>) -> Response {
    let dir = st.config.data.dir.clone();
    let ov = crate::state::load_overrides(&dir);
    let snap = st.engine.catalog_snapshot();
    let assigned = tags_for_catalog(&st, &snap);
    let usability = model_usability(&st);
    let (active_ids, ranked) = active_models(&st);
    let active_set: std::collections::HashSet<&String> = active_ids.iter().collect();
    let score_of: std::collections::HashMap<&str, u64> =
        ranked.iter().map(|(i, s)| (i.as_str(), *s)).collect();
    let out: Vec<Value> = snap.iter().map(|m| {
        let (auto_cap, auto_tier) = assigned
            .get(&m.id)
            .map(|(c, p)| (c.label().to_string(), p.label().to_string()))
            .unwrap_or_else(|| ("standard".into(), "medium".into()));
        let user = ov.model_tags.get(&m.id);
        let cap = user.and_then(|u| u.capability.clone()).unwrap_or_else(|| auto_cap.clone());
        let tier = user
            .and_then(|u| u.price_tier.clone())
            .map(|v| norm_price_tier(&v))
            .unwrap_or_else(|| auto_tier.clone());
        json!({
            "model": m.id, "provider": m.provider,
            "capability": cap, "price_tier": tier,
            "capability_auto": auto_cap, "price_tier_auto": auto_tier,
            "source": if user.is_some() { "user" } else { "auto" },
            "usable": !matches!(usability.get(&m.id), Some(Some(_))),
            "unusable_reason": usability.get(&m.id).cloned().flatten(),
            "active": active_set.contains(&m.id),
            "release_ts": evolve_core::recency::release_ts_of(m),
            "release_src": if m.release_ts.is_some() { "provider/models.dev" } else if evolve_core::recency::release_ts_from_id(&m.id).is_some() { "id" } else { "none" },
            "recency_score": score_of.get(m.id.as_str()).copied().unwrap_or(0),
        })
    }).collect();
    (axum::Json(json!({"models": out}))).into_response()
}

/// POST {model, capability?, price_tier?, reset?} → 保存/清除用户覆盖
pub async fn api_model_tags_post(
    State(st): State<AppState>,
    axum::Json(body): axum::Json<serde_json::Value>,
) -> Response {
    let dir = st.config.data.dir.clone();
    let Some(model) = body.get("model").and_then(|v| v.as_str()).map(|s| s.to_string()) else {
        return (StatusCode::BAD_REQUEST, axum::Json(json!({"error": {"message": "model is required"}}))).into_response();
    };
    let mut ov = crate::state::load_overrides(&dir);
    let reset = body.get("reset").and_then(|v| v.as_bool()).unwrap_or(false);
    if reset {
        ov.model_tags.remove(&model);
    } else {
        let e = ov.model_tags.entry(model.clone()).or_default();
        if let Some(c) = body.get("capability").and_then(|v| v.as_str()) {
            e.capability = if c.is_empty() { None } else { Some(c.to_string()) };
        }
        if let Some(p) = body.get("price_tier").and_then(|v| v.as_str()) {
            e.price_tier = if p.is_empty() { None } else { Some(p.to_string()) };
        }
        if e.capability.is_none() && e.price_tier.is_none() {
            ov.model_tags.remove(&model);
        }
    }
    crate::state::save_overrides(&dir, &ov);
    (axum::Json(json!({"status": "ok", "model": model}))).into_response()
}

/// 价位字符串归一（兼容历史 "standard" → "medium"；能力侧保持 standard 不动）
fn norm_price_tier(v: &str) -> String {
    evolve_core::tags::PriceTier::parse(v)
        .map(|p| p.label().to_string())
        .unwrap_or_else(|| v.to_string())
}

/// 当前**可用池**（用户裁决：7:2:1 分位要以真实可用的模型为分母）：
/// 有凭据 ∧ 未被删除 ∧ 无健康冷却（含永久黑名单）∧ 未被配额闸门关闭。
/// 冷却/闸门是动态的 → 调用方带 TTL 缓存，池变化即重算。
pub fn usable_model_ids(st: &AppState) -> std::collections::HashSet<String> {
    model_usability(st)
        .into_iter()
        .filter(|(_, r)| r.is_none())
        .map(|(id, _)| id)
        .collect()
}

/// 活跃集 id 集合（路由过滤用；空集 = 不启用过滤）
pub fn active_id_set(st: &AppState) -> std::collections::HashSet<String> {
    let (active, _) = active_models(st);
    active.into_iter().collect()
}

/// 最近使用时间缓存（30s TTL）：model → 最近调用 ts（epoch ms）
static LAST_USED_CACHE: std::sync::LazyLock<
    std::sync::Mutex<Option<(std::time::Instant, std::collections::HashMap<String, u64>)>>,
> = std::sync::LazyLock::new(|| std::sync::Mutex::new(None));

fn last_used_cached(st: &AppState) -> std::collections::HashMap<String, u64> {
    if let Ok(g) = LAST_USED_CACHE.lock()
        && let Some((at, m)) = g.as_ref()
        && at.elapsed().as_secs() < 30
    {
        return m.clone();
    }
    let mut last_used: std::collections::HashMap<String, u64> = std::collections::HashMap::new();
    if let Some(path) = st.events.path()
        && let Ok(content) = std::fs::read_to_string(path)
    {
        for line in content.lines().rev().take(4000) {
            if let Ok(e) = serde_json::from_str::<Value>(line)
                && let Some(chosen) = e.get("chosen").and_then(|v| v.as_str())
            {
                let ts = e.get("ts").and_then(|v| v.as_u64()).unwrap_or(0);
                last_used.entry(chosen.to_string()).and_modify(|t| *t = (*t).max(ts)).or_insert(ts);
            }
        }
    }
    if let Ok(mut g) = LAST_USED_CACHE.lock() {
        *g = Some((std::time::Instant::now(), last_used.clone()));
    }
    last_used
}

/// 活跃集（用户裁决）：最近发布 + 最近使用的前 50（ACTIVE_CAP），支持手动
/// 开启/停用；开启自动顶掉当前最低分者。返回 (活跃 id 有序表, 全量排名表)
pub fn active_models(
    st: &AppState,
) -> (Vec<String>, Vec<(String, u64)>) {
    let now = evolve_memory::health::now();
    // 最近使用时间（窗口内才计分；过期使用不占"最近使用"名额）。
    // 30s TTL 缓存：此前每请求整读 events 文件（32MB 轮转阈值）+ 解析 4000 行，
    // 是热点路径上的阻塞 I/O（评审 #3）
    let last_used = last_used_cached(st);
    // 结构不可用（no-key / deleted）不占活跃名额；冷却/闸门是临时的，不剔除
    let usability = model_usability(st);
    let snap = st.engine.catalog_snapshot(); // 先持有快照（避免借用临时值）
    let eligible: Vec<&evolve_core::types::ModelRecord> = snap
        .iter()
        .filter(|m| !matches!(usability.get(&m.id), Some(Some("no-key" | "deleted"))))
        .collect();

    // 两个池（各自降序）：新发布（有 id 内嵌日期者） / 最近使用（窗口内）
    let mut by_release: Vec<(String, u64)> = eligible
        .iter()
        .filter_map(|m| evolve_core::recency::release_ts_of(m).map(|ts| (m.id.clone(), ts)))
        .collect();
    by_release.sort_by(|a, b| b.1.cmp(&a.1).then(a.0.cmp(&b.0)));
    let mut by_used: Vec<(String, u64)> = eligible
        .iter()
        .filter_map(|m| last_used.get(&m.id).map(|ts| (m.id.clone(), *ts)))
        .filter(|(_, ts)| now.saturating_sub(*ts) <= evolve_core::recency::USED_WINDOW_MS)
        .collect();
    by_used.sort_by(|a, b| b.1.cmp(&a.1).then(a.0.cmp(&b.0)));

    let ov = crate::state::load_overrides(&st.config.data.dir);
    let active = evolve_core::recency::select_active(
        &by_release,
        &by_used,
        &ov.active_models.include,
        &ov.active_models.exclude,
        evolve_core::recency::ACTIVE_CAP,
        evolve_core::recency::NEWEST_QUOTA,
        now,
    );
    // 综合排名（供顶替预告与 UI 排序）：max(发布日期, 最近使用)
    let mut ranked: Vec<(String, u64)> = eligible
        .iter()
        .map(|m| {
            (
                m.id.clone(),
                {
                    let used = last_used.get(&m.id).copied();
                    evolve_core::recency::release_ts_of(m)
                        .unwrap_or(0)
                        .max(used.unwrap_or(0))
                },
            )
        })
        .collect();
    ranked.sort_by(|a, b| b.1.cmp(&a.1).then(a.0.cmp(&b.0)));
    (active, ranked)
}

/// POST /api/models/active {model, action: enable|disable|auto}
pub async fn api_models_active(
    State(st): State<AppState>,
    axum::Json(body): axum::Json<serde_json::Value>,
) -> Response {
    let Some(model) = body.get("model").and_then(|v| v.as_str()).map(|s| s.to_string()) else {
        return (StatusCode::BAD_REQUEST, axum::Json(json!({"error": {"message": "model is required"}}))).into_response();
    };
    let action = body.get("action").and_then(|v| v.as_str()).unwrap_or("enable");
    let dir = st.config.data.dir.clone();
    let (before, _) = active_models(&st); // 变更前快照
    let mut ov = crate::state::load_overrides(&dir);
    ov.active_models.include.retain(|m| m != &model);
    ov.active_models.exclude.retain(|m| m != &model);
    match action {
        "enable" => ov.active_models.include.push(model.clone()),
        "disable" => ov.active_models.exclude.push(model.clone()),
        _ => {} // auto：回到默认排序
    }
    crate::state::save_overrides(&dir, &ov);
    let (active, _ranked) = active_models(&st);
    // 真实被顶者 = 变更前有、变更后没有（不是"幸存者里最低分"——见评审 #4）
    let after: std::collections::HashSet<&String> = active.iter().collect();
    let victim = if action == "enable" {
        before.iter().find(|m| !after.contains(m)).cloned()
    } else {
        None
    };
    (axum::Json(json!({"status": "ok", "model": model, "action": action,
        "active_count": active.len(), "replaced": victim}))).into_response()
}

/// 不可用原因（None = 可用）：no-key / deleted / cooldown / quota-gated
/// ——用于标签卡灰显 + tooltip（用户裁决：分位动态排除的模型要看得见原因）
pub fn model_usability(st: &AppState) -> std::collections::HashMap<String, Option<&'static str>> {
    let now = evolve_memory::health::now();
    let health = st.health.snapshot();
    let ov = crate::state::load_overrides(&st.config.data.dir);
    let deleted: std::collections::HashSet<&str> = ov.deleted_models.iter().map(|s| s.as_str()).collect();
    let gated: std::collections::HashSet<String> = st.quota.gate_blocked().into_iter().collect();
    st.engine
        .catalog_snapshot()
        .iter()
        .map(|m| {
            let reason = if !m.has_credential() {
                Some("no-key")
            } else if deleted.contains(m.id.as_str()) {
                Some("deleted")
            } else if gated.contains(&m.provider) {
                Some("quota-gated")
            } else if health.get(&m.id).map(|h| !h.available(now)).unwrap_or(false) {
                Some("cooldown")
            } else {
                None
            };
            (m.id.clone(), reason)
        })
        .collect()
}

/// 目标分布（用户裁决）：能力/价位都要 7:2:1 分布——否则标签没有区分度。
const TAG_Q1: f32 = 0.70;
const TAG_Q2: f32 = 0.90;

/// 分配标签（分位法）：
/// - 能力：**全局分位**（tiers 跨家族可比）→ 70% basic / 20% standard / 10% powerful
/// - 价位：**按口径分桶**各自 70/90 分位（配额系数 / 官方价 / 目录价三种口径不可
///   混排，桶内各自 7:2:1 → 总体 ≈7:2:1）；无任何成本信息 → 中性 medium
pub fn assign_tags(
    snap: &[evolve_core::types::ModelRecord],
) -> std::collections::HashMap<String, (evolve_core::tags::Capability, evolve_core::tags::PriceTier)> {
    let all: std::collections::HashSet<String> = snap.iter().map(|m| m.id.clone()).collect();
    assign_tags_within(snap, &all)
}

/// 分位只在 `usable` 内统计（分母=可用池）；不可用模型用同一档线映射
/// （仍显示标签供参考，但不影响 7:2:1 形状）
pub fn assign_tags_within(
    snap: &[evolve_core::types::ModelRecord],
    usable: &std::collections::HashSet<String>,
) -> std::collections::HashMap<String, (evolve_core::tags::Capability, evolve_core::tags::PriceTier)> {
    use evolve_core::tags::{Capability, PriceTier};
    use std::collections::HashMap;

    // 成本代理三级（2026-10 修正：不依赖 m.plan 标志——用户只标了少数订阅模型，
    // 同家族其余模型 plan=false 会被误判为按量而拿不到配额口径提示）：
    // ① 命中套餐家族（plans::plan_for）→ 配额/系数口径  ② 官方按量价目  ③ 目录价
    //
    // 桶键**细化到口径内不可比的那一维**（否则分位被稀释/整体偏移）：
    //  - plan → 按 **plan_key 分桶**：智谱"积分系数"与 opencode-go"美元系数"量纲
    //    不同，混在一个桶里会把积分口径的大数值整体推到 premium（实测 zhipu 全族
    //    premium，进而被 `difficulty≤0.8 → 只允许 tier 0` 的硬闸全族挡死）
    //  - official / catalog → 按**币种**分桶（CNY 与 USD 不能直接比大小）
    let cost_proxy = |m: &evolve_core::types::ModelRecord| -> Option<(f32, String)> {
        if evolve_core::plans::plan_for(&m.base_url).is_some()
            && let Some(h) = evolve_core::plans::model_price_hint(&m.base_url, &m.id)
        {
            let pk = evolve_core::plans::plan_key_for(&m.base_url).unwrap_or("plan");
            return Some((h, format!("plan:{pk}")));
        }
        if let Some(sc) = evolve_core::provider_info::price_for(&m.base_url, &m.id) {
            let in_eff = sc.in_miss * 0.5 + sc.in_hit * 0.5;
            return Some((in_eff + sc.out * 0.3, format!("official:{}", m.currency)));
        }
        m.cost.map(|c| (c.input + c.output * 0.3, format!("catalog:{}", m.currency)))
    };

    // 分位基准：**可用池**（<8 个时退回全量兜底）
    let pool: Vec<&evolve_core::types::ModelRecord> =
        snap.iter().filter(|m| usable.contains(&m.id)).collect();
    let basis: Vec<&evolve_core::types::ModelRecord> =
        if pool.len() >= 8 { pool.clone() } else { snap.iter().collect() };

    // 1) 能力：在**可用池**上取分位阈值（值分档；不可用模型按同一阈值映射，
    //    修回归：此前用 position 在 basis 里找 index，不可用模型查不到 → rank 0
    //    → 全部落 basic（实测 87% basic））
    let cap_score = |m: &evolve_core::types::ModelRecord| -> f32 {
        m.tiers.coding.max(m.tiers.reasoning).max(m.tiers.agentic).max(m.tiers.vision * 0.9)
    };
    let mut cap_vals: Vec<f32> = basis.iter().map(|m| cap_score(m)).collect();
    cap_vals.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
    let qval = |v: &[f32], q: f32| -> f32 {
        if v.is_empty() { return 0.0; }
        v[(((v.len() - 1) as f32) * q).round() as usize]
    };
    let (cap_t1, cap_t2) = (qval(&cap_vals, TAG_Q1), qval(&cap_vals, TAG_Q2));
    let tier_by_rank = |rank: f32| -> PriceTier {
        if rank < TAG_Q1 { PriceTier::Cheap } else if rank < TAG_Q2 { PriceTier::Medium } else { PriceTier::Premium }
    };
    // 混合分档：**可用池内用分位秩**（并列值也能精确 70/20/10），
    // 池外（冷却/闸门/无凭据）用阈值映射（避免此前"查不到位置 → rank 0 → 全 basic"）。
    // ⚠ rank 必须建立在**按能力升序排序后的**列表上（与价位桶内 rank 同法）——
    // basis 是目录序，直接取 position 会让标签随目录位置漂移（强模型落 basic）。
    let mut cap_ranked: Vec<&evolve_core::types::ModelRecord> = basis.clone();
    cap_ranked.sort_by(|a, b| {
        cap_score(a).partial_cmp(&cap_score(b)).unwrap_or(std::cmp::Ordering::Equal)
    });
    let cap_of = |m: &evolve_core::types::ModelRecord| -> Capability {
        let by_rank = cap_ranked.iter().position(|x| x.id == m.id).map(|idx| {
            let rank = idx as f32 / (cap_ranked.len().max(2) - 1) as f32;
            if rank < TAG_Q1 { Capability::Basic } else if rank < TAG_Q2 { Capability::Standard } else { Capability::Powerful }
        });
        by_rank.unwrap_or_else(|| {
            let t = cap_score(m);
            if t <= cap_t1 { Capability::Basic }
            else if t <= cap_t2 { Capability::Standard }
            else { Capability::Powerful }
        })
    };

    // 2) 价位：按口径（+plan_key/币种）分桶，每桶取 70/90 分位**阈值**（同样用值分档）
    let mut groups: HashMap<String, Vec<(String, f32)>> = HashMap::new();
    for m in basis.iter() {
        if let Some((c, src)) = cost_proxy(m) {
            groups.entry(src).or_default().push((m.id.clone(), c));
        }
    }
    let mut t1_of: HashMap<String, f32> = HashMap::new();
    let mut t2_of: HashMap<String, f32> = HashMap::new();
    for (src, list) in groups.iter_mut() {
        list.sort_by(|a, b| a.1.partial_cmp(&b.1).unwrap_or(std::cmp::Ordering::Equal));
        let vals: Vec<f32> = list.iter().map(|(_, c)| *c).collect();
        t1_of.insert(src.clone(), qval(&vals, TAG_Q1));
        t2_of.insert(src.clone(), qval(&vals, TAG_Q2));
    }
    let price_of = |m: &evolve_core::types::ModelRecord| -> PriceTier {
        let Some((c, src)) = cost_proxy(m) else { return PriceTier::Medium }; // 无成本信息：中性档
        // 可用池内：桶内分位秩（精确 7:2:1）；池外：桶内阈值映射
        if let Some(list) = groups.get(&src)
            && let Some(idx) = list.iter().position(|(i, _)| i == &m.id)
        {
            return tier_by_rank(idx as f32 / (list.len().max(2) - 1) as f32);
        }
        let t1 = t1_of.get(&src).copied().unwrap_or(f32::MAX);
        let t2 = t2_of.get(&src).copied().unwrap_or(f32::MAX);
        if c <= t1 { PriceTier::Cheap } else if c <= t2 { PriceTier::Medium } else { PriceTier::Premium }
    };

    let mut out = HashMap::new();
    for m in snap {
        out.insert(m.id.clone(), (cap_of(m), price_of(m)));
    }
    out
}

/// 标签分配缓存（分位分配需 O(n log n)，按目录规模 + 60s TTL 缓存）
#[allow(clippy::type_complexity)]
static TAG_CACHE: std::sync::LazyLock<
    std::sync::Mutex<
        Option<(
            usize,
            std::time::Instant,
            (usize, std::collections::HashMap<String, (evolve_core::tags::Capability, evolve_core::tags::PriceTier)>),
        )>,
    >,
> = std::sync::LazyLock::new(|| std::sync::Mutex::new(None));

fn tags_for_catalog(
    st: &AppState,
    snap: &[evolve_core::types::ModelRecord],
) -> std::collections::HashMap<String, (evolve_core::tags::Capability, evolve_core::tags::PriceTier)> {
    let usable = usable_model_ids(st);
    if let Ok(g) = TAG_CACHE.lock()
        && let Some((len, at, m)) = g.as_ref()
        && *len == snap.len()
        && at.elapsed().as_secs() < 30
        && m.0 == usable.len()
    {
        return m.1.clone();
    }
    let assigned = assign_tags_within(snap, &usable);
    if let Ok(mut g) = TAG_CACHE.lock() {
        *g = Some((snap.len(), std::time::Instant::now(), (usable.len(), assigned.clone())));
    }
    assigned
}


/// S2：model_id → 价位层编码（0=cheap / 1=medium / 2=premium；用户覆盖优先）
/// ——relay/anthropic 构建 RoutingInput 时传入引擎的价格层硬闸
pub fn price_tier_map(st: &AppState) -> std::collections::HashMap<String, u8> {
    let snap = st.engine.catalog_snapshot();
    let assigned = tags_for_catalog(&st, &snap);
    let ov = crate::state::load_overrides(&st.config.data.dir);
    let mut out = std::collections::HashMap::new();
    for m in &snap {
        let auto_tier = assigned
            .get(&m.id)
            .map(|(_, p)| p.label().to_string())
            .unwrap_or_else(|| "medium".to_string());
        let tier = ov.model_tags.get(&m.id)
            .and_then(|u| u.price_tier.clone())
            .map(|v| norm_price_tier(&v))
            .unwrap_or(auto_tier);
        let code = match tier.as_str() { "cheap" => 0u8, "premium" => 2, _ => 1 };
        out.insert(m.id.clone(), code);
    }
    out
}

/// /api/report?limit=N：S7 回测验收报告（JSON）
pub async fn api_report(State(st): State<AppState>, axum::extract::Query(q): axum::extract::Query<std::collections::HashMap<String, String>>) -> Response {
    let limit = q.get("limit").and_then(|v| v.parse::<usize>().ok()).unwrap_or(500).min(5000);
    (axum::Json(distribution_report(&st, limit))).into_response()
}

/// S7 回测验收：对历史 events 做端到端分布报告（难度/能力/价位 + 缓存/成本 KPI
/// + 目标 7:2:1 判定）。CLI `evo-router report` 与 API `/api/report` 共用。
pub fn distribution_report(st: &AppState, limit: usize) -> Value {
    let Some(path) = st.events.path() else { return json!({"error": "events disabled"}) };
    let Ok(content) = std::fs::read_to_string(path) else { return json!({"error": "events unreadable"}) };
    let snap = st.engine.catalog_snapshot();
    let assigned = tags_for_catalog(st, &snap);
    let usability = model_usability(st);
    let ov = crate::state::load_overrides(&st.config.data.dir);
    let dsnap = st.engine.difficulty_snapshot();
    let rsnap = st.engine.ratio_snapshot();
    let by_id: std::collections::HashMap<&str, &evolve_core::types::ModelRecord> =
        snap.iter().map(|m| (m.id.as_str(), m)).collect();

    let mut diff = [0u32; 3];
    let mut cap = [0u32; 3];
    let mut price = [0u32; 3];
    let mut count = 0usize;
    let mut cached_tok = 0u64;
    let mut in_tok = 0u64;

    let mut cost_sum = 0f64;
    let mut cost_n = 0usize;
    let mut premium_on_easy = 0u32; // L1 却用了 premium（应≈0）
    let mut l3 = 0u32;
    let mut l3_premium = 0u32;
    for line in content.lines().rev() {
        if count >= limit { break; }
        let Ok(e) = serde_json::from_str::<Value>(line) else { continue };
        if e.get("kind").and_then(|v| v.as_str()) != Some("chat") { continue; }
        let Some(chosen) = e.get("chosen").and_then(|v| v.as_str()) else { continue };
        count += 1;
        let d = e.pointer("/judgment/difficulty").and_then(|v| v.as_f64()).unwrap_or(0.0) as f32;
        let dbin = if d <= 0.8 { 0 } else if d <= 2.0 { 1 } else { 2 };
        diff[dbin] += 1;
        if dbin == 2 { l3 += 1; }
        let (c, p) = match by_id.get(chosen) {
            Some(m) => {
                let (ac, ap) = assigned
                    .get(&m.id)
                    .map(|(c, p)| (c.label().to_string(), p.label().to_string()))
                    .unwrap_or_else(|| ("standard".into(), "medium".into()));
                let u = ov.model_tags.get(&m.id);
                (
                    u.and_then(|u| u.capability.clone()).unwrap_or(ac),
                    u.and_then(|u| u.price_tier.clone()).map(|v| norm_price_tier(&v)).unwrap_or(ap),
                )
            }
            None => ("standard".into(), "medium".into()),
        };
        let cbin = match c.as_str() { "basic" => 0, "powerful" => 2, _ => 1 };
        let pbin = match p.as_str() { "cheap" => 0, "premium" => 2, _ => 1 };
        cap[cbin] += 1;
        price[pbin] += 1;
        if dbin == 0 && pbin == 2 { premium_on_easy += 1; }
        if dbin == 2 && pbin == 2 { l3_premium += 1; }
        // token/缓存/成本（事件里带 usage 平铺字段）
        let it = e.get("prompt_tokens").and_then(|v| v.as_u64()).unwrap_or(0);
        let ct = e.get("cached_tokens").and_then(|v| v.as_u64()).unwrap_or(0);
        let ot = e.get("completion_tokens").and_then(|v| v.as_u64()).unwrap_or(0);
        in_tok += it; cached_tok += ct;
        if let Some(m) = by_id.get(chosen)
            && let Some(cst) = evolve_core::scoring::est_cost(m, it, ot.max(1), None)
        {
            cost_sum += cst as f64;
            cost_n += 1;
        }
    }
    let share = |v: &[u32; 3]| -> [f32; 3] {
        let n = v.iter().sum::<u32>().max(1) as f32;
        [v[0] as f32 / n, v[1] as f32 / n, v[2] as f32 / n]
    };
    let target = [0.7f32, 0.2, 0.1];
    let band = 0.10f32; // 报告用宽松带（历史样本含旧判定，逐步收敛）
    let verdict = |s: [f32; 3]| -> bool {
        (0..3).all(|i| (s[i] - target[i]).abs() <= band)
    };
    let ds = share(&diff);
    let cs = share(&cap);
    let ps = share(&price);
    let hit_rate = if in_tok > 0 { cached_tok as f64 / in_tok as f64 } else { 0.0 };
    json!({
        "count": count,
        "target": {"l1": target[0], "l2": target[1], "l3": target[2], "band": band},
        "difficulty": {"l1": ds[0], "l2": ds[1], "l3": ds[2], "pass": verdict(ds)},
        "capability": {"basic": cs[0], "standard": cs[1], "powerful": cs[2], "pass": verdict(cs)},
        "price_tier": {"cheap": ps[0], "medium": ps[1], "premium": ps[2], "pass": verdict(ps)},
        "kpi": {
            "premium_on_easy": premium_on_easy,
            "l3_share": if count > 0 { l3 as f64 / count as f64 } else { 0.0 },
            "l3_premium": l3_premium,
            "cache_hit_rate": hit_rate,
            "avg_cost_proxy": if cost_n > 0 { cost_sum / cost_n as f64 } else { 0.0 },
            "costed_requests": cost_n,
        },
        "unusable_models": usability.values().filter(|r| r.is_some()).count(),
        "live": {
            "difficulty": {
                "samples": dsnap.0.samples,
                "raw": {"l1": dsnap.0.raw_l1, "l2": dsnap.0.raw_l2, "l3": dsnap.0.raw_l3},
                "calibrated": {"l1": dsnap.0.cal_l1, "l2": dsnap.0.cal_l2, "l3": dsnap.0.cal_l3},
                "target": {"l1": dsnap.1[0], "l2": dsnap.1[1], "l3": dsnap.1[2]},
            },
            "ratio": {
                "samples": rsnap.0.samples,
                "observed": {"cheap": rsnap.0.cheap, "medium": rsnap.0.medium, "premium": rsnap.0.premium},
                "target": {"cheap": rsnap.1[0], "medium": rsnap.1[1], "premium": rsnap.1[2]},
                "penalties": {"cheap": rsnap.0.penalties[0], "medium": rsnap.0.penalties[1], "premium": rsnap.0.penalties[2]},
            },
        },
    })
}

/// 近 N 条 chat 请求的分布总览（用户裁决：过去 100 个请求的难度/能力/价位分布）
pub fn recent_distribution(st: &AppState, n: usize) -> Value {
    let Some(path) = st.events.path() else { return json!({}) };
    let Ok(content) = std::fs::read_to_string(path) else { return json!({}) };
    let snap = st.engine.catalog_snapshot();
    let assigned = tags_for_catalog(&st, &snap);
    let by_id: std::collections::HashMap<&str, &evolve_core::types::ModelRecord> =
        snap.iter().map(|m| (m.id.as_str(), m)).collect();
    let ov = crate::state::load_overrides(&st.config.data.dir);

    let mut diff = [0u32; 3];
    let mut cap = [0u32; 3];   // basic/standard/powerful
    let mut price = [0u32; 3]; // cheap/standard/premium
    let mut count = 0usize;
    for line in content.lines().rev() {
        if count >= n { break; }
        let Ok(e) = serde_json::from_str::<Value>(line) else { continue };
        if e.get("kind").and_then(|v| v.as_str()) != Some("chat") { continue; }
        let Some(chosen) = e.get("chosen").and_then(|v| v.as_str()) else { continue };
        count += 1;
        // 难度（judgment.difficulty 为校准后的实际使用值）
        let d = e.pointer("/judgment/difficulty").and_then(|v| v.as_f64()).unwrap_or(0.0) as f32;
        if d <= 0.8 { diff[0] += 1 } else if d <= 2.0 { diff[1] += 1 } else { diff[2] += 1 }
        // 能力/价位：用户覆盖优先，否则自动推导
        let (c, p) = match by_id.get(chosen) {
            Some(m) => {
                let (ac, ap) = assigned
                    .get(&m.id)
                    .map(|(c, p)| (c.label().to_string(), p.label().to_string()))
                    .unwrap_or_else(|| ("standard".into(), "medium".into()));
                let u = ov.model_tags.get(&m.id);
                (
                    u.and_then(|u| u.capability.clone()).unwrap_or(ac),
                    u.and_then(|u| u.price_tier.clone()).map(|v| norm_price_tier(&v)).unwrap_or(ap),
                )
            }
            None => ("standard".into(), "standard".into()),
        };
        match c.as_str() { "basic" => cap[0] += 1, "powerful" => cap[2] += 1, _ => cap[1] += 1 }
        let p = norm_price_tier(&p);
        match p.as_str() { "cheap" => price[0] += 1, "premium" => price[2] += 1, _ => price[1] += 1 }
    }
    json!({
        "window": n, "count": count,
        "difficulty": {"l1": diff[0], "l2": diff[1], "l3": diff[2]},
        "capability": {"basic": cap[0], "standard": cap[1], "powerful": cap[2]},
        "price_tier": {"cheap": price[0], "medium": price[1], "premium": price[2]},
    })
}

/// 新增/修改 provider（手动定义）：baseUrl 变更触发 /models 重拉
pub async fn api_providers_set(
    State(st): State<AppState>,
    axum::Json(body): axum::Json<serde_json::Value>,
) -> Response {
    let Some(key) = body.get("key").and_then(|k| k.as_str()).map(|s| s.to_string()) else {
        return (StatusCode::BAD_REQUEST, axum::Json(json!({"error": {"message": "key is required"}}))).into_response();
    };
    let text = |k: &str| body.get(k).and_then(|v| v.as_str()).map(|s| s.trim().to_string()).filter(|s| !s.is_empty());
    let num = |k: &str| body.get(k).and_then(|v| v.as_f64());
    let dir = &st.config.data.dir;
    let mut ov = crate::state::load_overrides(dir);
    let mut base_url_changed = false;
    {
        let entry = ov.providers.entry(key.clone()).or_default();
        if let Some(u) = text("base_url") {
            base_url_changed = entry.base_url != u;
            entry.base_url = u;
        }
        if let Some(k) = text("api_key") {
            entry.api_key = Some(k);
            entry.key_fp = None; // 显式 key 即身份，指纹失效
        }
        // 未填 key 的编辑（发现 provider 的 overlay）：回填该组现有 key 指纹，
        // 保持 apiKey+baseUrl 身份可判定（否则同端点不同账号会被误遮蔽/误判重）
        if entry.api_key.is_none() && entry.key_fp.is_none() {
            if let Some(k) = st.engine.catalog_snapshot().iter()
                .find(|m| m.provider == key)
                .and_then(|m| m.key_values().first().cloned())
            {
                use std::hash::{Hash, Hasher};
                let mut h = std::collections::hash_map::DefaultHasher::new();
                k.hash(&mut h);
                entry.key_fp = Some(format!("{:016x}", h.finish()));
            }
        }
        if let Some(v) = text("protocol") { entry.protocol = v; }
        if let Some(v) = text("plan_kind") { entry.plan_kind = v; }
        if let Some(v) = text("docs_url") { entry.docs_url = v; }
        if let Some(v) = text("tier") { entry.tier = v; }
        if let Some(v) = text("model_rates") { entry.model_rates = v; }
        if let Some(v) = num("window_5h") { entry.window_5h = Some(v); }
        if let Some(v) = num("window_weekly") { entry.window_weekly = Some(v); }
        if let Some(v) = num("window_monthly") { entry.window_monthly = Some(v); }
        if let Some(v) = body.get("disabled").and_then(|v| v.as_bool()) { entry.disabled = v; }
    }
    crate::state::save_overrides(dir, &ov);
    // baseUrl 新增/变更 → 立即拉取 /models
    let mut fetched = 0;
    let needs_fetch = base_url_changed
        || ov.providers.get(&key).map(|e| e.fetched_models.is_empty()).unwrap_or(true);
    if needs_fetch {
        let (base, api_key) = {
            let e = ov.providers.get(&key);
            (
                format!("{}{}", e.map(|e| e.base_url.trim_end_matches('/')).unwrap_or(""), "/models"),
                e.and_then(|e| e.api_key.clone()),
            )
        };
        let mut req = st.http.get(&base);
        if let Some(k) = &api_key { req = req.bearer_auth(k); }
        if let Ok(resp) = req.timeout(std::time::Duration::from_secs(5)).send().await
            && let Ok(v) = resp.json::<Value>().await
        {
                let ids: Vec<String> = v.get("data")
                    .and_then(|d| d.as_array())
                    .map(|arr| arr.iter()
                        .filter_map(|m| m.get("id").and_then(|i| i.as_str()).map(|s| s.to_string()))
                        .collect())
                    .unwrap_or_default();
                fetched = ids.len();
                if let Some(e) = ov.providers.get_mut(&key) { e.fetched_models = ids; }
                crate::state::save_overrides(dir, &ov);
        }
    }
    (axum::Json(json!({"status": "ok", "key": key, "fetched_models": fetched}))).into_response()
}


/// MiniMax M Plan 官方 remains 端点拉取 → 写 quota observed（5h/周，百分比口径）。
/// 由 reconcile 手动触发 + api_providers 轮询时过期自动刷新（60s 节流）。

/// 按量 provider 余额查询（deepseek 官方 /user/balance；Bearer key）。
/// 返回 {currency, total, is_available} 或 None。
async fn deepseek_balance(
    st: &AppState, key: &str, api_key: Option<String>,
) -> Option<Value> {
    let key_resolved = api_key.or_else(|| {
        st.engine.catalog_snapshot().iter()
            .find(|m| m.provider == key)
            .and_then(|m| m.key_values().first().cloned())
    })?;
    let mut req = st.http.get("https://api.deepseek.com/user/balance")
        .timeout(std::time::Duration::from_secs(8));
    req = req.bearer_auth(&key_resolved);
    let j = req.send().await.ok()?.json::<serde_json::Value>().await.ok()?;
    let avail = j["is_available"].as_bool()?;
    let infos = j["balance_infos"].as_array()?;
    let first = infos.first()?;
    Some(json!({
        "is_available": avail,
        "currency": first["currency"].as_str().unwrap_or(""),
        "total": first["total_balance"].as_str().and_then(|s| s.parse::<f64>().ok()).unwrap_or(0.0),
    }))
}

/// w2 配额看门狗：独立于面板轮询的官方端点定时刷新（90s 一轮，observed
/// 未过期则跳过）——面板不开也在刷新；刷新后把官方窗口结论写进 gate 状态机
pub fn spawn_quota_watchdog(st: AppState) {
    tokio::spawn(async move {
        let mut tick = tokio::time::interval(std::time::Duration::from_secs(90));
        tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        loop {
            tick.tick().await;
            quota_refresh_tick(st.clone(), 90 * 1000).await;
        }
    });
}

/// 一轮官方配额刷新：observed 过期（>max_age）的积分制 provider 逐一拉取；
/// 与 api_providers 面板轮询共用（面板用 60s 新鲜度）
pub(crate) async fn quota_refresh_tick(st: AppState, max_age_ms: u64) {
    // 顺带预热/保活按量余额（deepseek）：watchdog 常驻 → 面板不开也在刷，
    // 且启动首跳即填缓存（消除"重启后余额↔payg 闪烁"）
    refresh_payg_balances_if_stale(&st);
    let now0 = evolve_memory::health::now();
    let mut due: Vec<(String, String, String, Option<String>)> = Vec::new();
    // w3 按账号去重（provider 名 = 账号）：同 baseURL 多 key 各自刷新自己的
    // observed 槽位（用户裁决：新加的同 host provider 也必须知道可查配额）
    let mut seen_account: Vec<String> = Vec::new();
    let mut push_due = |k: &str, bu: &str, ak: Option<String>| {
        if let Some(api_kind) = evolve_core::plans::quota_api_for(bu) {
            if api_kind.window_based()
                && !seen_account.iter().any(|x| x == k)
            {
                let fresh = st.quota.observed_window(k, "5h", now0, max_age_ms).is_some();
                if fresh && st.quota.gate_get(k).is_none() {
                    return; // 数据新鲜且无闸门 → 无需刷新
                }
                seen_account.push(k.to_string());
                let pk = evolve_core::plans::plan_key_for(bu).unwrap_or_default().to_string();
                due.push((k.to_string(), bu.to_string(), pk, ak));
            }
        }
    };
    let ov = crate::state::load_overrides(&st.config.data.dir);
    for (k, p) in ov.providers.iter().filter(|(_, p)| !p.disabled) {
        push_due(k, &p.base_url, p.api_key.clone());
    }
    for m in st.engine.catalog_snapshot() {
        push_due(&m.provider, &m.base_url, m.key_values().first().cloned());
    }
    for (k, bu, pk, ak) in due {
        let was = st.quota.gate_get(&k).map(|g| !g.windows_full.is_empty()).unwrap_or(false);
        let _ = refresh_observed(&st, &k, &bu, &pk, ak, now0).await;
        let now = st.quota.gate_get(&k).map(|g| !g.windows_full.is_empty()).unwrap_or(false);
        if was != now {
            st.events.record(json!({
                "kind": "quota_state_changed",
                "provider": k, "state": if now { "quota_limited" } else { "ok" },
                "source": "api_watchdog",
            }));
        }
        // API 数据反证配额可用（开闸）→ 清该 provider 模型的配额类健康冷却
        if was && !now {
            let ids: Vec<String> = st.engine.catalog_snapshot().iter()
                .filter(|m| m.provider == k).map(|m| m.id.clone()).collect();
            st.health.clear_kind(&ids, evolve_core::types::HealthKind::QuotaExhausted);
        }
    }
    // w3 无 API 型周期探活（用户裁决：无官方配额端点 → 信任上游给的窗口期做冷却，
    // 但不干等——每 5~10min 独立发一次真实微请求验证；成功即开闸清冷却）
    let probe_now = evolve_memory::quota::now_ms();
    if let Some(provider) = st.quota.gate_pick_probe(probe_now) {
        // 探活目标 = 该 provider 当前处于配额冷却的模型（而不是随便挑首个模型——
        // 首模型可能本就没受限，2xx 会误清整账号冷却，造成面板"限制/可用"翻转）
        let cooling: Vec<String> = st.health.snapshot().iter()
            .filter(|(id, e)| {
                id.starts_with(&format!("{provider}/"))
                    && e.kind == evolve_core::types::HealthKind::QuotaExhausted
            })
            .map(|(id, _)| id.split('\u{1f}').next().unwrap_or(id).to_string())
            .collect();
        let target = cooling.first().cloned();
        let ok = probe_provider_model(&st, &provider, target.as_deref()).await;
        if ok {
            // 仅清被探活成功的那一个模型；该 provider 若仍有其它模型在配额冷却，
            // gate 保持关闭（按模型恢复，与失败信号粒度一致）
            if let Some(t) = &target {
                st.health.clear_kind(std::slice::from_ref(t), evolve_core::types::HealthKind::QuotaExhausted);
            }
            let still_cooling = st.health.snapshot().iter().any(|(id, e)| {
                id.starts_with(&format!("{provider}/"))
                    && e.kind == evolve_core::types::HealthKind::QuotaExhausted
            });
            if still_cooling {
                st.quota.gate_probe_fail(&provider, probe_now); // 保持关闭 + 退避重排
            } else {
                st.quota.gate_probe_success(&provider, probe_now);
                st.events.record(json!({
                    "kind": "quota_state_changed",
                    "provider": provider, "state": "ok", "source": "probe",
                }));
                tracing::info!(provider = %provider, "quota probe succeeded — gate opened");
            }
        } else {
            st.quota.gate_probe_fail(&provider, probe_now);
        }
    }
}

/// 单次探针请求：优先打指定（冷却中）模型，未指定则取该 provider 首模型；
/// max_tokens=1 的极小 completion，只看是否被上游接受（2xx）
async fn probe_provider_model(st: &AppState, provider: &str, model_id: Option<&str>) -> bool {
    let snap = st.engine.catalog_snapshot();
    let picked = match model_id {
        Some(mid) => snap.iter().find(|m| m.id == mid).or_else(|| snap.iter().find(|m| m.provider == provider)),
        None => snap.iter().find(|m| m.provider == provider),
    };
    let Some(m) = picked else { return false };
    let base = m.base_url.trim_end_matches('/').to_string();
    let key = m.key_values().first().cloned();
    let upstream = if m.upstream_model.is_empty() { m.id.split('/').last().unwrap_or("").to_string() } else { m.upstream_model.clone() };
    let body = json!({
        "model": upstream,
        "messages": [{"role": "user", "content": "ping"}],
        "max_tokens": 1,
        "stream": false,
    });
    let url = format!("{base}/chat/completions");
    let mut req = st.http.post(&url)
        .header("content-type", "application/json")
        .timeout(std::time::Duration::from_secs(10));
    if let Some(k) = key { req = req.bearer_auth(k); }
    match req.json(&body).send().await {
        Ok(resp) => {
            let status = resp.status();
            tracing::info!(provider = %provider, model = %upstream, status = status.as_u16(), "quota probe attempt");
            status.is_success()
        }
        Err(e) => {
            tracing::warn!(provider = %provider, error = %e, "quota probe transport error");
            false
        }
    }
}

pub(crate) async fn refresh_observed(
    st: &AppState, key: &str, base_url: &str, _plan_key: &str, api_key: Option<String>, now_ms: u64,
) -> Option<serde_json::Map<String, Value>> {
    // w3 分派走 hostname 注册表（quota_api_for）——新增同 host 账号（不同 key）
    // 自动命中同一刷新路径，无需在计划表里另立条目
    let api_kind = evolve_core::plans::quota_api_for(base_url)?;
    let host_is_zai = base_url.contains("z.ai");
    let origin = if host_is_zai {
        "https://api.z.ai".to_string()
    } else {
        base_url.split("://").nth(1).and_then(|r| r.split('/').next())
            .map(|h| h.split(':').next().unwrap_or(h))
            .map(|h| format!("https://{h}"))
            .unwrap_or_default()
    };
    let url = match api_kind {
        evolve_core::plans::QuotaApi::ZhipuMonitor | evolve_core::plans::QuotaApi::ZaiMonitor =>
            format!("{origin}/api/monitor/usage/quota/limit?type=1"),
        evolve_core::plans::QuotaApi::OpencodeZen => format!("{origin}/zen/go/v1/usage"),
        evolve_core::plans::QuotaApi::MinimaxRemains => format!("{origin}/v1/token_plan/remains"),
        evolve_core::plans::QuotaApi::DeepseekBalance => return None, // 余额走 deepseek_balance
    };
    let zhipuish = matches!(
        api_kind,
        evolve_core::plans::QuotaApi::ZhipuMonitor | evolve_core::plans::QuotaApi::ZaiMonitor
    );
    let ocgo = api_kind == evolve_core::plans::QuotaApi::OpencodeZen;
    let key_resolved = api_key.or_else(|| {
        st.engine.catalog_snapshot().iter()
            .find(|m| m.provider == key)
            .and_then(|m| m.key_values().first().cloned())
    })?;
    let mut req = st.http.get(&url).timeout(std::time::Duration::from_secs(8));
    req = req.bearer_auth(&key_resolved);
    let j = req.send().await.ok()?.json::<serde_json::Value>().await.ok()?;
    let mk = |scope: &str, pct: u64, reset: Option<u64>| evolve_memory::WindowState {
        scope: scope.to_string(), remaining: Some(100 - pct.min(100)), limit: Some(100),
        reset_epoch_ms: reset, updated_epoch_ms: now_ms,
    };
    let windows = if ocgo {
        // usage.rolling(5h)/weekly/monthly: percent + resetsAt（ISO8601）
        let parse_iso = |v: &Value| v.as_str().and_then(|s| evolve_memory::quota::parse_rfc3339_ms(s));
        let u = &j["usage"];
        let mut w = vec![
            mk("5h", u["rolling"]["percent"].as_u64().unwrap_or(0), parse_iso(&u["rolling"]["resetsAt"])),
            mk("weekly", u["weekly"]["percent"].as_u64().unwrap_or(0), parse_iso(&u["weekly"]["resetsAt"])),
        ];
        if u["monthly"]["percent"].as_u64().is_some() {
            w.push(mk("monthly", u["monthly"]["percent"].as_u64().unwrap_or(0), parse_iso(&u["monthly"]["resetsAt"])));
        }
        w
    } else if zhipuish {
        // data.limits[]: unit=3 → 5h（动态刷新，无固定重置时刻）；unit=6 → 周（nextResetTime）
        let limits = j["data"]["limits"].as_array()?;
        let pick = |unit: u64| limits.iter().find(|e| e["unit"].as_u64() == Some(unit)).cloned();
        let h5 = pick(3)?;
        let wk = pick(6)?;
        vec![
            mk("5h", h5["percentage"].as_u64().unwrap_or(0), None),
            mk("weekly", wk["percentage"].as_u64().unwrap_or(0), wk["nextResetTime"].as_u64()),
        ]
    } else {
        let general = j["model_remains"].as_array().and_then(|arr| {
            arr.iter().find(|e| e["model_name"] == "general").or_else(|| arr.first())
        })?.clone();
        vec![
            mk("5h", general["current_interval_remaining_percent"].as_u64().unwrap_or(0),
               general["end_time"].as_u64()),
            mk("weekly", general["current_weekly_remaining_percent"].as_u64().unwrap_or(0),
               general["weekly_end_time"].as_u64()),
        ]
    };
    st.quota.observe_provider(key, &windows);
    // w2 闸门状态机：官方窗口结论直写 gate（恢复路径的数据源；账号级键控）
    st.quota.gate_observe_windows(key, &windows, now_ms);
    let mut snapshot: serde_json::Map<String, Value> = windows.iter().map(|w| {
        (w.scope.clone(), json!({
            "remaining": w.remaining, "limit": w.limit, "reset_epoch_ms": w.reset_epoch_ms,
        }))
    }).collect();
    let exhausted_5h = windows.iter().find(|w| w.scope == "5h")
        .and_then(|w| w.remaining).map(|r| r == 0).unwrap_or(false);
    snapshot.insert("exhausted_5h".into(), json!(exhausted_5h));
    Some(snapshot)
}

/// 手动对账（用户裁决：以 Provider 测为准）：对一个 provider 的目录首模型发一个
/// 极小 completion 请求捕获响应头，把 provider 的真实配额/限额写入 observed
pub async fn api_providers_reconcile(
    State(st): State<AppState>,
    axum::Json(body): axum::Json<serde_json::Value>,
) -> Response {
    let Some(key) = body.get("key").and_then(|k| k.as_str()).map(|s| s.to_string()) else {
        return (StatusCode::BAD_REQUEST, axum::Json(json!({"error": {"message": "key is required"}}))).into_response();
    };
    let now_ms = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH).map(|d| d.as_millis() as u64).unwrap_or(0);
    let ov = crate::state::load_overrides(&st.config.data.dir);
    let (base_url, api_key) = if let Some(pd) = ov.providers.get(&key) {
        (pd.base_url.clone(), pd.api_key.clone())
    } else {
        // discovered：按 provider 名从目录找一个 base_url + 第一个 key
        let snap = st.engine.catalog_snapshot();
        let pick = snap.iter().find(|m| m.provider == key);
        match pick {
            Some(m) => (m.base_url.clone(), m.key_values().first().cloned()),
            None => return (StatusCode::NOT_FOUND, axum::Json(json!({"error": {"message": "provider not found"}}))).into_response(),
        }
    };
    let probe_body = json!({
        "model": key,
        "messages": [{"role":"user","content":"hi"}],
        "max_tokens": 1,
        "stream": false,
    });
    let url = format!("{}/chat/completions", base_url.trim_end_matches('/'));
    let mut req = st.http.post(&url)
        .header("content-type", "application/json")
        .timeout(std::time::Duration::from_secs(8));
    if let Some(k) = &api_key { req = req.bearer_auth(k); }
    let resp = match req.json(&probe_body).send().await {
        Ok(r) => r,
        Err(e) => return (StatusCode::BAD_GATEWAY,
            axum::Json(json!({"error": {"message": format!("transport: {e}")}}))).into_response(),
    };
    if let Some(pk) = evolve_core::plans::plan_key_for(&base_url) {
        // MiniMax M Plan：官方 remains 端点（零 token 成本），优先于探针请求
        if pk == "minimax-coding" {
            if let Some(snapshot) = refresh_observed(&st, &key, &base_url, &pk, api_key.clone(), now_ms).await {
                return (axum::Json(json!({"status":"ok","plan_key":pk,"windows":snapshot}))).into_response();
            }
        }
        let windows = evolve_memory::QuotaLedger::parse_headers(resp.headers());
        st.quota.observe_provider(&key, &windows);
        let snapshot: serde_json::Map<String, Value> = windows.iter().map(|w| {
            (w.scope.clone(), json!({
                "remaining": w.remaining, "limit": w.limit, "reset_epoch_ms": w.reset_epoch_ms,
            }))
        }).collect();
        return (axum::Json(json!({"status":"ok","plan_key":pk,"windows":snapshot}))).into_response();
    }
    (axum::Json(json!({"status":"ok","plan_key":Value::Null}))).into_response()
}

/// 删除 provider（软删除——disabled=true，面板可恢复）
pub async fn api_providers_delete(
    State(st): State<AppState>,
    axum::Json(body): axum::Json<serde_json::Value>,
) -> Response {
    let Some(key) = body.get("key").and_then(|k| k.as_str()).map(|s| s.to_string()) else {
        return (StatusCode::BAD_REQUEST, axum::Json(json!({"error": {"message": "key is required"}}))).into_response();
    };
    let dir = &st.config.data.dir;
    let mut ov = crate::state::load_overrides(dir);
    // 手动 provider：软删除（disabled，面板可恢复）
    if let Some(e) = ov.providers.get_mut(&key) {
        e.disabled = true;
        crate::state::save_overrides(dir, &ov);
        (axum::Json(json!({"status": "ok", "key": key, "disabled": true}))).into_response()
    } else if let Some(base_url) = body.get("base_url").and_then(|v| v.as_str()).filter(|s| !s.is_empty()) {
        // 发现 provider：按 base_url 持久化删除（定期扫描/重启不再加回）+ 热更新目录
        let base_url = base_url.to_string();
        if !ov.removed_base_urls.contains(&base_url) {
            ov.removed_base_urls.push(base_url.clone());
            crate::state::save_overrides(dir, &ov);
        }
        let models = st.engine.catalog_snapshot();
        let removed = models.iter().filter(|m| m.base_url == base_url).count();
        st.engine.replace_catalog(models.into_iter().filter(|m| m.base_url != base_url).collect());
        tracing::info!(key = %key, base_url = %base_url, removed, "discovered provider removed");
        (axum::Json(json!({"status": "ok", "key": key, "removed_models": removed}))).into_response()
    } else {
        (StatusCode::NOT_FOUND, axum::Json(json!({"error": {"message": "provider not found"}}))).into_response()
    }
}

/// 恢复被删除的发现 provider（从 removed_base_urls 移出 + 重新扫描拉回）
pub async fn api_providers_restore(
    State(st): State<AppState>,
    axum::Json(body): axum::Json<serde_json::Value>,
) -> Response {
    let Some(base_url) = body.get("base_url").and_then(|v| v.as_str()).map(|s| s.to_string()) else {
        return (StatusCode::BAD_REQUEST, axum::Json(json!({"error": {"message": "base_url is required"}}))).into_response();
    };
    let dir = &st.config.data.dir;
    let mut ov = crate::state::load_overrides(dir);
    ov.removed_base_urls.retain(|u| u != &base_url);
    crate::state::save_overrides(dir, &ov);
    st.rescan_discovery();
    (axum::Json(json!({"status": "ok", "base_url": base_url}))).into_response()
}

/// 手动重拉 provider 的 /models
pub async fn api_providers_refresh(
    State(st): State<AppState>,
    axum::Json(body): axum::Json<serde_json::Value>,
) -> Response {
    let Some(key) = body.get("key").and_then(|k| k.as_str()).map(|s| s.to_string()) else {
        return (StatusCode::BAD_REQUEST, axum::Json(json!({"error": {"message": "key is required"}}))).into_response();
    };
    let dir = &st.config.data.dir;
    let mut ov = crate::state::load_overrides(dir);
    let Some(entry) = ov.providers.get(&key) else {
        return (StatusCode::NOT_FOUND, axum::Json(json!({"error": {"message": "provider not found"}}))).into_response();
    };
    let base = format!("{}{}", entry.base_url.trim_end_matches('/'), "/models");
    let mut req = st.http.get(&base);
    if let Some(k) = &entry.api_key { req = req.bearer_auth(k); }
    match req.timeout(std::time::Duration::from_secs(8)).send().await {
        Ok(resp) if resp.status().is_success() => {
            if let Ok(v) = resp.json::<Value>().await {
                let ids: Vec<String> = v.get("data")
                    .and_then(|d| d.as_array())
                    .map(|arr| arr.iter()
                        .filter_map(|m| m.get("id").and_then(|i| i.as_str()).map(|s| s.to_string()))
                        .collect())
                    .unwrap_or_default();
                let count = ids.len();
                if let Some(e) = ov.providers.get_mut(&key) { e.fetched_models = ids; }
                crate::state::save_overrides(dir, &ov);
                return (axum::Json(json!({"status": "ok", "fetched": count}))).into_response();
            }
        }
        Ok(resp) => {
            return (StatusCode::BAD_GATEWAY, axum::Json(json!({"error": {"message": format!("upstream {}", resp.status())}}))).into_response();
        }
        Err(e) => {
            return (StatusCode::BAD_GATEWAY, axum::Json(json!({"error": {"message": e.to_string()}}))).into_response();
        }
    }
    (StatusCode::INTERNAL_SERVER_ERROR, axum::Json(json!({"error": {"message": "parse failed"}}))).into_response()
}

/// 配额方案注册表：按 baseUrl 匹配 provider 官方接入方案 + 用户订正
pub async fn api_plans(State(st): State<AppState>) -> Response {
    let ov = crate::state::load_overrides(&st.config.data.dir);
    let health = st.health.snapshot();
    let now_ms = evolve_memory::health::now();
    let budgets = st
        .plan_budgets
        .lock()
        .map(|b| b.clone())
        .unwrap_or_default();
    let soft_pct = st.config.policy.plan_soft_pct;
    let sums = st.quota.plan_usage_sums(now_ms, 5 * 3600 * 1000);
    let mut groups: std::collections::BTreeMap<String, serde_json::Value> = std::collections::BTreeMap::new();
    for m in &st.engine.catalog_snapshot() {
        let profile = evolve_core::plans::plan_for(&m.base_url);
        let key = profile.map(|p| p.key.to_string()).unwrap_or_else(|| format!("payg:{}", m.provider));
        let e = groups.entry(key.clone()).or_insert_with(|| {
            let p = profile.unwrap_or(&evolve_core::plans::PAYG);
            let pov = ov.plans.get(&key);
            let allowance = budgets.get(&key).copied();
            json!({
                "key": key,
                "provider": if p.key.is_empty() { m.provider.clone() } else { p.provider.to_string() },
                "plan_kind": p.plan_kind.to_string(),
                "scheme": pov.and_then(|o| o.scheme.clone()).unwrap_or_else(|| p.scheme_key.to_string()),
                "windows": pov.and_then(|o| o.windows.clone()).unwrap_or_else(|| p.windows.to_string()),
                "models_note": p.models_note.to_string(),
                "model_rates": p.model_rates.to_string(),
                "rates_kind": p.rates_kind.to_string(),
                "rates_note": p.rates_note.to_string(),
                "docs_url": p.docs_url.to_string(),
                "tiers": p.tiers.to_string(),
                "tier": pov.and_then(|o| o.tier.clone()).unwrap_or_default(),
                "used_5h": 0.0,
                "allowance": allowance,
                "soft_pct": soft_pct,
                "models": [],
            })
        });
        if let Some(arr) = e.get_mut("models").and_then(|m| m.as_array_mut()) {
            arr.push(json!(m.id));
        }
    }
    // 组内消耗(5h)：Σ 该方案全部模型的积分折算（修正只算首模型的问题）
    let mut used_by_group: std::collections::HashMap<String, f64> = std::collections::HashMap::new();
    for (key, e) in groups.iter_mut() {
        let mut used = 0.0f64;
        if let Some(arr) = e.get("models").and_then(|m| m.as_array()) {
            for mid in arr.iter().filter_map(|x| x.as_str()) {
                if let Some((i, c, o)) = sums.get(mid) {
                    used += evolve_core::plans::plan_credits_used_by_id(mid, *i, *c, *o);
                }
            }
        }
        e["used_5h"] = json!((used * 100.0).round() / 100.0);
        used_by_group.insert(key.clone(), used);
    }
    // 下次窗口重置：组内模型的健康冷却（配额/限流）取最近者
    for e in groups.values_mut() {
        let mut min_until: Option<u64> = None;
        for (hid, h) in &health {
            let hid_model = hid.split('\u{1f}').next().unwrap_or(hid);
            let in_group = e.get("models").and_then(|m| m.as_array()).map(|arr| {
                arr.iter().filter_map(|x| x.as_str()).any(|id| id == hid_model)
            }).unwrap_or(false);
            if !in_group {
                continue;
            }
            if matches!(h.kind, evolve_core::types::HealthKind::QuotaExhausted | evolve_core::types::HealthKind::RateLimited)
                && let Some(until) = h.until_epoch_ms
                && until > now_ms
            {
                min_until = Some(min_until.map_or(until, |u| u.min(until)));
            }
        }
        e["next_reset_epoch_ms"] = json!(min_until);
    }
    (axum::Json(json!({"providers": groups.values().collect::<Vec<_>>()}))).into_response()
}

/// 配额方案订正：档位/方案/窗口说明（持久化，重启重放；档位同时驱动预算额度）
pub async fn api_plans_set(
    State(st): State<AppState>,
    axum::Json(body): axum::Json<serde_json::Value>,
) -> Response {
    let Some(key) = body.get("key").and_then(|k| k.as_str()).map(|s| s.to_string()) else {
        return (StatusCode::BAD_REQUEST, axum::Json(json!({"error": {"message": "key is required"}}))).into_response();
    };
    let text = |k: &str| body.get(k).and_then(|v| v.as_str()).map(|s| s.trim().to_string()).filter(|s| !s.is_empty());
    let dir = &st.config.data.dir;
    let mut ov = crate::state::load_overrides(dir);
    {
        let e = ov.plans.entry(key.clone()).or_default();
        for (k, slot) in [("tier", &mut e.tier), ("scheme", &mut e.scheme), ("windows", &mut e.windows)] {
            if let Some(v) = text(k) {
                *slot = Some(v);
            }
        }
    }
    crate::state::save_overrides(dir, &ov);
    // 档位变化 → 预算额度即时更新（registry 档位表驱动）
    // provider 名不一定等于套餐注册表 key（如 opencode-go-github → opencode-go），
    // 通过 base_url 匹配注册表找到正确的 plan_key
    let base_url = ov.providers.get(&key).map(|p| p.base_url.clone()).unwrap_or_default();
    let plan_key = evolve_core::plans::plan_for(&base_url)
        .map(|p| p.key.to_string())
        .unwrap_or(key.clone());
    let tier: String = ov.providers.get(&key)
        .map(|p| p.tier.clone())
        .filter(|t| !t.is_empty())
        .or_else(|| text("tier"))
        .unwrap_or_default();
    let allowance_5h = if tier.is_empty() { None } else { evolve_core::plans::tier_allowance_by_key(&plan_key, &tier, 0) };
    let allowance_weekly = if tier.is_empty() { None } else { evolve_core::plans::tier_allowance_by_key(&plan_key, &tier, 1) };
    let allowance_monthly = if tier.is_empty() { None } else { evolve_core::plans::tier_allowance_by_key(&plan_key, &tier, 2) };
    // 预算写入用 plan_key（路由按 plan_key 查预算）
    if let Some(a) = allowance_5h {
        if let Ok(mut b) = st.plan_budgets.lock() {
            b.insert(plan_key.clone(), a);
        }
    }
    let e = ov.providers.get(&key).cloned().unwrap_or_default();
    (axum::Json(json!({"status": "ok", "key": key, "plan_key": plan_key, "override": e, "allowance_5h": allowance_5h, "allowance_weekly": allowance_weekly, "allowance_monthly": allowance_monthly}))).into_response()
}

/// SSE stream of routing events for the dashboard.
pub async fn api_stream(
    State(st): State<AppState>,
) -> Sse<impl Stream<Item = Result<Event, Infallible>>> {
    let rx = st.bus.subscribe();
    let stream = BroadcastStream::new(rx).filter_map(|e| async move {
        match e {
            Ok(v) => Some(Ok(Event::default().data(v.to_string()))),
            Err(_) => None,
        }
    });
    Sse::new(stream).keep_alive(KeepAlive::new().interval(Duration::from_secs(15)))
}

pub async fn messages_stub() -> Response {
    (
        StatusCode::NOT_IMPLEMENTED,
        axum::Json(json!({
            "error": {
                "type": "evolve_not_implemented",
                "message": "Anthropic /v1/messages ingress lands in M3; use the OpenAI-compatible /v1/chat/completions for now"
            }
        })),
    )
        .into_response()
}

#[cfg(test)]
mod tag_tests {
    use super::assign_tags_within;
    use evolve_core::tags::{Capability, PriceTier};
    use evolve_core::types::{Cost, ModelRecord, Source, Tiers};
    use std::collections::HashSet;

    fn mk(id: &str, coding: f32, in_price: f32) -> ModelRecord {
        ModelRecord {
            id: id.into(),
            provider: "p".into(),
            base_url: "http://127.0.0.1:7777/v1".into(), // 本地 → has_credential
            upstream_model: id.into(),
            tiers: Tiers { coding, reasoning: coding, agentic: coding, vision: 0.0 },
            cost: Some(Cost { input: in_price, output: in_price * 2.0 }),
            source: Source::User,
            ..Default::default()
        }
    }

    fn mk_plan(id: &str, base_url: &str) -> ModelRecord {
        ModelRecord {
            id: id.into(),
            provider: id.split('/').next().unwrap_or("p").into(),
            base_url: base_url.into(),
            upstream_model: id.split('/').last().unwrap_or(id).into(),
            tiers: Tiers { coding: 0.6, reasoning: 0.6, agentic: 0.6, vision: 0.0 },
            source: Source::User,
            ..Default::default()
        }
    }

    /// 价位分桶必须细化到方案：智谱"积分系数"与 opencode-go"美元系数"量纲不同，
    /// 混在一个桶里会把积分口径的大数值整体推到 premium（实测 zhipu 全族 premium，
    /// 再被 `difficulty≤0.8 → 只允许 tier 0` 的硬闸全族挡死）。分桶后**每个方案
    /// 内部各自呈 7:2:1**，每个方案都有 cheap 可用。
    #[test]
    fn price_tier_buckets_plans_per_scheme() {
        let zhipu = "https://open.bigmodel.cn/api/coding/paas/v4";
        let go = "https://opencode.ai/zen/go/v1";
        let mut snap: Vec<ModelRecord> = Vec::new();
        for i in 0..10 {
            snap.push(mk_plan(&format!("zhipuai-coding-plan/glm-5.{i}"), zhipu));
        }
        for i in 0..10 {
            snap.push(mk_plan(&format!("opencode-go/m{i}"), go));
        }
        let usable: HashSet<String> = snap.iter().map(|m| m.id.clone()).collect();
        let tags = assign_tags_within(&snap, &usable);
        let tier = |id: &str| tags.get(id).map(|(_, p)| *p);
        // 两个方案各自桶内最便宜者都应是 cheap
        assert_eq!(tier("zhipuai-coding-plan/glm-5.0"), Some(PriceTier::Cheap), "zhipu 桶内应有 cheap（旧行为：全族 premium）");
        assert_eq!(tier("opencode-go/m0"), Some(PriceTier::Cheap));
        // zhipu 不再全族 premium：桶内 70/90 分位应产生 cheap/medium/premium 分布
        let zhipu_tiers: Vec<PriceTier> = (0..10)
            .filter_map(|i| tier(&format!("zhipuai-coding-plan/glm-5.{i}")))
            .collect();
        assert!(zhipu_tiers.iter().any(|t| *t == PriceTier::Cheap));
        assert!(zhipu_tiers.iter().any(|t| *t != PriceTier::Cheap), "桶内应有非 cheap 档");
    }

    #[test]
    fn unusable_models_map_by_value_not_to_lowest() {
        // 回归修复：不可用模型此前在 basis 里查不到位置 → rank 0 → 全落 basic/cheap。
        // 现在按阈值（值）映射：最贵最强的不可用模型应落 premium/powerful。
        let mut snap: Vec<ModelRecord> = Vec::new();
        for i in 0..12 {
            snap.push(mk(&format!("m{i}"), 0.3 + i as f32 * 0.05, 1.0 + i as f32));
        }
        let usable: HashSet<String> = (0..8).map(|i| format!("m{i}")).collect();
        let tags = assign_tags_within(&snap, &usable);
        let (cap, price) = tags.get("m11").cloned().unwrap();
        assert_eq!(cap, Capability::Powerful, "unusable top model must map by value");
        assert_eq!(price, PriceTier::Premium, "unusable priciest model must map by value");
    }

    #[test]
    fn quantiles_use_usable_pool_only() {
        // 12 个模型；其中 4 个（最贵/最强的）标记为不可用
        let mut snap: Vec<ModelRecord> = Vec::new();
        for i in 0..12 {
            snap.push(mk(&format!("m{i}"), 0.3 + i as f32 * 0.05, 1.0 + i as f32));
        }
        let usable: HashSet<String> = (0..8).map(|i| format!("m{i}")).collect();
        let tags = assign_tags_within(&snap, &usable);
        // 分位分母=8：可用池内应呈 70/20/10（8 → 5~6 cheap / 1~2 medium / 0~1 premium）
        let cap_counts = |t: Capability| tags.values().filter(|(c, _)| *c == t).count();
        let price_counts = |t: PriceTier| tags.values().filter(|(_, p)| *p == t).count();
        // 不可用的 4 个（最贵）不得占据"可用池的"分位名额——
        // 用最贵模型 m11 验证：它不应被当作可用池的 premium 代表
        assert!(tags.contains_key("m11"));
        // 可用池 8 个里 premium ≤1（10% of 8）
        let premium_in_usable = (0..8).filter(|i| tags.get(&format!("m{i}")).map(|(_, p)| *p == PriceTier::Premium).unwrap_or(false)).count();
        assert!(premium_in_usable <= 1, "premium in usable should track 10% of 8, got {premium_in_usable}");
        // 能力分位也只看可用池：最强者（m7）应落在可用池的 powerful 档
        assert_eq!(tags.get("m7").map(|(c, _)| *c), Some(Capability::Powerful));
        let _ = (cap_counts, price_counts);
    }

    #[test]
    fn capability_rank_ignores_catalog_order() {
        // 回归：cap_of 曾在**未排序**的 basis 上取 position → 标签随目录位置漂移。
        // 这里把能力打乱（最强在中间、弱者在末位），标签必须按能力值分配。
        let caps = [0.30f32, 0.80, 0.35, 0.90, 0.40, 0.85, 0.45, 0.95, 0.50, 0.55];
        let mut snap: Vec<ModelRecord> = Vec::new();
        for (i, c) in caps.iter().enumerate() {
            snap.push(mk(&format!("m{i}"), *c, 1.0 + i as f32));
        }
        let usable: HashSet<String> = (0..caps.len()).map(|i| format!("m{i}")).collect();
        let tags = assign_tags_within(&snap, &usable);
        assert_eq!(
            tags.get("m7").map(|(c, _)| *c),
            Some(Capability::Powerful),
            "strongest (m7=0.95) must be powerful regardless of catalog position"
        );
        assert_eq!(
            tags.get("m0").map(|(c, _)| *c),
            Some(Capability::Basic),
            "weakest (m0=0.30) must be basic"
        );
        assert_ne!(
            tags.get("m9").map(|(c, _)| *c),
            Some(Capability::Powerful),
            "mid model at catalog tail must not be promoted by position"
        );
    }
}

#[cfg(test)]
mod quota_derive_tests {
    use super::derive_quota;
    use serde_json::json;

    #[test]
    fn observed_wins_and_binds_to_most_used_window() {
        let observed = json!({
            "5h": {"limit": 100.0, "remaining": 40.0, "reset_epoch_ms": 111},
            "7d": {"limit": 500.0, "remaining": 0.0, "reset_epoch_ms": 222},
        });
        let windows = json!({"5h": 100.0, "weekly": 500.0, "monthly": 3000.0});
        let (per, binding) = derive_quota(&observed, &windows, 12.0);
        // 5h 实测 60%；weekly 兼容 7d 键名（100%，耗尽）
        assert_eq!(per["5h"]["used_pct"], 60.0);
        assert_eq!(per["5h"]["source"], "observed");
        assert_eq!(per["weekly"]["used_pct"], 100.0);
        assert_eq!(per["weekly"]["source"], "observed");
        // 绑定窗口 = 用量最高者；有实测耗尽 → exhausted + 最长恢复时间
        assert_eq!(binding["scope"], "weekly");
        assert_eq!(binding["exhausted"], true);
        assert_eq!(binding["reset_epoch_ms"], 222.0);
    }

    #[test]
    fn estimates_when_no_observed() {
        let (per, binding) = derive_quota(
            &json!({}),
            &json!({"5h": 100.0, "weekly": 700.0, "monthly": 3000.0}),
            50.0,
        );
        assert_eq!(per["5h"]["used_pct"], 50.0);
        assert_eq!(per["5h"]["source"], "estimate");
        assert_eq!(per["weekly"]["used_pct"], 50.0);
        assert_eq!(per["monthly"]["used_pct"], 50.0);
        // 仅估算不算耗尽（无实测 remaining）
        assert_eq!(binding["exhausted"], false);
    }

    #[test]
    fn empty_when_nothing_declared() {
        let (per, binding) = derive_quota(&json!({}), &json!({}), 0.0);
        assert_eq!(per.as_object().unwrap().len(), 0);
        assert!(binding["scope"].is_null());
        assert_eq!(binding["exhausted"], false);
    }

    /// 有声明额度但无 observed、5h 用量为 0：仍须出窗并给出 scope——否则客户端
    /// 拿不到窗口只能回落 "payg" 文案，把订阅方案误标成按量（volcengine 实测回归）
    #[test]
    fn declared_allowance_alone_still_yields_a_window() {
        let (per, binding) = derive_quota(&json!({}), &json!({"5h": 12000.0, "weekly": 60000.0}), 0.0);
        assert_eq!(per["5h"]["used_pct"], 0.0);
        assert_eq!(per["5h"]["source"], "estimate");
        assert_eq!(per["weekly"]["used_pct"], 0.0);
        assert_eq!(binding["scope"], "5h", "全 0% 时绑定首个窗口（5h），不得为 null");
        assert_eq!(binding["exhausted"], false);
    }
}

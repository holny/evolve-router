//! 心选路径 API（/api/routing-mode、/api/models/favorite）——TDD Task 3。
//! 骨架沿 tests/proxy.rs：build_router(build_state(cfg)) + app.oneshot；
//! 每测试独立 data 目录（覆盖/事件隔离，防跨测试污染）。

use axum::body::Body;
use axum::http::{Request, StatusCode};
use http_body_util::BodyExt;
use evolve_core::config::{BenchmarksCfg, CatalogCfg, DataCfg, DecisionCfg, DiscoveryCfg, FileConfig, ModelEntry, PolicyCfg, QuotaCfg, ServerCfg};
use evolve_core::types::{Cost, Tiers};
use evolve_server::state::{build_router, build_state, load_overrides};
use serde_json::{json, Value};
use tower::ServiceExt;

const PROVIDER: &str = "mock";

/// mock 上游（沿 tests/proxy.rs）：非流式返回固定 usage，供 chat 事件断言
async fn spawn_mock_upstream() -> u16 {
    let app = axum::Router::new().route(
        "/v1/chat/completions",
        axum::routing::post(|body: axum::body::Bytes| async move {
            let v: Value = serde_json::from_slice(&body).unwrap_or(Value::Null);
            let model = v["model"].as_str().unwrap_or("unknown").to_string();
            axum::http::Response::builder()
                .status(200)
                .header("content-type", "application/json")
                .body(Body::from(
                    json!({"model": model, "choices": [{"message": {"role": "assistant", "content": "ok"}}],
                           "usage": {"prompt_tokens": 123, "completion_tokens": 45}})
                        .to_string(),
                ))
                .unwrap()
        }),
    );
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    tokio::spawn(async move {
        axum::serve(listener, app).await.unwrap();
    });
    port
}

/// 返回 (config, data_dir)：data_dir 供断言持久化（load_overrides），
/// 也可预写 overrides.json 后再 build_state
fn test_config() -> (FileConfig, String) {
    static SEQ: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);
    let n = SEQ.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    let dir = std::env::temp_dir()
        .join(format!("mr-fav-{}-{}", std::process::id(), n))
        .to_string_lossy()
        .into_owned();
    let mk = |id: &str| ModelEntry {
        id: id.into(),
        provider: PROVIDER.into(),
        base_url: "http://127.0.0.1:1/v1".into(),
        api_key_env: None,
        upstream_model: Some(format!("mock-{id}")),
        context_window: Some(128_000),
        max_output: 4096,
        cost: Some(Cost { input: 0.6, output: 2.4 }),
        tiers: Tiers { reasoning: 0.67, coding: 0.75, vision: 0.0, agentic: 0.75 },
        speed_tier: 0.7,
        source_note: None,
        ..Default::default()
    };
    let cfg = FileConfig {
        server: ServerCfg { host: "127.0.0.1".into(), port: 0 },
        policy: PolicyCfg { explore_ratio: 0.0, ..Default::default() },
        decision: DecisionCfg { backend: "heuristic".into(), redact: true, slots: vec![], judge_timeout_ms: None },
        providers: vec![],
        catalog: CatalogCfg::default(),
        quota: QuotaCfg::default(),
        data: DataCfg { dir: dir.clone() },
        discovery: DiscoveryCfg { agents: vec![] },
        benchmarks: BenchmarksCfg { enabled: false, interval_hours: 24, sources: vec![] },
        telemetry: Default::default(),
        strategy: Default::default(),
        models: vec![mk("mini"), mk("standard"), mk("frontier")],
    };
    (cfg, dir)
}

/// 预置 overrides.json（容量/换选场景需要心选表先有内容）。
/// ⚠ 落盘路径必须走 state::overrides_path（<data_dir>/state/overrides.json），
/// 直接写 data_dir 根会被网关无视——已实测踩坑（RED 阶段 400/200 假失败）。
fn config_with_seeded_favorites(favs: &[&str]) -> (FileConfig, String) {
    let (cfg, dir) = test_config();
    let path = evolve_server::state::overrides_path(&dir);
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    std::fs::write(path, json!({"favorites": {PROVIDER: favs}}).to_string()).unwrap();
    (cfg, dir)
}

async fn send(app: axum::Router, req: Request<Body>) -> (StatusCode, Value) {
    let resp = app.oneshot(req).await.unwrap();
    let status = resp.status();
    let bytes = resp.into_body().collect().await.unwrap().to_bytes();
    (status, serde_json::from_slice(&bytes).unwrap_or(Value::Null))
}

fn get(uri: &str) -> Request<Body> {
    Request::builder().method("GET").uri(uri).body(Body::empty()).unwrap()
}

fn req(method: &str, uri: &str, body: Value) -> Request<Body> {
    Request::builder()
        .method(method)
        .uri(uri)
        .header("content-type", "application/json")
        .body(Body::from(body.to_string()))
        .unwrap()
}

#[tokio::test]
async fn get_default_returns_gateway() {
    let (cfg, _) = test_config();
    let app = build_router(build_state(cfg));
    let (status, body) = send(app, get("/api/routing-mode")).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["mode"], "gateway");
}

#[tokio::test]
async fn put_favorites_roundtrips_and_persists() {
    let (cfg, dir) = test_config();
    let app = build_router(build_state(cfg));
    let (status, body) = send(app, req("PUT", "/api/routing-mode", json!({"mode": "favorites"}))).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["mode"], "favorites");
    // 持久化：从盘上重读
    assert_eq!(load_overrides(&dir).routing_mode.as_str(), "favorites");
}

#[tokio::test]
async fn put_unknown_mode_400() {
    let (cfg, _) = test_config();
    let app = build_router(build_state(cfg));
    let (status, _) = send(app, req("PUT", "/api/routing-mode", json!({"mode": "turbo"}))).await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
}

#[tokio::test]
async fn post_favorite_appends() {
    let (cfg, dir) = test_config();
    let app = build_router(build_state(cfg));
    let (s1, _) = send(app.clone(), req("POST", "/api/models/favorite", json!({"provider": PROVIDER, "model": "mini"}))).await;
    let (s2, _) = send(app.clone(), req("POST", "/api/models/favorite", json!({"provider": PROVIDER, "model": "standard"}))).await;
    assert_eq!(s1, StatusCode::OK);
    assert_eq!(s2, StatusCode::OK);
    assert_eq!(
        load_overrides(&dir).favorites.get(PROVIDER),
        Some(&vec!["mini".to_string(), "standard".to_string()])
    );
    // GET 回读：favorite 标记与清单
    let (_, body) = send(app, get(&format!("/api/models/favorite?provider={PROVIDER}"))).await;
    assert_eq!(body["favorites"], json!(["mini", "standard"]));
    let mini = body["models"].as_array().unwrap().iter().find(|m| m["model"] == "mini").unwrap();
    assert_eq!(mini["favorite"], true);
    assert!(
        ["cheap", "medium", "premium"].contains(&mini["price_tier"].as_str().unwrap_or("")),
        "price_tier must come from tags: {}",
        mini["price_tier"]
    );
    assert!(
        ["basic", "standard", "powerful"].contains(&mini["capability"].as_str().unwrap_or("")),
        "capability must come from tags: {}",
        mini["capability"]
    );
    assert!(mini["tokens_7d"].is_number(), "7-day tokens column must exist (user ruling 2026-10-29): {}", mini);
    assert!(mini["ttft_ms"].is_null() || mini["ttft_ms"].is_number());
    assert!(mini["cache_hit_pct"].is_null() || mini["cache_hit_pct"].is_number());
    // 评审 UX round：选择器行带 范围来源 + 免费标记（复用来源标签，提示"已在候选队列"）
    assert!(mini["free"].is_boolean(), "free flag must exist: {}", mini);
    assert_eq!(mini["scope_source"], "none", "gateway mode has no scope membership");
}

#[tokio::test]
async fn post_ghost_404() {
    let (cfg, _) = test_config();
    let app = build_router(build_state(cfg));
    let (status, _) = send(app, req("POST", "/api/models/favorite", json!({"provider": PROVIDER, "model": "nonexistent"}))).await;
    assert_eq!(status, StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn post_4th_without_replace_400() {
    let (cfg, _) = config_with_seeded_favorites(&["x1", "x2", "x3"]);
    let app = build_router(build_state(cfg));
    // 目录真实模型 + 已满 3 的预置心选表 → 不带 replace 必须 400 并提示 replace
    let (status, body) = send(app, req("POST", "/api/models/favorite", json!({"provider": PROVIDER, "model": "mini"}))).await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert!(body["error"]["message"].as_str().unwrap_or("").contains("replace"), "must tell the caller to pass replace");
}

#[tokio::test]
async fn post_replace_swaps_in_place() {
    let (cfg, dir) = config_with_seeded_favorites(&["x1", "x2", "frontier"]);
    let app = build_router(build_state(cfg));
    let (status, _) = send(app, req("POST", "/api/models/favorite", json!({"provider": PROVIDER, "model": "mini", "replace": "x2"}))).await;
    assert_eq!(status, StatusCode::OK);
    // x2 原位换成 mini，其余顺序不动
    assert_eq!(
        load_overrides(&dir).favorites.get(PROVIDER),
        Some(&vec!["x1".to_string(), "mini".to_string(), "frontier".to_string()])
    );
}

#[tokio::test]
async fn post_replace_unknown_400() {
    let (cfg, _) = config_with_seeded_favorites(&["x1", "x2", "x3"]);
    let app = build_router(build_state(cfg));
    let (status, _) = send(app, req("POST", "/api/models/favorite", json!({"provider": PROVIDER, "model": "mini", "replace": "nope"}))).await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
}

/// Task 4：routing_mode=favorites 时 active 判定必须走心选范围——
/// 只有 frontier 是心选 → tags 里 frontier.active=true、mini/standard.active=false
/// （gateway 组合口径下三者全 active，故此断言在分发实现前必红）
/// Task 6：chat 事件带来源标签——favorites 模式下路由到心选模型的事件
/// `source == "fav"`（gateway 模式恒 null，由 relay 分支保证）
/// Task 7：enable/disable 开关退役（用户裁决 2026-10-28）——
/// 排除语义归 Off(-3)、强制归 +3 Pinned/心选，路由范围由双路径决定。
#[tokio::test]
async fn models_active_route_retired() {
    let (cfg, _) = test_config();
    let app = build_router(build_state(cfg));
    let (status, _) = send(app, req("POST", "/api/models/active", json!({"model": "mini", "action": "enable"}))).await;
    assert_eq!(status, StatusCode::NOT_FOUND, "retired endpoint must be gone");
}

#[tokio::test]
async fn model_tags_have_active_but_no_pinned() {
    let (cfg, _) = test_config();
    let app = build_router(build_state(cfg));
    let (_, body) = send(app.clone(), get("/api/model-tags")).await;
    let m = body["models"].as_array().and_then(|a| a.first()).expect("catalog models present");
    assert!(m["active"].is_boolean(), "active must stay (mode-aware scope membership)");
    assert!(m.get("pinned").is_none(), "pinned flag retired from tags");
    let (_, pf) = send(app, get("/api/portfolio")).await;
    assert!(pf.get("totals").and_then(|t| t.get("pinned")).is_none(), "pinned retired from portfolio totals");
    let prov = pf["providers"].as_array().and_then(|a| a.first()).expect("providers present");
    assert!(prov.get("pinned").is_none(), "pinned retired from portfolio providers");
}

#[tokio::test]
async fn post_remove_deletes() {
    let (cfg, dir) = config_with_seeded_favorites(&["mini", "standard"]);
    let app = build_router(build_state(cfg));
    let (status, _) = send(app, req("POST", "/api/models/favorite", json!({"provider": PROVIDER, "model": "mini", "remove": true}))).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(
        load_overrides(&dir).favorites.get(PROVIDER),
        Some(&vec!["standard".to_string()])
    );
}

#[tokio::test]
async fn routing_mode_get_carries_favorites_map() {
    let (cfg, _) = config_with_seeded_favorites(&["mini"]);
    let app = build_router(build_state(cfg));
    let (_, body) = send(app, get("/api/routing-mode")).await;
    assert_eq!(body["mode"], "gateway");
    assert_eq!(body["favorites"][PROVIDER], json!(["mini"]), "tray needs the global favorites map in one call");
    // gateway 模式：scope 为空对象（范围构成只属于 Favorites 路径）
    assert_eq!(body["scope"], json!({}));
}

/// 评审 UX round 3：favorites 模式下 scope 计数按 provider 出列
/// （合并版 Providers 卡 subline：fav/top/new/rec/free/total）
#[tokio::test]
async fn routing_mode_get_scope_counts_in_favorites_mode() {
    let (cfg, _) = config_with_seeded_favorites(&["mini"]);
    let app = build_router(build_state(cfg));
    let (s, _) = send(app.clone(), req("PUT", "/api/routing-mode", json!({"mode": "favorites"}))).await;
    assert_eq!(s, StatusCode::OK);
    let (_, body) = send(app, get("/api/routing-mode")).await;
    let scope = &body["scope"][PROVIDER];
    assert!(scope.is_object(), "scope counts must exist in favorites mode: {}", body["scope"]);
    assert_eq!(scope["fav"].as_u64().unwrap(), 1, "seeded favorite elected");
    for k in ["top", "new", "rec", "free", "total"] {
        assert!(scope[k].is_number(), "scope.{k} must be a number: {}", scope);
    }
    assert_eq!(
        scope["total"].as_u64().unwrap(),
        ["fav", "top", "new", "rec", "free"].iter().map(|k| scope[*k].as_u64().unwrap_or(0)).sum::<u64>(),
        "total = sum of members"
    );
}

/// 评审 L10：remove 落空（provider 无心选记录）→ 404 且不得在 overrides
/// 里落 "provider": [] 幽灵条目
#[tokio::test]
async fn remove_miss_leaves_no_stray_entry() {
    let (cfg, dir) = test_config();
    let app = build_router(build_state(cfg));
    let (status, _) = send(app, req("POST", "/api/models/favorite", json!({"provider": PROVIDER, "model": "mini", "remove": true}))).await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    assert!(load_overrides(&dir).favorites.get(PROVIDER).is_none(), "no stray empty entry");
}

/// 评审 UX round 2：provider 级禁用（心选面板开关复用 providers disabled）
/// → 该 provider 的模型全路径排除（范围 active=false + unusable_reason）
#[tokio::test]
async fn disabled_provider_excluded_from_scope() {
    let (cfg, _) = test_config();
    let app = build_router(build_state(cfg));
    let (s, _) = send(app.clone(), req("PUT", "/api/routing-mode", json!({"mode": "favorites"}))).await;
    assert_eq!(s, StatusCode::OK);
    let (s, _) = send(app.clone(), req("POST", "/api/models/favorite", json!({"provider": PROVIDER, "model": "mini"}))).await;
    assert_eq!(s, StatusCode::OK);
    // 禁用 provider（心选面板开关写的同一端点：POST /api/providers）
    let (s, _) = send(app.clone(), req("POST", "/api/providers", json!({"key": PROVIDER, "disabled": true}))).await;
    assert_eq!(s, StatusCode::OK);
    let (_, body) = send(app, get("/api/model-tags")).await;
    let mini = body["models"].as_array().unwrap().iter().find(|m| m["model"] == "mini").unwrap().clone();
    assert_eq!(mini["active"], false, "disabled provider must leave the scope: {}", mini);
    assert_eq!(mini["unusable_reason"], "provider-off", "reason must surface in UI: {}", mini);
}

#[tokio::test]
async fn chat_event_carries_source_tag() {
    let port = spawn_mock_upstream().await;
    let (mut cfg, _) = test_config();
    for m in cfg.models.iter_mut() {
        m.base_url = format!("http://127.0.0.1:{port}/v1");
    }
    let app = build_router(build_state(cfg));
    let (s, _) = send(app.clone(), req("PUT", "/api/routing-mode", json!({"mode": "favorites"}))).await;
    assert_eq!(s, StatusCode::OK);
    let (s, _) = send(app.clone(), req("POST", "/api/models/favorite", json!({"provider": PROVIDER, "model": "mini"}))).await;
    assert_eq!(s, StatusCode::OK);
    // 显式指定心选模型（direct 路径亦可）——事件应带 source=fav
    let (status, _) = send(app.clone(), req("POST", "/v1/chat/completions", json!({"model": "mini", "messages": [{"role": "user", "content": "hi"}]}))).await;
    assert_eq!(status, StatusCode::OK);
    let (_, events) = send(app, get("/api/events?limit=5&kind=chat")).await;
    let chat = events.as_array().and_then(|a| a.iter().find(|e| e["chosen"] == "mini")).expect("chat event for mini must exist");
    assert_eq!(chat["source"], "fav", "chosen favorite must carry source=fav: {chat}");
}

/// Task 评审 M4：被 Off(-3) 的心选模型不得占用范围槽位（引擎硬排除它，
/// 留在范围里只是白吃 3-6 个名额中的一个）——active 必须为 false
#[tokio::test]
async fn offed_favorite_leaves_the_scope() {
    let (cfg, _) = test_config();
    let app = build_router(build_state(cfg));
    let (s, _) = send(app.clone(), req("PUT", "/api/routing-mode", json!({"mode": "favorites"}))).await;
    assert_eq!(s, StatusCode::OK);
    let (s, _) = send(app.clone(), req("POST", "/api/models/favorite", json!({"provider": PROVIDER, "model": "standard"}))).await;
    assert_eq!(s, StatusCode::OK);
    let (s, _) = send(app.clone(), req("POST", "/api/priority", json!({"model": "standard", "level": -3}))).await;
    assert_eq!(s, StatusCode::OK);
    let (_, body) = send(app, get("/api/model-tags")).await;
    let find = |id: &str| body["models"].as_array().unwrap().iter().find(|m| m["model"] == id).unwrap().clone();
    assert_eq!(find("standard")["active"], false, "Off'd favorite must not occupy scope slots: {}", find("standard"));
}

#[tokio::test]
async fn model_tags_active_follows_favorites_scope() {
    let (cfg, _) = test_config();
    let app = build_router(build_state(cfg));
    let (s, _) = send(app.clone(), req("PUT", "/api/routing-mode", json!({"mode": "favorites"}))).await;
    assert_eq!(s, StatusCode::OK);
    let (s, _) = send(app.clone(), req("POST", "/api/models/favorite", json!({"provider": PROVIDER, "model": "frontier"}))).await;
    assert_eq!(s, StatusCode::OK);
    let (_, body) = send(app, get("/api/model-tags")).await;
    let find = |id: &str| body["models"].as_array().unwrap().iter().find(|m| m["model"] == id).unwrap().clone();
    // fixture 标签（auto 推导）：frontier=powerful/cheap(2,0)，mini/standard=basic/cheap(0,0)
    assert_eq!(find("frontier")["active"], true, "favorite must be in scope: {}", find("frontier"));
    // 9 格覆盖：(0,0) 邻域无代表 → REC 兜底在 mini/standard（同格同分）中按 id 升序取 mini
    assert_eq!(find("mini")["active"], true, "REC backfill for cell coverage (user-approved): {}", find("mini"));
    // standard 输掉同分 tie-break，且没有格需要它 → 不在范围
    assert_eq!(find("standard")["active"], false, "loser of the tie with no cell to fill must stay out: {}", find("standard"));
}

/// 评审：显式点名被禁 provider 的模型 → 403 拒绝（"彻底不使用"含点名路径，
/// 错误信息指明重开入口）
#[tokio::test]
async fn explicit_request_to_disabled_provider_rejected() {
    let (cfg, _) = test_config();
    let app = build_router(build_state(cfg));
    // 禁用 provider（心选面板开关写的同一端点）
    let (s, _) = send(app.clone(), req("POST", "/api/providers", json!({"key": PROVIDER, "disabled": true}))).await;
    assert_eq!(s, StatusCode::OK);
    let (status, body) = send(app, req("POST", "/v1/chat/completions", json!({"model": "mini", "messages": [{"role": "user", "content": "hi"}]}))).await;
    assert_eq!(status, StatusCode::FORBIDDEN, "explicit request to a disabled provider must be rejected: {}", body);
    assert!(body["error"]["message"].as_str().unwrap_or("").contains("disabled"), "error must name the disable: {}", body);
}

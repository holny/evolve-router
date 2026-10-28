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
    assert!(mini["yesterday_tokens"].is_number(), "placeholder field must exist (Task 4 wires real data)");
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

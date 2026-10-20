//! Coding Plan 价格/额度定时拉取（2026-10-20）。
//!
//! 数据源：`wmpeng/codingplan` 仓库的 `plans.json`（schemaVersion 3）——
//! 191 个套餐 / 50 个平台的**结构化**数据，含模型级单价（每 M tokens）、
//! 5h/周/月 额度、**峰谷价**（timeTier: 谷/峰）、月费与币种。
//! vibecoding.dreamfree.space/pricing 就是该仓库的部署站点（同一数据源，
//! 页面为 JS 渲染且无 RSS/API）——拉仓库 JSON 即可，无需网页解析。
//!
//! 刷新节奏：启动即拉，之后每 6h 一次（数据源 generatedAt 本身为周期性更新）。
//! 拉取失败保留上次快照并按间隔重试。模型匹配键 = 小写字母数字归一化
//! （"glm-5-3-flash" 与 "glm-5.3-flash" 归同为 "glm53flash"）。

use serde::Deserialize;
use serde_json::json;
use std::collections::BTreeMap;
use std::sync::LazyLock;

use axum::extract::State;
use axum::response::IntoResponse;

const SOURCE_BASES: [&str; 3] = [
    "https://raw.githubusercontent.com/wmpeng/codingplan/main",
    // 国内网络下 raw.githubusercontent 时通时断（2026-10-20 实测 000）→
    // jsDelivr CDN（国内可达，缓存 ~12h，数据源本身按日更新，可接受）
    "https://cdn.jsdelivr.net/gh/wmpeng/codingplan@main",
    "https://gh-proxy.com/https://raw.githubusercontent.com/wmpeng/codingplan/main",
];

fn source_url(base: &str, file: &str) -> String {
    format!("{base}/{file}")
}
const REFRESH_INTERVAL_MS: u64 = 6 * 3600 * 1000;
const RETRY_FAIL_MS: u64 = 5 * 60 * 1000;

#[derive(Debug, Clone, serde::Serialize)]
pub struct PlanModelPrice {
    pub platform: String,
    pub plan: String,
    pub model_slug: String,
    /// 归一化匹配键（小写字母数字）——与网关目录模型名同法归一后比对
    pub model_key: String,
    /// 谷 / 峰（部分套餐无时段价 = None）
    pub time_tier: Option<String>,
    pub currency: String,
    pub monthly_price: Option<f64>,
    pub five_hour_tokens_m: Option<f64>,
    pub weekly_tokens_m: Option<f64>,
    pub monthly_tokens_m: Option<f64>,
    /// 每百万 tokens 单价（CNY；数据源已按套餐月费/额度折算）
    pub unit_price_cny_per_m: Option<f64>,
}

#[derive(Debug, Clone)]
struct Snapshot {
    generated_at: String,
    fetched_at_ms: u64,
    source: String,
    rows: Vec<PlanModelPrice>,
    /// platformSlug → (名称, 评分, 摘要)
    platforms: BTreeMap<String, (String, f64, String)>,
}

static SNAPSHOT: LazyLock<std::sync::RwLock<Option<Snapshot>>> =
    LazyLock::new(|| std::sync::RwLock::new(None));

#[derive(Deserialize)]
struct PlansFile {
    #[serde(rename = "generatedAt", default)]
    generated_at: String,
    #[serde(rename = "plans", default)]
    plans: Vec<PlanEntry>,
}

#[derive(Deserialize)]
struct PlanEntry {
    #[serde(rename = "platformSlug")]
    platform_slug: String,
    name: String,
    #[serde(rename = "monthlyPrice")]
    monthly_price: Option<serde_json::Value>,
    currency: Option<String>,
    #[serde(rename = "planModels", default)]
    plan_models: Vec<PlanModelEntry>,
}

#[derive(Deserialize)]
struct PlatformsFile {
    #[serde(default)]
    platforms: Vec<PlatformEntry>,
}

#[derive(Deserialize)]
struct PlatformEntry {
    slug: String,
    name: String,
    rating: Option<u32>,
    summary: Option<String>,
    #[serde(default)]
    dimensions: Option<PlatformDimensions>,
}

#[derive(Deserialize)]
struct PlatformDimensions {
    #[serde(rename = "value")]
    value: Option<DimScore>,
}

#[derive(Deserialize)]
struct DimScore {
    score: Option<u32>,
}

#[derive(Deserialize)]
struct PlanModelEntry {
    #[serde(rename = "modelSlug")]
    model_slug: String,
    #[serde(rename = "timeTier")]
    time_tier: Option<String>,
    usage: Option<serde_json::Value>,
}

fn num(v: &serde_json::Value, k: &str) -> Option<f64> {
    v.get(k).and_then(|x| x.as_f64())
}

fn nkey(s: &str) -> String {
    s.to_lowercase().chars().filter(|c| c.is_ascii_alphanumeric()).collect()
}

fn parse_file(text: &str) -> Option<(String, Vec<PlanModelPrice>)> {
    let file: PlansFile = serde_json::from_str(text).ok()?;
    let mut rows = Vec::new();
    for p in &file.plans {
        let monthly_price = p.monthly_price.as_ref().and_then(|v| v.as_f64());
        let currency = p.currency.clone().unwrap_or_default();
        for m in &p.plan_models {
            let (f5, wk, mo, unit) = m
                .usage
                .as_ref()
                .map(|u| {
                    (
                        num(u, "fiveHourTokenInM"),
                        num(u, "weeklyTokenInM"),
                        num(u, "monthlyTokenInM"),
                        num(u, "unitPriceCnyPerM"),
                    )
                })
                .unwrap_or((None, None, None, None));
            rows.push(PlanModelPrice {
                platform: p.platform_slug.clone(),
                plan: p.name.clone(),
                model_slug: m.model_slug.clone(),
                model_key: nkey(&m.model_slug),
                time_tier: m.time_tier.clone(),
                currency: currency.clone(),
                monthly_price,
                five_hour_tokens_m: f5,
                weekly_tokens_m: wk,
                monthly_tokens_m: mo,
                unit_price_cny_per_m: unit,
            });
        }
    }
    Some((file.generated_at, rows))
}

async fn fetch_text(file: &str) -> anyhow::Result<(String, String)> {
    let client = reqwest::Client::builder().timeout(std::time::Duration::from_secs(30)).build()?;
    let mut last = None;
    for base in SOURCE_BASES {
        let url = source_url(base, file);
        match client.get(&url).send().await {
            Ok(resp) => match resp.error_for_status() {
                Ok(resp) => match resp.text().await {
                    Ok(text) => return Ok((text, url)),
                    Err(e) => last = Some(anyhow::anyhow!("text: {e}")),
                },
                Err(e) => last = Some(anyhow::anyhow!("status: {e}")),
            },
            Err(e) => last = Some(anyhow::anyhow!("transport: {e}")),
        }
    }
    Err(last.unwrap_or_else(|| anyhow::anyhow!("all sources failed for {file}")))
}

async fn fetch_once() -> anyhow::Result<(String, Vec<PlanModelPrice>, BTreeMap<String, (String, f64, String)>, String)> {
    let (plans_text, plans_src) = fetch_text("plans.json").await?;
    let (plats_text, _src2) = fetch_text("platforms.json")
        .await
        .unwrap_or_else(|e| {
            tracing::warn!(error = %e, "platforms.json fetch failed (prices unaffected)");
            (String::new(), String::new())
        });
    let mut platforms = BTreeMap::new();
    if let Ok(pf) = serde_json::from_str::<PlatformsFile>(&plats_text) {
        for p in pf.platforms {
            let rating = p.rating.map(|r| r as f64).unwrap_or_else(|| {
                p.dimensions
                    .as_ref()
                    .and_then(|d| d.value.as_ref().and_then(|v| v.score))
                    .unwrap_or(0) as f64
            });
            platforms.insert(p.slug, (p.name, rating, p.summary.unwrap_or_default()));
        }
    }
    let (generated_at, rows) =
        parse_file(&plans_text).ok_or_else(|| anyhow::anyhow!("plans.json schema parse failed"))?;
    Ok((generated_at, rows, platforms, plans_src))
}

/// 拉取并落快照；失败仅记日志（保留上次快照）
pub async fn refresh() {
    match fetch_once().await {
        Ok((generated_at, rows, platforms, source)) => {
            let n = rows.len();
            let now = evolve_memory::health::now();
            tracing::info!(rows = n, source = %source, "coding plan prices refreshed");
            if let Ok(mut g) = SNAPSHOT.write() {
                *g = Some(Snapshot { generated_at, fetched_at_ms: now, source, rows, platforms });
            }
        }
        Err(e) => tracing::warn!(error = %e, "coding plan price refresh failed; keeping last snapshot"),
    }
}

/// 启动即拉，之后每 6h 刷新；失败 5 分钟后重试（源时通时断）
pub fn spawn_refresh_task() {
    tokio::spawn(async move {
        loop {
            refresh().await;
            let ok = snapshot_rows_len() > 0;
            tokio::time::sleep(std::time::Duration::from_millis(
                if ok { REFRESH_INTERVAL_MS } else { RETRY_FAIL_MS },
            ))
            .await;
        }
    });
}

fn snapshot_source() -> String {
    SNAPSHOT.read().ok().and_then(|g| g.as_ref().map(|s| s.source.clone())).unwrap_or_default()
}

fn snapshot_rows_len() -> usize {
    SNAPSHOT.read().ok().and_then(|g| g.as_ref().map(|s| s.rows.len())).unwrap_or(0)
}

fn snapshot_rows() -> Vec<PlanModelPrice> {
    SNAPSHOT.read().ok().and_then(|g| g.as_ref().map(|s| s.rows.clone())).unwrap_or_default()
}

fn snapshot_generated_at() -> String {
    SNAPSHOT
        .read()
        .ok()
        .and_then(|g| g.as_ref().map(|s| s.generated_at.clone()))
        .unwrap_or_default()
}

/// 网关 baseURL → codingplan platformSlug 静态映射（按端点关键词；
/// platforms.json 不含 API 端点字段 → 只能由我方维护这张表，新 provider
/// 接入时在此补一行）。
fn platform_for_base_url(base_url: &str) -> Option<&'static str> {
    let u = base_url.to_lowercase();
    const M: &[(&str, &str)] = &[
        ("opencode.ai", "opencode"),
        ("bigmodel.cn", "zhipu"),
        ("volces.com", "bytedance-ark"),
        ("minimax", "minimax"),
        ("deepseek.com", "deepseek-official"),
        ("moonshot", "kimi"),
        ("bailian", "aliyun-bailian-coding"),
        ("bigmodel", "zhipu"),
        ("trae", "trae-cn"),
    ];
    M.iter().find(|(kw, _)| u.contains(kw)).map(|(_, slug)| *slug)
}

/// GET /api/plan-prices：全量价格行 + 与本网关目录模型的归一化匹配
/// （按 model_key 对齐——同模型在不同 plan 的单价/额度/峰谷一目了然）。
/// `our_providers` 段：按 baseURL 映射到 codingplan 平台的我们自己的
/// provider 列表（平台评分/套餐/各模型峰谷价）。
/// 无快照时返回 503（后台任务启动即拉、失败 5 分钟重试——**不在此阻塞刷新**，
/// 否则源全慢时 API 挂 90s+）。
pub async fn api_plan_prices(
    State(st): State<crate::state::AppState>,
) -> axum::response::Response {
    let rows = snapshot_rows();
    let generated_at = snapshot_generated_at();
    if rows.is_empty() {
        return (
            axum::http::StatusCode::SERVICE_UNAVAILABLE,
            axum::Json(json!({"error": {"message": "price snapshot unavailable (background fetch retrying)"}})),
        )
            .into_response();
    }
    // 按模型归组：model_key → 各平台/套餐的价格行（性价比对比的直接材料）
    let mut by_model: BTreeMap<String, Vec<&PlanModelPrice>> = BTreeMap::new();
    for r in &rows {
        by_model.entry(r.model_key.clone()).or_default().push(r);
    }
    let platforms = snapshot_platforms();
    let models: Vec<serde_json::Value> = by_model
        .iter()
        .map(|(key, group)| {
            let unit_prices: Vec<f64> = group.iter().filter_map(|r| r.unit_price_cny_per_m).collect();
            let cheapest = unit_prices.iter().copied().fold(f64::INFINITY, f64::min);
            json!({
                "model_key": key,
                "model_slug": group.first().map(|r| r.model_slug.clone()).unwrap_or_default(),
                "cheapest_cny_per_m": if unit_prices.is_empty() { serde_json::Value::Null } else { json!(cheapest) },
                "offers": group.iter().map(|r| json!({
                    "platform": r.platform, "plan": r.plan, "time_tier": r.time_tier,
                    "currency": r.currency, "monthly_price": r.monthly_price,
                    "five_hour_tokens_m": r.five_hour_tokens_m,
                    "weekly_tokens_m": r.weekly_tokens_m,
                    "monthly_tokens_m": r.monthly_tokens_m,
                    "unit_price_cny_per_m": r.unit_price_cny_per_m,
                })).collect::<Vec<_>>(),
            })
        })
        .collect();
    // 我们自己的 provider（按 baseURL 映射 codingplan 平台）：平台评分/套餐/
    // 各模型的峰谷价——"我们在用的 coding plan 哪些模型单价最低"的直接答案
    let cat = st.engine.catalog_snapshot();
    let mut seen_pv: std::collections::HashSet<(String, String)> = std::collections::HashSet::new();
    let mut our_providers: Vec<serde_json::Value> = Vec::new();
    for m in &cat {
        if !seen_pv.insert((m.provider.clone(), m.base_url.clone())) {
            continue;
        }
        let Some(slug) = platform_for_base_url(&m.base_url) else {
            our_providers.push(json!({
                "provider": m.provider, "base_url": m.base_url, "platform": serde_json::Value::Null,
                "note": "codingplan 未收录该平台",
            }));
            continue;
        };
        let meta = platforms.get(slug);
        let plan_rows: Vec<&PlanModelPrice> =
            rows.iter().filter(|r| r.platform == slug).collect();
        let mut plans_seen: std::collections::BTreeMap<String, serde_json::Value> = BTreeMap::new();
        for r in &plan_rows {
            plans_seen
                .entry(r.plan.clone())
                .or_insert_with(|| {
                    json!({"plan": r.plan, "currency": r.currency, "monthly_price": r.monthly_price})
                });
        }
        let models_pv: Vec<serde_json::Value> = cat
            .iter()
            .filter(|mm| mm.provider == m.provider && mm.base_url == m.base_url)
            .map(|mm| {
                let key = nkey(&mm.upstream_model);
                let offers: Vec<serde_json::Value> = by_model
                    .get(&key)
                    .map(|g| {
                        g.iter()
                            .map(|r| {
                                json!({"platform": r.platform, "plan": r.plan,
                                       "time_tier": r.time_tier,
                                       "unit_price_cny_per_m": r.unit_price_cny_per_m,
                                       "monthly_tokens_m": r.monthly_tokens_m})
                            })
                            .collect()
                    })
                    .unwrap_or_default();
                let unit: Vec<f64> = offers
                    .iter()
                    .filter_map(|o| o.get("unit_price_cny_per_m").and_then(|x| x.as_f64()))
                    .collect();
                json!({
                    "model": mm.upstream_model,
                    "model_id": mm.id,
                    "cheapest_cny_per_m": if unit.is_empty() { serde_json::Value::Null } else { json!(unit.iter().copied().fold(f64::INFINITY, f64::min)) },
                    "offers": offers,
                })
            })
            .collect();
        our_providers.push(json!({
            "provider": m.provider,
            "base_url": m.base_url,
            "platform_slug": slug,
            "platform_name": meta.map(|(n, _, _)| n.clone()).unwrap_or_default(),
            "rating": meta.map(|(_, r, _)| *r).unwrap_or(0.0),
            "summary": meta.map(|(_, _, s)| s.clone()).unwrap_or_default(),
            "plans": plans_seen.values().cloned().collect::<Vec<_>>(),
            "models": models_pv,
        }));
    }

    (axum::Json(json!({
        "source": snapshot_source(),
        "generated_at": generated_at,
        "models": models,
        "our_providers": our_providers,
    })))
    .into_response()
}

fn snapshot_platforms() -> BTreeMap<String, (String, f64, String)> {
    SNAPSHOT
        .read()
        .ok()
        .and_then(|g| g.as_ref().map(|s| s.platforms.clone()))
        .unwrap_or_default()
}

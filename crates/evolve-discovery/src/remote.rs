//! Remote model-list discovery: GET {base_url}/models on each known
//! provider (OpenAI-compatible), returning the union with user-configured
//! entries. Connection facts always come from the catalog records.

use evolve_core::types::*;
use serde_json::Value;
use std::collections::HashMap;
use std::path::PathBuf;
use std::time::Duration;

/// Group records by (base_url, first key) so one fetch serves all aliases.
pub struct ProviderGroup {
    pub base_url: String,
    pub key: Option<String>,
    pub provider_hint: String,
}

pub fn provider_groups(records: &[ModelRecord]) -> Vec<ProviderGroup> {
    provider_groups_ex(records, None)
}

/// `self_origin`: e.g. "127.0.0.1:8787" — groups pointing at our own
/// listener are the gateway itself, never an upstream (loop guard).
pub fn provider_groups_ex(
    records: &[ModelRecord],
    self_origin: Option<&str>,
) -> Vec<ProviderGroup> {
    let mut groups: HashMap<(String, String), ProviderGroup> = HashMap::new();
    for r in records {
        if r.base_url.is_empty() || r.protocol != Protocol::OpenAI {
            continue;
        }
        if r.provider == "evolve" {
            continue;
        }
        if let Some(origin) = self_origin
            && r.base_url.contains(origin)
        {
            continue;
        }
        let key = r.key_values().first().cloned().unwrap_or_default();
        let e = groups
            .entry((r.base_url.clone(), key.clone()))
            .or_insert_with(|| ProviderGroup {
                base_url: r.base_url.clone(),
                key: Some(key.clone()),
                provider_hint: r.provider.clone(),
            });
        if e.provider_hint == "custom" && r.provider != "custom" {
            e.provider_hint = r.provider.clone();
        }
    }
    groups.into_values().collect()
}

fn provider_name_of(base_url: &str) -> String {
    // https://api.deepseek.com -> deepseek ; https://ark.../api/coding/v3 -> ark
    base_url
        .split("://")
        .nth(1)
        .and_then(|rest| rest.split('/').next())
        .and_then(|host| host.split('.').next())
        .unwrap_or("remote")
        .to_string()
}

/// 一个 /models 条目 → ModelRecord。除 id/created/context_length 外，
/// 还解析 **OpenRouter 风格**的附加元数据（该 provider 的 /models 是公开接口，
/// 不需要 key，且带上完整定价与能力）：
/// - `pricing.prompt` / `pricing.completion`：**每 token 美元**字符串
///   → 换算成目录口径「美元 / 1M tokens」（×1e6）。`:free` 变体为 "0"。
/// - `supported_parameters` 含 `tools` → agentic；`architecture.input_modalities`
///   含 `image` → vision；`reasoning` 存在 → reasoning 更强。
/// 其它 provider（zen 等）没有这些字段 → cost 保持 None、tiers 保持默认，
/// 行为与之前完全一致（避免对既有部署造成路由漂移）。
pub fn record_from_openai_model(
    provider: &str,
    base_url: &str,
    key: Option<&str>,
    m: &Value,
) -> Option<ModelRecord> {
    let id = m.get("id").and_then(|i| i.as_str())?;
    let price_per_mtok = |field: &str| -> Option<f32> {
        let v = m.get("pricing")?.get(field)?.as_str()?.parse::<f64>().ok()?;
        // OpenRouter 用 **-1** 表示"动态定价"（实测 7 条：auto / auto-beta / fusion /
        // pareto-code / jev-router / switchyard / bodybuilder）。若当成负价：
        // min_price 变负 → 其它付费模型成本因子全被 clamp 成 0（25% 成本权重失效）。
        if !v.is_finite() || v < 0.0 {
            return None;
        }
        Some((v * 1e6) as f32)
    };
    let cost = match (price_per_mtok("prompt"), price_per_mtok("completion")) {
        (Some(input), Some(output)) => Some(Cost { input, output }),
        _ => None,
    };
    let params = m.get("supported_parameters").and_then(|v| v.as_array());
    let caps = params.is_some();
    let has_param = |name: &str| {
        params
            .map(|a| a.iter().any(|x| x.as_str() == Some(name)))
            .unwrap_or(false)
    };
    let vision = m
        .get("architecture")
        .and_then(|a| a.get("input_modalities"))
        .and_then(|v| v.as_array())
        .map(|a| a.iter().any(|x| x.as_str() == Some("image")))
        .unwrap_or(false);
    let reasoning = m.get("reasoning").map(|r| !r.is_null()).unwrap_or(false);
    let tiers = if caps || vision || reasoning {
        Some(Tiers {
            reasoning: if reasoning { 0.75 } else { 0.5 },
            coding: 0.6,
            vision: if vision { 0.85 } else { 0.0 },
            agentic: if has_param("tools") { 0.7 } else { 0.4 },
        })
    } else {
        None
    };
    let max_output = m
        .get("top_provider")
        .and_then(|t| t.get("max_completion_tokens"))
        .and_then(|v| v.as_u64())
        .unwrap_or(8192)
        .min(65_536);
    let mut rec = ModelRecord {
        id: format!("{provider}/{id}"),
        provider: provider.to_string(),
        protocol: Protocol::OpenAI,
        base_url: base_url.trim_end_matches('/').to_string(),
        api_key: key.map(|k| k.to_string()),
        upstream_model: id.to_string(),
        context_window: m.get("context_length").and_then(|v| v.as_u64()),
        max_output,
        cost,
        currency: evolve_core::types::infer_currency(provider).to_string(),
        source: Source::Remote,
        source_note: Some("remote /models".into()),
        // 发布时间：OpenAI 风格 /models 的 `created`（epoch 秒）
        release_ts: m
            .get("created")
            .and_then(|v| v.as_u64())
            .filter(|v| *v > 0)
            .map(|s| s * 1000),
        ..Default::default()
    };
    if let Some(t) = tiers {
        rec.tiers = t;
        // provider 官方能力字段 = 最高置信来源（蒸馏锚定带宽最紧）
        rec.tiers_source = TiersSource::Provider;
    }
    Some(rec)
}

/// GET {base_url}/models -> Vec<ModelRecord> (source = Remote).
/// Records inherit nothing: caller merges by provider sample ids for keys.
pub async fn fetch_provider_models(
    client: &reqwest::Client,
    base_url: &str,
    key: Option<&str>,
) -> anyhow::Result<Vec<ModelRecord>> {
    let url = format!("{}/models", base_url.trim_end_matches('/'));
    let mut req = client.get(&url).timeout(Duration::from_secs(6));
    if let Some(k) = key.filter(|k| !k.is_empty()) {
        req = req.bearer_auth(k);
    }
    let resp = req.send().await?.error_for_status()?;
    let v: Value = resp.json().await?;
    let Some(data) = v.get("data").and_then(|d| d.as_array()) else {
        return Ok(Vec::new());
    };
    let provider = provider_name_of(base_url);
    let mut out = Vec::new();
    for m in data {
        if let Some(rec) = record_from_openai_model(&provider, base_url, key, m) {
            out.push(rec);
        }
    }
    Ok(out)
}

pub fn cache_path(dir: &str) -> PathBuf {
    evolve_core::config::state_file(dir, "remote-models.json")
}

const CACHE_TTL_MS: u64 = 60 * 60 * 1000;

/// Blocking discovery with a 1h cache file. Called from build_state
/// (outside any runtime). Falls back to stale cache on fetch failure.
pub fn discover_remote_blocking(
    records: &[ModelRecord],
    cache_dir: &str,
    self_origin: Option<&str>,
) -> Vec<ModelRecord> {
    discover_remote_blocking_ex(records, cache_dir, self_origin, false)
}

/// `force` = 忽略 1h 缓存直接重取（每小时刷新 tick / 新增 provider 时用）。
/// 否则缓存内**没有覆盖当前全部 provider 分组**时也强制重取——否则新增
/// provider（如默认接入的 openrouter）要等缓存过期（最长 1h）才出现模型
pub fn discover_remote_blocking_ex(
    records: &[ModelRecord],
    cache_dir: &str,
    self_origin: Option<&str>,
    force: bool,
) -> Vec<ModelRecord> {
    let path = cache_path(cache_dir);
    let fresh = std::fs::metadata(&path)
        .ok()
        .and_then(|m| m.modified().ok())
        .and_then(|t| t.elapsed().ok())
        .map(|e| (e.as_millis() as u64) < CACHE_TTL_MS)
        .unwrap_or(false);
    if fresh && !force
        && let Ok(text) = std::fs::read_to_string(&path)
            && let Ok(v) = serde_json::from_str::<Value>(&text) {
                let cached = parse_cached(&v);
                // 缓存必须覆盖当前所有 provider 分组（新增 provider → 缓存缺失 → 重取）
                let groups = provider_groups_ex(records, self_origin);
                let covered = groups.iter().all(|g| {
                    cached.iter().any(|m| m.base_url == g.base_url)
                        // 请求过但返回空清单的端点也算覆盖（避免每次重启都重试）
                        || v.get("groups").and_then(|x| x.as_array())
                            .map(|a| a.iter().any(|g2| g2.as_str() == Some(g.base_url.as_str())))
                            .unwrap_or(false)
                });
                if covered {
                    return cached;
                }
                tracing::info!("remote cache missing a provider group — refetching");
            }
    let client = match reqwest::blocking::Client::builder()
        .connect_timeout(Duration::from_secs(3))
        .timeout(Duration::from_secs(5))
        .user_agent("evolve/0.1")
        .build()
    {
        Ok(c) => c,
        Err(_) => return Vec::new(),
    };
    let mut all = Vec::new();
    let mut errors = 0;
    let mut tried: Vec<String> = Vec::new();
    let existing: std::collections::HashSet<(String, String)> = records
        .iter()
        .map(|r| (r.base_url.clone(), r.upstream_model.clone()))
        .collect();
    let mut group_meta: HashMap<String, String> = HashMap::new();
    for g in provider_groups_ex(records, self_origin) {
        group_meta.insert(g.base_url.clone(), g.provider_hint.clone());
        let base_url = g.base_url.clone();
        match fetch_provider_models_blocking(&client, &base_url, g.key.as_deref()) {
            Ok(mut models) => {
                tried.push(base_url.clone());
                let before = models.len();
                let hint = group_meta.get(&base_url).cloned().unwrap_or_default();
                // drop models the catalog already knows at this base_url
                models.retain(|m| {
                    !existing.contains(&(m.base_url.clone(), m.upstream_model.clone()))
                });
                for m in &mut models {
                    if !hint.is_empty() {
                        // id 用 hint 前缀（与已发现条目同族），保持目录整洁
                        m.id = format!("{hint}/{}", m.upstream_model);
                        m.provider = hint.clone();
                    }
                }
                let dropped = before - models.len();
                tracing::info!(provider = %base_url, fetched = before, deduped = dropped, kept = models.len(), "remote fetch done");
                all.append(&mut models);
            }
            Err(e) => {
                errors += 1;
                tried.push(base_url.clone());
                tracing::debug!(provider = %base_url, error = %e, "remote /models unavailable");
            }
        }
    }
    if errors as usize == provider_groups_ex(records, self_origin).len() && all.is_empty() {
        // every provider unreachable: keep stale cache usable next time
        return Vec::new();
    }
    // 部分失败保护：全空（如某 group 200 但空 data + 其它网络错误）不覆盖
    // last-good 缓存——否则 1h TTL 内持续返回空，重启后远程模型全消失
    if all.is_empty() {
        tracing::warn!("remote fetch yielded no models; keeping last-good cache");
        return parse_cached(
            &serde_json::from_str::<Value>(&std::fs::read_to_string(&path).unwrap_or_default())
                .unwrap_or(Value::Null),
        );
    }
    // groups 记录"本次尝试过的端点"：连空的端点也算已覆盖，避免缓存覆盖判定
    // 反复认为缺失而每次重启都重取（配合上面的 covered 检查）
    let snap = serde_json::json!({ "models": all, "groups": tried });
    if let Some(parent) = path.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    evolve_core::config::ensure_parent(&path);
    let _ = std::fs::write(&path, serde_json::to_string(&snap).unwrap_or_default());
    tracing::info!(count = all.len(), "remote model discovery complete");
    all
}

/// Parse a cached snapshot {"models": [ModelRecord...]}.
pub fn parse_cached(v: &Value) -> Vec<ModelRecord> {
    serde_json::from_value::<Vec<ModelRecord>>(v.get("models").cloned().unwrap_or(Value::Null))
        .unwrap_or_default()
}

fn fetch_provider_models_blocking(
    client: &reqwest::blocking::Client,
    base_url: &str,
    key: Option<&str>,
) -> anyhow::Result<Vec<ModelRecord>> {
    let url = format!("{}/models", base_url.trim_end_matches('/'));
    let mut req = client.get(&url);
    if let Some(k) = key.filter(|k| !k.is_empty()) {
        req = req.bearer_auth(k);
    }
    let resp = req.send()?.error_for_status()?;
    let v: Value = resp.json()?;
    let Some(data) = v.get("data").and_then(|d| d.as_array()) else {
        return Ok(Vec::new());
    };
    let provider = provider_name_of(base_url);
    let mut out = Vec::new();
    for m in data {
        if let Some(rec) = record_from_openai_model(&provider, base_url, key, m) {
            out.push(rec);
        }
    }
    Ok(out)
}

#[allow(dead_code)]
fn now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 1h 缓存的覆盖判定：缓存里的模型 + `groups`（尝试过的端点，含返回空清单的）
    /// 覆盖了当前全部 provider 分组 → 直接命中缓存，**不发网络请求**。
    /// 否则新增 provider（默认接入场景）要等缓存过期才出现模型。
    #[test]
    fn cache_covering_all_groups_is_used_without_network() {
        let dir = std::env::temp_dir().join(format!("er-remote-cache-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let mk = |id: &str, base: &str| ModelRecord {
            id: id.into(),
            provider: "p".into(),
            base_url: base.into(),
            upstream_model: id.into(),
            context_window: Some(1000),
            ..Default::default()
        };
        let a = "http://127.0.0.1:1/v1";
        // b 不可路由：若被当成"未覆盖"就会发起连接（≥3s 超时）——用耗时证明走缓存
        let b = "http://10.255.255.1/v1";
        let snap = serde_json::json!({
            "models": [mk("a/x", a)],
            "groups": [a, b],
        });
        let cache = cache_path(&dir.to_string_lossy());
        evolve_core::config::ensure_parent(&cache);
        std::fs::write(&cache, serde_json::to_string(&snap).unwrap()).unwrap();
        let started = std::time::Instant::now();
        let got = discover_remote_blocking_ex(&[mk("a/x", a), mk("b/y", b)], &dir.to_string_lossy(), None, false);
        assert_eq!(got.len(), 1, "cached snapshot returned as-is");
        assert!(
            started.elapsed() < std::time::Duration::from_secs(1),
            "must not hit the network when the cache covers every group (took {:?})",
            started.elapsed()
        );
    }

    /// OpenRouter 风格 /models：pricing 是「每 token 美元」字符串 → 目录口径
    /// 每 1M tokens；`:free` 变体定价 0 且被 is_free 识别；能力字段进 tiers。
    #[test]
    fn openrouter_payload_maps_pricing_and_capabilities() {
        let v: Value = serde_json::from_str(r#"{
            "id": "openai/gpt-5.4",
            "created": 1772734352,
            "context_length": 1050000,
            "pricing": {"prompt": "0.0000025", "completion": "0.000015"},
            "supported_parameters": ["tools", "temperature"],
            "architecture": {"input_modalities": ["text", "image"]},
            "reasoning": {"mandatory": false},
            "top_provider": {"max_completion_tokens": 128000}
        }"#).unwrap();
        let rec = record_from_openai_model("openrouter", "https://openrouter.ai/api/v1", Some("k"), &v).unwrap();
        assert_eq!(rec.id, "openrouter/openai/gpt-5.4");
        assert_eq!(rec.provider, "openrouter");
        assert_eq!(rec.currency, "USD");
        let cost = rec.cost.expect("pricing parsed");
        assert!((cost.input - 2.5).abs() < 1e-4, "per-1M input: {}", cost.input);
        assert!((cost.output - 15.0).abs() < 1e-4, "per-1M output: {}", cost.output);
        assert_eq!(rec.context_window, Some(1_050_000));
        assert!(rec.tiers.vision > 0.5, "image modality → vision tier");
        assert!(rec.tiers.agentic > 0.5, "tools supported → agentic tier");
        assert_eq!(rec.release_ts, Some(1_772_734_352_000));
        assert!(!rec.is_free());
    }

    /// 回归（2026-10 审查）：OpenRouter 用 `-1` 表示"动态定价"（实测 7 条）。
    /// 不得解析成负价——否则 min_price 为负，全池成本因子被 clamp 成 0。
    #[test]
    fn negative_pricing_is_treated_as_unknown_cost() {
        let v: Value = serde_json::from_str(r#"{
            "id": "openrouter/auto",
            "context_length": 2000000,
            "pricing": {"prompt": "-1", "completion": "-1"}
        }"#).unwrap();
        let rec = record_from_openai_model("openrouter", "https://openrouter.ai/api/v1", None, &v).unwrap();
        assert!(rec.cost.is_none(), "negative sentinel must be unknown cost, got {:?}", rec.cost);
        assert!(!rec.is_free(), "-1 is not free either");
    }

    /// OpenRouter 免费变体：`:free` 后缀 + 零定价 → 偏好用免费模型
    #[test]
    fn openrouter_free_variant_is_detected() {
        let v: Value = serde_json::from_str(r#"{
            "id": "nvidia/nemotron-3.5-super-vl:free",
            "context_length": 1000000,
            "pricing": {"prompt": "0", "completion": "0"}
        }"#).unwrap();
        let rec = record_from_openai_model("openrouter", "https://openrouter.ai/api/v1", None, &v).unwrap();
        assert!(rec.is_free(), ":free variant must be free");
        assert_eq!(rec.cost.map(|c| (c.input, c.output)), Some((0.0, 0.0)));
    }

    /// 其它 provider（如 zen）的 /models 没有 pricing/能力字段：
    /// cost 保持 None、tiers 保持默认——不得改变既有路由行为
    #[test]
    fn plain_openai_payload_keeps_previous_behaviour() {
        let v: Value = serde_json::from_str(r#"{"id":"glm-5.3","created":1}"#).unwrap();
        let rec = record_from_openai_model("opencode-go", "https://opencode.ai/zen/go/v1", None, &v).unwrap();
        assert!(rec.cost.is_none(), "no pricing in payload → unknown cost");
        assert_eq!(rec.tiers, Tiers::default());
        assert!(!rec.is_free());
    }

    #[test]
    fn provider_groups_dedupes_by_base_and_key() {
        let recs = vec![
            ModelRecord { id: "a/x".into(), base_url: "http://p1/v1".into(),
                          keys: vec![KeySlot { label: "k".into(), value: "k1".into() }],
                          ..Default::default() },
            ModelRecord { id: "a/y".into(), base_url: "http://p1/v1".into(),
                          keys: vec![KeySlot { label: "k".into(), value: "k1".into() }],
                          ..Default::default() },
        ];
        let groups = provider_groups(&recs);
        assert_eq!(groups.len(), 1, "same base+key = one fetch");
        assert_eq!(groups.len(), 1);
    }
}

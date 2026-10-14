//! models.dev reference metadata (decision record #22: reference ONLY).
//! Fills *missing* metadata (context window, cost, release date) for catalog
//! entries; never overrides user or agent-config values. Cached with 24h TTL
//! in the data dir; fetching happens in the background after startup.
//!
//! 2026-10 匹配修正（实测开启开关后 0 命中）：我们与 models.dev 的
//! provider/模型命名多口径不一致（`MiniMax` vs `minimax-coding-plan`、
//! `doubao-lite-128k-240428` vs `doubao-seed-2.0-lite`、`glm-5.3-flashx` vs
//! `glm-5.3-flash`）→ 精确匹配命中率极低。现采用：
//!  ① provider（含别名）精确键 ② canon 规范化精确 ③ canon 前缀包含（长度差 ≤6）
//! provider 不必同名（用户裁决）：全局命中即可；**同一模型被多家转售时取最早
//! 发布日**（最早那条才是模型真正的发布时间）。

use evolve_core::types::{Cost, ModelRecord};
use serde_json::Value;
use std::path::PathBuf;

pub const API_URL: &str = "https://models.dev/api.json";
pub const TTL_MS: u64 = 24 * 60 * 60 * 1000;

fn cache_path(dir: &str) -> PathBuf {
    let home = std::env::var("HOME").map(PathBuf::from).unwrap_or_default();
    let base = if dir.starts_with("~/") {
        home.join(dir.trim_start_matches("~/"))
    } else {
        PathBuf::from(dir)
    };
    base.join("modelsdev.json")
}

pub fn load_fresh(dir: &str) -> Option<Value> {
    let path = cache_path(dir);
    let meta = std::fs::metadata(&path).ok()?;
    let age = meta.modified().ok()?.elapsed().ok()?;
    if age.as_millis() as u64 > TTL_MS {
        return None;
    }
    let text = std::fs::read_to_string(&path).ok()?;
    serde_json::from_str(&text).ok()
}

pub fn spawn_refresh(dir: String) {
    // 无 tokio runtime（同步 build_state / CLI 路径）时静默跳过——
    // 网关把刷新挂在 start_background（runtime 内）执行
    if tokio::runtime::Handle::try_current().is_err() {
        return;
    }
    tokio::spawn(async move {
        let client = match reqwest::Client::builder()
            .timeout(std::time::Duration::from_secs(8))
            .user_agent("evolve/0.1")
            .build()
        {
            Ok(c) => c,
            Err(_) => return,
        };
        match client.get(API_URL).send().await {
            Ok(resp) if resp.status().is_success() => match resp.bytes().await {
                Ok(bytes) => {
                    let path = cache_path(&dir);
                    if let Some(parent) = path.parent() {
                        let _ = std::fs::create_dir_all(parent);
                    }
                    if std::fs::write(&path, &bytes).is_ok() {
                        tracing::info!(bytes = bytes.len(), "models.dev reference cached");
                    }
                }
                Err(e) => tracing::debug!(error = %e, "models.dev fetch body failed"),
            },
            Ok(resp) => tracing::debug!(status = %resp.status(), "models.dev fetch rejected"),
            Err(e) => tracing::debug!(error = %e, "models.dev fetch failed"),
        }
    });
}

/// Enrich missing metadata only. Never touches id/base_url/keys/protocol.
pub fn enrich(records: &mut [ModelRecord], api: &Value) {
    // 预建 canon 索引（一次性）：5.6MB / 万级条目下避免每个模型都遍历全库
    let index = build_canon_index(api);
    for r in records.iter_mut() {
        let model_entry = find_model_indexed(api, &index, &r.provider, &r.upstream_model);
        let Some(m) = model_entry else { continue };
        let mut touched = false;
        if r.context_window.is_none()
            && let Some(w) = m.get("limit").and_then(|l| l.get("context")).and_then(|v| v.as_u64()) {
                r.context_window = Some(w);
                touched = true;
            }
        if r.cost.is_none()
            && let Some(cost) = m.get("cost").and_then(|c| {
                let input = c.get("input").and_then(|v| v.as_f64())? as f32;
                let output = c.get("output").and_then(|v| v.as_f64())? as f32;
                Some(Cost { input, output })
            }) {
                r.cost = Some(cost);
                touched = true;
            }
        // 发布时间（三级来源链之 ②）：provider /models 的 created 优先（已在
        // discovery 层写入），此处补 models.dev 的 release_date
        if r.release_ts.is_none()
            && let Some(ts) = m.get("release_date").and_then(|v| v.as_str()).and_then(parse_release_date) {
                r.release_ts = Some(ts);
                touched = true;
            }
        if touched {
            r.source_note = Some(match r.source_note.take() {
                Some(n) => format!("{n} +models.dev"),
                None => "models.dev".into(),
            });
        }
    }
}

/// models.dev `release_date`（"YYYY-MM-DD" / "YYYY-MM"）→ epoch ms
fn parse_release_date(s: &str) -> Option<u64> {
    let mut it = s.split('-');
    let y: i64 = it.next()?.parse().ok()?;
    let m: i64 = it.next().and_then(|x| x.parse().ok()).unwrap_or(1);
    let d: i64 = it.next().and_then(|x| x.parse().ok()).unwrap_or(1);
    if !(1..=12).contains(&m) || !(1..=31).contains(&d) {
        return None;
    }
    let y2 = if m <= 2 { y - 1 } else { y };
    let era = if y2 >= 0 { y2 } else { y2 - 399 } / 400;
    let yoe = y2 - era * 400;
    let mp = (m + 9) % 12;
    let doy = (153 * mp + 2) / 5 + d - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    Some(((era * 146_097 + doe - 719_468) * 86_400) as u64 * 1000)
}

/// 归一化：小写 + 去非字母数字（"MiniMax-M3.1" → "minimaxm31"）
fn norm(s: &str) -> String {
    s.chars().filter(|c| c.is_ascii_alphanumeric()).collect::<String>().to_lowercase()
}

/// 规范化：去 provider 前缀、日期段（6/8 位数字）、常见后缀后归一化
/// ——用于"同一模型不同写法"的匹配（flashx vs flash、带日期 vs 不带）
fn canon(s: &str) -> String {
    let mut t = s.to_lowercase();
    if let Some((_, tail)) = t.rsplit_once('/') {
        t = tail.to_string();
    }
    let parts: Vec<&str> = t.split(['-', '_', '.']).collect();
    let kept: Vec<&str> = parts
        .into_iter()
        .filter(|p| {
            let d = p.trim_start_matches('v');
            !(d.len() == 6 && d.chars().all(|c| c.is_ascii_digit()))
                && !(d.len() == 8 && d.chars().all(|c| c.is_ascii_digit()))
        })
        .collect();
    let mut t = kept.join("-");
    for suffix in ["-latest", "-preview", "-exp", "-free", "-turbo", "-highspeed", "-vision", "-thinking"] {
        if let Some(stripped) = t.strip_suffix(suffix) {
            t = stripped.to_string();
        }
    }
    norm(&t)
}

/// provider 别名（我们的 provider 名 → models.dev provider id 候选，按优先级）。
/// provider 同名只是"最优"，不是必要条件（未命中即走全局 canon 匹配）
fn provider_candidates(provider: &str) -> Vec<String> {
    let p = provider.to_lowercase();
    let mut out: Vec<String> = Vec::new();
    let push = |out: &mut Vec<String>, v: &str| {
        if !out.iter().any(|x| x == v) {
            out.push(v.to_string());
        }
    };
    if p.starts_with("minimax") {
        push(&mut out, "minimax-coding-plan");
        push(&mut out, "minimax-cn-coding-plan");
        push(&mut out, "minimax");
    } else if p.starts_with("opencode") {
        push(&mut out, "opencode-go");
        push(&mut out, "opencode");
    } else if p.starts_with("zhipuai") {
        push(&mut out, "zhipuai-coding-plan");
        push(&mut out, "zhipuai");
    } else if p.starts_with("zai") {
        push(&mut out, "zai-coding-plan");
        push(&mut out, "zai");
    } else if p.starts_with("volces") || p.starts_with("volcengine") {
        push(&mut out, "volcengine-coding-plan");
        push(&mut out, "volcengine");
    } else if p.starts_with("deepseek") {
        push(&mut out, "deepseek");
    } else if p.starts_with("moonshot") || p.starts_with("kimi") {
        push(&mut out, "moonshotai-cn");
        push(&mut out, "moonshotai");
    }
    push(&mut out, provider);
    out
}

/// 条目的 release_date（epoch ms；缺失视为"较新"，让有日期的候选优先）
fn entry_release_ts(v: &Value) -> u64 {
    v.get("release_date")
        .and_then(|x| x.as_str())
        .and_then(parse_release_date)
        .unwrap_or(u64::MAX)
}

/// canon → 条目索引（enrich 前构建一次）。
/// **多家转售同名模型时取最早发布日**（用户裁决）——provider 不是必要条件。
fn build_canon_index(api: &Value) -> std::collections::HashMap<String, &Value> {
    let mut idx: std::collections::HashMap<String, &Value> = std::collections::HashMap::new();
    if let Some(obj) = api.as_object() {
        for p in obj.values() {
            if let Some(ms) = p.get("models").and_then(|m| m.as_object()) {
                for (k, v) in ms.iter() {
                    let c = canon(k);
                    if c.is_empty() {
                        continue;
                    }
                    match idx.get(&c) {
                        Some(existing) if entry_release_ts(existing) <= entry_release_ts(v) => {}
                        _ => {
                            idx.insert(c, v);
                        }
                    }
                }
            }
        }
    }
    idx
}

/// 索引化匹配：① provider（含别名）精确键 → ② canon 索引精确
/// → ③ canon 前缀包含（长度差 ≤6；多命中先取最具体，长度并列取最早发布日）
fn find_model_indexed<'a>(
    api: &'a Value,
    index: &std::collections::HashMap<String, &'a Value>,
    provider: &str,
    model_id: &str,
) -> Option<&'a Value> {
    if let Some(obj) = api.as_object() {
        for cand in provider_candidates(provider) {
            if let Some(ms) = obj.get(&cand).and_then(|p| p.get("models")).and_then(|m| m.as_object())
                && let Some(hit) = ms.get(model_id) {
                    return Some(hit);
                }
        }
    }
    let target = canon(model_id);
    if target.is_empty() {
        return None;
    }
    if let Some(hit) = index.get(&target) {
        return Some(hit);
    }
    let mut best: Option<(usize, u64, &'a Value)> = None;
    for (ck, v) in index.iter() {
        let (long, short) = if ck.len() >= target.len() {
            (ck.as_str(), target.as_str())
        } else {
            (target.as_str(), ck.as_str())
        };
        if long.len() - short.len() <= 6 && long.starts_with(short) {
            let cand = (ck.len(), entry_release_ts(v), *v);
            let better = match best {
                None => true,
                Some((bl, bt, _)) => cand.0 > bl || (cand.0 == bl && cand.1 < bt),
            };
            if better {
                best = Some(cand);
            }
        }
    }
    best.map(|(_, _, v)| v)
}

/// 查 models.dev 条目（测试/工具用；enrich 走 find_model_indexed 复用索引）
#[cfg(test)]
fn find_model<'a>(api: &'a Value, provider: &str, model_id: &str) -> Option<&'a Value> {
    let index = build_canon_index(api);
    find_model_indexed(api, &index, provider, model_id)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn fills_missing_only() {
        let api = json!({
            "zhipu": {
                "models": {
                    "glm-5.3": {"limit": {"context": 200_000},
                                "cost": {"input": 1.0, "output": 4.0}}
                }
            }
        });
        let mut records = vec![
            ModelRecord { id: "zhipu/glm-5.3".into(), provider: "zhipu".into(),
                          upstream_model: "glm-5.3".into(), ..Default::default() },
            ModelRecord { id: "zhipu/glm-x".into(), provider: "zhipu".into(),
                          upstream_model: "glm-x".into(),
                          context_window: Some(99_000), ..Default::default() },
        ];
        enrich(&mut records, &api);
        assert_eq!(records[0].context_window, Some(200_000));
        assert_eq!(records[0].cost.unwrap().input, 1.0);
        // never overrides present values
        assert_eq!(records[1].context_window, Some(99_000));
        assert!(records[1].cost.is_none());
    }

    #[test]
    fn enriches_release_date() {
        let api = json!({"zhipu": {"models": {"glm-5.3": {"release_date": "2025-09-01"}}}});
        let mut records = vec![ModelRecord {
            id: "zhipu/glm-5.3".into(),
            provider: "zhipu".into(),
            upstream_model: "glm-5.3".into(),
            ..Default::default()
        }];
        enrich(&mut records, &api);
        let ts = records[0].release_ts.expect("release date filled");
        assert!(ts > 0);
        // 已有值时不被覆盖
        let mut keep = vec![ModelRecord {
            id: "zhipu/glm-5.3".into(),
            provider: "zhipu".into(),
            upstream_model: "glm-5.3".into(),
            release_ts: Some(42),
            ..Default::default()
        }];
        enrich(&mut keep, &api);
        assert_eq!(keep[0].release_ts, Some(42));
    }

    #[test]
    fn matches_across_provider_and_suffix_differences() {
        let api = json!({
            "volcengine-coding-plan": {"models": {"glm-5.3-flash": {"release_date": "2025-10-01"}}},
            "minimax-coding-plan": {"models": {"MiniMax-M3.1-Flash-Preview": {"release_date": "2026-01-01"}}},
            "zhipuai-coding-plan": {"models": {"glm-5.3-flash": {"release_date": "2025-09-01"}}},
            "opencode-go": {"models": {"deepseek-v4-pro": {"release_date": "2026-02-01"}}},
        });
        // ① provider 同名 + 精确键
        assert!(find_model(&api, "volcengine-coding-plan", "glm-5.3-flash").is_some());
        // ② provider 别名（MiniMax → minimax-coding-plan）
        assert!(find_model(&api, "MiniMax", "MiniMax-M3.1-Flash-Preview").is_some());
        // ③ provider 不匹配（opencode-go-github → opencode-go 别名）
        assert!(find_model(&api, "opencode-go-github", "deepseek-v4-pro").is_some());
        // ④ 变体名（glm-5.3-flashx → canon 前缀匹配）
        assert!(find_model(&api, "zhipuai-coding-plan", "glm-5.3-flashx").is_some());
        // ⑤ 旧版带日期名不会误配到无关模型
        assert!(find_model(&api, "volcengine-coding-plan", "doubao-lite-128k-240428").is_none());
    }

    #[test]
    fn duplicate_keys_pick_earliest_release() {
        // 同一模型被三家列出 → 取最早发布日（原厂发布时间）
        let api = json!({
            "reseller-a": {"models": {"glm-5.3-flash": {"release_date": "2026-02-01"}}},
            "reseller-b": {"models": {"glm-5.3-flash": {"release_date": "2025-08-20"}}},
            "reseller-c": {"models": {"glm-5.3-flash": {"release_date": "2026-01-05"}}},
        });
        let m = find_model(&api, "unknown-provider", "glm-5.3-flash").unwrap();
        assert_eq!(m.get("release_date").and_then(|v| v.as_str()), Some("2025-08-20"));
    }

    #[test]
    fn canon_normalizes_variants() {
        assert_eq!(canon("MiniMax-M3.1"), "minimaxm31");
        // "flashx" 变体：canon 不相等，但以前缀关系命中（长度差 ≤6 守卫）
        let a = canon("vendor/glm-5.3-flashx");
        let b = canon("glm-5.3-flash");
        assert!(a.starts_with(&b) && a.len() - b.len() <= 6, "{a} vs {b}");
        assert_eq!(canon("doubao-seed-1-6-250615"), canon("doubao-seed-1-6"));
    }
}

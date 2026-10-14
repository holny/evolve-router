//! Quota window ledger: learns per-model token/request windows from
//! upstream rate-limit headers on successful responses, plus explicit
//! quota-error cooldowns. Feeds the engine's pre-check (never route a
//! request larger than remaining budget) and the dashboard quota card.
//!
//! Reference-only sources (decision record #22): header facts only, never
//! user config.

use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::HashMap;
use std::sync::Mutex;

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct WindowState {
    pub scope: String,
    pub remaining: Option<u64>,
    pub limit: Option<u64>,
    pub reset_epoch_ms: Option<u64>,
    pub updated_epoch_ms: u64,
}

/// (ts, 入, 缓存读, 出) 单条用量记录
pub type UsageRecord = (u64, f64, f64, f64);
type UsageMap = HashMap<String, Vec<UsageRecord>>;

pub struct QuotaLedger {
    inner: Mutex<HashMap<String, Vec<WindowState>>>,
    /// 订阅方案用量账本（模型 → token 记录，滚动保留 24h）
    plan_usage: Mutex<UsageMap>,
    /// provider 测的实时配额（plan_key + window_scope → 最近一次响应头里的剩余/限额）。
    /// 面板配额展示以此为权威源——本地按 token 累加的估算容易与 provider 真实值漂移
    /// （错误响应消耗配额但不返回 usage / 计费精度差异 / 并发令牌预留等）
    observed_provider: Mutex<HashMap<String, ObservedProviderQuota>>,
    /// w2 provider 配额闸门（QUOTA_LIMITED 状态机）：任一窗满 → 关闸不参与路由；
    /// API 型由官方端点定时刷新发现恢复（兜底重试 1h）；无 API 型 5~10min 探测
    provider_gate: Mutex<HashMap<String, ProviderGateState>>,
}

/// provider 配额闸门状态（plan_key 粒度；进程内寿命）
#[derive(Debug, Clone)]
pub struct ProviderGateState {
    /// 官方端点可查询配额（minimax/zhipu/zai/opencode-go）——决定恢复路径
    pub api_backed: bool,
    /// 耗尽窗口列表（"5h"/"weekly"/"monthly"）
    pub windows_full: Vec<String>,
    /// 最长窗口的官方重置时刻（无则未知）
    pub reset_epoch_ms: Option<u64>,
    pub since_ms: u64,
    /// 下次探测允许时刻（epoch ms）
    pub probe_at_ms: u64,
    pub probe_failures: u32,
    /// 探测进行中（探测单在飞时闸门暂开）
    pub probing: bool,
}

impl ProviderGateState {
    pub const PROBE_BASE_MS: u64 = 5 * 60 * 1000;
    pub const PROBE_CAP_MS: u64 = 10 * 60 * 1000;
    /// API 型官方查询失败的兜底重试间隔（用户裁决：1h——官方窗口期短查询无意义）
    pub const API_RETRY_MS: u64 = 60 * 60 * 1000;

    pub fn backoff_ms(&self, now: u64) -> u64 {
        if self.api_backed {
            Self::API_RETRY_MS
        } else {
            (Self::PROBE_BASE_MS + self.probe_failures as u64 * 60_000).min(Self::PROBE_CAP_MS)
        }
        .saturating_sub(now.saturating_sub(self.probe_at_ms))
        .max(0)
    }
}

/// provider 响应头里的实时配额快照（每个 plan_key 一份；面板展示窗口维度）
#[derive(Debug, Clone, Default)]
pub struct ObservedProviderQuota {
    /// ("5h" / "7d" / "tokens" / ...) → 剩余 + 限额（令牌/请求，原始单位由 provider 决定）
    pub windows: HashMap<String, ObservedWindow>,
}

#[derive(Debug, Clone, Default)]
pub struct ObservedWindow {
    pub remaining: Option<u64>,
    pub limit: Option<u64>,
    pub reset_epoch_ms: Option<u64>,
    pub updated_epoch_ms: u64,
}

pub fn now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

impl QuotaLedger {
    /// w2 观察到的官方窗口 → gate 判定（API 型恢复路径）。
    /// 任一窗 remaining==0 → 关闸（windows_full 记录）；全部有余量 → 开闸（清状态）。
    pub fn gate_observe_windows(&self, provider: &str, windows: &[WindowState], now: u64) {
        if provider.is_empty() { return; }
        let Ok(mut m) = self.provider_gate.lock() else { return };
        let full: Vec<String> = windows.iter()
            .filter(|w| !matches!(w.scope.as_str(), "5h" | "weekly" | "monthly"))
            .filter(|w| w.remaining == Some(0))
            .map(|w| w.scope.clone())
            .collect();
        if full.is_empty() {
            if m.contains_key(provider) {
                tracing::info!(provider = %provider, "provider quota gate opened (official windows show remaining)");
                m.remove(provider);
            }
            return;
        }
        let reset = windows.iter().filter(|w| full.contains(&w.scope))
            .filter_map(|w| w.reset_epoch_ms).max();
        let e = m.entry(provider.to_string()).or_insert_with(|| ProviderGateState {
            api_backed: true,
            windows_full: full.clone(),
            reset_epoch_ms: reset,
            since_ms: now,
            probe_at_ms: 0,
            probe_failures: 0,
            probing: false,
        });
        e.api_backed = true;
        e.windows_full = full;
        if e.reset_epoch_ms.is_none() { e.reset_epoch_ms = reset; }
    }

    /// 上游 429/402 失败 → 关闸（API 型与无 API 型共用；probe 时间表随之重排）
    pub fn gate_mark_limited(&self, provider: &str, api_backed: bool, reset_epoch_ms: Option<u64>, now: u64) {
        if provider.is_empty() { return; }
        let Ok(mut m) = self.provider_gate.lock() else { return };
        let newly = !m.contains_key(provider);
        let e = m.entry(provider.to_string()).or_insert_with(|| ProviderGateState {
            api_backed,
            windows_full: vec![],
            reset_epoch_ms,
            since_ms: now,
            probe_at_ms: now + ProviderGateState::API_RETRY_MS,
            probe_failures: 0,
            probing: false,
        });
        if newly {
            tracing::warn!(provider = %provider, api = api_backed, "provider quota gate closed (upstream quota signal)");
        }
        e.api_backed = api_backed;
        if e.reset_epoch_ms.is_none() { e.reset_epoch_ms = reset_epoch_ms; }
        // 重排下次探测/重试：API 型 1h 兜底；本地无 API 型 5~10min 退避
        e.probe_at_ms = now + if api_backed { ProviderGateState::API_RETRY_MS } else { ProviderGateState::PROBE_BASE_MS };
        e.probing = false;
    }

    /// 闸门关闭的 provider 名列表（账号级）——relay 据此在 quota 视图里
    /// 把该账号的全部模型余量清零（引擎按模型硬滤，替换早前 plan_pressure=1.5
    /// 的粗粒度方案：同 plan_key 多账号不再同伴受罚）
    pub fn gate_blocked(&self) -> Vec<String> {
        let Ok(m) = self.provider_gate.lock() else { return Vec::new() };
        m.keys().cloned().collect()
    }

    pub fn gate_get(&self, provider: &str) -> Option<ProviderGateState> {
        self.provider_gate.lock().ok()?.get(provider).cloned()
    }

    /// 选一个进入探测的 provider（无 API 型）：探测窗口到 + 未在探测中
    pub fn gate_pick_probe(&self, now: u64) -> Option<String> {
        let mut m = self.provider_gate.lock().ok()?;
        let mut best: Option<(u64, String)> = None;
        for (k, e) in m.iter() {
            if e.api_backed || e.probing || now < e.probe_at_ms { continue; }
            if best.as_ref().map(|(t, _)| e.probe_at_ms < *t).unwrap_or(true) {
                best = Some((e.probe_at_ms, k.clone()));
            }
        }
        let (t, k) = best?;
        if let Some(e) = m.get_mut(&k) { e.probing = true; e.probe_at_ms = t; }
        tracing::info!(provider = %k, "quota gate: probe request allowed (real-state redetection)");
        Some(k)
    }

    /// 探测/路由成功：开闸（真实可用是权威反证）；API 型状态由下轮 watchdog 校准
    pub fn gate_probe_success(&self, provider: &str, now: u64) {
        let Ok(mut m) = self.provider_gate.lock() else { return };
        if m.remove(provider).is_some() {
            tracing::info!(provider = %provider, "provider quota gate opened (routed request succeeded)");
        }
        let _ = now;
    }

    /// 探测失败：回闸 + 退避重排（5min 起步，+1min/次，封顶 10min）
    pub fn gate_probe_fail(&self, provider: &str, now: u64) {
        let Ok(mut m) = self.provider_gate.lock() else { return };
        if let Some(e) = m.get_mut(provider) {
            e.probing = false;
            e.probe_failures = e.probe_failures.saturating_add(1);
            let backoff = if e.api_backed { ProviderGateState::API_RETRY_MS }
                else { (ProviderGateState::PROBE_BASE_MS + e.probe_failures as u64 * 60_000).min(ProviderGateState::PROBE_CAP_MS) };
            e.probe_at_ms = now + backoff;
            tracing::warn!(provider = %provider, backoff_ms = backoff, "quota probe failed, gate re-closed with backoff");
        }
    }

    pub fn new() -> Self {
        Self {
            inner: Mutex::new(HashMap::new()),
            plan_usage: Mutex::new(HashMap::new()),
            observed_provider: Mutex::new(HashMap::new()),
            provider_gate: Mutex::new(HashMap::new()),
        }
    }

    /// 记录 provider 实时配额（每次响应 parse_headers 后调用）。
    /// w2 键控 = provider 名（账号级：同 plan_key 多账号不互串，opencode 双账号
    /// 同槽互相覆盖的实证缺陷由此修复）；调用方传 record.provider。
    pub fn observe_provider(&self, provider: &str, windows: &[WindowState]) {
        if windows.is_empty() || provider.is_empty() { return; }
        let Ok(mut m) = self.observed_provider.lock() else { return };
        let slot = m.entry(provider.to_string()).or_default();
        for w in windows {
            if w.remaining.is_none() && w.limit.is_none() { continue; }
            slot.windows.insert(w.scope.clone(), ObservedWindow {
                remaining: w.remaining,
                limit: w.limit,
                reset_epoch_ms: w.reset_epoch_ms,
                updated_epoch_ms: w.updated_epoch_ms,
            });
        }
    }

    /// 取某 provider 某窗口的最新观察值与年龄。max_age_ms 默认 10 分钟（Provider
    /// 测的时效性窗口——超过则退化到本地估算）
    pub fn observed_window(&self, provider: &str, scope: &str, now_ms: u64, max_age_ms: u64) -> Option<ObservedWindow> {
        let m = self.observed_provider.lock().ok()?;
        let slot = m.get(provider)?;
        let w = slot.windows.get(scope)?.clone();
        if now_ms.saturating_sub(w.updated_epoch_ms) > max_age_ms { return None; }
        Some(w)
    }

    /// 记录一次订阅方案请求的 token 用量（24h 滚动保留）
    pub fn note_plan_usage(&self, model: &str, in_tok: f64, cached_tok: f64, out_tok: f64, now_ms: u64) {
        const RETAIN_MS: u64 = 24 * 3600 * 1000;
        let Ok(mut m) = self.plan_usage.lock() else { return };
        let e = m.entry(model.to_string()).or_default();
        e.push((now_ms, in_tok, cached_tok, out_tok));
        e.retain(|(ts, _, _, _)| now_ms.saturating_sub(*ts) <= RETAIN_MS);
    }

    /// 窗口内各模型 token 用量合计（默认 5h 窗口）
    pub fn plan_usage_sums(&self, now_ms: u64, window_ms: u64) -> HashMap<String, (f64, f64, f64)> {
        let mut out: HashMap<String, (f64, f64, f64)> = HashMap::new();
        let Ok(m) = self.plan_usage.lock() else { return out };
        for (model, recs) in m.iter() {
            let mut acc = (0.0f64, 0.0f64, 0.0f64);
            for (ts, i, c, o) in recs {
                if now_ms.saturating_sub(*ts) <= window_ms {
                    acc.0 += i;
                    acc.1 += c;
                    acc.2 += o;
                }
            }
            out.insert(model.clone(), acc);
        }
        out
    }

    /// Extract quota windows from response headers (any protocol family).
    pub fn parse_headers(headers: &http::HeaderMap) -> Vec<WindowState> {
        let mut out: Vec<WindowState> = Vec::new();
        let now = now_ms();
        let get = |name: &str| -> Option<u64> {
            headers.get(name).and_then(|v| v.to_str().ok()).and_then(|v| v.parse::<u64>().ok())
        };
        // anthropic unified windows（同 scope 多 header 合并：remaining/limit/reset 分开放）
        let mut anthro: std::collections::HashMap<&str, (Option<u64>, Option<u64>, Option<u64>)> = std::collections::HashMap::new();
        for (scope, tag) in [("5h", "anthropic-ratelimit-unified-5h-token"), ("7d", "anthropic-ratelimit-unified-7d-token")] {
            let entry = anthro.entry(scope).or_insert((None, None, None));
            if let Some(v) = get(&format!("{tag}-remaining")) { entry.0 = Some(v); }
            if let Some(v) = get(&format!("{tag}-limit"))     { entry.1 = Some(v); }
            if let Some(v) = headers.get(format!("{tag}-reset")).and_then(|v| v.to_str().ok()).and_then(parse_rfc3339_ms) { entry.2 = Some(v); }
        }
        for (scope, (remaining, limit, reset)) in anthro {
            if remaining.is_some() || limit.is_some() || reset.is_some() {
                out.push(WindowState {
                    scope: scope.into(),
                    remaining, limit,
                    reset_epoch_ms: reset,
                    updated_epoch_ms: now,
                });
            }
        }
        // openai token window
        if let Some(remaining) = get("x-ratelimit-remaining-tokens") {
            // reset-tokens 是相对时长（"6m0s"）——换算为绝对纪元，消费端按 epoch 比较
            let reset = headers
                .get("x-ratelimit-reset-tokens")
                .and_then(|v| v.to_str().ok())
                .and_then(parse_duration_ms)
                .map(|ms| now.saturating_add(ms));
            out.push(WindowState {
                scope: "tokens".into(),
                remaining: Some(remaining),
                limit: get("x-ratelimit-limit-tokens"),
                reset_epoch_ms: reset,
                updated_epoch_ms: now,
            });
        }
        // openai request window (tracked separately)
        if let Some(remaining) = get("x-ratelimit-remaining-requests") {
            out.push(WindowState {
                scope: "requests".into(),
                remaining: Some(remaining),
                limit: get("x-ratelimit-limit-requests"),
                reset_epoch_ms: None,
                updated_epoch_ms: now,
            });
        }
        // 通用 weekly / monthly 窗口（zhipu/volces/策略派等 namespace 的变体）——
        // 失败响应 429 常带这两个 scope 的 reset（用户裁决：三窗口隔离展示）。
        // 多种 header 命名：anthropic 风格 -token- / 通用裸名 / vendor 风格 ratelimit-。
        let generic_windows: &[(&str, &str, &[&str])] = &[
            ("weekly",  "x-ratelimit-weekly",  &["", "-token"]),
            ("monthly", "x-ratelimit-monthly", &["", "-token"]),
            ("weekly",  "ratelimit-weekly",    &["-token"]),
            ("monthly", "ratelimit-monthly",   &["-token"]),
        ];
        for (scope, prefix, suffixes) in generic_windows {
            // 探测多种 header 命名变体
            let mut remaining: Option<u64> = None;
            let mut limit: Option<u64> = None;
            let mut reset_raw: Option<String> = None;
            for suf in suffixes.iter() {
                if remaining.is_none() {
                    remaining = headers.get(format!("{prefix}{suf}-remaining"))
                        .and_then(|v| v.to_str().ok())
                        .and_then(|v| v.parse::<u64>().ok());
                }
                if limit.is_none() {
                    limit = headers.get(format!("{prefix}{suf}-limit"))
                        .and_then(|v| v.to_str().ok())
                        .and_then(|v| v.parse::<u64>().ok());
                }
                if reset_raw.is_none() {
                    reset_raw = headers.get(format!("{prefix}{suf}-reset"))
                        .and_then(|v| v.to_str().ok().map(|s| s.to_string()));
                }
                if remaining.is_some() || limit.is_some() || reset_raw.is_some() { break; }
            }
            if remaining.is_none() && limit.is_none() && reset_raw.is_none() { continue; }
            // reset 三态：RFC3339 / 绝对 epoch 秒数（>10 年 = >315360000）/ 相对 now+秒
            let reset_epoch_ms = reset_raw.as_deref().and_then(|raw| {
                if let Some(ms) = parse_rfc3339_ms(raw) { return Some(ms); }
                raw.parse::<u64>().ok().map(|n| if n > 315_360_000 { n * 1000 } else { now.saturating_add(n * 1000) })
            });
            out.push(WindowState {
                scope: (*scope).into(),
                remaining,
                limit,
                reset_epoch_ms,
                updated_epoch_ms: now,
            });
        }
        out
    }

    pub fn observe(&self, health_id: &str, windows: Vec<WindowState>) {
        if windows.is_empty() {
            return;
        }
        let Ok(mut m) = self.inner.lock() else { return };
        let slot = m.entry(health_id.to_string()).or_default();
        for w in windows {
            if let Some(existing) = slot.iter_mut().find(|e| e.scope == w.scope) {
                *existing = w;
            } else {
                slot.push(w);
            }
        }
    }

    /// Model-level view: best remaining across all key slots (any key may
    /// still carry budget). `None` when nothing is known.
    pub fn model_remaining_tokens(&self, model_id: &str) -> Option<u64> {
        let m = self.inner.lock().ok()?;
        let now = now_ms();
        let mut best: Option<u64> = None;
        for (k, slots) in m.iter() {
            if health_model_id(k) != model_id {
                continue;
            }
            for w in slots {
                if w.scope == "requests" {
                    continue;
                }
                // expired window = budget refreshed; treat as unknown/ok
                if let Some(r) = w.reset_epoch_ms
                    && r <= now {
                        continue;
                    }
                if let Some(rem) = w.remaining {
                    // 跨 key 取 best（max）——"哪个 key 余量最多就用哪个"
                    best = Some(match best {
                        Some(b) => b.max(rem),
                        None => rem,
                    });
                }
            }
        }
        best
    }

    /// Bulk model view for the engine pre-check (one pass over the ledger).
    pub fn best_remaining_by_models(&self) -> std::collections::HashMap<String, u64> {
        let mut out = std::collections::HashMap::new();        let mut scoped: std::collections::HashMap<String, std::collections::HashMap<String, u64>> = std::collections::HashMap::new();
        let m = self.inner.lock();
        let Ok(m) = m else { return out };
        let now = now_ms();
        // stale-window guards: a window older than 24h is garbage; a
        // zero-remaining window with unknown reset only blocks for 30min
        // (otherwise a header-parse miss locks the model until restart)
        const MAX_WINDOW_AGE_MS: u64 = 24 * 3600 * 1000;
        const UNKNOWN_RESET_BLOCK_MS: u64 = 30 * 60 * 1000;
        for (k, slots) in m.iter() {
            let model_id = health_model_id(k);
            for w in slots {
                if w.scope == "requests" {
                    continue;
                }
                if now.saturating_sub(w.updated_epoch_ms) > MAX_WINDOW_AGE_MS {
                    continue;
                }
                match w.reset_epoch_ms {
                    Some(r) if r <= now => continue,
                    None if w.remaining == Some(0)
                        && now.saturating_sub(w.updated_epoch_ms) > UNKNOWN_RESET_BLOCK_MS =>
                    {
                        continue
                    }
                    _ => {}
                }
                if let Some(rem) = w.remaining {
                    // review#8 两级聚合：同 (model, scope) 跨 key 取 max（哪个
                    // key 余量多用哪个）；跨 scope 最终取 min——配额约束是
                    // "每个窗口都不能超"，任一窗口耗尽即紧张
                    let e = scoped
                        .entry(model_id.to_string())
                        .or_insert_with(HashMap::new)
                        .entry(w.scope.clone())
                        .or_insert(0);
                    *e = (*e).max(rem);
                }
            }
        }
        // 跨 scope 收敛为 model 级余量（min）
        for (model_id, scopes) in scoped {
            let tightest = scopes.values().copied().min().unwrap_or(0);
            out.insert(model_id, tightest);
        }
        out
    }

    pub fn snapshot(&self) -> Value {
        let m = self.inner.lock();
        match m {
            Ok(m) => serde_json::to_value(&*m).unwrap_or(serde_json::Value::Null),
            Err(_) => serde_json::Value::Null,
        }
    }
}

/// The model portion of a composite health id ("model\x1fN" -> "model").
pub fn health_model_id(health_id: &str) -> &str {
    health_id.split('\u{1f}').next().unwrap_or(health_id)
}

/// RFC3339 (Z suffix) -> epoch ms. Minimal parser: 2026-09-29T12:34:56Z.
pub fn parse_rfc3339_ms(s: &str) -> Option<u64> {
    // review#3：上游 reset 头完全受上游控制——短于 RFC3339 日期段（10 字节）
    // 直接放弃，不 panic（畸形值如 "bad"、"x" 等）
    if s.len() < 10 {
        return None;
    }
    // Tolerant RFC3339: accepts fractional seconds and +HH:MM/-HH:MM/Z offsets.
    // 偏移只会出现在时间部分（index ≥ 10）——从日期分隔符之后起找，
    // 否则 "2026-10-07" 的 '-' 命中 index 4，时区偏移被永久丢弃
    let (main, offset_ms) = match s[10..].find(['+', '-']).map(|p| p + 10) {
        Some(pos) => {
            let (m, off) = s.split_at(pos);
            // off: +HH:MM or -HH:MM
            let sign = if off.starts_with('-') { -1i64 } else { 1i64 };
            let h: i64 = off.get(1..3)?.parse().ok()?;
            let mi: i64 = off.get(4..6).unwrap_or("00").parse().ok()?;
            (m.to_string(), sign * (h * 3600 + mi * 60) * 1000)
        }
        _ => (s.to_string(), 0),
    };
    let bytes = main.as_bytes();
    // find 'Z' or strip fractional seconds before it
    let main = if bytes.len() > 19 && bytes[19] == b'Z' {
        main[..19].to_string()
    } else if bytes.len() > 19 && bytes[19] == b'.' {
        // fractional seconds: find terminator
        let z = main[19..].find(['Z', '+', '-'])? + 19;
        main[..z].to_string()
    } else {
        main
    };
    let bytes = main.as_bytes();
    if (bytes.len() < 19 || (bytes.len() == 20 && bytes[19] != b'Z'))
        && bytes.len() < 19 {
            return None;
        }
    let num = |a: usize, b: usize| main.get(a..b)?.parse::<i64>().ok();
    let year = num(0, 4)?;
    let month = num(5, 7)?;
    let day = num(8, 10)?;
    let hour = num(11, 13)?;
    let min = num(14, 16)?;
    let sec = num(17, 19)?;
    // days since epoch (civil algorithm)
    let days = |y: i64, m: i64, d: i64| -> i64 {
        let y = if m <= 2 { y - 1 } else { y };
        let era = y.div_euclid(400);
        let yoe = y - era * 400;
        let doy = (153 * (m + if m > 2 { -3 } else { 9 }) + 2) / 5 + d - 1;
        let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
        era * 146_097 + doe - 719_468
    };
    let epoch = days(year, month, day) * 86_400 + hour * 3600 + min * 60 + sec;
    let base = (epoch.max(0) as u64).saturating_mul(1000);
    Some(base.saturating_add_signed(-offset_ms))
}

/// OpenAI reset duration ("1s", "6m0s", "1h2m3s") -> ms.
fn parse_duration_ms(s: &str) -> Option<u64> {
    let mut total_ms = 0u64;
    let mut num = String::new();
    for c in s.chars() {
        if c.is_ascii_digit() {
            num.push(c);
        } else {
            let v: u64 = num.parse().ok()?;
            num.clear();
            total_ms += match c {
                's' => v * 1000,
                'm' => v * 60_000,
                'h' => v * 3_600_000,
                'd' => v * 86_400_000,
                _ => return None,
            };
        }
    }
    Some(total_ms)
}

impl Default for QuotaLedger {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use http::HeaderMap;

    fn hdr(pairs: &[(&str, &str)]) -> HeaderMap {
        let mut h = HeaderMap::new();
        for (k, v) in pairs {
            h.insert(
                http::HeaderName::from_bytes(k.as_bytes()).unwrap(),
                v.parse().unwrap(),
            );
        }
        h
    }

    #[test]
    fn parses_anthropic_unified_windows() {
        let h = hdr(&[
            ("anthropic-ratelimit-unified-5h-token-remaining", "45000"),
            ("anthropic-ratelimit-unified-5h-token-limit", "100000"),
            ("anthropic-ratelimit-unified-5h-token-reset", "2026-09-29T12:34:56Z"),
            ("anthropic-ratelimit-unified-7d-token-remaining", "900000"),
        ]);
        let w = QuotaLedger::parse_headers(&h);
        assert_eq!(w.len(), 2);
        let w5h = w.iter().find(|x| x.scope == "5h").expect("5h window");
        let w7d = w.iter().find(|x| x.scope == "7d").expect("7d window");
        assert_eq!(w5h.remaining, Some(45_000));
        assert!(w5h.reset_epoch_ms.is_some());
        assert_eq!(w7d.remaining, Some(900_000));
    }

    #[test]
    fn parses_openai_windows() {
        let h = hdr(&[
            ("x-ratelimit-remaining-tokens", "12000"),
            ("x-ratelimit-limit-tokens", "90000"),
            ("x-ratelimit-reset-tokens", "6m0s"),
            ("x-ratelimit-remaining-requests", "58"),
        ]);
        let w = QuotaLedger::parse_headers(&h);
        assert_eq!(w.len(), 2);
        assert_eq!(w[0].scope, "tokens");
        // 相对时长换算为绝对纪元（now + 6m），消费端按 epoch 比较
        let now = crate::quota::now_ms();
        match w[0].reset_epoch_ms {
            Some(t) => assert!(
                t >= now + 350_000 && t <= now + 370_000,
                "reset 应为 now+6m，实际 {t} (now={now})"
            ),
            None => panic!("reset_epoch_ms 应存在"),
        }
        assert_eq!(w[1].scope, "requests");
    }

    #[test]
    fn model_view_takes_best_key_and_ignores_expired() {
        let led = QuotaLedger::new();
        led.observe(
            "mini\u{1f}0",
            QuotaLedger::parse_headers(&hdr(&[("x-ratelimit-remaining-tokens", "1000")])),
        );
        led.observe(
            "mini\u{1f}1",
            QuotaLedger::parse_headers(&hdr(&[("x-ratelimit-remaining-tokens", "80000")])),
        );
        led.observe(
            "standard\u{1f}0",
            QuotaLedger::parse_headers(&hdr(&[("x-ratelimit-remaining-tokens", "50")])),
        );
        assert_eq!(led.model_remaining_tokens("mini"), Some(80_000));
        assert_eq!(led.model_remaining_tokens("standard"), Some(50));
        assert_eq!(led.model_remaining_tokens("frontier"), None);
    }
}


#[cfg(test)]
mod plan_usage_tests {
    use super::*;

    #[test]
    fn note_and_sum_roundtrip() {
        let l = QuotaLedger::new();
        let now = now_ms();
        l.note_plan_usage("m1", 13.0, 0.0, 10.0, now);
        let sums = l.plan_usage_sums(now, 5 * 3600 * 1000);
        assert_eq!(sums.get("m1"), Some(&(13.0, 0.0, 10.0)), "sums={:?}", sums);
    }
}

#[cfg(test)]
mod quota_observe_failure_tests {
    use super::*;
    use std::collections::HashMap;

    fn hdr(pairs: &[(&'static str, &'static str)]) -> http::HeaderMap {
        let mut h = http::HeaderMap::new();
        for (k, v) in pairs { h.insert(*k, v.parse().unwrap()); }
        h
    }

    #[test]
    fn parses_429_monthly_with_reset() {
        // anthropic 风格：失败响应也带 5h/weekly/monthly reset 头
        let h = hdr(&[
            ("anthropic-ratelimit-unified-5h-token-remaining", "0"),
            ("anthropic-ratelimit-unified-5h-token-reset", "2026-10-09T13:00:00Z"),
            ("anthropic-ratelimit-unified-7d-token-remaining", "1000"),
            ("anthropic-ratelimit-unified-7d-token-reset", "2026-10-14T00:00:00Z"),
            ("x-ratelimit-monthly-remaining", "0"),
            ("x-ratelimit-monthly-reset", "1736380800"), // 5h 后的 epoch seconds
        ]);
        let ws = QuotaLedger::parse_headers(&h);
        let mut by_scope: HashMap<String, WindowState> = ws.into_iter().map(|w| (w.scope.clone(), w)).collect();
        {
            let w5h = by_scope.remove("5h").unwrap();
            assert_eq!(w5h.remaining, Some(0));
            assert!(w5h.reset_epoch_ms.is_some());
        }
        assert_eq!(by_scope.remove("7d").unwrap().remaining, Some(1000));
        let monthly = by_scope.remove("monthly").unwrap();
        assert_eq!(monthly.remaining, Some(0));
        // 1736380800 epoch seconds (2025-01-09T16:00:00Z) → epoch ms
        assert_eq!(monthly.reset_epoch_ms, Some(1736380800 * 1000));
    }

    #[test]
    fn generic_weekly_monthly_parsed() {
        // 任意 vendor 通用 weekly / monthly 头
        let h = hdr(&[
            ("x-ratelimit-weekly-remaining", "100"),
            ("x-ratelimit-weekly-reset", "1736380800"),
            ("x-ratelimit-monthly-limit", "50000"),
            ("x-ratelimit-monthly-reset", "1736380800"),
        ]);
        let ws = QuotaLedger::parse_headers(&h);
        let weekly = ws.iter().find(|w| w.scope == "weekly").expect("weekly");
        assert_eq!(weekly.remaining, Some(100));
        assert_eq!(weekly.reset_epoch_ms, Some(1736380800 * 1000));
        let monthly = ws.iter().find(|w| w.scope == "monthly").expect("monthly");
        assert_eq!(monthly.limit, Some(50000));
    }
}

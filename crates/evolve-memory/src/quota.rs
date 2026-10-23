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

/// provider 级故障类型（非配额）：网络不可达 / 鉴权或区域被拒。
/// 与配额耗尽分开记账——恢复路径不同（配额等窗口重置，这两类靠探活回生）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ProviderIssue {
    Network,
    Auth,
}

impl ProviderIssue {
    pub fn label(&self) -> &'static str {
        match self {
            ProviderIssue::Network => "network unreachable",
            ProviderIssue::Auth => "auth/region rejected",
        }
    }
}

/// provider 配额闸门状态（plan_key 粒度；进程内寿命）
#[derive(Debug, Clone, Default)]
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
    /// 本次探测被领取的时刻（租约起点，2026-10-23）：probe 结果丢失时
    /// `probing` 会永久为 true → pick 永远跳过 → 闸门探测卡死。
    /// 租约超时（60s，> 探针 10s 超时）后允许重新领取。
    pub probe_picked_at_ms: u64,
    // ── 非配额故障（2026-10 新增：provider 级，连续失败达标才降级 + 定时探活）──
    /// 当前故障类型（None = 无）
    pub issue: Option<ProviderIssue>,
    /// 连续失败计数（成功一次清零；达阈值才真正降级）
    pub issue_fails: u32,
    /// 当前正在计数的是哪种故障（None = 尚未开始计数）——切换类型时重新计数，
    /// 但**未降级状态（issue=None）不能当成"切换了"**（否则计数永远回 0）
    pub issue_counting: Option<ProviderIssue>,
    /// 该故障的下次探测时刻（独立于配额探测调度）
    pub issue_probe_at_ms: u64,
    pub issue_probe_failures: u32,
    /// 本次探测被领取的时刻（租约起点，2026-10-23 实证）：probe 结果丢失时
    /// `probing` 会永久为 true → pick 永远跳过 → provider 卡死在降级态
    /// （实证：网络闪断后 3 个 provider next_probe_ms=0 却 8 分钟无人探）。
    /// 租约超时（60s，> 探针 10s 超时）后允许重新领取。
    pub issue_picked_at_ms: u64,
}

impl ProviderGateState {
    pub const PROBE_BASE_MS: u64 = 5 * 60 * 1000;
    pub const PROBE_CAP_MS: u64 = 10 * 60 * 1000;
    /// API 型官方查询失败的兜底重试间隔（用户裁决：1h——官方窗口期短查询无意义）
    pub const API_RETRY_MS: u64 = 60 * 60 * 1000;
    /// 非配额故障：连续失败达此数才降级（单次抖动不降级——用户裁决 2026-10-17）
    pub const ISSUE_MIN_FAILS: u32 = 3;
    /// 探活退避表（1m → 5m → 15m → 30m → 60m 封顶）
    pub const ISSUE_BACKOFF_MS: [u64; 5] = [
        60 * 1000,
        5 * 60 * 1000,
        15 * 60 * 1000,
        30 * 60 * 1000,
        60 * 60 * 1000,
    ];

    /// Network 类故障的探活退避封顶 2 分钟：瞬时抖动为主（睡眠唤醒/连接级），
    /// 端点恢复以秒计——30-60 分钟的流放曾把双订阅之一在兄弟账号全程 200 时
    /// 挡在池外（2026-10-20 实证）。鉴权/配额类保持原表。
    pub fn issue_backoff_for(issue: ProviderIssue, probe_failures: u32) -> u64 {
        let idx = (probe_failures as usize).min(Self::ISSUE_BACKOFF_MS.len() - 1);
        let ms = Self::ISSUE_BACKOFF_MS[idx];
        if issue == ProviderIssue::Network {
            ms.min(120 * 1000)
        } else {
            ms
        }
    }

    pub fn backoff_ms(&self, now: u64) -> u64 {
        // probe_at_ms 是绝对时刻（now + interval 各处赋值）→ 剩余等待 =
        // probe_at_ms − now。旧式 `interval − (now − probe_at_ms)` 只在
        // "probe_at 恰为上次赋值时刻 + interval" 时碰巧正确，重排后即错。
        let interval = if self.api_backed {
            Self::API_RETRY_MS
        } else {
            (Self::PROBE_BASE_MS + self.probe_failures as u64 * 60_000).min(Self::PROBE_CAP_MS)
        };
        self.probe_at_ms
            .saturating_sub(now)
            .min(interval)
            .max(0)
    }

    /// 非配额故障的退避剩余（0 = 可以探测了）
    pub fn issue_backoff_ms(&self, now: u64) -> u64 {
        self.issue_probe_at_ms.saturating_sub(now)
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
        // 2026-10-23 修复：此前排除的恰好是调用方（refresh_observed）实际发送的
        // 全部 scope（5h/weekly/monthly）→ 关闸分支不可达、"开闸"日志每轮刷屏
        let full: Vec<String> = windows.iter()
            .filter(|w| w.scope != "requests")
            .filter(|w| w.remaining == Some(0))
            .map(|w| w.scope.clone())
            .collect();
        if full.is_empty() {
            if let Some(e) = m.get_mut(provider) {
                let was_closed = !e.windows_full.is_empty();
                e.windows_full.clear();
                e.reset_epoch_ms = None;
                // 配额开了，但网络/鉴权 issue 可能仍在 → 只清配额相关，保留故障状态
                if e.issue.is_none() {
                    m.remove(provider);
                }
                if was_closed {
                    tracing::info!(provider = %provider, "provider quota gate opened (official windows show remaining)");
                }
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
            ..Default::default()
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
            // 合成窗标记：429/402 关闸时没有官方窗名可记，但所有"关闸"消费方
            // （gate_blocked / meta watchdog / relay 904）都以 windows_full 非空
            // 为判据——空列表会让这次关闸对它们完全不可见
            windows_full: vec!["quota".to_string()],
            reset_epoch_ms,
            since_ms: now,
            probe_at_ms: now + ProviderGateState::API_RETRY_MS,
            ..Default::default()
        });
        if newly {
            tracing::warn!(provider = %provider, api = api_backed, "provider quota gate closed (upstream quota signal)");
        }
        e.api_backed = api_backed;
        if e.reset_epoch_ms.is_none() { e.reset_epoch_ms = reset_epoch_ms; }
        // 重排下次探测/重试：API 型 1h 兜底；本地无 API 型 5min 起步。
        // 上游**明示了重置时刻**且晚于基础间隔 → 探测定在重置后 1min
        // （2026-10-23 用户定：一次配额失败即冷却到上游声明的重置点，
        // 中间的探测全是注定 429 的白耗——openrouter 日限实证）
        let base = if api_backed {
            ProviderGateState::API_RETRY_MS
        } else {
            ProviderGateState::PROBE_BASE_MS
        };
        e.probe_at_ms = match reset_epoch_ms {
            Some(r) if !api_backed && r > now + base => r + 60_000,
            _ => now + base,
        };
        e.probing = false;
    }

    /// 闸门关闭的 provider 名列表（账号级）——relay 据此在 quota 视图里
    /// 把该账号的全部模型余量清零（引擎按模型硬滤，替换早前 plan_pressure=1.5
    /// 的粗粒度方案：同 plan_key 多账号不再同伴受罚）
    /// 上游失败记账（provider 级，非配额）：网络不可达 / 鉴权·区域被拒。
    /// 连续失败达 `ISSUE_MIN_FAILS` 才真正降级（单次抖动不降级），降级后按
    /// 退避表安排探活（1m→5m→15m→30m→60m）——**绝不永久沉默**。
    /// 返回 true = 本次刚进入降级（调用方据此发事件）。
    pub fn provider_failure(&self, provider: &str, issue: ProviderIssue, now: u64) -> bool {
        if provider.is_empty() {
            return false;
        }
        let Ok(mut m) = self.provider_gate.lock() else { return false };
        let e = m.entry(provider.to_string()).or_default();
        // 故障类型切换（网络↔鉴权）时重新计数，避免两类互相顶替把阈值刷满
        if e.issue_counting != Some(issue) {
            e.issue_counting = Some(issue);
            e.issue_fails = 0;
        }
        e.issue_fails = e.issue_fails.saturating_add(1);
        let newly = e.issue != Some(issue) && e.issue_fails >= ProviderGateState::ISSUE_MIN_FAILS;
        if e.issue_fails >= ProviderGateState::ISSUE_MIN_FAILS {
            e.issue = Some(issue);
            e.issue_probe_at_ms =
                now + ProviderGateState::issue_backoff_for(issue, e.issue_probe_failures);
            if newly {
                tracing::warn!(
                    provider = %provider, issue = issue.label(), fails = e.issue_fails,
                    "provider degraded (non-quota) — excluded from the main pool, probed on backoff"
                );
            }
        }
        newly
    }

    /// 上游成功 = 真实可用的权威反证：清零 provider 级故障与连续失败计数。
    /// 返回 true = 本次从降级恢复（调用方据此发事件/立刻放回组合）。
    pub fn provider_success(&self, provider: &str) -> bool {
        if provider.is_empty() {
            return false;
        }
        let Ok(mut m) = self.provider_gate.lock() else { return false };
        let Some(e) = m.get_mut(provider) else { return false };
        let was = e.issue.is_some();
        e.issue = None;
        e.issue_fails = 0;
        e.issue_counting = None;
        e.issue_probe_at_ms = 0;
        e.issue_probe_failures = 0;
        if was {
            tracing::info!(provider = %provider, "provider recovered — back in the pool");
        }
        was
    }

    /// 当前处于降级的 provider 名单（供引擎主池过滤）。
    /// 含"已到探测时刻"者——探测若成功会立即清除降级状态。
    pub fn provider_degraded(&self) -> Vec<String> {
        let Ok(m) = self.provider_gate.lock() else { return Vec::new() };
        m.iter()
            .filter(|(_, e)| e.issue.is_some())
            .map(|(k, _)| k.clone())
            .collect()
    }

    /// provider 故障详情（面板展示 "degraded · next probe"）：(类型, 探测时刻, 连续失败数)
    pub fn provider_issue_of(&self, provider: &str) -> Option<(ProviderIssue, u64, u32)> {
        let m = self.provider_gate.lock().ok()?;
        let e = m.get(provider)?;
        e.issue.map(|i| (i, e.issue_probe_at_ms, e.issue_fails))
    }

    /// 探测调度：挑一个到期（且未在探测中）的降级 provider；`probing` 去重。
    /// probing 带租约（60s）：领取后超时未见结果 → 视为泄漏，允许重新领取
    /// （探针自身 10s 超时，60s 内必有结果；超出即 result 丢失，不能永久占坑）。
    pub fn provider_issue_pick_probe(&self, now: u64) -> Option<String> {
        const PICK_LEASE_MS: u64 = 60 * 1000;
        let Ok(mut m) = self.provider_gate.lock() else { return None };
        let mut best: Option<(u64, String)> = None;
        for (k, e) in m.iter() {
            if e.issue.is_none() || now < e.issue_probe_at_ms {
                continue;
            }
            if e.probing && now.saturating_sub(e.issue_picked_at_ms) < PICK_LEASE_MS {
                continue;
            }
            if best.as_ref().map(|(t, _)| e.issue_probe_at_ms < *t).unwrap_or(true) {
                best = Some((e.issue_probe_at_ms, k.clone()));
            }
        }
        let (t, k) = best?;
        if let Some(e) = m.get_mut(&k) {
            e.probing = true;
            e.issue_picked_at_ms = now;
            e.issue_probe_at_ms = t;
        }
        Some(k)
    }

    /// 探测结论：成功 → 立即恢复（清降级）；失败 → 退避加一档重排。
    pub fn provider_issue_probe_result(&self, provider: &str, ok: bool, now: u64) {
        let Ok(mut m) = self.provider_gate.lock() else { return };
        let Some(e) = m.get_mut(provider) else { return };
        e.probing = false;
        // 快照需要清理的字段（ borrow 结束后再做整条移除）
        let issue_only_entry = ok && e.windows_full.is_empty();
        if ok {
            e.issue = None;
            e.issue_fails = 0;
            e.issue_counting = None;
            e.issue_probe_at_ms = 0;
            tracing::info!(provider = %provider, "probe succeeded — provider revived");
        } else {
            e.issue_probe_failures = e.issue_probe_failures.saturating_add(1);
            e.issue_probe_at_ms = now
                + ProviderGateState::issue_backoff_for(
                    e.issue.unwrap_or(ProviderIssue::Network),
                    e.issue_probe_failures,
                );
        }
        // 只承载 issue 的条目：恢复后整条移除（配额状态本就为空）
        if issue_only_entry {
            m.remove(provider);
        }
    }

    pub fn gate_blocked(&self) -> Vec<String> {
        let Ok(m) = self.provider_gate.lock() else { return Vec::new() };
        // 只返回**真正配额耗尽**的 provider：网络/鉴权降级记在 issue 字段里，
        // 不能污染 "quota-gated" 语义（否则可用性标签与配额视图会误判）
        m.iter()
            .filter(|(_, e)| !e.windows_full.is_empty())
            .map(|(k, _)| k.clone())
            .collect()
    }

    pub fn gate_get(&self, provider: &str) -> Option<ProviderGateState> {
        self.provider_gate.lock().ok()?.get(provider).cloned()
    }

    /// 选一个进入探测的 provider（无 API 型）：探测窗口到 + 未在探测中。
    /// probing 带租约（60s）：领取后超时未见结果 → 视为泄漏，允许重新领取
    /// （与 issue 探活同一防楔死模式，2026-10-23 实证）。
    pub fn gate_pick_probe(&self, now: u64) -> Option<String> {
        const PICK_LEASE_MS: u64 = 60 * 1000;
        let mut m = self.provider_gate.lock().ok()?;
        let mut best: Option<(u64, String)> = None;
        for (k, e) in m.iter() {
            if e.api_backed || now < e.probe_at_ms { continue; }
            if e.probing && now.saturating_sub(e.probe_picked_at_ms) < PICK_LEASE_MS { continue; }
            if best.as_ref().map(|(t, _)| e.probe_at_ms < *t).unwrap_or(true) {
                best = Some((e.probe_at_ms, k.clone()));
            }
        }
        let (t, k) = best?;
        if let Some(e) = m.get_mut(&k) {
            e.probing = true;
            e.probe_picked_at_ms = now;
            e.probe_at_ms = t;
        }
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
        // stale-window guards（对齐 best_remaining_by_models 的 review#8 修复）：
        // 24h 前的窗是垃圾数据；零余量且无重置时刻只封锁 30min
        const MAX_WINDOW_AGE_MS: u64 = 24 * 3600 * 1000;
        const UNKNOWN_RESET_BLOCK_MS: u64 = 30 * 60 * 1000;
        let mut best: Option<u64> = None;
        for (k, slots) in m.iter() {
            if health_model_id(k) != model_id {
                continue;
            }
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
    if bytes.len() < 19 {
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
mod provider_issue_tests {
    use super::*;

    /// 用户裁决：**连续失败数**达标才降级；降级后按退避表探活（不永久沉默）；
    /// 成功一次立即恢复；故障类型切换要重新计数（网络↔鉴权不互相顶替）
    #[test]
    fn consecutive_failures_degrade_then_probe_revives() {
        let q = QuotaLedger::new();
        let t0 = 1_000_000u64;
        // 前两次：单次抖动，不降级
        assert!(!q.provider_failure("p", ProviderIssue::Network, t0));
        assert!(!q.provider_failure("p", ProviderIssue::Network, t0 + 1));
        assert!(q.provider_degraded().is_empty(), "two failures must not degrade");
        // 第三次达阈值 → 降级
        assert!(q.provider_failure("p", ProviderIssue::Network, t0 + 2));
        assert_eq!(q.provider_degraded(), vec!["p".to_string()]);
        let (issue, probe_at, fails) = q.provider_issue_of("p").unwrap();
        assert_eq!(issue, ProviderIssue::Network);
        assert_eq!(fails, 3);
        // 退避：未到点不探测
        assert!(q.provider_issue_pick_probe(probe_at - 1).is_none());
        // 到点可探测（去重：探测中不再派）
        assert_eq!(q.provider_issue_pick_probe(probe_at).as_deref(), Some("p"));
        assert!(q.provider_issue_pick_probe(probe_at).is_none(), "probing must dedupe");
        // 探测失败 → 退避加一档（下一次更远）
        q.provider_issue_probe_result("p", false, probe_at);
        let (_, probe_at2, _) = q.provider_issue_of("p").unwrap();
        assert!(probe_at2 > probe_at, "backoff must grow: {probe_at2} > {probe_at}");
        // 探测成功 → 立即恢复
        q.provider_issue_probe_result("p", true, probe_at2);
        assert!(q.provider_degraded().is_empty(), "probe success revives immediately");
    }

    /// 上游成功 = 权威反证：清零降级与连续失败计数
    #[test]
    fn routed_success_clears_degradation() {
        let q = QuotaLedger::new();
        for i in 0..3 {
            q.provider_failure("p", ProviderIssue::Auth, 100 + i);
        }
        assert_eq!(q.provider_degraded(), vec!["p".to_string()]);
        assert!(q.provider_success("p"), "was degraded → reported as revived");
        assert!(q.provider_degraded().is_empty());
        assert!(!q.provider_success("p"), "second call is a no-op");
        assert!(q.provider_issue_of("p").is_none());
    }

    /// 网络→鉴权切换重新计数（避免两类互相顶替把阈值刷满）
    #[test]
    fn switching_issue_kind_restarts_the_counter() {
        let q = QuotaLedger::new();
        q.provider_failure("p", ProviderIssue::Network, 1);
        q.provider_failure("p", ProviderIssue::Network, 2);
        q.provider_failure("p", ProviderIssue::Auth, 3); // 切换 → 计数从 1
        assert!(q.provider_degraded().is_empty(), "kind switch must restart counting");
        q.provider_failure("p", ProviderIssue::Auth, 4);
        assert!(q.provider_failure("p", ProviderIssue::Auth, 5));
        assert_eq!(q.provider_issue_of("p").unwrap().0, ProviderIssue::Auth);
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

    /// 探测租约（2026-10-23 实证）：probe 结果丢失时 probing 永久 true 会把
    /// provider 卡死在降级态——租约超时后必须允许重新领取
    #[test]
    fn issue_probe_lease_expires_and_reissues() {
        let q = QuotaLedger::default();
        let now = 1_000_000_000u64;
        // 降级 + due（issue_probe_at_ms 在过去），模拟"领取后 result 丢失"
        {
            let mut m = q.provider_gate.lock().unwrap();
            let e = m.entry("p".to_string()).or_default();
            e.issue = Some(ProviderIssue::Network);
            e.issue_probe_at_ms = now - 60_000;
            e.probing = true;
            e.issue_picked_at_ms = now - 300_000; // 租约早已超时
        }
        // 租约超时 → 重新可领取
        assert_eq!(q.provider_issue_pick_probe(now).as_deref(), Some("p"));
        // 刚领取（租约内）→ 不重复发放
        assert_eq!(q.provider_issue_pick_probe(now + 1000), None);
    }
}

//! Upstream health registry: classifies upstream failures into health kinds
//! and cools models down so the router avoids dead/unfunded ones.

use evolve_core::types::*;
use std::collections::HashMap;
use std::sync::Mutex;

pub struct HealthRegistry {
    inner: Mutex<HashMap<String, HealthEntry>>,
}

pub struct Failure {
    pub kind: HealthKind,
    pub message: String,
    /// 上游明确告知的窗口重置时间（如 retry-after/限额头）——优先于默认冷却
    pub until_epoch_ms: Option<u64>,
}

impl Default for Failure {
    fn default() -> Self {
        Self { kind: HealthKind::Transient, message: String::new(), until_epoch_ms: None }
    }
}

impl HealthRegistry {
    pub fn new() -> Self {
        Self { inner: Mutex::new(HashMap::new()) }
    }

    pub fn snapshot(&self) -> HealthMap {
        self.inner.lock().map(|m| m.clone()).unwrap_or_default()
    }

    pub fn mark_ok(&self, model_id: &str) {
        if let Ok(mut m) = self.inner.lock()
            && let Some(e) = m.get_mut(model_id)
        {
            // success resets the escalation counter so a recovered model
            // doesn't inherit multiplied cooldowns from its outage
            e.hits = 0;
            e.kind = HealthKind::Ok;
            e.until_epoch_ms = None;
            e.message.clear();
            e.updated_at = now();
        }
    }

    pub fn mark_failure(&self, model_id: &str, f: Failure) {        let Ok(mut m) = self.inner.lock() else { return };
        let entry = m.entry(model_id.to_string()).or_insert_with(|| HealthEntry {
            kind: HealthKind::Ok,
            until_epoch_ms: None,
            message: String::new(),
            hits: 0,
            updated_at: 0,
        });
        entry.hits += 1;
        // escalate cooldown when repeat failures stack. Quota windows are
        // periodic (coding-plan windows reset on their own schedule), so
        // quota stays flat: probe every base interval, reset on each failure
        let mult = if f.kind == HealthKind::QuotaExhausted {
            1
        } else {
            entry.hits.min(4) as u64
        };
        // 上游明确给出重置时间（retry-after/限额头）时优先采用（用户裁决：
        // 无 API 可查的 provider 只能信任上游给的窗口期，如月窗 9d9h 就如实
        // 展示）；仅保留 30 天理智上限防脏数据。永久哨兵（模型不存在）不封顶。
        let cooldown = match f.until_epoch_ms {
            Some(Self::PERMANENT_UNTIL) => Self::PERMANENT_UNTIL,
            Some(until) => until.saturating_sub(now()).min(30 * 24 * 3600 * 1000),
            None => f.kind.default_cooldown_ms().saturating_mul(mult),
        };
        entry.kind = f.kind;
        entry.message = f.message;
        entry.until_epoch_ms = if cooldown == Self::PERMANENT_UNTIL {
            Some(Self::PERMANENT_UNTIL)
        } else {
            Some(now() + cooldown)
        };
        entry.updated_at = now();
    }

    /// 清指定模型的健康条目（永久黑名单手动恢复 / API 确认后解除）
    pub fn clear(&self, model_ids: &[String]) {
        let Ok(mut m) = self.inner.lock() else { return };
        for id in model_ids {
            m.remove(id);
        }
    }

    /// 仅清指定 kind 的冷却（如 API 型 provider 的 QuotaExhausted 被官方
    /// 数据反证解除；不动其它健康记录）
    pub fn clear_kind(&self, model_ids: &[String], kind: HealthKind) {
        let Ok(mut m) = self.inner.lock() else { return };
        for id in model_ids {
            if let Some(e) = m.get(id)
                && e.kind == kind
            {
                m.remove(id);
            }
        }
    }

    /// 永久黑名单（上游确认不存在的模型，需手动恢复；until=u64::MAX 哨兵）
    pub const PERMANENT_UNTIL: u64 = u64::MAX;

    pub fn is_permanent(until: u64) -> bool {
        until == Self::PERMANENT_UNTIL
    }
}

impl Default for HealthRegistry {
    fn default() -> Self {
        Self::new()
    }
}

/// Classify an upstream error response into a health kind.
/// `body_snippet` is the (truncated) response body text; message keywords
/// refine 429s into quota-exhausted vs plain rate limits.
pub fn classify_failure(status: u16, body_snippet: &str, retry_after_ms: Option<u64>) -> Failure {
    let lower = body_snippet.to_lowercase();
    let no_credit = ["insufficient balance", "insufficient_quota", "balance", "arrears", "欠费", "余额不足", "no credit", "payment required", "billing"];
    let quota = ["quota", "exceeded your", "usage limit", "limit reached", "rate limit exceeded", "额度", "配额", "用量", "超限"];

    match status {
        413 => Failure {
            kind: HealthKind::ContextOverflow,
            message: "request body exceeds context".into(),
            until_epoch_ms: None,
        },
        404 => Failure {
            kind: HealthKind::Unsupported,
            message: first_hit(&lower, &["unsupported", "not support", "does not exist", "not found"])
                .unwrap_or("model not found on upstream")
                .into(),
            until_epoch_ms: None,
        },
        400 if ["context", "too long", "maximum context", "context length",
                "exceeds", "prompt is too long", "上下文", "超出了模型",
                "input length", "token limit", "max_tokens", "input tokens",
                "内容过长", "长度超过"]
            .iter()
            .any(|k| lower.contains(k)) =>
        {
            Failure {
                kind: HealthKind::ContextOverflow,
                message: "context window exceeded".into(),
                until_epoch_ms: None,
            }
        }
        // 协议不支持（实证：opencode zen 的 gpt-*/grok-* 只接受 Responses API，
        // 对 chat/completions 与 messages 均回 400 ModelProtocolUnsupported）：
        // 归入 Unsupported → 由 relay 按"在列表 5h / 不在列表永久"处理，
        // 避免当作瞬态错误每 1-2 分钟重试同一模型（面板长期显示冷却）
        400 if lower.contains("protocol") && (lower.contains("unsupported") || lower.contains("not support")) => {
            Failure {
                kind: HealthKind::Unsupported,
                message: "model does not support this protocol (provider requires another API)".into(),
                until_epoch_ms: None,
            }
        }
        401 | 403 => Failure {
            kind: HealthKind::AuthFailed,
            message: first_hit(&lower, &["invalid", "unauthorized", "forbidden", "无权", "鉴权"])
                .unwrap_or("auth rejected by upstream")
                .into(),
            until_epoch_ms: None,
        },
        402 => Failure {
            kind: HealthKind::PaymentRequired,
            message: first_hit(&lower, &no_credit).unwrap_or("payment required").into(),
            until_epoch_ms: None,
        },
        429 => {
            // 恢复时间优先级：正文"reset at <时间>"（精确时刻，如智谱 5h 超限）
            // > Retry-After 头 > 默认 5 分钟探测节奏
            let until = parse_reset_epoch_ms(body_snippet, now())
                .or_else(|| retry_after_ms.map(|ms| now() + ms));
            if no_credit.iter().any(|k| lower.contains(k)) {
                Failure { kind: HealthKind::PaymentRequired, message: "no credit".into(), until_epoch_ms: None }
            } else if quota.iter().any(|k| lower.contains(k)) {
                // 窗口标签（用户裁决：哪个门限超了要可判定——5h/weekly/monthly）；
                // 上游告知重置时间时精确冷却；否则固定 5 分钟探测节奏
                let win = if lower.contains("weekly") || lower.contains("week") { " (weekly)" }
                    else if lower.contains("monthly") || lower.contains("month") { " (monthly)" }
                    else { " (5h)" };
                Failure { kind: HealthKind::QuotaExhausted, message: format!("quota window exhausted{win}"), until_epoch_ms: until }
            } else {
                Failure {
                    kind: HealthKind::RateLimited,
                    message: match retry_after_ms {
                        Some(ms) => format!("retry after {}s", ms / 1000),
                        None => "rate limited".into(),
                    },
                    until_epoch_ms: until,
                }
            }
        }
        s if s >= 500 => Failure { kind: HealthKind::Transient, message: format!("upstream {s}"), until_epoch_ms: None },
        _ => Failure { kind: HealthKind::Transient, message: format!("upstream {status}"), until_epoch_ms: None },
    }
}

fn first_hit<'a>(lower: &str, keys: &[&'a str]) -> Option<&'a str> {
    keys.iter().find(|k| lower.contains(**k)).copied()
}


/// days-from-civil (Howard Hinnant 算法)：公历日期 → 自 1970-01-01 的天数
fn days_from_civil(y: i64, m: i64, d: i64) -> i64 {
    let y = if m <= 2 { y - 1 } else { y };
    let era = if y >= 0 { y } else { y - 399 } / 400;
    let yoe = y - era * 400;
    let mp = (m + 9) % 12;
    let doy = (153 * mp + 2) / 5 + d - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    era * 146097 + doe - 719468
}




pub fn now() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn classifies_deepseek_balance_error() {
        let f = classify_failure(402, "{\"error\":{\"message\":\"Insufficient Balance\"}}", None);
        assert_eq!(f.kind, HealthKind::PaymentRequired);
    }

    #[test]
    fn classifies_quota_429() {
        let f = classify_failure(429, "You have exceeded your usage limit", None);
        assert_eq!(f.kind, HealthKind::QuotaExhausted);
    }

    #[test]
    fn classifies_plain_429_with_retry_after() {
        let f = classify_failure(429, "too many requests", Some(5000));
        assert_eq!(f.kind, HealthKind::RateLimited);
        assert!(f.message.contains("5s"));
    }

    #[test]
    fn classifies_auth_and_transient() {
        assert_eq!(classify_failure(401, "invalid api key", None).kind, HealthKind::AuthFailed);
        assert_eq!(classify_failure(503, "overloaded", None).kind, HealthKind::Transient);
        assert_eq!(classify_failure(529, "overloaded", None).kind, HealthKind::Transient);
    }

    #[test]
    fn protocol_unsupported_is_classified() {
        let body = r#"{"type":"error","error":{"type":"ModelProtocolUnsupported","message":"Model does not support this protocol."}}"#;
        let f = classify_failure(400, body, None);
        assert_eq!(f.kind, HealthKind::Unsupported, "protocol-unsupported must not be transient");
        // 普通 400（上下文类）仍归 ContextOverflow
        let f2 = classify_failure(400, "context length exceeded", None);
        assert_eq!(f2.kind, HealthKind::ContextOverflow);
        // 其它 400 仍是瞬态
        let f3 = classify_failure(400, "bad request: invalid tool schema", None);
        assert_eq!(f3.kind, HealthKind::Transient);
    }

    #[test]
    fn repeat_failures_extend_cooldown() {
        let reg = HealthRegistry::new();
        for _ in 0..3 {
            reg.mark_failure("m", classify_failure(429, "rate limit exceeded", None));
        }
        let snap = reg.snapshot();
        let e = snap.get("m").unwrap();
        assert_eq!(e.hits, 3);
        assert!(e.cooldown_remaining_ms(now()).unwrap_or(0) >= 120_000, "3 hits x base 60s");
        reg.mark_ok("m");
        assert!(reg.snapshot().get("m").unwrap().available(now()));
    }
}

/// 从错误正文解析"恢复时间点"——上游（智谱 5h 超限等）时常把
/// `It will reset at 2026-10-12 19:11:41 +0800 CST` 写在正文而不带 Retry-After 头。
/// 覆盖三类写法（无法解析或时间已过 → None，走默认探测节奏）：
/// ① 绝对时间 `2026-10-12 19:11:41 (+0800|+08:00|Z|UTC)?`（无时区按中国时区 +08 近似）
/// ② `resets in 47 minutes` / `in 47 minutes`
/// ③ `resets in 2 hours` / `in 2 hours`
pub fn parse_reset_epoch_ms(body: &str, now_ms: u64) -> Option<u64> {
    let l = body.to_lowercase();

    // ③ 相对分钟 / 小时（先于绝对时间扫：两种写法不共存，顺序无所谓）
    if let Some(pos) = l.find("minute") {
        if let Some(n) = num_before(&l[..pos]) {
            if (1..600).contains(&n) {
                return Some(now_ms + n * 60_000);
            }
        }
    }
    if let Some(pos) = l.find("hour") {
        if let Some(n) = num_before(&l[..pos]) {
            if (1..48).contains(&n) {
                return Some(now_ms + n * 3_600_000);
            }
        }
    }

    // ① 绝对时间：找 `YYYY-MM-DD` 锚点
    let bytes = body.as_bytes();
    let mut i = 0usize;
    while i + 10 <= bytes.len() {
        if bytes[i..i + 4].iter().all(u8::is_ascii_digit)
            && bytes[i + 4] == b'-'
            && bytes[i + 5..i + 7].iter().all(u8::is_ascii_digit)
            && bytes[i + 7] == b'-'
            && bytes[i + 8..i + 10].iter().all(u8::is_ascii_digit)
        {
            let year: i64 = body[i..i + 4].parse().ok()?;
            let month: i64 = body[i + 5..i + 7].parse().ok()?;
            let day: i64 = body[i + 8..i + 10].parse().ok()?;
            if !(1..=12).contains(&month) || !(1..=31).contains(&day) {
                i += 10;
                continue;
            }
            let (mut hour, mut minute, mut second) = (0i64, 0i64, 0i64);
            // 可选时间部分：'T' 或空格 + HH:MM(:SS)?
            let rest = &body[i + 10..];
            let rb = rest.as_bytes();
            if !rb.is_empty() && (rb[0] == b'T' || rb[0] == b' ') && rb.len() >= 5
                && rb[1..3].iter().all(u8::is_ascii_digit)
                && rb[3] == b':'
                && rb[4..6].iter().all(u8::is_ascii_digit)
            {
                hour = rest[1..3].parse().ok()?;
                minute = rest[4..6].parse().ok()?;
                if rb.len() >= 9 && rb[6] == b':' && rb[7..9].iter().all(u8::is_ascii_digit) {
                    second = rest[7..9].parse().ok()?;
                }
            }
            // 时区尾巴：+0800 / +08:00 / Z / UTC → 0；其余（含 CST）→ +08 近似
            let tz_rest = rest.to_lowercase();
            let offset_s: i64 = if let Some(p) = tz_rest.find('+').or_else(|| tz_rest.find('-').filter(|_| i > 0)) {
                let _ = p;
                let tz: String = tz_rest
                    .chars()
                    .skip_while(|c| *c != '+' && *c != '-')
                    .skip(1)
                    .filter(|c| c.is_ascii_digit())
                    .take(4)
                    .collect();
                if tz.len() == 4 {
                    let h: i64 = tz[0..2].parse().unwrap_or(0);
                    let m: i64 = tz[2..4].parse().unwrap_or(0);
                    h * 3600 + m * 60
                } else {
                    8 * 3600
                }
            } else if tz_rest.contains("utc") || tz_rest.contains('z') {
                0
            } else {
                8 * 3600
            };
            let base = days_from_civil(year, month, day);
            let utc_ms = (base as i64) * 86_400_000 + hour * 3_600_000 + minute * 60_000 + second * 1000
                - offset_s * 1000;
            if utc_ms > 0 && (utc_ms as u64) > now_ms {
                return Some(utc_ms as u64);
            }
            return None;
        }
        i += 1;
    }
    None
}

/// 取文本尾部最近的 1~3 位数字（"resets in 47" → 47）
fn num_before(head: &str) -> Option<u64> {
    let digits: String = head
        .chars()
        .rev()
        .skip_while(|c| c.is_whitespace())
        .take_while(|c| c.is_ascii_digit())
        .collect::<Vec<_>>()
        .into_iter()
        .rev()
        .collect();
    let n: u64 = digits.parse().ok()?;
    if (1..6000).contains(&n) {
        Some(n)
    } else {
        None
    }
}

/// days-from-civil (Howard Hinnant)：公历日期 → 自 1970-01-01 的天数

#[cfg(test)]
mod reset_tests {
    use super::*;

    #[test]
    fn parses_zhipu_5h_notice() {
        let now = 1_760_000_000_000;
        let body = "You have exceeded the 5-hour usage quota. It will reset at 2026-10-12 19:11:41 +0800 CST.";
        let ms = parse_reset_epoch_ms(body, now).unwrap();
        // 2026-10-12 19:11:41 +0800 = 11:11:41 UTC = 1791805901000ms 附近
        let expected = 1_791_803_501_000u64;
        assert!((ms as i64 - expected as i64).abs() < 1_000, "got {ms}");
        assert!(ms > now);
    }

    #[test]
    fn parses_relative_minutes_and_hours() {
        let now = 1_760_000_000_000;
        assert_eq!(parse_reset_epoch_ms("resets in 47 minutes", now), Some(now + 47 * 60_000));
        assert_eq!(parse_reset_epoch_ms("try again in 2 hours", now), Some(now + 2 * 3_600_000));
    }

    #[test]
    fn rejects_garbage_and_past() {
        let now = 1_760_000_000_000;
        assert_eq!(parse_reset_epoch_ms("nothing here", now), None);
        assert_eq!(parse_reset_epoch_ms("reset at 2001-01-01 00:00:00 +0800", now), None);
    }
}

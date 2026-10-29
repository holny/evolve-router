//! 模型新旧度（2026-10，用户裁决）：可用模型太多时，只把**最近发布 + 最近用过**
//! 的 50 个留在活跃集（可路由），其余在 Web 展示、由用户手动开启（开启会自动
//! 顶掉当前第 50 名）。
//!
//! 发布时间来源（离线自足，无需外部依赖）：
//! 1. 模型 id 内嵌日期——各家命名惯例高度一致：
//!    `doubao-seed-1-6-250615`（YYMMDD）、`glm-4-5-air-20250728`（YYYYMMDD）、
//!    `deepseek-v3-1-250821`、`kimi-k2-250711` …
//! 2. 无内嵌日期 → `None`（排序时退化为"仅按最近使用"）。

/// 活跃集容量（用户裁决：最近发布 + 最近使用的前 50）
pub const ACTIVE_CAP: usize = 50;
/// 其中**保底留给"新发布"**的席位数（用户裁决：两部分占比要动态、且不能被
/// "最近使用"全部吃掉——否则新模型永远进不来 → 永远没流量 → 死锁）
pub const NEWEST_QUOTA: usize = 20;
/// 最近使用的统计窗口（超过则不计入"最近使用"，避免旧流量永久占位）
pub const USED_WINDOW_MS: u64 = 14 * 24 * 3600 * 1000;

/// 从模型 id 解析发布日期（epoch ms）。支持 YYMMDD / YYYYMMDD 数字段。
pub fn release_ts_from_id(model_id: &str) -> Option<u64> {
    // 逐段扫描数字串（分隔符任意）
    let bytes = model_id.as_bytes();
    let mut i = 0usize;
    while i < bytes.len() {
        if !bytes[i].is_ascii_digit() {
            i += 1;
            continue;
        }
        let start = i;
        while i < bytes.len() && bytes[i].is_ascii_digit() {
            i += 1;
        }
        let seg = &model_id[start..i];
        if let Some(ts) = parse_date_seg(seg) {
            return Some(ts);
        }
    }
    None
}

fn parse_date_seg(seg: &str) -> Option<u64> {
    let (y, m, d) = match seg.len() {
        // YYMMDD：年份窗口 2024..2027（模型目录的现实范围，避免把版本号当日期）
        6 => {
            let yy: i64 = seg[0..2].parse().ok()?;
            let y = 2000 + yy;
            (y, seg[2..4].parse::<i64>().ok()?, seg[4..6].parse::<i64>().ok()?)
        }
        8 => (
            seg[0..4].parse::<i64>().ok()?,
            seg[4..6].parse::<i64>().ok()?,
            seg[6..8].parse::<i64>().ok()?,
        ),
        _ => return None,
    };
    if !(2024..=2027).contains(&y) || !(1..=12).contains(&m) || !(1..=31).contains(&d) {
        return None;
    }
    // civil → epoch ms（Howard Hinnant 算法）
    let y2 = if m <= 2 { y - 1 } else { y };
    let era = if y2 >= 0 { y2 } else { y2 - 399 } / 400;
    let yoe = y2 - era * 400;
    let mp = (m + 9) % 12;
    let doy = (153 * mp + 2) / 5 + d - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    let days = era * 146_097 + doe - 719_468;
    Some((days * 86_400) as u64 * 1000)
}

/// 发布时间（三级来源链，用户提问后补全）：
/// ① provider `/models` 的 `created` ② models.dev `release_date`（两者都存于
/// `ModelRecord.release_ts`）③ 模型 id 内嵌日期解析（兜底）
pub fn release_ts_of(m: &crate::types::ModelRecord) -> Option<u64> {
    m.release_ts.or_else(|| release_ts_from_id(&m.id))
}

/// 新旧度评分：发布时间与最近使用时间取较大者（用户裁决：两者都算"新"）
pub fn recency_score(model_id: &str, last_used_ms: Option<u64>) -> u64 {
    release_ts_from_id(model_id).unwrap_or(0).max(last_used_ms.unwrap_or(0))
}

// ───────────────────────── 本地日历（心选范围"昨日用量"，2026-10-28） ─────────────────────────
// 服务端首次需要本地时区日历口径（api 的绝对窗口此前"由客户端算好 epoch 传入"，
// 但路由侧的昨日聚合不能依赖客户端暗示）。不引入 chrono/time 依赖——经
// localtime_r（macOS libSystem / Linux glibc 皆有）取本地民事日期。

#[repr(C)]
struct CTm {
    tm_sec: std::os::raw::c_int,
    tm_min: std::os::raw::c_int,
    tm_hour: std::os::raw::c_int,
    tm_mday: std::os::raw::c_int,
    tm_mon: std::os::raw::c_int,
    tm_year: std::os::raw::c_int,
    tm_wday: std::os::raw::c_int,
    tm_yday: std::os::raw::c_int,
    tm_isdst: std::os::raw::c_int,
    tm_gmtoff: std::os::raw::c_long,
    tm_zone: *const std::os::raw::c_char,
}

unsafe extern "C" {
    fn localtime_r(timep: *const i64, result: *mut CTm) -> *mut CTm;
}

/// Howard Hinnant civil → 天数（自 1970-01-01）；与 parse_date_seg 内联公式同源
fn days_from_civil(y: i64, m: i64, d: i64) -> i64 {
    let y2 = if m <= 2 { y - 1 } else { y };
    let era = if y2 >= 0 { y2 } else { y2 - 399 } / 400;
    let yoe = y2 - era * 400;
    let mp = (m + 9) % 12;
    let doy = (153 * mp + 2) / 5 + d - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    era * 146_097 + doe - 719_468
}

/// 民事日期（y,m,d）当日 00:00 UTC 的 epoch ms
pub fn civil_day_start_utc_ms(y: i64, m: i64, d: i64) -> u64 {
    (days_from_civil(y, m, d) * 86_400) as u64 * 1000
}

/// 本地时区的今天日期（年, 月, 日）+ GMT 偏移秒——经 localtime_r（仅 unix）。
/// 返回 None = 系统调用失败（极罕见）：调用方回退 UTC 口径并告警——静默
/// 1970 会让"昨日"窗口彻底失真（评审 M2）。非 unix 目标（MSVC 无
/// localtime_r 符号）同样回退 UTC（评审 M3 可移植性）。
#[cfg(unix)]
pub fn local_date_ymd_gmtoff(now_ms: u64) -> Option<((i64, i64, i64), i64)> {
    let secs = (now_ms / 1000) as i64;
    let mut tm = CTm {
        tm_sec: 0, tm_min: 0, tm_hour: 0, tm_mday: 0, tm_mon: 0, tm_year: 0,
        tm_wday: 0, tm_yday: 0, tm_isdst: 0, tm_gmtoff: 0, tm_zone: std::ptr::null(),
    };
    // SAFETY: `secs` 指向合法 i64，`tm` 是本函数持有的完整 struct tm
    // （glibc/BSD 布局，含尾部 tm_gmtoff/tm_zone）；localtime_r 线程安全、
    // 失败返回 null（下方已检查）
    let ptr = unsafe { localtime_r(&secs, &mut tm) };
    if ptr.is_null() {
        None
    } else {
        Some((
            ((tm.tm_year as i64) + 1900, (tm.tm_mon as i64) + 1, tm.tm_mday as i64),
            tm.tm_gmtoff as i64,
        ))
    }
}

#[cfg(not(unix))]
pub fn local_date_ymd_gmtoff(_now_ms: u64) -> Option<((i64, i64, i64), i64)> {
    None
}

fn days_in_month(y: i64, m: i64) -> i64 {
    match m {
        1 | 3 | 5 | 7 | 8 | 10 | 12 => 31,
        4 | 6 | 9 | 11 => 30,
        _ => {
            let leap = (y % 4 == 0 && y % 100 != 0) || y % 400 == 0;
            if leap { 29 } else { 28 }
        }
    }
}

/// 民事日期当日**本地**零点的 epoch ms：UTC 零点 − GMT 偏移
/// （offset 为该时刻 tm_gmtoff；DST 切换日两个边界共用同一 offset，±1h 近似）。
/// 饱和运算防回绕（评审 L9：1970-01-01 + 正偏移的中间值为负，`as u64` 会
/// 包成 ~u64::MAX 的荒谬窗口边界）
pub fn local_midnight_utc_ms(y: i64, m: i64, d: i64, gmtoff_secs: i64) -> u64 {
    let ms = civil_day_start_utc_ms(y, m, d) as i64 - gmtoff_secs * 1000;
    ms.max(0) as u64
}

/// 本地日历"昨天"的 [起点, 终点) epoch ms（按民事日借位，非 24h 回退——DST 日
/// 亦正确；零点锚定本地时区：UTC+8 的"昨天"= 本地 00:00–24:00，而非 UTC 零点
/// 平移（该 bug 曾让窗口落在本地 08:00–次日 08:00，早晨流量被记进"昨天"）。
/// Howard Hinnant 天数 → 民事日期（与 days_from_civil 互逆）
fn civil_from_days(z: i64) -> (i64, i64, i64) {
    let z = z + 719_468;
    let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    (if m <= 2 { y + 1 } else { y }, m, d)
}

/// 终点 = 本地今天零点。
pub fn local_yesterday_bounds_ms(now_ms: u64) -> (u64, u64) {
    let ((y, m, d), gmtoff) = match local_date_ymd_gmtoff(now_ms) {
        Some(v) => v,
        // localtime 失败/不可用：回退 UTC 口径（gmtoff=0）并告警——窗口仍为
        // 合法 24h，只是日界偏移时区差（评审 M2：不许静默 1970）
        None => {
            tracing::warn!(
                "localtime_r unavailable — falling back to UTC day boundary for favorites yesterday window"
            );
            let days = (now_ms / 86_400_000) as i64;
            (civil_from_days(days), 0)
        }
    };
    let (ny, nm, nd) = if d > 1 {
        (y, m, d - 1)
    } else if m > 1 {
        (y, m - 1, days_in_month(y, m - 1))
    } else {
        (y - 1, 12, 31)
    };
    (local_midnight_utc_ms(ny, nm, nd, gmtoff), local_midnight_utc_ms(y, m, d, gmtoff))
}

#[cfg(test)]
mod calendar_tests {
    #[test]
    fn civil_day_start_matches_known_utc_midnights() {
        assert_eq!(super::civil_day_start_utc_ms(1970, 1, 1), 0);
        assert_eq!(super::civil_day_start_utc_ms(1970, 1, 2), 86_400_000);
        assert_eq!(super::civil_day_start_utc_ms(2024, 1, 1), 1_704_067_200_000);
        assert_eq!(super::civil_day_start_utc_ms(2026, 10, 28), 1_793_145_600_000);
    }

    #[test]
    fn local_midnight_anchors_to_timezone_offset() {
        // UTC+8（Asia/Shanghai）：2026-10-28 本地零点 = UTC 零点 − 8h
        assert_eq!(
            super::local_midnight_utc_ms(2026, 10, 28, 8 * 3600),
            1_793_145_600_000 - 8 * 3_600_000
        );
        // UTC−5（纽约冬令时）：本地零点 = UTC 零点 + 5h
        assert_eq!(
            super::local_midnight_utc_ms(2026, 10, 28, -5 * 3600),
            1_793_145_600_000 + 5 * 3_600_000
        );
        // 零偏移 = UTC 零点本身
        assert_eq!(super::local_midnight_utc_ms(2026, 10, 28, 0), 1_793_145_600_000);
    }

    #[test]
    fn yesterday_bounds_span_exactly_one_civil_day() {
        let now = 1_793_200_000_000u64; // 2026-10-28 前后的任意时刻
        let (start, end) = super::local_yesterday_bounds_ms(now);
        assert_eq!(end - start, 86_400_000, "civil-day borrow must yield exactly 24h");
        assert!(start < now, "yesterday window must end at local midnight (before now)");
    }
}

/// 活跃集选择（用户裁决：新发布/最近使用两部分**动态占比**，且新发布有保底）：
/// 1. `pinned_in`（手动开启）先占位；`pinned_out`（手动停用）剔除
/// 2. 按 `newest_quota` 从"按发布日期降序"取新发布保底名额
/// 3. 余量从"按最近使用降序（限窗口内）"取
/// 4. 任一侧不足时互相借用，再不足则按综合分（release 与 used 取大）补齐
pub fn select_active(
    by_release: &[(String, u64)], // (id, release_ts) 降序
    by_used: &[(String, u64)],    // (id, last_used_ms) 降序（窗口内）
    pinned_in: &[String],
    pinned_out: &[String],
    cap: usize,
    newest_quota: usize,
    now_ms: u64,
) -> Vec<String> {
    use std::collections::HashSet;
    let out: HashSet<&str> = pinned_out.iter().map(|s| s.as_str()).collect();
    let mut seen: HashSet<String> = HashSet::new();
    let mut active: Vec<String> = Vec::with_capacity(cap);
    let push = |id: &str, active: &mut Vec<String>, seen: &mut HashSet<String>| -> bool {
        if out.contains(id) || seen.contains(id) || active.len() >= cap {
            return false;
        }
        seen.insert(id.to_string());
        active.push(id.to_string());
        true
    };
    // 1) 手动开启
    for id in pinned_in {
        push(id, &mut active, &mut seen);
    }
    // 2) 新发布保底名额（带发布日期者）
    let mut newest_taken = 0usize;
    for (id, _) in by_release {
        if newest_taken >= newest_quota || active.len() >= cap {
            break;
        }
        if push(id, &mut active, &mut seen) {
            newest_taken += 1;
        }
    }
    // 3) 最近使用（窗口内）占余量
    for (id, ts) in by_used {
        if active.len() >= cap {
            break;
        }
        if now_ms.saturating_sub(*ts) > USED_WINDOW_MS {
            break; // 已按时间降序，超窗即停
        }
        push(id, &mut active, &mut seen);
    }
    // 4) 互借补齐：**先补新发布**（新模型优先于旧流量），最后才用过期使用兜底
    for (id, _) in by_release {
        if active.len() >= cap {
            break;
        }
        push(id, &mut active, &mut seen);
    }
    for (id, ts) in by_used {
        if active.len() >= cap {
            break;
        }
        // 仅窗口外的"过期使用"才落到这里（窗口内的已在第 3 步取过）
        let _ = ts;
        push(id, &mut active, &mut seen);
    }
    active
}

// ───────────────────────── 组合（portfolio）2026-10 用户裁决 ─────────────────────────
// 旧模型：单一**全局**活跃集（新发布 20 + 近用 30 的前 50）——谁新模型多谁通吃。
// 实测事故：zen 一次上几十个新模型 → 占掉 50 里的 41 个，把 MiniMax(1M 窗口)、
// volc、zhipu glm-5.3 全挤出可路由范围；一家额度耗尽/网络故障即"全军覆没"。
// 新模型 = **Provider 组合**：provider 名额上限 + 每家内 能力×价位 矩阵覆盖 +
// 免费直通车（必选、不占名额）。目标（用户原话）：交给智能决策时**覆盖所有可用
// provider**、**覆盖 价钱×能力 各档位** 都有模型，同时兼顾活跃与新模型。

/// 组合下限（provider 少时也至少这么多，保持现状体验）
pub const PORTFOLIO_MIN_CAP: usize = 50;
/// 每家健康 provider 的名义名额（组合上限 = max(下限, 家数 × 此值)）
pub const SLOTS_PER_HEALTHY_PROVIDER: usize = 8;
/// 每家保底名额（模型少的小供应商也能进组合）
pub const PROVIDER_MIN_SLOTS: usize = 3;
/// 单家最多占总名额比例（防"新模型多的供应商"刷屏）
pub const PROVIDER_MAX_SHARE: f32 = 0.25;
/// 每家内"新发布"保底名额
pub const PROVIDER_NEWEST_MIN: usize = 2;
/// 每家内"近 14 天用过"保底名额
pub const PROVIDER_RECENT_MIN: usize = 3;

/// 组合候选（由 server 侧从 catalog + 标签 + 用量组装）
#[derive(Debug, Clone)]
pub struct PortfolioItem {
    pub id: String,
    pub provider: String,
    /// 能力档 0=basic / 1=standard / 2=powerful
    pub cap_rank: u8,
    /// 价位档 0=cheap / 1=medium / 2=premium
    pub price_rank: u8,
    /// 免费模型：**直通车**（必选，不占 provider 名额）
    pub free: bool,
    pub release_ts: Option<u64>,
    pub last_used_ms: Option<u64>,
    /// 配额余量 0..1（未知按 0.5）——额度多的 provider 权重更高
    pub quota_ratio: f32,
}

/// 动态名额：最少 `PORTFOLIO_MIN_CAP`，随健康 provider 数放大
pub fn portfolio_cap(healthy_providers: usize) -> usize {
    PORTFOLIO_MIN_CAP.max(healthy_providers.saturating_mul(SLOTS_PER_HEALTHY_PROVIDER))
}

/// 单 provider 名额上限
pub fn provider_slot_cap(cap: usize) -> usize {
    ((cap as f32 * PROVIDER_MAX_SHARE) as usize).max(PROVIDER_MIN_SLOTS)
}

/// 选组合（顺序即 UI/日志展示顺序）：
/// ① 免费直通车（不占名额）② 每 provider 按权重分名额，
/// 家内**先补满 能力×价位 9 格**（每格取代表作）③ 再按 新发布/近期使用 补齐。
/// （旧"用户 pin"步骤已退役——2026-10-28：强制入选由 +3 Pinned/心选范围承担）
pub fn select_portfolio(items: &[PortfolioItem], now_ms: u64) -> Vec<String> {
    use std::collections::{BTreeMap, HashSet};
    let mut out: Vec<String> = Vec::new();
    let mut seen: HashSet<String> = HashSet::new();
    let push = |id: &str, out: &mut Vec<String>, seen: &mut HashSet<String>| {
        if seen.insert(id.to_string()) {
            out.push(id.to_string());
        }
    };
    // 计入名额的只有"非免费"的模型（用户裁决 2026-10）——
    // 用独立计数器，别拿 out.len() 与 cap 比（那样免费会把上限吃掉）
    let mut slot_used = 0usize;
    // ① 免费直通车：必选、不占名额（免费是"必然可用"的保底覆盖）
    for it in items.iter().filter(|i| i.free) {
        push(&it.id, &mut out, &mut seen);
    }
    // ③ 按 provider 分组
    let mut by_provider: BTreeMap<&str, Vec<&PortfolioItem>> = BTreeMap::new();
    for it in items {
        by_provider.entry(it.provider.as_str()).or_default().push(it);
    }
    let providers = by_provider.len();
    let cap = portfolio_cap(providers);
    let share_cap = provider_slot_cap(cap);
    // 权重 = 1 + 配额余量均值 + 免费模型数×0.1（额度多的多给名额）
    let weight = |list: &[&PortfolioItem]| -> f32 {
        let n = list.len().max(1) as f32;
        let q = list.iter().map(|i| i.quota_ratio.clamp(0.0, 1.0)).sum::<f32>() / n;
        let f = list.iter().filter(|i| i.free).count() as f32;
        1.0 + q + f * 0.1
    };
    let total_w: f32 = by_provider.values().map(|l| weight(l)).sum::<f32>().max(1e-6);
    // 家内排序：新发布 > 近期使用 > id（稳定）
    let rank_key = |i: &PortfolioItem| -> (u64, u64) {
        let rel = i.release_ts.unwrap_or(0);
        let used = i
            .last_used_ms
            .filter(|ts| now_ms.saturating_sub(*ts) <= USED_WINDOW_MS)
            .unwrap_or(0);
        (rel, used)
    };
    for (provider, list) in &by_provider {
        let slots = (((cap as f32) * weight(list) / total_w).round() as usize)
            .clamp(PROVIDER_MIN_SLOTS, share_cap);
        let mut budget = slots;
        // ③a 矩阵覆盖：9 格各取一个代表（格内按 rank_key 取最优）
        let mut cells: BTreeMap<(u8, u8), Vec<&PortfolioItem>> = BTreeMap::new();
        for it in list.iter() {
            cells
                .entry((it.cap_rank.min(2), it.price_rank.min(2)))
                .or_default()
                .push(it);
        }
        for (_cell, mut members) in cells {
            if budget == 0 {
                break;
            }
            members.sort_by(|a, b| rank_key(b).cmp(&rank_key(a)).then(a.id.cmp(&b.id)));
            if let Some(best) = members.first()
                && !seen.contains(&best.id)
            {
                push(&best.id, &mut out, &mut seen);
                budget -= 1;
                slot_used += 1;
            }
        }
        // ③b 家内保底：新发布 ≥2、近用 ≥3（不足不硬凑）
        let mut newest = 0usize;
        let mut recent = 0usize;
        let mut ordered: Vec<&PortfolioItem> = list.clone();
        ordered.sort_by(|a, b| rank_key(b).cmp(&rank_key(a)).then(a.id.cmp(&b.id)));
        for it in &ordered {
            if budget == 0 {
                break;
            }
            let is_new = it.release_ts.is_some() && newest < PROVIDER_NEWEST_MIN;
            let is_recent = it
                .last_used_ms
                .map(|ts| now_ms.saturating_sub(ts) <= USED_WINDOW_MS)
                .unwrap_or(false)
                && recent < PROVIDER_RECENT_MIN;
            if (is_new || is_recent) && !seen.contains(&it.id) {
                push(&it.id, &mut out, &mut seen);
                budget -= 1;
                slot_used += 1;
                if is_new {
                    newest += 1;
                }
                if is_recent {
                    recent += 1;
                }
            }
        }
        // ③c 余量按 新发布/近用 综合序补齐
        for it in &ordered {
            if budget == 0 {
                break;
            }
            if !seen.contains(&it.id) {
                push(&it.id, &mut out, &mut seen);
                budget -= 1;
                slot_used += 1;
            }
        }
        let _ = provider;
    }
    // ④ 空缺补齐（需求少、供应商少时别浪费名额）：份额上限只在"供给充足"时生效——
    // 若按份额分完仍没到组合上限，而某些家还有没入选的模型，则轮转补进来。
    // （单供应商 41 个模型只取 12 个是浪费；多家争抢时防刷屏的初衷仍然成立）
    if slot_used < cap {
        let mut rest: BTreeMap<&str, Vec<&PortfolioItem>> = BTreeMap::new();
        for (provider, list) in &by_provider {
            let mut left: Vec<&PortfolioItem> =
                list.iter().filter(|i| !seen.contains(&i.id)).copied().collect();
            left.sort_by(|a, b| rank_key(b).cmp(&rank_key(a)).then(a.id.cmp(&b.id)));
            if !left.is_empty() {
                rest.insert(*provider, left);
            }
        }
        let mut round = 0usize;
        while slot_used < cap {
            let mut took = false;
            for left in rest.values() {
                if slot_used >= cap {
                    break;
                }
                if let Some(it) = left.get(round) {
                    took = true;
                    push(&it.id, &mut out, &mut seen);
                    slot_used += 1;
                }
            }
            if !took {
                break;
            }
            round += 1;
        }
    }
    out
}

/// 启用一个模型会顶掉谁（UI 预告）：当前活跃集里分数最低者。
/// `exclude` = 刚被启用、已进入活跃集的那个模型（不能算作自己顶自己）。
pub fn swap_victim(ranked: &[(String, u64)], active: &[String], exclude: Option<&str>) -> Option<String> {
    let set: std::collections::HashSet<&str> = active.iter().map(|s| s.as_str()).collect();
    ranked
        .iter()
        .filter(|(id, _)| set.contains(id.as_str()) && Some(id.as_str()) != exclude)
        .last()
        .map(|(id, _)| id.clone())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_embedded_dates() {
        // YYMMDD
        let a = release_ts_from_id("volcengine-coding-plan/doubao-seed-1-6-250615").unwrap();
        // YYYYMMDD
        let b = release_ts_from_id("zhipuai-coding-plan/glm-4-5-air-20250728").unwrap();
        assert!(a > 0 && b > 0);
        assert!(b > a, "2025-07-28 should be later than 2025-06-15: {b} vs {a}");
        // 无日期 → None
        assert_eq!(release_ts_from_id("opencode-go-github/glm-5.3"), None);
        // 版本号不得误判为日期（2024..2027 窗口）
        assert_eq!(release_ts_from_id("vendor/model-v1.2.3"), None);
        assert_eq!(release_ts_from_id("vendor/x-1000000"), None);
    }

    #[test]
    fn recency_prefers_newer_or_recently_used() {
        let new_model = "p/a-260101"; // 2026-01-01
        let old_model = "p/b-240101"; // 2024-01-01
        let used = 1_800_000_000_000u64; // 最近用过（更晚）
        assert!(recency_score(new_model, None) > recency_score(old_model, None));
        assert!(recency_score(old_model, Some(used)) > recency_score(new_model, None));
    }

    #[test]
    fn active_selection_is_split_and_dynamic() {
        let now = 2_000_000_000_000u64;
        // 60 个"最近用过"的（时间递减，全在窗口内）
        let by_used: Vec<(String, u64)> = (0..60)
            .map(|i| (format!("u{i}"), now - i as u64 * 1000))
            .collect();
        // 30 个新发布（发布日期递减；与 used 无重叠）
        let by_release: Vec<(String, u64)> = (0..30)
            .map(|i| (format!("n{i}"), 1_700_000_000_000 - i as u64 * 100_000))
            .collect();
        let a = select_active(&by_release, &by_used, &[], &[], ACTIVE_CAP, NEWEST_QUOTA, now);
        assert_eq!(a.len(), 50);
        // 新发布保底 20 个（n0..n19）不被"最近使用"挤掉
        let newest_in = (0..20).filter(|i| a.contains(&format!("n{i}"))).count();
        assert_eq!(newest_in, 20, "newest quota must be honored");
        // 其余 30 个来自最近使用
        let used_in = (0..30).filter(|i| a.contains(&format!("u{i}"))).count();
        assert_eq!(used_in, 30);
        // 手动开启优先 + 顶掉最低者
        let a2 = select_active(&by_release, &by_used, &["zzz".to_string()], &[], ACTIVE_CAP, NEWEST_QUOTA, now);
        assert!(a2.contains(&"zzz".to_string()));
        assert_eq!(a2.len(), 50);
        // 手动停用：n0 被排除，名额由 used 补
        let a3 = select_active(&by_release, &by_used, &[], &["n0".to_string()], ACTIVE_CAP, NEWEST_QUOTA, now);
        assert!(!a3.contains(&"n0".to_string()));
        assert_eq!(a3.len(), 50);
        // 顶替预告：排除自己后取真正最低者
        let v = swap_victim(&by_used, &a2, Some("zzz"));
        assert!(v.is_some());
    }

    fn item(id: &str, provider: &str, cap: u8, price: u8, free: bool) -> PortfolioItem {
        PortfolioItem {
            id: id.into(),
            provider: provider.into(),
            cap_rank: cap,
            price_rank: price,
            free,
            release_ts: Some(1_700_000_000_000),
            last_used_ms: None,
            quota_ratio: 0.5,
        }
    }

    /// 用户裁决的核心：单一 provider 的模型再多也不能刷屏（zen 41/50 事故）
    #[test]
    fn portfolio_caps_any_single_provider_share() {
        let mut items: Vec<PortfolioItem> = (0..60)
            .map(|i| item(&format!("zen/m{i}"), "zen", (i % 3) as u8, ((i / 3) % 3) as u8, false))
            .collect();
        // 另外 3 家各有足够模型（供给充足时份额上限才该生效）
        for p in ["minimax", "volc", "zhipu"] {
            for c in 0..20 {
                items.push(item(&format!("{p}/m{c}"), p, (c % 3) as u8, ((c / 3) % 3) as u8, false));
            }
        }
        let out = select_portfolio(&items, 1_700_000_000_000);
        let cap = portfolio_cap(4);
        let share = provider_slot_cap(cap);
        let zen = out.iter().filter(|i| i.starts_with("zen/")).count();
        assert!(
            zen <= share + 1,
            "one provider must not flood the pool ({share}/+1 of {cap}): zen took {zen}"
        );
        // 供给充足时其余每家也应拿满份额（不再被一家挤出局）
        for p in ["minimax", "volc", "zhipu"] {
            let n = out.iter().filter(|i| i.starts_with(&format!("{p}/"))).count();
            assert!(n >= share, "{p} should get its share of {share}: got {n} — {out:?}");
        }
    }

    /// 能力×价位 9 格覆盖：每家每格都尽量有代表
    #[test]
    fn portfolio_covers_capability_and_price_cells() {
        let mut items: Vec<PortfolioItem> = Vec::new();
        for cap in 0..3u8 {
            for price in 0..3u8 {
                items.push(item(&format!("p/c{cap}p{price}"), "p", cap, price, false));
            }
        }
        let out = select_portfolio(&items, 1_700_000_000_000);
        for cap in 0..3u8 {
            for price in 0..3u8 {
                let id = format!("p/c{cap}p{price}");
                assert!(out.contains(&id), "cell cap{cap}/price{price} missing: {out:?}");
            }
        }
    }

    /// 免费直通车：必选且不占名额（名额被占满也要在）
    #[test]
    fn portfolio_free_models_ride_free() {
        let mut items: Vec<PortfolioItem> = vec![item("p/free-1", "p", 0, 0, true)];
        for i in 0..40 {
            items.push(item(&format!("p/m{i}"), "p", (i % 3) as u8, ((i / 3) % 3) as u8, false));
        }
        assert_eq!(portfolio_cap(1), PORTFOLIO_MIN_CAP);
        let out = select_portfolio(&items, 1_700_000_000_000);
        assert!(out.contains(&"p/free-1".to_string()), "free is a must: {out:?}");
        // 单供应商 41 个模型 + 免费：组合上限 50 没满 → 全部纳入
        // （免费是"额外"的，不吃名额——若吃名额就不会是 41）
        assert_eq!(out.len(), 41, "free rides free on top of the slots: {out:?}");
    }

    /// 免费/pin 不占名额：即使共享同一个 provider，也要在名额耗尽后仍被纳入
    #[test]
    fn portfolio_free_does_not_consume_the_cap() {
        // 12 个付费（恰好填满该 provider 的份额上限）+ 3 个免费
        let mut items: Vec<PortfolioItem> = (0..12)
            .map(|i| item(&format!("p/m{i}"), "p", (i % 3) as u8, ((i / 3) % 3) as u8, false))
            .collect();
        for f in 0..3 {
            items.push(item(&format!("p/free{f}"), "p", 0, 0, true));
        }
        let cap = portfolio_cap(1);
        let share = provider_slot_cap(cap);
        let out = select_portfolio(&items, 1_700_000_000_000);
        let paid = out.iter().filter(|i| i.starts_with("p/m")).count();
        assert_eq!(paid, share, "paid models fill the share cap: {paid} vs {share}");
        for f in 0..3 {
            assert!(out.contains(&format!("p/free{f}")), "free must always be in: {out:?}");
        }
        assert_eq!(out.len(), share + 3, "free rides on top of the cap: {out:?}");
    }

    /// 动态名额：provider 多时放大（最少 50）
    #[test]
    fn portfolio_cap_grows_with_provider_count() {
        assert_eq!(portfolio_cap(0), PORTFOLIO_MIN_CAP);
        assert_eq!(portfolio_cap(6), PORTFOLIO_MIN_CAP);
        assert_eq!(portfolio_cap(8), 64);
        // 每家 12 个（总供给 96 > 上限 64）→ 应恰好填满动态上限
        let items: Vec<PortfolioItem> = (0..8)
            .flat_map(|p| {
                (0..12).map(move |i| item(&format!("p{p}/m{i}"), &format!("p{p}"), (i % 3) as u8, ((i / 3) % 3) as u8, false))
            })
            .collect();
        let out = select_portfolio(&items, 1_700_000_000_000);
        assert_eq!(out.len(), portfolio_cap(8), "dynamic cap must be filled: {}", out.len());
        assert!(out.len() > PORTFOLIO_MIN_CAP, "cap should grow: {}", out.len());
    }

    #[test]
    fn stale_usage_falls_out_of_window() {
        let now = 2_000_000_000_000u64;
        // 只有 5 个在窗口内，其余过期
        let by_used: Vec<(String, u64)> = (0..60)
            .map(|i| (format!("u{i}"), now - i as u64 * USED_WINDOW_MS))
            .collect();
        let by_release: Vec<(String, u64)> = (0..40)
            .map(|i| (format!("n{i}"), 1_700_000_000_000 - i as u64 * 100_000))
            .collect();
        let a = select_active(&by_release, &by_used, &[], &[], ACTIVE_CAP, NEWEST_QUOTA, now);
        assert_eq!(a.len(), 50);
        // 过期使用不占"最近使用"名额 → 名额由新发布互借补足
        let newest_in = (0..40).filter(|i| a.contains(&format!("n{i}"))).count();
        assert!(newest_in > NEWEST_QUOTA, "stale usage must free slots for newest: {newest_in}");
    }
}

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

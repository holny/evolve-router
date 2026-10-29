//! 心选路径的每 provider 候选范围选举（2026-10-28 用户裁决）。
//!
//! 范围 = ≤3 心选位 + 2 昨日用量位 + 1 最新位（去重）+ 全部免费模型（调用方
//! 直通车叠加，不在本函数返回内）+ REC 兜底（9 格覆盖需要）。主来源标签反映
//! **当选来源**：Fav = 用户亲选；Top = 昨日用量；New = 最新入库；Rec = 填格
//! 推荐。空位链 FAV→TOP→NEW 只决定**填充顺序**，不继承标签——空心选位由昨日
//! 冠军补位时标 Top（它不是用户亲选的，标 Fav 会撒谎，spec 标签词表为准）。

/// 主来源标签（每模型恰一个；FREE 是调用方叠加的第二标签，不在此处）
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SourceTag {
    /// 用户亲选心爱（≤3/provider）
    Fav,
    /// 昨日用量最高（含补入空心选位者——标签按来源不按槽位）
    Top,
    /// 最新入库（含空位链末端补位者）
    New,
    /// 推荐：为盖满 9 格而入范围
    Rec,
}

impl SourceTag {
    pub fn as_str(&self) -> &'static str {
        match self {
            SourceTag::Fav => "fav",
            SourceTag::Top => "top",
            SourceTag::New => "new",
            SourceTag::Rec => "rec",
        }
    }
}

/// 心选位数量（每 provider，用户裁决）
pub const FAV_SLOTS: usize = 3;
/// 昨日用量位数量
pub const TOP_SLOTS: usize = 2;
/// 最新位数量
pub const NEW_SLOTS: usize = 1;

/// 9 格覆盖的邻接容差：能力/价位各 ±1 视为可代表（用户裁决：跨格覆盖、
/// 门槛放宽、格不允许空着）
fn cell_adjacent(a: (u8, u8), cell: (u8, u8)) -> bool {
    (i16::from(a.0) - i16::from(cell.0)).abs() <= 1 && (i16::from(a.1) - i16::from(cell.1)).abs() <= 1
}

/// 候选（由 server 侧从目录 + 标签 + 用量组装）
#[derive(Debug, Clone)]
pub struct ScopeCandidate {
    pub id: String,
    /// 能力档 0=basic / 1=standard / 2=powerful
    pub cap_rank: u8,
    /// 价位档 0=cheap / 1=mid / 2=premium
    pub price_rank: u8,
    /// 免费模型：直通车成员（调用方必然叠加），不占槽位、不被槽位选举
    pub free: bool,
    /// 发布时间（None = 不参与"最新"序）
    pub release_ts: Option<u64>,
    /// 昨日 token 用量（0 = 不参与"昨日"序）
    pub yesterday_tokens: u64,
    /// 稳定口径分（bench 档 × 用户权重 × 价位效率；无实时遥测）——REC 兜底序
    pub stable_score: f32,
}

/// 选范围：返回（当选成员 id, 主来源标签），按当选顺序。
/// 算法：① 用户心选（输入序，≤FAV_SLOTS，幽灵 id 跳过）→ ② 剩余心选位：
/// 昨日 token 降序（0 排除）→ 最新发布降序，标 Top/New（按实际来源）→
/// ③ 昨日位：昨日降序（排除已选）→ 最新兜底 → ④ 最新位：最新降序 →
/// ⑤ 9 格覆盖：覆盖成员 = 已选 ∪ 免费候选；某格无 ±1 邻接成员时，从
/// 非免费未选者取 stable_score 最高者以 Rec 入范围。同分按 id 升序。
pub fn select_provider_scope(
    cands: &[ScopeCandidate],
    favorites: &[String],
) -> Vec<(String, SourceTag)> {
    let mut out: Vec<(String, SourceTag)> = Vec::new();
    let mut seen: std::collections::HashSet<String> = std::collections::HashSet::new();
    let by_id = |id: &str| cands.iter().find(|c| c.id == id);

    // ① 用户心选：输入序、容量 FAV_SLOTS、幽灵静默跳过（免费模型可被亲选）
    let mut fav_count = 0usize;
    for id in favorites {
        if fav_count >= FAV_SLOTS {
            break;
        }
        if let Some(c) = by_id(id)
            && seen.insert(c.id.clone())
        {
            out.push((c.id.clone(), SourceTag::Fav));
            fav_count += 1;
        }
    }

    // 槽位选举序：昨日 token 降序优先（0 = 无来源），其次最新发布降序，同分 id 升序
    fn slot_order(a: &ScopeCandidate, b: &ScopeCandidate) -> std::cmp::Ordering {
        b.yesterday_tokens
            .cmp(&a.yesterday_tokens)
            .then(b.release_ts.cmp(&a.release_ts))
            .then(a.id.cmp(&b.id))
    }
    // 槽位来源标签：有昨日用量 = Top；只有发布时间 = New（标签反映来源不反映槽位）
    let slot_tag = |c: &ScopeCandidate| {
        if c.yesterday_tokens > 0 {
            SourceTag::Top
        } else {
            SourceTag::New
        }
    };

    // ② 剩余心选位 + ③ 昨日位：同一资格（有昨日用量或可判定"最新"），
    // 各自排除已选；②③ 用同一条序，④ 只按"最新"序（评审 H1：复用 ②③ 的
    // 昨日优先序会让第 6 个昨日量模型偷走 NEW 槽，真最新反而落选）
    fn has_source(c: &ScopeCandidate) -> bool {
        c.yesterday_tokens > 0 || c.release_ts.is_some()
    }
    fn has_release(c: &ScopeCandidate) -> bool {
        c.release_ts.is_some()
    }
    fn new_order(a: &ScopeCandidate, b: &ScopeCandidate) -> std::cmp::Ordering {
        b.release_ts.cmp(&a.release_ts).then(a.id.cmp(&b.id))
    }
    // 显式 fn 指针类型：三个 fn item 类型不同，数组需要统一成指针
    let slot_order: fn(&ScopeCandidate, &ScopeCandidate) -> std::cmp::Ordering = slot_order;
    let new_order: fn(&ScopeCandidate, &ScopeCandidate) -> std::cmp::Ordering = new_order;
    let has_source: fn(&ScopeCandidate) -> bool = has_source;
    let has_release: fn(&ScopeCandidate) -> bool = has_release;
    for (slots, eligible, order) in [
        (FAV_SLOTS.saturating_sub(fav_count), has_source, slot_order),
        (TOP_SLOTS, has_source, slot_order),
        (NEW_SLOTS, has_release, new_order),
    ] {
        let mut pool: Vec<&ScopeCandidate> =
            cands.iter().filter(|c| !c.free && !seen.contains(&c.id) && eligible(c)).collect();
        pool.sort_by(|a, b| order(a, b));
        for c in pool.into_iter().take(slots) {
            out.push((c.id.clone(), slot_tag(c)));
            seen.insert(c.id.clone());
        }
    }

    // ⑤ 9 格覆盖：覆盖成员 = 已选 ∪ 免费候选；空格由邻接的未选非免费者
    // 按 stable_score 降序补 Rec（格不允许空着——用户裁决）
    fn covers(out: &[(String, SourceTag)], cands: &[ScopeCandidate], cell: (u8, u8)) -> bool {
        out.iter().any(|(id, _)| {
            cands
                .iter()
                .find(|c| c.id == *id)
                .is_some_and(|c| cell_adjacent((c.cap_rank, c.price_rank), cell))
        }) || cands
            .iter()
            .filter(|c| c.free)
            .any(|c| cell_adjacent((c.cap_rank, c.price_rank), cell))
    }
    for cap in 0u8..3 {
        for price in 0u8..3 {
            let cell = (cap, price);
            if covers(&out, cands, cell) {
                continue;
            }
            let mut adjacent: Vec<&ScopeCandidate> = cands
                .iter()
                .filter(|c| !c.free && !seen.contains(&c.id) && cell_adjacent((c.cap_rank, c.price_rank), cell))
                .collect();
            adjacent.sort_by(|a, b| {
                b.stable_score
                    .partial_cmp(&a.stable_score)
                    .unwrap_or(std::cmp::Ordering::Equal)
                    .then(a.id.cmp(&b.id))
            });
            if let Some(c) = adjacent.first() {
                out.push((c.id.clone(), SourceTag::Rec));
                seen.insert(c.id.clone());
            }
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cand(id: &str, cap: u8, price: u8, free: bool, release: Option<u64>, ytok: u64, score: f32) -> ScopeCandidate {
        ScopeCandidate {
            id: id.to_string(),
            cap_rank: cap,
            price_rank: price,
            free,
            release_ts: release,
            yesterday_tokens: ytok,
            stable_score: score,
        }
    }

    fn tags_of(out: &[(String, SourceTag)], id: &str) -> Option<SourceTag> {
        out.iter().find(|(i, _)| i == id).map(|(_, t)| t.clone())
    }

    /// ① 心选优先：前 3 个亲选全中 Fav；第 4 个心选被容量拒绝
    #[test]
    fn favorites_elected_first_with_fav_tag() {
        let cands: Vec<_> = ["a", "b", "c", "d"]
            .iter()
            .map(|id| cand(id, 1, 1, false, None, 0, 1.0))
            .collect();
        let favs = ["a", "b", "c", "d"].iter().map(|s| s.to_string()).collect::<Vec<_>>();
        let out = select_provider_scope(&cands, &favs);
        assert_eq!(out.len(), 3, "fav cap 3: {out:?}");
        for id in ["a", "b", "c"] {
            assert_eq!(tags_of(&out, id), Some(SourceTag::Fav), "{id} must be Fav");
        }
        assert!(tags_of(&out, "d").is_none(), "4th favorite must be ignored: {out:?}");
    }

    /// ② 空心选位由昨日冠军补位——标签按来源记 Top（不是用户亲选，不冒充 Fav）
    #[test]
    fn empty_fav_slots_fill_from_yesterday_top() {
        let cands = vec![
            cand("fav1", 1, 1, false, None, 100, 1.0),
            cand("t1", 1, 1, false, None, 900, 1.0),
            cand("t2", 1, 1, false, None, 500, 1.0),
            cand("t3", 1, 1, false, None, 300, 1.0),
        ];
        let out = select_provider_scope(&cands, &["fav1".to_string()]);
        assert_eq!(tags_of(&out, "fav1"), Some(SourceTag::Fav));
        assert_eq!(tags_of(&out, "t1"), Some(SourceTag::Top), "highest ytok fills fav slot, tagged by source");
        assert_eq!(tags_of(&out, "t2"), Some(SourceTag::Top));
        assert_eq!(tags_of(&out, "t3"), Some(SourceTag::Top), "top slot election");
        assert_eq!(out.len(), 4);
    }

    /// 首日无昨日用量：空位链走到"最新入库"，标 New；范围不许为空
    #[test]
    fn day1_empty_yesterday_falls_through_to_new() {
        let cands = vec![
            cand("n1", 1, 1, false, Some(1_000), 0, 1.0),
            cand("n2", 1, 1, false, Some(2_000), 0, 1.0),
            cand("n3", 1, 1, false, Some(3_000), 0, 1.0),
        ];
        let out = select_provider_scope(&cands, &[]);
        assert_eq!(out.len(), 3, "scope must not be empty on day 1: {out:?}");
        for id in ["n1", "n2", "n3"] {
            assert_eq!(tags_of(&out, id), Some(SourceTag::New), "{id} filled from newest");
        }
    }

    /// 心选与昨日冠军重合：只出现一次，标最高优先来源 Fav
    #[test]
    fn dedup_highest_precedence_tag() {
        let cands = vec![
            cand("m", 1, 1, false, None, 1000, 1.0),
            cand("o1", 1, 1, false, None, 500, 1.0),
            cand("o2", 1, 1, false, None, 300, 1.0),
        ];
        let out = select_provider_scope(&cands, &["m".to_string()]);
        assert_eq!(out.iter().filter(|(i, _)| i == "m").count(), 1, "dedup: {out:?}");
        assert_eq!(tags_of(&out, "m"), Some(SourceTag::Fav));
        assert_eq!(tags_of(&out, "o1"), Some(SourceTag::Top));
    }

    /// 免费模型不占槽位也不被槽位选举（直通车由调用方叠加）
    #[test]
    fn free_models_not_elected_by_slots() {
        let cands = vec![
            cand("f1", 1, 0, true, Some(9_999), 5000, 1.0),
            cand("p1", 1, 1, false, None, 100, 1.0),
        ];
        let out = select_provider_scope(&cands, &[]);
        assert_eq!(tags_of(&out, "f1"), None, "free must not be slot-elected");
        assert_eq!(tags_of(&out, "p1"), Some(SourceTag::Top));
        assert_eq!(out.len(), 1);
    }

    /// 免费模型可以被用户亲选为心爱：照常 Fav（用户裁决优先于直通车规则）
    #[test]
    fn free_favorite_gets_fav_primary() {
        let cands = vec![cand("ff", 1, 0, true, None, 0, 1.0)];
        let out = select_provider_scope(&cands, &["ff".to_string()]);
        assert_eq!(tags_of(&out, "ff"), Some(SourceTag::Fav), "free favorite is still a favorite");
        // FREE 第二标签由调用方按 is_free 叠加渲染，不在本函数职责内
    }

    /// 幽灵心选 id（目录中不存在/已删除）：静默跳过
    #[test]
    fn ghost_favorite_id_is_ignored() {
        let cands = vec![cand("a", 1, 1, false, None, 0, 1.0)];
        let favs = vec!["ghost".to_string(), "a".to_string()];
        let out = select_provider_scope(&cands, &favs);
        assert_eq!(out, vec![("a".to_string(), SourceTag::Fav)]);
    }

    /// ④ 最新位必须是"真最新"：昨日活跃模型 ≥5 时心选/昨日位先吃掉前 5，
    /// 新入库（ytok=0）模型仍应凭 release_ts 拿到 NEW 槽（评审 H1）
    #[test]
    fn newest_slot_goes_to_newest_release_not_leftover_yesterday() {
        let mut cands: Vec<ScopeCandidate> = Vec::new();
        // 6 个昨日活跃模型：p1..p6，ytok 递减
        for (i, tok) in [900u64, 800, 700, 600, 500, 400].iter().enumerate() {
            cands.push(cand(&format!("p{i}"), 1, 1, false, Some(1_000 + i as u64), *tok, 1.0));
        }
        // 真最新入库：release 最大、无昨日用量
        cands.push(cand("fresh", 1, 1, false, Some(9_999), 0, 1.0));
        let out = select_provider_scope(&cands, &[]);
        assert_eq!(tags_of(&out, "fresh"), Some(SourceTag::New), "newest release must take the NEW slot: {out:?}");
        // 3 心选位（按来源标 Top）+ 2 昨日位 + 1 最新位 = 6 个入选
        assert_eq!(out.len(), 6, "3+2+1 slots fully elected: {out:?}");
        assert_eq!(tags_of(&out, "p0"), Some(SourceTag::Top));
        assert_eq!(tags_of(&out, "p4"), Some(SourceTag::Top), "5th yesterday-active fills the last TOP slot");
        assert!(tags_of(&out, "p5").is_none(), "6th yesterday-active must not steal the NEW slot");
    }

    /// ⑤ REC 兜底：范围盖不住的格由稳定分最高的邻接候选补齐；免费成员
    /// 已覆盖的格不再补；全格盖满后不再多选
    #[test]
    fn rec_backfills_empty_cells_by_stable_score() {
        let cands = vec![
            cand("strong1", 2, 0, false, None, 0, 0.9),
            cand("strong2", 2, 1, false, None, 0, 0.7),
            cand("low1", 0, 0, false, None, 0, 0.5),
            cand("extra", 0, 1, false, None, 0, 0.1),
            cand("free1", 0, 2, true, None, 0, 0.0),
        ];
        let out = select_provider_scope(&cands, &[]);
        // free1 覆盖 (0,2) 邻域 → extra 不需要
        assert_eq!(tags_of(&out, "extra"), None, "cell (0,2) covered by free member");
        // (0,0)/(0,1)/(1,*) 由 low1 覆盖 → low1 以 Rec 入选
        assert_eq!(tags_of(&out, "low1"), Some(SourceTag::Rec));
        // (2,0) 空格 → 邻接者中 stable_score 最高 = strong1；strong2 补 (2,2)
        assert_eq!(tags_of(&out, "strong1"), Some(SourceTag::Rec));
        assert_eq!(tags_of(&out, "strong2"), Some(SourceTag::Rec));
        assert_eq!(out.len(), 3, "no extra elections once all cells covered: {out:?}");
        // 全格覆盖校验（成员 = 当选 ∪ 免费候选）
        let members: Vec<(u8, u8)> = cands
            .iter()
            .filter(|c| out.iter().any(|(i, _)| i == &c.id) || c.free)
            .map(|c| (c.cap_rank, c.price_rank))
            .collect();
        for cap in 0u8..3 {
            for price in 0u8..3 {
                assert!(
                    members.iter().any(|m| cell_adjacent(*m, (cap, price))),
                    "cell ({cap},{price}) must be covered"
                );
            }
        }
    }
}

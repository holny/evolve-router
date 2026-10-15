use crate::types::*;
use std::collections::HashMap;

pub use crate::types::CandidateScore;

pub fn tier_for_domain(domain: Domain, tiers: &Tiers, difficulty: f32) -> f32 {
    let d = (difficulty / 3.0).clamp(0.0, 1.0);
    match domain {
        Domain::Code => tiers.coding * (1.0 - d * 0.4) + tiers.reasoning * (d * 0.4),
        Domain::MathLogic => tiers.reasoning,
        Domain::Data => tiers.coding * 0.7 + tiers.reasoning * 0.3,
        Domain::AgentOps => tiers.agentic,
        Domain::Writing | Domain::Lookup | Domain::Chitchat | Domain::Other => {
            1.0 - d * 0.2 + 0.2 * d * tiers.reasoning
        }
    }
}

pub fn quality(m: &ModelRecord, j: &JudgmentSet, difficulty_eff: f32) -> f32 {
    let tier = tier_for_domain(j.domain, &m.tiers, difficulty_eff);
    let requirement = (difficulty_eff / 3.0).clamp(0.0, 1.0);
    let steep = 1.0 + 2.0 * requirement;
    let tier_st = tier.powf(steep);
    let q = 1.0 - requirement * (1.0 - tier_st);
    if j.high_stakes > 0.6 {
        q * (0.6 + 0.4 * tier.max(m.tiers.reasoning))
    } else {
        q
    }
}

/// 质量门槛（2026-10 修正方向）：**难度越低门槛越松**——简单任务谁都能干，
/// 让成本主导（便宜模型可用）；难度越高门槛越高，只留给强模型。
/// 旧公式（1.0 - 0.45·d/3）方向相反：L1 门槛 0.925 会把便宜弱模型全挡掉，
/// 违背"简单任务用便宜模型"（实证：trivial → 被挡 → 选了 medium 层）。
/// d=0 → 0.50（形同不设限，成本主导）；d=3 → 0.85（仅强模型可过）。
pub fn quality_floor(difficulty_eff: f32) -> f32 {
    0.50 + 0.35 * (difficulty_eff / 3.0).clamp(0.0, 1.0)
}

/// 配额内偏好幅度上限（用户裁决 2026-10）：订阅（配额）模型在配额充裕时获得的
/// 分数加成上限。配额已预付、用不掉就是浪费 → 同等条件下略优先用掉；幅度刻意
/// 保持很小，明显更优的按量（API 计费）模型仍会胜出，不掐断其使用机会。
/// 实际加成随方案消耗率线性衰减（见 engine 的配额头寸处理），越接近软阈值越小。
pub const PLAN_QUOTA_BONUS: f32 = 0.05;

/// 近期使用偏好上限（用户裁决 2026-10，缓存前缀亲和）：同难度层内优先复用
/// 近期用过的模型（prompt 缓存前缀仍热），减少模型跳变。加成随时长线性衰减
/// （窗口 2h），上限 5% —— 不干扰"简单任务用便宜模型"的大原则（难度闸与
/// 成本权重不变）。
pub const RECENCY_BONUS: f32 = 0.05;
pub const RECENCY_WINDOW_MS: u64 = 2 * 3600 * 1000;

/// 难度-费用联动（用户裁决：简单任务用便宜模型，复杂任务可以用贵的模型）。
/// 难度 0：成本权重 ×2、质量权重 ×0.75（省钱优先）；
/// 难度 3：成本权重 ×0.4、质量权重满额（成本让位，质量主导）。
/// 返回归一化后的五因子 [质量, 速度, 成本, 可靠, 余量]。
pub fn difficulty_weights(w: &PolicyWeights, difficulty_eff: f32) -> [f32; 5] {
    let [wq, ws, wc, wr, wh] = w.normalized();
    let d = (difficulty_eff / 3.0).clamp(0.0, 1.0);
    let cost_boost = 2.0 - 1.6 * d;
    let quality_scale = 0.75 + 0.25 * d;
    let mut v = [wq * quality_scale, ws, wc * cost_boost, wr, wh];
    let sum: f32 = v.iter().sum();
    for x in v.iter_mut() {
        *x /= sum.max(1e-6);
    }
    v
}

/// ④ 配额感知权重（BaRP Eq.1）：偏好向量随配额状态线性调整，纯运行时无重训。
/// tight ∈ [0,1]：候选集中配额紧张（剩余 < 本次需求若干倍）的比例。
/// 配额充裕（tight→0）保持原权重（质量优先）；紧张（tight→1）成本权重上调、
/// 质量轻微让位，平滑映射不跳变；无配额数据（tight=0）不干预。
pub fn quota_aware_weights(w: &[f32; 5], tight: f32) -> [f32; 5] {
    let t = tight.clamp(0.0, 1.0);
    let mut v = *w;
    v[2] *= 1.0 + 1.5 * t;
    v[0] *= 1.0 - 0.25 * t;
    let sum: f32 = v.iter().sum();
    for x in v.iter_mut() {
        *x /= sum.max(1e-6);
    }
    v
}

pub fn blended_price(m: &ModelRecord, est_input: u64, est_output: u64) -> f32 {
    match m.cost {
        Some(c) => (est_input as f32 / 1e6) * c.input + (est_output as f32 / 1e6) * c.output,
        None => 0.0,
    }
}

/// 该模型近期实测缓存命中率（无样本 None）——S4 缓存经济决策依据
pub fn cache_hit_rate(tel: Option<&ModelTelemetry>) -> Option<f32> {
    let samples = tel.and_then(|x| x.recent.as_ref())?;
    let (mut in_s, mut hit) = (0f64, 0f64);
    for s in samples.iter().rev().take(20) {
        in_s += s.in_tok as f64;
        hit += s.cached_tok as f64;
    }
    if in_s <= 0.0 { return None; }
    Some(((hit / in_s) as f32).clamp(0.0, 1.0))
}

/// 近期实测比例：(缓存命中率, 输出/输入比)。无样本用保守默认（50% 命中、30% 输出）
fn recent_ratios(tel: Option<&ModelTelemetry>) -> (f32, f32) {
    if let Some(samples) = tel.and_then(|x| x.recent.as_ref()) {
        let (mut in_s, mut hit, mut out_s, mut n) = (0f64, 0f64, 0f64, 0f64);
        for s in samples.iter().rev().take(20) {
            in_s += s.in_tok as f64;
            hit += s.cached_tok as f64;
            out_s += s.out_tok as f64;
            n += 1.0;
        }
        if in_s > 0.0 && n > 0.0 {
            return (((hit / in_s) as f32).clamp(0.0, 0.99), (out_s / in_s) as f32);
        }
    }
    (0.5, 0.3)
}

/// S1 有效成本（估算，provider 币种金额）：官方价目优先——含**缓存命中价**与
/// **峰谷折扣**（DeepSeek 命中价约为未命中 1/50，非高峰半价，影响远超选型直觉）；
/// 无官方价目回退目录单价。返回 None = 无任何成本信息（评分按成本中性处理）
pub fn est_cost(
    m: &ModelRecord,
    est_input: u64,
    est_output: u64,
    tel: Option<&ModelTelemetry>,
) -> Option<f32> {
    if let Some(s) = crate::provider_info::price_for(&m.base_url, &m.id) {
        let (hit_rate, _) = recent_ratios(tel);
        let in_eff = s.in_miss * (1.0 - hit_rate) + s.in_hit * hit_rate;
        let peak = crate::provider_info::peak_factor_now(&m.base_url);
        return Some(((est_input as f32 / 1e6) * in_eff + (est_output as f32 / 1e6) * s.out) * peak);
    }
    m.cost.map(|c| (est_input as f32 / 1e6) * c.input + (est_output as f32 / 1e6) * c.output)
}

#[test]
fn official_price_sheet_effective_cost() {
    use crate::types::{Cost, ModelRecord, Source};
    let mk = |id: &str, base: &str| ModelRecord {
        id: id.into(),
        provider: "p".into(),
        base_url: base.into(),
        upstream_model: id.into(),
        source: Source::Remote,
        ..Default::default()
    };
    // 官方价目覆盖目录单价：deepseek flash 命中价极低 → 有效成本远低于线性价
    let mut flash = mk("deepseek-flash", "https://api.deepseek.com");
    flash.cost = Some(Cost { input: 99.0, output: 99.0 }); // 故意给错的目录价
    let no_tel = None;
    let c = est_cost(&flash, 100_000, 30_000, no_tel).unwrap();
    // 50% 命中默认：in_eff = 1*0.5 + 0.02*0.5 = 0.51 → 0.1M*0.51 + 0.03M*4 = 0.171 元（空闲档）
    assert!(c < 0.5, "official sheet should dominate: {c}");
    // pro 显著更贵
    let mut pro = mk("deepseek-v4-pro", "https://api.deepseek.com");
    pro.cost = None;
    let cp = est_cost(&pro, 100_000, 30_000, no_tel).unwrap();
    assert!(cp > c * 2.0, "pro should cost several times flash: {cp} vs {c}");
    // 无价目且无目录价 → None（成本中性，不冒充便宜）
    let unknown = mk("x-9b", "http://127.0.0.1:9999/v1");
    assert!(est_cost(&unknown, 1000, 100, no_tel).is_none());
}

#[test]
fn quota_aware_weights_shift_cost_when_tight() {
    let base = [0.35f32, 0.15, 0.25, 0.15, 0.10];
    let calm = quota_aware_weights(&base, 0.0);
    assert_eq!(calm, base, "no quota pressure must leave weights untouched");
    let tight = quota_aware_weights(&base, 1.0);
    let sum: f32 = tight.iter().sum();
    assert!((sum - 1.0).abs() < 1e-4, "weights must stay normalized, got {sum}");
    assert!(tight[2] > calm[2], "cost weight must rise under quota pressure");
    assert!(tight[0] < calm[0], "quality weight must yield slightly");
}

pub fn score_all(
    candidates: &[&ModelRecord],
    j: &JudgmentSet,
    difficulty_eff: f32,
    est_input: u64,
    est_output: u64,
    weights: &PolicyWeights,
    telemetry: &TelemetrySnapshot,
) -> Vec<CandidateScore> {
    // 兼容旧签名：不带层级乘子（引擎侧用 score_all_tiered 传比例控制器乘子）
    score_all_tiered(
        candidates, j, difficulty_eff, est_input, est_output, weights, telemetry,
        &std::collections::HashMap::new(), &[1.0; 3],
    )
}

/// 带层级乘子的评分（S5 比例控制器）：`tier_penalty[i]` 作用于第 i 层（0 cheap/1 medium/2 premium）
/// 的成本因子——超标层乘子 >1（抑制），不足层 <1（偏好）。
#[allow(clippy::too_many_arguments)]
pub fn score_all_tiered(
    candidates: &[&ModelRecord],
    j: &JudgmentSet,
    difficulty_eff: f32,
    est_input: u64,
    est_output: u64,
    weights: &PolicyWeights,
    telemetry: &TelemetrySnapshot,
    price_tier_of: &std::collections::HashMap<String, u8>,
    tier_penalty: &[f32; 3],
) -> Vec<CandidateScore> {
    let [wq, ws, wc, wr, wh] = weights.normalized();
    // Known-cost models only anchor the price floor; unknown-cost models
    // are cost-neutral (0.5) — never rewarded as if they were cheapest.
    // 成本锚点：payg 模型用官方价目有效成本（缓存命中率 + 峰谷），
    // 无价目回退目录单价；订阅模型走配额口径（下方 hints）
    let min_price = candidates
        .iter()
        .filter(|m| !m.plan)
        .filter_map(|m| est_cost(m, est_input, est_output, telemetry.get(&m.id)))
        .fold(f32::MAX, f32::min);
    // 订阅模型配额消耗强度（官方系数折算）：**按方案分组取最小**——
    // 不同方案的系数不可比（智谱积分 vs Go 美元），全局取最小会稀释基准
    let mut hints: HashMap<&str, f32> = HashMap::new();
    let mut scheme_min: HashMap<String, f32> = HashMap::new();
    for m in candidates.iter().filter(|m| m.plan) {
        if let Some(h) = crate::plans::model_price_hint(&m.base_url, &m.id) {
            // 峰谷折扣（provider_info）：非高峰半价 → 有效成本更低（错峰优先）
            let h = h * crate::provider_info::peak_factor_now(&m.base_url);
            hints.insert(m.id.as_str(), h);
            let key = crate::plans::plan_key_for(&m.base_url)
                .unwrap_or(m.base_url.as_str())
                .to_string();
            scheme_min.entry(key).and_modify(|v| *v = v.min(h)).or_insert(h);
        }
    }

    let mut scores: Vec<CandidateScore> = candidates
        .iter()
        .map(|m| {
            let q = quality(m, j, difficulty_eff);
            let prior_speed = m.speed_tier.clamp(0.0, 1.0);
            let s = match telemetry.get(&m.id).and_then(|t| t.speed_obs) {
                Some(obs) => 0.7 * prior_speed + 0.3 * obs.clamp(0.0, 1.0),
                None => prior_speed,
            };
            let c = if m.plan {
                // 订阅套餐：配额内边际成本≈0，但配额消耗速率按官方系数折算
                // （同方案内旗舰烧配额是轻量模型的数倍——简单任务便宜模型胜出）
                match hints.get(m.id.as_str()) {
                    Some(hint) => {
                        let key = crate::plans::plan_key_for(&m.base_url)
                            .unwrap_or(m.base_url.as_str());
                        let min = scheme_min.get(key).copied().unwrap_or(*hint);
                        let tier = price_tier_of.get(&m.id).copied().unwrap_or(1) as usize;
                        let hint = hint * tier_penalty[tier.min(2)];
                        (min / hint.max(1e-6)).clamp(0.05, 1.0)
                    }
                    None => 1.0,
                }
            } else {
                match est_cost(m, est_input, est_output, telemetry.get(&m.id)) {
                    Some(p) => {
                        let tier = price_tier_of.get(&m.id).copied().unwrap_or(1) as usize;
                        let price = (p * tier_penalty[tier.min(2)]).max(1e-6);
                        (min_price / price).clamp(0.0, 1.0)
                    }
                    None => 0.5, // 无成本信息：成本中性，不冒充最便宜
                }
            };
            // 可靠性分层融合（用户裁决：近30/10 比较重要但非主参考）：
            // 长期基线 50% + 近30次 30% + 近10次 20%，缺层权重归并——
            // 持续性劣化与瞬时抖动分层感知，单次故障不过度反应
            let t = telemetry.get(&m.id);
            let r_long = t.and_then(|x| x.reliability).unwrap_or(0.7).clamp(0.0, 1.0);
            let r30 = t.and_then(|x| x.rel_30);
            let r10 = t.and_then(|x| x.rel_10);
            let r = match (r30, r10) {
                (Some(a), Some(b)) => 0.5 * r_long + 0.3 * a + 0.2 * b,
                (Some(a), None) => 0.6 * r_long + 0.4 * a,
                (None, Some(b)) => 0.7 * r_long + 0.3 * b,
                (None, None) => r_long,
            }
            .clamp(0.0, 1.0);
            let h = m
                .context_window
                .map(|w| ((w as f64 - est_input as f64 - est_output as f64) / w as f64).clamp(0.0, 1.0) as f32)
                .unwrap_or(0.0);
            let mut score = wq * q + ws * s + wc * c + wr * r + wh * h;
            // user weight x learned bias: soft multiplier, never decisive
            let user_w = m.weight.unwrap_or(1.0).clamp(0.2, 3.0);
            let bias = telemetry
                .get(&m.id)
                .and_then(|t| t.learned_bias)
                .unwrap_or(1.0)
                .clamp(0.7, 1.3);
            score *= (user_w * bias).sqrt().clamp(0.4, 1.8);
            // 近期使用偏好（缓存前缀亲和，见 RECENCY_BONUS）：同层内优先复用
            // 近期用过的模型，减少跳变；加成随窗口内时长线性衰减
            if let Some(latest_ts) = telemetry
                .get(&m.id)
                .and_then(|t| t.recent.as_ref())
                .and_then(|r| r.last())
                .map(|s| s.ts)
                .filter(|ts| *ts > 0)
            {
                let age = crate::engine::now_epoch_ms().saturating_sub(latest_ts);
                if age < RECENCY_WINDOW_MS {
                    score *= 1.0 + RECENCY_BONUS * (1.0 - age as f32 / RECENCY_WINDOW_MS as f32);
                }
            }
            CandidateScore { model_id: m.id.clone(), score, q, s, c, r, h, uw: user_w, bias, qp: 1.0 }
        })
        .collect();

    // EvoRoute paper §4.3 Pareto-optimal filtration：剔除被支配模型——
    // 若另一模型在全部五因子上 ≥ 且至少一因子 >，则该模型被支配（不可能最优）
    // 这防止加权求和的 quirk 选中全面劣势的模型
    filter_pareto_dominated(&mut scores);
    scores
}

/// Pareto 支配检测（EvoRoute paper §4.3）：移除在所有五因子上被其他候选
/// 弱等于或严格劣于的模型。O(n²) 但 n ≤ 20（hard constraints 之后）。
fn filter_pareto_dominated(scores: &mut Vec<CandidateScore>) {
    if scores.len() <= 1 { return; }
    let dominated: Vec<bool> = scores.iter().enumerate().map(|(i, a)| {
        scores.iter().enumerate().any(|(j, b)| {
            j != i
                && b.q >= a.q && b.s >= a.s && b.c >= a.c && b.r >= a.r && b.h >= a.h
                && (b.q > a.q || b.s > a.s || b.c > a.c || b.r > a.r || b.h > a.h)
        })
    }).collect();
    let mut kept: Vec<CandidateScore> = Vec::new();
    for (i, s) in scores.drain(..).enumerate() {
        if !dominated[i] { kept.push(s); }
    }
    if !kept.is_empty() { *scores = kept; }
}

pub fn context_fits(m: &ModelRecord, est_input: u64, max_output: u64) -> Result<(), &'static str> {
    // None window: the engine's proven-bound check (telemetry max_accepted)
    // is the single gate — reaching here means the model is proven, so no
    // declared-window check applies.
    let Some(window) = m.context_window else { return Ok(()) };
    let need = (est_input as f64 * 1.1) as u64 + max_output;
    if need <= window {
        Ok(())
    } else {
        Err("context window too small")
    }
}

#[cfg(test)]
mod difficulty_cost_tests {
    use super::*;

    fn plan_model(id: &str, tier: f32) -> ModelRecord {
        ModelRecord {
            id: id.into(),
            provider: "zhipu".into(),
            base_url: "https://open.bigmodel.cn/api/coding/paas/v4".into(),
            api_key: Some("k".into()),
            upstream_model: id.into(),
            context_window: Some(1_000_000),
            max_output: 4096,
            cost: None,
            plan: true,
            tiers: Tiers { reasoning: tier, coding: tier, vision: 0.0, agentic: tier },
            speed_tier: 0.5,
            source: Source::User,
            ..Default::default()
        }
    }

    fn judge(domain: Domain, difficulty: f32) -> JudgmentSet {
        JudgmentSet {
            domain,
            domain_confidence: 1.0,
            difficulty,
            difficulty_confidence: 1.0,
            needs_vision: 0.0,
            is_trivial: 0.0,
            tool_heavy: 0.0,
            high_stakes: 0.0,
            session_relevance: 0.0,
            session_depth: 0.0,
            judge_source: "test",
        route_recommendation: None,
        route_recommendation_confidence: 0.0,
        }
    }

    fn rank(eff: [f32; 5], difficulty: f32) -> Vec<String> {
        let flash = plan_model("zhipuai-coding-plan/glm-5.3-flash", 0.70);
        let big = plan_model("zhipuai-coding-plan/glm-5.3", 0.95);
        let cands = vec![&flash, &big];
        let j = judge(Domain::Code, difficulty);
        let w = PolicyWeights {
            quality: eff[0], speed: eff[1], cost: eff[2], stability: eff[3], headroom: eff[4],
        };
        let tel = TelemetrySnapshot::new();
        let mut out = score_all(&cands, &j, difficulty, 10_000, 4_096, &w, &tel);
        out.sort_by(|a, b| b.score.partial_cmp(&a.score).unwrap_or(std::cmp::Ordering::Equal));
        out.iter().map(|s| s.model_id.clone()).collect()
    }

    #[test]
    fn dbg_scores() {
        let flash = plan_model("zhipuai-coding-plan/glm-5.3-flash", 0.70);
        let big = plan_model("zhipuai-coding-plan/glm-5.3", 0.95);
        let cands = vec![&flash, &big];
        let j = judge(Domain::Code, 2.5);
        let eff = difficulty_weights(&PolicyWeights::balanced(), 2.5);
        println!("eff={:?}", eff);
        let w = PolicyWeights { quality: eff[0], speed: eff[1], cost: eff[2], stability: eff[3], headroom: eff[4] };
        let tel = TelemetrySnapshot::new();
        let out = score_all(&cands, &j, 2.5, 10_000, 4_096, &w, &tel);
        for s in &out { println!("{} score={:.4} q={:.4} s={:.4} c={:.4} r={:.4} h={:.4}", s.model_id, s.score, s.q, s.s, s.c, s.r, s.h); }
    }

    /// 用户裁决：简单任务便宜模型胜（成本权重 ×2，且 flash 配额消耗仅 1/3）
    #[test]
    fn easy_task_prefers_credit_light_model() {
        let eff = difficulty_weights(&PolicyWeights::balanced(), 0.3);
        assert_eq!(rank(eff, 0.3)[0], "zhipuai-coding-plan/glm-5.3-flash");
    }

    /// 复杂任务质量主导：成本让位（×0.4），旗舰胜出
    #[test]
    fn hard_task_prefers_capable_model_despite_cost() {
        let eff = difficulty_weights(&PolicyWeights::balanced(), 2.5);
        assert_eq!(rank(eff, 2.5)[0], "zhipuai-coding-plan/glm-5.3");
    }

    /// 难度联动本身：成本权重单调递减
    #[test]
    fn cost_weight_monotonically_decreases_with_difficulty() {
        let lo = difficulty_weights(&PolicyWeights::balanced(), 0.0)[2];
        let mid = difficulty_weights(&PolicyWeights::balanced(), 1.5)[2];
        let hi = difficulty_weights(&PolicyWeights::balanced(), 3.0)[2];
        assert!(lo > mid && mid > hi, "lo={lo} mid={mid} hi={hi}");
    }

    /// 近期使用偏好（缓存前缀亲和，用户裁决 2026-10）：同分候选中近期用过的
    /// 获得小幅加成（减少跳变），幅度 <6%
    #[test]
    fn recency_prefers_recently_used_model() {
        use crate::types::{ModelTelemetry, ReqSample};
        let now = crate::engine::now_epoch_ms();
        let mk = |id: &str| ModelRecord {
            id: id.into(),
            provider: "zhipu".into(),
            base_url: "https://open.bigmodel.cn/api/coding/paas/v4".into(),
            api_key: Some("k".into()),
            upstream_model: id.into(),
            context_window: Some(1_000_000),
            max_output: 4096,
            plan: true,
            tiers: Tiers { reasoning: 0.8, coding: 0.8, vision: 0.0, agentic: 0.8 },
            speed_tier: 0.5,
            source: Source::User,
            ..Default::default()
        };
        let cands = vec![mk("plan-a"), mk("plan-b")];
        let j = judge(Domain::Code, 1.0);
        let w = PolicyWeights {
            quality: 0.35, speed: 0.15, cost: 0.25, stability: 0.15, headroom: 0.10,
        };
        let mut tel = TelemetrySnapshot::new();
        tel.insert(
            "plan-a".into(),
            ModelTelemetry {
                recent: Some(vec![ReqSample {
                    ts: now, ttft_ms: 100, total_ms: 400,
                    in_tok: 10, cached_tok: 0, out_tok: 5,
                    ok: true, tools_total: 0, tools_ok: 0,
                }]),
                ..Default::default()
            },
        );
        let refs: Vec<&ModelRecord> = cands.iter().collect();
        let scores = score_all(&refs, &j, 1.0, 10_000, 4_000, &w, &tel);
        let sa = scores.iter().find(|s| s.model_id == "plan-a").unwrap().score;
        let sb = scores.iter().find(|s| s.model_id == "plan-b").unwrap().score;
        assert!(sa > sb, "recently used model gets the affinity bonus: {sa} vs {sb}");
        assert!(sa / sb < 1.06, "bonus must stay small (<6%): {sa} vs {sb}");
    }
}

#[cfg(test)]
mod layered_reliability_tests {
    

    /// 融合公式：三层全在 5:3:2；缺层归并
    #[test]
    fn fusion_weights() {
        let f = |rel30: Option<f32>, rel10: Option<f32>| {
            let r_long = 0.8;
            match (rel30, rel10) {
                (Some(a), Some(b)) => 0.5 * r_long + 0.3 * a + 0.2 * b,
                (Some(a), None) => 0.6 * r_long + 0.4 * a,
                (None, Some(b)) => 0.7 * r_long + 0.3 * b,
                (None, None) => r_long,
            }
        };
        // 基线 0.8 + 近层全 1.0 → 0.5×0.8+0.3+0.2 = 0.9（近层拉回但不淹没基线）
        assert!((f(Some(1.0), Some(1.0)) - 0.9).abs() < 1e-5);
        // 近窗骤降会拉低但不淹没长期基线（0.8 基线 + 近10全败）
        let v = f(Some(0.8), Some(0.0));
        assert!(v < 0.8 && v > 0.5, "v={v}");
        // 全缺 → 纯长期
        assert!((f(None, None) - 0.8).abs() < 1e-5);
    }
}

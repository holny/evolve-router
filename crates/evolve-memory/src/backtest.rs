//! ⑦ 策略回放验证（MERA 灵感）：在历史事件流上 A/B 对比策略参数。
//!
//! 原理：events.jsonl 里每条路由事件带完整 scoreboard（公式分）与实际 outcome。
//! bandit 反馈的根本局限是"只有被选模型有该次结果"——因此模型真实质量用
//! **回放窗内的全局表现**（成功率 + 延迟的融合 proxy）作 ground truth 代理。
//! 策略收益 = Σ(策略重排后 top1 模型的全局质量 proxy)；两策略在同一事件集上
//! 对比，均势或更优才建议采纳（宁保守，不回滚到更差）。

use crate::flywheel::outcome_reward;
use crate::strategy::StrategyParams;
use serde::Serialize;
use std::collections::HashMap;
use std::io::BufRead;
use std::path::Path;

const MIN_MODEL_SAMPLES: usize = 5;
const SPEED_LN_MAX: f32 = 11.512_925; // ln(100_000)

#[derive(Debug, Serialize)]
pub struct StrategyScore {
    pub version: u32,
    pub ok_weight: f32,
    pub bias_gain: f32,
    /// 重放路由的平均质量 proxy（0-1）
    pub avg_quality: f32,
    /// A1 PGR（RouteLLM Performance Gap Recovered）：策略恢复了多少
    /// "oracle 最优与 oracle 最差"之间的质量差距，[0,1] 归一。
    /// 比裸均值更有说服力：0 = 与最差选择同劣，1 = 达到事后最优
    pub pgr: f32,
    /// 策略 top1 与实际 chosen 不同的比例（干预率）
    pub intervention_rate: f32,
    pub events_scored: u32,
}

#[derive(Debug, Serialize)]
pub struct ReplayReport {
    pub events_replayed: u32,
    pub models_with_outcomes: usize,
    /// A = 当前/默认参数，B = 候选参数
    pub a: StrategyScore,
    pub b: StrategyScore,
    pub verdict: String,
    /// A3b LLM-as-a-Judge 评分统计（未启用 judge 时全零）
    pub judge: JudgeOutcome,
}

struct EvRow {
    board: Vec<(String, f32)>,
    chosen: String,
    ok: bool,
    total_ms: u64,
    /// A3b LLM-as-a-Judge：resp_digest 摘要（opt-in 落盘才有）
    resp_digest: Option<String>,
    /// ②a 阈值标定：判定快照（judgment + difficulty_eff + explore_gate）——
    /// confidence_gate 灵敏度扫描的数据源。事件流自 2026-10 起带全量
    /// judgment（relay 的 decision.extras），旧事件缺失时为 None、不参与扫描
    j: Option<JudgmentSnap>,
}

/// gate_sweep / gate_replay 所需的事件侧判定快照
#[derive(Debug, Clone, Copy)]
pub struct JudgmentSnap {
    pub domain_confidence: f32,
    pub difficulty_confidence: f32,
    /// 决策时生效难度（顶层 difficulty_eff，旧事件回退 judgment.difficulty）
    pub difficulty_eff: f32,
    pub high_stakes: f32,
    /// 本次是否实际发生了探索（off-policy 对照用）
    pub explored: bool,
    /// 评分候选数（探索门控要求 > 1）
    pub candidates: usize,
    /// 决策时的实际探索门控结果（explore_gate.confidence_ok）。
    /// None = 事件缺 explore_gate（旧事件/单候选）——gate_replay 据此
    /// 区分"实际允许探索"与"实际门控未知"两种 off-policy 区制
    pub actual_conf_ok: Option<bool>,
}

fn parse_events(path: &Path, limit: usize) -> anyhow::Result<Vec<EvRow>> {
    use std::fs::File;
    let file = File::open(path)
        .map_err(|e| anyhow::anyhow!("无法打开事件流 {}: {e}", path.display()))?;
    let mut rows = Vec::new();
    for line in std::io::BufReader::new(file).lines().map_while(Result::ok) {
        let Ok(v) = serde_json::from_str::<serde_json::Value>(&line) else {
            continue;
        };
        if v.get("kind").and_then(|k| k.as_str()) != Some("chat") {
            continue;
        }
        let Some(board_v) = v.get("scoreboard").and_then(|s| s.as_array()) else {
            continue;
        };
        let board: Vec<(String, f32)> = board_v
            .iter()
            .filter_map(|s| {
                let id = s.get("model_id")?.as_str()?.to_string();
                let score = s.get("score")?.as_f64()? as f32;
                Some((id, score))
            })
            .collect();
        if board.is_empty() {
            continue;
        }
        let Some(chosen) = v.get("chosen").and_then(|c| c.as_str()).map(String::from) else {
            continue;
        };
        let status = v.get("status").and_then(|s| s.as_u64()).unwrap_or(0) as u16;
        let total_ms = v.get("total_ms").and_then(|s| s.as_u64()).unwrap_or(0);
        // 与写入端对齐：既看类型化布尔字段（degenerate/empty_response）也看
        // flags 数组（含 refusal）——此前只查 flags 且漏掉 refusal
        let bad_quality = v
            .get("quality")
            .map(|q| {
                let degenerate = q.get("degenerate").and_then(|b| b.as_bool()).unwrap_or(false);
                let empty = q.get("empty_response").and_then(|b| b.as_bool()).unwrap_or(false);
                let flags_bad = q
                    .get("flags")
                    .and_then(|f| f.as_array())
                    .map(|flags| {
                        flags.iter().filter_map(|f| f.as_str()).any(|f| {
                            f == "degenerate" || f == "empty_stop" || f == "no_choices" || f == "refusal"
                        })
                    })
                    .unwrap_or(false);
                degenerate || empty || flags_bad
            })
            .unwrap_or(false);
        let j = parse_judgment_snap(&v, board.len());
        rows.push(EvRow {
            board,
            chosen,
            ok: status == 200 && !bad_quality,
            total_ms,
            resp_digest: v
                .get("resp_digest")
                .and_then(|d| d.as_str())
                .filter(|s| !s.is_empty())
                .map(String::from),
            j,
        });
    }
    // review#2 修复：events.jsonl 按时间序追加——取尾部才是"最近 N 条"，
    // 文件头是最旧的流量（大日志下回放陈旧数据会让 A/B 裁决失真）
    if rows.len() > limit {
        rows.drain(..rows.len() - limit);
    }
    Ok(rows)
}

/// 从事件 JSON 提取 gate_sweep 所需的判定快照；任一关键字段缺失即返回 None
/// （旧事件 / sticky 快路径外的异常形状一律不参与扫描，宁缺毋假）
fn parse_judgment_snap(v: &serde_json::Value, candidates: usize) -> Option<JudgmentSnap> {
    let j = v.get("judgment")?;
    let domain_confidence = j.get("domain_confidence")?.as_f64()? as f32;
    let difficulty_confidence = j.get("difficulty_confidence")?.as_f64()? as f32;
    let high_stakes = j.get("high_stakes")?.as_f64()? as f32;
    // 决策难度取顶层 difficulty_eff（评分序生效值），旧事件回退 judgment.difficulty
    let difficulty_eff = v
        .get("difficulty_eff")
        .and_then(|x| x.as_f64())
        .or_else(|| j.get("difficulty").and_then(|x| x.as_f64()))? as f32;
    let explored = v
        .get("explore_gate")
        .and_then(|g| g.get("explored"))
        .and_then(|b| b.as_bool())
        .unwrap_or(false);
    let actual_conf_ok = v
        .get("explore_gate")
        .and_then(|g| g.get("confidence_ok"))
        .and_then(|b| b.as_bool());
    Some(JudgmentSnap {
        domain_confidence,
        difficulty_confidence,
        difficulty_eff,
        high_stakes,
        explored,
        candidates,
        actual_conf_ok,
    })
}

/// 每模型 outcome 序列（回放窗内被选时的结果，按时间序）
fn model_outcomes(rows: &[EvRow]) -> HashMap<String, Vec<(bool, u64)>> {
    let mut map: HashMap<String, Vec<(bool, u64)>> = HashMap::new();
    for r in rows {
        map.entry(r.chosen.clone()).or_default().push((r.ok, r.total_ms));
    }
    map
}

/// 全局质量 proxy（ground truth 代理）：0.7·成功率 + 0.3·速度归一（与 speed_obs 同式）
fn quality_proxy(outcomes: &HashMap<String, Vec<(bool, u64)>>) -> HashMap<String, f32> {
    let mut map = HashMap::new();
    for (id, seq) in outcomes {
        if seq.len() < MIN_MODEL_SAMPLES {
            continue;
        }
        let rate = seq.iter().filter(|(ok, _)| *ok).count() as f32 / seq.len() as f32;
        let mut lat: Vec<f32> = seq.iter().map(|(_, ms)| (*ms).max(1) as f32).collect();
        lat.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
        let med_ms = lat[lat.len() / 2];
        let speed = (1.0 - med_ms.ln() / SPEED_LN_MAX).clamp(0.0, 1.0);
        map.insert(id.clone(), 0.7 * rate + 0.3 * speed);
    }
    map
}

/// 策略 bias：与运行时 telemetry_snapshot 同构——近 30 条 reward + 跨模型 median 中心化
fn strategy_bias(
    outcomes: &HashMap<String, Vec<(bool, u64)>>,
    params: &StrategyParams,
) -> HashMap<String, f32> {
    let mut rewards: Vec<(String, f32)> = Vec::new();
    for (id, seq) in outcomes {
        // 与运行时同构（review 2026-10-23）：telemetry_snapshot 喂给 outcome_reward
        // 的是全量 recent 样本环——回放不再截断到 30 条
        let recent: Vec<evolve_core::types::ReqSample> = seq
            .iter()
            .map(|(ok, ms)| evolve_core::types::ReqSample {
                ts: 0,
                ttft_ms: 0,
                total_ms: *ms,
                in_tok: 0,
                cached_tok: 0,
                out_tok: 0,
                ok: *ok,
                tools_total: 0,
                est_tok: None,
                tools_ok: 0,
            })
            .collect();
        if let Some(r) = outcome_reward(&recent, params) {
            rewards.push((id.clone(), r));
        }
    }
    let mut bias = HashMap::new();
    if rewards.len() < 3 {
        // 统计不足：中性 bias（回放退化为纯公式序，诚实反映数据不够）
        for (id, _) in rewards {
            bias.insert(id, 1.0);
        }
        return bias;
    }
    let mut rs: Vec<f32> = rewards.iter().map(|(_, r)| *r).collect();
    rs.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
    let median = rs[rs.len() / 2];
    for (id, r) in rewards {
        bias.insert(id, (1.0 + (r - median) * params.bias_gain).clamp(0.7, 1.3));
    }
    bias
}

fn score_strategy(
    rows: &[EvRow],
    bias: &HashMap<String, f32>,
    proxy: &HashMap<String, f32>,
    params: &StrategyParams,
) -> StrategyScore {
    let mut total_q = 0.0f32;
    let mut total_opt = 0.0f32;
    let mut total_pess = 0.0f32;
    let mut scored = 0u32;
    let mut intervened = 0u32;
    for r in rows {
        // 计分事件集：board 中至少一个模型有全局质量 proxy
        let mut best: Option<(&String, f32)> = None;
        let mut opt: Option<f32> = None;
        let mut pess: Option<f32> = None;
        for (id, score) in &r.board {
            let b = bias.get(id).copied().unwrap_or(1.0);
            let noisy = score * b;
            if best.map_or(true, |(_, bs)| noisy > bs) {
                best = Some((id, noisy));
            }
            if let Some(q) = proxy.get(id) {
                opt = Some(match opt {
                    Some(v) => v.max(*q),
                    None => *q,
                });
                pess = Some(match pess {
                    Some(v) => v.min(*q),
                    None => *q,
                });
            }
        }
        let Some((top, _)) = best else { continue };
        let Some((o, p)) = opt.zip(pess) else { continue };
        // 干预计数与评分同分母（2026-10-23 review）：此前 top1 无 proxy 的行
        // 只进 intervened 不进 scored，比率被系统性抬高
        if *top != r.chosen {
            intervened += 1;
        }
        // top1 无 proxy（样本不足）时取中性 0.5，与 oracle 同集计分
        let sq = proxy.get(top).copied().unwrap_or(0.5);
        total_q += sq;
        total_opt += o;
        total_pess += p;
        scored += 1;
    }
    let avg_quality = if scored > 0 { total_q / scored as f32 } else { 0.0 };
    let avg_opt = if scored > 0 { total_opt / scored as f32 } else { 0.0 };
    let avg_pess = if scored > 0 { total_pess / scored as f32 } else { 0.0 };
    let pgr = if avg_opt - avg_pess > 1e-4 {
        ((avg_quality - avg_pess) / (avg_opt - avg_pess)).clamp(0.0, 1.0)
    } else {
        // oracle 差距过小（候选同质）：PGR 无意义，取中性
        0.5
    };
    StrategyScore {
        version: params.version,
        ok_weight: params.ok_weight,
        bias_gain: params.bias_gain,
        avg_quality,
        pgr,
        intervention_rate: if scored > 0 {
            intervened as f32 / scored as f32
        } else {
            0.0
        },
        events_scored: scored,
    }
}

/// 在 events.jsonl 上 A/B 回放两套策略参数，产出对比报告与采纳建议。
/// judge 配置存在时（A3b），对带 resp_digest 的采样事件用 LLM-as-a-Judge
/// 做 point-wise 质量评分（"是否充分回答"），修正规则 ok 的误判——
/// 偏见消除：judge 与被评模型天然异构、point-wise 无位置偏差、
/// 评分维度单一明确（充分性）。
pub fn replay(
    events_path: &Path,
    a: &StrategyParams,
    b: &StrategyParams,
    limit: usize,
) -> anyhow::Result<ReplayReport> {
    replay_inner(events_path, a, b, limit, None, 0)
}

/// 带 judges 的回放（`backtest --judge-samples N` 时启用）。
/// asker: 闭包（query 摘要, 响应摘要）→ 充分性概率 [0,1]；None 表示本次未评分。
pub fn replay_with_judge(
    events_path: &Path,
    a: &StrategyParams,
    b: &StrategyParams,
    limit: usize,
    asker: &dyn Fn(&str, &str) -> Option<f32>,
    judge_samples: usize,
) -> anyhow::Result<ReplayReport> {
    replay_inner(events_path, a, b, limit, Some(asker), judge_samples)
}

fn replay_inner(
    events_path: &Path,
    a: &StrategyParams,
    b: &StrategyParams,
    limit: usize,
    asker: Option<&dyn Fn(&str, &str) -> Option<f32>>,
    judge_samples: usize,
) -> anyhow::Result<ReplayReport> {
    let mut rows = parse_events(events_path, limit)?;
    anyhow::ensure!(
        rows.len() >= 50,
        "事件流样本不足（{} 条，需 ≥50）：先积累生产流量再回放",
        rows.len()
    );
    let mut judge_report = JudgeOutcome::default();
    if let Some(ask) = asker {
        judge_report = apply_judge_scores(&mut rows, ask, judge_samples);
    }
    let outcomes = model_outcomes(&rows);
    let proxy = quality_proxy(&outcomes);
    let sa = score_strategy(&rows, &strategy_bias(&outcomes, a), &proxy, a);
    let sb = score_strategy(&rows, &strategy_bias(&outcomes, b), &proxy, b);
    // 均势保守：B 未显著优于 A（≥0.5% 相对差）则建议保持当前——
    // 策略变更的收益必须超过换策略本身的风险。裁决用 PGR（归一恢复率）
    let margin = 0.005f32 * sa.pgr.max(0.01);
    let verdict = if sb.pgr > sa.pgr + margin {
        format!("建议采纳 B（PGR +{:.2}pp：{:.3} → {:.3}）", (sb.pgr - sa.pgr) * 100.0, sa.pgr, sb.pgr)
    } else if sb.pgr >= sa.pgr - margin {
        "B 与 A 均势：保持当前参数（变更收益不显著）".into()
    } else {
        format!("保持 A：B 更差（PGR −{:.2}pp），拒绝采纳", (sa.pgr - sb.pgr) * 100.0)
    };
    Ok(ReplayReport {
        events_replayed: rows.len() as u32,
        models_with_outcomes: outcomes.len(),
        a: sa,
        b: sb,
        verdict,
        judge: judge_report,
    })
}

/// confidence_gate 灵敏度扫描的单档结果
#[derive(Debug, Serialize)]
pub struct GateSweepRow {
    pub gate: f32,
    /// 判定快照可判定的事件数
    pub events_judged: usize,
    /// 缺判定快照（旧事件）被排除的事件数
    pub events_missing_judgment: usize,
    /// 该 gate 值下允许探索的事件数
    pub explore_allowed: usize,
    pub explore_allowed_pct: f32,
    /// 相对上一档（更低 gate）新解锁的探索事件数；首档为 None
    pub delta_allowed: Option<i64>,
}

/// ②a confidence_gate 灵敏度扫描（TokenRouter 借鉴项：阈值从数据推导）。
///
/// 对每个候选 gate 值，在回放窗内按 engine 同式重算探索门控的**确定性部分**：
/// `候选数 > 1 ∧ high_stakes < 0.6 ∧ difficulty_eff < 2.0 ∧ domain_confidence ≥ gate`
///（对应 engine.rs 的 should_explore 三闸门）。
///
/// 诚实边界（off-policy 混淆）：实际 chosen 还受 Thompson 采样（随机）与
/// 低置信粘性偏好影响，这两部分无法从事件流反事实重算——因此本扫描是
/// **灵敏度预览**（改 gate 会动多少决策），不是性能预测（改完 PGR 变多少）。
/// 性能结论仍须等新参数上线积累事件后由 `backtest` A/B 裁决。
pub fn gate_sweep(
    events_path: &Path,
    gates: &[f32],
    limit: usize,
) -> anyhow::Result<Vec<GateSweepRow>> {
    let rows = parse_events(events_path, limit)?;
    anyhow::ensure!(
        rows.len() >= 50,
        "事件流样本不足（{} 条，需 ≥50）：先积累生产流量再扫描",
        rows.len()
    );
    // 探索门控的确定性前提（与 gate 无关的部分先算好）
    let eligible: Vec<Option<f32>> = rows
        .iter()
        .map(|r| {
            r.j.and_then(|j| {
                (j.candidates > 1
                    && j.high_stakes < evolve_core::thresholds::STAKES_HIGH
                    && j.difficulty_eff < evolve_core::thresholds::DIFF_HARD)
                .then_some(j.domain_confidence)
            })
        })
        .collect();
    let judged = rows.iter().filter(|r| r.j.is_some()).count();
    let missing = rows.len() - judged;
    anyhow::ensure!(
        judged >= 50,
        "带判定快照的事件不足（{judged}/{total} 条，需 ≥50）：\
         judgment 字段自 2026-10 版本起才写入，等新事件积累",
        total = rows.len(),
    );
    let mut out: Vec<GateSweepRow> = Vec::new();
    for (i, &g) in gates.iter().enumerate() {
        let allowed = eligible
            .iter()
            .filter(|e| e.map(|dc| dc >= g).unwrap_or(false))
            .count();
        out.push(GateSweepRow {
            gate: g,
            events_judged: judged,
            events_missing_judgment: missing,
            explore_allowed: allowed,
            explore_allowed_pct: if judged > 0 {
                allowed as f32 / judged as f32
            } else {
                0.0
            },
            delta_allowed: if i == 0 {
                None
            } else {
                Some(allowed as i64 - out[i - 1].explore_allowed as i64)
            },
        });
    }
    Ok(out)
}

/// gate 扫描的人类可读输出
pub fn print_gate_sweep(rows: &[GateSweepRow]) {
    println!(
        "\nconfidence_gate 灵敏度扫描（探索门控确定性部分；Thompson 采样与\
低置信粘性属 off-policy 部分，不在本表——结论是灵敏度预览，性能须以 backtest A/B 为准）\n"
    );
    println!(
        "{:<8} {:<10} {:<12} {:<14} 排除(无快照)",
        "gate", "允许探索", "占比", "Δvs上一档"
    );
    for r in rows {
        let delta = r
            .delta_allowed
            .map(|d| format!("{d:+}"))
            .unwrap_or_else(|| "-".into());
        println!(
            "{:<8} {:<10} {:<12} {:<14} {}",
            format!("{:.2}", r.gate),
            r.explore_allowed,
            format!("{:.1}%", r.explore_allowed_pct * 100.0),
            delta,
            if r.events_missing_judgment > 0 {
                r.events_missing_judgment.to_string()
            } else {
                "0".into()
            },
        );
    }
    println!(
        "\n选值提示：gate 越低探索越多（发现更好模型）但简单任务被扰动越多；\
候选值应落在'Δ 陡变点'附近做小步 A/B（backtest），不要一次性大改。"
    );
}

/// gate_replay（②b 反事实 A/B 模拟）的单档结果
#[derive(Debug, Serialize)]
pub struct GateReplayRow {
    pub gate: f32,
    /// 进入质量计分的事件数
    pub scored: u32,
    /// 区制排除：实际门控未知或封锁、而候选 gate 会放开探索——
    /// 反事实选择是随机的（Thompson 采样），不可精确重算，宁缺毋假
    pub excluded_stochastic: u32,
    /// 缺判定快照（旧事件）被排除的事件数
    pub missing_judgment: u32,
    /// 反事实 chosen 的质量 proxy 均值（0-1）
    pub avg_quality: f32,
    /// PGR（Performance Gap Recovered，与 A/B 回放同口径）：反事实选择
    /// 恢复了"oracle 最优与最差"质量差距的比例 [0,1]
    pub pgr: f32,
    /// 相对最宽松档（gates 升序首档）的质量差（pp）
    pub delta_vs_base_pp: f32,
    /// 反事实封锁且计分的事件中，公式 top1 与实际 chosen 不同的比例——
    /// "收紧 gate 会改写多少决策"的质量侧对照（对应 gate_sweep 的计数视角）。
    /// 分母只含封锁档计分事件（双向允许档无法观测改写，不进分母）
    pub rewrite_rate: f32,
    /// board 内无任何模型有质量 proxy（样本不足）而无法与 oracle 同集计分、
    /// 被静默跳过的事件数——保证各列可核对总数
    pub skipped_no_proxy: u32,
}

/// ②b confidence_gate 反事实 A/B 模拟（TokenRouter 借鉴：阈值从数据推导）。
///
/// Thompson 采样只改写局部 `best_idx`、不改公式序（engine.rs 采样段），
/// 因此事件里的 scoreboard 就是**未扰动公式序**——反事实 chosen 按区制精确还原：
///
/// - **反事实封锁**（候选 gate 封掉探索）：chosen' = 公式 top1，确定性可算；
/// - **双向都允许**：实际事件与反事实同属"允许探索"分布，实际 chosen 是
///   无偏样本，直接计分；
/// - **封锁→放开**：反事实选择随机，排除计数（不可精确重算）；
/// - 缺快照：排除计数。
///
/// 诚实边界（写入输出）：低置信粘性路径（engine 的 low_conf 偏好）依赖
/// prev sticky 模型，事件流未记录——对 dc < gate 的事件有少量未建模偏差；
/// 各档计分集不同（排除数不同），跨档比较看 Δ 与排除占比。
pub fn gate_replay(
    events_path: &Path,
    gates: &[f32],
    limit: usize,
) -> anyhow::Result<Vec<GateReplayRow>> {
    let rows = parse_events(events_path, limit)?;
    anyhow::ensure!(
        rows.len() >= 50,
        "事件流样本不足（{} 条，需 ≥50）：先积累生产流量再模拟",
        rows.len()
    );
    let outcomes = model_outcomes(&rows);
    let proxy = quality_proxy(&outcomes);
    anyhow::ensure!(
        !proxy.is_empty(),
        "回放窗内没有任何模型达到质量 proxy 样本量（每模型 ≥{} 次）：先积累流量",
        MIN_MODEL_SAMPLES
    );
    anyhow::ensure!(
        gates.len() >= 2,
        "--gate-replay 至少两个候选值（需要基档做 Δ 比较）"
    );

    let mut out: Vec<GateReplayRow> = Vec::new();
    for &g in gates {
        let mut scored_n = 0u32;
        let mut excluded = 0u32;
        let mut missing = 0u32;
        let mut rewritten = 0u32;
        let mut blocked_scored = 0u32;
        let mut skipped_no_proxy = 0u32;
        let mut total_q = 0f32;
        let mut tot_opt = 0f32;
        let mut tot_pess = 0f32;
        for r in &rows {
            let Some(j) = r.j else {
                missing += 1;
                continue;
            };
            let stakes_ok = j.high_stakes < evolve_core::thresholds::STAKES_HIGH;
            let diff_ok = j.difficulty_eff < evolve_core::thresholds::DIFF_HARD;
            let multi = j.candidates > 1;
            let cf_allowed = multi && stakes_ok && diff_ok && j.domain_confidence >= g;
            // 计分与 oracle 同集口径（与 score_strategy 一致）
            let mut opt: Option<f32> = None;
            let mut pess: Option<f32> = None;
            for (id, _) in &r.board {
                if let Some(q) = proxy.get(id) {
                    opt = Some(match opt {
                        Some(v) => v.max(*q),
                        None => *q,
                    });
                    pess = Some(match pess {
                        Some(v) => v.min(*q),
                        None => *q,
                    });
                }
            }
            let (Some(o), Some(p)) = (opt, pess) else {
                // board 内无任何模型有 proxy：无法与 oracle 同集计分——
                // 显式计数（列总和可核对），不再静默消失
                skipped_no_proxy += 1;
                continue;
            };
            // 反事实 chosen 的质量
            let cf_top = r
                .board
                .iter()
                .max_by(|a, b| a.1.partial_cmp(&b.1).unwrap_or(std::cmp::Ordering::Equal))
                .map(|(id, _)| id.clone());
            let q = if !cf_allowed {
                // 确定性：公式 top1（top1 无 proxy 时取中性 0.5，与 A/B 回放同口径）
                blocked_scored += 1;
                if cf_top.as_ref().map(|t| t != &r.chosen).unwrap_or(false) {
                    rewritten += 1;
                }
                cf_top.as_ref().and_then(|t| proxy.get(t)).copied().unwrap_or(0.5)
            } else {
                match j.actual_conf_ok {
                    // 双向都允许：实际事件是同分布无偏样本
                    Some(true) => proxy.get(&r.chosen).copied().unwrap_or(0.5),
                    // 实际封锁或未知 → 反事实随机，排除
                    _ => {
                        excluded += 1;
                        continue;
                    }
                }
            };
            total_q += q;
            tot_opt += o;
            tot_pess += p;
            scored_n += 1;
        }
        anyhow::ensure!(
            scored_n >= 50,
            "gate {g} 的可计分事件不足（{scored_n} 条，需 ≥50）：扩大 --limit 或积累流量"
        );
        let avg_quality = total_q / scored_n as f32;
        let avg_opt = tot_opt / scored_n as f32;
        let avg_pess = tot_pess / scored_n as f32;
        let pgr = if avg_opt - avg_pess > 1e-4 {
            ((avg_quality - avg_pess) / (avg_opt - avg_pess)).clamp(0.0, 1.0)
        } else {
            0.5
        };
        out.push(GateReplayRow {
            gate: g,
            scored: scored_n,
            excluded_stochastic: excluded,
            missing_judgment: missing,
            skipped_no_proxy,
            avg_quality,
            pgr,
            delta_vs_base_pp: 0.0,
            rewrite_rate: if blocked_scored > 0 {
                rewritten as f32 / blocked_scored as f32
            } else {
                0.0
            },
        });
    }
    // Δ 相对最宽松档（升序首档；调用方传入顺序随意，这里按 gate 排序）
    out.sort_by(|a, b| a.gate.partial_cmp(&b.gate).unwrap_or(std::cmp::Ordering::Equal));
    let base_q = out.first().map(|r| r.avg_quality).unwrap_or(0.0);
    for r in &mut out {
        r.delta_vs_base_pp = (r.avg_quality - base_q) * 100.0;
    }
    Ok(out)
}

/// gate 反事实模拟的人类可读输出（含跨档裁决建议）
pub fn print_gate_replay(rows: &[GateReplayRow]) {
    println!(
        "\nconfidence_gate 反事实 A/B 模拟（区制感知：封锁档=公式 top1 精确还原；\
双向允许档=实际结果无偏样本；封锁→放开档=随机，已排除）\n"
    );
    println!(
        "{:<8} {:<8} {:<10} {:<10} {:<10} {:<10} {:<12} {:<12} {}",
        "gate", "计分", "排除(随)*", "缺快照", "无proxy", "平均质量", "Δvs最宽(pp)", "PGR", "改写率"
    );
    for r in rows {
        println!(
            "{:<8} {:<8} {:<10} {:<10} {:<10} {:<10.4} {:<12.2} {:<12.3} {:<12}",
            format!("{:.2}", r.gate),
            r.scored,
            r.excluded_stochastic,
            r.missing_judgment,
            r.skipped_no_proxy,
            r.avg_quality,
            r.delta_vs_base_pp,
            r.pgr,
            format!("{:.1}%", r.rewrite_rate * 100.0),
        );
    }
    if let (Some(best), Some(base)) = (rows.iter().max_by(|a, b| a.avg_quality.partial_cmp(&b.avg_quality).unwrap_or(std::cmp::Ordering::Equal)), rows.first()) {
        let gain = best.delta_vs_base_pp;
        let excl_rate = if base.scored > 0 {
            best.excluded_stochastic as f32 / (best.scored + best.excluded_stochastic) as f32
        } else {
            1.0
        };
        println!("\n裁决：");
        if best.gate == base.gate {
            println!("  最宽松档（{:.2}）已是质量最优：无收紧依据，保持当前 gate。", base.gate);
        } else if gain >= 0.5 && excl_rate < 0.2 {
            println!(
                "  gate {:.2} 质量 +{:.2}pp 且随机排除占比 {:.0}%（<20%，结论可信）——\
建议写入配置试运行，积累新事件后再跑 backtest A/B 复核。",
                best.gate, gain, excl_rate * 100.0
            );
        } else if gain.abs() < 0.5 {
            println!(
                "  各档质量均势（最优 Δ {:+.2}pp < 0.5pp）：不值得变更，保持当前 gate。",
                gain
            );
        } else {
            println!(
                "  gate {:.2} 看似 +{:.2}pp 但随机排除占比 {:.0}%（≥20%）——\
差异可能来自计分集偏移而非真实增益，先扩大样本再判断。",
                best.gate, gain, excl_rate * 100.0
            );
        }
        println!(
            "\n未建模偏差声明：低置信粘性路径（事件流无 prev sticky 模型）未纳入；\
各档计分集随排除数不同。本模拟是 off-policy 估计，上线前必须以新流量 A/B 复核。"
        );
    }
}

/// A3b judge 评分结果统计
#[derive(Debug, Default, Serialize)]
pub struct JudgeOutcome {
    /// 实际评分的事件数
    pub judged: u32,
    /// judge 判"不充分"（<0.5）但规则判 ok 的事件数——规则漏判被纠正
    pub rule_missed: u32,
    /// judge 判"充分"但规则判 fail 的事件数——规则误杀被纠正
    pub rule_overstrict: u32,
    /// judge 与规则 ok 判定的一致率（[0,1]；不足为可信度参考）
    pub agreement: f32,
}

/// 对带 resp_digest 的事件均匀采样并调 judge 评分，事件 ok 就地修正。
/// 评分维度单一：响应是否充分回答了查询（充分性，无风格项——防风格偏见）。
fn apply_judge_scores(
    rows: &mut [EvRow],
    ask: &dyn Fn(&str, &str) -> Option<f32>,
    samples: usize,
) -> JudgeOutcome {
    // 采样：带 resp_digest 的事件均匀跨窗取 N 条（步长抽样，避免全量调用成本
    // 也不偏头部——此前 take(N) 只取窗口最早的事件，2026-10-23 review）
    let all: Vec<usize> = rows
        .iter()
        .enumerate()
        .filter(|(_, r)| r.resp_digest.is_some())
        .map(|(i, _)| i)
        .collect();
    let step = all.len().div_ceil(samples.max(1)).max(1);
    let digest_idx: Vec<usize> = all.into_iter().step_by(step).collect();
    let mut out = JudgeOutcome::default();
    let mut agree = 0u32;
    for i in digest_idx {
        let r = &mut rows[i];
        let query_head = format!("event#{}", i); // 事件流无 query 正文（redact），以摘要上下文代之
        let Some(score) = ask(&query_head, r.resp_digest.as_deref().unwrap_or("")) else {
            continue;
        };
        let judge_ok = score >= 0.5;
        if judge_ok == r.ok {
            agree += 1;
        } else if !judge_ok && r.ok {
            out.rule_missed += 1;
        } else {
            out.rule_overstrict += 1;
        }
        r.ok = judge_ok; // 事件 ok 就地修正——下游 proxy/bias 全部继承
        out.judged += 1;
    }
    out.agreement = if out.judged > 0 { agree as f32 / out.judged as f32 } else { 1.0 };
    out
}

/// 报告的人类可读输出
pub fn print_report(r: &ReplayReport) {
    println!(
        "策略回放验证：{} 条事件，{} 个模型有 outcome 样本\n",
        r.events_replayed, r.models_with_outcomes
    );
    println!(
        "{:<6} {:<10} {:<10} {:<12} {:<10} {:<12} {:<8}",
        "策略", "ok_weight", "bias_gain", "平均质量", "PGR", "干预率", "计分事件"
    );
    for (name, s) in [("A(当前)", &r.a), ("B(候选)", &r.b)] {
        println!(
            "{:<6} {:<10.2} {:<10.2} {:<12.4} {:<10.3} {:<11.2}% {:<8}",
            name, s.ok_weight, s.bias_gain, s.avg_quality, s.pgr, s.intervention_rate * 100.0, s.events_scored
        );
    }
    println!("\n裁决：{}", r.verdict);
    if r.judge.judged > 0 {
        println!(
            "\nLLM-as-a-Judge：评分 {} 条，与规则判定一致率 {:.1}%（规则漏判 {}、误杀 {}）",
            r.judge.judged,
            r.judge.agreement * 100.0,
            r.judge.rule_missed,
            r.judge.rule_overstrict
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn score_strategy_prefers_bias_toward_quality_model() {
        // 模型 g 全成功且快（proxy 高）、模型 b 常失败（proxy 低）
        let mut outcomes = HashMap::new();
        outcomes.insert("g".to_string(), vec![(true, 500u64); 10]);
        outcomes.insert("b".to_string(), vec![(false, 9000u64); 10]);
        let proxy = quality_proxy(&outcomes);
        let rows = vec![
            EvRow { board: vec![("b".into(), 0.6), ("g".into(), 0.59)], chosen: "b".into(), ok: false, total_ms: 9000, resp_digest: None, j: None },
            EvRow { board: vec![("b".into(), 0.6), ("g".into(), 0.59)], chosen: "b".into(), ok: false, total_ms: 9000, resp_digest: None, j: None },
        ];
        // 策略给 g 强 bias：重排后 top1 = g，平均质量应显著高于纯公式序（top1=b）
        let mut params = StrategyParams::default();
        params.ok_weight = 1.0;
        params.bias_gain = 1.0;
        let mut bias = HashMap::new();
        bias.insert("g".to_string(), 1.3f32);
        bias.insert("b".to_string(), 0.7f32);
        let s = score_strategy(&rows, &bias, &proxy, &params);
        assert!(s.intervention_rate > 0.9, "bias 应翻转 top1: {s:?}");
        let neutral = score_strategy(&rows, &HashMap::new(), &proxy, &params);
        assert!(s.avg_quality > neutral.avg_quality, "偏向高质量模型的策略应得分更高");
    }

    #[test]
    fn quality_proxy_requires_min_samples() {
        let mut outcomes = HashMap::new();
        outcomes.insert("thin".to_string(), vec![(true, 100u64); 2]);
        assert!(quality_proxy(&outcomes).is_empty());
    }

    /// ②a：构造带 judgment 快照的事件流，验证 gate 扫描的确定性计数
    #[test]
    fn gate_sweep_counts_deterministic_gate_condition() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("events.jsonl");
        let mut body = String::new();
        // 55 条：domain_confidence = i/60（0.016..0.916 递增），全部过
        // stakes/difficulty/候选数前提；其中 5 条是旧事件（无 judgment）
        for i in 0..55u32 {
            let line = if i < 5 {
                json!({"kind": "chat", "scoreboard": [{"model_id": "a", "score": 0.9}],
                       "chosen": "a", "status": 200, "total_ms": 500})
            } else {
                json!({"kind": "chat", "scoreboard": [{"model_id": "a", "score": 0.9},
                                                      {"model_id": "b", "score": 0.8}],
                       "chosen": "a", "status": 200, "total_ms": 500,
                       "difficulty_eff": 1.0,
                       "judgment": {"domain_confidence": i as f64 / 60.0,
                                    "difficulty_confidence": 0.9,
                                    "difficulty": 1.0, "high_stakes": 0.1},
                       "explore_gate": {"stakes_ok": true, "difficulty_ok": true,
                                        "confidence_ok": true, "explored": false}})
            };
            body.push_str(&line.to_string());
            body.push('\n');
        }
        std::fs::write(&path, body).unwrap();
        let rows = gate_sweep(&path, &[0.2, 0.5], 1000).unwrap();
        assert_eq!(rows.len(), 2);
        // 50 条带快照：dc = i/60, i∈5..55 → dc≥0.2 有 43 条（i≥12），dc≥0.5 有 25 条（i≥30）
        assert_eq!(rows[0].events_judged, 50);
        assert_eq!(rows[0].events_missing_judgment, 5);
        assert_eq!(rows[0].explore_allowed, 43);
        assert_eq!(rows[1].explore_allowed, 25);
        assert_eq!(rows[1].delta_allowed, Some(-18));
        assert!((rows[0].explore_allowed_pct - 0.86).abs() < 1e-4);
    }

    /// 不满足确定性前提（单候选 / 高风险 / 高难度）的事件不随 gate 变化
    #[test]
    fn gate_sweep_excludes_non_eligible_events() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("events.jsonl");
        let mut body = String::new();
        for i in 0..60u32 {
            // 前 30 条单候选（candidates=1）、后 30 条高风险：都不合格
            let (board, hs) = if i < 30 {
                (json!([{"model_id": "a", "score": 0.9}]), 0.1)
            } else {
                (json!([{"model_id": "a", "score": 0.9}, {"model_id": "b", "score": 0.8}]), 0.9)
            };
            let line = json!({"kind": "chat", "scoreboard": board, "chosen": "a",
                              "status": 200, "total_ms": 500, "difficulty_eff": 1.0,
                              "judgment": {"domain_confidence": 0.99, "difficulty_confidence": 0.9,
                                           "difficulty": 1.0, "high_stakes": hs},
                              "explore_gate": {"explored": false}});
            body.push_str(&line.to_string());
            body.push('\n');
        }
        std::fs::write(&path, body).unwrap();
        let rows = gate_sweep(&path, &[0.2, 0.5], 1000).unwrap();
        assert_eq!(rows[0].events_judged, 60);
        assert_eq!(rows[0].explore_allowed, 0, "全部事件不满足候选数/风险前提");
        assert_eq!(rows[0].delta_allowed, None);
        assert_eq!(rows[1].delta_allowed, Some(0));
    }

    #[test]
    fn gate_sweep_requires_enough_judged_events() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("events.jsonl");
        let mut body = String::new();
        for _ in 0..60 {
            let line = json!({"kind": "chat", "scoreboard": [{"model_id": "a", "score": 0.9}],
                              "chosen": "a", "status": 200, "total_ms": 500});
            body.push_str(&line.to_string());
            body.push('\n');
        }
        std::fs::write(&path, body).unwrap();
        assert!(gate_sweep(&path, &[0.3], 1000).is_err(), "60 条全无快照应拒绝扫描");
    }

    /// ②b：四区制反事实模拟——封锁档还原公式 top1、双向允许档用实际结果、
    /// 封锁→放开档排除、缺快照排除
    #[test]
    fn gate_replay_regime_semantics() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("events.jsonl");
        let mut body = String::new();
        // 80 条事件，board 恒为 a(0.9)/b(0.8)；a 恒成功快（proxy≈1.0）、
        // b 恒失败慢（proxy≈0.0）→ oracle 差距满格。每 20 条一组：
        // A 反事实封锁（dc=0.1，任何候选 gate 都封锁）：计 top1=a
        // B 双向允许（dc=0.9, G=0.2，actual conf_ok=true）：计实际 chosen=a
        // C 实际允许→反事实封锁（dc=0.4, G=0.5, conf_ok=true, 实际 chosen=b
        //   =探索改写）：宽松档计实际 b；收紧档还原 top1=a 且计改写
        // D 实际封锁→宽松档放开（dc=0.4, conf_ok=false）：宽松档排除（随机）；
        //   收紧档反事实也封锁 → 确定性计 top1=a
        for i in 0..80u32 {
            let group = i / 20;
            let (dc, conf_ok, chosen) = match group {
                0 => (0.1, false, "a"),   // A：实际封锁（dc<0.35）
                1 => (0.9, true, "a"),    // B：双向允许
                2 => (0.4, true, "b"),    // C：探索改写首选
                _ => (0.4, false, "a"),   // D：实际封锁（confidence_ok=false 记录）
            };
            let line = json!({"kind": "chat",
                              "scoreboard": [{"model_id": "a", "score": 0.9},
                                             {"model_id": "b", "score": 0.8}],
                              "chosen": chosen,
                              "status": if chosen == "a" { 200 } else { 500 },
                              "total_ms": if chosen == "a" { 100 } else { 9000 },
                              "difficulty_eff": 1.0,
                              "judgment": {"domain_confidence": dc, "difficulty_confidence": 0.9,
                                           "difficulty": 1.0, "high_stakes": 0.1},
                              "explore_gate": {"stakes_ok": true, "difficulty_ok": true,
                                               "confidence_ok": conf_ok, "explored": chosen == "b"}});
            body.push_str(&line.to_string());
            body.push('\n');
        }
        std::fs::write(&path, body).unwrap();
        let rows = gate_replay(&path, &[0.2, 0.5], 1000).unwrap();
        assert_eq!(rows.len(), 2);
        // G=0.2（宽松）：A 封锁（dc=0.1）→top1=a；B 双向允许→实际 a；C 双向允许
        // →实际 b；D 实际封锁→反事实放开→随机排除
        // proxy：a = 0.7·1.0 + 0.3·(1−ln100/ln100000) = 0.88；b = 0.3·(1−ln9000/ln100000) ≈ 0.06274
        let loose = &rows[0];
        assert_eq!(loose.gate, 0.2);
        assert_eq!(loose.scored, 60, "A20+B20+C20 计分，D20 排除");
        assert_eq!(loose.excluded_stochastic, 20);
        assert!(loose.missing_judgment == 0);
        let q_a = 0.7 * 1.0 + 0.3 * (1.0 - (100f64).ln() / 100_000f64.ln()) as f32;
        let q_b = 0.3 * (1.0 - (9000f64).ln() / 100_000f64.ln()) as f32;
        let expect_loose = (40.0 * q_a + 20.0 * q_b) / 60.0;
        assert!((loose.avg_quality - expect_loose).abs() < 1e-4,
                "A/B 40 条 a({q_a}) + C 20 条 b({q_b})，实际 {:.4}", loose.avg_quality);
        assert!(loose.rewrite_rate.abs() < 1e-6, "宽松档无 top1 还原路径命中改写（A 组 top1=实际）");
        // G=0.5（收紧）：A/C/D 全部反事实封锁 → 确定性 top1=a（C 组计改写 20）；
        // B 双向允许 → 实际 a。全部 80 条计分，质量恒为 q_a → PGR=1
        let tight = &rows[1];
        assert_eq!(tight.gate, 0.5);
        assert_eq!(tight.scored, 80);
        assert_eq!(tight.excluded_stochastic, 0);
        assert!((tight.avg_quality - q_a).abs() < 1e-4,
                "全部反事实 chosen=a → {q_a}，实际 {:.4}", tight.avg_quality);
        // 改写率分母 = 封锁档计分事件（A20+C20+D20=60），改写仅 C 组 20 条
        assert!((tight.rewrite_rate - 1.0 / 3.0).abs() < 1e-4, "实际 {:.3}", tight.rewrite_rate);
        assert_eq!(tight.skipped_no_proxy, 0);
        // Δ vs 最宽档（0.2 是升序首档）
        assert!((tight.delta_vs_base_pp - (q_a - expect_loose) * 100.0).abs() < 1e-2);
        assert!(loose.delta_vs_base_pp.abs() < 1e-6);
        // PGR：紧档质量=oracle 最优（q_a）→ PGR=1
        assert!((tight.pgr - 1.0).abs() < 1e-4);
    }

    /// 部分事件的 board 全是样本不足模型：显式计入 skipped_no_proxy（列可核对）
    #[test]
    fn gate_replay_counts_skipped_no_proxy_events() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("events.jsonl");
        let mut body = String::new();
        // 50 条正常事件（board a/b，chosen=a，dc=0.9，双向允许）+ 10 条
        // board 只有 x（x 从未成为 chosen，无 proxy）
        for i in 0..60u32 {
            let line = if i < 50 {
                json!({"kind": "chat",
                       "scoreboard": [{"model_id": "a", "score": 0.9}, {"model_id": "b", "score": 0.8}],
                       "chosen": "a", "status": 200, "total_ms": 100,
                       "difficulty_eff": 1.0,
                       "judgment": {"domain_confidence": 0.9, "difficulty_confidence": 0.9,
                                    "difficulty": 1.0, "high_stakes": 0.1},
                       "explore_gate": {"confidence_ok": true, "explored": false}})
            } else {
                // board 只有 x/y（从未被选，无 proxy）→ 无法与 oracle 同集计分
                json!({"kind": "chat",
                       "scoreboard": [{"model_id": "x", "score": 0.9}, {"model_id": "y", "score": 0.8}],
                       "chosen": "a", "status": 200, "total_ms": 100,
                       "difficulty_eff": 1.0,
                       "judgment": {"domain_confidence": 0.9, "difficulty_confidence": 0.9,
                                    "difficulty": 1.0, "high_stakes": 0.1},
                       "explore_gate": {"confidence_ok": true, "explored": false}})
            };
            body.push_str(&line.to_string());
            body.push('\n');
        }
        std::fs::write(&path, body).unwrap();
        let rows = gate_replay(&path, &[0.2, 0.5], 1000).unwrap();
        for r in &rows {
            assert_eq!(r.skipped_no_proxy, 10, "x-board 事件应显式计数");
            assert_eq!(r.scored, 50);
            assert_eq!(r.excluded_stochastic, 0);
            assert_eq!(r.missing_judgment, 0);
        }
        // proxy 只有 a（b 从未被选）→ oracle 差距为 0 → PGR 取中性 0.5
        assert!((rows[0].pgr - 0.5).abs() < 1e-4);
    }

    /// board 内无任何模型达到 proxy 样本量时应拒绝模拟
    #[test]
    fn gate_replay_requires_quality_proxy() {        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("events.jsonl");
        let mut body = String::new();
        for i in 0..60u32 {
            // 每个模型只被选 1 次（60 个不同模型名）→ proxy 永远空
            let m = format!("m{i}");
            let line = json!({"kind": "chat",
                              "scoreboard": [{"model_id": m, "score": 0.9}],
                              "chosen": m, "status": 200, "total_ms": 500,
                              "difficulty_eff": 1.0,
                              "judgment": {"domain_confidence": 0.9, "difficulty_confidence": 0.9,
                                           "difficulty": 1.0, "high_stakes": 0.1},
                              "explore_gate": {"confidence_ok": true, "explored": false}});
            body.push_str(&line.to_string());
            body.push('\n');
        }
        std::fs::write(&path, body).unwrap();
        assert!(gate_replay(&path, &[0.2, 0.5], 1000).is_err());
    }

    /// 单候选 gate 列表应拒绝（需要基档做 Δ 比较）
    #[test]
    fn gate_replay_requires_multiple_gates() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("events.jsonl");
        let mut body = String::new();
        for _ in 0..60u32 {
            let line = json!({"kind": "chat",
                              "scoreboard": [{"model_id": "a", "score": 0.9}, {"model_id": "b", "score": 0.8}],
                              "chosen": "a", "status": 200, "total_ms": 500,
                              "difficulty_eff": 1.0,
                              "judgment": {"domain_confidence": 0.9, "difficulty_confidence": 0.9,
                                           "difficulty": 1.0, "high_stakes": 0.1},
                              "explore_gate": {"confidence_ok": true, "explored": false}});
            body.push_str(&line.to_string());
            body.push('\n');
        }
        std::fs::write(&path, body).unwrap();
        assert!(gate_replay(&path, &[0.3], 1000).is_err());
    }
}

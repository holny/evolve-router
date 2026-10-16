use crate::slots::{build_slots, ChainSlot, SlotChainJudge, SlotKind};
use evolve_core::config::DecisionSlotCfg;
use evolve_core::types::{DigestSignals, Judge, JudgmentSet, RequestFeatures};
use std::sync::Arc;

/// M2 will add typesafe/laya HTTP backends behind this same constructor.
pub enum DecisionBackend {
    Heuristic,
}

pub struct HeuristicBackend;

impl Judge for HeuristicBackend {
    fn judge(&self, features: &RequestFeatures, digest: &DigestSignals, _candidates_hint: &str) -> JudgmentSet {
        evolve_core::heuristic::HeuristicJudge.judge(features, digest, _candidates_hint)
    }
}

/// 双判官取严融合（决策记录 #24 扩展）：Jev 语义判定为主，heuristic 为
/// 独立第二意见；冲突时取更保守值。
pub struct HybridBackend {
    primary: Box<dyn Judge>,
    secondary: evolve_core::heuristic::HeuristicJudge,
}

impl Judge for HybridBackend {
    fn judge(&self, features: &RequestFeatures, digest: &DigestSignals, _candidates_hint: &str) -> JudgmentSet {
        let j = self.primary.judge(features, digest, _candidates_hint);
        let h = self.secondary.judge(features, digest, _candidates_hint);
        evolve_core::types::fuse_judgments(&j, &h)
    }
}

/// w2 槽位链裁剪（强制后端名时只留对应族槽位）
fn pick_slots(kind: &str, cfg: &[DecisionSlotCfg]) -> Vec<ChainSlot> {
    let all = build_slots(cfg);
    match kind {
        "decision_model" | "typesafe" => all.into_iter().filter(|s| s.kind == SlotKind::Jev).collect(),
        "laya" => all.into_iter().filter(|s| s.kind == SlotKind::Laya).collect(),
        _ => all,
    }
}

impl DecisionBackend {
    /// w2 三槽位架构（用户裁决）：JEV > LAYA > OPENAI 优先级，配一即用；
    /// [decision].backend 仅作强制指定（默认 auto = 全槽位链）。
    /// 硬错（鉴权失败/404/无法连接）进程内拉黑该槽位，重启复位；
    /// 全链失败 → heuristic 兜底，永不阻塞。
    pub fn build(kind: &str, cfg: &[DecisionSlotCfg]) -> Box<dyn Judge> {
        let (judge, _) = Self::build_with_chain(kind, cfg);
        judge
    }

    /// 供 state.rs 同时拿 judge 与槽位链（advisor / distiller 取材）。
    /// 槽位来自配置（`[[decision.slots]]`）——不再读环境变量
    pub fn build_with_chain(kind: &str, cfg: &[DecisionSlotCfg]) -> (Box<dyn Judge>, Option<Arc<SlotChainJudge>>) {
        match kind {
            "heuristic" => (Box::new(HeuristicBackend), None),
            other => {
                let slots = pick_slots(other, cfg);
                if slots.is_empty() {
                    tracing::warn!("decision kind={other}: no slot configured → heuristic fallback");
                    return (Box::new(HeuristicBackend), None);
                }
                let chain = Arc::new(SlotChainJudge::new(slots));
                let judge: Box<dyn Judge> = match other {
                    // decision_model/typesafe 保持 #24 双判官融合语义
                    "decision_model" | "typesafe" => Box::new(HybridBackend {
                        primary: Box::new((*chain).clone()),
                        secondary: evolve_core::heuristic::HeuristicJudge,
                    }),
                    _ => Box::new((*chain).clone()),
                };
                (judge, Some(chain))
            }
        }
    }
}

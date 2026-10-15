//! 决策派生标签的**单一事实源**（用户裁决 2026-10）：难度档位 / 任务域 / 请求特征 /
//! 判定来源 / 难度↔能力匹配，全部在这里由 decision 算好写进事件，面板与 Web 只做渲染。
//!
//! 为什么放服务端：此前这些派生量在 Swift 与 JS 各写一遍，词表与阈值已经漂移
//! （难度边界 ≤0.8/≤2.0 vs <1/<1.9/<2.6、domain 词表两套、特征阈值 0.5 vs 0.6），
//! 面板作为 Web 的子级却给出不同结论。归一化到事件里即"派生量服务端化"惯例。
use evolve_core::types::{Domain, JudgmentSet};
use serde_json::{json, Value};

/// 特征阈值：与路由自身判定口径对齐——vision 0.5（engine 硬约束）、
/// stakes 0.6（engine 探索抑制 / scoring 高价值加成）；trivial/tools 路由不设闸，
/// 取 0.5 作为"判定器说有"的展示口径。
pub const FLAG_THRESHOLD: f32 = 0.5;
pub const VISION_THRESHOLD: f32 = 0.5;
pub const STAKES_THRESHOLD: f32 = 0.6;

/// 难度档位：边界与 engine 的价位层映射（sticky tier_ok / S5 比例控制器）同源
pub fn difficulty_label(difficulty: f32) -> &'static str {
    if difficulty <= 0.8 {
        "easy"
    } else if difficulty <= 2.0 {
        "medium"
    } else {
        "hard"
    }
}

/// 任务域短词（面板与 Web 共用一套）——比枚举名好读、比长描述短
pub fn domain_label(domain: Domain) -> &'static str {
    match domain {
        Domain::Code => "code",
        Domain::MathLogic => "math",
        Domain::Data => "data",
        Domain::AgentOps => "agent",
        Domain::Writing => "writing",
        Domain::Lookup => "lookup",
        Domain::Chitchat => "chat",
        Domain::Other => "other",
    }
}

/// 判定来源：5 条互斥路径。前两条走了决策模型，后三条没有
pub fn mode_label(judge_source: &str, sticky: bool) -> Option<&'static str> {
    if sticky {
        return Some("reused");
    }
    match judge_source {
        "decision_model" => Some("model"),
        "decision_model+rules" => Some("model+rules"),
        "heuristic" => Some("rules"),
        "explicit" => Some("pinned"),
        _ => None,
    }
}

fn capability_index(capability: &str) -> Option<usize> {
    match capability {
        "basic" => Some(0),
        "standard" => Some(1),
        "powerful" => Some(2),
        _ => None,
    }
}

/// 难度↔能力档位对齐（决策级结论：match / over / under）
pub fn fit_label(difficulty: f32, capability: Option<&str>) -> Option<&'static str> {
    let c = capability_index(capability?)?;
    let d = if difficulty <= 0.8 {
        0
    } else if difficulty <= 2.0 {
        1
    } else {
        2
    };
    Some(if d == c {
        "match"
    } else if c > d {
        "over"
    } else {
        "under"
    })
}

/// 请求特征命中（按阈值）
pub fn flags(j: &JudgmentSet) -> Vec<&'static str> {
    let mut out = Vec::new();
    if j.is_trivial > FLAG_THRESHOLD {
        out.push("trivial");
    }
    if j.tool_heavy > FLAG_THRESHOLD {
        out.push("tools");
    }
    if j.needs_vision > VISION_THRESHOLD {
        out.push("vision");
    }
    if j.high_stakes > STAKES_THRESHOLD {
        out.push("stakes");
    }
    out
}

/// 判定为空（粘性复用 / 显式指定）时只有 mode——其余字段是服务端占位值
/// （difficulty=0、flags=0，sticky 的 domain 直接沿用上轮），照原样展示会变成
/// "easy/code" 这种从没判过的结论（Web 端同口径：只出 mode 徽章）
fn judgment_is_placeholder(mode: &str) -> bool {
    mode == "reused" || mode == "pinned"
}

/// 事件里的 `tags` 对象（面板/Web 的唯一数据源）
pub fn derive(j: &JudgmentSet, capability: Option<&str>, sticky: bool) -> Value {
    let Some(mode) = mode_label(j.judge_source, sticky) else {
        return json!(null);
    };
    if judgment_is_placeholder(mode) {
        return json!({ "mode": mode });
    }
    json!({
        "difficulty": difficulty_label(j.difficulty),
        "domain": domain_label(j.domain),
        "flags": flags(j),
        "mode": mode,
        "fit": fit_label(j.difficulty, capability),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn j(difficulty: f32, domain: Domain, judge_source: &'static str) -> JudgmentSet {
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
            route_recommendation: None,
            route_recommendation_confidence: 0.0,
            judge_source,
        }
    }

    #[test]
    fn difficulty_and_fit_share_boundaries() {
        assert_eq!(difficulty_label(0.8), "easy");
        assert_eq!(difficulty_label(2.0), "medium");
        assert_eq!(difficulty_label(2.1), "hard");
        assert_eq!(fit_label(0.5, Some("basic")), Some("match"));
        assert_eq!(fit_label(0.5, Some("powerful")), Some("over"));
        assert_eq!(fit_label(2.5, Some("basic")), Some("under"));
        assert_eq!(fit_label(2.5, None), None);
        assert_eq!(fit_label(2.5, Some("unknown")), None);
    }

    #[test]
    fn placeholder_paths_emit_mode_only() {
        let reused = derive(&j(0.0, Domain::Code, "sticky"), Some("powerful"), true);
        assert_eq!(reused, json!({ "mode": "reused" }));
        let pinned = derive(&j(0.0, Domain::Other, "explicit"), Some("basic"), false);
        assert_eq!(pinned, json!({ "mode": "pinned" }));
        let model = derive(&j(1.2, Domain::Code, "decision_model"), Some("standard"), false);
        assert_eq!(model["mode"], "model");
        assert_eq!(model["difficulty"], "medium");
        assert_eq!(model["domain"], "code");
        assert_eq!(model["fit"], "match");
    }

    #[test]
    fn flags_use_routing_thresholds() {
        let mut x = j(1.0, Domain::Code, "heuristic");
        x.needs_vision = 0.5; // 不 > 0.5 → 不算 vision（与 engine 硬约束同口径）
        x.high_stakes = 0.6; // 不 > 0.6 → 不算 stakes（与探索抑制同口径）
        x.is_trivial = 0.51;
        assert_eq!(flags(&x), vec!["trivial"]);
    }
}

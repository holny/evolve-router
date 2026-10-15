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

/// 判定为空时只有 mode：**显式指定**（pinned）没走任何判定，服务端写的是
/// `Domain::Other` + 全 0 占位（relay 的 direct 分支）——照原样展示会变成
/// "easy/other" 这种从没判过的结论。
/// 粘性复用（reused）不再算占位：它沿用的是**同一次工具流程**上一轮的真实判定
/// （engine 粘性分支已把 domain/difficulty/flags 全量照搬），如实展示即可
/// （用户裁决 2026-10-15：沿用的值是真的，只有"没判定"才是假的）
fn judgment_is_placeholder(mode: &str) -> bool {
    mode == "pinned"
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
        // 显式指定：没判定过 → 只出 mode
        let pinned = derive(&j(0.0, Domain::Other, "explicit"), Some("basic"), false);
        assert_eq!(pinned, json!({ "mode": "pinned" }));
        // 粘性复用：沿用值是真的 → 全量出（含 mode）
        let mut st = j(1.2, Domain::Code, "sticky");
        st.is_trivial = 0.0;
        st.tool_heavy = 0.9;
        let reused = derive(&st, Some("standard"), true);
        assert_eq!(reused["mode"], "reused");
        assert_eq!(reused["difficulty"], "medium");
        assert_eq!(reused["domain"], "code");
        assert_eq!(reused["flags"], json!(["tools"]));
        assert_eq!(reused["fit"], "match");
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

/// 事件级派生（读取时补算）：老事件（部署前写入）没有 `tags` 字段，但输入齐全
/// （`judgment` / `sticky` / `model_tags.capability`）——用**同一份**规则补出来，
/// 免得面板/Web 在过渡期显示"没标签"的断档（用户 2026-10-15 观测到）
pub fn derive_from_event(ev: &Value) -> Option<Value> {
    let j = ev.get("judgment")?;
    let f = |k: &str| -> f32 {
        j.get(k)
            .and_then(|x| x.as_f64().or_else(|| x.as_i64().map(|i| i as f64)))
            .unwrap_or(0.0) as f32
    };
    let domain = match j.get("domain").and_then(|x| x.as_str()) {
        Some("Code") => Domain::Code,
        Some("MathLogic") => Domain::MathLogic,
        Some("Data") => Domain::Data,
        Some("AgentOps") => Domain::AgentOps,
        Some("Writing") => Domain::Writing,
        Some("Lookup") => Domain::Lookup,
        Some("Chitchat") => Domain::Chitchat,
        _ => Domain::Other,
    };
    let judge_source: &'static str = match ev.get("judge").and_then(|x| x.as_str()) {
        Some("decision_model") => "decision_model",
        Some("decision_model+rules") => "decision_model+rules",
        Some("heuristic") => "heuristic",
        Some("explicit") => "explicit",
        Some("sticky") => "sticky",
        _ => return None,
    };
    let mut set = JudgmentSet {
        domain,
        domain_confidence: 1.0,
        difficulty: f("difficulty"),
        difficulty_confidence: 1.0,
        needs_vision: f("needs_vision"),
        is_trivial: f("is_trivial"),
        tool_heavy: f("tool_heavy"),
        high_stakes: f("high_stakes"),
        session_relevance: 0.0,
        session_depth: 0.0,
        route_recommendation: None,
        route_recommendation_confidence: 0.0,
        judge_source,
    };
    // 旧事件的 difficulty 可能是 difficulty_eff 之外唯一来源；缺失时退回顶层
    if set.difficulty == 0.0 {
        set.difficulty = ev
            .get("difficulty_eff")
            .and_then(|x| x.as_f64())
            .unwrap_or(0.0) as f32;
    }
    let sticky = ev.get("sticky").and_then(|x| x.as_bool()).unwrap_or(false);
    let cap = ev
        .get("model_tags")
        .and_then(|m| m.get("capability"))
        .and_then(|x| x.as_str());
    let mut out = derive(&set, cap, sticky);
    // 补算期的诚实性：**旧**粘性事件（本次修复前写入，无 tags 字段）里的
    // difficulty/flags 是历史占位值 0——真实的沿用难度当时没落库，无法恢复。
    // 此时宁可不出难度（也不出依赖难度的 fit），别把占位 0 读成"easy"
    if sticky && set.difficulty == 0.0 {
        if let Some(o) = out.as_object_mut() {
            o.remove("difficulty");
            o.remove("fit");
        }
    }
    Some(out)
}

    #[test]
    fn legacy_event_gets_tags_backfilled() {
        // 部署前写入的事件：只有 judgment/sticky/model_tags，没有 tags
        let ev = json!({
            "kind": "chat",
            "judge": "decision_model+rules",
            "sticky": false,
            "difficulty_eff": 2.4,
            "judgment": {"domain": "Code", "difficulty": 2.4, "is_trivial": 0.0,
                          "tool_heavy": 0.7, "needs_vision": 0.0, "high_stakes": 0.9},
            "model_tags": {"capability": "powerful", "price_tier": "premium"},
        });
        let t = derive_from_event(&ev).expect("legacy event must backfill tags");
        assert_eq!(t["difficulty"], "hard");
        assert_eq!(t["domain"], "code");
        assert_eq!(t["mode"], "model+rules");
        assert_eq!(t["fit"], "match");
        assert_eq!(t["flags"], json!(["tools", "stakes"]));

        // 粘性复用事件 → 沿用真实判定（难度取自 judgment，缺失时退回 difficulty_eff）
        let sticky = json!({
            "judge": "sticky", "sticky": true, "difficulty_eff": 1.2,
            "judgment": {"domain": "Code", "difficulty": 1.2, "tool_heavy": 0.9},
        });
        let t = derive_from_event(&sticky).expect("legacy sticky tags");
        assert_eq!(t["mode"], "reused");
        assert_eq!(t["difficulty"], "medium");
        assert_eq!(t["domain"], "code");
        assert_eq!(t["flags"], json!(["tools"]));

        // 修复前的旧粘性事件：占位 0（真难度未落库）→ 不出难度/fit，别读成 easy
        let old_sticky = json!({
            "judge": "sticky", "sticky": true, "difficulty_eff": 0.0,
            "judgment": {"domain": "Code", "difficulty": 0.0, "tool_heavy": 0.0},
        });
        let t = derive_from_event(&old_sticky).expect("legacy sticky tags");
        assert_eq!(t["mode"], "reused");
        assert!(t.get("difficulty").is_none(), "placeholder difficulty must be hidden: {t}");
        assert!(t.get("fit").is_none());
        assert_eq!(t["domain"], "code", "domain is genuinely inherited");

        // 缺 judgment → 不补（老到没有判定字段的事件保持原样）
        assert_eq!(derive_from_event(&json!({"kind": "chat"})), None);
    }

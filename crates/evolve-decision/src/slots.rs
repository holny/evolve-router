//! w2 三槽位决策模型（用户裁决：JEV > LAYA > OPENAI 优先级，配一即用）。
//!
//! 每个槽位独立环境变量三元组（baseURL / API_KEY / MODEL），未配置即跳过。
//! "已配置" = base 或 key 任一非空；JEV 槽 key 即可（默认 TypeSafe 云），
//! LAYA 槽 key 可选（本地 sidecar 无鉴权），OPENAI 槽须显式 BASE_URL + MODEL。
//!
//! 失败记忆（进程内）：硬错（401/403 鉴权、404 端点或模型不存在、无法连接）
//! 标记槽位 dead，本进程内跳过；重启复位。软错（429/5xx/超时/解析失败）
//! 仅当当时跳过不拉黑。全部槽位失败 → heuristic 兜底，永不阻塞路由。

use crate::decision_backend::{DecisionModelBackend, SlotFail};
use crate::laya::LayaBackend;
use evolve_core::config::DecisionSlotCfg;
use evolve_core::types::{DigestSignals, Judge, JudgmentSet, RequestFeatures};
use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::Duration;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum SlotKind {
    Jev,
    Laya,
    OpenAI,
}

impl SlotKind {
    pub fn label(&self) -> &'static str {
        match self {
            SlotKind::Jev => "jev",
            SlotKind::Laya => "laya",
            SlotKind::OpenAI => "openai",
        }
    }
}

/// 槽位统一请求端：原生契约（jev/laya 各异）与 OpenAI chat 都收敛到
/// "进 state+questions（或 features），出 answers 对象"。
#[derive(Clone)]
pub enum SlotImpl {
    Model(DecisionModelBackend),
    Laya(LayaBackend),
}

#[derive(Clone)]
pub struct ChainSlot {
    pub kind: SlotKind,
    pub imp: SlotImpl,
}

impl ChainSlot {
    /// 返回 answers 对象；失败带分类（ChainJudge 据此决定拉黑或跳过）
    pub async fn consult_raw(
        &self,
        features: &RequestFeatures,
        digest: &DigestSignals,
        candidates_hint: &str,
        timeout: Duration,
    ) -> Result<serde_json::Value, SlotFail> {
        match &self.imp {
            SlotImpl::Model(b) => {
                let payload = b.build_payload(features, digest, candidates_hint);
                b.consult(payload["state"].clone(), payload["questions"].clone(), timeout).await
            }
            SlotImpl::Laya(l) => l.consult_raw(features, digest, timeout).await,
        }
    }
}

/// 进程内寿命的槽位死亡标记（重启复位）+ **最近一次失败**（含瞬态，供界面显示原因）
#[derive(Default)]
pub struct SlotHealth {
    dead: Mutex<HashMap<&'static str, SlotFail>>,
    last_fail: Mutex<Option<LastSlotFail>>,
}

/// 最近一次槽位失败（面板/网页横幅显示"为什么不可达"）
#[derive(Debug, Clone, serde::Serialize)]
pub struct LastSlotFail {
    pub slot: String,
    pub kind: SlotFail,
    /// 人读原因（如 "timeout" / "auth failed (401/403)"）
    pub label: String,
    pub at_ms: u64,
}

impl SlotHealth {
    /// 记录一次失败（瞬态也记：横幅要显示"最近为什么不可达"）
    pub fn note_fail(&self, kind: SlotKind, why: SlotFail) {
        if let Ok(mut g) = self.last_fail.lock() {
            *g = Some(LastSlotFail {
                slot: kind.label().to_string(),
                kind: why,
                // 人读标签只在 SlotFail::label() 定义一次（此前两处逐字重复）
                label: why.label().to_string(),
                at_ms: std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .map(|d| d.as_millis() as u64)
                    .unwrap_or(0),
            });
        }
    }

    /// 最近一次失败（读；空 = 没失败过）
    pub fn last_fail(&self) -> Option<LastSlotFail> {
        self.last_fail.lock().ok().and_then(|g| g.clone())
    }

    pub fn mark_dead(&self, kind: SlotKind, why: SlotFail) {
        let mut m = self.dead.lock().unwrap_or_else(|e| e.into_inner());
        if m.contains_key(kind.label()) {
            return;
        }
        tracing::warn!(slot = kind.label(), ?why, "decision slot marked dead (fatal, skips until process restart)");
        m.insert(kind.label(), why);
    }

    pub fn is_dead(&self, kind: SlotKind) -> bool {
        self.dead.lock().unwrap_or_else(|e| e.into_inner()).contains_key(kind.label())
    }

    pub fn dead_kinds(&self) -> Vec<&'static str> {
        self.dead.lock().unwrap_or_else(|e| e.into_inner()).keys().copied().collect()
    }
}

/// 槽位链判官（Arc 共享 health 给同进程多个使用者：engine judge / advisor）
/// Clone = 共享 health、复制槽位配置（强制后端名的双判官融合需要）
#[derive(Clone)]
pub struct SlotChainJudge {
    pub slots: Vec<ChainSlot>,
    pub health: Arc<SlotHealth>,
    /// 单次判定超时（默认 3s）。走远程中继/大模型判定端点时配置调大——
    /// 实测 Bamboo 中继单次往返 1.7-2.6s，3s 会在响应到达前就超时降级
    pub judge_timeout: Duration,
}

/// 默认判定超时（与旧硬编码一致）
pub const DEFAULT_JUDGE_TIMEOUT_MS: u64 = 3000;

impl SlotChainJudge {
    pub fn new(slots: Vec<ChainSlot>) -> Self {
        Self::with_timeout(slots, Duration::from_millis(DEFAULT_JUDGE_TIMEOUT_MS))
    }

    pub fn with_timeout(slots: Vec<ChainSlot>, judge_timeout: Duration) -> Self {
        let labels: Vec<&'static str> = slots.iter().map(|s| s.kind.label()).collect();
        tracing::info!(slots = ?labels, timeout_ms = judge_timeout.as_millis() as u64, "decision slot chain built (priority order)");
        Self { slots, health: Arc::new(SlotHealth::default()), judge_timeout }
    }

    /// 优先级最高的 Model 型槽位（蒸馏器/直推 advisor 用，laya 不参与）
    pub fn first_model(&self) -> Option<DecisionModelBackend> {
        self.slots.iter().find_map(|s| match &s.imp {
            SlotImpl::Model(b) => Some(b.clone()),
            SlotImpl::Laya(_) => None,
        })
    }

    /// Model 型槽位列表（advisor 用）
    fn model_slots(&self) -> Vec<&ChainSlot> {
        self.slots.iter().filter(|s| matches!(s.imp, SlotImpl::Model(_))).collect()
    }
}

impl Judge for SlotChainJudge {
    fn judge(&self, features: &RequestFeatures, digest: &DigestSignals, candidates_hint: &str) -> JudgmentSet {
        for slot in &self.slots {
            if self.health.is_dead(slot.kind) {
                continue;
            }
            let result: Option<Result<serde_json::Value, SlotFail>> = tokio::task::block_in_place(|| {
                tokio::runtime::Handle::try_current().ok().map(|h| {
                    h.block_on(async { slot.consult_raw(features, digest, candidates_hint, self.judge_timeout).await })
                })
            });
            match result {
                Some(Ok(answers)) => {
                    if let Some(j) = crate::decision_backend::parse_judgment_answers(&answers) {
                        return j;
                    }
                    // answers 形状不合预期（模型输出漂移）——软错，尝试下一槽位
                    tracing::warn!(slot = slot.kind.label(), "answers shape mismatch, trying next slot");
                }
                Some(Err(f)) if f.fatal() => {
                    self.health.note_fail(slot.kind, f);
                    self.health.mark_dead(slot.kind, f);
                }
                Some(Err(f)) => {
                    self.health.note_fail(slot.kind, f);
                    tracing::warn!(slot = slot.kind.label(), ?f, "slot transient failure, trying next slot");
                }
                None => {
                    // 无 tokio runtime（同步测试等边角）——跳到下一槽位
                }
            }
        }
        evolve_core::heuristic::HeuristicJudge.judge(features, digest, candidates_hint)
    }
}

/// 直推 advisor：仅 Model 槽位（laya 无 route_recommendation 能力），槽位间
/// 故障语义与 judge 相同（fatal 拉黑共享 health）
pub struct SlotAdvisor(pub Arc<SlotChainJudge>);

impl evolve_core::types::RouteAdvisor for SlotAdvisor {
    fn recommend(&self, task_summary: &str, candidates_json: &str, policy: &str) -> Option<(String, f32)> {
        let mut criteria = serde_json::Map::new();
        if let Ok(serde_json::Value::Array(arr)) = serde_json::from_str::<serde_json::Value>(candidates_json) {
            for x in &arr {
                if let Some(id) = x.get("id").and_then(|i| i.as_str()) {
                    criteria.insert(id.to_string(), serde_json::json!("candidate profile listed in state.candidates at this position"));
                }
            }
        }
        if criteria.is_empty() {
            return None;
        }
        let state = serde_json::json!({
            "task": task_summary.chars().take(600).collect::<String>(),
            "candidates": candidates_json.chars().take(6_000).collect::<String>(),
            "policy": policy,
        });
        let questions = serde_json::json!({
            "route_recommendation": {
                "type": "choice",
                "instructions": "Given this task and the candidate model profiles in state.candidates, which single model should handle this request? Consider capability match, cost efficiency, reliability track record, context window fit, and quota availability. Reply with exactly one candidate id from the criteria list.",
                "criteria": criteria,
            }
        });
        for slot in self.0.model_slots() {
            if self.0.health.is_dead(slot.kind) {
                continue;
            }
            let SlotImpl::Model(b) = &slot.imp else { continue };
            let result: Option<Result<serde_json::Value, crate::decision_backend::SlotFail>> = tokio::task::block_in_place(|| {
                tokio::runtime::Handle::try_current().ok().map(|h| {
                    h.block_on(async { b.consult(state.clone(), questions.clone(), self.0.judge_timeout + Duration::from_secs(2)).await })
                })
            });
            match result {
                Some(Ok(answers)) => {
                    let parsed = (|| {
                        let choice = answers.pointer("/route_recommendation/choice")?.as_str()?.to_string();
                        let conf = answers.pointer("/route_recommendation/confidence").and_then(|c| c.as_f64()).unwrap_or(0.5) as f32;
                        Some((choice, conf))
                    })();
                    if let Some(p) = parsed {
                        return Some(p);
                    }
                }
                Some(Err(f)) if f.fatal() => {
                    self.0.health.note_fail(slot.kind, f);
                    self.0.health.mark_dead(slot.kind, f);
                }
                Some(Err(f)) => {
                    self.0.health.note_fail(slot.kind, f);
                    tracing::warn!(slot = slot.kind.label(), ?f, "advisor transient failure, trying next slot");
                }
                None => {}
            }
        }
        None
    }
}

/// 决策横幅状态判定：任一槽位配置即 ok；否则 missing
pub fn any_slot_configured(cfg: &[DecisionSlotCfg]) -> bool {
    !build_slots(cfg).is_empty()
}

/// 带显式协议的槽位构造：`protocol` 非空时覆盖 URL 判别
pub fn slot_from_with_protocol(
    kind: SlotKind,
    base: Option<String>,
    api_key: Option<String>,
    model: Option<String>,
    protocol: Option<String>,
    think_mode: Option<String>,
) -> Option<SlotImpl> {
    let explicit = protocol.map(|p| p.trim().to_lowercase()).filter(|p| !p.is_empty());
    let kind = match explicit.as_deref() {
        // 显式 openai → 按 OPENAI 槽的规则构造（base + model 必填）
        Some(p) if p.contains("openai") || p.contains("chat") => SlotKind::OpenAI,
        _ => kind,
    };
    let mut imp = slot_from(kind, base.clone(), api_key.clone(), model.clone())?;
    if let SlotImpl::Model(b) = imp {
        imp = SlotImpl::Model(b.with_think_mode(think_mode.clone()));
    }
    match explicit.as_deref() {
        // 显式原生契约：即使 kind 是 OpenAI 也强制走 SystemOne 形状
        Some(p) if p.contains("system") || p.contains("decision") => match imp {
            SlotImpl::Model(b) => Some(SlotImpl::Model(b.with_systemone_protocol())),
            other => Some(other),
        },
        _ => Some(imp),
    }
}

/// 纯函数：槽位配置规则（测试友好；build_slots 只读 env 后调用）
pub fn slot_from(kind: SlotKind, base: Option<String>, api_key: Option<String>, model: Option<String>) -> Option<SlotImpl> {
    let base = base.map(|s| s.trim().to_string()).filter(|s| !s.is_empty());
    let key = api_key.map(|k| k.trim().to_string()).filter(|k| !k.is_empty());
    match kind {
        SlotKind::Jev => {
            // key 即可（默认 TypeSafe 云）；base 显式给时固定原生契约形状
            //（{model, state, questions} → {answers}），本地部署免 key
            if base.is_none() && key.is_none() {
                return None;
            }
            let b = DecisionModelBackend::from_parts(key.as_deref(), base.as_deref(), model.as_deref(), Some("system-one")).ok()?;
            Some(SlotImpl::Model(b))
        }
        SlotKind::Laya => {
            // LAYA 槽必须显式 base（本地 sidecar 地址无从默认到用户实例）；key 可选
            let base = base?;
            Some(SlotImpl::Laya(LayaBackend::with_endpoint(base.trim_end_matches('/'), key.as_deref())))
        }
        SlotKind::OpenAI => {
            // OPENAI 槽：BASE_URL + MODEL 必填，key 可选（ollama/vLLM 无鉴权）
            let base = base?;
            let model = model?;
            let b = DecisionModelBackend::from_parts(key.as_deref(), Some(&base), Some(&model), Some("openai")).ok()?;
            Some(SlotImpl::Model(b))
        }
    }
}

/// 按配置组装三槽位（优先级顺序 JEV > LAYA > OPENAI）。
///
/// **配置唯一来源 = `~/.evo-router/evo-router.toml` 的 `[[decision.slots]]`**
/// （2026-10-16 用户裁决：统一走 toml）。此前只读环境变量，于是"菜单栏 app /
/// 开机自启"拉起的网关拿不到 shell 里的 key → 面板显示"未配置决策模型"；
/// 现在 key 可内联（`api_key`）或经 `api_key_env` 显式引用环境变量。
pub fn build_slots(cfg: &[DecisionSlotCfg]) -> Vec<ChainSlot> {
    let mut out = Vec::new();
    for kind in [SlotKind::Jev, SlotKind::Laya, SlotKind::OpenAI] {
        // 同一 kind 取配置里的第一项；不合法（如 openai 缺 base/model）→ 告警跳过
        let Some(entry) = cfg.iter().find(|s| s.kind.trim().eq_ignore_ascii_case(kind.label())) else {
            continue;
        };
        let key = match (&entry.api_key, &entry.api_key_env) {
            (Some(k), _) if !k.trim().is_empty() => Some(k.clone()),
            (_, Some(env_name)) => match std::env::var(env_name) {
                Ok(v) if !v.trim().is_empty() => Some(v),
                _ => {
                    tracing::warn!(slot = kind.label(), env = env_name, "decision slot: api_key_env is not set");
                    None
                }
            },
            _ => None,
        };
        // protocol 显式指定时优先（原生契约端点未必带 "systemone" 字样，
        // 如 StartLux / Bamboo 的 /v1/decisions）
        match slot_from_with_protocol(
            kind,
            entry.base_url.clone(),
            key,
            entry.model.clone(),
            entry.protocol.clone(),
            entry.think_mode.clone(),
        ) {
            Some(imp) => out.push(ChainSlot { kind, imp }),
            None => tracing::warn!(
                slot = kind.label(),
                "decision slot skipped (incomplete config: see [[decision.slots]] in evo-router.toml)"
            ),
        }
    }
    out
}

#[cfg(test)]
mod slot_config_tests {
    use super::*;
    use evolve_core::config::DecisionSlotCfg;

    fn cfg(kind: &str, key: Option<&str>, key_env: Option<&str>, base: Option<&str>, model: Option<&str>) -> DecisionSlotCfg {
        DecisionSlotCfg {
            kind: kind.into(),
            api_key: key.map(String::from),
            api_key_env: key_env.map(String::from),
            base_url: base.map(String::from),
            model: model.map(String::from),
            protocol: None,
            think_mode: None,
        }
    }

    #[test]
    fn slots_come_from_config_only() {
        // 空配置 → 无槽位（**不再**回退环境变量：统一走 toml，用户裁决 2026-10-16）
        assert!(build_slots(&[]).is_empty());
        // 内联 key 即可（jev 云端）
        let slots = build_slots(&[cfg("jev", Some("k"), None, None, None)]);
        assert_eq!(slots.len(), 1);
        assert_eq!(slots[0].kind, SlotKind::Jev);
    }

    #[test]
    fn slots_keep_priority_order_regardless_of_config_order() {
        // 配置里倒序书写，输出仍须 JEV > LAYA > OPENAI
        let slots = build_slots(&[
            cfg("openai", Some("k"), None, Some("http://127.0.0.1:9/v1"), Some("m")),
            cfg("laya", None, None, Some("http://127.0.0.1:8/judge"), None),
            cfg("jev", Some("k"), None, None, None),
        ]);
        let kinds: Vec<&str> = slots.iter().map(|s| s.kind.label()).collect();
        assert_eq!(kinds, vec!["jev", "laya", "openai"]);
    }

    #[test]
    fn api_key_env_is_resolved_and_missing_env_degrades() {
        // 间接引用：读环境变量取值（key 值不落配置文件）
        unsafe { std::env::set_var("EV_TEST_SLOT_KEY", "from-env") };
        let slots = build_slots(&[cfg("jev", None, Some("EV_TEST_SLOT_KEY"), None, None)]);
        assert_eq!(slots.len(), 1, "api_key_env must resolve");
        unsafe { std::env::remove_var("EV_TEST_SLOT_KEY") };
        // 变量缺失且无 base → 槽位不成立（告警跳过，不 panic、不阻塞路由）
        assert!(build_slots(&[cfg("jev", None, Some("EV_TEST_SLOT_KEY"), None, None)]).is_empty());
    }

    #[test]
    fn invalid_slots_are_skipped_not_fatal() {
        // openai 缺 base/model、laya 缺 base → 跳过；同配置里的 jev 仍生效
        let slots = build_slots(&[
            cfg("openai", Some("k"), None, None, None),
            cfg("laya", None, None, None, None),
            cfg("jev", Some("k"), None, None, None),
        ]);
        assert_eq!(slots.len(), 1);
        assert_eq!(slots[0].kind, SlotKind::Jev);
        assert!(any_slot_configured(&[cfg("jev", Some("k"), None, None, None)]));
        assert!(!any_slot_configured(&[]));
    }

    #[test]
    fn think_mode_reaches_the_payload_and_timeout_is_configurable() {
        use evolve_core::types::{DigestSignals, RequestFeatures};
        let cfg = DecisionSlotCfg {
            kind: "jev".into(),
            api_key: Some("bk-x".into()),
            base_url: Some("https://api.bamboonode.dev/v1/systemone".into()),
            model: Some("startlux-decision-27b-autothink".into()),
            think_mode: Some("never".into()),
            protocol: None,
            api_key_env: None,
        };
        let slots = build_slots(&[cfg]);
        match &slots[0].imp {
            SlotImpl::Model(b) => {
                let f = RequestFeatures {
                    est_input_tokens: 100,
                    user_text_chars: 12,
                    code_density: 0.0,
                    tool_count: 0,
                    tool_ratio: 0.0,
                    has_images: false,
                    turn_count: 1,
                    cjk_ratio: 0.0,
                };
                let d = DigestSignals::default();
                let payload = b.build_payload(&f, &d, "");
                assert_eq!(payload["think_mode"], "never", "think_mode must reach the request body");
            }
            _ => panic!("expected model slot"),
        }
        // 超时可配：默认 3s，配置可调大（远程中继往返就 1.7-2.6s）
        let chain = SlotChainJudge::new(vec![]);
        assert_eq!(chain.judge_timeout.as_millis(), 3000);
        let chain = SlotChainJudge::with_timeout(vec![], std::time::Duration::from_millis(12_000));
        assert_eq!(chain.judge_timeout.as_millis(), 12_000);
    }

    #[test]
    fn startlux_decisions_endpoint_is_systemone() {
        use crate::decision_backend::DecisionProtocol;
        // StartLux / Bamboo 的原生路径（不带 systemone 字样）也必须判为原生契约
        let slots = build_slots(&[DecisionCfgHelper::jev_decisions()]);
        assert_eq!(slots.len(), 1);
        match &slots[0].imp {
            SlotImpl::Model(b) => {
                assert_eq!(b.protocol, DecisionProtocol::SystemOne, "…/v1/decisions must be native");
                assert_eq!(b.endpoint, "https://api.bamboonode.dev/v1/decisions");
                assert_eq!(b.model, "startlux-decision-27b-autothink");
            }
            _ => panic!("expected model slot"),
        }
    }

    #[test]
    fn explicit_protocol_overrides_url_guess() {
        use crate::decision_backend::DecisionProtocol;
        // 路径不含 systemone/decisions，但显式 protocol = "system-one" → 仍走原生契约
        let cfg = DecisionSlotCfg {
            kind: "jev".into(),
            api_key: Some("k".into()),
            base_url: Some("http://127.0.0.1:8090/api".into()),
            model: None,
            protocol: Some("system-one".into()),
            think_mode: None,
            api_key_env: None,
        };
        match &build_slots(&[cfg])[0].imp {
            SlotImpl::Model(b) => assert_eq!(b.protocol, DecisionProtocol::SystemOne),
            _ => panic!("expected model slot"),
        }
    }
/// 测试用构造 helper（StartLux / Bamboo 决策端点）
struct DecisionCfgHelper;
impl DecisionCfgHelper {
    fn jev_decisions() -> DecisionSlotCfg {
        DecisionSlotCfg {
            kind: "jev".into(),
            api_key: Some("bk-test".into()),
            base_url: Some("https://api.bamboonode.dev/v1/decisions".into()),
            model: Some("startlux-decision-27b-autothink".into()),
            protocol: None,
            think_mode: None,
            api_key_env: None,
        }
    }
}
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::decision_backend::DecisionProtocol;

    #[test]
    fn jev_slot_rules() {
        // key 即可（默认云端）
        let s = slot_from(SlotKind::Jev, None, Some("k".into()), None).unwrap();
        assert!(matches!(s, SlotImpl::Model(_)));
        // 都没有 → 未配置
        assert!(slot_from(SlotKind::Jev, None, None, None).is_none());
        // base 即可（本地部署，免 key，原生契约形状固定）
        let s = slot_from(SlotKind::Jev, Some("http://127.0.0.1:8000/system_one".into()), None, None).unwrap();
        if let SlotImpl::Model(b) = s {
            assert_eq!(b.protocol, DecisionProtocol::SystemOne);
        } else {
            panic!("expected model slot");
        }
    }

    #[test]
    fn laya_openai_slot_rules() {
        // LAYA 必须显式 base
        assert!(slot_from(SlotKind::Laya, None, None, None).is_none());
        assert!(slot_from(SlotKind::Laya, Some(" http://127.0.0.1:8321/v1/judge ".into()), None, None).is_some());
        // OPENAI 必须 base+model，key 可选
        assert!(slot_from(SlotKind::OpenAI, Some("http://127.0.0.1:1234/v1".into()), None, None).is_none());
        let openai = slot_from(SlotKind::OpenAI, Some("http://127.0.0.1:1234/v1".into()), None, Some("og-9b".into())).unwrap();
        if let SlotImpl::Model(b) = openai {
            assert_eq!(b.endpoint, "http://127.0.0.1:1234/v1/chat/completions");
        } else {
            panic!("expected model slot");
        }
        assert!(slot_from(SlotKind::OpenAI, None, Some("k".into()), Some("m".into())).is_none());
    }
}

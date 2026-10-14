//! TypeSafe Jev decision backend (System One: state + typed questions).
//!
//! Context management: the 8 question definitions are the stable part of
//! every request; only `state` varies. State is budget-capped (extractive
//! digest + bucketized features, no raw content under redact=true) so each
//! decision costs a bounded number of input tokens. Sticky fast-path and
//! judgment reuse keep the call frequency low; on any backend failure we
//! fall back to the heuristic judge so routing never blocks.

use async_trait::async_trait;
use evolve_core::types::*;
use serde_json::json;
use std::time::Duration;

pub const STATE_TOKEN_BUDGET: usize = 2048;

/// advisor 调用失败后的全局冷却截止（epoch ms）：60s 内不再白打调用
/// （consult 吞掉 HTTP 状态细节，抓取/解析失败等同计失败）
static ADVISOR_FAIL_UNTIL: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

fn now_millis() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

#[derive(Clone)]
pub struct DecisionModelBackend {
    pub client: reqwest::Client,
    pub api_key: String,
    pub model: String,
    pub endpoint: String,
    pub redact: bool,
    /// w1 双协议：SystemOne = 原生 {state, questions}→{answers} 契约；
    /// OpenAIChat = 任意 chat/completions 兼容端点（vLLM/ollama/OpenRouter/各家云）——
    /// 契约问题内置为 prompt，回复内容解析出 {answers}
    pub protocol: DecisionProtocol,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DecisionProtocol {
    SystemOne,
    OpenAIChat,
}

/// w2 槽位失败分类：fatal → ChainJudge 拉黑槽位（进程内，重启复位）；
/// 其余仅当次跳过。
/// - Auth：401/403 鉴权失败（key 错/失效）
/// - NotFound：404（端点不存在 / 模型不存在）
/// - Unreachable：无法建立连接（服务未起 / DNS / 拒绝连接）
/// - Timeout/Server/Parse：暂时性——限流、5xx、双端时序、模型输出不合预期
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SlotFail {
    Auth,
    NotFound,
    Unreachable,
    Timeout,
    Server,
    Parse,
}

impl SlotFail {
    pub fn fatal(&self) -> bool {
        matches!(self, SlotFail::Auth | SlotFail::NotFound | SlotFail::Unreachable)
    }
}

#[async_trait]
impl Judge for DecisionModelBackend {
    fn judge(&self, features: &RequestFeatures, digest: &DigestSignals, candidates_hint: &str) -> JudgmentSet {
        let payload = self.build_payload(features, digest, candidates_hint);
        let state = payload["state"].clone();
        let questions = payload["questions"].clone();
        let consult = ConsultOnce { backend: self.clone(), state, questions, timeout: Duration::from_millis(3000) };
        // Engine is sync; we are always called from the multi-thread runtime.
        let result = tokio::task::block_in_place(|| {
            tokio::runtime::Handle::try_current().ok().map(|h| h.block_on(async { consult.run("decision model judgment failed").await }))
        });
        match result.flatten().as_ref().and_then(parse_judgment_answers) {
            Some(j) => j,
            None => {
                evolve_core::heuristic::HeuristicJudge.judge(features, digest, candidates_hint)
            }
        }
    }
}

/// w1 统一薄封装：state+questions → answers，失败自动记 warn 并返回 None。
/// judge/recommend/distill/judge_quality 各自行点均由此发出网络调用，
/// protocol 差异（SystemOne vs OpenAIChat）被封在 consult 内部。
#[derive(Clone)]
struct ConsultOnce {
    backend: DecisionModelBackend,
    state: serde_json::Value,
    questions: serde_json::Value,
    timeout: Duration,
}

impl ConsultOnce {
    async fn run(self, warn: &str) -> Option<serde_json::Value> {
        match self.backend.consult(self.state, self.questions, self.timeout).await {
            Ok(a) => Some(a),
            Err(f) => {
                tracing::warn!("{warn}: {f:?}");
                None
            }
        }
    }
}

impl DecisionModelBackend {
    /// v2 直接路由推荐：决策模型看到任务文本+候选全画像，直接推荐哪个模型。
    /// 返回 (model_id, confidence)。调用失败返回 None（调用方走公式回退）。
    pub fn recommend_route(
        &self,
        task_summary: &str,
        candidates_json: &str,
        policy: &str,
    ) -> Option<(String, f32)> {
        // 冷却期内直接跳过（静默，不刷日志）
        if now_millis() < ADVISOR_FAIL_UNTIL.load(std::sync::atomic::Ordering::Relaxed) {
            return None;
        }
        // choice 问题的 criteria 是必填的选项映射（缺失 → 422）：
        // 从候选画像提取占位 id 动态生成
        let cand_ids: Vec<String> = serde_json::from_str::<serde_json::Value>(candidates_json)
            .ok()
            .and_then(|v| v.as_array().cloned())
            .map(|a| {
                a.iter()
                    .filter_map(|x| x.get("id").and_then(|i| i.as_str()).map(String::from))
                    .collect()
            })
            .unwrap_or_default();
        let mut criteria = serde_json::Map::new();
        for id in &cand_ids {
            criteria.insert(
                id.clone(),
                json!("candidate profile listed in state.candidates at this position"),
            );
        }
        let state = json!({
            "task": task_summary.chars().take(600).collect::<String>(),
            "candidates": candidates_json.chars().take(6_000).collect::<String>(),
            "policy": policy,
        });
        let questions = json!({
            "route_recommendation": {
                "type": "choice",
                "instructions": "Given this task and the candidate model profiles in state.candidates, which single model should handle this request? Consider capability match, cost efficiency, reliability track record, context window fit, and quota availability. Reply with exactly one candidate id from the criteria list.",
                "criteria": criteria,
            }
        });
        let consult = ConsultOnce {
            backend: self.clone(),
            state: state.clone(),
            questions: questions.clone(),
            timeout: Duration::from_secs(5),
        };
        // 451/5xx 间歇性（边缘风控），退避后重试一次
        let result = tokio::task::block_in_place(|| {
            tokio::runtime::Handle::try_current().ok().and_then(|h| {
                h.block_on(async {
                    let first = consult.clone().run("route_advisor: recommend call failed").await;
                    if first.is_none() {
                        // 可重试错误（连接/5xx/429）退避 500ms 再试一次；consult 已吞掉
                        // 具体错误类型——重试仅对失败一次的情况做，成本低
                        tokio::time::sleep(Duration::from_millis(500)).await;
                        consult.run("route_advisor: recommend retry failed").await
                    } else {
                        first
                    }
                })
            })
        });
        let Some(a) = result else {
            ADVISOR_FAIL_UNTIL.store(now_millis() + 60_000, std::sync::atomic::Ordering::Relaxed);
            return None;
        };
        let parsed = (|| {
            let choice = a.pointer("/route_recommendation/choice")?.as_str()?.to_string();
            let conf = a.pointer("/route_recommendation/confidence")
                .and_then(|c| c.as_f64()).unwrap_or(0.5) as f32;
            Some((choice, conf))
        })();
        if parsed.is_none() {
            ADVISOR_FAIL_UNTIL.store(now_millis() + 60_000, std::sync::atomic::Ordering::Relaxed);
            tracing::warn!(answers = %a, "route_advisor: recommend response missing choice");
        }
        parsed
    }

    /// env 读取：DECISION_* 为标准名（DECISION_API_KEY / DECISION_BASE_URL /
    /// DECISION_MODEL / DECISION_PROTOCOL / DECISION_REDACT）；DECISION_MODEL_* /
    /// TYPESAFE_* 为历史名（回退兼容，已部署环境无需改动）。
    /// w2 两大协议族统一接入 = baseURL + apiKey + modelId(可选) +
    /// protocol（自动判别）：
    /// - 原生 System One（TypeSafe Jev 云 / Laya sidecar / 未来新端点）：
    ///   请求 {state, questions} → {answers}，API key 可选（仅默认 Jev 云必填）；
    ///   BASE_URL 以 /judge 结尾识别为 Laya 型判定端点
    /// - OpenAI 兼容（chat/completions）：判定契约编译为 prompt（模板内置），
    ///   约定输出 {answers} JSON，此时 DECISION_MODEL 必填
    fn env_var(names: &[&str]) -> Option<String> {
        for n in names {
            if let Ok(v) = std::env::var(n) {
                return Some(v);
            }
        }
        None
    }

    pub fn from_env() -> Option<Self> {
        let api_key = Self::env_var(&["DECISION_API_KEY", "DECISION_MODEL_API_KEY", "TYPESAFE_API_KEY"])
            .filter(|k| !k.is_empty());
        let base = Self::env_var(&["DECISION_BASE_URL", "DECISION_MODEL_ENDPOINT", "TYPESAFE_ENDPOINT"]);
        let model = Self::env_var(&["DECISION_MODEL", "DECISION_MODEL_MODEL", "TYPESAFE_MODEL"]);
        let protocol_hint = Self::env_var(&["DECISION_PROTOCOL"]);
        let redact = Self::env_var(&["DECISION_REDACT", "DECISION_MODEL_REDACT", "TYPESAFE_REDACT"])
            .map(|v| v != "0")
            .unwrap_or(true);
        let mut b = match base {
            Some(ref b) if Self::is_judge_style(b) => {
                // Laya 型判定端点：key 可选、model 无关
                Self::from_parts(api_key.as_deref(), Some(b), None, protocol_hint.as_deref()).ok()?
            }
            Some(ref b) => {
                Self::from_parts(api_key.as_deref(), Some(b), model.as_deref(), protocol_hint.as_deref()).ok()?
            }
            None => {
                // 默认 TypeSafe Jev 云：不设 BASE_URL 时 key 必填
                Self::from_parts(api_key.as_deref(), None, model.as_deref(), protocol_hint.as_deref()).ok()?
            }
        };
        b.redact = redact;
        Some(b)
    }

    /// Laya 型判定端点识别（/judge 结尾）——决策选型与蒸馏器门槛共用
    pub fn is_judge_style(base: &str) -> bool {
        let b = base.trim_end_matches('/');
        b.ends_with("/judge")
    }

    /// Jev 型端点识别：systemone / system-one / system_one（去分隔符归一化，大小写无关）
    /// ——本地原生契约服务端点的常用路径命名都不落判别盲区。
    /// 兜底：判错协议的调用失败一律自动降级 heuristic，用户也可用
    /// DECISION_PROTOCOL 强制指定，因此误判只会表现为回退启发式而非阻塞。
    pub fn is_systemone_style(base: &str) -> bool {
        let folded: String = base
            .to_lowercase()
            .chars()
            .filter(|c| *c != '-' && *c != '_')
            .collect();
        folded.contains("systemone")
    }

    /// w1 双协议统一咨询 → Result<Value, SlotFail>（w2 槽位链路需要失败分类）
    pub async fn consult(
        &self,
        state: serde_json::Value,
        questions: serde_json::Value,
        timeout: Duration,
    ) -> Result<serde_json::Value, SlotFail> {
        let payload = match self.protocol {
            DecisionProtocol::SystemOne => {
                json!({ "model": &self.model, "state": state, "questions": questions })
            }
            DecisionProtocol::OpenAIChat => {
                let system = "You are the decision model of an LLM router. You receive a `state` object describing the current request/session (and possibly candidate models) and a `questions` object of typed questions: type \"choice\" = answer with exactly one criterion id in `choice`; type \"noul\" = probability 0-1 in `noul`; type \"score\" = rating 0-3 in `score`. Every answer should include `confidence` (0-1). Reply with ONLY one JSON object, no prose and no markdown fences, shaped exactly as: {\"answers\": { \"<question_key>\": { ...typed fields shown above... } }}. For a `choice` question whose key is route_recommendation, the choice must be a candidate id listed in state.candidates.".to_string();
                let user = format!(
                    "state:\n{}\n\nquestions:\n{}",
                    serde_json::to_string_pretty(&state).map_err(|_| SlotFail::Parse)?,
                    serde_json::to_string_pretty(&questions).map_err(|_| SlotFail::Parse)?
                );
                json!({
                    "model": &self.model,
                    "messages": [
                        { "role": "system", "content": system },
                        { "role": "user", "content": user },
                    ],
                })
            }
        };
        let mut req = self.client.post(&self.endpoint);
        if !self.api_key.is_empty() {
            req = req.bearer_auth(&self.api_key);
        }
        let v = req
            .timeout(timeout)
            .json(&payload)
            .send()
            .await
            .map_err(|e| {
                if e.is_timeout() {
                    SlotFail::Timeout
                } else if e.is_connect() {
                    SlotFail::Unreachable
                } else {
                    SlotFail::Server
                }
            })?;
        let status = v.status();
        if !status.is_success() {
            return Err(match status.as_u16() {
                401 | 403 => SlotFail::Auth,
                404 => SlotFail::NotFound,
                429 => SlotFail::Server,
                _ if status.is_server_error() => SlotFail::Server,
                _ => SlotFail::Server,
            });
        }
        let v = v.json::<serde_json::Value>().await.map_err(|_| SlotFail::Parse)?;
        match self.protocol {
            DecisionProtocol::SystemOne => Ok(v.get("answers").cloned().ok_or(SlotFail::Parse)?),
            DecisionProtocol::OpenAIChat => {
                let content = v.pointer("/choices/0/message/content").and_then(|c| c.as_str()).ok_or(SlotFail::Parse)?;
                let parsed = extract_json(content).ok_or(SlotFail::Parse)?;
                Ok(parsed.get("answers").cloned().unwrap_or(parsed))
            }
        }
    }

    /// ⑧ 能力卡蒸馏（FlyRoute）：观测统计达标后由 Jev 重估模型 tiers。
    /// 返回 [reasoning, coding, vision, agentic]（0-1）。失败返回 None——
    /// 静默跳过，下个调度周期再试；失败永不影响路由主链路。
    pub async fn distill_tiers(
        &self,
        model_id: &str,
        stats_summary: &serde_json::Value,
    ) -> Option<[f32; 4]> {
        let criteria = ["weak", "moderate", "strong", "elite"];
        let tier_q = |dim: &str| {
            json!({
                "type": "score",
                "instructions": format!(
                    "Rate this model's {} capability based on the observed production statistics in state.observations.",
                    dim
                ),
                "criteria": criteria,
            })
        };
        let state = json!({
            "task": format!(
                "Re-estimate the capability tiers of model '{}' from its observed production statistics. Statistics reflect real traffic outcomes.",
                model_id
            ),
            "observations": stats_summary,
        });
        let questions = json!({
            "reasoning_tier": tier_q("reasoning"),
            "coding_tier": tier_q("coding"),
            "vision_tier": tier_q("vision"),
            "agentic_tier": tier_q("agentic tool use"),
        });
        let consult = ConsultOnce {
            backend: self.clone(),
            state,
            questions,
            timeout: Duration::from_secs(20),
        };
        let answers = match tokio::task::block_in_place(|| {
            tokio::runtime::Handle::try_current().ok().and_then(|h| {
                h.block_on(async { consult.run("decision model distill_tiers failed").await })
            })
        }) {
            Some(a) => a,
            None => return None, // 无 tokio runtime（CLI 路径），静默跳过下个周期再试
        };
        // score 类型 answer 形状：answers.<key>.score，0-3 刻度（同 session_depth）—
        // consult 返回的即为 answers 对象
        let tier = |k: &str| -> f32 {
            answers
                .pointer(&format!("/{k}/score"))
                .and_then(|s| s.as_f64())
                .map(|s| (s as f32 / 3.0).clamp(0.0, 1.0))
                .unwrap_or(0.5)
        };
        Some([
            tier("reasoning_tier"),
            tier("coding_tier"),
            tier("vision_tier"),
            tier("agentic_tier"),
        ])
    }

    /// A3b LLM-as-a-Judge：point-wise 充分性评分 [0,1]（backtest 采样用）。
    /// 单一维度（是否充分且正确回答）——防风格偏见；judge 与被评模型异构
    /// ——防自我偏好。失败返回 None。
    pub async fn judge_response_quality(&self, query: &str, response: &str) -> Option<f32> {
        let state = json!({
            "task": query.chars().take(600).collect::<String>(),
            "response": response.chars().take(1200).collect::<String>(),
        });
        let questions = json!({
            "sufficiency": {
                "type": "noul",
                "instructions": "Does this response adequately and correctly answer the query? Judge only sufficiency and correctness — ignore style, tone, and wording.",
            }
        });
        let consult = ConsultOnce {
            backend: self.clone(),
            state,
            questions,
            timeout: Duration::from_secs(15),
        };
        let answers = consult.run("decision model judge_response_quality failed").await?;
        answers
            .pointer("/sufficiency/noul")
            .and_then(|v| v.as_f64())
            .map(|v| v as f32)
    }

    /// w2 槽位链要用：公开 payload 组装（state+questions 一次成型）
    pub fn build_payload(&self, features: &RequestFeatures, digest: &DigestSignals, candidates_hint: &str) -> serde_json::Value {
        let (first_head, current_head) = if self.redact {
            // bucketed placeholder: no user text leaves the machine
            ("[text]".to_string(), "[text]".to_string())
        } else {
            (head(&digest.first_user_text, 200), head(&digest.last_user_text, 400))
        };
        let state = json!({
            "task": {
                "length_bucket": bucket(features.user_text_chars, &[30, 100, 400, 1500]),
                "code_density": (features.code_density * 100.0).round() / 100.0,
                "tool_count": features.tool_count,
                "has_images": features.has_images,
                "turn_count": features.turn_count.min(999),
                "est_tokens": features.est_input_tokens,
            },
            "session": {
                "first_task_head": first_head,
                "current_head": current_head,
                "continues_topic": digest.has_deixis || digest.overlap_ratio > 0.15,
                "topic_shift": digest.topic_shift_marker,
                "tools_seen": digest.session_tools_seen.min(999),
            },
        });
        let mut payload = json!({
            "model": self.model,
            "state": state,
            "questions": {
                "task_domain": {
                    "type": "choice",
                    "instructions": "What is the primary domain of the user's current request?",
                    "criteria": {
                        "code": "software engineering, refactoring, debugging, architecture, tests",
                        "math_logic": "mathematics, proofs, calculations, logic puzzles",
                        "writing": "creative or professional writing, emails, translation, copy",
                        "factual_lookup": "facts, definitions, how-to questions",
                        "data_analysis": "statistics, SQL, spreadsheets, metrics",
                        "chitchat": "greetings, small talk, pleasantries",
                        "agent_ops": "running commands, managing services, shell work",
                        "other": "none of the above"
                    }
                },
                "difficulty": {
                    "type": "choice",
                    "instructions": "How hard is this request for a language model? Pick the HIGHEST level that applies — anchor on the examples, not on a vague feeling.",
                    "criteria": {
                        "l1": "L1 trivial: greetings/small talk, single fact lookup, one-sentence translation, rename or reformat — zero reasoning chains",
                        "l2": "L2 moderate: single-file edit, one focused function, straightforward debugging, explain a known concept, careful multi-step but well-trodden work",
                        "l3": "L3 complex: multi-file refactor, architecture trade-offs, novel algorithm design, cross-module debugging, long multi-step plans",
                        "l4": "L4 critical: production changes touching money/auth/security/migrations, distributed consistency, performance-critical paths, frontier research problems"
                    }
                },
                "needs_vision": {"type": "noul", "instructions": "Does answering require seeing images?"},
                "is_trivial": {"type": "noul", "instructions": "Is this answerable in one short sentence with no tools?"},
                "tool_heavy": {"type": "noul", "instructions": "Is this request tool-intensive (many tool invocations needed)?"},
                "high_stakes": {"type": "noul", "instructions": "Could this request cause money, legal, medical, safety or production damage?"},
                "session_relevance": {"type": "noul", "instructions": "Does the current message continue the session's main task rather than switch topics?"},
                "session_depth": {
                    "type": "score",
                    "instructions": "How complex is the ongoing session task?",
                    "criteria": ["simple ongoing task", "moderate ongoing task", "complex ongoing task"]
                }
            }
        });
        // A2 Speculative Fan-Out：candidates hint（静态粗排 top-12 占位符画像，
        // 原生数组嵌入——JSON-in-string 形式曾触发上游 WAF 451）存在时，
        // 同一次请求附带 route_recommendation 问题：判定与举荐一次往返。
        if !candidates_hint.is_empty() {
            if let Ok(arr) = serde_json::from_str::<serde_json::Value>(candidates_hint) {
                payload["state"]["candidates"] = arr.clone();
                let mut criteria = serde_json::Map::new();
                if let Some(list) = arr.as_array() {
                    for x in list {
                        if let Some(id) = x.get("id").and_then(|i| i.as_str()) {
                            criteria.insert(
                                id.to_string(),
                                json!("candidate profile listed in state.candidates at this position"),
                            );
                        }
                    }
                }
                if !criteria.is_empty() {
                    payload["questions"]["route_recommendation"] = json!({
                        "type": "choice",
                        "instructions": "Given this task and the candidate model profiles in state.candidates, which single model should handle this request? Consider capability match, cost efficiency, reliability track record, context window fit, and quota availability. Reply with exactly one candidate id from the criteria list.",
                        "criteria": criteria,
                    });
                }
            }
        }
        payload
    }
}

fn bucket(v: usize, edges: &[usize]) -> usize {
    edges.iter().filter(|&&e| v > e).count()
}

fn head(s: &str, chars: usize) -> String {
    s.chars().take(chars).collect()
}

/// w1 OpenAIChat 协议：模型回复内容 → JSON（容忍 prose/代码围栏包裹）
fn extract_json(content: &str) -> Option<serde_json::Value> {
    let s = content.trim();
    match (s.find('{'), s.rfind('}')) {
        (Some(a), Some(b)) if a < b => serde_json::from_str(&s[a..=b]).ok(),
        _ => serde_json::from_str(s).ok(),
    }
}

pub(crate) fn parse_judgment_answers(a: &serde_json::Value) -> Option<JudgmentSet> {
    let domain = match a.get("task_domain")?.get("choice")?.as_str()? {
        "code" => Domain::Code,
        "math_logic" => Domain::MathLogic,
        "writing" => Domain::Writing,
        "factual_lookup" => Domain::Lookup,
        "data_analysis" => Domain::Data,
        "chitchat" => Domain::Chitchat,
        "agent_ops" => Domain::AgentOps,
        _ => Domain::Other,
    };
    let noul = |k: &str| a.get(k).and_then(|x| x.get("noul")).and_then(|x| x.as_f64()).unwrap_or(0.5) as f32;
    let score = |k: &str| a.get(k).and_then(|x| x.get("score")).and_then(|x| x.as_f64()).unwrap_or(1.0) as f32;
    // 锚定量表：L1-L4 级别映射到数值（消双峰——模型选级别而非凭空打分）
    let level = a.get("difficulty").and_then(|x| x.get("choice")).and_then(|x| x.as_str()).map(|s| s.to_string());
    let difficulty = match level.as_deref() {
        Some("l1") => 0.5,
        Some("l2") => 1.4,
        Some("l3") => 2.2,
        Some("l4") => 3.0,
        _ => score("difficulty").clamp(0.0, 3.0), // 兼容旧 score 型应答
    };
    Some(JudgmentSet {
        domain,
        domain_confidence: a.get("task_domain").and_then(|x| x.get("confidence")).and_then(|x| x.as_f64()).unwrap_or(0.5) as f32,
        difficulty,
        difficulty_confidence: a.get("difficulty").and_then(|x| x.get("confidence")).and_then(|x| x.as_f64()).unwrap_or(0.5) as f32,
        needs_vision: noul("needs_vision"),
        is_trivial: noul("is_trivial"),
        tool_heavy: noul("tool_heavy"),
        high_stakes: noul("high_stakes"),
        session_relevance: noul("session_relevance"),
        session_depth: score("session_depth").clamp(0.0, 3.0),
        route_recommendation: a.get("route_recommendation")
            .and_then(|x| x.get("choice"))
            .and_then(|x| x.as_str())
            .map(|s| s.to_string()),
        route_recommendation_confidence: a.get("route_recommendation")
            .and_then(|x| x.get("confidence"))
            .and_then(|x| x.as_f64())
            .unwrap_or(0.0) as f32,
        judge_source: "decision_model",
    })
}

/// v2 直接路由：决策模型看到候选全画像 + 飞轮经验，直接推荐模型
pub struct RouteAdvisor {
    backend: DecisionModelBackend,
}

impl RouteAdvisor {
    pub fn new(backend: DecisionModelBackend) -> Self {
        Self { backend }
    }

    pub fn recommend(
        &self,
        task_text: &str,
        candidates_json: &str,
        session_summary: &str,
        policy: &str,
    ) -> Option<(String, String, f32)> {
        let state = json!({
            "task": task_text.chars().take(600).collect::<String>(),
            "candidates": candidates_json.chars().take(6000).collect::<String>(),
            "session": session_summary.chars().take(300).collect::<String>(),
            "policy": policy,
        });
        let questions = json!({
            "route_recommendation": {
                "type": "choice",
                "instructions": {
                    "question": "Given this task and the candidate model profiles, which single model should handle this request?",
                    "note": "Consider capability match, cost efficiency, reliability track record, context window fit, and quota availability. Recommend exactly one candidate id.",
                },
            }
        });
        let consult = ConsultOnce {
            backend: self.backend.clone(),
            state,
            questions,
            timeout: Duration::from_secs(5),
        };
        let a = tokio::task::block_in_place(|| {
            tokio::runtime::Handle::try_current().ok().and_then(|h| {
                h.block_on(consult.run("route advisor consult failed"))
            })
        })?;
        let choice = a.pointer("/route_recommendation/choice").and_then(|c| c.as_str())?.to_string();
        let confidence = a
            .pointer("/route_recommendation/confidence")
            .and_then(|c| c.as_f64())
            .unwrap_or(0.5) as f32;
        let reasoning = a
            .pointer("/route_recommendation/reasoning")
            .and_then(|r| r.as_str())
            .unwrap_or("")
            .to_string();
        Some((choice, reasoning, confidence))
    }
}

impl evolve_core::types::RouteAdvisor for DecisionModelBackend {
    fn recommend(
        &self,
        task_summary: &str,
        candidates_json: &str,
        policy: &str,
    ) -> Option<(String, f32)> {
        self.recommend_route(task_summary, candidates_json, policy)
    }
}

/// 测试与部署共用的构造路径：from_env 读 env 后委托 from_parts，
/// 接入规则单点定义：protocol 判定 + endpoint 归一化 + model 必填约束
impl DecisionModelBackend {
    /// base 缺省 = TypeSafe Jev 云默认端点（此时 key 必填——云鉴权）；
    /// 显式 BASE_URL 时 key 一律可选（systemone → 原生契约，/judge → Laya 型，
    /// 其余 → OpenAI chat 兼容——此时 model 必填，判别规则与 endpoint 归一化
    /// 单点定义在此，from_env 与测试共用）
    pub(crate) fn from_parts(
        api_key: Option<&str>,
        base: Option<&str>,
        model: Option<&str>,
        protocol_hint: Option<&str>,
    ) -> Result<Self, ()> {
        let base = base
            .map(String::from)
            .unwrap_or_else(|| "https://api.typesafe.ai/v1/systemone".into());
        let model_set = model.map(String::from);
        let model = model_set.clone().unwrap_or_else(|| "jev-latest".into());
        let protocol = match protocol_hint.map(|p| p.to_lowercase()) {
            Some(ref p) if p.contains("openai") || p.contains("chat") => DecisionProtocol::OpenAIChat,
            Some(ref p) if p.contains("system") => DecisionProtocol::SystemOne,
            // URL 形态：systemone/system-one/system_one（去分隔符归一化）→ 原生契约；
            // /judge 结尾 → Laya 型；其余 → OpenAI chat 兼容
            _ if Self::is_systemone_style(&base) || Self::is_judge_style(&base) => DecisionProtocol::SystemOne,
            _ => DecisionProtocol::OpenAIChat,
        };
        let endpoint = if protocol == DecisionProtocol::SystemOne {
            base
        } else if base.ends_with("/chat/completions") {
            base
        } else {
            format!("{}/chat/completions", base.trim_end_matches('/'))
        };
        if endpoint.is_empty() {
            return Err(());
        }
        match protocol {
            DecisionProtocol::OpenAIChat if model_set.is_none() => return Err(()),
            DecisionProtocol::SystemOne if api_key.is_none()
                && Self::is_default_jev(&endpoint) =>
            {
                return Err(());
            }
            _ => {}
        }
        Ok(Self {
            client: reqwest::Client::new(),
            api_key: api_key.unwrap_or("").into(),
            model,
            endpoint,
            redact: true,
            protocol,
        })
    }

    fn is_default_jev(endpoint: &str) -> bool {
        endpoint == "https://api.typesafe.ai/v1/systemone"
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn extract_json_tolerates_fences_and_prose() {
        let fenced = "```json\n{\"answers\":{\"task_domain\":{\"choice\":\"code\"}}}\n```";
        assert_eq!(extract_json(fenced).unwrap()["answers"]["task_domain"]["choice"], "code");
        let prose = "Sure! {\"answers\":{\"difficulty\":{\"choice\":\"l2\"}}} hope that helps";
        assert_eq!(extract_json(prose).unwrap()["answers"]["difficulty"]["choice"], "l2");
        // 裸 answers（模型直接回 answers 对象）：consult 兜底 unwrap_or
        let bare = "{\"task_domain\":{\"choice\":\"code\"}}";
        assert_eq!(extract_json(bare).unwrap()["task_domain"]["choice"], "code");
    }

    #[test]
    fn protocol_auto_detection() {
        // TypeSafe 云默认（base 缺省 + key）→ 原生契约
        let b = DecisionModelBackend::from_parts(Some("key"), None, None, None).unwrap();
        assert_eq!(b.protocol, DecisionProtocol::SystemOne);
        assert!(b.endpoint.contains("systemone"));
        // 默认 Jev 云缺 key → 视为未配置
        assert!(DecisionModelBackend::from_parts(None, None, None, None).is_err());
        // 第三方 OpenAI 端点 → OpenAIChat，sign without key（ollama/vLLM 无鉴权）
        assert!(DecisionModelBackend::from_parts(None, Some("https://api.openai.com/v1"), None, None).is_err());
        let ok = DecisionModelBackend::from_parts(None, Some("https://api.openai.com/v1"), Some("gpt-6-mini"), None).unwrap();
        assert_eq!(ok.protocol, DecisionProtocol::OpenAIChat);
        assert_eq!(ok.endpoint, "https://api.openai.com/v1/chat/completions");
        assert!(ok.api_key.is_empty());
        let keyed = DecisionModelBackend::from_parts(Some("k"), Some("https://api.openai.com/v1"), Some("m"), None).unwrap();
        assert_eq!(keyed.api_key, "k");
        // 已带 /chat/completions 的 base 不重复追加
        let done = DecisionModelBackend::from_parts(None, Some("https://api.openai.com/v1/chat/completions"), Some("m"), None).unwrap();
        assert_eq!(done.endpoint, "https://api.openai.com/v1/chat/completions");
        // Laya 型判定端点（/judge 结尾）→ 原生契约、免 key、model 无关
        let native = DecisionModelBackend::from_parts(None, Some("http://127.0.0.1:8321/v1/judge"), None, None).unwrap();
        assert_eq!(native.protocol, DecisionProtocol::SystemOne);
        assert_eq!(native.endpoint, "http://127.0.0.1:8321/v1/judge");
        // is_judge_style 判别
        assert!(DecisionModelBackend::is_judge_style("http://x:1/v1/judge/"));
        assert!(!DecisionModelBackend::is_judge_style("http://x:1/v1/chat/completions"));
        // systemone 归一化判别（本地原生契约常见命名变体）
        assert!(DecisionModelBackend::is_systemone_style("http://127.0.0.1:8000/system_one"));
        assert!(DecisionModelBackend::is_systemone_style("http://127.0.0.1:8000/system-one/judge"));
        assert!(DecisionModelBackend::is_systemone_style("https://api.typesafe.ai/v1/systemone"));
        assert!(!DecisionModelBackend::is_systemone_style("http://127.0.0.1:8000/api/v1"));
        let sysone = DecisionModelBackend::from_parts(None, Some("http://127.0.0.1:8000/system_one"), None, None).unwrap();
        assert_eq!(sysone.protocol, DecisionProtocol::SystemOne);
        assert_eq!(sysone.endpoint, "http://127.0.0.1:8000/system_one");
    }

    #[test]
    fn answers_parse_both_shapes() {
        let full = serde_json::json!({"answers": {"task_domain": {"choice": "code", "confidence": 0.8}}});
        assert_eq!(parse_judgment_answers(full.get("answers").unwrap()).unwrap().domain, Domain::Code);
        let answers = serde_json::json!({"task_domain": {"choice": "code", "confidence": 0.8}});
        assert_eq!(parse_judgment_answers(&answers).unwrap().domain, Domain::Code);
        assert_eq!(parse_judgment_answers(&answers).unwrap().domain, Domain::Code);
    }
}

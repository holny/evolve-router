//! Laya decision backend via the local Python sidecar (scripts/laya_server.py).
//!
//! State is structure-first with short text heads only — laya-multilingual
//! carries 1024 context, so the state must stay tiny. Fully local: zero
//! per-call cost, no data leaves the machine. Any sidecar failure falls
//! back to the heuristic judge.

use async_trait::async_trait;
use evolve_core::types::*;
use serde_json::json;
use std::time::Duration;

pub const LAYA_STATE_HEAD_CHARS: usize = 160;

#[derive(Clone)]
pub struct LayaBackend {
    client: reqwest::Client,
    endpoint: String,
    api_key: String,
}

#[async_trait]
impl Judge for LayaBackend {
    fn judge(&self, features: &RequestFeatures, digest: &DigestSignals, _candidates_hint: &str) -> JudgmentSet {
        let result = tokio::task::block_in_place(|| {
            tokio::runtime::Handle::try_current().ok().map(|h| {
                h.block_on(async { self.consult_raw(features, digest, Duration::from_millis(1500)).await })
            })
        });
        let parsed = result.as_ref().and_then(|r| r.as_ref().ok()).and_then(crate::decision_backend::parse_judgment_answers);
        match parsed {
            Some(j) => j,
            None => {
                tracing::warn!("laya judgment failed, falling back to heuristic");
                evolve_core::heuristic::HeuristicJudge.judge(features, digest, _candidates_hint)
            }
        }
    }
}

impl LayaBackend {
    pub fn from_env() -> Option<Self> {
        // w2：LAYA_URL 显式优先；DECISION_BASE_URL 的 /judge 型判定端点同样
        // 可作为 sidecar 入口（两类协议族统一 = 环境变量）；最后回退默认本地端点
        let endpoint = std::env::var("LAYA_URL")
            .ok()
            .filter(|v| !v.trim().is_empty())
            .or_else(|| {
                let b = ["DECISION_BASE_URL", "DECISION_MODEL_ENDPOINT"].iter().find_map(|n| std::env::var(n).ok())?;
                if crate::decision_backend::DecisionModelBackend::is_judge_style(&b) { Some(b) } else { None }
            })
            .unwrap_or_else(|| "http://127.0.0.1:8321/v1/judge".into());
        Some(Self::with_endpoint(&endpoint, None))
    }

    pub fn with_endpoint(endpoint: &str, api_key: Option<&str>) -> Self {
        Self { client: reqwest::Client::new(), endpoint: endpoint.to_string(), api_key: api_key.unwrap_or("").to_string() }
    }

    /// w2 槽位链入口：返回 answers 对象（或全部响应形状兜底），失败带分类。
    /// 与 DecisionModelBackend::consult 同构——ChainJudge 用失败分类决定拉黑。
    pub async fn consult_raw(
        &self,
        features: &RequestFeatures,
        digest: &DigestSignals,
        timeout: Duration,
    ) -> Result<serde_json::Value, crate::decision_backend::SlotFail> {
        use crate::decision_backend::SlotFail;
        let payload = self.build_payload(features, digest);
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
        if !v.status().is_success() {
            let s = v.status();
            return Err(match s.as_u16() {
                401 | 403 => SlotFail::Auth,
                404 => SlotFail::NotFound,
                _ => SlotFail::Server,
            });
        }
        let body = v.json::<serde_json::Value>().await.map_err(|_| SlotFail::Parse)?;
        Ok(body.get("answers").cloned().unwrap_or(body))
    }

    fn build_payload(&self, features: &RequestFeatures, digest: &DigestSignals) -> serde_json::Value {
        let cjk = features.cjk_ratio;
        let lang_hint = if cjk > 0.15 { "multilingual" } else { "typed-decisions" };
        json!({
            "lang_hint": lang_hint,
            "state": {
                "task": {
                    "length_bucket": bucket(features.user_text_chars, &[30, 100, 400, 1500]),
                    "code_density": (features.code_density * 100.0).round() / 100.0,
                    "tool_count": features.tool_count,
                    "has_images": features.has_images,
                    "est_tokens": features.est_input_tokens,
                },
                "session": {
                    "first_task_head": head(&digest.first_user_text, LAYA_STATE_HEAD_CHARS / 2),
                    "current_head": head(&digest.last_user_text, LAYA_STATE_HEAD_CHARS),
                    "continues_topic": digest.has_deixis || digest.overlap_ratio > 0.15,
                    "topic_shift": digest.topic_shift_marker,
                    "tools_seen": digest.session_tools_seen.min(999),
                },
            }
        })
    }
}

fn bucket(v: usize, edges: &[usize]) -> usize {
    edges.iter().filter(|&&e| v > e).count()
}

fn head(s: &str, chars: usize) -> String {
    s.chars().take(chars).collect()
}

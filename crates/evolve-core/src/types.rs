use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "lowercase")]
pub enum Protocol {
    #[default]
    OpenAI,
    Anthropic,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "lowercase")]
pub enum Source {
    #[default]
    Builtin,
    User,
    Discovered,
    Remote,
}

impl Source {
    pub fn label(&self) -> &'static str {
        match self {
            Source::Builtin => "builtin",
            Source::User => "user",
            Source::Discovered => "discovered",
            Source::Remote => "remote",
        }
    }
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq)]
pub struct Cost {
    pub input: f32,
    pub output: f32,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq)]
#[serde(default)]
pub struct Tiers {
    pub reasoning: f32,
    pub coding: f32,
    pub vision: f32,
    pub agentic: f32,
}

impl Default for Tiers {
    fn default() -> Self {
        Self { reasoning: 0.5, coding: 0.5, vision: 0.0, agentic: 0.5 }
    }
}

/// One credential slot for a model (multi-key pool, decision record #12).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct KeySlot {
    pub label: String,
    pub value: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct ModelRecord {
    pub id: String,
    pub provider: String,
    pub protocol: Protocol,
    pub base_url: String,
    pub api_key_env: Option<String>,
    pub api_key: Option<String>,
    /// Resolved key pool: first = primary, rest rotate on quota/auth errors.
    pub keys: Vec<KeySlot>,
    pub upstream_model: String,
    pub context_window: Option<u64>,
    pub max_output: u64,
    pub cost: Option<Cost>,
    pub tiers: Tiers,
    /// True when the user explicitly declared tiers (benchmarks blend at 0.3)
    pub tiers_explicit: bool,
    /// 订阅套餐标志：plan 模型边际成本≈0，配额窗口内优先（用掉才值）
    pub plan: bool,
    /// 计价货币：CNY / USD（缺省按厂商推断）
    pub currency: String,
    pub speed_tier: f32,
    /// User bias weight (participates in scoring, never decisive).
    pub weight: Option<f32>,
    pub source: Source,
    pub source_note: Option<String>,
    /// 模型发布时间（epoch ms）——三级来源：provider /models 的 `created`
    /// → models.dev `release_date` → 模型 id 内嵌日期解析（见 recency.rs）
    #[serde(default)]
    pub release_ts: Option<u64>,
}

impl Default for ModelRecord {
    fn default() -> Self {
        Self {
            id: String::new(),
            provider: String::new(),
            protocol: Protocol::OpenAI,
            base_url: String::new(),
            api_key_env: None,
            api_key: None,
            keys: Vec::new(),
            upstream_model: String::new(),
            context_window: None,
            max_output: 8192,
            cost: None,
            tiers: Tiers::default(),
            tiers_explicit: false,
            plan: false,
            currency: String::new(),
            speed_tier: 0.6,
            weight: None,
            source: Source::User,
            source_note: None,
            release_ts: None,
        }
    }
}

impl ModelRecord {
    /// 免费模型（2026-10 用户需求：周期性识别免费模型并优先使用）。
    /// 判据 = 模型名（`upstream_model`，退 `id`）带独立的 `free` 段
    /// （`-free` / `_free` / `.free` / `/free` / `:free`）——opencode/zen 与
    /// OpenRouter 对免费模型的命名约定，models.dev 的 `opencode-go`/`opencode`
    /// 条目亦完全一致（两处目录里每个免费模型名都带 `-free`）。因此该信号精确、
    /// 无假阳性，且随每小时远端 /models 扫描 + 每日 models.dev 刷新自动跟进。
    ///
    /// **为何不用"目录价 0/0"**（曾试，实测误判）：models.dev 的 0/0 还表示
    /// "订阅套餐内含"，而 canon 全局兜底会取**其它 provider** 的条目——
    /// 实测 `opencode-go/qwen3.7-max`、`opencode-go/qwen3.6-plus` 被兜底匹配到
    /// `alibaba-token-plan`（0/0，套餐内含），一旦据此判"免费"就会去优先使用
    /// 实际要烧配额的模型。故只保留命名信号。
    pub fn is_free(&self) -> bool {
        let name = if self.upstream_model.is_empty() {
            self.id.to_lowercase()
        } else {
            self.upstream_model.to_lowercase()
        };
        name.split(['-', '/', '_', '.', ':']).any(|s| s == "free")
    }
}

/// 国内厂商按人民币计价；其余默认美元（可被配置显式覆盖）
pub fn infer_currency(provider: &str) -> &'static str {
    let p = provider.to_lowercase();
    for cn in ["zhipu", "deepseek", "minimax", "volces", "volcengine", "ark", "moonshot", "kimi",
               "qwen", "dashscope", "alibaba", "baidu", "ernie", "tencent", "hunyuan",
               "stepfun", "01ai", "baichuan", "sensetime", "doubao"] {
        if p.contains(cn) {
            return "CNY";
        }
    }
    "USD"
}

impl ModelRecord {
    pub fn has_credential(&self) -> bool {
        !self.keys.is_empty()
            || self.api_key.as_deref().map(|k| !k.is_empty()).unwrap_or(false)
            || self.base_url.contains("127.0.0.1")
            || self.base_url.contains("localhost")
    }

    /// Resolved key values in rotation order (api_keys_env first, then the
    /// legacy single api_key so old configs keep working).
    pub fn key_values(&self) -> Vec<String> {
        let mut v: Vec<String> = self.keys.iter().map(|k| k.value.clone()).collect();
        if v.is_empty()
            && let Some(k) = self.api_key.as_deref().filter(|k| !k.is_empty()) {
                v.push(k.to_string());
            }
        v
    }

    /// 命名先验（2026-10）：按模型名后缀语义对**冷启动**能力卡做小幅相对修正。
    /// 语义为业界惯例（Gemini Flash-Lite = 快省轻量、Pro/Max/Ultra = 旗舰、
    /// Thinking = 推理强化、-vl = 视觉）：轻量款 speed ↑ 能力略 ↓，旗舰款反之。
    /// `free` 段先剥离——商业变体与基模同能力（`minimax-m3.1-flash-free` ≡
    /// `minimax-m3.1-flash`）。只是一层浅先验：蒸馏卡与实测观测随时覆盖；
    /// 用户显式 tiers（tiers_explicit）不动。
    pub fn apply_naming_priors(&mut self) {
        if self.tiers_explicit {
            return;
        }
        let segs: Vec<String> = self
            .upstream_model
            .split(['-', '_', '.', '/', ':', '+'])
            .map(|s| s.to_ascii_lowercase())
            .filter(|s| s != "free")
            .collect();
        let has = |names: &[&str]| segs.iter().any(|s| names.contains(&s.as_str()));
        let mut t = self.tiers;
        // 轻量提速组：延迟低、成本低，但同系里能力偏弱（flash/turbo/lite/mini…）
        if has(&["flash", "flashx", "turbo", "lite", "mini", "small", "tiny", "nano",
                 "air", "instant", "highspeed", "lightning"]) {
            self.speed_tier = self.speed_tier.max(0.8);
            t.reasoning = (t.reasoning - 0.05).max(0.35);
            t.coding = (t.coding - 0.05).max(0.35);
        }
        // 旗舰组：能力上限更高、速度让位（pro/max/ultra/plus/super…）
        if has(&["pro", "max", "ultra", "plus", "super", "heavy", "large"]) {
            t.reasoning = (t.reasoning + 0.10).min(0.85);
            t.coding = (t.coding + 0.10).min(0.85);
            self.speed_tier = (self.speed_tier - 0.05).max(0.3);
        }
        // 推理强化组：think/thinking 系（o 系/DeepSeek-R 系惯例）
        if has(&["think", "thinking", "reasoner", "reasoning"]) {
            t.reasoning = (t.reasoning + 0.15).min(0.9);
        }
        // 视觉组（-vl = vision-language，Qwen-VL 系惯例）
        if has(&["vl", "vision", "omni", "multimodal"]) {
            t.vision = t.vision.max(0.7);
        }
        // 代码专精组
        if has(&["coder", "code", "codegen"]) {
            t.coding = (t.coding + 0.10).min(0.85);
        }
        self.tiers = t;
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Domain {
    Code,
    MathLogic,
    Writing,
    Lookup,
    Data,
    Chitchat,
    AgentOps,
    Other,
}

#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct RequestFeatures {
    pub est_input_tokens: u64,
    pub user_text_chars: usize,
    pub code_density: f32,
    pub tool_count: usize,
    pub tool_ratio: f32,
    pub has_images: bool,
    pub turn_count: usize,
    pub cjk_ratio: f32,
}

#[derive(Debug, Clone, Default)]
pub struct DigestSignals {
    pub first_user_text: String,
    pub last_user_text: String,
    pub overlap_ratio: f32,
    pub has_deixis: bool,
    pub topic_shift_marker: bool,
    pub session_tools_seen: usize,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct JudgmentSet {
    pub domain: Domain,
    pub domain_confidence: f32,
    pub difficulty: f32,
    pub difficulty_confidence: f32,
    pub needs_vision: f32,
    pub is_trivial: f32,
    pub tool_heavy: f32,
    pub high_stakes: f32,
    pub session_relevance: f32,
    pub session_depth: f32,
    /// Jev 的模型推荐（cookbook intent-routing 模式）——哪个候选模型 Jev 认为最合适
    pub route_recommendation: Option<String>,
    /// route_recommendation 的置信度（0-1）——高置信度时 engine 加分该模型
    pub route_recommendation_confidence: f32,
    /// 谁做的判定：decision_model / decision_model+rules / heuristic /
    /// sticky / explicit——进事件供面板展示判定来源
    pub judge_source: &'static str,
}

pub trait Judge: Send + Sync {
    /// candidates_hint: 静态粗排候选的占位符画像 JSON（A2 Fan-Out——
    /// 判定与路由推荐合并为一次调用）。空串 = 不问推荐。
    fn judge(&self, features: &RequestFeatures, digest: &DigestSignals, candidates_hint: &str) -> JudgmentSet;
}

/// 判定器给出的请求特征（0-1 置信度）——粘性复用时按字段名照搬，避免下标错位
#[derive(Debug, Clone, Copy, Default, PartialEq, Serialize, Deserialize)]
pub struct JudgmentFlags {
    pub is_trivial: f32,
    pub tool_heavy: f32,
    pub needs_vision: f32,
    pub high_stakes: f32,
}

impl JudgmentFlags {
    pub fn from_judgment(j: &JudgmentSet) -> Self {
        Self {
            is_trivial: j.is_trivial,
            tool_heavy: j.tool_heavy,
            needs_vision: j.needs_vision,
            high_stakes: j.high_stakes,
        }
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct StickyState {
    pub chosen: String,
    pub est_tokens_band: i32,
    pub turns_left: u32,
    pub tools_sig: u64,
    pub domain: Domain,
    /// 上次判定的有效难度：粘性只对低难度任务延续（L2 上限）
    pub difficulty: f32,
    /// 上次判定的请求特征：与 difficulty/domain 一起沿用（同一次工具流程的续跑，
    /// 特征不变）；不存的话占位判定只能是 0，面板/Web 会把"沿用"误读成"没有特征"。
    /// 用具名字段而不是 [f32;4]——按下标取值的错位是静默的（review 2026-10-15）
    pub flags: JudgmentFlags,
    /// 上次请求的估算输入 tokens：粘性量级保护上限（2 倍暴增/减半即断）
    pub est_tokens: u64,
}

#[derive(Debug, Clone, Serialize)]
pub struct FilteredOut {
    pub model: String,
    pub cause: String,
}

/// 逐候选评分分量（决策透明）：与公式权重一同随事件暴露
#[derive(Debug, Clone, Serialize)]
pub struct CandidateScore {
    pub model_id: String,
    pub score: f32,
    pub q: f32,
    pub s: f32,
    pub c: f32,
    pub r: f32,
    pub h: f32,
    /// 用户配置权重（clamp 后）
    pub uw: f32,
    /// 飞轮学习偏置
    pub bias: f32,
    /// 订阅方案预算压制系数（1.0=无压制，<1=预算软阈值降权）
    pub qp: f32,
}

#[derive(Debug, Clone, Serialize)]
pub struct Decision {
    pub id: String,
    pub chosen: String,
    pub upstream_model: String,
    pub chain: Vec<String>,
    pub reason: String,
    pub scores: BTreeMap<String, f32>,
    /// 逐候选评分分量（榜单质量/速度/成本/可靠/余量/用户权重/学习偏置/总分）
    /// ——决策透明的数据地基，随事件暴露给面板评分矩阵
    pub scored: Vec<CandidateScore>,
    /// 本次决策生效的公式权重（normalized [质量,速度,成本,可靠,余量]）
    pub weights: [f32; 5],
    pub judgment: JudgmentSet,
    pub filtered: Vec<FilteredOut>,
    /// 决策漏斗：目录总数→硬约束后→质量及格后→评分排序→选中
    pub funnel: [u32; 4],
    pub sticky: bool,
    pub est_input_tokens: u64,
    pub difficulty_eff: f32,
    /// 任务判定阶段耗时（决策模型 LLM 调用 + 规则融合）；粘性路径为 0
    pub judge_ms: u64,
    /// 探索门控实况（三条件 + 生效的置信门限 + 采样是否真的改写了首选）——
    /// 服务端算好，Web/面板只渲染；此前 Web 自己重算三条件、还把 config 里可配的
    /// `confidence_gate` 硬编码成 0.35（2026-10-15 review 指出的漂移）
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub explore_gate: Option<ExploreGate>,
}

/// 探索门控实况（Thompson Sampling 前的三道闸）
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq)]
pub struct ExploreGate {
    pub stakes_ok: bool,
    pub difficulty_ok: bool,
    pub confidence_ok: bool,
    /// 本次生效的置信门限（来自 policy.confidence_gate，不是硬编码）
    pub confidence_gate: f32,
    /// 门开着 + 采样后确实改写了首选（reason 里的 `[exploring]` 同源）
    pub explored: bool,
}

impl Decision {
    pub fn header_reason(&self) -> String {
        self.reason.chars().map(|c| if c.is_ascii() { c } else { ' ' }).collect()
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct PolicyWeights {
    pub quality: f32,
    pub speed: f32,
    pub cost: f32,
    pub stability: f32,
    pub headroom: f32,
}

impl Default for PolicyWeights {
    fn default() -> Self {
        Self::balanced()
    }
}

impl PolicyWeights {
    pub fn balanced() -> Self {
        Self { quality: 0.35, speed: 0.15, cost: 0.25, stability: 0.15, headroom: 0.10 }
    }
    pub fn cost() -> Self {
        Self { quality: 0.25, speed: 0.10, cost: 0.50, stability: 0.10, headroom: 0.05 }
    }
    pub fn quality() -> Self {
        Self { quality: 0.55, speed: 0.15, cost: 0.10, stability: 0.15, headroom: 0.05 }
    }

    pub fn normalized(&self) -> [f32; 5] {
        let sum = (self.quality + self.speed + self.cost + self.stability + self.headroom).max(1e-6);
        [
            self.quality / sum,
            self.speed / sum,
            self.cost / sum,
            self.stability / sum,
            self.headroom / sum,
        ]
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum PolicyProfile {
    Balanced,
    Cost,
    Quality,
}

impl PolicyProfile {
    pub fn weights(&self) -> PolicyWeights {
        match self {
            PolicyProfile::Balanced => PolicyWeights::balanced(),
            PolicyProfile::Cost => PolicyWeights::cost(),
            PolicyProfile::Quality => PolicyWeights::quality(),
        }
    }
    pub fn parse(s: &str) -> Option<Self> {
        match s.to_ascii_lowercase().as_str() {
            "balanced" => Some(Self::Balanced),
            "cost" | "cheap" => Some(Self::Cost),
            "quality" | "best" => Some(Self::Quality),
            _ => None,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum HealthKind {
    Ok,
    AuthFailed,
    PaymentRequired,
    QuotaExhausted,
    RateLimited,
    Unsupported,
    ContextOverflow,
    Transient,
}

impl HealthKind {
    pub fn label(&self) -> &'static str {
        match self {
            HealthKind::Ok => "ok",
            HealthKind::AuthFailed => "auth failed",
            HealthKind::PaymentRequired => "no credit",
            HealthKind::QuotaExhausted => "quota exhausted",
            HealthKind::RateLimited => "rate limited",
            HealthKind::Unsupported => "model unsupported",
            HealthKind::ContextOverflow => "context overflow",
            HealthKind::Transient => "transient error",
        }
    }
    pub fn default_cooldown_ms(&self) -> u64 {
        match self {
            HealthKind::Ok => 0,
            HealthKind::AuthFailed | HealthKind::PaymentRequired => 30 * 60 * 1000,
            HealthKind::Unsupported => 24 * 60 * 60 * 1000,
            HealthKind::ContextOverflow => 2 * 60 * 1000,
            HealthKind::QuotaExhausted => 5 * 60 * 1000,
            HealthKind::RateLimited => 60 * 1000,
            HealthKind::Transient => 30 * 1000,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct HealthEntry {
    pub kind: HealthKind,
    pub until_epoch_ms: Option<u64>,
    pub message: String,
    pub hits: u32,
    pub updated_at: u64,
}

impl HealthEntry {
    pub fn available(&self, now_epoch_ms: u64) -> bool {
        match self.kind {
            HealthKind::Ok => true,
            _ => self.until_epoch_ms.map(|t| now_epoch_ms >= t).unwrap_or(false),
        }
    }
    pub fn cooldown_remaining_ms(&self, now_epoch_ms: u64) -> Option<u64> {
        self.until_epoch_ms.map(|t| t.saturating_sub(now_epoch_ms))
    }
}

pub type HealthMap = std::collections::HashMap<String, HealthEntry>;

/// 模型级可用性：HealthMap 键是 `模型⟨sep⟩keyidx`（多 key 池）或裸模型 id。
/// 任一 key 可用即视为可用；该模型无任何记录视为可用。
pub fn model_available(map: &HealthMap, id: &str, now_epoch_ms: u64) -> bool {
    let mut seen = false;
    for (k, e) in map {
        if k.split('\u{1f}').next() == Some(id) {
            seen = true;
            if e.available(now_epoch_ms) {
                return true;
            }
        }
    }
    !seen
}

/// Observed per-model telemetry snapshot fed back into scoring (flywheel).
#[derive(Debug, Clone, Default, Serialize)]
pub struct ModelTelemetry {
    pub reliability: Option<f32>,
    pub speed_obs: Option<f32>,
    pub calibration: Option<f32>,
    /// Flywheel-learned bias: realized success vs catalog median, clamped.
    pub learned_bias: Option<f32>,
    /// Window lower-bound inference: largest prompt tokens a successful
    /// request actually accepted (for unknown-window models).
    pub max_accepted: Option<u64>,
    /// 请求样本数（含冷启动模型）——探索目标选择用
    pub samples: Option<u32>,
    /// 近 30 次请求实测可靠性（≥5 样本有效）——路由修正权重 30%
    pub rel_30: Option<f32>,
    /// 近 10 次请求实测可靠性（≥3 样本有效）——路由修正权重 20%
    pub rel_10: Option<f32>,
    /// 近期请求样本环（时间窗口聚合 + 趋势图）
    pub recent: Option<Vec<ReqSample>>,
}

impl ModelTelemetry {
    /// 观测到的 `真实 prompt_tokens / 估算 tokens` 中位数（仅成功样本）。
    /// 内容混合度不同，偏差方向也不同（纯中文估高、代码/JSON 估低）——用它把
    /// 硬过滤的 `need` 校到真实量级（2026-10 实测：大会话低估 1.5×）。
    pub fn token_ratio(&self) -> Option<f32> {
        let recent = self.recent.as_ref()?;
        let mut v: Vec<f32> = recent
            .iter()
            .filter(|s| s.ok && s.in_tok > 0)
            .filter_map(|s| {
                s.est_tok
                    .filter(|e| *e > 200)
                    .map(|e| s.in_tok as f32 / e as f32)
            })
            .collect();
        if v.is_empty() {
            return None;
        }
        v.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
        let n = v.len();
        let med = if n % 2 == 1 {
            v[n / 2]
        } else {
            (v[n / 2 - 1] + v[n / 2]) / 2.0
        };
        Some(med.clamp(0.5, 2.5))
    }
}

pub type TelemetrySnapshot = std::collections::HashMap<String, ModelTelemetry>;

/// 单次请求样本（模型动态窗口聚合 + 趋势图数据源）
#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
pub struct ReqSample {
    /// 请求完成时刻（epoch ms）
    pub ts: u64,
    pub ttft_ms: u64,
    pub total_ms: u64,
    pub in_tok: u64,
    pub cached_tok: u64,
    pub out_tok: u64,
    pub ok: bool,
    pub tools_total: u32,
    pub tools_ok: u32,
    /// 该请求的 token 估算值（用于按模型校准 `prompt_tokens/est` 的偏差）
    #[serde(default)]
    pub est_tok: Option<u64>,
}

/// Learned quota remaining (tokens) per model, best key across pool.
pub type QuotaView = std::collections::HashMap<String, u64>;

/// Gateway-side response quality analysis (L2 signals, no plugin needed).
#[derive(Debug, Clone, Default, Serialize)]
pub struct ResponseQuality {
    pub finish_reason: Option<String>,
    pub content_chars: usize,
    pub tool_calls_total: u32,
    pub tool_calls_valid_json: u32,
    pub tool_calls_known_name: u32,
    pub tool_calls_schema_ok: u32,
    /// finish_reason == length (truncated)
    pub truncated: bool,
    /// empty content with no tool calls and finish=stop
    pub empty_response: bool,
    /// repetition loop / refusal / garbage heuristics
    pub degenerate: bool,
    pub flags: Vec<String>,
}

impl ResponseQuality {
    pub fn syntactic_ok(&self) -> bool {
        self.tool_calls_total == 0
            || (self.tool_calls_valid_json == self.tool_calls_total
                && self.tool_calls_known_name == self.tool_calls_total)
    }
}

/// 双判官取严融合（决策记录 #24 扩展）：Jev 语义判定为主，heuristic 关键词
/// 为独立第二意见；逐字段取更保守值。专治 Jev 的 CJK 弱点——中文任务被
/// 误判为 Other/简单时，heuristic 把域和难度救回来。
pub fn fuse_judgments(jev: &JudgmentSet, heur: &JudgmentSet) -> JudgmentSet {
    let take_max = |a: f32, b: f32| a.max(b);
    // 域冲突：Jev 说 Other 而 heuristic 有明确域 -> 采信 heuristic（CJK 救援）
    let domain = if jev.domain == Domain::Other && heur.domain != Domain::Other {
        heur.domain
    } else if jev.domain_confidence >= heur.domain_confidence {
        jev.domain
    } else {
        heur.domain
    };
    JudgmentSet {
        domain,
        // 难度融合（2026-10 修正）：纯均值会让"中文被决策模型低估"拖低难度
        // （实证：heuristic 判 2.54、Jev 判 ~0 → 均值 1.21，难任务被降级）；
        // 纯取严又会钉死上限（双峰根因）。改为**均值与取高的折中**：
        // 0.25a + 0.25b + 0.5·max — 两官一致时=原值，分歧时向高侧靠但不顶格。
        difficulty: {
            let (a, b) = (jev.difficulty, heur.difficulty);
            let avg = (a + b) * 0.5;
            let hi = a.max(b);
            (avg + hi) * 0.5
        },
        domain_confidence: jev.domain_confidence.min(heur.domain_confidence),
        difficulty_confidence: jev.difficulty_confidence.min(heur.difficulty_confidence),
        needs_vision: take_max(jev.needs_vision, heur.needs_vision),
        is_trivial: jev.is_trivial.min(heur.is_trivial),
        tool_heavy: take_max(jev.tool_heavy, heur.tool_heavy),
        high_stakes: take_max(jev.high_stakes, heur.high_stakes),
        // 会话相关性取严：任一判官认为跑题即按跑题处理
        session_relevance: jev.session_relevance.min(heur.session_relevance),
        session_depth: take_max(jev.session_depth, heur.session_depth),
        // ⚠ 只有**主判官真的走了模型**才标 "decision_model+rules"：槽位链在冷却/失败时
        // 会回退成启发式（judge_source = "heuristic"），此时融合结果必须如实标 heuristic，
        // 否则界面判定不出"配了模型但当前没在用"（用户实测：超时后状态仍 ok）
        judge_source: if jev.judge_source == "heuristic" { "heuristic" } else { "decision_model+rules" },
    route_recommendation: jev.route_recommendation.clone(),
    route_recommendation_confidence: jev.route_recommendation_confidence,
    }
}

/// v2 直接路由顾问：决策模型看到候选全画像后直接推荐模型。
/// 与 Judge（语义判定器）不同——RouteAdvisor 的输出就是路由决定。
pub trait RouteAdvisor: Send + Sync {
    fn recommend(
        &self,
        task_summary: &str,
        candidates_json: &str,
        policy: &str,
    ) -> Option<(String, f32)>;
}

#[cfg(test)]
mod currency_tests {
    use super::infer_currency;

    #[test]
    fn cn_vendors_bill_in_cny() {
        for p in ["zhipuai-coding-plan", "volcengine-coding-plan", "volces",
                  "deepseek", "MiniMax", "moonshot"] {
            assert_eq!(infer_currency(p), "CNY", "{p} 应计人民币");
        }
    }

    #[test]
    fn international_vendors_bill_in_usd() {
        for p in ["opencode-go", "openai", "anthropic", "zen"] {
            assert_eq!(infer_currency(p), "USD", "{p} 应计美元");
        }
    }
}

#[cfg(test)]
mod fusion_tests {
    use super::*;

    fn mk(source: &'static str) -> JudgmentSet {
        JudgmentSet {
            domain: Domain::Code,
            domain_confidence: 1.0,
            difficulty: 1.0,
            difficulty_confidence: 1.0,
            needs_vision: 0.0,
            is_trivial: 0.0,
            tool_heavy: 0.0,
            high_stakes: 0.0,
            session_relevance: 0.0,
            session_depth: 0.0,
            route_recommendation: None,
            route_recommendation_confidence: 0.0,
            judge_source: source,
        }
    }

    /// 回归：槽位链回退成启发式时，双判官融合不得声称"模型判定"（否则界面无法识别降级）
    #[test]
    fn fusion_reports_when_the_model_was_not_used() {
        let heur = mk("heuristic");
        assert_eq!(
            fuse_judgments(&mk("heuristic"), &heur).judge_source,
            "heuristic",
            "model not used → must not claim model+rules"
        );
        assert_eq!(fuse_judgments(&mk("decision_model"), &heur).judge_source, "decision_model+rules");
    }
}

#[cfg(test)]
mod token_ratio_tests {
    use super::*;

    fn sample(in_tok: u64, est: u64, ok: bool) -> ReqSample {
        ReqSample {
            ts: 0, ttft_ms: 0, total_ms: 0,
            in_tok, cached_tok: 0, out_tok: 0,
            ok, tools_total: 0, tools_ok: 0, est_tok: Some(est),
        }
    }

    /// 只取成功样本的中位数；失败样本（上游报错时 prompt_tokens 可能缺失/异常）不计
    #[test]
    fn token_ratio_uses_successful_samples_median() {
        let t = ModelTelemetry {
            recent: Some(vec![
                sample(1500, 1000, true),
                sample(1600, 1000, true),
                sample(9000, 1000, false),
            ]),
            ..Default::default()
        };
        assert_eq!(t.token_ratio(), Some(1.55));
    }

    #[test]
    fn token_ratio_is_none_without_samples() {
        assert!(ModelTelemetry::default().token_ratio().is_none());
        let t = ModelTelemetry {
            recent: Some(vec![sample(1000, 0, true)]),
            ..Default::default()
        };
        assert!(t.token_ratio().is_none(), "zero/absent estimate must not produce a ratio");
    }
}

#[cfg(test)]
mod naming_prior_tests {
    use super::*;

    fn rec(name: &str) -> ModelRecord {
        ModelRecord {
            id: format!("p/{name}"),
            provider: "p".into(),
            base_url: "http://x/v1".into(),
            upstream_model: name.into(),
            ..Default::default()
        }
    }

    /// free 段先剥离：商业变体与基模同先验（2026-10 用户需求核心场景）
    #[test]
    fn free_variant_matches_base_model() {
        let mut f = rec("minimax-m3.1-flash-free");
        let mut b = rec("minimax-m3.1-flash");
        f.apply_naming_priors();
        b.apply_naming_priors();
        assert_eq!(f.tiers, b.tiers, "free is a commercial variant of the base model");
        assert_eq!(f.speed_tier, b.speed_tier);
    }

    /// 轻量组提速 + 略弱；旗舰组反之
    #[test]
    fn lightweight_speeds_up_flagship_strengthen() {
        let mut flash = rec("glm-5.3-flash");
        flash.apply_naming_priors();
        assert!(flash.speed_tier >= 0.8, "flash must look fast: {}", flash.speed_tier);
        assert!(flash.tiers.reasoning < 0.5 && flash.tiers.coding < 0.5, "lightweight slightly weaker");

        let mut big = rec("qwen3.8-max");
        big.apply_naming_priors();
        assert!(big.tiers.reasoning > 0.55 && big.tiers.coding > 0.55, "flagship stronger");
        assert!(big.speed_tier < 0.6, "flagship slightly slower: {}", big.speed_tier);

        // minimax 整段不会被拆成 mini+max 误判
        let mut mm = rec("MiniMax-M3");
        mm.apply_naming_priors();
        assert_eq!(mm.tiers, Tiers::default(), "brand name must not trigger priors");
    }

    /// think/vl/coder 各自强化对应维度
    #[test]
    fn domain_suffixes_boost_matching_dimension() {
        let mut think = rec("glm-5.3-think");
        think.apply_naming_priors();
        assert!(think.tiers.reasoning >= 0.65, "think → reasoning: {}", think.tiers.reasoning);

        let mut vl = rec("qwen3.8-vl");
        vl.apply_naming_priors();
        assert!(vl.tiers.vision >= 0.7, "vl → vision: {}", vl.tiers.vision);

        let mut coder = rec("kimi-k3-code");
        coder.apply_naming_priors();
        assert!(coder.tiers.coding >= 0.6, "code → coding: {}", coder.tiers.coding);
    }

    /// 用户显式 tiers 不动；无信号名字保持 default
    #[test]
    fn explicit_tiers_and_signal_free_names_untouched() {
        let mut u = rec("x-flash");
        u.tiers = Tiers { reasoning: 0.9, coding: 0.9, vision: 0.9, agentic: 0.9 };
        u.tiers_explicit = true;
        let before = u.tiers;
        u.apply_naming_priors();
        assert_eq!(u.tiers, before, "user-declared tiers are never touched");

        let mut plain = rec("step-5-preview");
        plain.apply_naming_priors();
        assert_eq!(plain.tiers, Tiers::default(), "preview carries no capability signal");
    }
}

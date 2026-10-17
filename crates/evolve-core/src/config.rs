use crate::types::*;
use crate::types::infer_currency as mr_infer_currency;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::path::Path;

#[derive(Debug, Clone, Deserialize, Default, Serialize)]
#[serde(default)]
pub struct ServerCfg {
    pub host: String,
    pub port: u16,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(default)]
pub struct PolicyCfg {
    pub default: String,
    pub weights: PolicyWeights,
    pub sticky_turns: u32,
    /// 失败降级链深度：链耗尽后从合格集合评分序继续补位（含链本身）
    pub fallback_depth: u32,
    pub confidence_gate: f32,
    /// ε-greedy exploration: share of requests that try alternative models
    /// (also drives sticky-escape re-evaluation). 0 disables both.
    pub explore_ratio: f32,
    /// 订阅方案预算软阈值（消耗占额度百分比，超过后模型渐进降权、粘性断开）
    pub plan_soft_pct: f32,
    /// S5 层级比例控制器容忍带（默认 0.05 = ±5pp）
    #[serde(default = "ratio_band_default")]
    pub ratio_band: f32,
}

impl Default for PolicyCfg {
    fn default() -> Self {
        Self {
            default: "balanced".into(),
            weights: PolicyWeights::balanced(),
            // 粘性的“度”（用户裁决：粘性保留，控制好度）：默认 2 轮短粘性——
            // 上游 prompt 缓存仍受益，但不会躺平；探索逃逸保证 ~explore_ratio
            // 的粘性命中被打破重评估。显式设 0 = 每请求完整决策
            sticky_turns: 2,
            fallback_depth: 6,
            confidence_gate: 0.35,
            explore_ratio: 0.05,
            plan_soft_pct: 60.0,
            ratio_band: ratio_band_default(),
        }
    }
}

#[derive(Debug, Clone, Deserialize, Default, Serialize)]
#[serde(default)]
pub struct DecisionCfg {
    pub backend: String,
    pub redact: bool,
    /// 判定调用超时（ms）。默认 3000；走远程中继/大模型判定端点时建议 8000-15000
    /// （实测 Bamboo 中继单次往返 1.7-2.6s，3s 会在拿到响应前就超时降级）
    pub judge_timeout_ms: Option<u64>,
    /// 决策模型槽位（JEV > LAYA > OPENAI 优先级，配一即用）。
    /// **决策模型配置的唯一来源**（2026-10-16 用户裁决：统一走 toml）——
    /// 此前只读环境变量，导致"开机自启的菜单栏 app 拉起的网关"拿不到 shell 里的
    /// key，面板显示"未配置决策模型"（env 只在终端启动时才存在）。
    pub slots: Vec<DecisionSlotCfg>,
}

/// 单个决策模型槽位（`[[decision.slots]]`）
#[derive(Debug, Clone, Deserialize, Default, Serialize)]
#[serde(default)]
pub struct DecisionSlotCfg {
    /// jev | laya | openai
    pub kind: String,
    /// 直接内联 key（建议配合 chmod 600 的配置文件）
    pub api_key: Option<String>,
    /// 间接引用：启动时读取该环境变量的值（key 值不落配置文件）
    pub api_key_env: Option<String>,
    /// 槽位端点（laya/openai 必填；jev 云端可省，本地部署填本地地址）
    pub base_url: Option<String>,
    /// 模型名（openai 槽必填）
    pub model: Option<String>,
    /// 协议显式指定：`system-one`（原生 {state,questions}→{answers}）/ `openai`（chat 兼容）/
    /// 留空 = 按 base_url 自动判别。原生契约的第三方端点（如 StartLux / Bamboo 的
    /// `/v1/decisions`）建议显式写死，别依赖路径猜测
    pub protocol: Option<String>,
    /// StartLux 系 AutoThink 的思考开关：`never` = 单遍作答（**最快**，路由判定推荐）、
    /// `auto` = 不确定时先想再答（准但慢，秒级）、`always` = 总是先想。留空用服务端默认
    pub think_mode: Option<String>,
}

/// 注意：这里保持**全 false**（代码内默认=关闭远端拉取，测试/CLI 构建不联网）。
/// 面向用户"默认开启"的是 `config/evo-router.default.toml`（首次运行据此生成
/// 用户配置）——默认接入的 provider 靠它把模型清单与免费模型带进来。
#[derive(Debug, Clone, Deserialize, Default, Serialize)]
#[serde(default)]
pub struct CatalogCfg {
    pub remote_fetch: bool,
    pub builtin_priors: bool,
    /// models.dev as reference-only metadata (never overrides user values)
    pub modelsdev_reference: bool,
}

#[derive(Debug, Clone, Deserialize, Default, Serialize)]
#[serde(default)]
pub struct QuotaCfg {
    pub learn_from_headers: bool,
}

/// External benchmark feed source (decision record #23).
#[derive(Debug, Clone, Deserialize, Default, Serialize)]
#[serde(default)]
pub struct BenchSourceCfg {
    pub name: String,
    pub url: String,
    pub format: String,
    pub interval_hours: Option<u64>,
    pub headers: Option<HashMap<String, String>>,
    pub alias_key: Option<String>,
    pub score_key: Option<String>,
    pub max_score: Option<f32>,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(default)]
pub struct BenchmarksCfg {
    pub enabled: bool,
    pub interval_hours: u64,
    pub sources: Vec<BenchSourceCfg>,
}

impl Default for BenchmarksCfg {
    fn default() -> Self {
        // enabled by default: the embedded curated seed applies cold-start
        // tiers offline; configured HTTP sources layer on top.
        // LMArena 镜像（用户盲测榜）为默认源——官方 lmarena.ai 国内不可达，
        // 镜像页内嵌同源榜单数据；失败保留旧值（fetch 失败不清 tiers）
        Self {
            enabled: true,
            interval_hours: 24,
            sources: vec![BenchSourceCfg {
                name: "lmarena".into(),
                url: "https://arena.atease.dev/text/".into(),
                format: "lmarena_mirror".into(),
                interval_hours: Some(24),
                headers: None,
                alias_key: None,
                score_key: None,
                max_score: None,
            }],
        }
    }
}

#[derive(Debug, Clone, Deserialize, Default, Serialize)]
#[serde(default)]
pub struct DataCfg {
    pub dir: String,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(default)]
pub struct DiscoveryCfg {
    pub agents: Vec<String>,
}

impl Default for DiscoveryCfg {
    fn default() -> Self {
        Self { agents: vec!["opencode".into()] }
    }
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(default)]
pub struct ModelEntry {
    pub id: String,
    pub provider: String,
    pub protocol: Protocol,
    pub base_url: String,
    pub api_key_env: Option<String>,
    pub api_keys_env: Option<Vec<String>>,
    /// 内联 key（等价于 api_key_env，但值直接写在配置里）——
    /// 适合"配置即唯一来源"的部署（如登录自启的网关拿不到 shell 环境变量）
    pub api_key: Option<String>,
    pub upstream_model: Option<String>,
    pub context_window: Option<u64>,
    pub max_output: u64,
    pub cost: Option<Cost>,
    pub tiers: Tiers,
    pub speed_tier: f32,
    pub weight: Option<f32>,
    /// 订阅套餐（边际成本≈0，配额窗口内优先）
    pub plan: Option<bool>,
    /// 计价货币（缺省按厂商推断：国内厂商 CNY，其余 USD）
    pub currency: Option<String>,
    pub source_note: Option<String>,
}

impl Default for ModelEntry {
    fn default() -> Self {
        let d = ModelRecord::default();
        Self {
            id: String::new(),
            provider: d.provider.clone(),
            protocol: d.protocol,
            base_url: d.base_url.clone(),
            api_key_env: None,
            api_keys_env: None,
            api_key: None,
            plan: None,
            currency: None,
            upstream_model: None,
            context_window: d.context_window,
            max_output: d.max_output,
            cost: None,
            tiers: d.tiers,
            speed_tier: d.speed_tier,
            weight: None,
            source_note: None,
        }
    }
}

/// agent/会话标识提取：自定义请求头 + 内置已知 agent 逻辑
#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(default)]
pub struct TelemetryCfg {
    /// 会话提取的自定义请求头（优先级最高）
    pub session_header: String,
    /// agent 提取的自定义请求头（优先级最高）
    pub agent_header: String,
    /// A3 响应摘要落盘（opt-in）：非流式成功响应的前 N 字符写入事件
    /// `resp_digest`，供 `backtest --judge-samples` 的 LLM-as-a-Judge 评分。
    /// 仅 decision.redact=false 时生效（redact 模式正文永不落盘）；默认关闭。
    #[serde(default)]
    pub response_digest: bool,
}

impl Default for TelemetryCfg {
    fn default() -> Self {
        Self {
            session_header: "x-ev-session".into(),
            agent_header: "x-ev-client".into(),
            response_digest: false,
        }
    }
}

/// 配置里的 provider 条目（`providers = [...]`）——**用户可定义的全部 provider 配置**
/// 都写在这里（导出时会带上来源标记，见 `origin` / `source_path`）。
/// 载入优先级：本表（origin=user）> overrides.json（面板/网页改动）> 自动发现（agent 配置）。
/// 去重仍由网关内部的 base_url + api_key 身份指纹完成，本表不参与去重逻辑。
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
#[serde(default)]
pub struct ProviderCfg {
    /// provider key（如 zhipuai-coding-plan）；留空则按 base_url 主机名推断
    pub key: String,
    pub base_url: String,
    /// 内联 key（本文件 0600）
    pub api_key: Option<String>,
    /// 或引用环境变量名
    pub api_key_env: Option<String>,
    /// openai | anthropic
    pub protocol: Option<String>,
    /// coding | agent | go | api
    pub plan_kind: Option<String>,
    /// lite / pro / max / go / go-plus …
    pub tier: Option<String>,
    pub docs_url: Option<String>,
    /// 软删（面板"禁用"）——保留条目但不出现在路由里
    pub disabled: Option<bool>,
    /// 来源：`user`（你自己写的）| `discovered`（从某个 Agent 配置读来的）
    pub origin: Option<String>,
    /// origin=discovered 时：**从哪个文件/环境读来的**（如 ~/.config/opencode/opencode.jsonc）
    pub source_path: Option<String>,
}

#[derive(Debug, Clone, Deserialize, Default, Serialize)]
#[serde(default)]
pub struct FileConfig {
    pub server: ServerCfg,
    pub policy: PolicyCfg,
    pub decision: DecisionCfg,
    pub catalog: CatalogCfg,
    pub quota: QuotaCfg,
    pub data: DataCfg,
    pub discovery: DiscoveryCfg,
    pub benchmarks: BenchmarksCfg,
    pub telemetry: TelemetryCfg,
    /// ⑦ 飞轮策略参数（backtest 裁决后写入；缺省 = 编译默认，删除段即回滚）
    #[serde(default)]
    pub strategy: StrategyCfg,
    pub models: Vec<ModelEntry>,
    /// 用户可定义的 provider 清单（导出时带 origin/source_path；见 ProviderCfg）
    pub providers: Vec<ProviderCfg>,
}

/// ⑦ 飞轮策略参数的配置形态。Default = 编译默认（v2）。
/// 与 evolve-memory::strategy::StrategyParams 字段一一对应——
/// config 层不能反向依赖 memory 层，故此处独立定义。
/// review#10 修复：字段级 serde default——只写部分字段不再整份解析失败
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StrategyCfg {
    #[serde(default)]
    pub version: u32,
    #[serde(default = "strategy_default_ok_weight")]
    pub ok_weight: f32,
    #[serde(default = "strategy_default_bias_gain")]
    pub bias_gain: f32,
}

fn strategy_default_ok_weight() -> f32 {
    0.7
}

fn strategy_default_bias_gain() -> f32 {
    0.6
}

impl Default for StrategyCfg {
    fn default() -> Self {
        Self { version: 2, ok_weight: 0.7, bias_gain: 0.6 }
    }
}

/// 把配置序列化成规范 JSONC 文本（带说明头）。迁移旧 TOML、导出 provider 清单共用。
pub fn to_jsonc(cfg: &FileConfig) -> String {
    let mut body = serde_json::to_string_pretty(cfg).unwrap_or_else(|_| "{}".into());
    // 内联 key 的字段能省则省，避免污染文件
    body = body.replace("\n  \"api_key\": null,", "");
    let header = r#"// EvolveRouter configuration (canonical: evo-router-config.jsonc)
// data dir: ~/.evo-router/  ·  docs: https://github.com/holny/evolve-router
//
// ── Reloading ───────────────────────────────────────────────────────────────
//   [decision] (+ decision.slots)      hot-reloaded within ~5 s (or POST /api/reload)
//   anything else                      restart the gateway to apply
//     menu-bar app: panel Stop then Start · LaunchAgent: launchctl kickstart -k gui/$UID/ai.evolve-router-gateway
//   GET /api/live -> "config": { reloaded_at_ms, restart_required, restart_reason, active_slots }
//
// ── Decision model (the ONLY place it is configured) ────────────────────────
//   priority jev > laya > openai; configure one and it is used; none -> heuristic judge.
//   "decision": {
//     "backend": "auto",
//     "slots": [
//       { "kind": "jev", "api_key": "<key>" },                                  // JEV cloud
//       { "kind": "jev", "base_url": "https://api.bamboonode.dev/v1/systemone",  // StartLux-Decision 27B
//         "model": "startlux-decision-27b-autothink", "api_key": "<bamboo-key>" },
//       { "kind": "laya", "base_url": "http://127.0.0.1:8321/v1/judge" },       // local sidecar
//       { "kind": "openai", "base_url": "http://127.0.0.1:11434/v1",            // ollama / vLLM
//         "model": "<model-id>" }
//     ]
//   }
//   Speed knobs (a remote 27B judge costs ~2.5-4 s per judged request; session
//   stickiness means only the first request of a tool loop pays it):
//     "judge_timeout_ms": 12000        // default 3000; raise it for internet relays
//     per slot: "think_mode": "never"  // StartLux AutoThink: one pass, fastest
//
// ── Credentials: where providers come from ──────────────────────────────────
//   "providers": entries below are the explicit list. "origin" says where each came from:
//     "user"        you (or the dashboard/panel) defined it
//     "discovered"  read from an agent's own config - see "source_path" for the file
//                   (e.g. "opencode @ ~/.config/opencode/opencode.jsonc")
//     "builtin"     shipped catalog default
//   Rules: an entry here wins over discovery for the same account; deduplication
//   inside the gateway keys on base_url + api_key, so the same account listed twice
//   (config + agent config) is still one provider. Keys written here live on disk:
//   keep this file at mode 600. Export the current merged list with
//   POST /api/config/export  (or `evo-router config export`).
//   "models" ([[models]]) is the per-model companion: base_url + api_key_env/api_key.
//
// ── Editing by hand is fine ─────────────────────────────────────────────────
//   Unknown keys are ignored; every section is optional; the gateway writes this
//   file back on migration and on /api/config/export (comments here are preserved
//   only in the header - put your notes in this block).

"#;
    header.to_string() + &body + "\n"
}

/// 规范配置文件路径（`~/.evo-router/evo-router-config.jsonc`）。
/// 首次运行（无任何配置文件）时以嵌入默认配置生成——网关的 provider 清单、
/// 默认接入与面板/网页的"填 key"提示都依赖一个真实存在的配置文件。
pub fn canonical_config_path() -> Option<std::path::PathBuf> {
    std::env::var("HOME")
        .ok()
        .map(|h| Path::new(&h).join(".evo-router").join(DEFAULT_CONFIG_NAME))
}

/// 写出配置文件（规范 JSONC + 0600 + 自动建父目录）。导出 provider / 迁移 / 播种共用。
pub fn write_config(path: &Path, cfg: &FileConfig) -> std::io::Result<()> {
    ensure_parent(path);
    std::fs::write(path, to_jsonc(cfg))?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let _ = std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600));
    }
    Ok(())
}

/// 一次性迁移：读到旧名配置 → 写出规范名（旧文件保留为 .migrated）
pub fn migrate_to_canonical(cfg: &FileConfig, found: &Path) -> Option<std::path::PathBuf> {    let dir = found.parent()?;
    let target = dir.join(DEFAULT_CONFIG_NAME);
    if found == target || target.exists() {
        return None;
    }
    if std::fs::write(&target, to_jsonc(cfg)).is_err() {
        return None;
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let _ = std::fs::set_permissions(&target, std::fs::Permissions::from_mode(0o600));
    }
    let backup = found.with_extension(format!(
        "{}migrated",
        found.extension().map(|e| format!("{}.", e.to_string_lossy())).unwrap_or_default()
    ));
    let _ = std::fs::rename(found, &backup);
    tracing::info!(
        from = %found.display(),
        to = %target.display(),
        kept = %backup.display(),
        "config migrated to the canonical evo-router-config.jsonc"
    );
    Some(target)
}

/// 剥掉 JSONC 注释（`//` 行注释与 `/* */` 块注释），**字符串内的注释符不动**。
/// 目的：JSON 配置也能像 TOML 一样写说明，解析前清理即可。
pub fn strip_json_comments(text: &str) -> String {
    let b: Vec<char> = text.chars().collect();
    let mut out = String::with_capacity(text.len());
    let (mut i, mut in_str, mut esc) = (0usize, false, false);
    while i < b.len() {
        let c = b[i];
        if in_str {
            out.push(c);
            if esc {
                esc = false;
            } else if c == '\\' {
                esc = true;
            } else if c == '"' {
                in_str = false;
            }
            i += 1;
            continue;
        }
        match c {
            '"' => {
                in_str = true;
                out.push(c);
                i += 1;
            }
            '/' if i + 1 < b.len() && b[i + 1] == '/' => {
                while i < b.len() && b[i] != '\n' {
                    i += 1;
                }
            }
            '/' if i + 1 < b.len() && b[i + 1] == '*' => {
                i += 2;
                while i + 1 < b.len() && !(b[i] == '*' && b[i + 1] == '/') {
                    i += 1;
                }
                i = (i + 2).min(b.len());
            }
            _ => {
                out.push(c);
                i += 1;
            }
        }
    }
    out
}

impl FileConfig {
    /// 解析 TOML（内部错误类型保持 toml::de::Error，兼容既有调用）
    pub fn parse(text: &str) -> Result<Self, toml::de::Error> {
        toml::from_str(text)
    }

    /// 按扩展名解析：`.json` / `.jsonc` 走 JSON（jsonc 允许 `//` 与 `/* */` 注释），
    /// 其余（含内置默认）走 TOML。JSON 与 TOML 是**同一套结构**（serde 同一份定义），
    /// 所以两种文件可互换；注释在 JSON 里也能写（jsonc），只是解析前被剥掉。
    pub fn parse_text(text: &str, path: Option<&Path>) -> anyhow::Result<Self> {
        let is_json = path
            .and_then(|p| p.extension().map(|e| e.to_string_lossy().to_lowercase()))
            .map(|e| e == "json" || e == "jsonc")
            .unwrap_or(false);
        if !is_json {
            return Ok(Self::parse(text)?);
        }
        let cleaned = strip_json_comments(text);
        Ok(serde_json::from_str(&cleaned)?)
    }

    pub fn from_paths(candidates: &[std::path::PathBuf], embedded_default: &str) -> anyhow::Result<(Self, Option<std::path::PathBuf>)> {
        for p in candidates {
            if p.is_file() {
                let text = std::fs::read_to_string(p)?;
                let cfg = Self::parse_text(&text, Some(p))?;
                // 旧名（含 TOML）→ 迁移成规范 JSONC；失败不影响启动
                if let Some(new_path) = migrate_to_canonical(&cfg, p) {
                    return Ok((cfg, Some(new_path)));
                }
                return Ok((cfg, Some(p.clone())));
            }
        }
        Ok((Self::parse(embedded_default)?, None))
    }

    pub fn resolve_model(&self, entry: &ModelEntry) -> ModelRecord {
        // "provider/model" 形式的目录 id：上游真实模型名取 '/' 后段
        // （覆盖 zhipuai-coding-plan/glm-5.3-flash -> glm-5.3-flash）
        let upstream_model = entry
            .upstream_model
            .clone()
            .unwrap_or_else(|| {
                entry
                    .id
                    .rsplit('/')
                    .next()
                    .unwrap_or(&entry.id)
                    .to_string()
            });
        let mut keys: Vec<KeySlot> = Vec::new();
        // 内联 key 优先（显式写在配置文件里 = 用户意图最强）
        if let Some(k) = entry.api_key.as_ref().filter(|k| !k.trim().is_empty()) {
            keys.push(KeySlot { label: "config:api_key".into(), value: k.trim().to_string() });
        }
        if keys.is_empty()
            && let Some(envs) = &entry.api_keys_env {
            for (i, env) in envs.iter().enumerate() {
                if let Ok(v) = std::env::var(env)
                    && !v.is_empty() {
                        keys.push(KeySlot { label: format!("{env}[{i}]"), value: v });
                    }
            }
        }
        if keys.is_empty()
            && let Some(env) = &entry.api_key_env
                && let Ok(v) = std::env::var(env)
                    && !v.is_empty() {
                        keys.push(KeySlot { label: env.clone(), value: v });
                    }
        let api_key = entry
            .api_key
            .clone()
            .filter(|k| !k.trim().is_empty())
            .or_else(|| entry.api_key_env.as_ref().map(|env| std::env::var(env).unwrap_or_default()));
        // provider 为空时从 id 前缀推断（"zhipuai-coding-plan/glm-5.3" → "zhipuai-coding-plan"）
        let provider = if entry.provider.is_empty() {
            entry.id.split('/').next().unwrap_or(&entry.id).to_string()
        } else {
            entry.provider.clone()
        };
        ModelRecord {
            id: entry.id.clone(),
            provider,
            protocol: entry.protocol,
            base_url: entry.base_url.trim_end_matches('/').to_string(),
            api_key_env: entry.api_key_env.clone(),
            api_key,
            keys,
            upstream_model,
            context_window: entry.context_window,
            max_output: entry.max_output,
            cost: entry.cost,
            tiers: entry.tiers,
            tiers_explicit: entry.tiers != Tiers::default(),
            plan: entry.plan.unwrap_or(false),
            currency: entry.currency.clone().filter(|c| !c.is_empty())
                .unwrap_or_else(|| mr_infer_currency(&entry.provider).to_string()),
            speed_tier: entry.speed_tier,
            weight: entry.weight,
            source: Source::User,
            source_note: entry.source_note.clone(),
            release_ts: None,
        }
    }

    pub fn model_records(&self) -> Vec<ModelRecord> {
        self.models.iter().map(|m| self.resolve_model(m)).collect()
    }

    /// 仅供远端 `/models` 拉取分组用的**端点占位记录**：`providers` 段里只声明了
    /// base_url、没有任何 `[[models]]` 的 provider（如默认接入的 openrouter）也必须
    /// 被拉取模型清单——否则它的 `/models` 永远不会被访问，目录里就看不到它的
    /// 模型与免费模型。占位记录只在 `provider_groups` 里用于分组，**不进目录**。
    pub fn endpoint_placeholders(&self) -> Vec<ModelRecord> {
        self.providers
            .iter()
            .filter(|p| !p.base_url.trim().is_empty() && p.disabled != Some(true))
            .map(|p| ModelRecord {
                id: format!("{}/__endpoint__", p.key),
                provider: p.key.clone(),
                base_url: p.base_url.trim_end_matches('/').to_string(),
                api_key: p.api_key.clone(),
                api_key_env: p.api_key_env.clone(),
                upstream_model: "__endpoint__".into(),
                ..Default::default()
            })
            .collect()
    }
}

/// 数据目录与配置文件名（对外命名铁律：不出现裸 `evolve`，用 evo-router / EvolveRouter / ER）
pub const DEFAULT_DATA_DIR: &str = "~/.evo-router";
pub const DEFAULT_CONFIG_NAME: &str = "evo-router-config.jsonc";
/// 规范配置名的纯 JSON 变体（无注释需求/机器生成时用）
pub const DEFAULT_CONFIG_NAME_JSON: &str = "evo-router-config.json";
/// 旧名（只读兼容 + 一次性迁移到规范名）
pub const LEGACY_CONFIG_NAMES: [&str; 3] = ["evo-router.toml", "evo-router.jsonc", "evo-router.json"];
/// 改名前的旧默认值（仅用于兼容与一次性迁移）
pub const LEGACY_DATA_DIR: &str = "~/.evolve";

/// macOS TCC 保护目录（「文稿」「桌面」「下载」及 iCloud Drive）。
///
/// 从这些目录下启动网关时，去 stat **相对 cwd** 的候选配置会让系统弹出
/// "EvolveRouter.app 想访问文稿文件夹中的文件"——只是探测文件名而已。
/// 判断纯路径、不触发任何文件系统访问（避免"为了判断而 stat"再触发弹窗）。
pub fn is_tcc_protected_dir(dir: &Path) -> bool {
    let s = dir.to_string_lossy();
    let s = s.trim_end_matches('/');
    let suffixes = [
        "/Documents",
        "/Desktop",
        "/Downloads",
        "/Library/Mobile Documents",
    ];
    // 目录本身或其任意父级落在保护目录内
    let mut cur = s.to_string();
    loop {
        if suffixes.iter().any(|x| cur.ends_with(x)) {
            return true;
        }
        match cur.rfind('/') {
            Some(0) | None => return false,
            Some(i) => cur.truncate(i),
        }
    }
}

/// 是否跳过 **cwd 相对** 的配置候选（cwd 落在 TCC 保护目录内 → 探测会弹窗）
fn skip_cwd_candidates() -> bool {
    std::env::current_dir()
        .map(|d| is_tcc_protected_dir(&d))
        .unwrap_or(false)
}

pub fn default_config_paths() -> Vec<std::path::PathBuf> {
    // 规范名优先（JSONC = 带注释的 JSON，注释即文档）；旧名随后（升级兼容，读到即迁移）。
    // cwd 相对候选**在 TCC 保护目录内一律跳过**：那会让 macOS 反复弹
    // "想访问文稿文件夹"（实测：从 ~/Documents/... 下启动网关就弹）。
    // 需要在别处用配置请显式 `--config <path>`。
    let mut paths: Vec<std::path::PathBuf> = Vec::new();
    if !skip_cwd_candidates() {
        paths.push(Path::new(DEFAULT_CONFIG_NAME).to_path_buf());
        paths.push(Path::new(DEFAULT_CONFIG_NAME_JSON).to_path_buf());
        for n in LEGACY_CONFIG_NAMES {
            paths.push(Path::new(n).to_path_buf());
        }
    }
    if let Ok(home) = std::env::var("HOME") {
        let dir = Path::new(&home).join(".evo-router");
        paths.push(dir.join(DEFAULT_CONFIG_NAME));
        paths.push(dir.join(DEFAULT_CONFIG_NAME_JSON));
        for n in LEGACY_CONFIG_NAMES {
            paths.push(dir.join(n));
        }
        // 改名前的旧位置：新名优先，旧名兜底——升级用户配置不静默丢失
        // 新目录里的旧文件名（迁移后可能残留）→ 兜底读取
        paths.push(dir.join("evolve.toml"));
        let legacy = Path::new(&home).join(".evolve");
        paths.push(legacy.join("evo-router.toml"));
        paths.push(legacy.join("evolve.toml"));
        // 更早的 cwd 位置（同样受 TCC 守卫约束）
        if !skip_cwd_candidates() {
            paths.push(Path::new("evolve.toml").to_path_buf());
        }
    }
    paths
}

/// 解析并（必要时）迁移数据目录。
///
/// - 配置里是**旧默认值**（`~/.evolve`）或未设置 → 目标为 `~/.evo-router`；
///   目标不存在而旧目录存在 → 一次性 `rename` 迁移（失败则退回旧目录并告警，
///   绝不丢数据）；
/// - 用户显式指定的其他路径 → 原样使用（尊重用户选择）。
pub fn resolve_data_dir(configured: &str) -> std::path::PathBuf {
    let expand = |d: &str| -> std::path::PathBuf {
        if let Some(rest) = d.strip_prefix("~/") {
            if let Ok(home) = std::env::var("HOME") {
                return Path::new(&home).join(rest);
            }
        }
        Path::new(d).to_path_buf()
    };
    let t = configured.trim();
    let legacy = expand(LEGACY_DATA_DIR);
    let target = expand(DEFAULT_DATA_DIR);
    // 只对"默认目录名"（旧默认 / 新默认 / 未设置）做核对与迁移；
    // 用户显式指定的其他目录一律尊重、不碰
    if !t.is_empty() && t != LEGACY_DATA_DIR && t != DEFAULT_DATA_DIR {
        return expand(t);
    }
    // 两个目录都在时，以 events.jsonl 心跳判断谁是活的：
    // 若旧目录才是活的（新目录是历次改名留下的空壳），把空壳挪到 .stale-<ts> 后继续迁移
    if target.exists() && legacy.exists() && legacy_is_live(&target, &legacy) {
        let ts = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_secs())
            .unwrap_or(0);
        let aside = target.with_file_name(format!(".evo-router.stale-{ts}"));
        match std::fs::rename(&target, &aside) {
            Ok(()) => tracing::warn!(
                stale = %aside.display(),
                "stale data dir moved aside before migration"
            ),
            Err(e) => {
                tracing::warn!(error = %e, "could not move the stale data dir aside; staying on it");
                return target;
            }
        }
    }
    if !target.exists() && legacy.exists() {
        match std::fs::rename(&legacy, &target) {
            Ok(()) => tracing::info!(
                from = %legacy.display(),
                to = %target.display(),
                "data dir migrated to the evo-router naming"
            ),
            Err(e) => {
                // 跨设备/权限问题：退回旧目录继续跑，不阻塞启动、不动用户数据
                tracing::warn!(
                    error = %e,
                    legacy = %legacy.display(),
                    "data dir migration failed, staying on the legacy directory"
                );
                return legacy;
            }
        }
    }
    let resolved = if target.exists() {
        target
    } else if legacy.exists() {
        legacy
    } else {
        target
    };
    tidy_config_name(&resolved);
    tidy_layout(&resolved);
    resolved
}

// ── 数据目录布局（2026-10-16 用户裁决：根目录只留配置，其余归类）───────────────
///
/// ```text
/// ~/.evo-router/
/// ├── evo-router.toml        # 配置（唯一需要手动编辑的文件）
/// ├── events/                # 追加型事件日志（回放/统计的数据底座）
/// │   ├── events.jsonl
/// │   └── events.jsonl.1     # 32MB 轮转
/// ├── state/                 # 机器写的状态与缓存
/// │   ├── snapshot.json        # 飞轮快照（含小时桶）
/// │   ├── distilled.json       # 能力卡蒸馏
/// │   ├── overrides.json       # 面板/网页调控覆盖
/// │   ├── benchmarks.json      # 榜单缓存
/// │   ├── modelsdev.json       # models.dev 参考缓存
/// │   └── remote-models.json   # /v1/models 扫描缓存
/// └── logs/
///     ├── gateway.log          # LaunchAgent 模式
///     └── gateway-child.log    # 菜单栏 app 子进程
/// ```
pub const EVENTS_DIR: &str = "events";
pub const STATE_DIR: &str = "state";
pub const LOGS_DIR: &str = "logs";
pub const EVENTS_NAME: &str = "events.jsonl";

fn expand_home(dir: &str) -> std::path::PathBuf {
    if let Some(rest) = dir.strip_prefix("~/") {
        if let Ok(home) = std::env::var("HOME") {
            return std::path::Path::new(&home).join(rest);
        }
    }
    std::path::Path::new(dir).to_path_buf()
}

/// `~/.evo-router/events/events.jsonl`（`prev=true` 取轮转后的 `.1`）
pub fn events_file(data_dir: &str, prev: bool) -> std::path::PathBuf {
    let root = expand_home(data_dir);
    let name = if prev { format!("{EVENTS_NAME}.1") } else { EVENTS_NAME.to_string() };
    root.join(EVENTS_DIR).join(name)
}

/// `~/.evo-router/state/<name>`
pub fn state_file(data_dir: &str, name: &str) -> std::path::PathBuf {
    expand_home(data_dir).join(STATE_DIR).join(name)
}

/// `~/.evo-router/logs/<name>`
pub fn log_file(data_dir: &str, name: &str) -> std::path::PathBuf {
    expand_home(data_dir).join(LOGS_DIR).join(name)
}

/// 写文件前确保父目录存在（归类后 state/ 等子目录可能是新建的）
pub fn ensure_parent(path: &Path) {
    if let Some(parent) = path.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
}

/// 布局整理（幂等）：把历史遗留**散落在根目录**的文件挪进 events/state/logs。
/// 目标已存在时不覆盖（宁可留一份也不静默丢数据）；失败只告警不阻塞启动。
pub fn tidy_layout(data_dir: &Path) {
    // 归类后的三个子目录先建好（幂等）：写入方直接落文件即可
    for sub in [EVENTS_DIR, STATE_DIR, LOGS_DIR] {
        let _ = std::fs::create_dir_all(data_dir.join(sub));
    }
    let moves: [(&str, &str); 11] = [
        ("events.jsonl", EVENTS_DIR),
        ("events.jsonl.1", EVENTS_DIR),
        ("snapshot.json", STATE_DIR),
        ("distilled.json", STATE_DIR),
        ("overrides.json", STATE_DIR),
        ("benchmarks.json", STATE_DIR),
        ("modelsdev.json", STATE_DIR),
        ("remote-models.json", STATE_DIR),
        ("gateway.log", LOGS_DIR),
        ("gateway-child.log", LOGS_DIR),
        ("gateway.log.1", LOGS_DIR),
    ];
    for (name, sub) in moves {
        let from = data_dir.join(name);
        if !from.is_file() {
            continue;
        }
        let to_dir = data_dir.join(sub);
        if std::fs::create_dir_all(&to_dir).is_err() {
            continue;
        }
        let mut to = to_dir.join(name);
        if to.exists() {
            // 目标已有一份（例如旧根目录里那个 35MB 的历史日志 vs 正在写的同名日志）：
            // 不覆盖、也不留在根目录——改名带时间戳挪过去，两份都保住
            let ts = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_secs())
                .unwrap_or(0);
            to = to_dir.join(format!("{name}.old-{ts}"));
        }
        match std::fs::rename(&from, &to) {
            Ok(()) => tracing::info!(file = name, into = sub, "data dir file moved into its category"),
            Err(e) => tracing::warn!(error = %e, file = name, "data dir tidy-up failed (left in place)"),
        }
    }
}

/// events.jsonl 的修改时间 = 该目录的"心跳"（不存在 → None）
fn heartbeat(dir: &Path) -> Option<std::time::SystemTime> {
    // 归类后的位置优先，兼容尚未整理的历史根目录文件
    [dir.join(EVENTS_DIR).join(EVENTS_NAME), dir.join(EVENTS_NAME)]
        .iter()
        .find_map(|p| std::fs::metadata(p).and_then(|m| m.modified()).ok())
}

/// 旧目录是否才是"活的那个"：
/// - 旧有心跳、新无心跳 → 旧活；
/// - 两者都有 → 比较心跳新旧；
/// - 旧无心跳 → 新目录（哪怕是空壳）优先，避免把用户的新目录搬走
fn legacy_is_live(target: &Path, legacy: &Path) -> bool {
    match (heartbeat(legacy), heartbeat(target)) {
        (Some(l), Some(t)) => l > t,
        (Some(_), None) => true,
        (None, _) => {
            // 旧目录没事件文件：只在新目录完全是空壳时才迁移
            std::fs::read_dir(target).map(|mut d| d.next().is_none()).unwrap_or(true)
        }
    }
}

/// 目录内的配置文件名规范化 → 规范 JSONC（幂等）。
/// ⚠ 必须**转换**而不是改名：旧版是 TOML，直接改名成 `.jsonc` 会让解析器按 JSON 读而失败
/// （等于配置静默丢失、回退内置默认）。这里统一走 `migrate_to_canonical`（解析 + 序列化）。
fn tidy_config_name(dir: &Path) {
    let canonical = dir.join(DEFAULT_CONFIG_NAME);
    if canonical.exists() {
        return;
    }
    let mut legacy: Vec<std::path::PathBuf> = vec![dir.join("evolve.toml"), dir.join("evo-router.toml")];
    for n in LEGACY_CONFIG_NAMES {
        legacy.push(dir.join(n));
    }
    for old in legacy {
        if !old.is_file() {
            continue;
        }
        let Ok(text) = std::fs::read_to_string(&old) else { continue };
        let Ok(cfg) = FileConfig::parse_text(&text, Some(&old)) else {
            tracing::warn!(path = %old.display(), "legacy config unparsable, left as-is");
            continue;
        };
        if migrate_to_canonical(&cfg, &old).is_some() {
            return;
        }
    }
}

fn ratio_band_default() -> f32 { 0.05 }

#[cfg(test)]
mod data_dir_tests {
    use super::*;

    /// HOME 是进程级全局变量：并行测试会互相污染（踩到过），统一串行化
    static ENV_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

    /// 端点占位记录：只有 base_url 的 provider 也要被远端 /models 拉取覆盖
    #[test]
    fn endpoint_placeholders_cover_keyless_providers() {
        let mut cfg = FileConfig::default();
        cfg.providers.push(ProviderCfg {
            key: "openrouter".into(),
            base_url: "https://openrouter.ai/api/v1/".into(),
            api_key: Some("k".into()),
            ..Default::default()
        });
        cfg.providers.push(ProviderCfg {
            key: "off".into(),
            base_url: "https://example.test/v1".into(),
            disabled: Some(true),
            ..Default::default()
        });
        let ph = cfg.endpoint_placeholders();
        assert_eq!(ph.len(), 1, "disabled providers are not fetched: {ph:?}");
        assert_eq!(ph[0].provider, "openrouter");
        assert_eq!(ph[0].base_url, "https://openrouter.ai/api/v1", "trailing slash trimmed for grouping");
        assert_eq!(ph[0].key_values(), vec!["k".to_string()]);
        // 占位记录有独特的 upstream_model，绝不会与真实模型混淆
        assert_eq!(ph[0].upstream_model, "__endpoint__");
        assert!(!ph[0].is_free());
    }

    /// TCC 守卫：~/Documents、~/Desktop、~/Downloads、iCloud 下不探测 cwd 相对配置
    /// （那是 "EvolveRouter 想访问文稿文件夹" 弹窗的根因）
    #[test]
    fn tcc_protected_dirs_are_detected_by_path_only() {
        for p in [
            "/Users/u/Documents",
            "/Users/u/Documents/proj",
            "/Users/u/Desktop/x",
            "/Users/u/Downloads",
            "/Users/u/Library/Mobile Documents/com~apple~CloudDocs",
        ] {
            assert!(is_tcc_protected_dir(Path::new(p)), "should be protected: {p}");
        }
        for p in ["/", "/Applications", "/Users/u", "/Users/u/.evo-router", "/tmp/x"] {
            assert!(!is_tcc_protected_dir(Path::new(p)), "should not be protected: {p}");
        }
    }

    #[test]
    fn default_config_paths_prefer_new_names() {
        let paths = default_config_paths();
        let names: Vec<String> = paths.iter().map(|p| p.display().to_string()).collect();
        assert!(names[0].ends_with("evo-router-config.jsonc"), "canonical name first: {names:?}");
        assert!(
            names.iter().any(|n| n.contains(".evo-router/evo-router-config.jsonc")),
            "home dir candidate missing: {names:?}"
        );
        // 旧名仍在候选链里（升级用户配置不静默丢失）
        assert!(names.iter().any(|n| n.contains(".evolve/evolve.toml")), "{names:?}");
    }

    #[test]
    fn explicit_data_dir_is_respected() {
        let got = resolve_data_dir("/tmp/er-explicit");
        assert_eq!(got, Path::new("/tmp/er-explicit").to_path_buf());
    }

    #[test]
    fn legacy_default_migrates_once() {
        let _guard = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        // 用假 HOME 隔离：旧目录存在、新目录不存在 → rename 迁移
        let base = std::env::temp_dir().join(format!("er-migrate-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&base);
        let home = base.join("home");
        let legacy = home.join(".evolve");
        std::fs::create_dir_all(legacy.join("sub")).unwrap();
        std::fs::write(legacy.join("events.jsonl"), "{}").unwrap();
        let old_home = std::env::var("HOME").ok();
        unsafe { std::env::set_var("HOME", &home) };

        std::fs::write(legacy.join("evolve.toml"), "[data]\ndir = \"~/.evolve\"\n").unwrap();
        let got = resolve_data_dir(LEGACY_DATA_DIR);
        assert_eq!(got, home.join(".evo-router"), "must migrate to the new dir");
        assert!(got.join("events/events.jsonl").exists(), "data must survive the move");
        assert!(!legacy.exists(), "legacy dir is moved, not copied");
        // 配置文件要**转换**成规范 JSONC：否则移动后主候选路径找不到 → 静默回退内置默认
        assert!(
            got.join("evo-router-config.jsonc").is_file(),
            "config must be converted to the canonical jsonc in the new dir"
        );
        assert!(!got.join("evolve.toml").exists());

        // 再次调用（新目录已在）保持幂等
        assert_eq!(resolve_data_dir(LEGACY_DATA_DIR), home.join(".evo-router"));
        // 空配置 = 默认值，同样迁移（幂等）
        assert_eq!(resolve_data_dir(""), home.join(".evo-router"));

        match old_home {
            Some(h) => unsafe { std::env::set_var("HOME", h) },
            None => unsafe { std::env::remove_var("HOME") },
        }
        let _ = std::fs::remove_dir_all(&base);
    }

    /// 目标目录已存在但更陈旧（改名遗留的空壳）→ 挪到 .stale-<ts> 后照常迁移，
    /// 不能因为一个残留目录就放弃迁移（本机实测踩到：~/.evo-router 是 10-10 的残留）
    #[test]
    fn stale_target_dir_is_moved_aside_then_migrated() {
        let _guard = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let base = std::env::temp_dir().join(format!("er-stale-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&base);
        let home = base.join("home");
        let legacy = home.join(".evolve");
        let target = home.join(".evo-router");
        std::fs::create_dir_all(&legacy).unwrap();
        std::fs::write(legacy.join("events.jsonl"), "{\"ts\":2}\n").unwrap();
        std::fs::create_dir_all(&target).unwrap();
        std::fs::write(target.join("events.jsonl"), "old\n").unwrap();
        // 让目标文件时间早于旧目录（陈旧空壳）
        let old_time = std::time::SystemTime::now() - std::time::Duration::from_secs(86_400);
        let f = std::fs::File::options().write(true).open(target.join("events.jsonl")).unwrap();
        f.set_modified(old_time).unwrap();  // 旧心跳 → 判定目标为残留空壳

        let old_home = std::env::var("HOME").ok();
        unsafe { std::env::set_var("HOME", &home) };
        let got = resolve_data_dir(LEGACY_DATA_DIR);
        match old_home {
            Some(h) => unsafe { std::env::set_var("HOME", h) },
            None => unsafe { std::env::remove_var("HOME") },
        }
        assert_eq!(got, target, "must migrate into the new dir");
        assert!(got.join("events/events.jsonl").exists());
        let stale: Vec<_> = std::fs::read_dir(&home)
            .unwrap()
            .flatten()
            .filter(|e| e.file_name().to_string_lossy().starts_with(".evo-router.stale-"))
            .collect();
        assert_eq!(stale.len(), 1, "stale dir must be kept aside, not deleted");
        assert!(!legacy.exists());
        let _ = std::fs::remove_dir_all(&base);
    }

    /// 目标目录心跳更新（真的在被使用）→ 不动它，避免搬走用户的活动目录
    #[test]
    fn live_target_dir_wins_over_legacy() {
        let _guard = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let base = std::env::temp_dir().join(format!("er-live-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&base);
        let home = base.join("home");
        let legacy = home.join(".evolve");
        let target = home.join(".evo-router");
        std::fs::create_dir_all(&legacy).unwrap();
        std::fs::write(legacy.join("events.jsonl"), "{\"ts\":1}\n").unwrap();
        std::fs::create_dir_all(&target).unwrap();
        std::fs::write(target.join("events.jsonl"), "{\"ts\":2}\n").unwrap();
        let old_time = std::time::SystemTime::now() - std::time::Duration::from_secs(86_400);
        std::fs::File::options()
            .write(true)
            .open(legacy.join("events.jsonl"))
            .unwrap()
            .set_modified(old_time)
            .unwrap();

        let old_home = std::env::var("HOME").ok();
        unsafe { std::env::set_var("HOME", &home) };
        let got = resolve_data_dir(LEGACY_DATA_DIR);
        match old_home {
            Some(h) => unsafe { std::env::set_var("HOME", h) },
            None => unsafe { std::env::remove_var("HOME") },
        }
        assert_eq!(got, target);
        assert!(legacy.exists(), "the older legacy dir must be left untouched");
        let _ = std::fs::remove_dir_all(&base);
    }

    /// 新默认目录被**残留空壳**占位（配置里写的已是新默认名）时也要核对：
    /// 否则空壳会一直遮蔽旧目录的真实数据（本机实测踩到）
    #[test]
    fn stale_target_with_new_default_config_still_reconciles() {
        let _guard = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let base = std::env::temp_dir().join(format!("er-stale2-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&base);
        let home = base.join("home");
        let legacy = home.join(".evolve");
        let target = home.join(".evo-router");
        std::fs::create_dir_all(&legacy).unwrap();
        std::fs::write(legacy.join("events.jsonl"), "{\"ts\":9}\n").unwrap();
        std::fs::write(legacy.join("evolve.toml"), "[data]\n").unwrap();
        std::fs::create_dir_all(&target).unwrap();
        std::fs::write(target.join("events.jsonl"), "old\n").unwrap();
        std::fs::File::options()
            .write(true)
            .open(target.join("events.jsonl"))
            .unwrap()
            .set_modified(std::time::SystemTime::now() - std::time::Duration::from_secs(86_400))
            .unwrap();

        let old_home = std::env::var("HOME").ok();
        unsafe { std::env::set_var("HOME", &home) };
        let got = resolve_data_dir(DEFAULT_DATA_DIR);
        match old_home {
            Some(h) => unsafe { std::env::set_var("HOME", h) },
            None => unsafe { std::env::remove_var("HOME") },
        }
        assert_eq!(got, target);
        assert!(
            got.join("evo-router-config.jsonc").is_file(),
            "migrated config must be converted to the canonical jsonc"
        );
        assert!(!legacy.exists());
        let _ = std::fs::remove_dir_all(&base);
    }

    /// 布局整理：散落的旧文件进 events/state/logs；幂等 + 不覆盖已有目标
    #[test]
    fn tidy_layout_categorises_loose_files() {
        let base = std::env::temp_dir().join(format!("er-layout-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&base);
        std::fs::create_dir_all(&base).unwrap();
        for f in ["events.jsonl", "snapshot.json", "overrides.json", "gateway.log", "gateway-child.log"] {
            std::fs::write(base.join(f), "x").unwrap();
        }
        std::fs::write(base.join("evo-router.toml"), "cfg").unwrap();
        tidy_layout(&base);
        assert!(base.join("events/events.jsonl").is_file());
        assert!(base.join("state/snapshot.json").is_file());
        assert!(base.join("state/overrides.json").is_file());
        assert!(base.join("logs/gateway.log").is_file());
        assert!(base.join("logs/gateway-child.log").is_file());
        assert!(base.join("evo-router.toml").is_file(), "config stays at the root");
        assert!(!base.join("events.jsonl").exists(), "no loose files left");
        // 幂等：再跑一次不报错、不搬动
        tidy_layout(&base);
        assert!(base.join("events/events.jsonl").is_file());
        // 目标已存在时不覆盖：旧的原样保留，根目录那份改名带时间戳挪进来
        std::fs::write(base.join("events.jsonl"), "new").unwrap();
        tidy_layout(&base);
        assert_eq!(std::fs::read_to_string(base.join("events/events.jsonl")).unwrap(), "x");
        assert!(!base.join("events.jsonl").exists(), "root stays clean even on conflict");
        let moved: Vec<_> = std::fs::read_dir(base.join("events"))
            .unwrap()
            .flatten()
            .filter(|e| e.file_name().to_string_lossy().contains(".old-"))
            .collect();
        assert_eq!(moved.len(), 1, "conflicting file must be kept aside, not dropped");
        assert_eq!(std::fs::read_to_string(moved[0].path()).unwrap(), "new");
        let _ = std::fs::remove_dir_all(&base);
    }

    /// JSON / JSONC 与 TOML 是同一套结构：同一份配置两种写法都要能解析
    #[test]
    fn json_and_jsonc_configs_parse_like_toml() {
        let toml_text = r#"
[server]
port = 8787

[decision]
backend = "auto"

[[decision.slots]]
kind = "jev"
api_key_env = "TYPESAFE_API_KEY"

[[models]]
id = "p/m"
base_url = "https://example.test/v1"
api_key = "inline-key"
"#;
        let json_text = r#"{
  // server 段
  "server": { "port": 8787 },
  /* decision 段 */
  "decision": {
    "backend": "auto",
    "slots": [{ "kind": "jev", "api_key_env": "TYPESAFE_API_KEY" }]
  },
  "models": [
    { "id": "p/m", "base_url": "https://example.test/v1", "api_key": "inline-key" }
  ]
}"#;
        let t = FileConfig::parse_text(toml_text, Some(Path::new("/tmp/evo-router.toml"))).unwrap();
        let j = FileConfig::parse_text(json_text, Some(Path::new("/tmp/evo-router.jsonc"))).unwrap();
        assert_eq!(t.server.port, j.server.port);
        assert_eq!(t.decision.slots.len(), 1);
        assert_eq!(j.decision.slots.len(), 1);
        assert_eq!(j.decision.slots[0].kind, "jev");
        // 内联 key 生效（配置文件即唯一来源）
        let m = j.resolve_model(&j.models[0]);
        assert_eq!(m.keys.len(), 1);
        assert_eq!(m.keys[0].value, "inline-key");
        assert_eq!(m.api_key.as_deref(), Some("inline-key"));
    }

    /// 注释剥离不能误伤字符串里的 `//`（URL！）
    #[test]
    fn json_comment_stripping_keeps_urls() {
        let src = r#"{
  "base_url": "https://example.test/v1", // trailing comment
  "note": "a /* not a comment */ inside a string"
}"#;
        let out = strip_json_comments(src);
        assert!(out.contains("https://example.test/v1"));
        assert!(out.contains("/* not a comment */"));
        assert!(!out.contains("trailing comment"));
        let parsed: serde_json::Value = serde_json::from_str(&out).unwrap();
        assert_eq!(parsed["base_url"], "https://example.test/v1");
    }

    /// 旧 TOML 在数据目录里必须被**转换**成规范 JSONC——直接改名会让解析器按 JSON 读而失败
    #[test]
    fn legacy_toml_is_converted_not_renamed() {
        let base = std::env::temp_dir().join(format!("er-canon-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&base);
        std::fs::create_dir_all(&base).unwrap();
        std::fs::write(
            base.join("evo-router.toml"),
            "[server]\nport = 8787\n\n[decision]\nbackend = \"heuristic\"\n",
        )
        .unwrap();
        tidy_config_name(&base);
        let canonical = base.join(DEFAULT_CONFIG_NAME);
        assert!(canonical.is_file(), "canonical jsonc must be written");
        let text = std::fs::read_to_string(&canonical).unwrap();
        assert!(text.trim_start().starts_with("//"), "canonical file carries a header comment");
        let parsed = FileConfig::parse_text(&text, Some(&canonical)).unwrap();
        assert_eq!(parsed.server.port, 8787);
        assert_eq!(parsed.decision.backend, "heuristic");
        // 旧文件保留（带 .migrated 后缀），不丢内容
        assert!(base.join("evo-router.toml.migrated").is_file());
        let _ = std::fs::remove_dir_all(&base);
    }

    /// 回归：热加载路径必须走 `parse_text`（按扩展名），否则 JSONC 配置永远解析失败
    /// —— 实测用户写好 StartLux 槽位却不生效，日志只有 "config reload: parse failed"
    #[test]
    fn jsonc_file_parses_for_reload_path() {
        let dir = std::env::temp_dir().join(format!("er-reload-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let file = dir.join(DEFAULT_CONFIG_NAME);
        std::fs::write(
            &file,
            r#"// header comment
{
  "decision": {
    "backend": "auto",
    "slots": [{ "kind": "jev", "base_url": "https://api.bamboonode.dev/v1/systemone",
                "model": "startlux-decision-27b-autothink", "api_key": "bk-x" }]
  }
}"#,
        )
        .unwrap();
        let text = std::fs::read_to_string(&file).unwrap();
        let cfg = FileConfig::parse_text(&text, Some(&file)).expect("jsonc config must parse");
        assert_eq!(cfg.decision.slots.len(), 1);
        assert_eq!(cfg.decision.slots[0].kind, "jev");
        assert_eq!(cfg.decision.slots[0].model.as_deref(), Some("startlux-decision-27b-autothink"));
        // 同一份文本走 TOML 解析器必然失败（这正是曾经的 bug）
        assert!(FileConfig::parse(&text).is_err());
        let _ = std::fs::remove_dir_all(&dir);
    }
}

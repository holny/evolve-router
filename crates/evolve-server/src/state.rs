use evolve_core::catalog::Catalog;
use evolve_core::config::FileConfig;
use evolve_core::engine::Engine;
use evolve_decision::DecisionBackend;
use evolve_memory::{EventLog, Flywheel, HealthRegistry, QuotaLedger, SessionStore};
use serde_json::{json, Value};
use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tokio::sync::broadcast;

pub struct Inner {
    pub config: FileConfig,
    /// 实际加载的配置文件路径（None = 内置默认）——配置热加载 watcher 用
    pub config_path: Option<std::path::PathBuf>,
    /// 配置热加载状态（面板/网页可读：热加载时间 + 是否需要重启）
    pub config_watch: Mutex<ConfigWatch>,
    /// 配置文件里的 `providers` 段（热挂载：导出/手改后 5s 内生效，供 effective_overrides 合并）
    pub cfg_providers: Mutex<Vec<evolve_core::config::ProviderCfg>>,
    pub key_cursor: Mutex<HashMap<String, usize>>,
    pub engine: Engine,
    pub sessions: SessionStore,
    pub events: EventLog,
    pub health: HealthRegistry,
    /// 共享记账（Arc：流式 Finalizer 与 API 读侧必须同一实例，否则用量入不了账）
    pub quota: std::sync::Arc<QuotaLedger>,
    pub flywheel: Flywheel,
    pub bus: broadcast::Sender<serde_json::Value>,
    pub http: reqwest::Client,
    /// 订阅方案预算（plan_key → 5h 积分额度），面板档位驱动
    pub plan_budgets: Mutex<HashMap<String, f64>>,
    /// 全局扫描进度（面板轮询展示：逐 provider 拉取日志）
    pub scan_progress: Mutex<ScanProgress>,
    /// 最近一次判定的实际来源（面板显示 "judge by …" + 配了模型却回退规则时告警）
    pub last_judge: Mutex<LastJudge>,
    /// 判定槽位链的健康句柄（含"最近一次失败原因"）——热加载后替换
    pub slot_health: Mutex<Option<std::sync::Arc<evolve_decision::slots::SlotHealth>>>,
    /// ⑧ 能力卡蒸馏器（decision_model/auto 后端才有；None = 蒸馏停用）
    pub distiller: Option<evolve_decision::decision_backend::DecisionModelBackend>,
    /// A3 completion cache（逻辑请求指纹 → 响应，本地内存 TTL 缓存）
    pub ccache: Mutex<crate::relay::CompletionCache>,
    /// 按量 provider 余额缓存（provider key → /user/balance 响应节选；60s 节流刷新）
    pub payg_balance: Mutex<HashMap<String, Value>>,
    /// models.dev 参考刷新待执行（build_state 同步上下文不能 spawn，交给 start_background）
    pub modelsdev_refresh_due: bool,
    /// 进程内剔除表（用户裁决 2026-10）：协议不支持（如 Responses-only）且用户
    /// **未显式配置**的模型，从目录/路由/面板/探活整体剔除（id → 原因）。仅内存
    /// 不落盘——网关重启即复位，用户在 evolve.toml 显式声明的模型（Source::User）
    /// 自然复活。
    pub excluded_models: Mutex<HashMap<String, String>>,
}

/// 全局扫描进度快照（/api/providers/scan-progress 响应体）
#[derive(Debug, Clone, Default, serde::Serialize)]
pub struct ScanProgress {
    pub running: bool,
    pub done: bool,
    pub log: Vec<String>,
}

/// 最近一次判定的实际来源（relay/anthropic 决策后写入）
#[derive(Debug, Clone, Default, serde::Serialize)]
pub struct LastJudge {
    /// judge_source（judgment 里的字符串）
    pub source: String,
    /// 写入时刻（epoch ms）
    pub at_ms: u64,
    /// 该次判定是否走了决策模型（decision_model / decision_model+rules）
    pub model_used: bool,
}

/// 配置热加载状态（`[[decision.slots]]` 等判定配置可热替换；其余改动需重启）
#[derive(Debug, Default, Clone, serde::Serialize)]
pub struct ConfigWatch {
    /// 上次热加载成功的时刻（epoch ms）
    pub reloaded_at_ms: Option<u64>,
    /// 配置文件里**不可热加载**的部分发生了变化 → 面板/网页提示重启
    pub restart_required: bool,
    /// 触发 restart_required 的原因（人读）
    pub restart_reason: Option<String>,
    /// 启动时的"除 [decision] 之外"指纹（不可变基线：当前指纹与它不同 → 需重启，
    /// 改回原样后告警自动消失，不会留假提醒）
    #[serde(skip)]
    pub static_fingerprint: String,
    /// 当前配置文件的同口径指纹
    #[serde(skip)]
    pub current_fingerprint: String,
    /// 当前**实际生效**的判定后端（热替换后即时更新——api_live 的状态以它为准，
    /// 而不是启动时的配置快照）
    pub active_backend: String,
    /// 当前实际生效的槽位标签（如 ["jev"]）
    pub active_slots: Vec<String>,
    /// 是否有可用的判定槽位链在跑（heuristic-only 时为 false）
    pub judge_active: bool,
}

/// 指纹：配置中**除 `decision` 段以外**的内容（判定配置可热加载，其余改动需重启）。
/// 走解析后的结构而不是原始文本——同一份配置 TOML / JSONC 两种写法指纹一致。
pub fn static_config_fingerprint(text: &str, path: Option<&std::path::Path>) -> Option<String> {
    let cfg = evolve_core::config::FileConfig::parse_text(text, path).ok()?;
    let mut v = serde_json::to_value(&cfg).ok()?;
    if let Some(obj) = v.as_object_mut() {
        // decision 与 providers 都是**可热加载**段（判定链 / provider 清单）——
        // 只有其余段落改动才算"需要重启"
        obj.remove("decision");
        obj.remove("providers");
    }
    Some(canonical_json(&v))
}

/// 稳定的 JSON 文本（对象键排序）——避免 HashMap 顺序导致指纹抖动
fn canonical_json(v: &serde_json::Value) -> String {
    match v {
        serde_json::Value::Object(m) => {
            let mut keys: Vec<&String> = m.keys().collect();
            keys.sort();
            let body: Vec<String> = keys
                .iter()
                .map(|k| format!("{}:{}", serde_json::to_string(k).unwrap_or_default(), canonical_json(&m[*k])))
                .collect();
            format!("{{{}}}", body.join(","))
        }
        serde_json::Value::Array(a) => {
            format!("[{}]", a.iter().map(canonical_json).collect::<Vec<_>>().join(","))
        }
        other => other.to_string(),
    }
}

pub type AppState = Arc<Inner>;

pub fn build_state_with_path(config: FileConfig, config_src: Option<std::path::PathBuf>) -> AppState {
    // 兼容旧签名：无路径 = 不启用热加载 watcher
    build_state_inner(config, config_src)
}

pub fn build_state(config: FileConfig) -> AppState {
    build_state_inner(config, None)
}

fn build_state_inner(config: FileConfig, config_src: Option<std::path::PathBuf>) -> AppState {
    let mut modelsdev_refresh_due = false;
    // overrides 提前加载：removed_base_urls 过滤初始发现（重启不复活已删 provider）
    let ov = merge_config_providers(&config.data.dir, &config.providers);
    let removed: std::collections::HashSet<String> =
        ov.removed_base_urls.iter().cloned().collect();

    let mut discovered = evolve_discovery::discover(&config.discovery.agents);
    discovered.retain(|m| !removed.contains(&m.base_url));

    // remote /models discovery FIRST (provider union), THEN models.dev
    // enrichment can fill unknown windows for remote entries too
    if config.catalog.remote_fetch {
        let mut base_records = config.model_records();
        base_records.extend(discovered.iter().cloned());
        let self_origin = format!("{}:{}", config.server.host, config.server.port);
        discovered.extend(evolve_discovery::remote::discover_remote_blocking(
            &base_records,
            &config.data.dir,
            Some(&self_origin),
        ));
    }

    if config.catalog.modelsdev_reference {
        if let Some(api) = evolve_discovery::modelsdev::load_fresh(&config.data.dir) {
            evolve_discovery::modelsdev::enrich(&mut discovered, &api);
        }
        // 刷新放到 start_background（runtime 内）——同步 build_state 里 spawn 会 panic
        modelsdev_refresh_due = true;
    }

    // remote entries without a usable window are dead weight — drop them
    discovered.retain(|m| m.source != evolve_core::types::Source::Remote || m.context_window.is_some());
    discovered.retain(|m| !removed.contains(&m.base_url));

    let catalog = Catalog::build_with_discovered(&config, discovered);
    let policy = config.policy.clone();
    // w2 槽位链（JEV > LAYA > OPENAI，配一即用；硬错进程内拉黑，重启复位）
    let (backend, chain) = DecisionBackend::build_with_chain(&config.decision.backend, &config.decision);
    let mut engine = Engine::new(catalog, policy, backend);
    // S5 层级比例控制器：目标 7:2:1（cheap/medium/premium），容忍带来自 [policy]
    engine.set_ratio_control(true, [0.7, 0.2, 0.1], engine.policy.ratio_band);

    // Provider 官方信息复核提醒（provider_info）：价格/折扣/额度会变——
    // 超过 30 天未核对官方文档则提示（附 docs_url，便于快速复核）
    {
        let now_ms = evolve_memory::quota::now_ms();
        let due = evolve_core::provider_info::review_due(now_ms);
        if !due.is_empty() {
            let list: Vec<String> = due.iter()
                .map(|p| format!("{} ({}, {})", p.host, p.reviewed_at, p.docs_url))
                .collect();
            tracing::warn!(count = due.len(), providers = ?list, "provider info review due (>30d) — re-verify official docs");
        }
    }

    // ⑧ 蒸馏卡叠加：上次蒸馏的 tiers 修正回放进 catalog（重启不丢失）。
    // 用户显式声明（tiers_explicit）不被覆盖；回滚 = 删 distilled.json
    {
        let store = evolve_core::distill::load_store(&config.data.dir);
        if !store.is_empty() {
            let mut models = engine.catalog_snapshot();
            let applied = evolve_core::distill::apply_distilled(&mut models, &store);
            engine.replace_catalog(models);
            tracing::info!(applied, "distilled capability cards applied to catalog");
        }
    }

    // decision-model intent-routing：chain 里有 Model 型槽位即接通槽位链
    // advisor（fatal 拉黑与 judge 共享同一 health）；laya-only 部署无 advisor。
    // 蒸馏器复用优先级最高的 Model 槽位（Jev > OpenAI）。
    let distiller = chain.as_ref().and_then(|c| c.first_model());
    if let Some(c) = &chain {
        engine.set_route_advisor(Some(std::sync::Arc::new(evolve_decision::slots::SlotAdvisor(c.clone()))));
        tracing::info!("route_advisor: slot chain advisor connected");
    }

    // 面板调控覆盖（overrides.json，重启重放）：模型权重 + 公式权重
    for (id, w) in &ov.models {
        engine.set_weight_override(id, Some(*w));
    }
    if let Some(w) = ov.weights {
        engine.set_weights_override(Some(evolve_core::types::PolicyWeights {
            quality: w[0], speed: w[1], cost: w[2], stability: w[3], headroom: w[4],
        }));
    }

    // 订阅方案预算：档位→额度（registry），显式 allowance 覆盖优先。
    // 仅取 plans 层（方案级）；手动 provider 档位额度只进展示层（meta），
    // 不入路由预算——同 plan_key 多账号（如 opencode-go 与 -github）档位
    // 各异，方案级注入会错把一家档位额度压到别家账号头上
    let mut plan_budgets: HashMap<String, f64> = HashMap::new();
    for (key, po) in &ov.plans {
        if let Some(tier) = &po.tier
            && let Some(a) = evolve_core::plans::tier_allowance_by_key(key, tier, 0)
        {
            plan_budgets.insert(key.clone(), a);
        }
    }

    let events = EventLog::open(&config.data.dir);
    // ⑦ 飞轮策略参数从配置注入（[strategy] 段；缺省 = 编译默认）
    let flywheel = Flywheel::open_with(
        &config.data.dir,
        evolve_memory::strategy::StrategyParams {
            version: config.strategy.version,
            ok_weight: config.strategy.ok_weight,
            bias_gain: config.strategy.bias_gain,
        },
    );
    let (bus, _) = broadcast::channel(256);
    let http = reqwest::Client::builder()
        .connect_timeout(std::time::Duration::from_secs(10))
        .pool_idle_timeout(std::time::Duration::from_secs(90))
        .build()
        .expect("http client");

    let initial_providers = config.providers.clone();
    let slot_health = chain.as_ref().map(|c| c.health.clone());
    // 当前生效的判定配置（api_live 的状态来源；热加载后由 reload_config 更新）
    let active_slots: Vec<String> = chain
        .as_ref()
        .map(|c| c.slots.iter().map(|s| s.kind.label().to_string()).collect())
        .unwrap_or_default();
    // 热加载基线：读一遍原始配置文本，记录"除 [decision] 外"的指纹
    let (config_path, watch) = match &config_src {
        Some(p) => {
            let text = std::fs::read_to_string(p).unwrap_or_default();
            (
                Some(p.clone()),
                ConfigWatch {
                    reloaded_at_ms: None,
                    restart_required: false,
                    restart_reason: None,
                    static_fingerprint: static_config_fingerprint(&text, Some(p))
                        .unwrap_or_default(),
                    current_fingerprint: static_config_fingerprint(&text, Some(p))
                        .unwrap_or_default(),
                    active_backend: config.decision.backend.clone(),
                    active_slots: active_slots.clone(),
                    judge_active: !active_slots.is_empty(),
                },
            )
        }
        None => (
            None,
            ConfigWatch {
                active_backend: config.decision.backend.clone(),
                active_slots: active_slots.clone(),
                judge_active: !active_slots.is_empty(),
                ..Default::default()
            },
        ),
    };
    Arc::new(Inner {
        config,
        config_path,
        config_watch: Mutex::new(watch),
        cfg_providers: Mutex::new(initial_providers),
        key_cursor: Mutex::new(HashMap::new()),
        engine,
        sessions: SessionStore::new(),
        events,
        health: HealthRegistry::new(),
        quota: std::sync::Arc::new(QuotaLedger::new()),
        flywheel,
        bus,
        http,
        plan_budgets: Mutex::new(plan_budgets),
        scan_progress: Mutex::new(ScanProgress::default()),
        last_judge: Mutex::new(LastJudge::default()),
        slot_health: Mutex::new(slot_health.clone()),
        distiller,
        ccache: Mutex::new(crate::relay::CompletionCache::new()),
        payg_balance: Mutex::new(HashMap::new()),
        modelsdev_refresh_due,
        excluded_models: Mutex::new(HashMap::new()),
    })
}

/// 配置热加载 watcher（5s 轮询 mtime，改动即生效）：
///
/// - **可热加载**：`[[decision.slots]]`（判定槽位）→ 重建槽位链并替换引擎里的
///   判定器 + route advisor，用户无需重启网关；
/// - **需重启**：其余任何段落改动（模型目录/策略/服务端口/provider 等）→ 只置
///   `config_watch.restart_required` 并在日志/接口里提示，**不会假装生效**。
pub fn spawn_config_watcher(st: AppState) {
    let Some(path) = st.config_path.clone() else { return };
    tokio::spawn(async move {
        let mut last = std::fs::metadata(&path).and_then(|m| m.modified()).ok();
        let mut tick = tokio::time::interval(Duration::from_secs(5));
        tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        loop {
            tick.tick().await;
            let now = std::fs::metadata(&path).and_then(|m| m.modified()).ok();
            if now.is_none() || now == last {
                continue;
            }
            // 解析失败（编辑器半写/语法错）不推进 last → 5s 后自动重试，用户改好即生效
            if reload_config(&st, &path) {
                last = now;
            }
        }
    });
}

/// 立即重载配置（watcher 与 POST /api/reload 共用）
pub fn reload_config(st: &AppState, path: &std::path::Path) -> bool {
    let Ok(text) = std::fs::read_to_string(path) else {
        tracing::warn!(path = %path.display(), "config reload: file unreadable, keeping the running config");
        return false;
    };
    // ⚠ 必须按扩展名解析：用 TOML-only 的 `parse` 会让 JSONC 配置永远"parse failed"
    //（实测：用户写好 StartLux 槽位却迟迟不生效，日志里只有 parse failed）
    let Ok(new_cfg) = evolve_core::config::FileConfig::parse_text(&text, Some(path)) else {
        tracing::warn!(path = %path.display(), "config reload: parse failed, keeping the running config");
        return false;
    };
    // ① 不可热加载部分是否变了 → 只提示，不假装生效
    let fingerprint = static_config_fingerprint(&text, Some(path)).unwrap_or_default();
    let restart_reason = Some(
        "sections other than [decision] changed — restart the gateway to apply".to_string(),
    );
    // ② 判定槽位热替换
    let (judge, chain) = evolve_decision::DecisionBackend::build_with_chain(
        &new_cfg.decision.backend,
        &new_cfg.decision,
    );
    let slots: Vec<&str> = chain
        .as_ref()
        .map(|c| c.slots.iter().map(|s| s.kind.label()).collect())
        .unwrap_or_default();
    st.engine.set_judge(judge);
    if let Ok(mut g) = st.slot_health.lock() {
        *g = chain.as_ref().map(|c| c.health.clone());
    }
    st.engine.set_route_advisor(
        chain
            .as_ref()
            .map(|c| std::sync::Arc::new(evolve_decision::slots::SlotAdvisor(c.clone())) as std::sync::Arc<dyn evolve_core::types::RouteAdvisor>),
    );
    let prev_providers = st.cfg_providers.lock().map(|g| g.len()).unwrap_or(0);
    if let Ok(mut g) = st.cfg_providers.lock() {
        *g = new_cfg.providers.clone();
    }
    if prev_providers != new_cfg.providers.len() {
        tracing::info!(
            from = prev_providers,
            to = new_cfg.providers.len(),
            "config hot-reloaded: provider list updated"
        );
    }
    let now_ms = evolve_memory::health::now();
    if let Ok(mut w) = st.config_watch.lock() {
        w.reloaded_at_ms = Some(now_ms);
        w.current_fingerprint = fingerprint;
        w.active_backend = new_cfg.decision.backend.clone();
        w.active_slots = slots.iter().map(|s| s.to_string()).collect();
        w.judge_active = !slots.is_empty();
        // 与启动基线比较：非判定段落改动 → 提示重启；改回原样 → 自动清除
        w.restart_required = w.current_fingerprint != w.static_fingerprint;
        w.restart_reason = if w.restart_required { restart_reason } else { None };
    }
    tracing::info!(
        slots = ?slots,
        backend = %new_cfg.decision.backend,
        "config hot-reloaded: decision slots swapped (no restart needed)"
    );
    true
}

/// 记录一次判定的实际来源（决策完成后调用；轻量，一次锁写入）
pub fn note_judge(st: &AppState, judge_source: &str) {
    let src = judge_source.trim();
    if src.is_empty() {
        return;
    }
    if let Ok(mut g) = st.last_judge.lock() {
        g.source = src.to_string();
        g.at_ms = evolve_memory::health::now();
        g.model_used = src.starts_with("decision_model");
    }
}

/// 把配置文件里的 `providers` 合并进 overrides（**配置优先于 overrides.json**），
/// 并保留来源标记（origin / source_path），供面板与网页展示"这个 provider 从哪来"。
/// 去重依旧由网关内部按 base_url + api_key 指纹完成，这里只做条目级归并。
/// 请求路径上的**生效配置**读取：overrides.json + 配置文件里的 providers（配置优先）。
/// 面板/网页读 provider 列表时必须走这里——直接用 `load_overrides` 会漏掉配置文件里的
/// 来源标记（origin / source_path）与用户显式声明的 provider。
pub fn effective_overrides(st: &AppState) -> Overrides {
    let providers = st
        .cfg_providers
        .lock()
        .map(|g| g.clone())
        .unwrap_or_default();
    merge_config_providers(&st.config.data.dir, &providers)
}

pub fn merge_config_providers(
    data_dir: &str,
    cfg_providers: &[evolve_core::config::ProviderCfg],
) -> Overrides {
    let mut ov = load_overrides(data_dir);
    for p in cfg_providers {
        if p.base_url.trim().is_empty() {
            continue;
        }
        // 自指条目（base_url 指向本机网关自身）永不当成上游 provider
        let self_hint = p.base_url.contains("127.0.0.1:") || p.base_url.contains("localhost:");
        if self_hint {
            tracing::debug!(key = %p.key, base = %p.base_url, "skip self-referential provider entry");
            continue;
        }
        let key = if p.key.trim().is_empty() {
            // 没给 key：按 base_url 主机名推断（与目录侧 provider 命名习惯一致）
            p.base_url
                .split("//")
                .nth(1)
                .unwrap_or(&p.base_url)
                .split('/')
                .next()
                .unwrap_or("provider")
                .to_string()
        } else {
            p.key.clone()
        };
        let entry = ov.providers.entry(key).or_default();
        entry.base_url = p.base_url.trim_end_matches('/').to_string();
        if p.api_key.as_deref().map(|k| !k.trim().is_empty()).unwrap_or(false) {
            entry.api_key = p.api_key.clone();
        } else if entry.api_key.is_none()
            && let Some(env) = p.api_key_env.as_deref()
            && let Ok(v) = std::env::var(env)
            && !v.trim().is_empty()
        {
            entry.api_key = Some(v);
        }
        if let Some(v) = &p.protocol { entry.protocol = v.clone(); }
        if let Some(v) = &p.plan_kind { entry.plan_kind = v.clone(); }
        if let Some(v) = &p.tier { entry.tier = v.clone(); }
        if let Some(v) = &p.docs_url { entry.docs_url = v.clone(); }
        if let Some(v) = p.disabled { entry.disabled = v; }
        entry.origin = p.origin.clone();
        entry.source_path = p.source_path.clone();
    }
    ov
}

/// 导出 provider 清单到规范配置（`POST /api/config/export` / `evo-router config export`）。
///
/// 逐条来源判定：
/// - `origin = "user"`：用户在面板/网页新增或编辑过的（`overrides.json`）
/// - `origin = "discovered"`：从某个 Agent 配置读来的，`source_path` 记下**加载路径**
///   （发现器的 `source_note`，形如 `opencode @ ~/.config/opencode/opencode.jsonc`）
///
/// 不导出模型级目录（那是 `[[models]]` 的职责，且体量大）；只导出 provider 级凭据，
/// 因为那才是"用户可定义、又散落在各处"的部分。返回写出的条数。
pub fn export_providers_to_config(st: &AppState, path: &std::path::Path) -> anyhow::Result<usize> {
    let text = std::fs::read_to_string(path).unwrap_or_default();
    let mut cfg = evolve_core::config::FileConfig::parse_text(&text, Some(path))?;
    let snap = st.engine.catalog_snapshot();
    let ov = load_overrides(&st.config.data.dir);
    let self_ep = format!("127.0.0.1:{}", st.config.server.port);
    let self_ep2 = format!("localhost:{}", st.config.server.port);

    // 现网 provider（含发现来的）→ 按 key 归并；key 取目录里的 provider 名
    let mut entries: Vec<evolve_core::config::ProviderCfg> = Vec::new();
    let mut seen: std::collections::HashSet<String> = std::collections::HashSet::new();
    for m in &snap {
        if !seen.insert(m.provider.clone()) {
            continue;
        }
        // 网关自身（agent 配置里指向本机的那个 provider）不是上游：不导出，
        // 否则会被配置回灌成"手动 provider"，面板/网页出现自指条目（用户实测反馈）
        if m.base_url.contains(&self_ep) || m.base_url.contains(&self_ep2) {
            continue;
        }
        let user_override = ov.providers.get(&m.provider);
        let discovered = matches!(m.source, evolve_core::types::Source::Discovered | evolve_core::types::Source::Remote);
        // 发现来的条目**必须带上真实 key**：只写 base_url 会让配置回灌后
        // has_key=false、身份指纹为空 → 面板把同端点的两个账号误判成"重复"
        let discovered_key = m
            .keys
            .iter()
            .find(|k| !k.value.trim().is_empty())
            .map(|k| k.value.clone())
            .or_else(|| m.api_key.clone().filter(|k| !k.trim().is_empty()));
        entries.push(evolve_core::config::ProviderCfg {
            key: m.provider.clone(),
            base_url: user_override.map(|p| p.base_url.clone()).unwrap_or_else(|| m.base_url.clone()),
            api_key: user_override
                .and_then(|p| p.api_key.clone())
                .or(discovered_key),
            api_key_env: m.api_key_env.clone(),
            protocol: user_override.map(|p| p.protocol.clone()),
            plan_kind: user_override.map(|p| p.plan_kind.clone()),
            tier: user_override.map(|p| p.tier.clone()),
            docs_url: user_override.map(|p| p.docs_url.clone()),
            disabled: user_override.map(|p| p.disabled),
            origin: Some(if user_override.is_some() { "user".into() } else if discovered { "discovered".into() } else { "builtin".into() }),
            // 发现来源的**加载路径**（opencode @ …, codex @ …）；用户配置则标明来自哪里
            source_path: if user_override.is_some() {
                Some("overrides.json (dashboard/panel)".into())
            } else {
                m.source_note.clone()
            },
        });
    }
    entries.sort_by(|a, b| a.key.cmp(&b.key));
    let n = entries.len();
    cfg.providers = entries.clone();
    // 立即生效：无需等 5s watcher，也不用重启
    if let Ok(mut g) = st.cfg_providers.lock() {
        *g = entries;
    }
    std::fs::write(path, evolve_core::config::to_jsonc(&cfg))?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let _ = std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600));
    }
    tracing::info!(count = n, path = %path.display(), "provider list exported to the config file");
    Ok(n)
}

impl Inner {
    /// 按 id 查模型（读锁短临界区，克隆返回）
    pub fn catalog_get(&self, id: &str) -> Option<evolve_core::types::ModelRecord> {
        self.engine.catalog.read().ok().and_then(|c| c.get(id).cloned())
    }

    /// 进程内剔除模型（协议不支持）：登记原因并从 catalog 物理移除——路由选型、
    /// 面板展示、周期探活三处都读 catalog_snapshot，一并消失。幂等；仅内存，
    /// 重启复位（见 `excluded_models` 字段说明）。
    pub fn exclude_models(&self, ids: Vec<(String, String)>) {
        if ids.is_empty() {
            return;
        }
        if let Ok(mut ex) = self.excluded_models.lock() {
            for (id, reason) in &ids {
                ex.insert(id.clone(), reason.clone());
            }
        }
        let mut models = self.engine.catalog_snapshot();
        let before = models.len();
        models.retain(|m| !ids.iter().any(|(id, _)| id == &m.id));
        let removed = before - models.len();
        if removed > 0 {
            self.engine.replace_catalog(models);
            tracing::info!(removed, "protocol-unsupported models excluded from catalog");
        }
    }

    /// 已在剔除表内的模型 id（重扫/重拉时跳过，避免复活被剔除的模型）
    pub fn excluded_ids(&self) -> std::collections::HashSet<String> {
        self.excluded_models
            .lock()
            .map(|m| m.keys().cloned().collect())
            .unwrap_or_default()
    }

    /// Provider 实时配额上报（每次响应头部到达即写）——面板展示以此为准。
    /// plan_key 由调用方按 record.base_url 解析；headers 为原始上游响应头
    pub fn observe_provider_quota(&self, plan_key: &str, headers: &axum::http::HeaderMap) {
        let parsed = evolve_memory::quota::QuotaLedger::parse_headers(headers);
        self.quota.observe_provider(plan_key, &parsed);
    }

    /// 订阅方案预算压力（plan_key → 消耗占比）：额度内积分消耗 / 档位额度。
    /// 无额度配置（未声明档位）的方案无压力。
    pub fn plan_pressure_map(&self) -> HashMap<String, f32> {
        let mut out = HashMap::new();
        let Ok(budgets) = self.plan_budgets.lock() else { return out };
        if budgets.is_empty() {
            return out;
        }
        let now = evolve_memory::quota::now_ms();
        let sums = self.quota.plan_usage_sums(now, 5 * 3600 * 1000);
        // 模型 → plan_key 映射（按 catalog base_url 匹配注册表）
        for m in self.engine.catalog_snapshot() {
            if let Some(key) = evolve_core::plans::plan_key_for(&m.base_url)
                && let Some(&allowance) = budgets.get(key)
                && allowance > 0.0
                && let Some((i, c, o)) = sums.get(&m.id)
            {
                // 同方案多模型共享账户额度——SUM 而非 MAX（MAX 会漏计并发燃烧）
                let credits = evolve_core::plans::plan_credits_used(&m.base_url, &m.id, *i, *c, *o);
                let p = (credits / allowance).clamp(0.0, 1.5) as f32;
                out.entry(key.to_string()).and_modify(|e| *e = (*e + p).min(1.5)).or_insert(p);
            }
        }
        out
    }

    /// Round-robin starting index for a model's key pool (load spread).
    pub fn key_start(&self, model: &str, len: usize) -> usize {
        if len <= 1 {
            return 0;
        }
        let Ok(mut m) = self.key_cursor.lock() else { return 0 };
        let e = m.entry(model.to_string()).or_insert(0);
        let v = *e;
        *e = (*e + 1) % len;
        v
    }

    /// Must be called inside the tokio runtime.
    /// 榜单快照读取（/api/benchmarks 展示 + 拉取失败时保留旧值）
    pub(crate) fn bench_snapshot(&self) -> evolve_discovery::benchmarks::BenchSnapshot {
        std::fs::read_to_string(evolve_core::config::state_file(&self.config.data.dir, "benchmarks.json"))
            .ok()
            .and_then(|t| serde_json::from_str(&t).ok())
            .unwrap_or_default()
    }

    /// 榜单快照写入
    pub(crate) fn save_bench_snapshot(
        &self,
        sources: &[(String, Vec<evolve_discovery::benchmarks::BenchScore>)],
    ) {
        let snapshot = evolve_discovery::benchmarks::BenchSnapshot {
            sources: sources
                .iter()
                .map(|(name, scores)| {
                    (
                        name.clone(),
                        evolve_discovery::benchmarks::SourceState {
                            fetched_at_ms: evolve_memory::health::now(),
                            scores: scores.clone(),
                            error: None,
                        },
                    )
                })
                .collect(),
        };
        if let Ok(text) = serde_json::to_string_pretty(&snapshot) {
            let path = evolve_core::config::state_file(&self.config.data.dir, "benchmarks.json");
            evolve_core::config::ensure_parent(&path);
            let _ = std::fs::write(path, text);
        }
    }

    pub fn start_background(self: &AppState) {
        if self.modelsdev_refresh_due {
            evolve_discovery::modelsdev::spawn_refresh(self.config.data.dir.clone());
        }
        // 榜单管线（决策 #23 + 用户裁决 2026-10）：启动拉一次 + 每 interval_hours
        // 刷新——外部榜单（默认 LMArena 镜像，用户盲测）→ 混合 tiers → 引擎叠加；
        // 单源拉取失败保留该源旧值（快照），永不影响路由主链路
        if self.config.benchmarks.enabled {
            let b_state = self.clone();
            let interval_h = self.config.benchmarks.interval_hours.max(1) as u64;
            tokio::spawn(async move {
                loop {
                    let now_ms = evolve_memory::health::now();
                    let prev = b_state.bench_snapshot();
                    // 逐源构建 SourceState：新拉成功 → 新值；失败/空 → 保留旧值
                    // 并标记错误（快照如实反映数据新旧，不把旧数据伪装成新鲜）
                    let mut sources: Vec<(String, evolve_discovery::benchmarks::SourceState)> = vec![(
                        "seed".into(),
                        evolve_discovery::benchmarks::SourceState {
                            fetched_at_ms: now_ms,
                            scores: evolve_discovery::benchmarks::load_seed(),
                            error: None,
                        },
                    )];
                    for cfg in &b_state.config.benchmarks.sources {
                        match evolve_discovery::benchmarks::fetch_source(&b_state.http, cfg).await {
                            Ok(scores) if !scores.is_empty() => {
                                sources.push((
                                    cfg.name.clone(),
                                    evolve_discovery::benchmarks::SourceState {
                                        fetched_at_ms: now_ms,
                                        scores,
                                        error: None,
                                    },
                                ));
                            }
                            other => {
                                let err = match &other {
                                    Ok(_) => "fetch returned no rows (page structure changed?)".to_string(),
                                    Err(e) => format!("fetch failed: {e}"),
                                };
                                tracing::warn!(source = %cfg.name, error = %err, "benchmark source: keeping last-good scores");
                                if let Some(prev_src) = prev.sources.get(&cfg.name) {
                                    let mut kept = prev_src.clone();
                                    kept.error = Some(err);
                                    sources.push((cfg.name.clone(), kept));
                                }
                            }
                        }
                    }
                    // apply/快照只吃分数列表（SourceState 的 error/时间戳仅用于展示）
                    let scores_only: Vec<(String, Vec<evolve_discovery::benchmarks::BenchScore>)> =
                        sources.iter()
                            .map(|(n, st)| (n.clone(), st.scores.clone()))
                            .collect();
                    let n = evolve_discovery::benchmarks::apply_to_engine(&b_state.engine, &scores_only);
                    b_state.save_bench_snapshot(&scores_only);
                    tracing::info!(applied = n, sources = sources.len(), "benchmark tiers refreshed");
                    tokio::time::sleep(Duration::from_secs(interval_h * 3600)).await;
                }
            });
        }
        let flywheel = self.flywheel.clone();
        tokio::spawn(async move {
            let mut tick = tokio::time::interval(Duration::from_secs(30));
            tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
            loop {
                tick.tick().await;
                flywheel.save();
            }
        });
        // 定期目录重扫（用户裁决）：每 5 分钟重新发现 agent 配置——新增自动进
        // 目录；删除不主动删（面板手动删）。每 60 分钟重拉远程 /models
        // （远端模型更新后网关及时跟进）。
        // AppState = Arc<Inner>，直接 clone 保持引用（网关生命周期内常驻）
        let state: AppState = self.clone();
        tokio::spawn(async move {
            let mut scan_tick = tokio::time::interval(Duration::from_secs(300));
            scan_tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
            let mut remote_tick = tokio::time::interval(Duration::from_secs(3600));
            remote_tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
            let mut distill_tick = tokio::time::interval(Duration::from_secs(600));
            distill_tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
            distill_tick.tick().await; // 启动首跳立即消费：等 10 分钟后再首轮检查
            loop {
                tokio::select! {
                    _ = scan_tick.tick() => {
                        state.rescan_discovery();
                    }
                    _ = remote_tick.tick() => {
                        state.refresh_remote_models();
                    }
                    _ = distill_tick.tick() => {
                        // review#9 修复：蒸馏含逐模型 LLM 调用（单次 20s 超时），
                        // spawn 出去避免阻塞共享循环里的 scan/remote tick
                        let d_state = state.clone();
                        tokio::spawn(async move { d_state.maybe_distill().await });
                    }
                }
            }
        });
    }

    /// ⑧ 能力卡蒸馏检查（每小时）：达到里程碑（每 50 成功/模型）的模型
    /// 交 Jev 重估 tiers，保守收缩后写入 distilled.json 并叠加 catalog。
    /// Jev 失败静默跳过——蒸馏失败永不影响路由主链路。
    async fn maybe_distill(&self) {
        let Some(d) = &self.distiller else { return };
        let stats = self.flywheel.stats();
        let mut store = evolve_core::distill::load_store(&self.config.data.dir);
        let mut changed = 0usize;
        for (id, s) in &stats {
            if !evolve_core::distill::due_for_distill(s.success, store.get(id)) {
                continue;
            }
            let success_rate = if s.requests > 0 {
                s.success as f64 / s.requests as f64
            } else {
                0.0
            };
            let avg_total_ms =
                if s.total_ms_n > 0 { s.total_ms_sum as f64 / s.total_ms_n as f64 } else { 0.0 };
            let summary = json!({
                "requests": s.requests,
                "success": s.success,
                "failures": s.failures,
                "success_rate": success_rate,
                "avg_total_ms": avg_total_ms,
                "tool_calls": {"total": s.tc_total, "valid_json": s.tc_valid_json, "schema_ok": s.tc_schema_ok},
                "truncations": s.truncations,
                "degenerate": s.degenerate,
                "empty_responses": s.empty_responses,
            });
            let Some(suggested) = d.distill_tiers(id, &summary).await else {
                tracing::debug!(model = %id, "distill: jev call failed, will retry next cycle");
                continue;
            };
            let suggested = evolve_core::types::Tiers {
                reasoning: suggested[0],
                coding: suggested[1],
                vision: suggested[2],
                agentic: suggested[3],
            };
            // review#12 复查修复：merge 基准必须用首次蒸馏时固化的原始声明
            // （card.declared）——上一版 declared_base 存了但 merge 用的还是
            // 当前 catalog tiers（已是上轮 merged 值），多轮仍会阶梯漂移
            let declared_base = store
                .get(id)
                .map(|c| c.declared.clone())
                .unwrap_or_else(|| {
                    self.engine
                        .catalog_snapshot()
                        .into_iter()
                        .find(|m| m.id == *id)
                        .map(|m| m.tiers)
                        .unwrap_or_else(|| suggested.clone())
                });
            let merged = evolve_core::distill::conservative_merge(&declared_base, &suggested);
            let note = evolve_core::distill::stats_note(
                s.success,
                success_rate as f32,
                avg_total_ms,
                s.degenerate,
                s.empty_responses,
            );
            tracing::info!(model = %id, note = %note, ?merged, "distill: capability card updated");
            store.insert(
                id.clone(),
                evolve_core::distill::DistilledCard {
                    tiers: merged,
                    declared: declared_base,
                    note,
                    at: evolve_core::distill::now_ms(),
                    basis_success: s.success,
                    version: 1,
                },
            );
            changed += 1;
        }
        if changed > 0 {
            evolve_core::distill::save_store(&self.config.data.dir, &store);
            let mut models = self.engine.catalog_snapshot();
            evolve_core::distill::apply_distilled(&mut models, &store);
            self.engine.replace_catalog(models);
            tracing::info!(distilled = changed, "distill cycle complete, catalog updated");
        }
    }

    /// 重新扫描 agent 配置：新增模型合入 catalog，删除的保留（用户面板手动删）
    pub fn rescan_discovery(&self) {
        let discovered = evolve_discovery::discover(&self.config.discovery.agents);
        let removed: std::collections::HashSet<String> =
            load_overrides(&self.config.data.dir).removed_base_urls.into_iter().collect();
        let excluded = self.excluded_ids();
        let mut models = self.engine.catalog_snapshot();
        let mut added = 0usize;
        for d in discovered {
            if removed.contains(&d.base_url) {
                continue;
            }
            if !models.iter().any(|m| m.id == d.id) {
                if excluded.contains(&d.id) {
                    continue; // 协议不支持被剔除的模型不再复活（重启复位）
                }
                tracing::info!(model = %d.id, "discovery rescan: new model added");
                models.push(d);
                added += 1;
            }
        }
        if added > 0 {
            self.engine.replace_catalog(models);
            tracing::info!(added, "discovery rescan complete");
        }
    }

    /// 重拉远程 /models：新模型合入，已删的不动
    pub fn refresh_remote_models(&self) {
        let base_records: Vec<evolve_core::types::ModelRecord> = self.engine.catalog_snapshot();
        let self_origin = format!("http://127.0.0.1:{}", self.config.server.port);
        let fresh = evolve_discovery::remote::discover_remote_blocking(
            &base_records,
            &self.config.data.dir,
            Some(&self_origin),
        );
        let removed: std::collections::HashSet<String> =
            load_overrides(&self.config.data.dir).removed_base_urls.into_iter().collect();
        let excluded = self.excluded_ids();
        let mut models = base_records;
        let mut added = 0usize;
        for r in fresh {
            if removed.contains(&r.base_url) {
                continue;
            }
            if !models.iter().any(|m| m.id == r.id) {
                if excluded.contains(&r.id) {
                    continue; // 协议不支持被剔除的模型不再复活（重启复位）
                }
                models.push(r);
                added += 1;
            }
        }
        if added > 0 {
            self.engine.replace_catalog(models);
            tracing::info!(added, "remote models refresh: new models added");
        }
    }
}

#[cfg(test)]
mod provider_merge_tests {
    use super::*;
    use evolve_core::config::ProviderCfg;

    /// 自指条目（base_url 指向本机网关）不是上游 → 合并时跳过，不出现在 Provider 管理
    #[test]
    fn self_referential_provider_entry_is_skipped() {
        let dir = std::env::temp_dir().join(format!("er-selfprov-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let cfg = vec![
            ProviderCfg {
                key: "evo-router".into(),
                base_url: "http://127.0.0.1:8787/v1".into(),
                ..Default::default()
            },
            ProviderCfg {
                key: "real".into(),
                base_url: "https://example.test/v1".into(),
                ..Default::default()
            },
        ];
        let ov = merge_config_providers(&dir.to_string_lossy(), &cfg);
        assert!(!ov.providers.contains_key("evo-router"), "self-referential entry must not become a provider");
        assert!(ov.providers.contains_key("real"));
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// 配置文件里的 providers 必须以**配置优先**合并进 overrides，并保留来源标记
    #[test]
    fn config_providers_override_and_keep_provenance() {
        let dir = std::env::temp_dir().join(format!("er-cfgprov-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        // overrides.json 里已有同一 provider 的旧值
        std::fs::write(
            dir.join("overrides.json"),
            r#"{"providers":{"p1":{"base_url":"https://old.test/v1","tier":"lite"}}}"#,
        )
        .unwrap();
        let cfg = vec![
            ProviderCfg {
                key: "p1".into(),
                base_url: "https://new.test/v1/".into(),
                api_key: Some("k1".into()),
                tier: Some("max".into()),
                origin: Some("user".into()),
                source_path: Some("evo-router-config.jsonc".into()),
                ..Default::default()
            },
            ProviderCfg {
                key: "p2".into(),
                base_url: "https://agent.test/v1".into(),
                origin: Some("discovered".into()),
                source_path: Some("opencode @ ~/.config/opencode/opencode.jsonc".into()),
                ..Default::default()
            },
        ];
        let ov = merge_config_providers(&dir.to_string_lossy(), &cfg);
        assert_eq!(ov.providers["p1"].base_url, "https://new.test/v1", "config wins over overrides.json");
        assert_eq!(ov.providers["p1"].tier, "max");
        assert_eq!(ov.providers["p1"].api_key.as_deref(), Some("k1"));
        assert_eq!(ov.providers["p1"].origin.as_deref(), Some("user"));
        assert_eq!(
            ov.providers["p2"].source_path.as_deref(),
            Some("opencode @ ~/.config/opencode/opencode.jsonc"),
            "discovered entries carry their load path"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }
}

#[cfg(test)]
mod config_watch_tests {
    use super::*;

    #[test]
    fn fingerprint_ignores_only_the_decision_section() {
        // JSONC 与 TOML 两种写法指纹必须一致（同一份配置结构）
        let jsonc = "{\"server\":{\"port\":8787},\"decision\":{\"backend\":\"auto\"}}";
        let toml = "[server]\nport = 8787\n\n[decision]\nbackend = \"auto\"\n";
        assert_eq!(
            static_config_fingerprint(jsonc, Some(std::path::Path::new("a.jsonc"))),
            static_config_fingerprint(toml, Some(std::path::Path::new("a.toml"))),
        );
        // providers 段也是可热加载的（与 decision 同）：改它不要求重启
        let prov_changed = "{\"server\":{\"port\":8787},\"decision\":{\"backend\":\"auto\"},\"providers\":[{\"key\":\"p\",\"base_url\":\"https://x/v1\"}]}";
        assert_eq!(
            static_config_fingerprint(jsonc, Some(std::path::Path::new("a.jsonc"))),
            static_config_fingerprint(prov_changed, Some(std::path::Path::new("a.jsonc"))),
            "providers are hot-reloadable and must not ask for a restart"
        );
        // 判定段变化不触发重启；server 段变化触发
        let slots_changed = "{\"server\":{\"port\":8787},\"decision\":{\"backend\":\"auto\",\"slots\":[{\"kind\":\"jev\"}]}}";
        let port_changed = "{\"server\":{\"port\":9999},\"decision\":{\"backend\":\"auto\"}}";
        assert_eq!(
            static_config_fingerprint(jsonc, Some(std::path::Path::new("a.jsonc"))),
            static_config_fingerprint(slots_changed, Some(std::path::Path::new("a.jsonc"))),
            "decision-slot changes must stay hot-reloadable"
        );
        assert_ne!(
            static_config_fingerprint(jsonc, Some(std::path::Path::new("a.jsonc"))),
            static_config_fingerprint(port_changed, Some(std::path::Path::new("a.jsonc"))),
            "server changes must ask for a restart"
        );
    }
}

pub fn build_router(state: AppState) -> axum::Router {
    axum::Router::new()
        // favicon：面板浏览器标签图标（SVG 编译进二进制）
        .route("/favicon.svg", axum::routing::get(|| async {
            (
                [(axum::http::header::CONTENT_TYPE, "image/svg+xml")],
                include_bytes!("../assets/favicon.svg").to_vec(),
            )
        }))
        .route("/api/weight", axum::routing::post(crate::meta::api_weight))
        .route(
            "/api/policy-weights",
            axum::routing::get(crate::meta::api_policy_weights_get)
                .post(crate::meta::api_policy_weights_set),
        )
        .route("/api/trends", axum::routing::get(crate::meta::api_trends))
        .route("/api/providers",
            axum::routing::get(crate::meta::api_providers)
                .post(crate::meta::api_providers_set))
        .route("/api/providers/delete", axum::routing::post(crate::meta::api_providers_delete))
        .route("/api/providers/refresh", axum::routing::post(crate::meta::api_providers_refresh))
                .route("/api/providers/scan", axum::routing::post(crate::meta::api_providers_scan))
        .route("/api/providers/scan-progress", axum::routing::get(crate::meta::api_providers_scan_progress))
        .route("/api/providers/restore", axum::routing::post(crate::meta::api_providers_restore))
        .route("/api/providers/reconcile", axum::routing::post(crate::meta::api_providers_reconcile))
        .route("/api/health/clear", axum::routing::post(crate::meta::api_health_clear))
        .route("/api/model-tags", axum::routing::get(crate::meta::api_model_tags_get).post(crate::meta::api_model_tags_post))
        .route("/api/report", axum::routing::get(crate::meta::api_report))
        .route("/api/models/active", axum::routing::post(crate::meta::api_models_active))
        .route(
            "/api/plans",
            axum::routing::get(crate::meta::api_plans).post(crate::meta::api_plans_set),
        )
        .route("/v1/chat/completions", axum::routing::post(crate::relay::chat_completions))
        .route("/v1/models", axum::routing::get(crate::meta::list_models))
        .route("/healthz", axum::routing::get(crate::meta::healthz))
        .route("/api/health", axum::routing::get(crate::meta::api_health))
        .route("/api/stats", axum::routing::get(crate::meta::api_stats_query))
        .route("/api/live", axum::routing::get(crate::meta::api_live))
        .route("/api/reload", axum::routing::post(crate::meta::api_reload))
        .route("/api/config/export", axum::routing::post(crate::meta::api_config_export))
        .route("/api/feedback", axum::routing::post(crate::meta::api_feedback))
        .route("/api/quota", axum::routing::get(crate::meta::api_quota))
        .route("/api/events", axum::routing::get(crate::meta::api_events))
        .route("/api/benchmarks", axum::routing::get(crate::meta::api_benchmarks))
        .route("/api/stream", axum::routing::get(crate::meta::api_stream))
        .route("/", axum::routing::get(crate::meta::dashboard))
        .route("/v1/messages", axum::routing::post(crate::anthropic::messages))
        .route("/v1/messages/count_tokens", axum::routing::post(crate::anthropic::count_tokens))
        .with_state(state)
        .layer(axum::extract::DefaultBodyLimit::max(64 * 1024 * 1024))
}

/// 面板调控覆盖的持久化形态（data.dir/overrides.json）
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize, Default)]
pub struct Overrides {
    #[serde(default)]
    pub models: HashMap<String, f32>,
    /// normalized [quality, speed, cost, stability, headroom]
    #[serde(default)]
    pub weights: Option<[f32; 5]>,
    /// provider 配额方案订正（档位/方案/窗口说明）
    #[serde(default)]
    pub plans: HashMap<String, PlanOverride>,
    /// 手动新增/修改的 provider 定义（面板 Provider 管理卡写入口）
    #[serde(default)]
    pub providers: HashMap<String, ProviderDef>,
    /// 用户在面板主动删除的模型 id（定期扫描不再自动加回）
    #[serde(default)]
    pub deleted_models: Vec<String>,
    /// 用户在面板删除的发现 provider（按 base_url 记忆，重扫不加回；恢复即移出此表）
    #[serde(default)]
    pub removed_base_urls: Vec<String>,
    /// 活跃集手动覆盖（2026-10 用户裁决：默认活跃=最近发布/最近的 50 个）：
    /// include = 手动开启（顶掉最低分），exclude = 手动停用
    #[serde(default)]
    pub active_models: ActiveOverride,
    /// 模型标签用户覆盖（2026-10）：model_id → {capability, price_tier}
    /// ——默认自动推导（tags::default_tags），此处仅存用户显式修改
    #[serde(default)]
    pub model_tags: HashMap<String, TagOverride>,
}

/// 活跃集覆盖
#[derive(Debug, Clone, Default, serde::Serialize, serde::Deserialize)]
pub struct ActiveOverride {
    #[serde(default)]
    pub include: Vec<String>,
    #[serde(default)]
    pub exclude: Vec<String>,
}

/// 标签覆盖（可只改一项）
#[derive(Debug, Clone, Default, serde::Serialize, serde::Deserialize)]
pub struct TagOverride {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub capability: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub price_tier: Option<String>,
}

/// 手动 provider 定义——用户在面板新增/修改，与扫描发现并存
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize, Default)]
pub struct ProviderDef {
    pub base_url: String,
    pub api_key: Option<String>,
    /// 身份指纹（用户裁决：apiKey+baseUrl 唯一标识 provider）。编辑发现
    /// provider 而未重填 key 时回填其现有 key 指纹——同端点不同 key 的账号
    /// （如 opencode-go 与 opencode-go-github）不因此被遮蔽
    #[serde(default)]
    pub key_fp: Option<String>,
    /// openai / anthropic
    #[serde(default = "default_protocol")]
    pub protocol: String,
    /// coding / agent / go / api
    #[serde(default = "default_plan_kind")]
    pub plan_kind: String,
    #[serde(default)]
    pub docs_url: String,
    /// 档位：lite/pro/max/go/go-plus 等
    #[serde(default)]
    pub tier: String,
    /// 来源标记（配置文件里的 origin）：user（你写的/面板改的）| discovered（agent 配置）
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub origin: Option<String>,
    /// origin=discovered 时的**加载路径**（如 "opencode @ ~/.config/opencode/opencode.jsonc"）
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub source_path: Option<String>,
    /// 各窗口额度（积分/次数）——5h/weekly/monthly
    #[serde(default)]
    pub window_5h: Option<f64>,
    #[serde(default)]
    pub window_weekly: Option<f64>,
    #[serde(default)]
    pub window_monthly: Option<f64>,
    /// 模型级系数表（"model|in,cached,out" 多行）
    #[serde(default)]
    pub model_rates: String,
    /// 远程拉取到的模型（缓存）
    #[serde(default)]
    pub fetched_models: Vec<String>,
    /// 删除标记（软删除，面板可恢复）
    #[serde(default)]
    pub disabled: bool,
}

fn default_protocol() -> String { "openai".into() }
fn default_plan_kind() -> String { "api".into() }

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize, Default)]
pub struct PlanOverride {
    #[serde(default)]
    pub tier: Option<String>,
    #[serde(default)]
    pub scheme: Option<String>,
    #[serde(default)]
    pub windows: Option<String>,
}

pub fn overrides_path(data_dir: &str) -> std::path::PathBuf {
    evolve_core::config::state_file(data_dir, "overrides.json")
}

pub fn load_overrides(data_dir: &str) -> Overrides {
    std::fs::read_to_string(overrides_path(data_dir))
        .ok()
        .and_then(|t| serde_json::from_str(&t).ok())
        .unwrap_or_default()
}

pub fn save_overrides(data_dir: &str, o: &Overrides) {
    if let Ok(t) = serde_json::to_string_pretty(o) {
        let path = overrides_path(data_dir);
        evolve_core::config::ensure_parent(&path);
        let _ = std::fs::write(path, t);
    }
}

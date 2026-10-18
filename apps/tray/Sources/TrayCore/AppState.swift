import Foundation
import SwiftUI

public struct RecentDecision: Identifiable, Equatable {
    public init(model: String, status: Int, totalMs: Int, ttftMs: Int?, tokS: Double?, agent: String, time: String, preprocessMs: Int = 0, decisionMs: Int = 0, totalTokens: Int = 0, cachePct: Int? = nil, failReason: String? = nil, capability: String? = nil, tags: DecisionTags? = nil) {
        self.model = model; self.status = status; self.totalMs = totalMs
        self.ttftMs = ttftMs; self.tokS = tokS; self.agent = agent; self.time = time
        self.preprocessMs = preprocessMs; self.decisionMs = decisionMs
        self.totalTokens = totalTokens; self.cachePct = cachePct; self.failReason = failReason
        self.capability = capability; self.tags = tags
    }
    public let id = UUID()
    public let model: String
    public let status: Int
    public let totalMs: Int
    public let ttftMs: Int?
    public let tokS: Double?
    public let agent: String
    public let time: String
    public let preprocessMs: Int
    public let decisionMs: Int
    public let totalTokens: Int
    public let cachePct: Int?
    public let failReason: String?
    /// 模型能力档位（服务端 model_tags，决策时点快照）
    public let capability: String?
    /// 服务端派生判定标签（难度/域/特征/来源/匹配）——单一事实源
    public let tags: DecisionTags?
}

/// 事件里的 `tags`（evo-server/src/decision_tags.rs 派生）：面板只渲染，不重算
public struct DecisionTags: Equatable {
    public let difficulty: String?
    public let domain: String?
    public let flags: [String]
    public let mode: String?
    public let fit: String?
    public init(difficulty: String?, domain: String?, flags: [String], mode: String?, fit: String?) {
        self.difficulty = difficulty; self.domain = domain
        self.flags = flags; self.mode = mode; self.fit = fit
    }
    public init?(json: [String: Any]?) {
        guard let json else { return nil }
        let mode = json["mode"] as? String
        let diff = json["difficulty"] as? String
        let dom = json["domain"] as? String
        let fit = json["fit"] as? String
        if mode == nil && diff == nil && dom == nil { return nil }
        self.difficulty = diff
        self.domain = dom
        self.flags = (json["flags"] as? [String]) ?? []
        self.mode = mode
        self.fit = fit
    }
}

public struct PaygBalance: Equatable {
    public let currency: String
    public let total: Double
    public let available: Bool
    public init(currency: String, total: Double, available: Bool) {
        self.currency = currency; self.total = total; self.available = available
    }
}

public struct ChatResult: Equatable {
    public let decisionId: String
    public let status: Int
    public let ts: Date
    public init(decisionId: String, status: Int, ts: Date) {
        self.decisionId = decisionId; self.status = status; self.ts = ts
    }
}

public struct ProviderRow: Identifiable, Equatable {
    public init(key: String, enabled: Bool, available: Bool, cooldownUntil: Date?, cooldownCause: String?, pct: Int?, planKind: String, tokensWin: Int = 0, avgTtftMs: Int? = nil, avgRate: Double? = nil, cachePctWin: Int? = nil, paygBalance: PaygBalance? = nil, quotaUsed: Int? = nil, quotaScope: String? = nil, quotaExhausted: Bool = false, quotaReset: Date? = nil, cooldownModel: String? = nil, quotaSource: String = "", blockedModels: [String] = [], mergedFrom: [String] = [], tier: String = "", modelCount: Int = 0, freeModels: Int = 0, needsKey: Bool = false, keyURL: String = "", issueLabel: String = "", nextProbeMs: Int = 0,
                tierEffective: String = "") {
        self.key = key; self.enabled = enabled; self.available = available
        self.cooldownUntil = cooldownUntil; self.cooldownCause = cooldownCause
        self.pct = pct; self.planKind = planKind
        self.tokensWin = tokensWin; self.avgTtftMs = avgTtftMs; self.avgRate = avgRate; self.cachePctWin = cachePctWin
        self.paygBalance = paygBalance
        self.quotaUsed = quotaUsed; self.quotaScope = quotaScope
        self.quotaExhausted = quotaExhausted; self.quotaReset = quotaReset
        self.cooldownModel = cooldownModel
        self.quotaSource = quotaSource
        self.mergedFrom = mergedFrom
        self.blockedModels = blockedModels
        self.tier = tier
        self.modelCount = modelCount
        self.freeModels = freeModels
        self.needsKey = needsKey
        self.keyURL = keyURL
        self.issueLabel = issueLabel
        self.nextProbeMs = nextProbeMs
        self.tierEffective = tierEffective
    }
    public let key: String
    public let enabled: Bool
    public let available: Bool
    public let cooldownUntil: Date?
    public let cooldownCause: String?
    /// 冷却中剩余最久的模型 id（provider 级聚合来源，展示用）
    public let cooldownModel: String?
    /// w2 配额信息源：official（官方 API/响应头）/ official_headers / estimated（网关推算）/ ""
    public let quotaSource: String
    /// 合并行的来源账号（服务端 merged_from；窗口指标跨账号聚合用）
    public let mergedFrom: [String]
    /// w3 永久黑名单模型（上游 404 且不在模型列表）——仅 Web 手动恢复
    public let blockedModels: [String]
    public let pct: Int?
    public let planKind: String
    /// 方案档位（lite/pro/max/go…）——"按档位管理 provider"的展示依据
    public let tier: String
    /// 该 provider 当前可用模型数（来自 /models 拉取；0 = 未拉取/未暴露）
    public let modelCount: Int
    /// 免费模型数（命名带 free 段：zen 的 -free / OpenRouter 的 :free）——
    /// 路由优先且不消耗套餐额度；>0 时卡片上标注
    public let freeModels: Int
    /// 缺 API key（服务端判定）→ 卡片提示"去申请"+ 不参与路由
    public let needsKey: Bool
    /// 申请 API key 的官方入口（默认接入的 provider 才有；空 = 无可展示）
    public let keyURL: String
    /// provider 级降级原因（网络/鉴权；空 = 正常）→ 卡片显示 `degraded · next probe`
    public let issueLabel: String
    /// 下次探活倒计时（ms）——"降级不是永久沉默"的可见证据
    public let nextProbeMs: Int
    /// 生效档位（服务端 tier_effective）：本条目显式值 → 方案级 override（不继承同方案账号）
    public let tierEffective: String
    public let tokensWin: Int
    public let avgTtftMs: Int?
    public let avgRate: Double?
    public let cachePctWin: Int?
    public let paygBalance: PaygBalance?
    public let quotaUsed: Int?
    public let quotaScope: String?
    public let quotaExhausted: Bool
    public let quotaReset: Date?
    public var id: String { key }
}

public enum IconState { case idle, busy, success, fail, stopped, starting }

/// 模型维度行（Providers 卡切到 Model 标签）：同一窗口的逐模型指标，
/// 与 Provider 行同布局（dot + 名称 + tokens/hit/速率/TTFT）
/// 模型级健康冷却（/api/health）：剩余时长 + 原因 + 是否属于 provider/账户级
public struct ModelCoolState: Equatable {
    public let ms: Int
    public let message: String
    /// quota / payment（账户或方案级）→ 属于 provider 行，不在 model 行重复
    public let accountLevel: Bool
}

/// 模型实时打标 + 可用性（服务端 /api/model-tags；单一事实源）
public struct ModelTagInfo: Equatable {
    public let capability: String
    public let priceTier: String
    /// nil = 可用；否则是服务端给的原因（no-key / deleted）
    public let unusableReason: String?
}

public struct ModelRow: Identifiable, Equatable {
    public let id: String
    public let tok: Int
    public let hit: Int?
    public let rate: Double?
    public let ttft: Int?
    public let requests: Int
    /// 模型打标（能力/价位，实时口径）——仅 today 视图展示（yesterday 的标签是当前态，无意义）
    public let capability: String?
    public let priceTier: String?

    /// 失败率（0-100；requests>0 时才有意义）
    public let failPct: Int?
    /// 不可用原因（no-key / deleted…，nil = 可用）
    public let unusableReason: String?
    /// 健康冷却剩余（ms；>0 = 冷却中）
    public let coolingMs: Int
    /// 冷却原因（**仅非账户级**才带；quota/payment 属 provider 行，此处为 nil）
    public let coolMsg: String?

    public init(id: String, tok: Int, hit: Int?, rate: Double?, ttft: Int?, requests: Int,
                capability: String? = nil, priceTier: String? = nil,
                failPct: Int? = nil, unusableReason: String? = nil, coolingMs: Int = 0,
                coolMsg: String? = nil) {
        self.id = id; self.tok = tok; self.hit = hit
        self.rate = rate; self.ttft = ttft; self.requests = requests
        self.capability = capability; self.priceTier = priceTier
        self.failPct = failPct; self.unusableReason = unusableReason; self.coolingMs = coolingMs
        self.coolMsg = coolMsg
    }
}

@MainActor
public final class AppState: ObservableObject {
    public init() {}

    static func payg(from row: [String: Any]) -> PaygBalance? {
        guard let b = row["payg_balance"] as? [String: Any],
              let total = (b["total"] as? Double) ?? (b["total"] as? String).flatMap(Double.init) else { return nil }
        let currency = (b["currency"] as? String) ?? ""
        let avail = (b["is_available"] as? Bool) ?? true
        return PaygBalance(currency: currency, total: total, available: avail)
    }
    @Published public var healthy = true
    @Published public var external = false
    @Published public var starting = false
    @Published public var inflight = 0
    /// 决策模型可用性：ok / stale（已配置待重启）/ missing（未配置）——/api/live 每次 1s 轮询现读 env
    @Published public var decisionStatus = "ok"
    /// 网关版本号（/api/live 下发；展示用，空串 = 未取到）
    @Published public var version = ""
    /// 配置热加载状态（/api/live 下发）：非 [decision] 段落改动 → 面板提示重启
    /// 在飞请求明细（/api/live 的 inflight_detail）：model / session / age_ms
    @Published public var inflightDetail: [[String: Any]] = []
    @Published public var configRestartRequired = false
    @Published public var configRestartReason: String?
    @Published public var configReloadedAt: Int64?
    /// 当前实际生效的判定槽位（如 ["jev"]；空 = 纯启发式）
    @Published public var activeSlots: [String] = []
    /// 最近一次槽位失败的人读原因（如 "timeout" / "auth failed (401/403)"；空 = 没失败过/已恢复）
    @Published public var decisionFailReason: String = ""
    /// 瞬态失败后的指数回退剩余（ms；0 = 不在冷却）/ 连续失败次数——面板显示"自动重试中"
    @Published public var decisionRetryInMs: Int = 0
    @Published public var decisionFails: Int = 0
    /// 决策模型配置的**路径**（backend 字符串：auto / decision_model / heuristic …）
    /// —— 头行显示"配置了什么"，而不是"最近一次发生了什么"（后者在每条 request 的行内标签）
    @Published public var decisionBackend: String = "auto"
    /// 配额告警阈值（服务端 plan_soft_pct；与 Web 面板同源，默认 60）
    @Published public var softPct = 60
    /// 指标窗口：true = 今天（本地日历日），false = 昨天（用户裁决，面板内可切换）
    @Published public var statWindowToday = true
    /// Providers 卡维度：false = Provider（默认），true = Model（逐模型统计）
    @Published public var modelDim = false
    /// Model 维度行（当前窗口、按 tokens 降序、前 6 行）
    @Published public var modelRows: [ModelRow] = []
    /// 模型实时打标（/api/model-tags；id → (capability, price_tier)）
    @Published public var modelTagMap: [String: ModelTagInfo] = [:]
    /// 模型级健康冷却（/api/health）：model → 冷却态（原因 + 是否账户级）
    @Published public var modelCooling: [String: ModelCoolState] = [:]

    @Published public var pulse = false
    @Published public var requests = 0
    @Published public var successRate = 0
    @Published public var tokensK = "-"
    @Published public var recent: [RecentDecision] = []
    @Published public var providers: [ProviderRow] = []
    @Published public var lastSync: Date?
    @Published public var lastResult: ChatResult?
    private var seenDecisionIDs = Set<String>()

    public var iconState: IconState {
        if !healthy { return .stopped }
        if starting { return .starting }
        if inflight > 0 { return .busy }  // busy 期间用绿色 ER（用户裁决：路由中才显示）
        if let r = lastResult, !(200...299).contains(r.status) { return .fail }  // idle+失败 → 黄 ER；新请求会清掉 lastResult
        return .idle
    }

    /// 新事件钩子：去重后把"上次结果"写回 lastResult（busy 中仍展示旧颜色）
    public func noteChatEvent(decisionId: String, status: Int) {
        guard !seenDecisionIDs.contains(decisionId) else { return }
        seenDecisionIDs.insert(decisionId)
        // 容量保护：清掉太久的事件 id（保留最近 256 个）
        if seenDecisionIDs.count > 256 {
            seenDecisionIDs = Set(seenDecisionIDs.suffix(192))
        }
        lastResult = ChatResult(decisionId: decisionId, status: status, ts: Date())
    }

    /// 仅值变化时写入 @Published——每秒轮询若无条件写新值，unchanged 数据也会
    /// 让整棵 body 重建；长跑 6h+ 后触发 SwiftUI AttributeGraph 表爆炸
    /// （SIGABRT / AG grow_region，2026-10-18 05:08 实证）
    private func upd<T: Equatable>(_ kp: ReferenceWritableKeyPath<AppState, T>, _ value: T) {
        if self[keyPath: kp] != value { self[keyPath: kp] = value }
    }

    /// 高频轻量轮询（1s）：in-flight 计数 + 健康，驱动图标与"进行中"
    public func pollLive() async {
        let gw = GatewayProcess.shared
        if case (200, let data)? = try? await get("/api/live"),
           let obj = try? JSONSerialization.jsonObject(with: data) as? [String: Any] {
            upd(\.inflight, (obj["inflight"] as? Int) ?? Int((obj["inflight"] as? Double) ?? 0))
            inflightDetail = (obj["inflight_detail"] as? [[String: Any]]) ?? []
            upd(\.decisionStatus, ((obj["decision"] as? [String: Any])?["status"] as? String) ?? "ok")
            upd(\.version, (obj["version"] as? String) ?? version)
            let cfgObj = obj["config"] as? [String: Any]
            upd(\.configRestartRequired, (cfgObj?["restart_required"] as? Bool) ?? false)
            upd(\.configRestartReason, cfgObj?["restart_reason"] as? String)
            upd(\.configReloadedAt, (cfgObj?["reloaded_at_ms"] as? Double).map { Int64($0) })
            upd(\.activeSlots, (cfgObj?["active_slots"] as? [String]) ?? [])
            let decObj = obj["decision"] as? [String: Any]
            upd(\.decisionBackend, (decObj?["backend"] as? String) ?? decisionBackend)
            upd(\.decisionFailReason, ((decObj?["last_fail"] as? [String: Any])?["label"] as? String) ?? "")
            upd(\.decisionRetryInMs, Int((decObj?["retry_in_ms"] as? Double) ?? 0))
            upd(\.decisionFails, Int((decObj?["consecutive_fails"] as? Double) ?? 0))
        } else {
            upd(\.inflight, 0)
        }
        let net = await gw.networkHealthy()
        upd(\.healthy, net)
        upd(\.starting, gw.starting || (!net && gw.isChildAlive))
        upd(\.external, !gw.owned && net)
        // 自愈：网关不在且非外部实例 → 周期性尝试拉起（ensureRunning 内部 10s 节流）
        if !net && !external { await gw.ensureRunning() }
    }

    /// 低频明细轮询（3s）：统计/Provider/事件
    public func pollData() async {
        guard healthy else { return }
        var models: [String: Any] = [:]
        // 指标窗口（用户裁决）：**今天 / 昨天**（日历口径，各有上下界），顶部指标卡
        // 与 Providers 行共用；可点击切换。服务端不引时区库 → 面板按本地时区算好
        // 两端 epoch，用 `window=range:<s>-<e>` 传。
        let cal = Calendar.current
        let dayStart = cal.startOfDay(for: Date())
        let startMs: Int64
        let endMs: Int64
        if statWindowToday {
            startMs = Int64(dayStart.timeIntervalSince1970 * 1000)
            endMs = Int64(Date().timeIntervalSince1970 * 1000)
        } else {
            let yStart = cal.date(byAdding: .day, value: -1, to: dayStart) ?? dayStart
            startMs = Int64(yStart.timeIntervalSince1970 * 1000)
            endMs = Int64(dayStart.timeIntervalSince1970 * 1000)
        }
        // 模型级健康（冷却中/原因）：Model 行显示报错状态用；与 stats 同轮询
        if case (200, let hData)? = try? await get("/api/health"),
           let hObj = try? JSONSerialization.jsonObject(with: hData) as? [String: Any],
           let hModels = hObj["models"] as? [String: Any] {
            var cool: [String: ModelCoolState] = [:]
            for (mid, hv) in hModels {
                guard let h = hv as? [String: Any] else { continue }
                // ⚠ 服务端字段是 cooldown_remaining_ms（此前误读 until_epoch_ms → 永远 0）
                let ms = (h["cooldown_remaining_ms"] as? Int)
                    ?? Int((h["cooldown_remaining_ms"] as? Double) ?? 0)
                let available = (h["available"] as? Bool) ?? (ms <= 0)
                guard !available || ms > 0 else { continue }
                let kind = (h["kind"] as? String) ?? ""
                let msg = (h["message"] as? String) ?? ""
                // 账户/方案级（配额、付款余额）→ 属于 provider 行，model 行不重复
                let accountLevel = kind.contains("quota") || kind.contains("payment")
                    || msg.lowercased().contains("quota")
                cool[mid] = ModelCoolState(ms: max(ms, 0), message: msg, accountLevel: accountLevel)
            }
            upd(\.modelCooling, cool)
        }
        if case (200, let data)? = try? await get("/api/stats?window=range:\(startMs)-\(endMs)"),
           let obj = try? JSONSerialization.jsonObject(with: data) as? [String: Any] {
            models = (obj["models"] as? [String: Any]) ?? obj
            // 模型实时打标（能力/价位）：Providers 卡 Model 标签展示用
            if case (200, let mtData)? = try? await get("/api/model-tags"),
               let mtObj = try? JSONSerialization.jsonObject(with: mtData) as? [String: Any],
               let mtArr = mtObj["models"] as? [[String: Any]] {
                var tm: [String: ModelTagInfo] = [:]
                for m in mtArr {
                    guard let id = m["model"] as? String else { continue }
                    tm[id] = ModelTagInfo(
                        capability: (m["capability"] as? String) ?? "",
                        priceTier: (m["price_tier"] as? String) ?? "",
                        unusableReason: ((m["usable"] as? Bool) == false)
                            ? ((m["unusable_reason"] as? String) ?? "unusable") : nil)
                }
                upd(\.modelTagMap, tm)
            }
            var reqs = 0, ok = 0, toks = 0
            for (_, s) in models {
                guard let s = s as? [String: Any] else { continue }
                reqs += (s["requests"] as? Int) ?? Int((s["requests"] as? Double) ?? 0)
                ok += (s["success"] as? Int) ?? Int((s["success"] as? Double) ?? 0)
                toks += (s["prompt_tokens"] as? Int) ?? Int((s["prompt_tokens"] as? Double) ?? 0)
                toks += (s["completion_tokens"] as? Int) ?? Int((s["completion_tokens"] as? Double) ?? 0)
            }
            upd(\.requests, reqs)
            upd(\.successRate, reqs > 0 ? Int(round(100.0 * Double(ok) / Double(reqs))) : 0)
            upd(\.tokensK, Self.fmtTok(toks))
        }
        // Model 维度行（Providers 卡 Model 标签）：同一窗口的逐模型指标，
        // 与 Provider 行同布局，按 tokens 降序前 6 行
        var mrows: [ModelRow] = []
        for (mid, msAny) in models {
            guard let ms = msAny as? [String: Any] else { continue }
            let req = (ms["requests"] as? Int) ?? Int((ms["requests"] as? Double) ?? 0)
            let pin = (ms["prompt_tokens"] as? Int) ?? Int((ms["prompt_tokens"] as? Double) ?? 0)
            let cached = (ms["cached_tokens"] as? Int) ?? Int((ms["cached_tokens"] as? Double) ?? 0)
            let out = (ms["completion_tokens"] as? Int) ?? Int((ms["completion_tokens"] as? Double) ?? 0)
            let tok = pin + out
            // 只展示**用过的模型**（用户裁决 2026-10-17）：窗口内没有任何调用、
            // 也没有产出的目录模型不进这一列（它是用量视图，不是目录清单）
            if tok <= 0 && req <= 0 { continue }
            let fails = (ms["failures"] as? Int) ?? Int((ms["failures"] as? Double) ?? 0)
            let hit = pin > 0 ? Int(round(100.0 * Double(cached) / Double(pin))) : nil
            let ttft = (ms["avg_ttft_ms"] as? Int) ?? (ms["avg_ttft_ms"] as? Double).map(Int.init)
            let rate = (ms["avg_rate_tok_s"] as? Double) ?? (ms["avg_rate_tok_s"] as? Int).map(Double.init)
            let failPct: Int? = req > 0 ? Int((Double(fails) * 100.0 / Double(req)).rounded()) : nil
            mrows.append(ModelRow(id: mid, tok: tok, hit: hit, rate: rate, ttft: ttft, requests: req,
                                  failPct: failPct))
        }
        mrows.sort { if $0.tok != $1.tok { return $0.tok > $1.tok } ; return $0.id < $1.id }
        // 模型打标（实时口径）：仅 today 视图挂到行上（yesterday 的标签是当前态，无意义）
        // 上限 50（可在面板内滚动查看）：仍只含用过的模型
        let mappedRows: [ModelRow] = mrows.prefix(50).map { m in
            let tag = statWindowToday ? modelTagMap[m.id] : nil
            return ModelRow(id: m.id, tok: m.tok, hit: m.hit, rate: m.rate, ttft: m.ttft,
                            requests: m.requests, capability: tag?.capability, priceTier: tag?.priceTier,
                            failPct: m.failPct,
                            // 只保留结构性不可用（no-key / deleted）；"cooldown"/
                            // "quota-gated" 是健康的派生结果，交给下面的冷却原因显示
                            unusableReason: tag?.unusableReason.flatMap { r in
                                (r == "no-key" || r == "deleted") ? r : nil
                            },
                            coolingMs: modelCooling[m.id]?.ms ?? 0,
                            coolMsg: modelCooling[m.id].flatMap { $0.accountLevel ? nil : $0.message })
        }
        upd(\.modelRows, mappedRows)

        var rows: [ProviderRow] = []
        // 派生量（配额窗口/绑定窗口/冷却/黑名单/可用性）全部由服务端算好——macOS
        // 面板与 web 面板同源，客户端只渲染（消除两端各自计算的口径漂移）
        if case (200, let data)? = try? await get("/api/providers"),
           let wrapper = try? JSONSerialization.jsonObject(with: data) as? [String: Any],
           let arr = wrapper["providers"] as? [[String: Any]] {
            // 配额告警阈值与 Web 同源：**逐行** soft_pct（= 服务端 plan_soft_pct）。
            // 顶层没有该字段（旧代码在此读 wrapper["soft_pct"]，永不命中，已删）
            for p in arr {
                if let sp = (p["soft_pct"] as? Double) ?? (p["soft_pct"] as? Int).map(Double.init) {
                    upd(\.softPct, Int(sp))
                }
                let key = (p["key"] as? String) ?? "-"
                let kind = (p["plan_kind"] as? String) ?? "-"
                let disabled = (p["disabled"] as? Bool) ?? false
                // 冷却/黑名单/可用性/配额——服务端派生（merged_from 聚合也在服务端）
                let available = (p["available"] as? Bool) ?? true
                let cooling = p["cooling"] as? [String: Any]
                let coolMs = (cooling?["remaining_ms"] as? Int)
                    ?? (cooling?["remaining_ms"] as? Double).map(Int.init)
                let cooldownUntil = coolMs.map { Date().addingTimeInterval(Double($0) / 1000.0) }
                let cooldownCause = cooling?["cause"] as? String
                let cooldownModel = (cooling?["model"] as? String).flatMap { AppState.shortModel($0, key: key) }
                let blockedModels = ((p["blocked_models"] as? [Any])?.compactMap { $0 as? String } ?? [])
                    .map { AppState.shortModel($0, key: "") }
                let binding = p["binding"] as? [String: Any]
                let usedPct = (binding?["used_pct"] as? Double).map { Int($0.rounded()) }
                let quotaScope = binding?["scope"] as? String
                let quotaExhausted = (binding?["exhausted"] as? Bool) ?? false
                let quotaReset = (binding?["reset_epoch_ms"] as? Double)
                    .map { Date(timeIntervalSince1970: $0 / 1000) }
                let mergedKeys = ((p["merged_from"] as? [Any])?.compactMap { $0 as? String }) ?? [key]
                let tier = (p["tier"] as? String) ?? ""
                let modelCount: Int = {
                    if let n = p["model_count"] as? Int { return n }
                    if let fm = p["fetched_models"] as? [String] { return fm.count }
                    if let fm = p["fetched_models"] as? String {
                        return fm.split(separator: ",").filter { !$0.trimmingCharacters(in: .whitespaces).isEmpty }.count
                    }
                    return 0
                }()
                rows.append(ProviderRow(key: key, enabled: !disabled, available: available,
                                        cooldownUntil: cooldownUntil, cooldownCause: cooldownCause, pct: usedPct,
                                        planKind: kind,
                                        paygBalance: AppState.payg(from: p),
                                        quotaUsed: usedPct, quotaScope: quotaScope,
                                        quotaExhausted: quotaExhausted, quotaReset: quotaReset,
                                        cooldownModel: cooldownModel,
                                        quotaSource: (p["quota_source"] as? String) ?? "",
                                        blockedModels: blockedModels,
                                        mergedFrom: mergedKeys,
                                        tier: tier,
                                        modelCount: modelCount,
                                        freeModels: (p["free_models"] as? Int) ?? 0,
                                        needsKey: (p["needs_key"] as? Bool) ?? false,
                                        keyURL: (p["key_url"] as? String) ?? "",
                                        issueLabel: ((p["provider_issue"] as? [String: Any])?["issue"] as? String) ?? "",
                                        nextProbeMs: ((p["provider_issue"] as? [String: Any])?["next_probe_ms"] as? Int) ?? 0,
                                        tierEffective: (p["tier_effective"] as? String) ?? (p["tier"] as? String) ?? ""))
            }
        }
        // per-provider 5h 指标：stats 的 model key 形如 "provider/model"，按前缀聚合
        var agg: [String: (tok: Int, ttftW: Double, ttftN: Int, rateW: Double, rateN: Int, cacheSum: Int, promptSum: Int)] = [:]
        for (mid, msAny) in models {
            guard let ms = msAny as? [String: Any] else { continue }
            let prov = mid.split(separator: "/").first.map(String.init) ?? mid
            var e = agg[prov] ?? (0, 0.0, 0, 0.0, 0, 0, 0)
            let req = (ms["requests"] as? Int) ?? Int((ms["requests"] as? Double) ?? 0)
            let pin = (ms["prompt_tokens"] as? Int) ?? Int((ms["prompt_tokens"] as? Double) ?? 0)
            let pout = (ms["completion_tokens"] as? Int) ?? Int((ms["completion_tokens"] as? Double) ?? 0)
            e.tok += pin + pout
            if let t = (ms["avg_ttft_ms"] as? Double) ?? (ms["avg_ttft_ms"] as? Int).map(Double.init) {
                e.ttftW += t * Double(req); e.ttftN += req
            }
            if let r = (ms["avg_rate_tok_s"] as? Double) ?? (ms["avg_rate_tok_s"] as? Int).map(Double.init) {
                e.rateW += r * Double(req); e.rateN += req
            }
            e.cacheSum += (ms["cached_tokens"] as? Int) ?? Int((ms["cached_tokens"] as? Double) ?? 0)
            e.promptSum += pin
            agg[prov] = e
        }
        // 合并行（服务端按 key_fp 并账号，merged_from 记录来源账号）的窗口指标
        // 须跨来源账号聚合——stats 的 model 前缀是各账号自己的 provider 名
        let mergedBy: [String: [String]] = rows.reduce(into: [:]) { acc, r in
            acc[r.key] = r.mergedFrom.isEmpty ? [r.key] : r.mergedFrom
        }
        rows = rows.map { r in
            let keys = mergedBy[r.key] ?? [r.key]
            // 跨来源账号求和（tokens/hit/速率/TTFT）
            let e = keys.reduce(nil as (tok: Int, ttftW: Double, ttftN: Int, rateW: Double, rateN: Int, cacheSum: Int, promptSum: Int)?) { acc, k in
                guard let x = agg[k] else { return acc }
                guard var a = acc else { return x }
                a.tok += x.tok; a.ttftW += x.ttftW; a.ttftN += x.ttftN
                a.rateW += x.rateW; a.rateN += x.rateN
                a.cacheSum += x.cacheSum; a.promptSum += x.promptSum
                return a
            }
            guard let e else { return r }
            let cachePct: Int? = e.promptSum > 0 ? Int(round(100.0 * Double(e.cacheSum) / Double(e.promptSum))) : nil
            // 配额/冷却等派生量已在服务端算好（见上），此处只叠加 /api/stats 的指标
            // ⚠ 具名参数也必须按声明顺序（Swift 位置化解析）——新增字段一律放末尾
            return ProviderRow(key: r.key, enabled: r.enabled, available: r.available,
                               cooldownUntil: r.cooldownUntil, cooldownCause: r.cooldownCause,
                               pct: r.pct, planKind: r.planKind,
                               tokensWin: e.tok,
                               avgTtftMs: e.ttftN > 0 ? Int(e.ttftW / Double(e.ttftN)) : nil,
                               avgRate: e.rateN > 0 ? e.rateW / Double(e.rateN) : nil,
                               cachePctWin: cachePct,
                               paygBalance: r.paygBalance,
                               quotaUsed: r.quotaUsed, quotaScope: r.quotaScope,
                               quotaExhausted: r.quotaExhausted, quotaReset: r.quotaReset,
                               cooldownModel: r.cooldownModel,
                               quotaSource: r.quotaSource,
                               blockedModels: r.blockedModels,
                               mergedFrom: r.mergedFrom,
                               tier: r.tier, modelCount: r.modelCount,
                               freeModels: r.freeModels, needsKey: r.needsKey, keyURL: r.keyURL,
                               issueLabel: r.issueLabel, nextProbeMs: r.nextProbeMs,
                               tierEffective: r.tierEffective)
        }
        // 按**当前窗口的 token 用量降序**（用户裁决）——须在窗口指标聚合后排序
        // （此前用 24h 累计字段、且在聚合前排，与面板显示口径不一致）；
        // 用量并列时启用者优先、再按名字稳定排序
        rows.sort {
            if $0.tokensWin != $1.tokensWin { return $0.tokensWin > $1.tokensWin }
            if $0.enabled != $1.enabled { return $0.enabled }
            return $0.key < $1.key
        }
        // 上限 12（面板内滚动；此前 6 条是为了不撑高面板）
        upd(\.providers, Array(rows.prefix(12)))
        // 最近 10 天（用户裁决 2026-10-17）：窗口过滤交给服务端（kind=chat + since_ms），
        // 否则 limit 会被非 chat 事件吃掉、只回几小时的数据；面板拿到的就是"10 天内
        // 最近 60 条请求"，由 PanelView 放在固定高度的 ScrollView 里滚动查看
        let since10d = Int(Date().timeIntervalSince1970 * 1000) - 10 * 24 * 60 * 60 * 1000
        if case (200, let data)? = try? await get("/api/events?kind=chat&since_ms=\(since10d)&limit=60"),
           let arr = try? JSONSerialization.jsonObject(with: data) as? [[String: Any]] {
            var decRows: [RecentDecision] = []
            for ev in arr.reversed() where (ev["kind"] as? String) == "chat" {
                let model = (ev["chosen"] as? String) ?? "-"
                let status = (ev["status"] as? Int) ?? Int((ev["status"] as? Double) ?? 0)
                let total = (ev["total_ms"] as? Int) ?? Int((ev["total_ms"] as? Double) ?? 0)
                let ttft = (ev["ttft_ms"] as? Int) ?? Int((ev["ttft_ms"] as? Double) ?? -1)
                let out = (ev["completion_tokens"] as? Int) ?? Int((ev["completion_tokens"] as? Double) ?? 0)
                let gen = max(1, total - max(0, ttft))
                let tokS = out > 0 ? Double(out) / (Double(gen) / 1000.0) : nil
                let agent = ((ev["agent"] as? String) ?? "-").split(separator: "/").first.map(String.init) ?? "-"
                let time = tsString(ev["ts"])
                let pre = (ev["preprocess_ms"] as? Int) ?? Int((ev["preprocess_ms"] as? Double) ?? 0)
                let dec = (ev["decision_ms"] as? Int) ?? Int((ev["decision_ms"] as? Double) ?? 0)
                let pin = (ev["prompt_tokens"] as? Int) ?? Int((ev["prompt_tokens"] as? Double) ?? 0)
                let cached = (ev["cached_tokens"] as? Int) ?? Int((ev["cached_tokens"] as? Double) ?? 0)
                let totalTok = pin + out
                let cachePct: Int? = pin > 0 ? Int(round(100.0 * Double(cached) / Double(pin))) : nil
                var failReason: String? = nil
                if !(200...299).contains(status) {
                    // 优先结构化 filtered[]（服务端派生，与 Web 面板同源同文案）；
                    // 旧事件缺字段时回退 reason 原文
                    var causes: [String] = []
                    if let arr = ev["filtered"] as? [[String: Any]] {
                        for f in arr {
                            guard let c = f["cause"] as? String else { continue }
                            let t = c.split(separator: "(").first.map(String.init)?
                                .trimmingCharacters(in: .whitespaces) ?? ""
                            if !t.isEmpty, !causes.contains(t) { causes.append(t) }
                            if causes.count >= 2 { break }
                        }
                    }
                    if causes.isEmpty {
                        let raw = (ev["reason"] as? String) ?? ""
                        failReason = raw.isEmpty ? "HTTP \(status)" : String(raw.prefix(60))
                    } else {
                        failReason = causes.prefix(2).joined(separator: " / ")
                    }
                }
                // 判定标签全部由服务端派生（decision_tags.rs）——面板不做阈值/词表推导，
                // 避免与 Web（父级）各算一套导致口径漂移（用户裁决 2026-10）
                let modelTags = ev["model_tags"] as? [String: Any]
                let tags = DecisionTags(json: ev["tags"] as? [String: Any])
                decRows.append(RecentDecision(
                    model: model, status: status, totalMs: total,
                    ttftMs: ttft > 0 ? ttft : nil, tokS: tokS,
                    agent: agent, time: time,
                    preprocessMs: pre, decisionMs: dec,
                    totalTokens: totalTok, cachePct: cachePct, failReason: failReason,
                    capability: modelTags?["capability"] as? String,
                    tags: tags))
                let did = ((ev["decision_id"] as? String) ?? "")
                if !did.isEmpty { noteChatEvent(decisionId: did, status: status) }
                if decRows.count >= 60 { break }
            }
            upd(\.recent, decRows)
            // 失败态改由 lastResult（持久到下次成功）承载；60s 时限已废
        }
        lastSync = Date()
    }

    /// 冷却模型 id → 短名（去 provider 前缀；provider/ 路径形如 opencode-go-github/kimi-k3）
    static func shortModel(_ id: String, key: String) -> String {
        let s = id.hasPrefix("\(key)/") ? String(id.dropFirst(key.count + 1)) : id
        return s
    }

    // Recent Requests：本地时区时分秒
    private static let tsFmt: DateFormatter = {
        let f = DateFormatter()
        f.dateFormat = "HH:mm:ss"
        f.timeZone = .current
        return f
    }()
    private func tsString(_ ts: Any?) -> String {
        let ms = ((ts as? Double) ?? 0) / 1000
        guard ms > 0 else { return "--:--:--" }
        return Self.tsFmt.string(from: Date(timeIntervalSince1970: ms))
    }

    /// 速率缩写（t/s 数值部分）：30000 → "30.0k"，1.2M → "1.2M"，92 → "92"
    public static func fmtRate(_ v: Double) -> String {
        v >= 1_000_000 ? String(format: "%.1fM", v / 1e6)
          : v >= 1_000 ? String(format: "%.1fk", v / 1e3)
          : String(format: "%.0f", v)
    }

    public static func fmtTok(_ n: Int) -> String {
        n >= 1_000_000 ? String(format: "%.1fM", Double(n) / 1e6)
          : n >= 1_000 ? String(format: "%.1fk", Double(n) / 1e3)
          : "\(n)"
    }

    private func get(_ path: String) async throws -> (Int, Data) {
        let (data, resp) = try await URLSession.shared.data(from: URL(string: Config.baseURL + path)!)
        return ((resp as? HTTPURLResponse)?.statusCode ?? 0, data)
    }
}

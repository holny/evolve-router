import Foundation
import SwiftUI

public struct RecentDecision: Identifiable {
    public init(model: String, status: Int, totalMs: Int, ttftMs: Int?, tokS: Double?, agent: String, time: String, preprocessMs: Int = 0, decisionMs: Int = 0, totalTokens: Int = 0, cachePct: Int? = nil, failReason: String? = nil) {
        self.model = model; self.status = status; self.totalMs = totalMs
        self.ttftMs = ttftMs; self.tokS = tokS; self.agent = agent; self.time = time
        self.preprocessMs = preprocessMs; self.decisionMs = decisionMs
        self.totalTokens = totalTokens; self.cachePct = cachePct; self.failReason = failReason
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

public struct ProviderRow: Identifiable {
    public init(key: String, enabled: Bool, available: Bool, cooldownUntil: Date?, cooldownCause: String?, pct: Int?, tokens24h: Int, planKind: String, tokens5h: Int = 0, avgTtftMs: Int? = nil, avgRate: Double? = nil, cachePct5h: Int? = nil, paygBalance: PaygBalance? = nil, observedRaw: [String: Any]? = nil, quotaUsed: Int? = nil, quotaScope: String? = nil, quotaExhausted: Bool = false, quotaReset: Date? = nil) {
        self.key = key; self.enabled = enabled; self.available = available
        self.cooldownUntil = cooldownUntil; self.cooldownCause = cooldownCause
        self.pct = pct; self.tokens24h = tokens24h; self.planKind = planKind
        self.tokens5h = tokens5h; self.avgTtftMs = avgTtftMs; self.avgRate = avgRate; self.cachePct5h = cachePct5h
        self.paygBalance = paygBalance
        self.observedRaw = observedRaw
        self.quotaUsed = quotaUsed; self.quotaScope = quotaScope
        self.quotaExhausted = quotaExhausted; self.quotaReset = quotaReset
    }
    public let key: String
    public let enabled: Bool
    public let available: Bool
    public let cooldownUntil: Date?
    public let cooldownCause: String?
    public let pct: Int?
    public let tokens24h: Int
    public let planKind: String
    public let tokens5h: Int
    public let avgTtftMs: Int?
    public let avgRate: Double?
    public let cachePct5h: Int?
    public let paygBalance: PaygBalance?
    public let observedRaw: [String: Any]?
    public let quotaUsed: Int?
    public let quotaScope: String?
    public let quotaExhausted: Bool
    public let quotaReset: Date?
    public var id: String { key }
}

public enum IconState { case idle, busy, success, fail, stopped, starting }

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
        if let r = lastResult, r.status != 200 { return .fail }  // idle+失败 → 黄 ER；新请求会清掉 lastResult
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

    /// 高频轻量轮询（1s）：in-flight 计数 + 健康，驱动图标与"进行中"
    public func pollLive() async {
        let gw = GatewayProcess.shared
        if case (200, let data)? = try? await get("/api/live"),
           let obj = try? JSONSerialization.jsonObject(with: data) as? [String: Any] {
            inflight = (obj["inflight"] as? Int) ?? Int((obj["inflight"] as? Double) ?? 0)
        } else {
            inflight = 0
        }
        let net = await gw.networkHealthy()
        healthy = net
        starting = gw.starting || (!net && gw.isChildAlive)
        external = !gw.owned && net
        // 自愈：网关不在且非外部实例 → 周期性尝试拉起（ensureRunning 内部 10s 节流）
        if !net && !external { await gw.ensureRunning() }
    }

    /// 低频明细轮询（3s）：统计/Provider/事件
    public func pollData() async {
        guard healthy else { return }
        var models: [String: Any] = [:]
        if case (200, let data)? = try? await get("/api/stats?window=5h"),
           let obj = try? JSONSerialization.jsonObject(with: data) as? [String: Any] {
            models = (obj["models"] as? [String: Any]) ?? obj
            var reqs = 0, ok = 0, toks = 0
            for (_, s) in models {
                guard let s = s as? [String: Any] else { continue }
                reqs += (s["requests"] as? Int) ?? Int((s["requests"] as? Double) ?? 0)
                ok += (s["success"] as? Int) ?? Int((s["success"] as? Double) ?? 0)
                toks += (s["prompt_tokens"] as? Int) ?? Int((s["prompt_tokens"] as? Double) ?? 0)
                toks += (s["completion_tokens"] as? Int) ?? Int((s["completion_tokens"] as? Double) ?? 0)
            }
            requests = reqs
            successRate = reqs > 0 ? Int(round(100.0 * Double(ok) / Double(reqs))) : 0
            tokensK = Self.fmtTok(toks)
        }
        var rows: [ProviderRow] = []
        var coolByKey: [String: (Bool, Date?, String)] = [:]
        if case (200, let sdata)? = try? await get("/api/providers/status"),
           let sw = try? JSONSerialization.jsonObject(with: sdata) as? [String: Any],
           let sarr = sw["providers"] as? [[String: Any]] {
            for s in sarr {
                let k = (s["key"] as? String) ?? ""
                let cd = (s["cooldown_remaining_ms"] as? Int)
                    ?? (s["cooldown_remaining_ms"] as? Double).map(Int.init)
                let cause = (s["message"] as? String) ?? ((s["kind"] as? String) ?? "")
                let until = cd.map { Date().addingTimeInterval(Double($0)/1000.0) }
                coolByKey[k] = (((s["available"] as? Bool) ?? true), until, cause)
            }
        }
        if case (200, let data)? = try? await get("/api/providers"),
           let wrapper = try? JSONSerialization.jsonObject(with: data) as? [String: Any],
           let arr = wrapper["providers"] as? [[String: Any]] {
            for p in arr {
                let key = (p["key"] as? String) ?? "-"
                let mergedKeys = (p["merged_from"] as? [Any])?.compactMap { $0 as? String } ?? [key]
                let kind = (p["plan_kind"] as? String) ?? "-"
                let windows = p["windows"] as? [String: Any]
                let w5 = (windows?["5h"] as? Double) ?? (windows?["5h"] as? Int).map(Double.init)
                let used5 = (p["used_5h"] as? Double) ?? 0
                var pct: Int? = nil
                if let obs = p["observed"] as? [String: Any],
                   let o5 = obs["5h"] as? [String: Any],
                   let limit = (o5["limit"] as? Double) ?? (o5["limit"] as? Int).map(Double.init),
                   let remaining = (o5["remaining"] as? Double) ?? (o5["remaining"] as? Int).map(Double.init),
                   limit > 0 {
                    pct = Int(((limit - remaining) / limit * 100).rounded())
                }
                if pct == nil, let w5, w5 > 0 {
                    pct = Int((used5 / w5 * 100).rounded())
                }
                var available = true
                var cooldownUntil: Date? = nil
                var cooldownCause: String? = nil
                for k in mergedKeys {
                    if let (av, until, cause) = coolByKey[k] {
                        available = available && av
                        if let until, until > Date() {
                            if cooldownUntil == nil || until > cooldownUntil! { cooldownUntil = until }
                            if cooldownCause == nil || cooldownCause!.isEmpty { cooldownCause = cause }
                        }
                    }
                }
                let disabled = (p["disabled"] as? Bool) ?? false
                let tok24 = (p["tokens_24h"] as? Int) ?? Int((p["tokens_24h"] as? Double) ?? 0)
                rows.append(ProviderRow(key: key, enabled: !disabled, available: available,
                                        cooldownUntil: cooldownUntil, cooldownCause: cooldownCause, pct: pct,
                                        tokens24h: tok24, planKind: kind,
                                        paygBalance: AppState.payg(from: p), observedRaw: (p["observed"] as? [String: Any]) ?? nil))
            }
        }
        rows.sort {
            if $0.enabled != $1.enabled { return $0.enabled }
            return $0.tokens24h > $1.tokens24h
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
        rows = rows.map { r in
            guard let e = agg[r.key] else { return r }
            let cachePct: Int? = e.promptSum > 0 ? Int(round(100.0 * Double(e.cacheSum) / Double(e.promptSum))) : nil
            // binding quota：5h/周/月 任一窗口都足以卡死请求——取用量最大者展示；
            // 多窗耗尽时恢复时间取最长（用户裁决）
            var qUsed: Int? = nil; var qScope: String? = nil
            var qExhausted = false; var qReset: Date? = nil
            if let obsRaw = r.observedRaw {
                let scopeLabel = ["5h": "5h", "weekly": "weekly", "monthly": "monthly"]
                for (scope, label) in scopeLabel {
                    guard let w = obsRaw[scope] as? [String: Any],
                          let limit = (w["limit"] as? Double) ?? (w["limit"] as? Int).map(Double.init),
                          limit > 0,
                          let remaining = (w["remaining"] as? Double) ?? (w["remaining"] as? Int).map(Double.init) else { continue }
                    let used = Int(round(100.0 * (limit - remaining) / limit))
                    let exhausted = remaining <= 0
                    let reset = (w["reset_epoch_ms"] as? Double).map { Date(timeIntervalSince1970: $0 / 1000) }
                    if qUsed == nil || used > qUsed! {
                        qUsed = used; qScope = label
                    }
                    if exhausted {
                        qExhausted = true
                        if qReset == nil || (reset ?? .distantFuture) > qReset! { qReset = reset }
                    }
                }
            }
            return ProviderRow(key: r.key, enabled: r.enabled, available: r.available,
                               cooldownUntil: r.cooldownUntil, cooldownCause: r.cooldownCause,
                               pct: r.pct, tokens24h: r.tokens24h, planKind: r.planKind,
                               tokens5h: e.tok,
                               avgTtftMs: e.ttftN > 0 ? Int(e.ttftW / Double(e.ttftN)) : nil,
                               avgRate: e.rateN > 0 ? e.rateW / Double(e.rateN) : nil,
                               cachePct5h: cachePct,
                               paygBalance: r.paygBalance,
                               quotaUsed: qUsed, quotaScope: qScope,
                               quotaExhausted: qExhausted, quotaReset: qReset)
        }
        providers = Array(rows.prefix(8))
        if case (200, let data)? = try? await get("/api/events?limit=30"),
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
                if status != 200 {
                    let raw = (ev["reason"] as? String) ?? ""
                    // reason 形如 "all routed upstreams failed: m1[0](transient error); m2[0](no credit)"
                    // 提取括号内原因去重，最多 2 个
                    var causes: [String] = []
                    var rest = Substring(raw)
                    while let r1 = rest.range(of: "("), let r2 = rest.range(of: ")", range: r1.upperBound..<rest.endIndex) {
                        let c = String(rest[r1.upperBound..<r2.lowerBound])
                        if !causes.contains(c) { causes.append(c) }
                        rest = rest[r2.upperBound...]
                    }
                    if causes.isEmpty {
                        failReason = raw.isEmpty ? "HTTP \(status)" : String(raw.prefix(60))
                    } else {
                        failReason = causes.prefix(2).joined(separator: " / ")
                    }
                }
                decRows.append(RecentDecision(
                    model: model, status: status, totalMs: total,
                    ttftMs: ttft > 0 ? ttft : nil, tokS: tokS,
                    agent: agent, time: time,
                    preprocessMs: pre, decisionMs: dec,
                    totalTokens: totalTok, cachePct: cachePct, failReason: failReason))
                let did = ((ev["decision_id"] as? String) ?? "")
                if !did.isEmpty { noteChatEvent(decisionId: did, status: status) }
                if decRows.count >= 5 { break }
            }
            recent = decRows
            if let newest = arr.last(where: { ($0["kind"] as? String) == "chat" }) {
                let status = (newest["status"] as? Int) ?? Int((newest["status"] as? Double) ?? 0)
                // 失败态改由 lastResult（持久到下次成功）承载；60s 时限已废
            }
        }
        lastSync = Date()
    }

    // Recent Requests：时分秒（24h 进制，零时区本地）
    private func tsString(_ ts: Any?) -> String {
        let ms = Int(((ts as? Double) ?? 0))
        let s = max(0, (ms / 1000) % 86400)
        return String(format: "%02d:%02d:%02d", s / 3600, (s % 3600) / 60, s % 60)
    }

    public static func fmtTok(_ n: Int) -> String {
        n >= 1_000_000 ? String(format: "%.1fM", Double(n) / 1e6)
          : n >= 1_000 ? String(format: "%.0fk", Double(n) / 1e3)
          : "\(n)"
    }

    private func get(_ path: String) async throws -> (Int, Data) {
        let (data, resp) = try await URLSession.shared.data(from: URL(string: Config.baseURL + path)!)
        return ((resp as? HTTPURLResponse)?.statusCode ?? 0, data)
    }
}

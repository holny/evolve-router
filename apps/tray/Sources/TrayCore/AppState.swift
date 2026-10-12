import Foundation
import SwiftUI

public struct RecentDecision: Identifiable {
    public init(model: String, status: Int, totalMs: Int, ttftMs: Int?, tokS: Double?, agent: String, time: String) {
        self.model = model; self.status = status; self.totalMs = totalMs
        self.ttftMs = ttftMs; self.tokS = tokS; self.agent = agent; self.time = time
    }
    public let id = UUID()
    public let model: String
    public let status: Int
    public let totalMs: Int
    public let ttftMs: Int?
    public let tokS: Double?
    public let agent: String
    public let time: String
}

public struct ProviderRow: Identifiable {
    public init(key: String, enabled: Bool, available: Bool, cooldownMs: Int?, pct: Int?, tokens24h: Int, planKind: String) {
        self.key = key; self.enabled = enabled; self.available = available
        self.cooldownMs = cooldownMs; self.pct = pct; self.tokens24h = tokens24h; self.planKind = planKind
    }
    public let key: String
    public let enabled: Bool
    public let available: Bool
    public let cooldownMs: Int?
    public let pct: Int?
    public let tokens24h: Int
    public let planKind: String
    public var id: String { key }
}

public enum IconState { case idle, busy, fail, stopped, starting }

@MainActor
public final class AppState: ObservableObject {
    public init() {}
    @Published public var healthy = true
    @Published public var external = false
    @Published public var starting = false
    @Published public var inflight = 0
    @Published public var pulse = false
    @Published public var requests = 0
    @Published public var successRate = 0
    @Published public var tokensK = "-"
    @Published public var plans: [(String, Int)] = []
    @Published public var recent: [RecentDecision] = []
    @Published public var providers: [ProviderRow] = []
    @Published public var lastSync: Date?
    @Published public var lastFailAt: Date?

    public var iconState: IconState {
        if !healthy { return .stopped }
        if starting { return .starting }
        if let f = lastFailAt, Date().timeIntervalSince(f) < 60 { return .fail }
        if inflight > 0 { return .busy }
        return .idle
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
    }

    /// 低频明细轮询（3s）：统计/Provider/事件
    public func pollData() async {
        guard healthy else { return }
        if case (200, let data)? = try? await get("/api/stats?window=24h"),
           let obj = try? JSONSerialization.jsonObject(with: data) as? [String: Any] {
            let models = (obj["models"] as? [String: Any]) ?? obj
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
        var byPlan: [String: Int] = [:]
        var coolByKey: [String: (Bool, Int?)] = [:]
        if case (200, let sdata)? = try? await get("/api/providers/status"),
           let sw = try? JSONSerialization.jsonObject(with: sdata) as? [String: Any],
           let sarr = sw["providers"] as? [[String: Any]] {
            for s in sarr {
                let k = (s["key"] as? String) ?? ""
                let cd = (s["cooldown_remaining_ms"] as? Int)
                    ?? (s["cooldown_remaining_ms"] as? Double).map(Int.init)
                coolByKey[k] = (((s["available"] as? Bool) ?? true), cd)
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
                if let pct {
                    let short = ["coding": "CP", "go": "GO", "agent": "AP", "token": "TOK"][kind]
                        ?? String(kind.prefix(4)).uppercased()
                    byPlan[short] = max(byPlan[short] ?? 0, pct)
                }
                var available = true
                var cooldownMs: Int? = nil
                for k in mergedKeys {
                    if let (av, cd) = coolByKey[k] {
                        available = available && av
                        if let cd, cd > 0 { cooldownMs = min(cooldownMs ?? Int.max, cd) }
                    }
                }
                let disabled = (p["disabled"] as? Bool) ?? false
                let tok24 = (p["tokens_24h"] as? Int) ?? Int((p["tokens_24h"] as? Double) ?? 0)
                rows.append(ProviderRow(key: key, enabled: !disabled, available: available,
                                        cooldownMs: cooldownMs, pct: pct,
                                        tokens24h: tok24, planKind: kind))
            }
        }
        rows.sort {
            if $0.enabled != $1.enabled { return $0.enabled }
            return $0.tokens24h > $1.tokens24h
        }
        providers = Array(rows.prefix(8))
        plans = byPlan.sorted { $0.value > $1.value }
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
                decRows.append(RecentDecision(model: model, status: status, totalMs: total,
                                              ttftMs: ttft > 0 ? ttft : nil, tokS: tokS,
                                              agent: agent, time: time))
                if decRows.count >= 5 { break }
            }
            recent = decRows
            if let newest = arr.last(where: { ($0["kind"] as? String) == "chat" }) {
                let status = (newest["status"] as? Int) ?? Int((newest["status"] as? Double) ?? 0)
                if status != 200 {
                    let ts = (newest["ts"] as? Double) ?? 0
                    lastFailAt = Date(timeIntervalSince1970: ts / 1000)
                } else {
                    lastFailAt = nil
                }
            }
        }
        lastSync = Date()
    }

    private func tsString(_ ts: Any?) -> String {
        let d = Date(timeIntervalSince1970: ((ts as? Double) ?? 0) / 1000)
        return d.formatted(date: .omitted, time: .shortened)
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

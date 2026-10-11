// EvolveTray — EvolveRouter 菜单栏伴侣
// 图标实时反映网关健康（绿=正常 / 红=异常），点击在图标下方弹出迷你监测窗：
// 网关状态、请求与成功率、Tokens、各方案 5h 用量、最近决策、完整面板入口。
// 数据源：本地网关 http://127.0.0.1:8787（3s 轮询，与网关解耦、离线静默重试）。

import SwiftUI
import AppKit

let baseURL = ProcessInfo.processInfo.environment["EVOLVE_URL"] ?? "http://127.0.0.1:8787"

// MARK: - 状态

@MainActor
final class AppState: ObservableObject {
    @Published var healthy = false
    @Published var requests = 0
    @Published var successRate = 0
    @Published var tokensK = "-"
    @Published var plans: [(String, Int)] = []
    @Published var recent: [(String, Int, Int)] = []   // (model, status, total_ms)
    @Published var lastSync: Date?
    @Published var syncing = false

    func apply(status: Int) {
        healthy = (200..<300).contains(status)
    }
}

// MARK: - 轮询

@MainActor
final class Poller {
    private var timer: Timer?

    func start(state: AppState, onHealthy: @escaping (Bool) -> Void) {
        let task = { Task { await self.tick(state: state, onHealthy: onHealthy) } }
        task()
        timer = Timer.scheduledTimer(withTimeInterval: 3, repeats: true) { _ in task() }
    }

    private func tick(state: AppState, onHealthy: @escaping (Bool) -> Void) {
        if state.syncing { return }
        state.syncing = true
        Task {
            defer { Task { @MainActor in state.syncing = false } }
            // healthz：图标状态源
            if let hz = try? await get("/healthz") {
                let ok = hz.0 == 200
                state.healthy = ok
                onHealthy(ok)
            } else {
                state.healthy = false
                onHealthy(false)
            }
            // /api/stats：HashMap<model, stats> 聚合
            if case (200, let data)?? = try? await get("/api/stats"),
               let obj = try? JSONSerialization.jsonObject(with: data) as? [String: Any] {
                var reqs = 0, ok = 0, toks = 0
                for (_, s) in obj {
                    guard let s = s as? [String: Any] else { continue }
                    reqs += (s["requests"] as? Int) ?? Int((s["requests"] as? Double) ?? 0)
                    ok += (s["success"] as? Int) ?? Int((s["success"] as? Double) ?? 0)
                    toks += (s["prompt_tokens"] as? Int) ?? Int((s["prompt_tokens"] as? Double) ?? 0)
                    toks += (s["completion_tokens"] as? Int) ?? Int((s["completion_tokens"] as? Double) ?? 0)
                }
                state.requests = reqs
                state.successRate = reqs > 0 ? Int(round(100.0 * Double(ok) / Double(reqs))) : 0
                state.tokensK = toks >= 1_000_000 ? String(format: "%.1fM", Double(toks)/1e6)
                              : toks >= 1_000 ? String(format: "%.0fk", Double(toks)/1e3) : "\(toks)"
            }
            // /api/providers：各方案 5h 用量占比
            if case (200, let data)?? = try? await get("/api/providers"),
               let arr = try? JSONSerialization.jsonObject(with: data) as? [[String: Any]] {
                var byPlan: [String: Int] = [:]
                for p in arr {
                    let kind = (p["plan_kind"] as? String) ?? "-"
                    let short = ["coding": "CP", "go": "GO", "agent": "AP", "token": "TOK"][kind] ?? nil
                    guard let short else { continue }
                    guard let windows = p["windows"] as? [String: Any],
                          let allow = (windows["5h"] as? Double) ?? (windows["5h"] as? Int).map(Double.init), allow > 0 else { continue }
                    let used = (p["used_5h"] as? Double) ?? (p["used_5h"] as? Int).map(Double.init) ?? 0
                    let pct = Int(round(100 * used / allow))
                    byPlan[short] = max(byPlan[short] ?? 0, pct)
                }
                state.plans = byPlan.sorted { $0.value > $1.value }
            }
            // /api/events?limit=5：最近决策
            if case (200, let data)?? = try? await get("/api/events?limit=8"),
               let arr = try? JSONSerialization.jsonObject(with: data) as? [[String: Any]] {
                var rows: [(String, Int, Int)] = []
                for ev in arr where (ev["kind"] as? String) == "chat" {
                    let model = (ev["chosen"] as? String) ?? "-"
                    let status = (ev["status"] as? Int) ?? Int((ev["status"] as? Double) ?? 0)
                    let ms = (ev["total_ms"] as? Int) ?? Int((ev["total_ms"] as? Double) ?? 0)
                    rows.append((model, status, ms))
                    if rows.count >= 5 { break }
                }
                state.recent = rows
            }
            state.lastSync = Date()
        }
    }

    private func get(_ path: String) async throws -> (Int, Data) {
        let (data, resp) = try await URLSession.shared.data(from: URL(string: baseURL + path)!)
        return ((resp as? HTTPURLResponse)?.statusCode ?? 0, data)
    }
}

// MARK: - 迷你面板（popover 内容）

struct PanelView: View {
    @ObservedObject var state: AppState

    var body: some View {
        VStack(alignment: .leading, spacing: 10) {
            HStack {
                Circle().fill(state.healthy ? Color.green : Color.red).frame(width: 8, height: 8)
                Text(state.healthy ? "网关运行中 · 8787" : "网关不可达")
                    .font(.system(size: 12, weight: .semibold))
                Spacer()
                if let t = state.lastSync {
                    Text(t, style: .time).font(.system(size: 10)).foregroundStyle(.secondary)
                }
            }
            HStack(spacing: 14) {
                stat("请求", "\(state.requests)")
                stat("成功率", "\(state.successRate)%")
                stat("Tokens", state.tokensK)
            }
            if !state.plans.isEmpty {
                section("方案 5h 用量") {
                    ForEach(state.plans, id: \.0) { name, pct in
                        HStack {
                            Text(name).font(.system(size: 11, weight: .medium)).frame(width: 36, alignment: .leading)
                            ProgressBar(pct: pct)
                            Text("\(pct)%").font(.system(size: 11, design: .monospaced)).frame(width: 34, alignment: .trailing)
                        }
                    }
                }
            }
            if !state.recent.isEmpty {
                section("最近决策") {
                    ForEach(Array(state.recent.enumerated()), id: \.offset) { _, row in
                        HStack(spacing: 6) {
                            Circle().fill(row.1 == 200 ? Color.green : Color.red).frame(width: 5, height: 5)
                            Text(row.0).font(.system(size: 10.5)).lineLimit(1)
                            Spacer()
                            Text(row.1 == 200 ? "\(row.2)ms" : "fail")
                                .font(.system(size: 10, design: .monospaced))
                                .foregroundStyle(row.1 == 200 ? .secondary : Color.red)
                        }
                    }
                }
            }
            Divider()
            Button {
                NSWorkspace.shared.open(URL(string: baseURL)!)
            } label: {
                Label("打开完整面板", systemImage: "gauge.with.needle").frame(maxWidth: .infinity)
            }
            .controlSize(.small)
        }
        .padding(12)
        .frame(width: 300)
    }

    private func stat(_ label: String, _ value: String) -> some View {
        VStack(alignment: .leading, spacing: 1) {
            Text(label).font(.system(size: 10)).foregroundStyle(.secondary)
            Text(value).font(.system(size: 14, weight: .semibold, design: .monospaced))
        }
    }

    private func section<Content: View>(_ title: String, @ViewBuilder content: () -> Content) -> some View {
        VStack(alignment: .leading, spacing: 4) {
            Text(title).font(.system(size: 10, weight: .semibold)).foregroundStyle(.secondary)
            content()
        }
    }
}

struct ProgressBar: View {
    let pct: Int
    var body: some View {
        GeometryReader { geo in
            let w = geo.size.width
            ZStack(alignment: .leading) {
                Capsule().fill(.quaternary)
                Capsule().fill(pct >= 85 ? AnyShapeStyle(Color.orange) : AnyShapeStyle(Color.accentColor))
                    .frame(width: max(3, w * CGFloat(min(100, max(0, pct))) / 100))
            }
        }.frame(height: 6)
    }
}

// MARK: - AppKit shell（状态栏项 + Popover）

@MainActor
final class AppDelegate: NSObject, NSApplicationDelegate {
    let state = AppState()
    let poller = Poller()
    var statusItem: NSStatusItem!
    var popover: NSPopover!

    func applicationDidFinishLaunching(_ note: Notification) {
        NSApp.setActivationPolicy(.accessory) // 不进 Dock
        statusItem = NSStatusBar.system.statusItem(withLength: NSStatusItem.squareLength)
        if let btn = statusItem.button {
            btn.image = NSImage(systemSymbolName: "arrow.triangle.branch", accessibilityDescription: "EvolveRouter")
            btn.contentTintColor = .systemGreen
            btn.action = #selector(togglePopover)
            btn.target = self
        }
        let root = NSHostingView(rootView: PanelView(state: state))
        root.frame = NSRect(x: 0, y: 0, width: 300, height: 380)
        let vc = NSViewController()
        vc.view = root
        popover = NSPopover()
        popover.contentViewController = vc
        popover.behavior = .transient
        popover.contentSize = NSSize(width: 300, height: 380)
        poller.start(state: state) { [weak self] healthy in
            self?.statusItem.button?.contentTintColor = healthy ? .systemGreen : .systemRed
        }
    }

    @objc func togglePopover() {
        if popover.isShown {
            popover.performClose(nil)
        } else if let btn = statusItem.button {
            popover.show(relativeTo: btn.bounds, of: btn, preferredEdge: .minY)
        }
    }
}

// main.swift 顶层入口
let app = NSApplication.shared
MainActor.assumeIsolated {
    let delegate = AppDelegate()
    app.delegate = delegate
    app.run()
}

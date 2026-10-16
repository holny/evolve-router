import Foundation

// 面板文案英化精简（用户裁决：减少 6 语翻译成本；短词 + 图标优先）
public let LANGS = ["en"]
public let LOCALE = ["en": "en-US"]
public var lang: String = "en"

let T: [String: String] = [
    "healthy": "Running · 8787",
    "stopped": "Stopped",
    "starting": "Starting…",
    "external": "External instance running (read-only)",
    "inflight": "in flight",
    "copy_cfg": "Copy wiring config (baseURL / model / provider)",
    "reqs": "reqs",
    "success_rate": "success",
    "tokens": "tokens",
    "last_24h": "last 5h",
    "last_sync": "synced",
    "plan_5h": "Plan usage · 5h",
    "recent": "Recent requests",
    "fail_short": "fail",
    "providers": "Providers",
    "cooldown": "cooldown",
    "disabled": "disabled",
    "payg": "payg",
    "autostart": "autostart",
    "stop_gw": "Stop",
    "start_gw": "Start",
    "dashboard": "Dashboard",
    "github": "GitHub",
    "quit": "Quit",
    "quit_confirm": "Quit EvolveRouter completely?",
    "quit_action": "Quit (gateway and menu bar app will both close)",
    "cancel": "Cancel",
    "quit_hint": "Reopen: Applications → EvolveRouter, or `open -a EvolveRouter`",
    "gateway_err": "gateway error",
    "l_edit_weight": "Edit model weight",
    "dm_missing": "Decision model missing — heuristic only",
    "dm_missing_hint": "Add a [[decision.slots]] block in evo-router.toml (hot-reloads in ~5s)",
    "dm_stale": "Decision slots configured — applying",
    "restart": "Restart",
    "restart_help": "Restart the gateway (applies [server]/[policy]/[[models]] changes)",
    "reload": "Reload",
    "reload_help": "Reload config now (decision slots hot-reload automatically every ~5s)",
    "cfg_dirty": "Config changed — restart to apply",
]

public func t(_ key: String) -> String {
    T[key] ?? key
}

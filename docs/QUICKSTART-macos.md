# Quick start — macOS menu-bar app

Everything below assumes the app is installed in `/Applications/EvolveRouter.app` and running (click the **ER** badge in the menu bar to open the panel).

## Three entry points

| What you want | Where |
|---|---|
| Status & controls | Menu bar **ER** badge → the panel (Stop/Start · **Restart** · Quit · autostart switch) |
| Full data view | <http://127.0.0.1:8787/> |
| Point an agent at the gateway | base URL `http://127.0.0.1:8787/v1`, model `auto` (or `Auto (smart routing)`) |

The panel header tells you what is judging and whether it works:

- line 1 — gateway state (`Running · 8787`) + version
- line 2 — last sync time + `judged by decision model + rules` / `judged by rules only` (the **configured** judge path)
- an orange banner appears for `Decision model not configured` / `Decision model unreachable (<reason>) · retrying in <n>s` / `Decision model configured — applying`

## Editing the config

Config file: **`~/.evo-router/evo-router-config.jsonc`** (JSONC — comments are allowed, the file documents itself).

| Section | How it takes effect |
|---|---|
| `decision` (decision model slots) and `providers` | **hot-reloaded in ~5 s**; force it now with `curl -sX POST http://127.0.0.1:8787/api/reload` |
| everything else (`server`, `policy`, `models`, `discovery`, …) | needs a restart — panel **Restart**, or `launchctl kickstart -k gui/$(id -u)/ai.evolve-router.gateway`, or Ctrl-C and re-run |

`GET /api/live` tells you the truth instead of making you guess:

```bash
curl -s localhost:8787/api/live | python3 -m json.tool
# "config":   { "reloaded_at_ms": …, "restart_required": false, "path": "…", "active_slots": ["jev"] }
# "decision": { "backend": "decision_model", "status": "ok", "last_judge": "decision_model+rules",
#               "last_model_used": true, "last_fail": null, "consecutive_fails": 0, "retry_in_ms": 0 }
```

`decision.status` means: `ok` (judging as configured) · `degraded` (slots configured, but the last judge fell back to rules — `last_fail` names the cause) · `stale` (configured, not applied yet) · `missing` (nothing configured).

## Decision model (optional)

Routing works without one (built-in rule judge). To use one, add a slot to the config:

```jsonc
"decision": {
  "backend": "decision_model",      // decision_model = model + rules fusion; auto = model only; heuristic = rules only
  "judge_timeout_ms": 12000,        // default 3000 — raise it for internet relays
  "slots": [
    { "kind": "jev", "api_key": "<key>" },                                    // JEV cloud
    { "kind": "jev",                                                           // StartLux-Decision 27B (Bamboo Relay)
      "base_url": "https://api.bamboonode.dev/v1/systemone",
      "model": "startlux-decision-27b-autothink",
      "think_mode": "never",         // one pass, fastest
      "api_key": "<bamboo-key>" },
    { "kind": "laya", "base_url": "http://127.0.0.1:8321/v1/judge" },          // local sidecar, no key
    { "kind": "openai", "base_url": "http://127.0.0.1:11434/v1",               // any OpenAI-compatible endpoint
      "model": "<model-id>" }
  ]
}
```

Slots are tried **jev > laya > openai**. Fatal errors (auth/404/unreachable) blacklist a slot for the process lifetime; transient errors (timeout/5xx) retry on the next request and then back off exponentially (30 s → 10 min) — a single success clears everything. A decision-model outage never blocks routing.

## Where things live

```
~/.evo-router/
├── evo-router-config.jsonc   # the config (the only file you edit)
├── events/events.jsonl       # append-only decision log (replay, stats, backtest)
├── state/                    # snapshot, overrides, benchmark & catalog caches
└── logs/gateway-child.log    # menu-bar mode log (gateway.log for LaunchAgent mode)
```

Provider credentials come from the agent configs themselves (`~/.config/opencode/…`, `~/.codex/config.toml`), from the panel/dashboard (`state/overrides.json`), or from `providers` / `models` in the config — priority: config → overrides → discovery, deduplicated inside the gateway on `base_url + api_key`. `POST /api/config/export` writes the live provider list (keys included, with `origin` and `source_path`) back into the config.

## Self-checks

```bash
curl -s localhost:8787/healthz                              # ok
curl -s localhost:8787/api/live | python3 -m json.tool      # version / judge / slots / restart_required
curl -s localhost:8787/api/providers | python3 -m json.tool # provider rows + provenance
curl -sX POST localhost:8787/api/reload                     # reload config now
/Applications/EvolveRouter.app/Contents/MacOS/evo-router doctor   # config + catalog + credentials
```

## Troubleshooting

| Symptom | What to do |
|---|---|
| Panel says `Decision model unreachable (timeout)` | The endpoint answered too late — raise `decision.judge_timeout_ms`; the banner shows the retry countdown and clears itself after one success |
| Panel says `Decision model not configured` | Add a `decision.slots` block (hot-reloads in ~5 s) |
| Panel says `Config changed — restart to apply` | A non-hot-reloadable section changed → click **Restart** |
| Changed a provider / the decision model and nothing happened | Wait ~5 s, or `POST /api/reload`; check `/api/live` → `config.reloaded_at_ms` |
| Gateway not responding | Panel → **Start** (or `Restart`); logs in `~/.evo-router/logs/` |
| Restarting from a terminal | send **SIGTERM** (`pkill -TERM -f "evo-router serve"`), never `-9`: SIGTERM stops accepting, lets in-flight requests (including live SSE streams) finish, flushes the flywheel, then exits. SIGKILL cuts live streams — the client sees a truncated response with no usage/terminator |
| A stale `~/.evolve` or flat `~/.evo-router` layout | Migrated automatically on startup; leftovers are kept aside as `*.migrated` / `.old-<ts>` |

## Headless / service mode

```bash
/Applications/EvolveRouter.app/Contents/MacOS/evo-router serve   # foreground, same config
evo-router service install                                       # LaunchAgent: start/stop/restart/status
```

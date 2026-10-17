<p align="center">
  <img src="docs/assets/logo.png" width="120" alt="EvolveRouter logo">
</p>

<h1 align="center">EvolveRouter</h1>

<p align="center"><b>Think before you route. Evolve with every request.</b></p>

<p align="center">
  <a href="LICENSE"><img src="https://img.shields.io/badge/license-MIT-blue.svg" alt="License"></a>
  <a href="#development"><img src="https://img.shields.io/badge/tests-259%20passing-brightgreen.svg" alt="Tests"></a>
  <a href="https://www.rust-lang.org"><img src="https://img.shields.io/badge/rust-stable-orange.svg" alt="Rust"></a>
</p>

EvolveRouter is a **local LLM routing gateway** for agents. Every request is judged on eight signals, priced in its own plan's currency, scored on five factors, and routed to the model that fits — then the outcome feeds back so the next decision is better. It ships as one Rust binary with a native macOS menu-bar panel and a zero-build dashboard. Your prompts, sessions and telemetry never leave the machine.

```
opencode · Claude Code · Codex · Pi · openclaw · hermes · dsh
                    │  model = "auto"
                    ▼
        ┌───────────────────────────┐
        │  judge → score → route    │   eight signals · five factors · plan-aware pricing
        │  fallback → learn         │   quality cascade · flywheel · Thompson exploration
        └───────────────────────────┘
                    ▼
     zhipu · Z.ai · MiniMax · volces · opencode zen · DeepSeek · any OpenAI/Anthropic API
```

<p align="center">
  <img src="docs/assets/dashboard.png" width="760" alt="Dashboard: live decisions, onboarding, provider cards">
  <img src="docs/assets/panel.png" width="280" alt="macOS menu-bar panel: providers, recent requests">
</p>

## Contents

- [Install](#install) · [Point your agent at it](#point-your-agent-at-it)
- [How a request is routed](#how-a-request-is-routed) · [Price-aware layers](#price-aware-layers)
- [Decision model (optional)](#decision-model-optional) · [Request tags](#request-tags)
- [macOS menu-bar panel](#macos-menu-bar-panel) · [Dashboard](#dashboard)
- [Configuration](#configuration) · [Operations](#operations)
- [Why not another proxy](#why-not-another-proxy) · [Managed plans](#managed-plans) · [Development](#development)

## Install

**Homebrew (macOS menu-bar app + gateway):**

```bash
brew install --cask holny/evolve/evolve-router     # or: brew tap holny/evolve && brew install --cask evolve-router
```

Unzip-free: the cask installs `EvolveRouter.app` (menu-bar panel + bundled gateway). Not notarized — first launch needs right-click → Open.

**From source (gateway only, any OS):**

```bash
git clone https://github.com/holny/evolve-router && cd evolve-router
cargo build --release
./target/release/evo-router serve          # http://127.0.0.1:8787
```

## Point your agent at it

One endpoint, one model name. Nothing else changes in your stack.

```jsonc
// ~/.config/opencode/opencode.jsonc
{
  "provider": {
    "evo-router": {
      "npm": "@ai-sdk/openai-compatible",
      "options": { "baseURL": "http://127.0.0.1:8787/v1" },
      "models": { "auto": { "name": "Auto (smart routing)" } }
    }
  }
}
```

OpenAI-compatible clients use `http://127.0.0.1:8787/v1` with model `auto`; Claude Code and other Anthropic clients use `http://127.0.0.1:8787` as the Anthropic base URL (bidirectional streaming translation is built in). `codex`, `pi`, `openclaw`, `hermes` and `dsh` are discovered automatically — their providers, keys and models are read from their own config files, so you usually have **nothing to configure**.

## How a request is routed

<p align="center">
  <img src="docs/assets/architecture.svg" width="820" alt="EvolveRouter architecture">
</p>

```mermaid
flowchart TD
    A["request · model = auto"] --> B{"hard constraints<br/>context window · vision · credential"}
    B -- "cannot fit" --> X["filtered out<br/>(reason recorded)"]
    B --> C{"provider health<br/>degraded · quota exhausted · cooling"}
    C -- "unavailable" --> X
    C --> D["portfolio<br/>per-provider slots · capability×price cells<br/>free models always in"]
    D --> E["judge<br/>decision model + rule second opinion<br/>8 signals"]
    E --> F["difficulty calibration<br/>rolling quantiles → 7:2:1"]
    F --> G["dynamic weights<br/>difficulty-linked × quota-aware"]
    G --> H["five-factor score<br/>quality · speed · cost · reliability · headroom<br/>× flywheel bias × user weight × recency × free bonus"]
    H --> I{"price-tier gate<br/>cheap / medium / premium"}
    I -- "all gated" --> I2["gate yields<br/>before quality does"]
    I --> J["Thompson exploration<br/>Beta posterior"]
    I2 --> J
    J --> K["winner queue<br/>provider round-robin, up to 5"]
    K --> L["upstream"]
    L -- "failure" --> M["classify<br/>quota · auth · network · context · request shape"]
    M --> N["provider degrade<br/>≥3 consecutive → probe 1m…60m"]
    M --> O["session quarantine<br/>request-shape 4xx → this session only"]
    M --> P["constraint-aware sweep<br/>provider rotation"]
    P --> L
    L -- "success" --> Q["event + flywheel<br/>reliability · bias · calibration"]
    Q --> F
```

| Step | What it does |
|---|---|
| **Judge** | A pluggable **decision model** reads the request (domain, difficulty, vision need, triviality, tool density, stakes, session depth, size). A built-in multilingual heuristic judge runs as an independent second opinion (take-conservative fusion). If the decision model is unreachable the rules take over — **a decision-model outage never blocks routing** |
| **Difficulty calibration** | Rolling-quantile calibration reshapes the judge's raw difficulty onto the 7:2:1 target, so a biased judge cannot flood the router with "hard" tasks |
| **Hard filters** | Context window, vision capability, provider health (degraded / quota exhausted / cooling), per-model quota, and the price-tier gate — each rejection is recorded with its reason |
| **Dynamic weights** | Difficulty-linked (easy → cost matters 2×, quality 0.75×; hard → the reverse) and quota-aware ([BaRP](https://arxiv.org/abs/2510.07429) Eq.1) |
| **Five-factor score** | Quality · speed · cost · reliability · headroom. Cost is an *effective* cost: official price sheets, per-plan credit multipliers, cache-hit vs miss (~1/50), peak/off-peak windows. Reliability blends long-term 50 % / last-30 30 % / last-10 20 % |
| **Exploration** | Beta posterior per model with the judge's quality as prior mean; Thompson sampling explores sample-poor models and converges on sample-rich ones |
| **Winner queue** | The ranked chain is tried in order — a failure (or a *poor-quality* response: degenerate, empty, broken tool calls) escalates to the next candidate; the last-resort sweep covers out-of-chain models |
| **Flywheel** | Four feedback layers (transport success, tool-call syntax, session-confirmed semantic loops, plugin-reported outcomes) are two-stage normalized and learned relative to the cross-model median |

Strategy parameters are versioned and must pass `evo-router backtest` (replay on real production events, judged by Performance-Gap-Recovered) before adoption. Every learned quantity is clamped (bias ∈ [0.7, 1.3], distillation shift ≤ 0.15).

### How it scores

`d ∈ [0,1]` is the calibrated difficulty (`D/3`); `q` quality, `s` speed, `c` cost, `r` reliability, `h` headroom.

```
# 1. quality — difficulty steepens the requirement
tier = tier_for_domain(domain, tiers)          # coding / reasoning / agentic / vision
q    = 1 - d * (1 - tier ^ (1 + 2d))
q   *= 0.6 + 0.4 * max(tier, reasoning)        # only when high_stakes

# 2. quality floor — easy tasks are permissive, hard tasks are not
floor(d) = 0.50 + 0.35 * d

# 3. difficulty-linked weights (easy -> cost, hard -> quality)
w_cost    = w_c * (2.0 - 1.6 * d)
w_quality = w_q * (0.75 + 0.25 * d)            # rest unchanged, then normalized
w_cost   *= 1 + 1.5 * tight                    # BaRP Eq.1, quota pressure in [0,1]

# 4. effective cost (per 1M tokens, plan currency)
in_eff = price_in_miss * (1 - hit_rate) + price_in_hit * hit_rate
cost   = (est_in / 1e6) * in_eff + (est_out / 1e6) * price_out
cost  *= peak_factor(now)                      # e.g. DeepSeek peak = 2x

# 5. cost factor (higher = cheaper; two rulers that are never mixed)
pay-as-you-go:  c = clamp(min_price / cost, 0, 1)
subscription:   c = clamp(scheme_min / quota_intensity, 0.05, 1)
unknown cost:   c = 0.5                        # neutral — never "free"

# 6. score
S  = w_q*q + w_s*s + w_c*c + w_r*r + w_h*h
r  = 0.5*r_long + 0.3*r_30 + 0.2*r_10          # layers merged when missing
h  = (context_window - est_tokens) / context_window
S *= clamp(sqrt(user_weight * learned_bias), 0.4, 1.8)
S *= 1 + 0.05 * recency(<= 2h)                 # cache-prefix affinity
S *= 1 + 0.10 * free                           # free model, bounded

# 7. portfolio — what is routable at all
CAP     = max(50, 8 * healthy_providers)
w_i     = 1 + quota_headroom_i + 0.1 * free_i
slots_i = clamp(round(CAP * w_i / sum(w)), 3, 0.25 * CAP)
# free and pinned models are always in and do not consume slots

# 8. health — recovery, not exile
provider: 3 consecutive failures -> fallback only, probe at 1m, 5m, 15m, 30m, 60m
          first success -> back in the pool immediately
model:    per failure class cooldown (quota -> window reset, unsupported -> 5h, transient -> 30s..10m)
session:  request-shape 4xx -> that (session, model) only, 30 min
```

Every learned quantity is clamped (`bias ∈ [0.7, 1.3]`, distillation shift ≤ 0.15), and strategy changes must pass `evo-router backtest` on real events before adoption.

### Price-aware layers

| Layer | What it does |
|---|---|
| **Model tags** (capability / price) | Auto-derived by quantile (70/20/10 in both dimensions) over the **currently usable pool** only — cooling, quota-gated and keyless models are excluded dynamically; user-editable in the dashboard, and locked by contract tests |
| **Price-tier gate** | L1 tasks may use `cheap` models only, L2 opens `medium`, L3 everything — with an **inversion exception** (a pricier model whose effective cost ≤ the allowed pool's median passes) and a quality-first relaxation (the gate yields before quality does) |
| **Ratio controller** | Watches the actual chosen-tier mix and nudges it back toward 7:2:1 |
| **Routing portfolio** | The routable pool is a **per-provider portfolio**, not one global list: `cap = max(50, healthy providers × 8)`, each provider ≤ 25 % of it. Inside a provider the capability × price cells are filled first, then newest / recently used. Free models are always in and do not consume slots, and pinned models do not either. This keeps one provider that ships many models from taking every slot — and keeps large-context models reachable |
| **Supply awareness** | Subscription quota (it expires!) ranks above pay-as-you-go spend. API-queryable providers refresh through a 90 s quota watchdog; others are probed with real requests every 5–10 min |

## Decision model (optional)

Routing works without one — the built-in rule judge is fully functional. To add one, declare slots in the config file; that is the only place they are read from (a gateway started by the menu-bar app at login has no shell environment, so environment-only configuration would silently degrade to rules):

```jsonc
// ~/.evo-router/evo-router-config.jsonc
"decision": {
  "backend": "decision_model",     // decision_model = model + rules fusion · auto = model only · heuristic = rules only
  "judge_timeout_ms": 12000,       // default 3000 — raise it for internet relays
  "slots": [
    { "kind": "jev", "api_key": "<key>" },                                    // JEV cloud (native contract)
    { "kind": "jev",                                                           // StartLux-Decision 27B via Bamboo Relay
      "base_url": "https://api.bamboonode.dev/v1/systemone",
      "model": "startlux-decision-27b-autothink",
      "think_mode": "never",         // one pass — fastest; AutoThink otherwise reasons when unsure
      "api_key": "<bamboo-key>" },
    { "kind": "laya", "base_url": "http://127.0.0.1:8321/v1/judge" },          // local sidecar (scripts/laya_server.py)
    { "kind": "openai", "base_url": "http://127.0.0.1:11434/v1",               // any OpenAI-compatible endpoint
      "model": "<model-id>" }
  ]
}
```

- **Priority** jev > laya > openai; the first usable slot wins, and `protocol` / `think_mode` / `api_key_env` are optional per slot.
- **Fatal errors** (auth, 404, unreachable) blacklist a slot for the process lifetime. **Transient errors** (timeout, 5xx, unparseable) retry on the next request, then back off exponentially (30 s → 10 min) so no request pays the same timeout twice in a row; **one success clears everything**.
- **Status is honest**: `GET /api/live` reports `decision.status` = `ok` · `degraded` (slots configured but the last judge fell back to rules — `last_fail` names the cause, e.g. `timeout` or `auth failed (401/403)`) · `stale` (configured, not applied yet) · `missing` (nothing configured), plus `last_judge`, `consecutive_fails` and `retry_in_ms`.
- Editing `decision` (and `providers`) **hot-reloads in ~5 s** — no restart, or `POST /api/reload` to force it.

## Request tags

Every request and every model row carries the decision itself, rendered from one server-side derivation (`decision_tags.rs`) that both the dashboard and the panel read — the vocabulary and thresholds exist exactly once:

| Category | Values | What it answers |
|---|---|---|
| `difficulty` | easy / medium / hard | how hard the gateway judged this request (drives quality floor and cost weight) |
| `domain` | code / math / data / agent / writing / lookup / chat | which capability tiers the request is scored against |
| `flags` | trivial / tools / vision / stakes | request traits the judge flagged (thresholds match the routing gates) |
| `fit` | match / over / under | task difficulty ↔ chosen model's capability tier |
| `mode` | model / model+rules / reused / rules / pinned | who decided the route — the last three skipped the decision model |
| `capability` · `price` | basic / standard / powerful · cheap / medium / premium | the chosen model's tier (Model tab) |

Request-side tags live on the request rows; model-side tags live in the Model list. Placeholder judgments are suppressed: on a reused (`sticky`) or pinned turn the row shows `mode` only, never a difficulty that was not actually judged.

## macOS menu-bar panel

<p align="center">
  <img src="docs/assets/panel.png" width="300" alt="Providers and recent requests">
  <img src="docs/assets/panel-models.png" width="300" alt="Models view with inline failure reasons">
</p>

The panel is a native SwiftUI app; the badge itself is the status light (idle / routing / last request failed / stopped / starting).

| Area | What it shows |
|---|---|
| Header | gateway state (`Running · 8787`) + version · clock + `judged by decision model + rules` / `judged by rules only` (**configured** path, not the last request's outcome) |
| Banner | the *why* when something is off: `Decision model not configured` · `Decision model unreachable (timeout) · retrying in 26s` · `configured — applying` |
| Metrics | requests / success rate / tokens for **today** or **yesterday** (calendar windows, persisted in hourly buckets) |
| Providers / Models card | up to 12 providers (or 50 used models) in a scrollable, **three-line** row: name + state (quota bar / balance / `5h full` warning, right-aligned) · data (`tokens · hit% · t/s · TTFT`) · tags (tier · models · free · degraded · get key). Failure reasons are inline (`fail 25%`, `unsupported model 4m`, `no key`) |
| Recent requests | the last 60 requests within 10 days, scrollable; `<N> in flight` on the title line lists what is being generated right now |
| Controls | GitHub · Dashboard · **Stop/Start** · **Restart** · Quit · autostart |

**Restarts are graceful.** SIGINT/SIGTERM stops accepting new requests, lets in-flight ones (including live SSE streams) finish — up to a 60 s cap for a stuck upstream, which is logged — flushes the flywheel, then exits. Restarting never truncates a running generation; if requests are in flight, the panel asks for confirmation first. Never `kill -9` a gateway you want to keep clean.

## Dashboard

<p align="center">
  <img src="docs/assets/dashboard.png" width="820" alt="Dashboard: onboarding, live decisions, provider cards">
</p>

`http://127.0.0.1:8787/` — zero build, same data as the panel, wider:

- **Live decision stream** — every request with its judgment, chosen model, tags and a plain-language reason
- **Decision autopsy** — click a row to see the 8-signal judgment, dual-judge source, candidate funnel, scoring matrix, regret, fallback hops with per-hop causes, and the exploration gate as it actually evaluated
- **Latency waterfall** — preprocess / decision / TTFT / streaming, per request
- **Providers** — add, edit, scan; tier allowances (5h/week/month); provider-reported quotas with one-click reconciliation; **provenance per provider** (`user` / `discovered` + the config file it came from)
- **Plans, trends, flywheel** — per-plan usage, model trends, learned bias / reliability / calibration
- **Benchmarks** — leaderboard-derived tier overlay (LMArena and friends) with review reminders

## Configuration

One canonical file: **`~/.evo-router/evo-router-config.jsonc`** — JSONC (JSON with comments), so the file documents itself. `evo-router-config.json` is the comment-free variant. Older names (`evo-router.toml`, `evo-router.jsonc`, `evo-router.json`, `evolve.toml`) are still read and **converted on startup**, the original kept as `*.migrated`.

| Section | Takes effect |
|---|---|
| `decision` (slots, timeout, think_mode) and `providers` | **hot-reloaded in ~5 s**; `POST /api/reload` forces it |
| `server`, `policy`, `models`, `discovery`, `data`, … | restart the gateway (panel **Restart**, `launchctl kickstart -k gui/$UID/ai.evolve-router.gateway`, or Ctrl-C + `evo-router serve`) |

`GET /api/live` reports the truth so you never guess:

```json
{
  "version": "0.1.9",
  "inflight": 1,
  "inflight_detail": [{ "model": "zhipuai-coding-plan/glm-5.3", "session": "…", "age_ms": 42000 }],
  "config":   { "reloaded_at_ms": 1792140000000, "restart_required": false,
                "path": "~/.evo-router/evo-router-config.jsonc", "active_slots": ["jev"] },
  "decision": { "backend": "decision_model", "status": "ok",
                "last_judge": "decision_model+rules", "last_model_used": true,
                "last_fail": null, "consecutive_fails": 0, "retry_in_ms": 0 }
}
```

### Providers: tier · model list · quota · assignment

A provider is more than a base URL. The **Providers card** (panel and dashboard alike) manages four things, and says so on the card:

| Duty | How it works | Where you see it |
|---|---|---|
| **Plan tier** | The plan is detected from the base URL and the tier you pick (`lite` / `pro` / `max` / `go` / `go-plus`) drives that plan's window allowances | tier chip on every row; editable per plan in the dashboard |
| **Model list** | The gateway fetches the provider's `/models` (and scans agent configs), and keeps the catalog in sync — new models appear, removed ones are pruned, protocol-unsupported ones are dropped in-process | `N models` on the row (endpoint-level, so two accounts on one endpoint agree), full list in the dashboard |
| **Quota probing** | Quota numbers come from the official API or upstream response headers where the vendor reports them, and are *estimated* otherwise; a 90 s watchdog refreshes the ones we can query, and the rest are probed with real requests every 5–10 min | `quota obs` / `quota hdr` / `quota est` marker + the quota bar and `5h / monthly full` countdowns |
| **Assignment** | An enabled, available provider's models join the **portfolio** (per-provider slots + capability × price coverage) — subject to provider health, the price-tier gate and per-model quota. Disabled, degraded, cooling or exhausted providers are marked and stay out of the main pool (they remain in the fallback sweep and are probed) | `assigned` / `disabled` / `unavailable` badge on the row |

### Where provider credentials come from

| Source | Contains | When |
|---|---|---|
| Agent configs (`~/.config/opencode/opencode.jsonc`, `~/.codex/config.toml`, …) | base_url + api_key | read at startup, **never copied** into our files |
| `~/.evo-router/state/overrides.json` | providers you added or edited in the panel / dashboard | written by the UI, replayed at startup |
| `providers` / `models` in the config | explicit declarations — `api_key_env` (env var name) or **inline `api_key`** (chmod 600) | config wins over overrides, which wins over discovery |

Priority is **config → overrides → discovery**, deduplicated inside the gateway on `base_url + api_key` (so one account listed twice is still one provider), and every row carries its provenance: `origin` (`user` / `discovered` / `builtin`) plus `source_path` (e.g. `opencode @ ~/.config/opencode/opencode.jsonc (OpenCode Go-github)` — two accounts on one endpoint stay distinguishable).

`POST /api/config/export` writes the live merged list (keys included, with provenance) back into the config file, after which the file alone reproduces your setup.

### Data layout

```
~/.evo-router/
├── evo-router-config.jsonc   the config (the only file you edit)
├── events/events.jsonl       append-only decision log (replay · stats · backtest), 32 MB rotation
├── state/                    snapshot · distilled cards · overrides · benchmark & catalog caches
└── logs/                     gateway-child.log (menu-bar) · gateway.log (LaunchAgent)
```

Older flat layouts and the pre-rename `~/.evolve` migrate automatically on startup — files are moved, never deleted; conflicts are kept aside as `*.old-<ts>`.

## Operations

```bash
curl -s localhost:8787/healthz                              # ok
curl -s localhost:8787/api/live | python3 -m json.tool      # version · judge · slots · inflight · restart_required
curl -sX POST localhost:8787/api/reload                     # apply decision/providers now
curl -sX POST localhost:8787/api/config/export              # materialise the provider list into the config
evo-router doctor                                           # config + catalog + credentials sanity check
evo-router backtest                                         # A/B replay of strategy params on real events (PGR verdict)
evo-router service install                                  # LaunchAgent: start / stop / restart / status
```

Every response also carries `x-ev-model`, `x-ev-reason`, `x-ev-decision-id`, `x-ev-est-tokens`, `x-ev-sticky` and `x-ev-skipped` headers; `x-ev-session` and `x-ev-policy` steer stickiness and profile per request, `x-ev-no-cache` bypasses the completion cache.

## Why not another proxy

| | EvolveRouter | LiteLLM / OpenRouter | RouteLLM |
|---|---|---|---|
| **Routing decision** | a decision model judges *every request* (8 signals) + rule second opinion, fail-open | config/prefix rules, or an offline-trained router | offline-trained router models |
| **Self-evolution** | flywheel: 4 feedback layers rewrite per-model bias & reliability | static weights | static after training |
| **Subscription economics** | per-plan credit multipliers, tier allowances (5h/week/month), quota protection (soft-deprioritize → sticky-break → hard-filter) | cost tracking only | – |
| **Failure handling** | key pools → per-provider/endpoint circuit breaking → winner queue → out-of-chain sweep | retries | – |
| **Protocol** | OpenAI ↔ Anthropic bidirectional translation, streaming included | OpenAI-centric | OpenAI |
| **Multi-agent** | native identity for opencode / codex / claude-code / pi / openclaw / hermes / dsh, session-aware stickiness | generic | generic |
| **Deployment** | one Rust binary (~7 MB), zero deps, no Docker, no DB | Python + DB | Python |

## Managed plans

| Plan | Vendor | Kind | Pricing model | Tiers | Windows |
|---|---|---|---|---|---|
| `zhipu-coding` | zhipu (bigmodel.cn) | Coding Plan | credits, off-peak 50 % | lite / pro / max | 5h · week · month |
| `zai-devpack` | Z.ai | Coding Plan | credits | lite / pro / max | 5h · week · month |
| `opencode-go` | opencode zen | Go Plan | $/1M per model + per-model monthly cap | go / go-plus | month (5h = 20 %) |
| `volces-coding` | volces | Coding Plan | AFP credits | lite / pro | 5h · week · month |
| `volces-agent` | volces | Agent Plan | AFP credits | — | 5h · week · month |
| `minimax-coding` | MiniMax | Coding Plan | credits | lite / pro / max | 5h · week · month |
| pay-as-you-go | any OpenAI / Anthropic | API | $ per token | — | — |

Plans are detected from base URLs; every window is tracked with provider-truth quota headers where the vendor reports them.

## Development

```bash
cargo build --release
cargo test          # 225 tests, run from the workspace root
cargo clippy        # zero-warning policy
```

| Crate | Role |
|---|---|
| `evolve-core` | decision engine, scoring, catalog, plan registry, config, thresholds, capability-card distillation |
| `evolve-decision` | decision-model backends (cloud / self-hosted / laya / heuristic), slot chain, route advisor |
| `evolve-discovery` | agent config scanning, remote `/models` fetching, models.dev & leaderboard enrichment |
| `evolve-memory` | flywheel, hourly buckets, quota ledger, health registry, session store, event log, strategy & backtest |
| `evolve-server` | axum gateway, OpenAI/Anthropic relay, dashboard, provider APIs, CLI (`evo-router`) |
| `apps/tray` | macOS menu-bar panel (SwiftUI) + panel preview harness |

Deep dives: [Architecture](docs/ARCHITECTURE.md) · [Decision flow](docs/DECISION-FLOW.md) · [Scoring factors](docs/FACTORS.md) · [macOS quick start](docs/QUICKSTART-macos.md) · [Papers](docs/reference.md) *(Chinese; English translation welcome)*

Research the design borrows from, critically: two-stage reward normalization and Bayesian exploration ([BayesianRouter][br]), baseline-relative bias updates and quota-aware preference ([BaRP][barp]), quality-triggered cascades ([FrugalGPT][fgpt]), replay-validated strategy changes ([MERA][mera]), capability-card distillation ([FlyRoute][flyroute]).

[br]: https://arxiv.org/abs/2510.02850
[barp]: https://arxiv.org/abs/2510.07429
[fgpt]: https://arxiv.org/abs/2305.05176
[mera]: https://arxiv.org/abs/2608.10333
[flyroute]: https://arxiv.org/abs/2605.22057

## License

[MIT](LICENSE)

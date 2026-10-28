# Favorites Routing Path Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Add a second routing path "Favorites" (user-curated per-provider candidate scope with emergent traffic mix) alongside the existing "Gateway" path, switchable hot via a mode toggle, with the priority banding reworked to multiplicative for both paths.

**Architecture:** Two routing modes share one engine. A new pure module (`evolve-core/src/scope.rs`) elects per-provider scope members (FAV/TOP/NEW + free + REC backfill). `meta.rs` dispatches `active_id_set` by `routing_mode`. The engine gains curated-only soft boosts (FAV ×1.30, NEW novelty decay) fed through `RoutingInput`. Tray and dashboard render mode-differentiated UI against two new APIs.

**Tech Stack:** Rust (evolve-core, evolve-server/axum, evolve-memory/flywheel), SwiftUI MenuBarExtra (apps/tray), vanilla JS dashboard (crates/evolve-server/src/dashboard.html).

**Spec:** `docs/superpowers/specs/2026-10-28-favorite-model-scope-design.md`

## Global Constraints

- Band factors (both paths): 0→×1.00, +1→×1.30, +2→×1.60, +3→×1.90, −1→×0.70, −2→×0.40 (−3 never reaches scoring; filtered as today).
- Curated-only constants: `FAV_SOFT_BOOST = 1.30`; `NEW_DECAY_BOOST = 0.40`; `NEW_DECAY_TOKENS = 50_000` (linear decay to 0).
- Scope slots per provider: FAV ≤3, TOP 2, NEW 1; dedup; all free models always in (FREE is a secondary tag only).
- Cell adjacency for 9-cell coverage: capability ±1 AND price ±1 counts as covering; no cell may stay empty while any unelected candidate fits it.
- Source tag vocabulary (English, uppercase): `FAV`, `TOP`, `NEW`, `REC`, `FREE`; primary-tag precedence FAV > TOP > NEW > REC; colors FAV `#84cc16`, TOP `#3b82f6`, NEW `#a855f7`, REC `#14b8a6`, FREE `#94a3b8`.
- Yesterday tokens = `in_tok + out_tok` summed over calendar "yesterday" (local time, same tz basis as `parse_absolute_window`).
- `routing_mode` values: `"gateway"` | `"favorites"`; serde default `gateway`.
- No new crate or SPM dependencies. `cargo test --workspace` green at every task end; Task 8 additionally `cd apps/tray && swift build`.
- Commit messages in English (open-source repo). Conventional prefixes (`feat:`, `refactor:`, `docs:`).
- Swift: new fields appended at the **end** of structs; no `.help()` in MenuBarExtra (tooltips don't fire).
- REC backfill "stable score" = `(tiers.coding + tiers.reasoning + tiers.agentic)/3 × weight_override(default 1.0) × price_eff`, where `price_eff`: free → 1.0, else price_rank inverted (cheap 1.0 / mid 0.66 / premium 0.33). No live telemetry (reliability/speed) in this score.

## Review Focus

- **Day-1 empty yesterday:** a provider with zero yesterday traffic must still get a full scope (FAV slots fall through to NEW; TOP slots backfilled by next-newest). → pinned by Task 2 `day1_empty_yesterday_falls_through_to_new`.
- **Ghost favorite id** (model deleted from catalog while in favorites): silently ignored at scope election; POST rejects with 404. → Task 2 `ghost_favorite_id_is_ignored`, Task 3 API test.
- **Free favorite:** gets primary FAV tag + FREE secondary; boost applied exactly once (no double counting). → Task 2 `free_favorite_gets_fav_primary_and_free_secondary`, Task 5 `fav_boost_applied_once`.
- **Explicit ▲▼ on a favorite:** no soft-boost stacking — explicit level replaces the ×1.30 default (only band factor applies). → Task 5 `explicit_priority_replaces_fav_soft_boost`.
- **Mode switch mid-session:** sticky sessions are not killed; the boost difference only shows at the next fresh decision. → Task 5 `sticky_continuation_ignores_curated_boosts`.

---

### Task 1: Multiplicative priority banding (both paths)

**Files:**
- Modify: `crates/evolve-core/src/engine.rs:11` (const), `engine.rs:944-952` (band application in `decide`)
- Test: `crates/evolve-core/src/engine.rs` (`mod tests` — existing `priority_band_dominates_formula_within_and_across_bands`, `pinned_bypasses_quality_floor`, `priority_off_removes_model_from_routing`)

**Interfaces:**
- Produces: `pub fn band_factor(level: i8) -> f32` in `engine.rs` — 0→1.0, +1→1.3, +2→1.6, +3→1.9, −1→0.7, −2→0.4, −3→0.4 (unreachable in practice). `PRIORITY_BAND_STEP` const removed.

- [ ] **Step 1: Rewrite the band test to multiplicative expectations**

Replace `priority_band_dominates_formula_within_and_across_bands` with `band_factor_multiplies_score`: same engine fixture; assert a +1 model's `dec.scored` score equals its un-tiered score × 1.30 (compare against the same model at level 0 via two `decide` calls with priorities set), +2 → ×1.60, −1 → ×0.70, −2 → ×0.40. Keep asserting a +1 model beats a materially better 0-level model only when formula gap < 30% (multiplicative semantics), and loses when gap > 30%.

- [ ] **Step 2: Run test to verify it fails**

Run: `cargo test -p evolve-core band_factor_multiplies_score`
Expected: FAIL (additive behavior / missing `band_factor`)

- [ ] **Step 3: Implement `band_factor` and swap the application site**

`engine.rs:950` `s.score += PRIORITY_BAND_STEP * (*b as f32);` → `s.score *= band_factor(*b);`. Delete `PRIORITY_BAND_STEP`. Do NOT touch `priority_of` / `max_positive_band` / the `band >= 1` quality-floor bypass — they keep explicit-overlay semantics.

- [ ] **Step 4: Run tests to verify they pass**

Run: `cargo test -p evolve-core`
Expected: PASS (update `pinned_bypasses_quality_floor` comment if it references band arithmetic; its assertions must hold unchanged)

- [ ] **Step 5: Commit**

```bash
git add crates/evolve-core/src/engine.rs
git commit -m "feat(core): multiplicative priority banding (x1.3/x1.6/x1.9, x0.7/x0.4)"
```

---

### Task 2: Per-provider scope election (pure module)

**Files:**
- Create: `crates/evolve-core/src/scope.rs`; register `pub mod scope;` in `crates/evolve-core/src/lib.rs`
- Test: `crates/evolve-core/src/scope.rs` (`#[cfg(test)]` at bottom)

**Interfaces:**
- Produces (used by Tasks 4 and 6):
```rust
pub enum SourceTag { Fav, Top, New, Rec }            // derives Debug, Clone, PartialEq, Eq
impl SourceTag { pub fn as_str(&self) -> &'static str }  // "fav" | "top" | "new" | "rec"
pub struct ScopeCandidate {
    pub id: String,
    pub cap_rank: u8,        // 0=basic 1=standard 2=powerful
    pub price_rank: u8,      // 0=cheap 1=mid 2=premium
    pub free: bool,
    pub release_ts: Option<u64>,
    pub yesterday_tokens: u64,
    pub stable_score: f32,
}
pub const FAV_SLOTS: usize = 3;
pub const TOP_SLOTS: usize = 2;
pub const NEW_SLOTS: usize = 1;
pub fn select_provider_scope(cands: &[ScopeCandidate], favorites: &[String]) -> Vec<(String, SourceTag)>
```
- Returns **elected members with their primary tag only** (free models without FAV/TOP/NEW/REC are not in the return; the caller adds them separately as untagged scope members).

- [ ] **Step 1: Write the failing tests** (all in `scope.rs` test module, fixture helper `cand(id, cap, price, free, release, ytok, score)`)

- `favorites_elected_first_with_fav_tag` — 3 favorites win FAV tag regardless of scores; a 4th favorite id is ignored (cap 3).
- `empty_fav_slots_fill_from_yesterday_top` — 1 favorite + `TOP` filling: remaining FAV slots take highest `yesterday_tokens`, tagged **Fav** (slot fallback keeps slot semantics, per spec "空位链 FAV→TOP→NEW").
- `day1_empty_yesterday_falls_through_to_new` — all `yesterday_tokens = 0` → FAV slots and TOP slots fill from newest `release_ts`, tagged New; scope never empty while candidates exist.
- `dedup_highest_precedence_tag` — a model that is both favorite and yesterday-top appears once, tagged Fav.
- `free_models_not_elected_by_slots` — free candidates are never elected into FAV/TOP/NEW slots by tokens/release; they join via the caller (assert `select_provider_scope` output excludes a free model that isn't a favorite).
- `free_favorite_gets_fav_primary_and_free_secondary` — a favorite free model IS elected with Fav (function returns primary only; assert it is present with `Fav`; FREE secondary is the caller's render concern — assert and document).
- `ghost_favorite_id_is_ignored` — favorite id absent from `cands` silently skipped.
- `rec_backfills_empty_cells_by_stable_score` — build candidates covering only (2,0)-(2,1) cells; assert cells (0,*) and (1,*) get backfill members tagged Rec, chosen by highest `stable_score` among candidates whose (cap,price) sits within ±1 adjacency of the cell; already-covered cells get no backfill.

- [ ] **Step 2: Run tests to verify they fail**

Run: `cargo test -p evolve-core scope::`
Expected: FAIL (module empty)

- [ ] **Step 3: Implement `select_provider_scope`**

Algorithm (doc-comment it in Chinese, matching codebase style): ① favorites (input order, cap `FAV_SLOTS`, skip ghosts, skip non-existent) → ② remaining FAV slots: `yesterday_tokens` desc (zero-token excluded) → fallback newest `release_ts` desc → ③ TOP slots: yesterday desc excluding elected → fallback newest → ④ NEW slot: newest excluding elected → ⑤ 9-cell coverage: for each cell (cap 0..3 × price 0..3), if no elected member has `|Δcap|≤1 && |Δprice|≤1`, elect the highest-`stable_score` non-free non-elected candidate that satisfies adjacency, tag Rec; repeat until covered or exhausted. Ties broken by id asc everywhere (deterministic).

- [ ] **Step 4: Run tests to verify they pass**

Run: `cargo test -p evolve-core scope::`
Expected: PASS (all 8)

- [ ] **Step 5: Commit**

```bash
git add crates/evolve-core/src/scope.rs crates/evolve-core/src/lib.rs
git commit -m "feat(core): per-provider favorites scope election (fav/top/new/rec)"
```

---

### Task 3: Overrides fields + routing-mode & favorites APIs

**Files:**
- Modify: `crates/evolve-server/src/state.rs:1448` (`Overrides` struct), route table `state.rs:~1413`
- Modify: `crates/evolve-server/src/meta.rs` (new handlers, near `api_priority_set`)
- Test: `crates/evolve-server/tests/favorites_api.rs` (new; harness copied from `tests/proxy.rs` — `app.oneshot(req)` pattern)

**Interfaces:**
- Consumes: Task 2 types for scope preview (GET only needs favorites list + catalog, not scope).
- Produces:
  - `Overrides` gains (appended at struct end, `#[serde(default)]`):
    ```rust
    pub routing_mode: RoutingModeCfg,          // enum RoutingModeCfg { Gateway, Favorites }, serde lowercase, default Gateway
    pub favorites: HashMap<String, Vec<String>>, // provider → model ids, server-enforced ≤3
    ```
  - `GET /api/routing-mode` → `{"mode": "gateway"|"favorites"}`
  - `PUT /api/routing-mode` body `{"mode": "..."}` → 200 `{"status":"ok","mode":...}`; unknown mode → 400
  - `GET /api/models/favorite?provider=X` → `{"provider":..., "favorites":[ids], "models":[{"model","price_tier","yesterday_tokens","fail_pct","ttft_ms","favorite":bool}]}` (`yesterday_tokens` may be `0` placeholder until Task 4 wires real data; keep the field)
  - `POST /api/models/favorite` body `{"provider","model","replace"?}` → append; ghost id → 404; len would exceed 3 without `replace` → 400 `{"error":{"message":"3 favorites set — pass replace"}}`; with `replace` = existing member → swap in place; unknown `replace` → 400. Persistence via `crate::state::update_overrides`.

- [ ] **Step 1: Write failing API tests** in `favorites_api.rs`: get-default-returns-gateway; put-favorites-roundtrips-and-persists (assert via `load_overrides`); put-unknown-mode-400; post-favorite-appends; post-ghost-404; post-4th-without-replace-400; post-replace-swaps (assert order in overrides).
- [ ] **Step 2: Run to verify failure** — `cargo test -p evolve-server --test favorites_api` → FAIL (no routes).
- [ ] **Step 3: Implement** struct fields + enum + 4 handlers + route registrations (`get`/`put`/`post` routing, following `api_priority_set`'s ghost-id 404 pattern).
- [ ] **Step 4: Run to verify pass** — same command → PASS; also `cargo test -p evolve-server` (existing suites untouched).
- [ ] **Step 5: Commit**

```bash
git add crates/evolve-server/src/state.rs crates/evolve-server/src/meta.rs crates/evolve-server/tests/favorites_api.rs
git commit -m "feat(server): routing-mode and model-favorites APIs"
```

---

### Task 4: Yesterday tokens + scope wiring behind routing_mode

**Files:**
- Modify: `crates/evolve-memory/src/flywheel.rs` (new method near `hourly_window`, ~line 473)
- Modify: `crates/evolve-server/src/meta.rs` (`active_id_set` ~2153, new helpers near `portfolio_selection` ~2191)
- Test: `crates/evolve-memory/src/flywheel.rs` tests (`mod hourly_tests` pattern); `crates/evolve-server/src/meta.rs` unit tests if a pure seam exists, else covered via Task 3 harness style

**Interfaces:**
- Produces:
  - `pub fn tokens_window(&self, start_ms: u64, end_ms: u64) -> HashMap<String, u64>` — per model, sum `in_tok + out_tok` over `HourBucket`s in range.
  - `pub fn routing_mode(st: &AppState) -> RoutingModeCfg`
  - `pub fn yesterday_token_map(st: &AppState) -> HashMap<String, u64>` — calendar yesterday (local tz, reuse `parse_absolute_window`'s basis), 5-min TTL static cache (pattern: `LAST_USED_CACHE`).
  - `pub fn favorite_scope_selection(st: &AppState) -> (Vec<String>, HashMap<String, SourceTag>)` — builds `ScopeCandidate`s from catalog (cap/price via `tags_for_catalog`, `release_ts` via `evolve_core::recency::release_ts_of`, `stable_score` per Global Constraints, `free` via `is_free`), eligibility filters identical to `portfolio_selection` (no-key/deleted/self-endpoint excluded), calls `select_provider_scope`, then appends free models (untagged). 30s TTL cache keyed by catalog len + favorites hash (pattern: `LAST_USED_CACHE`).
  - `active_id_set(st)` now dispatches: `Favorites` → scope ids; `Gateway` → existing `portfolio_selection` path unchanged.
- Consumes: Task 2 `select_provider_scope`, Task 3 `routing_mode` field.

- [ ] **Step 1: Write failing test** `tokens_window_sums_in_and_out` in `flywheel.rs` tests: seed two buckets (in 100/out 50, in 10/out 5), assert `tokens_window` → 165 for that model, and a model with buckets only outside range → 0/absent.
- [ ] **Step 2: Run to verify failure** — `cargo test -p evolve-memory tokens_window` → FAIL.
- [ ] **Step 3: Implement `tokens_window`** (iterate `inner.hourly` buckets, filter range, sum).
- [ ] **Step 4: Run to verify pass** — `cargo test -p evolve-memory` → PASS.
- [ ] **Step 5: Implement `favorite_scope_selection` + dispatch in `active_id_set`** (+ TTL caches). No new public API otherwise.
- [ ] **Step 6: Verify** — `cargo test --workspace` → PASS. Task 3's GET assertions stay valid (field present; value may remain 0 without yesterday traffic), no rewrite needed.
- [ ] **Step 7: Commit**

```bash
git add crates/evolve-memory/src/flywheel.rs crates/evolve-server/src/meta.rs
git commit -m "feat(server): favorites scope wiring (yesterday tokens, active set dispatch)"
```

---

### Task 5: Curated-only engine boosts (FAV soft, NEW decay)

**Files:**
- Modify: `crates/evolve-core/src/engine.rs` (`RoutingInput` — append fields at struct end; scoring section right after the band application from Task 1)
- Test: `crates/evolve-core/src/engine.rs` tests

**Interfaces:**
- Produces (consumed by Task 6's relay construction):
  - `RoutingInput` appended fields:
    ```rust
    pub curated: bool,                                              // routing_mode == Favorites
    pub favorites: &'a std::collections::HashSet<String>,
    pub model_tokens_total: &'a std::collections::HashMap<String, u64>, // completed tokens, for NEW decay
    ```
  - `pub const FAV_SOFT_BOOST: f32 = 1.30; pub const NEW_DECAY_BOOST: f32 = 0.40; pub const NEW_DECAY_TOKENS: f32 = 50_000.0;`

- [ ] **Step 1: Write failing tests** (use the existing `input_with` fixture; append the 3 new fields with defaults `false`/empty so old call sites compile)

- `curated_fav_soft_boost_multiplies_score` — curated=true, model in favorites, no explicit priority → score = baseline × 1.30; curated=false → baseline.
- `curated_explicit_priority_replaces_fav_boost` — same favorite with explicit +1 → score = baseline × band_factor(1) = ×1.30 only (i.e. identical to explicit +1 on a non-favorite; assert no ×1.30²).
- `curated_new_decay_bonuses_then_vanishes` — curated model_tokens_total = 0 → ×1.40; = 25_000 → ×1.20; = 50_000 → ×1.00 (only in curated mode).
- `curated_fav_boost_applied_once` — favorite that is also free: exactly one ×1.30 (score equals non-free favorite's boosted score given identical fixture fields).
- `curated_sticky_continuation` — with sticky state hit, decision reuses `sticky.chosen` even when curated favorites contain a different model (sticky path unchanged).

- [ ] **Step 2: Run to verify failure** — `cargo test -p evolve-core curated` → FAIL.
- [ ] **Step 3: Implement** — after the band block: `if input.curated { ... }` — FAV boost only `if !prio_map.contains_key(&s.model_id) && input.favorites.contains(&s.model_id)`; NEW decay for all scored models in curated mode. Boosts are score-only; do not touch `priority_overlay`, floor bypass (`band >= 1`), or sticky comparisons.
- [ ] **Step 4: Run to verify pass** — `cargo test --workspace` → PASS (Task 1's `input` fixtures get the appended defaults).
- [ ] **Step 5: Commit**

```bash
git add crates/evolve-core/src/engine.rs
git commit -m "feat(core): curated-mode fav soft boost and new-model decay"
```

---

### Task 6: Relay wiring — RoutingInput population + source in events

**Files:**
- Modify: `crates/evolve-server/src/relay.rs` (`RoutingInput` construction ~line 245; chat event records ~lines 126, 139, 214)
- Test: `crates/evolve-server/tests/favorites_api.rs` (extend) — scope-dependent integration; primary verification is `cargo test --workspace` + one new test

**Interfaces:**
- Consumes: Task 4 `routing_mode` / `favorite_scope_selection` / flywheel cumulative tokens (add `pub fn tokens_total(&self) -> HashMap<String, u64>` to flywheel — sum over all hourly buckets; test: same fixture style as Task 4 Step 1).
- Produces: chat events carry `"source": "fav"|"top"|"new"|"rec"|null` (null when chosen model untagged or mode is gateway).

- [ ] **Step 1: Write failing test** `chat_event_carries_source_tag` in `favorites_api.rs`: switch mode to favorites via PUT, POST a favorite, fire one chat request routed to the favorite (direct-model path is fine), read events via `GET /api/events?limit=1` → `source == "fav"`.
- [ ] **Step 2: Run to verify failure** — FAIL (no `source` field).
- [ ] **Step 3: Implement** — in relay's `Target::Auto` branch: populate `curated` (mode), `favorites` (from `effective_overrides(st).favorites` flattened), `model_tokens_total` (flywheel `tokens_total()`); compute `scope_source_map` (from `favorite_scope_selection`) once and add `"source"` to the main chat record and the cache-hit record.
- [ ] **Step 4: Run to verify pass** — `cargo test --workspace` → PASS.
- [ ] **Step 5: Commit**

```bash
git add crates/evolve-memory/src/flywheel.rs crates/evolve-server/src/relay.rs crates/evolve-server/tests/favorites_api.rs
git commit -m "feat(server): curated inputs to engine, source tag in chat events"
```

---

### Task 7: Retire the enable/disable switch

**Files:**
- Modify: `crates/evolve-server/src/state.rs` (`ActiveOverride` struct, `Overrides.active_models` field, `/api/models/active` route)
- Modify: `crates/evolve-server/src/meta.rs` (`api_models_active`, `portfolio_selection` exclude/pinned reads, `api_model_tags_get` active computation)
- Modify: `crates/evolve-core/src/recency.rs` (`PortfolioItem.pinned`, `select_portfolio` step ①, related tests)
- Modify: `crates/evolve-server/src/dashboard.html`, `apps/tray/Sources/TrayCore/PanelView.swift` + `AppState.swift` (any callers)
- Test: touched crates' existing suites

**Interfaces:**
- Produces: `api_model_tags_get`'s `"active"` field **stays** but now means "in current mode's routing scope" (gateway: portfolio; favorites: scope). `include`/`exclude`/`replaced` are gone. `Overrides` loses `active_models`.
- Note: `"pinned"` flag in `api_portfolio` output: remove (pinned concept retired; `active` covers membership).

- [ ] **Step 1: Write failing test** in `favorites_api.rs`: `GET /api/models/active` → 404 (route gone); `GET /api/model-tags` → has `active: bool` and no `pinned`.
- [ ] **Step 2: Run to verify failure** — FAIL.
- [ ] **Step 3: Remove** — `grep -rn "active_models\|models/active\|pinned" crates apps/tray/Sources crates/evolve-server/src/dashboard.html` and excise: struct + field, handler + route, `portfolio_selection` exclude/pinned code, `PortfolioItem.pinned`, `select_portfolio` ① pin step (renumber comments), dashboard enable/disable UI + copy, tray callers of `/api/models/active` (AppState). Keep `active_id_set` / `"active"` field semantics per Interfaces.
- [ ] **Step 4: Run to verify pass** — `cargo test --workspace` → PASS.
- [ ] **Step 5: Commit**

```bash
git add -A crates apps/tray/Sources
git commit -m "refactor: retire active-set enable/disable switch (off/pinned/favorites cover it)"
```

---

### Task 8: Tray — mode switch, favorites card, picker, source tags

**Files:**
- Modify: `apps/tray/Sources/TrayCore/AppState.swift` (appended `@Published` fields + async funcs mirroring `setPriority`)
- Modify: `apps/tray/Sources/TrayCore/PanelView.swift` (header row, new card views, model-row pills)
- Modify: `apps/tray/Sources/TrayCore/I18n.swift` (new keys)

**Interfaces:**
- Consumes: Task 3/6 HTTP APIs (`/api/routing-mode`, `/api/models/favorite`), `source` field in `/api/model-tags` responses.
- Produces: `AppState.routingMode: String` (`"gateway"|"favorites"`), `AppState.favorites: [String: [String]]`, `func setRoutingMode(_ mode: String) async`, `func setFavorite(provider: String, model: String, replace: String?) async`, `func removeFavorite(provider: String, model: String) async`.

- [ ] **Step 1: Implement AppState additions** (URLSession calls following `setPriority`'s error handling; all new keys in I18n.swift, English copy: "Gateway", "Favorites", "No favorites picked — running on auto defaults", "Choose", "search…", column headers "y-day", "fail", "ttft", "price").
- [ ] **Step 2: Implement PanelView UI** — header inline compact toggle (B mockup: two segments right of title); when mode == favorites show: passive nudge banner (hidden once any favorite exists; "Choose" opens picker for first provider with an empty slot), FavoritesCard (per-provider chips `provider: model ×` + dashed `+` slots), ProviderPickerSheet (searchable list from `GET /api/models/favorite`, columns y-day/fail/ttft/price, tap row → `setFavorite` with `replace` when swapping a chip), source tag pills on model rows (tinted-outline style, colors per Global Constraints, FREE secondary tag), source-distribution mini card (client-side aggregate of `GET /api/events` last 500 chat events by `source`). Gateway mode: none of these render; panel as today.
- [ ] **Step 3: Build** — `cd apps/tray && swift build` → exit 0 (no Swift test target exists; UI verified by build + manual review step below).
- [ ] **Step 4: Manual check** — run the app against a dev gateway: toggle modes, add/replace/remove a favorite, confirm banner hides, tags render. Note results in the PR description.
- [ ] **Step 5: Commit**

```bash
git add apps/tray/Sources
git commit -m "feat(tray): gateway/favorites mode switch with favorites card and picker"
```

---

### Task 9: Web dashboard — mode switch, tags, favorites editor, source distribution

**Files:**
- Modify: `crates/evolve-server/src/dashboard.html` (header, models list rendering, new card, i18n dict ~line 358+)

**Interfaces:**
- Consumes: same APIs as Task 8.

- [ ] **Step 1: Implement** — header segmented control (`Gateway`/`Favorites`, zh labels 网关智能/心选智能 in the i18n dict) writing `PUT /api/routing-mode`; models list renders tinted source pills only in favorites mode; favorites editor card per provider (chips + picker with the four compare columns, reusing `GET /api/models/favorite`); source-distribution card (client-side aggregate of `/api/events`); i18n entries for every new string (en + zh at minimum, following the existing 5-language dict shape for new keys where trivially translatable).
- [ ] **Step 2: Verify** — `cargo build -p evolve-server` → exit 0; serve dev gateway, open dashboard: toggle persists across reload (overrides), pills render in favorites mode only, editor swaps favorites.
- [ ] **Step 3: Commit**

```bash
git add crates/evolve-server/src/dashboard.html
git commit -m "feat(web): mode switch, source tags and favorites editor in dashboard"
```

---

### Task 10: Docs sync

**Files:**
- Modify: `docs/plans/model-priority.md` (band semantics table: multiplicative factors)
- Modify: `README.md`, `README.zh-CN.md` (feature section: dual-path routing, one short paragraph each)

- [ ] **Step 1: Update** both docs — priority table gains the ×1.30/×1.60/×1.90/×0.70/×0.40 semantics; README gains a "Gateway / Favorites routing paths" paragraph (en) and「网关智能 / 心选智能」(zh).
- [ ] **Step 2: Commit**

```bash
git add docs README.md README.zh-CN.md
git commit -m "docs: dual-path routing and multiplicative priority banding"
```

# Work-intensity chart window

## Context

The app already polls Anthropic usage limits every ~10 min and appends each reading to an append-only `usage_history.jsonl` (`UsageHistoryStore`, ~2400 records / ~19 days so far). That data is currently write-only — nothing reads it back. We want to *use* it: visualize **when** and **how intensely** Claude has been working.

The signal is the **5h-limit utilization** (`five_hour_pct`, raw 0..100, cumulative within the rolling 5h window). The *rate of consumption* — the delta between consecutive observations — is a proxy for work intensity. Binning that delta into fixed 10-minute buckets (6/hour, 144/day, 1008/week) yields a per-week activity chart.

Reference "full intensity" = `100 / 5 / 6 = 3.333…%` per 10-min bucket: the rate at which 5 hours of sustained work exhausts exactly 100% of the 5h limit. It's drawn as a reference line.

**Outcome:** a new always-available secondary window (mirroring the existing `history`/`about` windows) opened from a tray item, rendering the current week (with prev/next navigation) as **7 day-rows, each a day's 144 ten-minute bars across time-of-day**, auto-scaled to the week's peak with the reference line overlaid.

**Decisions locked in:** Layout = 7 day-rows × time-of-day; Y-axis = auto-scale to week peak, reference as a line. **Defaults taken** (stated, not asked — easy to change): week starts **local Monday 00:00**; the intensity window is an **independent** full window (does not hide together with the main dashboard).

All binning/delta/reset/gap logic lives in a **pure, unit-tested Rust function**; the command returns a ready-to-render bucket array; the frontend only draws. This follows the project's "stamp Rust decisions onto data fields; don't re-derive in TS" convention.

## Rust changes

### `src-tauri/src/usage_history.rs` — read method, model, pure binning, tests

This file owns the data, so the pure function and its `#[cfg(test)]` tests sit here next to `UsageHistoryRecord`.

1. **`read_all(&self) -> Vec<UsageHistoryRecord>`** on `impl UsageHistoryStore`: hold `write_lock` for a consistent read, `read_to_string`, `lines().filter_map(serde_json::from_str)` (skip malformed), `sort_by_key(|r| r.ts)` (defensive against a clock step). Missing file → empty vec. Re-reading per call is cheap at this size; no caching.

2. **Constants:**
   - `BUCKET_MS = 10*60*1000`, `BUCKETS_PER_WEEK = 1008`, `WEEK_MS = BUCKET_MS * 1008`.
   - `FULL_INTENSITY_PCT: f32 = 100.0/5.0/6.0` — the reference marker.
   - `GAP_THRESHOLD_MS = 3*BUCKET_MS` (30 min): an inter-observation gap longer than this marks the spanned interior as *no data* (app closed / poller stopped), distinct from idle.

3. **Serialize-able model** (snake_case, matches the TS interface):
   - `WeekBucket { intensity: f32 (>=0, % of 5h limit consumed in this 10-min slot), has_data: bool }`
   - `WeekChart { week_start_ms, week_end_ms, buckets: Vec<WeekBucket> (len 1008), data_min_ms: Option<i64>, data_max_ms: Option<i64>, full_intensity: f32 }`

4. **`build_week_chart(records: &[UsageHistoryRecord], week_start_ms: i64) -> WeekChart`** — pure, clock-free, tz-free (all timezone logic stays in the command):
   - Init 1008 buckets `{intensity:0, has_data:false}`; `data_min/max_ms` from first/last record (sorted).
   - Walk consecutive pairs `(prev, cur)`:
     - Skip if either `five_hour_pct` is `None`, or `dt = cur.ts - prev.ts <= 0`.
     - **Reset detection:** `reset = (both resets_at Some && differ) || cur_pct + EPS < prev_pct` (EPS≈0.01). `resets_at` change is primary; negative-delta is the fallback.
     - `delta = if reset { cur_pct } else { cur_pct - prev_pct }`, then `.max(0.0)` — never negative. On reset, attribute the post-reset accumulation (`cur_pct`).
     - `is_gap = dt > GAP_THRESHOLD_MS`.
     - Clip `[prev.ts, cur.ts]` to `[week_start, week_end]`; skip if no overlap.
     - For each overlapped bucket: if `is_gap`, leave it (`has_data:false`); else set `has_data:true` and add **time-weighted** intensity `rate = delta/dt; intensity += rate * overlap_ms_in_bucket` (splits an interval straddling a boundary; gives a true per-10-min rate).
   - The bucket holding a post-gap observation is reclaimed by the next non-gap interval, so only the gap *interior* is no-data. Idle = `has_data:true, intensity:0`; gap = `has_data:false`.

5. **Tests** (`#[cfg(test)]`, fixed `WEEK_START` on a bucket boundary; `rec(ts,pct,resets)` helper): normal accumulation (deltas sum, trailing bucket stays no-data); time-weighting across a boundary (15-min/Δ3 → ~2.0+~1.0); reset mid-week (80→5, no negative, post-reset ≈5); multi-hour gap (interior no-data, next interval re-marks data); empty input (all no-data, len 1008, min/max None); week clipping (straddling `week_start` contributes only in-week fraction); `None` pct between valid records (skipped, no panic). Plus a `read_all` round-trip via the existing `temp_store` helper.

### `src-tauri/src/commands.rs` — the command

`#[tauri::command] pub fn get_usage_intensity_week(week_offset: i32, app: AppHandle) -> Result<WeekChart, String>`:
- Resolve week start in **local time** (like the history clock): `chrono::Local::now()` → date → back to Monday → `and_hms(0,0,0)` → shift `week_offset` weeks via `Duration::weeks` → `timestamp_millis()`. `0` = current week, `-1` = previous. DST caveat (fixed 7×24×6 grid can be ±1h in the last bucket on a DST week) noted in a comment — acceptable for a personal dashboard.
- `let records = app.state::<UsageHistoryStore>().read_all();` then `Ok(usage_history::build_week_chart(&records, week_start_ms))`.
- `week_offset` keeps week-alignment authoritative in Rust (no client date math). The returned `data_min/max_ms` + `week_start/end_ms` let the UI gate nav: disable **next** when `week_offset >= 0`; disable **prev** when `data_min_ms` is set and `week_start_ms <= data_min_ms`.

### `src-tauri/src/lib.rs`

- Register `commands::get_usage_intensity_week` in the `generate_handler!` block (~line 119–145).
- Add an `"intensity"` arm to `on_window_event` (~line 317–337) mirroring `"about"`: `api.prevent_close(); let _ = window.hide();` so the webview stays warm across closes.

### `src-tauri/tauri.conf.json` + `src-tauri/capabilities/default.json`

- Add a window after `about` (`tauri.conf.json` ~line 39–54): `{ "label": "intensity", "title": "Work intensity", "width": 920, "height": 520, "decorations": true, "resizable": true, "visible": false, "acceptFirstMouse": true, "theme": "Dark" }`.
- **Required (easy to miss):** add `"intensity"` to `windows` in `capabilities/default.json:5` (`["main","history","about","intensity"]`) or the webview silently can't invoke commands.

### `src-tauri/src/tray.rs`

Mirror the `show_about` pattern (`tray.rs:471`): const `MENU_OPEN_INTENSITY = "open_intensity"`; `MenuItem::with_id(app, MENU_OPEN_INTENSITY, "Work intensity", true, None)`; insert into `Menu::with_items` (after `open_data_dir`, above the Help submenu); match arm `MENU_OPEN_INTENSITY => show_intensity(app)`; handler does `get_webview_window("intensity").show()+set_focus()`. Sentence-case label. Window stays independent of the main-window hide.

## Frontend changes

### `src/lib/types.ts` / `src/lib/api.ts`

- Add `WeekBucket` and `WeekChart` interfaces (snake_case fields matching the Rust serialize).
- `getUsageIntensityWeek(weekOffset: number): Promise<WeekChart>` → `invoke('get_usage_intensity_week', { weekOffset })`.

### `src/IntensityApp.svelte` (new root component, Layout A)

Theme/parity with `HistoryApp.svelte`: dark `#1c1c1e`, `#d6d6d6` text, system-ui + monospace for numbers, sentence-case strings, `<svelte:window onkeydown>` with **Esc → `closeWindow()`** and **←/→ → prev/next week** (bounds-gated).

- State: `weekOffset = $state(0)`, `chart = $state<WeekChart|null>(null)`. `onMount` fetches week 0; optional `onUsageLimitsUpdated(() => { if (weekOffset===0) reload() })` for live refresh (nice-to-have).
- Header: week-range label (`new Date(week_start_ms)…`), prev/next buttons `disabled` per the §command nav contract.
- **Render (Layout A, `<canvas>` to avoid ~1000 DOM nodes):** 7 stacked day-rows (Mon→Sun), each row a band of 144 columns = `bucket_index % 144` is time-of-day, `bucket_index / 144` is the row.
  - **Shared auto-scale:** global `maxIntensity` = max `intensity` over all `has_data` buckets in the week; every row scales to it so bars are comparable across days. Quiet weeks zoom in automatically.
  - Bar height ∝ `intensity / maxIntensity`. Optional green→amber→red ramp by `intensity / full_intensity` (reuse a lerp like `colorAtPercent` in `types.ts`).
  - **Reference line** drawn in each row at the y for `full_intensity / maxIntensity` (dashed, labeled once, e.g. "full 5h pace"). If `maxIntensity < full_intensity`, the line sits at the top.
  - `has_data:false` → faint diagonal-hatch / dim band (clearly "no data", as the chosen preview shows for a closed day). `has_data:true, intensity:0` → baseline tick only (idle).
  - Gridlines: row labels (Mon–Sun) on the left; x-axis ticks at 0/6/12/18/24h under the bottom row; faint hour gridlines every 6 buckets. Fit-to-width (whole week visible).

### `src/App.svelte` routing (4 edits)

- Import `IntensityApp`; `let intensityMode = $state(false)`.
- In `onMount` after the `about` check (~line 178): `if (label === 'intensity') { intensityMode = true; return }`.
- Extend the conditional render (~line 286): `{:else if intensityMode} <IntensityApp />`.
- Extend the auto-reveal guard in `finally` (~line 236): `if (!historyMode && !aboutMode && !intensityMode) { … showWindow() }` — shown on demand by the tray, like history/about.

## Implementation order

1. `usage_history.rs`: `read_all`, constants, model, `build_week_chart`, tests.
2. `commands.rs`: `get_usage_intensity_week`.
3. `lib.rs`: register command + `"intensity"` close arm.
4. `tauri.conf.json` + `capabilities/default.json`: window + capability.
5. `tray.rs`: const, item, menu insert, match arm, `show_intensity`.
6. `types.ts` + `api.ts`.
7. `IntensityApp.svelte` (Layout A canvas).
8. `App.svelte` routing.

## Verification

1. `cargo test` in `src-tauri/` — the `build_week_chart` cases + `read_all` round-trip pass.
2. Deploy via `bash scripts/deploy.sh` — compiles, command registered.
3. Tray → "Work intensity" opens the window; Esc closes; reopen is instant (kept warm).
4. Eyeball the current week against reality: busy coding afternoons → tall bars near/above the reference line; overnight → flat `has_data` zero rows; app-closed periods → distinct no-data hatch (like Wed in the chosen preview). Cross-check a known-busy timestamp against the History window.
5. ←/→ navigation: **prev** disables at the oldest data (~19 days back), **next** disables at the current week; a row boundary lines up with local Monday/each day's 00:00.
6. A 5h-window rollover (`five_hour_resets_at` change) shows no negative/black bar — intensity stays ≥ 0.

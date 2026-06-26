# Sync usage stats across dashboards to fill Work-intensity gaps

## Context

The dashboard already syncs sessions + dialogs across a user's devices (`sync.rs`,
`remote_history.rs`). It does **not** sync the Anthropic usage-limit polls that feed the
Work intensity chart. Each device only sees the windows in which *its own* app was running:
close the laptop and its `usage_history.jsonl` has a multi-hour gap (rendered as no-data
buckets) even though the desktop was polling the whole time.

Since the 5h/7d usage counter is **account-wide** (same Anthropic account on every device —
the assumption this feature rests on), another device's polls during your downtime describe
the *same* timeline. The goal (memo 2026-06-26): each device shares its usage records to
peers, peers keep them **per-device, separately on disk**, and merge them into the
Work-intensity chart so a gap on one device is filled by another's coverage. Confirmed
choices: filled bars render **seamlessly** (no visual distinction), and syncing rides the
**existing sync gate** (no new config flag — active whenever token + peers are configured).

## Approach

Mirror the existing remote-dialog machinery (`remote_history.rs`) for usage records:
piggyback a usage delta on the existing sync push, store it per-device on disk, and union
local + remote records at chart-build time. The chart math (`build_week_chart`) already
handles a unioned, sorted timeline correctly — it walks consecutive 5h-pct deltas with a
`max(0, Δ)` clamp, so interleaving same-account polls from two devices just adds
finer-grained points (zero-ish deltas where they overlap, real intensity across a gap).
No changes to `build_week_chart`.

### 1. `usage_history.rs` — make the record cloneable
- Add `Clone` to `UsageHistoryRecord`'s derive (needed to copy into the remote store and
  the push payload). No other change.

### 2. `remote_usage.rs` — NEW per-device store (mirror `remote_history.rs`)
- `RemoteUsageStore { dir, data: Mutex<HashMap<String, DeviceUsage>> }`, one
  `remote_usage/<sanitized-device>.json` file per peer device. Reuse the
  `sanitize_filename` pattern; store `{ device, records: Vec<UsageHistoryRecord> }` so the
  filename never needs reversing. Lives under the app-data dir, so it survives the deploy
  step that wipes `config.json` (same rationale as `remote_history`).
- `new(dir)` loads all `*.json` on startup (tolerant of malformed/empty, like
  `RemoteHistoryStore::new`).
- `merge_device(&self, device, &[UsageHistoryRecord])` — append records whose `ts` is newer
  than the device's current max (cheap dedup against replays), keep sorted ascending, persist.
- `all_records(&self) -> Vec<UsageHistoryRecord>` — flatten every device's records (for the
  chart merge). Keeps all history (unbounded, matching the local store; records are ~80 bytes).
- `#[cfg(test)]`: round-trip save/load, merge dedups by ts + stays sorted, separate files per
  device (incl. unsafe-char sanitize), missing dir loads empty, `all_records` flattens across
  devices. Mirror the `remote_history.rs` test set.

### 3. `sync.rs` — carry usage records on the push
- Add to `SyncPush`: `#[serde(default) ] pub usage_delta: Vec<UsageHistoryRecord>` (top-level,
  not per-session — usage is a global timeline). `serde(default)` keeps legacy/heartbeat
  pushes valid.
- Pure helper `usage_delta_since(records: &[UsageHistoryRecord], watermark: i64) -> Vec<…>`
  = records with `ts > watermark` (records already sorted ascending). Unit-tested.
- `push_all`: add a `usage_watermarks: &mut HashMap<String,i64>` param (separate from the
  dialog `watermarks`). Read local usage once per cycle via `UsageHistoryStore::read_all()`.
  Per peer, compute `usage_delta_since(&usage, uw)`; attach it to the **first** drain-loop
  chunk only (`chunk.push.usage_delta = …`, then a `usage_sent` flag suppresses it on
  subsequent dialog-backlog chunks so it isn't resent). On that chunk's 2xx, advance
  `usage_watermarks[peer]` to the last included `ts`. Failure leaves the watermark, so the
  next successful push resends — same offline-safe contract as dialogs.
- `spawn_pusher`: create the `usage_watermarks` map alongside `watermarks` and thread it in.
- `post_sync` (receiver): after the session ingest, if `!push.usage_delta.is_empty()`,
  `RemoteUsageStore::merge_device(&push.device_name, &push.usage_delta)`. This sits after the
  existing same-/empty-device-name guard, so a device never stores its own records. Then call
  `emit_usage_limits_updated(&app)` so an open Intensity window re-fetches (verify
  `IntensityApp.svelte` refreshes on that event during impl; if it doesn't, the chart simply
  picks up the merged data the next time it's opened — acceptable for v1).

### 4. `commands.rs` — union local + remote at build time
- Helper `merged_usage_records(app) -> Vec<UsageHistoryRecord>`: `UsageHistoryStore::read_all()`
  extended with `RemoteUsageStore::all_records()`, then `sort_by_key(|r| r.ts)`. Tolerant if
  either store is absent.
- Use it in **both** `get_usage_intensity_week` and `get_usage_intensity_weeks` in place of
  the current `store.read_all()`. `data_min_ms`/`data_max_ms` and the weeks-back loop bound
  then reflect the union automatically.
- Optional pure helper + test for the concat-and-sort if it reads cleanly; otherwise rely on
  the existing `build_week_chart` test coverage (the union is just more sorted records).

### 5. `lib.rs` — register the store
- `mod remote_usage;` and
  `app.manage(remote_usage::RemoteUsageStore::new(app_data.join("remote_usage")));`
  next to the existing `RemoteHistoryStore` / `UsageHistoryStore` registrations (~lines 169–190).

## Files

- `src-tauri/src/usage_history.rs` — `Clone` derive on `UsageHistoryRecord`
- `src-tauri/src/remote_usage.rs` — **new** store (pattern from `remote_history.rs`)
- `src-tauri/src/sync.rs` — `usage_delta` field, `usage_delta_since`, push + receive paths
- `src-tauri/src/commands.rs` — `merged_usage_records`, both intensity commands
- `src-tauri/src/lib.rs` — `mod` + `manage`

## Verification

- `cd src-tauri && cargo test` — new `remote_usage` tests, `usage_delta_since` test, plus the
  existing `usage_history` / `sync` suites stay green.
- Manual e2e with the fake-peer harness (per the `debug_sync_fake_peer` memory): add a temp
  `sync` block in `config/local.json` pointing at a Python peer on `:9080`; confirm this
  device's `POST /api/sync` payload now includes a non-empty `usage_delta`. Then reverse it —
  POST a `SyncPush` carrying `usage_delta` (older timestamps spanning a known local gap) to
  this device, confirm `remote_usage/<device>.json` is written, open the Work intensity window
  from the tray, and verify the previously empty buckets now show intensity.
- Deploy with `bash scripts/deploy.sh` for the visual check.

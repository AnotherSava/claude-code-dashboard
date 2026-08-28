export type Status = 'idle' | 'working' | 'waiting' | 'blocked' | 'done' | 'error'

export type DialogRole = 'user' | 'assistant' | 'separator'

export interface DialogEntry {
  role: DialogRole
  text: string
  timestamp: number
  status: Status
  // True when this user prompt started a fresh task — stamped by the Rust
  // state machine (the same decision that drives the sticky label). The
  // history highlight and row tooltip read it directly. Always false for
  // assistant/separator entries and for entries persisted before the field
  // existed.
  task_start: boolean
}

export interface AgentSession {
  id: string
  status: Status
  label: string
  original_prompt: string | null
  task_started_at: number
  dialog: DialogEntry[]
  source: string
  model: string | null
  input_tokens: number | null
  updated: number
  state_entered_at: number
  working_accumulated_ms: number
  display_name?: string | null
  // Device name of the peer dashboard this session was synced from; null for
  // sessions on this machine. Remote ids are namespaced "{origin}/{raw_id}".
  origin?: string | null
  // Instruction-adherence canary flag: true when the last final-message-bearing
  // Stop was missing this session's rotating marker (see the Rust
  // `Config::instruction_canary_enabled`). Orthogonal to `status` — rendered as a
  // ⚠ badge alongside the state pill. Absent for rows from older backends.
  instruction_drift?: boolean
  // Instruction-adherence canary status, coloring the agent name: 'alive' = set up
  // and confirmed adhering (green), 'dead' = set up but drifted (red), 'pending' =
  // set up but not yet confirmed — the marker hasn't been observed, so aliveness is
  // unknown (amber), 'off' = not set up (default). Absent for rows from older backends.
  canary?: 'off' | 'pending' | 'alive' | 'dead'
}

export interface UsageColors {
  green: string
  amber: string
  red: string
}

export type AutoResize = 'none' | 'up' | 'down'

export type HistoryFontSize = 'smallest' | 'small' | 'regular' | 'large' | 'largest'
// Which unit the Work intensity chart plots, orthogonal to its Days/Weeks view.
export type IntensityUnit = 'percent' | 'tokens'

export interface Config {
  server_port: number
  always_on_top: boolean
  save_window_position: boolean
  window_position: { x: number; y: number } | null
  context_window_tokens: Record<string, number>
  usage_colors: UsageColors
  token_gradient: boolean
  benign_closers: string[]
  benign_openers: string[]
  usage_limits_poll_interval_seconds: number
  limit_bar_segments: number
  auto_resize: AutoResize
  history_font_size: HistoryFontSize
  intensity_unit: IntensityUnit
  intensity_axis_max_tokens: number | null
  // Compact view: hide each row's current prompt and time-in-state, and
  // collapse the usage bars down to their bare percentage. Toggled from the
  // tray's "Compact view" checkbox.
  compact_mode: boolean
}

export type UsageStatus = 'ok' | 'unavailable' | 'auth_expired' | 'network_error'

export interface LimitBucket {
  utilization: number
  resets_at: number | null
}

export interface UsageLimits {
  five_hour: LimitBucket | null
  seven_day: LimitBucket | null
  status: UsageStatus
  updated: number
}

export interface SetupState {
  hook_script_path: string
  settings_snippet: string
  has_history: boolean
}

// One 10-minute bar of the work-intensity chart. `intensity` is the percent of
// the 5h limit consumed in the slot (>= 0); `has_data` distinguishes genuine
// idle (true, 0) from a gap where the app was closed (false). See the Rust
// `WeekBucket` / `build_week_chart` — these mirror its serialized shape.
export interface WeekBucket {
  intensity: number
  has_data: boolean
}

// Per-day roll-up shown to the right of each day row. `active_minutes` counts
// 10-min buckets with work; `weekly_pct` is the day's share of the 7-day quota.
export interface DaySummary {
  active_minutes: number
  weekly_pct: number
}

export interface WeekChart {
  week_start_ms: number
  week_end_ms: number
  buckets: WeekBucket[]
  days: DaySummary[]
  data_min_ms: number | null
  data_max_ms: number | null
  full_intensity: number
}

// Token-unit twins of the three above, mirroring the Rust `TokenBucket` /
// `TokenDaySummary` / `TokenWeekChart`. `tokens` is input + cache creation +
// output — cache reads are stored but excluded, being 97% of the raw sum and a
// measure of conversation length rather than work. `has_data` here means "inside
// the span we hold token records for"; unlike the percentage chart it does not
// track whether the dashboard was running, since transcripts are written by
// Claude Code itself and the scanner catches up afterwards.
export interface TokenBucket {
  tokens: number
  has_data: boolean
}

export interface TokenDaySummary {
  active_minutes: number
  tokens: number
}

export interface TokenWeekChart {
  week_start_ms: number
  week_end_ms: number
  buckets: TokenBucket[]
  days: TokenDaySummary[]
  data_min_ms: number | null
  data_max_ms: number | null
  // Full-height value for one 10-min bar. A stated ceiling, not a derived one:
  // tokens have no quota to be a fraction of.
  axis_max_tokens: number
}

export const stateLabel: Record<Status, string> = {
  idle: 'IDLE',
  working: 'WORK',
  waiting: 'WAIT',
  blocked: 'BLOCK',
  done: 'DONE',
  error: 'ERROR',
}

export function displayLabel(session: AgentSession): string {
  if (session.status === 'blocked' || session.status === 'error') return session.label
  return session.original_prompt ?? session.label
}

export function displayTimeMs(session: AgentSession, now: number): number {
  const inCurrent = Math.max(0, now - session.state_entered_at)
  if (session.status === 'working') return session.working_accumulated_ms + inCurrent
  return inCurrent
}

export function formatTime(ms: number): string {
  const totalMin = Math.floor(ms / 60_000)
  const h = Math.floor(totalMin / 60)
  const m = totalMin % 60
  const pad = (n: number) => n.toString().padStart(2, '0')
  return `${pad(h)}:${pad(m)}`
}

export function formatTokens(n: number): string {
  return Math.ceil(n / 1000).toString()
}

export function formatCompactRemaining(ms: number | null, mode: 'hm' | 'dhm'): string {
  if (ms === null || !Number.isFinite(ms) || ms <= 0) {
    return mode === 'dhm' ? '-:--:--' : '--:--'
  }
  const totalMin = Math.floor(ms / 60_000)
  const pad = (n: number) => n.toString().padStart(2, '0')
  if (mode === 'dhm') {
    const d = Math.floor(totalMin / 1440)
    const h = Math.floor((totalMin % 1440) / 60)
    const m = totalMin % 60
    return `${d}:${pad(h)}:${pad(m)}`
  }
  const h = Math.floor(totalMin / 60)
  const m = totalMin % 60
  return `${pad(h)}:${pad(m)}`
}

// Resolve a model's context window: exact key first, then the longest key
// that is a prefix of the model name — so "claude-opus" covers every future
// opus release without a config update. Mirrored by Rust `window_for` in
// notifications.rs; keep the two in sync.
export function windowFor(model: string, map: Record<string, number>): number | null {
  const exact = map[model]
  if (exact) return exact
  let best: string | null = null
  for (const key of Object.keys(map)) {
    if (model.startsWith(key) && map[key] > 0 && (best === null || key.length > best.length)) best = key
  }
  return best === null ? null : map[best]
}

export function tokenColor(session: AgentSession, config: Config): string {
  if (session.input_tokens === null || session.model === null) return '#8a8a8e'
  const max = windowFor(session.model, config.context_window_tokens)
  if (!max) return '#8a8a8e'
  const pct = Math.min(100, (session.input_tokens / max) * 100)
  return config.token_gradient
    ? usageColorGradient(pct, config.usage_colors)
    : usageColor(pct, config.usage_colors)
}

// Single source for the usage color palette. `usageColor` is the 3-color step —
// green below 50%, amber 50–84%, red at 85%+ — used by the limit bars (fill +
// percentage) and, by default, the token counter. Colors come from
// `config.usage_colors` so they're tunable, and the tray icon reads the same
// palette in Rust (`urgency_color`), so every usage number — widget and tray —
// agrees. The default palette is tuned bright enough to read as a ~16px icon.
export function usageColor(pct: number, c: UsageColors): string {
  if (pct >= 85) return c.red
  if (pct >= 50) return c.amber
  return c.green
}

// Smooth variant for the token counter when `token_gradient` is on: interpolate
// green→amber over 0–50% and amber→red over 50–85%, red beyond. The limit bars
// always use the step above.
export function usageColorGradient(pct: number, c: UsageColors): string {
  if (pct <= 0) return c.green
  if (pct >= 85) return c.red
  if (pct <= 50) return lerpHex(c.green, c.amber, pct / 50)
  return lerpHex(c.amber, c.red, (pct - 50) / 35)
}

function lerpHex(a: string, b: string, t: number): string {
  const ah = [1, 3, 5].map((i) => parseInt(a.slice(i, i + 2), 16))
  const bh = [1, 3, 5].map((i) => parseInt(b.slice(i, i + 2), 16))
  const out = ah.map((v, i) => Math.round(v + (bh[i] - v) * t))
  return `#${out.map((n) => n.toString(16).padStart(2, '0')).join('')}`
}

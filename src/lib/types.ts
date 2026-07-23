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

export interface ContextBarThreshold {
  percent: number
  color: string
}

export type AutoResize = 'none' | 'up' | 'down'

export type HistoryFontSize = 'smallest' | 'small' | 'regular' | 'large' | 'largest'

export interface Config {
  server_port: number
  always_on_top: boolean
  save_window_position: boolean
  window_position: { x: number; y: number } | null
  context_window_tokens: Record<string, number>
  context_bar_thresholds: ContextBarThreshold[]
  benign_closers: string[]
  benign_openers: string[]
  usage_limits_poll_interval_seconds: number
  limit_bar_segments: number
  auto_resize: AutoResize
  history_font_size: HistoryFontSize
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
  return colorAtPercent(pct, config.context_bar_thresholds)
}

export function colorAtPercent(p: number, stops: ContextBarThreshold[]): string {
  if (stops.length === 0) return '#3a7c4a'
  const sorted = [...stops].sort((a, b) => a.percent - b.percent)
  if (p <= sorted[0].percent) return sorted[0].color
  if (p >= sorted[sorted.length - 1].percent) return sorted[sorted.length - 1].color
  for (let i = 0; i < sorted.length - 1; i++) {
    const a = sorted[i]
    const b = sorted[i + 1]
    if (p >= a.percent && p <= b.percent) {
      const t = (p - a.percent) / (b.percent - a.percent)
      return lerpHex(a.color, b.color, t)
    }
  }
  return sorted[0].color
}

function lerpHex(a: string, b: string, t: number): string {
  const ah = [1, 3, 5].map((i) => parseInt(a.slice(i, i + 2), 16))
  const bh = [1, 3, 5].map((i) => parseInt(b.slice(i, i + 2), 16))
  const out = ah.map((v, i) => Math.round(v + (bh[i] - v) * t))
  return `#${out.map((n) => n.toString(16).padStart(2, '0')).join('')}`
}

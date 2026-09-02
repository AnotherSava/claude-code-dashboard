import { invoke } from '@tauri-apps/api/core'
import { listen, type UnlistenFn } from '@tauri-apps/api/event'
import { getCurrentWebviewWindow } from '@tauri-apps/api/webviewWindow'
import type { AgentSession, Config, DialogEntry, IntensityUnit, SetupState, TokenWeekChart, UsageLimits, WeekChart } from './types'

export function getSessions(): Promise<AgentSession[]> {
  return invoke<AgentSession[]>('get_sessions')
}

export function getConfig(): Promise<Config> {
  return invoke<Config>('get_config')
}

export function getUsageLimits(): Promise<UsageLimits> {
  return invoke<UsageLimits>('get_usage_limits')
}

export function refreshUsageLimits(): Promise<boolean> {
  return invoke<boolean>('refresh_usage_limits')
}

// `weekOffset` is relative to the current local week: 0 = this week, -1 = last.
export function getUsageIntensityWeek(weekOffset: number): Promise<WeekChart> {
  return invoke<WeekChart>('get_usage_intensity_week', { weekOffset })
}

// Every week from the current one back to the oldest record, newest first.
export function getUsageIntensityWeeks(): Promise<WeekChart[]> {
  return invoke<WeekChart[]>('get_usage_intensity_weeks')
}

// Token-unit twins of the two above, for the chart's Tokens view.
export function getTokenIntensityWeek(weekOffset: number): Promise<TokenWeekChart> {
  return invoke<TokenWeekChart>('get_token_intensity_week', { weekOffset })
}

export function getTokenIntensityWeeks(): Promise<TokenWeekChart[]> {
  return invoke<TokenWeekChart[]>('get_token_intensity_weeks')
}

// Persist which unit the intensity chart plots.
export function setIntensityUnit(unit: IntensityUnit): Promise<void> {
  return invoke('set_intensity_unit', { unit })
}

// Resize the main window to fit `physicalHeight` physical px. Fire-and-forget:
// the frontend sizes against the webview's own devicePixelRatio (see App.svelte
// effectiveScale), so nothing needs to round-trip back from Rust.
export function applyAutoResize(physicalHeight: number): Promise<void> {
  return invoke('apply_auto_resize', { physicalHeight })
}

// Fit the main window's width to compact view (`compact=true`, passing the
// header's natural content width in physical px), or restore the remembered
// non-compact width (`compact=false`, `headerInnerWidthPhys=null`). The compact
// width is never persisted — the backend reconstructs the non-compact geometry
// on close. Fire-and-forget, like applyAutoResize.
export function setCompactWidth(
  compact: boolean,
  headerInnerWidthPhys: number | null,
): Promise<void> {
  return invoke('set_compact_width', { compact, headerInnerWidthPhys })
}

export function frontendLog(
  level: 'trace' | 'debug' | 'info' | 'warn' | 'error',
  message: string,
  data: Record<string, unknown> = {},
): Promise<void> {
  return invoke('frontend_log', { level, message, data })
}

export function hideWindow(): Promise<void> {
  return invoke('hide_window')
}

export function showWindow(): Promise<void> {
  return invoke('show_window')
}

export function quitApp(): Promise<void> {
  return invoke('quit_app')
}

export function openHistory(id: string): Promise<void> {
  return invoke('open_history', { id })
}

export function closeWindow(): Promise<void> {
  return invoke('close_window')
}

export function getWindowLabel(): Promise<string> {
  return invoke<string>('get_window_label')
}

// Synchronous counterpart: the current window's label is injected before the
// app script runs, so it's readable without an IPC round-trip. Used where a
// decision must be made on the component's first synchronous pass, before the
// async `getWindowLabel()` above could resolve (e.g. gating the auto-resize
// measure subsystem to the main window on its very first reactive run).
export function getWindowLabelSync(): string {
  return getCurrentWebviewWindow().label
}

export function setHistoryFontSize(size: string): Promise<void> {
  return invoke('set_history_font_size', { size })
}

export function setChatName(chatId: string, name: string): Promise<void> {
  return invoke('set_chat_name', { chatId, name })
}

export function hideHistory(): Promise<void> {
  return invoke('hide_history')
}

export function getSetupState(): Promise<SetupState> {
  return invoke<SetupState>('get_setup_state')
}

export function openHookScriptLocation(): Promise<void> {
  return invoke('open_hook_script_location')
}

export function openSetupDocs(): Promise<void> {
  return invoke('open_setup_docs')
}

export function openDocsHome(): Promise<void> {
  return invoke('open_docs_home')
}

export interface AboutInfo {
  version: string
  release_date: string
  docs_url: string
}

export function getAboutInfo(): Promise<AboutInfo> {
  return invoke<AboutInfo>('get_about_info')
}

export function setWindowSize(
  label: string,
  logicalWidth: number,
  logicalHeight: number,
  recenter = false,
): Promise<void> {
  return invoke('set_window_size', { label, logicalWidth, logicalHeight, recenter })
}

export function onSessionsUpdated(
  handler: (sessions: AgentSession[]) => void,
): Promise<UnlistenFn> {
  return listen<AgentSession[]>('sessions_updated', (evt) => handler(evt.payload))
}

export function onConfigUpdated(
  handler: (config: Config) => void,
): Promise<UnlistenFn> {
  return listen<Config>('config_updated', (evt) => handler(evt.payload))
}

export function onUsageLimitsUpdated(
  handler: (usage: UsageLimits) => void,
): Promise<UnlistenFn> {
  return listen<UsageLimits>('usage_limits_updated', (evt) => handler(evt.payload))
}

export function onShowSetupInstructions(handler: () => void): Promise<UnlistenFn> {
  return listen('show_setup_instructions', () => handler())
}

// The backend saw the window come back from being minimized and is asking for a
// fresh measurement. The webview can't notice that on its own — minimizing
// doesn't resize it, so `window.innerHeight` stays frozen across the whole
// minimize/restore round trip.
export function onRefitWindow(handler: () => void): Promise<UnlistenFn> {
  return listen('refit_window', () => handler())
}

export function onHistoryLoading(
  handler: (payload: { id: string; loading: boolean }) => void,
): Promise<UnlistenFn> {
  return listen<{ id: string; loading: boolean }>('history_loading', (evt) => handler(evt.payload))
}

// A peer has nothing running for a project a local agent messaged, and offered
// directories it could start one in. The answer is a standing permission on
// that machine, so the prompt is raised here — where a human plausibly is.
export interface StartCandidate {
  dir: string
  trusted: boolean
}

export interface PendingStart {
  id: string
  device: string
  project: string
  target: string
  from_agent: string
  candidates: StartCandidate[]
  requested_at: number
  /** False once the requesting agent stopped waiting: approving still records
   *  the permission for next time, but can no longer deliver that message. */
  still_waiting: boolean
}

export function getStartApprovals(): Promise<PendingStart[]> {
  return invoke<PendingStart[]>('get_start_approvals')
}

export function approveStart(requestId: string, dir: string): Promise<void> {
  return invoke('approve_start', { requestId, dir })
}

export function dismissStart(requestId: string): Promise<void> {
  return invoke('dismiss_start', { requestId })
}

export function onStartApprovalsUpdated(
  handler: (pending: PendingStart[]) => void,
): Promise<UnlistenFn> {
  return listen<PendingStart[]>('start_approvals_updated', (evt) => handler(evt.payload))
}

// The stored dialog for a session with no live row. Rows are not restored at
// startup (the dashboard cannot tell a closed session from an idle one), so
// without this a conversation is unreadable after a restart until that session
// next acts — while sitting complete on disk the whole time.
export function getPersistedDialog(id: string): Promise<DialogEntry[]> {
  return invoke<DialogEntry[]>('get_persisted_dialog', { id })
}

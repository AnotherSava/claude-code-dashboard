<script lang="ts">
  import type { AgentSession, Config } from '../types'
  import {
    displayLabel,
    displayTimeMs,
    formatTime,
    formatTokens,
    stateLabel,
    tokenColor,
  } from '../types'
  import { removeSession } from '../api'

  interface Props {
    session: AgentSession
    config: Config
    now: number
  }

  let { session, config, now }: Props = $props()

  const HISTORY_VISIBLE = 4

  const label = $derived(displayLabel(session))
  const time = $derived(formatTime(displayTimeMs(session, now)))
  const tokensText = $derived(
    session.input_tokens !== null ? formatTokens(session.input_tokens) : '',
  )
  const tokColor = $derived(tokenColor(session, config))
  const shouldPulse = $derived(session.status === 'awaiting' || session.status === 'error')

  function formatClock(ms: number): string {
    if (!ms) return ' --:--'
    const d = new Date(ms)
    const pad = (n: number) => n.toString().padStart(2, '0')
    return `${pad(d.getHours())}:${pad(d.getMinutes())}`
  }

  // Multi-line plain-text history rendered by the OS-native title tooltip,
  // which is what gives us the ability to exceed the dashboard window's
  // width. Format: each line is `HH:MM  prompt`. Older prompts on top,
  // current on the bottom, prefixed with an arrow marker.
  const titleText = $derived.by(() => {
    const taskPrompts = session.dialog.filter((e) => {
      if (e.role !== 'user') return false
      const idx = session.dialog.indexOf(e)
      if (idx === 0) return true
      const prev = session.dialog[idx - 1]
      return prev.status !== 'awaiting'
    })
    const visible = taskPrompts.slice(-(HISTORY_VISIBLE + 1))
    const lines: string[] = visible.map((e, i) => {
      const marker = i === visible.length - 1 ? '  ▸ ' : '    '
      return `${formatClock(e.timestamp)}${marker}${e.text}`
    })
    return lines.join('\n')
  })

  function onRemove(e: MouseEvent) {
    e.stopPropagation()
    removeSession(session.id).catch((err) => console.error('remove failed', err))
  }
</script>

<div class="row">
  <div class="content">
    <div class="top">
      <span class="id" title={session.id}>{session.id}</span>
      <span class="pill state-{session.status}" class:pulse={shouldPulse}>{stateLabel[session.status]}</span>
      <span class="time-slot">
        <span class="time">{time}</span>
        <button class="remove" onclick={onRemove} aria-label="Remove session" tabindex="-1">×</button>
      </span>
      <span class="tokens" style:color={tokColor}>{#if tokensText}{tokensText}<span class="k">k</span>{/if}</span>
    </div>
    {#if label}
      <div class="label" title={titleText}>{label}</div>
    {/if}
  </div>
</div>

<style>
  .row {
    display: flex;
    align-items: center;
    gap: 10px;
    padding: 2px 12px 3px;
    border-bottom: 1px solid #2a2a2d;
  }
  .row:last-child {
    border-bottom: none;
  }
  .pulse {
    animation: pulse 1.6s ease-in-out infinite;
  }
  @keyframes pulse {
    0%, 100% { opacity: 1; }
    50% { opacity: 0.45; }
  }
  .content {
    flex: 1;
    min-width: 0;
    display: flex;
    flex-direction: column;
    gap: 3px;
  }
  .top {
    display: flex;
    align-items: center;
    gap: 8px;
    min-width: 0;
  }
  .id {
    font-size: 13px;
    font-weight: 600;
    color: #e8e8ea;
    white-space: nowrap;
    overflow: hidden;
    text-overflow: ellipsis;
    flex: 1;
    min-width: 0;
  }
  .pill {
    font-size: 9px;
    font-weight: 700;
    letter-spacing: 0.5px;
    padding: 2px 6px;
    border-radius: 9px;
    flex-shrink: 0;
    font-family: ui-monospace, Consolas, monospace;
    min-width: 44px;
    text-align: center;
  }
  .state-idle {
    background: #2f2f33;
    color: #a1a1aa;
  }
  .state-working {
    background: #1e40af;
    color: #bfdbfe;
  }
  .state-awaiting {
    background: #b45309;
    color: #fde68a;
  }
  .state-done {
    background: #047857;
    color: #a7f3d0;
  }
  .state-error {
    background: #b91c1c;
    color: #fecaca;
  }
  .time-slot {
    position: relative;
    display: inline-flex;
    justify-content: flex-end;
    flex-shrink: 0;
    min-width: 36px;
  }
  .time {
    font-size: 11px;
    color: #8a8a8e;
    font-family: ui-monospace, Consolas, monospace;
    font-variant-numeric: tabular-nums;
    text-align: right;
    transition: opacity 120ms ease;
  }
  .remove {
    position: absolute;
    inset: 0;
    display: flex;
    align-items: center;
    justify-content: center;
    opacity: 0;
    pointer-events: none;
    background: transparent;
    border: 0;
    padding: 0;
    color: #b91c1c;
    font-size: 16px;
    font-weight: 700;
    line-height: 1;
    cursor: pointer;
    transition: opacity 120ms ease, color 120ms ease;
  }
  .remove:hover {
    color: #ef4444;
  }
  .row:hover .remove {
    opacity: 1;
    pointer-events: auto;
  }
  .row:hover .time {
    opacity: 0;
  }
  .tokens {
    font-size: 12px;
    font-weight: 600;
    font-family: ui-monospace, Consolas, monospace;
    font-variant-numeric: tabular-nums;
    flex-shrink: 0;
    min-width: 32px;
    text-align: right;
  }
  .tokens .k {
    color: #4b5563;
    margin-left: 1px;
  }
  .label {
    font-size: 11px;
    color: #8a8a8e;
    white-space: nowrap;
    overflow: hidden;
    text-overflow: ellipsis;
    line-height: 1.3;
  }
</style>

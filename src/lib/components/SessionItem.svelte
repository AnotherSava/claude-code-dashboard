<script module lang="ts">
  import { writable } from 'svelte/store'
  import { listen } from '@tauri-apps/api/event'

  const historyOpen = writable(false)
  const historyTarget = writable<string | null>(null)
  const historyClosedAt = writable(0)

  listen('history_hidden', () => {
    historyOpen.set(false)
    historyTarget.set(null)
    historyClosedAt.set(Date.now())
  })

  // The OS-native tooltip renders a proportional font (Segoe UI on Windows), so
  // a marker padded by character count can't pixel-align — the ▸ glyph isn't a
  // whole number of spaces wide. Measure the glyphs once and build, for the
  // current task: `current` — a marker the exact width of the `gap` the other
  // rows use, with the ▸ centered; and `arrowBlank` — a blank the ▸'s exact
  // width, for the wrapped-line indent. Ratios are size-independent, so the
  // measurement px size is irrelevant. Falls back to a space approximation if
  // canvas/font measurement is unavailable.
  function measureMarker(): { gap: string; current: string; arrowBlank: string } {
    const fallback = { gap: '    ', current: ' ▸ ', arrowBlank: ' ' }
    try {
      const ctx = document.createElement('canvas').getContext('2d')
      if (!ctx) return fallback
      ctx.font = '40px "Segoe UI", system-ui, sans-serif'
      const w = (s: string) => ctx.measureText(s).width
      const space = w(' ')
      const arrow = w('▸')
      if (!(space > 0) || !(arrow > 0)) return fallback
      // Blank glyphs, widest first, for sub-space padding precision: regular,
      // figure (U+2007), thin (U+2009), hair (U+200A) space.
      const palette = ([[' ', space], [' ', w(' ')], [' ', w(' ')], [' ', w(' ')]] as Array<[string, number]>)
        .filter(([, width]) => width > 0)
        .sort((a, b) => b[1] - a[1])
      // Greedily approximate `target` px with palette glyphs (never overshoots;
      // residual is under one hair space, i.e. sub-pixel at tooltip size).
      const pad = (target: number): string => {
        let s = '', rem = target
        for (const [ch, width] of palette) while (rem >= width) { s += ch; rem -= width }
        return s
      }
      const gapPx = 4 * space
      const leftover = Math.max(0, gapPx - arrow)
      const left = pad(leftover / 2)
      const right = pad(gapPx - arrow - w(left))
      return { gap: '    ', current: left + '▸' + right, arrowBlank: pad(arrow) }
    } catch {
      return fallback
    }
  }

  const MARKER = measureMarker()
</script>

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
  import { hideHistory, openHistory, setChatName } from '../api'

  interface Props {
    session: AgentSession
    config: Config
    now: number
  }

  let { session, config, now }: Props = $props()

  const HISTORY_VISIBLE = 4

  // Remote ids arrive namespaced as "{origin}/{raw_id}"; the badge already
  // carries the device, so an unnamed remote row shows just the raw id.
  const displayName = $derived(
    session.display_name ?? (session.origin ? session.id.slice(session.origin.length + 1) : session.id),
  )

  let editing = $state(false)
  let draft = $state('')

  function focusSelect(node: HTMLInputElement) {
    node.focus()
    node.select()
  }

  function startEdit() {
    draft = displayName
    editing = true
  }

  function commitEdit() {
    if (!editing) return
    editing = false
    // An empty / whitespace-only value is treated as a cancel — keep the
    // current name rather than clearing the custom name back to the derived id.
    if (!draft.trim()) return
    setChatName(session.id, draft).catch((err) => console.error('set_chat_name failed', err))
  }

  function cancelEdit() {
    editing = false
  }

  function onNameKeydown(e: KeyboardEvent) {
    if (e.key === 'Enter') { e.preventDefault(); commitEdit() }
    else if (e.key === 'Escape') { e.preventDefault(); cancelEdit() }
  }

  const label = $derived(displayLabel(session))
  // When there's no active task to show (e.g. an idle row after `/clear`), fall
  // back to the most recent task prompt so the history tooltip + click-to-open
  // stay reachable. Rendered muted/italic via `isPastTask` so it reads clearly
  // as a past task, not the current one.
  const lastTask = $derived.by(() => {
    const tasks = session.dialog.filter((e) => e.task_start)
    if (tasks.length) return tasks[tasks.length - 1].text
    // No flagged task starts — a dialog synced from a peer, persisted before
    // backend task-start marking, or made of only continuations/approvals.
    // Fall back to a user prompt so a row with restorable history never goes
    // blank (the click target stays alive and history stays reachable).
    // Prefer the most recent non-trivial prompt over single-token approvals
    // like "y"/"ok" so the hint reads as a task, not a confirmation; fall back
    // to any user prompt, then any non-separator entry.
    const users = session.dialog.filter((e) => e.role === 'user' && e.text.trim() !== '')
    const substantive = users.filter((e) => e.text.trim().length > 4)
    if (substantive.length) return substantive[substantive.length - 1].text
    if (users.length) return users[users.length - 1].text
    const any = session.dialog.filter((e) => e.role !== 'separator' && e.text.trim() !== '')
    return any.length ? any[any.length - 1].text : ''
  })
  const labelText = $derived(label || lastTask)
  const isPastTask = $derived(!label && !!lastTask)
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

  // Target line width before a prompt wraps in the tooltip. The OS tooltip uses
  // a proportional font so this is approximate; kept comfortably below the
  // tooltip's own max width so it never re-wraps our already-wrapped lines.
  const TOOLTIP_WRAP_COLS = 80

  // Build a blank string the same *rendered* width as `prefix` for the hanging
  // indent. The OS tooltip uses a proportional font, so plain spaces are
  // narrower than the `HH:MM` digits and continuation lines drift left. Map each
  // glyph to a width-matched blank: figure space (U+2007 ≈ a tabular digit) for
  // digits, punctuation space (U+2008 ≈ a colon/period) for `:`, the measured
  // equal-width blank for the ▸ marker, and existing blanks (the marker's
  // padding) pass through unchanged. Wrap math keys off prefix.length, not the
  // indent, so variable-length blanks are harmless.
  function blankLike(prefix: string): string {
    let out = ''
    for (const ch of prefix) {
      if (ch >= '0' && ch <= '9') out += ' '
      else if (ch === ':') out += ' '
      else if (ch === '▸') out += MARKER.arrowBlank // arrow → its measured equal-width blank
      else if (ch === ' ' || ch === ' ' || ch === ' ' || ch === ' ') out += ch
      else out += ' '
    }
    return out
  }

  // Word-wrap `text` to `TOOLTIP_WRAP_COLS`, prefixing the first line with
  // `prefix` (the `HH:MM` + marker column) and every continuation line with a
  // hanging indent of the same width — so wrapped text lines up under the
  // prompt's first character instead of falling back to the time column on the
  // left. Newlines already in the prompt are honoured and indented too.
  function wrapWithHangingIndent(prefix: string, text: string): string {
    const indent = blankLike(prefix)
    const avail = Math.max(16, TOOLTIP_WRAP_COLS - prefix.length)
    const out: string[] = []
    for (const segment of text.split('\n')) {
      let line = ''
      for (const word of segment.split(/\s+/).filter((w) => w !== '')) {
        let w = word
        // A single word wider than the budget is hard-broken across lines.
        while (w.length > avail) {
          if (line) { out.push(line); line = '' }
          out.push(w.slice(0, avail))
          w = w.slice(avail)
        }
        if (line === '') line = w
        else if (line.length + 1 + w.length <= avail) line += ' ' + w
        else { out.push(line); line = w }
      }
      out.push(line) // preserves blank lines from the original prompt
    }
    return out.map((l, i) => (i === 0 ? prefix : indent) + l).join('\n')
  }

  // Multi-line plain-text history rendered by the OS-native title tooltip,
  // which is what gives us the ability to exceed the dashboard window's
  // width. Format: each line is `HH:MM  prompt`, with long prompts wrapped to a
  // hanging-indented second column. Older prompts on top, current on the
  // bottom, prefixed with an arrow marker.
  const titleText = $derived.by(() => {
    const taskPrompts = session.dialog.filter((e) => e.task_start)
    const visible = taskPrompts.slice(-(HISTORY_VISIBLE + 1))
    const lines: string[] = visible.map((e, i) => {
      // Current task gets the ▸ centered in a marker measured to the exact width
      // of the gap (MARKER.gap) the other rows fill, so its prompt text lines up
      // with the rows above it regardless of the proportional triangle width.
      const marker = i === visible.length - 1 ? MARKER.current : MARKER.gap
      return wrapWithHangingIndent(`${formatClock(e.timestamp)}${marker}`, e.text)
    })
    return lines.join('\n')
  })

  const effectiveTitle = $derived.by(() => {
    if ($historyOpen) return ''
    if ($historyClosedAt > 0 && now - $historyClosedAt < 2000) return ''
    return titleText
  })

  function onLabelClick() {
    if ($historyOpen && $historyTarget === session.id) {
      hideHistory().catch(() => {})
      return
    }
    historyOpen.set(true)
    historyTarget.set(session.id)
    historyClosedAt.set(0)
    openHistory(session.id).catch((err) => console.error('open_history failed', err))
  }
</script>

<div class="row">
  <div class="content">
    <div class="top">
      {#if editing}
        <input
          class="id-edit"
          use:focusSelect
          bind:value={draft}
          onkeydown={onNameKeydown}
          onblur={cancelEdit}
        />
      {:else}
        <span class="id" title="{displayName} — double-click to rename" ondblclick={startEdit} role="textbox" tabindex="-1">{displayName}</span>
      {/if}
      {#if session.origin}
        <span class="device" title="Session on {session.origin}">{session.origin}</span>
      {/if}
      <span class="pill state-{session.status}" class:pulse={shouldPulse}>{stateLabel[session.status]}</span>
      <span class="time">{time}</span>
      <span class="tokens" style:color={tokColor}>{#if tokensText}{tokensText}<span class="k">k</span>{/if}</span>
    </div>
    {#if labelText}
      <div class="label" class:past={isPastTask} title={effectiveTitle} onclick={onLabelClick} onkeydown={(e) => { if (e.key === 'Enter') onLabelClick() }} role="button" tabindex="-1">{labelText}</div>
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
  .id-edit {
    font-size: 13px;
    font-weight: 600;
    color: #e8e8ea;
    flex: 1;
    min-width: 0;
    background: #1c1c1e;
    border: 1px solid #3b82f6;
    border-radius: 4px;
    padding: 0 4px;
    outline: none;
    font-family: inherit;
  }
  .device {
    font-size: 9px;
    font-weight: 600;
    letter-spacing: 0.3px;
    color: #a78bfa;
    background: #2e2a3f;
    padding: 2px 6px;
    border-radius: 9px;
    flex-shrink: 0;
    max-width: 80px;
    white-space: nowrap;
    overflow: hidden;
    text-overflow: ellipsis;
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
  .time {
    font-size: 11px;
    color: #8a8a8e;
    font-family: ui-monospace, Consolas, monospace;
    font-variant-numeric: tabular-nums;
    text-align: right;
    flex-shrink: 0;
    min-width: 36px;
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
    cursor: pointer;
    transition: color 120ms ease;
  }
  .label:hover {
    color: #b0b0b4;
  }
  .label.past {
    font-style: italic;
    color: #6a6a6e;
  }
  .label.past:hover {
    color: #9a9a9e;
  }
</style>

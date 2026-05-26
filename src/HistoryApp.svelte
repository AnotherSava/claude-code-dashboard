<script lang="ts">
  import { onMount } from 'svelte'
  import { invoke } from '@tauri-apps/api/core'
  import { listen, type UnlistenFn } from '@tauri-apps/api/event'
  import { closeWindow, getConfig, getSessions, onConfigUpdated, onSessionsUpdated, setHistoryFontSize } from './lib/api'
  import { isTaskBoundary } from './lib/dialog'
  import type { AgentSession, HistoryFontSize } from './lib/types'

  const SIZE_ORDER: HistoryFontSize[] = ['smallest', 'small', 'regular', 'large', 'largest']
  const SIZE_PX: Record<HistoryFontSize, number> = { smallest: 11, small: 12, regular: 14, large: 16, largest: 18 }

  // Horizontal-rule-like separators used by the assistant to delimit chunks:
  // markdown `---` / `___` / `***` / `===`, and box-drawing horizontals like
  // `━━━` / `───` / `═══`. Three or more identical chars on a line.
  const SEPARATOR_RE = /^[-_*=━─═]{3,}$/

  function collapseRange(lines: string[]): [number, number] | null {
    const hrs: number[] = []
    for (let i = 0; i < lines.length; i++) {
      if (SEPARATOR_RE.test(lines[i].trim())) hrs.push(i)
    }
    if (hrs.length < 2) return null
    const first = hrs[0]
    const last = hrs[hrs.length - 1]
    if (last - first < 2) return null
    return [first, last]
  }

  let sessionId = $state<string | null>(null)
  let session = $state<AgentSession | null>(null)
  let fontSize = $state<HistoryFontSize>('regular')
  let error = $state<string | null>(null)
  let entriesEl: HTMLDivElement | undefined = $state()
  let expanded = $state<Set<number>>(new Set())
  let unlistenSessions: (() => void) | undefined
  let unlistenConfig: (() => void) | undefined
  let unlistenTarget: UnlistenFn | undefined

  async function loadSession(id: string) {
    sessionId = id
    error = null
    expanded = new Set()
    try {
      const sessions = await getSessions()
      session = sessions.find((s) => s.id === id) ?? null
      if (!session) error = `session "${id}" not found`
    } catch (e) {
      error = String(e)
    }
  }

  onMount(() => {
    ;(async () => {
      const target = await invoke<string | null>('get_history_target')
      if (target) await loadSession(target)

      unlistenTarget = await listen<string>('history_target', (evt) => {
        loadSession(evt.payload)
      })

      const cfg = await getConfig()
      fontSize = cfg.history_font_size

      unlistenSessions = await onSessionsUpdated((s) => {
        if (!sessionId) return
        const found = s.find((x) => x.id === sessionId) ?? null
        session = found
        if (!found) { session = null; error = 'session removed' }
      })

      unlistenConfig = await onConfigUpdated((c) => { fontSize = c.history_font_size })
    })()

    return () => {
      unlistenTarget?.()
      unlistenSessions?.()
      unlistenConfig?.()
    }
  })

  function formatClock(ms: number): string {
    if (!ms) return '--:--'
    const d = new Date(ms)
    const pad = (n: number) => n.toString().padStart(2, '0')
    return `${pad(d.getHours())}:${pad(d.getMinutes())}`
  }

  function deduplicatedDialog(): import('./lib/types').DialogEntry[] {
    if (!session) return []
    const d = session.dialog
    return d.filter((entry, i) => {
      if (entry.role !== 'user') return true
      const next = d[i + 1]
      if (!next || next.role !== 'user') return true
      const prefixLen = Math.min(entry.text.length, 10)
      return entry.text.slice(0, prefixLen) !== next.text.slice(0, prefixLen)
    })
  }

  let dialog = $derived(deduplicatedDialog())

  function expandEntry(idx: number) {
    expanded.add(idx)
    expanded = new Set(expanded)
  }

  let wasAtBottom = true

  function onEntriesScroll() {
    if (entriesEl) wasAtBottom = Math.abs(entriesEl.scrollTop) < 2
  }

  $effect(() => {
    fontSize
    if (entriesEl && wasAtBottom) entriesEl.scrollTop = 0
  })

  function cycleSize(direction: 1 | -1) {
    const idx = SIZE_ORDER.indexOf(fontSize)
    const newIdx = Math.max(0, Math.min(SIZE_ORDER.length - 1, idx + direction))
    if (newIdx !== idx) setHistoryFontSize(SIZE_ORDER[newIdx]).catch(() => {})
  }

  function onKeydown(e: KeyboardEvent) {
    if (e.key === 'Escape') { closeWindow(); return }
    if (e.ctrlKey && (e.key === '+' || e.key === '=')) { e.preventDefault(); cycleSize(1) }
    if (e.ctrlKey && e.key === '-') { e.preventDefault(); cycleSize(-1) }
  }
</script>

<svelte:window onkeydown={onKeydown} />

{#if error}
  <div class="msg">{error}</div>
{:else if session}
  <div class="entries" bind:this={entriesEl} onscroll={onEntriesScroll} style:font-size="{SIZE_PX[fontSize]}px">
    <div class="entries-inner">
      {#each dialog as entry, i}
        {#if entry.role === 'separator'}
          <div class="separator"><hr /></div>
        {:else}
          {@const lines = entry.text.split('\n')}
          {@const range = expanded.has(i) ? null : collapseRange(lines)}
          <div class="entry" class:sticky={isTaskBoundary(dialog, i)} class:assistant={entry.role === 'assistant'}>
            <span class="ts">{formatClock(entry.timestamp)}</span>
            <span class="text">
              {#if range}
                {#each lines.slice(0, range[0]) as line, j}{#if j > 0}<br />{/if}{line}{/each}<br /><button type="button" class="ellipsis" onclick={() => expandEntry(i)} title="Expand {range[1] - range[0] + 1} hidden lines">&lt;...&gt;</button><br />{#each lines.slice(range[1] + 1) as line, j}{#if j > 0}<br />{/if}{line}{/each}
              {:else}
                {#each lines as line, j}{#if j > 0}<br />{/if}{line}{/each}
              {/if}
            </span>
          </div>
        {/if}
      {/each}
      {#if dialog.length === 0}
        <div class="msg">No history</div>
      {/if}
    </div>
  </div>
{:else if sessionId}
  <div class="msg">Loading...</div>
{/if}

<style>
  .entries {
    overflow-y: auto;
    height: 100vh;
    padding: 8px 0;
    color: #d6d6d6;
    display: flex;
    flex-direction: column-reverse;
  }
  .entries-inner {
    display: flex;
    flex-direction: column;
  }
  .entry {
    display: flex;
    gap: 8px;
    padding: 3px 12px;
    line-height: 1.4;
  }
  .ts {
    color: #6b7280;
    font-family: ui-monospace, Consolas, monospace;
    font-variant-numeric: tabular-nums;
    flex-shrink: 0;
  }
  .text {
    color: #e8e8ea;
    word-break: break-word;
    min-width: 0;
    white-space: pre-wrap;
  }
  .sticky {
    background: rgba(255, 255, 255, 0.04);
  }
  .assistant .text {
    font-style: italic;
    color: #6b7280;
  }
  .ellipsis {
    background: none;
    border: none;
    padding: 0;
    margin: 0;
    color: #7aa2f7;
    font: inherit;
    cursor: pointer;
  }
  .ellipsis:hover {
    color: #a4c0ff;
    text-decoration: underline;
  }
  .separator {
    padding: 4px 12px;
  }
  .separator hr {
    border: none;
    border-top: 1px solid rgba(255, 255, 255, 0.1);
    margin: 0;
  }
  .msg {
    display: flex;
    align-items: center;
    justify-content: center;
    height: 100vh;
    font-size: 12px;
    color: #6b7280;
  }
</style>

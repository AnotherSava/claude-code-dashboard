<script lang="ts">
  import { onMount, tick } from 'svelte'
  import SessionList from './lib/components/SessionList.svelte'
  import HistoryApp from './HistoryApp.svelte'
  import LimitBar from './lib/components/LimitBar.svelte'
  import {
    applyAutoResize,
    frontendLog,
    getConfig,
    getSessions,
    getUsageLimits,
    getWindowLabel,
    hideWindow,
    onConfigUpdated,
    onSessionsUpdated,
    onUsageLimitsUpdated,
    refreshUsageLimits,
    showWindow,
  } from './lib/api'
  import type { AgentSession, Config, UsageLimits } from './lib/types'

  let historyMode = $state(false)
  let sessions = $state<AgentSession[]>([])
  let config = $state<Config | null>(null)
  let usage = $state<UsageLimits | null>(null)
  let now = $state(Date.now())

  let widgetEl: HTMLDivElement | undefined = $state()
  let lastSentHeight = -1
  let measureTimer: ReturnType<typeof setTimeout> | null = null

  function scheduleMeasure() {
    if (measureTimer !== null) clearTimeout(measureTimer)
    measureTimer = setTimeout(measureAndSend, 50)
  }

  function measureAndSend() {
    measureTimer = null
    if (!widgetEl || !config || config.auto_resize === 'none') return
    const headerEl = widgetEl.querySelector('header') as HTMLElement | null
    if (!headerEl) return
    // Walk the SessionList's natural content height ourselves rather than
    // reading `.list.scrollHeight`: the list has `flex: 1; min-height: 0`, so
    // when the window is currently larger than its content, it stretches to
    // fill the viewport and `scrollHeight` reports the stretched size, not
    // the intrinsic content size — locking us at whatever height we last set.
    let listH = 0
    const list = widgetEl.querySelector('.list')
    if (list) {
      for (const child of list.children) {
        listH += (child as HTMLElement).getBoundingClientRect().height
      }
    } else if (widgetEl.querySelector('.empty')) {
      listH = 36
    }
    // Use subpixel-accurate getBoundingClientRect() heights, then ceil the
    // total. `offsetHeight` rounds each child down to an integer, and the
    // accumulated fractional loss across rows would leave us asking for ~3-6
    // px less than the content needs — the OS resizes to exactly what we
    // asked, and a scrollbar appears.
    const desired = Math.ceil(headerEl.getBoundingClientRect().height + listH)
    // Always fire when content actually exceeds the viewport — that's the
    // overflow case we're guarding against. Drift sources (DPI shift, OS
    // clamp on a prior request, external resize) leave `lastSentHeight`
    // stale but `window.innerHeight` accurate, so this comparison catches
    // them where a request-based dedup wouldn't.
    const overflowing = desired > window.innerHeight + 1
    // For non-overflow cases, dedup against what we last requested. Crucial
    // for the OS-clamp scenario: if we asked for 87 but Windows enforced a
    // ~150 minimum, viewport stays at 150 while desired stays at 87 — and
    // re-asking for 87 every event would feedback-loop. The request-based
    // dedup pins this at one fire per measurement.
    if (!overflowing && Math.abs(desired - lastSentHeight) < 1) return
    lastSentHeight = desired
    applyAutoResize(desired).catch((err) => console.error('apply_auto_resize failed', err))
  }

  // Re-measure whenever something that affects content height could have
  // changed: session list contents, limit bar visibility, or the mode itself.
  // The dedup in measureAndSend prevents feedback loops from the resulting
  // window resize.
  $effect(() => {
    sessions
    usage
    config?.auto_resize
    scheduleMeasure()
  })

  onMount(() => {
    let unlistenSessions: (() => void) | undefined
    let unlistenConfig: (() => void) | undefined
    let unlistenUsage: (() => void) | undefined

    ;(async () => {
      try {
        const label = await getWindowLabel()
        if (label === 'history') {
          historyMode = true
          return
        }
        config = await getConfig()
        sessions = await getSessions()
        usage = await getUsageLimits()
        frontendLog('trace', 'mount snapshot', {
          five_hour_present: usage?.five_hour != null,
          seven_day_present: usage?.seven_day != null,
          status: usage?.status,
        }).catch(() => {})
        unlistenSessions = await onSessionsUpdated((s) => {
          frontendLog('trace', 'event sessions_updated', { sessions: s.length }).catch(() => {})
          sessions = s
        })
        unlistenConfig = await onConfigUpdated((c) => (config = c))
        unlistenUsage = await onUsageLimitsUpdated((u) => {
          frontendLog('trace', 'event usage_limits_updated', {
            five_hour_present: u.five_hour != null,
            seven_day_present: u.seven_day != null,
            status: u.status,
          }).catch(() => {})
          usage = u
        })
        // Unconditional refresh on mount — if the webview was discarded by OS
        // power-saving and reloaded while the process was suspended, the
        // cached snapshot may be stale. The 60s floor inside the backend
        // protects Anthropic from thrash on real reloads.
        refreshUsageLimits().catch((err) => console.error('mount refresh failed', err))
      } catch (err) {
        frontendLog('error', 'init_failed', { error: String(err) }).catch(() => {})
        console.error('failed to initialize', err)
      } finally {
        await tick()
        // Wait two animation frames before revealing the window: tick() flushes
        // the Svelte update queue, but the browser still needs to composite at
        // least one frame before the WKWebView's layer has dark pixels. Showing
        // any earlier produces a brief white flash on first open on macOS.
        await new Promise<void>((resolve) =>
          requestAnimationFrame(() => requestAnimationFrame(() => resolve()))
        )
        if (!historyMode) {
          try {
            await showWindow()
          } catch (err) {
            console.error('failed to reveal window', err)
          }
        }
      }
    })()

    const tickId = setInterval(() => (now = Date.now()), 1000)

    // Wake the backend poller when the user brings the widget back to the
    // foreground — the process may have been suspended by OS power management
    // while occluded, leaving the bars showing a snapshot from hours ago.
    const onVisibility = () => {
      if (document.visibilityState === 'visible') {
        refreshUsageLimits().catch((err) => console.error('refresh failed', err))
      }
    }
    document.addEventListener('visibilitychange', onVisibility)

    // Re-measure when the viewport itself changes — DPI shifts from a monitor
    // move, manual horizontal drag, anything the OS does to the window outside
    // our own `apply_auto_resize` call. The dedup inside measureAndSend
    // short-circuits when the viewport already matches `desired`, so our own
    // resize calls don't recurse.
    window.addEventListener('resize', scheduleMeasure)

    return () => {
      clearInterval(tickId)
      document.removeEventListener('visibilitychange', onVisibility)
      window.removeEventListener('resize', scheduleMeasure)
      unlistenSessions?.()
      unlistenConfig?.()
      unlistenUsage?.()
      if (measureTimer !== null) clearTimeout(measureTimer)
    }
  })

  function onHide() {
    hideWindow().catch((err) => console.error('hide failed', err))
  }
</script>

{#if historyMode}
  <HistoryApp />
{:else}
<div class="widget" bind:this={widgetEl}>
  <header data-tauri-drag-region>
    <span class="title" data-tauri-drag-region>AI AGENTS</span>
    <div class="limits" data-tauri-drag-region>
      <LimitBar
        bucket={usage?.five_hour ?? null}
        status={usage?.status ?? 'unavailable'}
        updated={usage?.updated ?? 0}
        {now}
        segments={config?.limit_bar_segments ?? 16}
        format="hm"
      />
      <LimitBar
        bucket={usage?.seven_day ?? null}
        status={usage?.status ?? 'unavailable'}
        updated={usage?.updated ?? 0}
        {now}
        segments={config?.limit_bar_segments ?? 16}
        format="dhm"
      />
    </div>
    <button class="hide-btn" onclick={onHide} aria-label="Hide to tray" title="Hide to tray">×</button>
  </header>
  {#if config}
    <SessionList {sessions} {config} {now} />
  {/if}
</div>
{/if}

<style>
  :global(html, body) {
    margin: 0;
    padding: 0;
    height: 100%;
    /* Match the .widget bg so WKWebView's first-paint snapshot on macOS is
       dark even before the .widget layout completes — otherwise the layer
       backing flashes white briefly before the Svelte tree composits. */
    background: #1c1c1e;
    overflow: hidden;
    font-family: system-ui, 'Segoe UI', Roboto, sans-serif;
  }
  :global(*) {
    box-sizing: border-box;
  }
  .widget {
    display: flex;
    flex-direction: column;
    height: 100vh;
    width: 100vw;
    background: #1c1c1e;
    color: #d6d6d6;
    user-select: none;
    -webkit-user-select: none;
  }
  header {
    display: flex;
    align-items: center;
    gap: 4px;
    padding: 4px 4px 4px 12px;
    background: #17171a;
    border-bottom: 1px solid #2a2a2d;
    cursor: grab;
  }
  header:active {
    cursor: grabbing;
  }
  .title {
    font-size: 11px;
    font-weight: 600;
    letter-spacing: 0.6px;
    color: #8a8a8e;
    flex-shrink: 0;
    margin-right: 10px;
  }
  .limits {
    display: flex;
    flex-direction: row;
    align-items: center;
    gap: 8px;
    flex: 1;
    min-width: 0;
  }
  .limits > :global(*) {
    flex: 1 1 0;
    min-width: 0;
  }
  .hide-btn {
    background: transparent;
    border: 0;
    padding: 0 6px;
    color: #8a8a8e;
    font-size: 16px;
    line-height: 1;
    cursor: pointer;
    border-radius: 3px;
    opacity: 0;
    transition: opacity 120ms ease, background 120ms ease, color 120ms ease;
    margin-left: auto;
    flex-shrink: 0;
  }
  header:hover .hide-btn {
    opacity: 1;
  }
  .hide-btn:hover {
    background: #2a2a2d;
    color: #e8e8ea;
  }
</style>

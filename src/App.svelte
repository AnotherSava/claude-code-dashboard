<script lang="ts">
  import { onMount, tick } from 'svelte'
  import SessionList from './lib/components/SessionList.svelte'
  import SetupPanel from './lib/components/SetupPanel.svelte'
  import HistoryApp from './HistoryApp.svelte'
  import AboutApp from './AboutApp.svelte'
  import LimitBar from './lib/components/LimitBar.svelte'
  import {
    applyAutoResize,
    frontendLog,
    getConfig,
    getSessions,
    getSetupState,
    getUsageLimits,
    getWindowLabel,
    hideWindow,
    onConfigUpdated,
    onSessionsUpdated,
    onShowSetupInstructions,
    onUsageLimitsUpdated,
    refreshUsageLimits,
    showWindow,
  } from './lib/api'
  import type { AgentSession, Config, SetupState, UsageLimits } from './lib/types'

  let historyMode = $state(false)
  let aboutMode = $state(false)
  let sessions = $state<AgentSession[]>([])
  let config = $state<Config | null>(null)
  let usage = $state<UsageLimits | null>(null)
  let setup = $state<SetupState | null>(null)
  let now = $state(Date.now())

  // Once the dashboard has *ever* received a hook event (history persisted to
  // prompt_history.json), the onboarding panel is permanently retired. We
  // latch the `has_history` flag from the initial snapshot, then also flip it
  // on the first sessions_updated that carries any session — so the panel
  // disappears immediately on the first event without waiting for a restart.
  let hookEverReceived = $derived(
    (setup?.has_history ?? false) || sessions.length > 0,
  )

  // Per-session manual override on top of `hookEverReceived`. Tray menu
  // "Help → Instructions to connect to Claude" sets `'shown'` and the
  // panel's hover-to-dismiss × sets `'hidden'`. `null` means follow the auto
  // behavior. Reset on every app launch — we don't persist the override.
  let setupOverride = $state<'shown' | 'hidden' | null>(null)
  let showSetup = $derived(
    setupOverride === 'shown' ||
      (setupOverride === null && !hookEverReceived),
  )

  let widgetEl: HTMLDivElement | undefined = $state()
  let lastSentHeight = -1
  let measureTimer: ReturnType<typeof setTimeout> | null = null
  // Timestamp until which `window` 'resize' events are treated as the echo of
  // our own applyAutoResize and ignored. Without this, a resize that lands the
  // window on a different-DPI monitor leaves `innerHeight` != `desired`, so the
  // overflow re-trigger fires forever and drifts the window across screens.
  let suppressResizeUntil = 0

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
    const panel = widgetEl.querySelector('.panel')
    if (list) {
      for (const child of list.children) {
        listH += (child as HTMLElement).getBoundingClientRect().height
      }
    } else if (panel) {
      // SetupPanel renders one block — measure its intrinsic content height
      // so auto-resize grows to fit the onboarding copy.
      listH = (panel as HTMLElement).scrollHeight
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
    // Cover the IPC + OS-resize round-trip so the resulting 'resize' event is
    // recognized as our own echo, not an external change worth re-measuring.
    suppressResizeUntil = Date.now() + 150
    applyAutoResize(desired).catch((err) => console.error('apply_auto_resize failed', err))
  }

  // Re-measure on external viewport changes (manual drag, monitor move, DPI
  // shift) — but skip the echo of our own applyAutoResize, which would
  // otherwise re-arm a feedback loop across a DPI boundary.
  function onWindowResize() {
    if (Date.now() < suppressResizeUntil) return
    scheduleMeasure()
  }

  // Re-measure whenever something that affects content height could have
  // changed: session list contents, limit bar visibility, or the mode itself.
  // The dedup in measureAndSend prevents feedback loops from the resulting
  // window resize.
  $effect(() => {
    sessions
    usage
    config?.auto_resize
    showSetup
    scheduleMeasure()
  })

  onMount(() => {
    let unlistenSessions: (() => void) | undefined
    let unlistenConfig: (() => void) | undefined
    let unlistenUsage: (() => void) | undefined
    let unlistenShowSetup: (() => void) | undefined

    ;(async () => {
      try {
        const label = await getWindowLabel()
        if (label === 'history') {
          historyMode = true
          return
        }
        if (label === 'about') {
          aboutMode = true
          return
        }
        config = await getConfig()
        sessions = await getSessions()
        usage = await getUsageLimits()
        setup = await getSetupState()
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
        unlistenShowSetup = await onShowSetupInstructions(() => {
          // Help menu → Instructions: force the panel visible regardless of
          // history state. Clears any prior dismiss.
          setupOverride = 'shown'
        })
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
        // Only the main widget auto-reveals on mount. History and About
        // windows are shown on demand (history click / Help → About) — the
        // initial visibility comes from `visible: false` in tauri.conf.json.
        if (!historyMode && !aboutMode) {
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
    // our own `apply_auto_resize` call. `onWindowResize` skips the echo of our
    // own resize via the `suppressResizeUntil` cooldown: the measureAndSend
    // dedup alone can't catch it, because across a DPI boundary the post-resize
    // `innerHeight` stops matching `desired` and the overflow path re-fires.
    window.addEventListener('resize', onWindowResize)

    return () => {
      clearInterval(tickId)
      document.removeEventListener('visibilitychange', onVisibility)
      window.removeEventListener('resize', onWindowResize)
      unlistenSessions?.()
      unlistenConfig?.()
      unlistenUsage?.()
      unlistenShowSetup?.()
      if (measureTimer !== null) clearTimeout(measureTimer)
    }
  })

  function onHide() {
    hideWindow().catch((err) => console.error('hide failed', err))
  }
</script>

{#if historyMode}
  <HistoryApp />
{:else if aboutMode}
  <AboutApp />
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
    {#if showSetup && setup}
      <SetupPanel
        snippet={setup.settings_snippet}
        hookPath={setup.hook_script_path}
        onDismiss={() => (setupOverride = 'hidden')}
      />
    {:else}
      <SessionList {sessions} {config} {now} />
    {/if}
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

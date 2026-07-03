<script lang="ts">
  import { onMount, tick } from 'svelte'
  import SessionList from './lib/components/SessionList.svelte'
  import SetupPanel from './lib/components/SetupPanel.svelte'
  import HistoryApp from './HistoryApp.svelte'
  import AboutApp from './AboutApp.svelte'
  import IntensityApp from './IntensityApp.svelte'
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
    onSetupState,
    onShowSetupInstructions,
    onUsageLimitsUpdated,
    refreshUsageLimits,
    showWindow,
  } from './lib/api'
  import type { AgentSession, Config, SetupState, UsageLimits } from './lib/types'

  let historyMode = $state(false)
  let aboutMode = $state(false)
  let intensityMode = $state(false)
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
  // our own applyAutoResize and ignored — so we don't re-measure on a resize we
  // just caused. (The DPI-mismatch loop itself is prevented by sizing in
  // physical px; see measureAndSend.)
  let suppressResizeUntil = 0
  // Self-heal for the display-disable collapse: when a monitor holding the
  // widget is disabled, Windows moves/shrinks the window and swallows the
  // set_size we issue mid-transition (observed in widget.jsonl: innerHeight
  // stuck at 18px while the content wanted 105+). The burst of 'resize' events
  // that drove us here ends while the window is still a header-height sliver,
  // so no further event re-fires a measure and it stays collapsed until some
  // unrelated later trigger. These retries keep re-applying past the transition
  // until the window reaches its content height, then self-cancel. Delays are
  // measured from each retry; ~10s total spans a display/DPI transition without
  // looping forever (the budget then falls back to event-driven re-measures).
  const HEAL_DELAYS = [300, 700, 1500, 3000, 5000]
  let healTimer: ReturnType<typeof setTimeout> | null = null
  let healTries = 0

  function scheduleMeasure() {
    if (measureTimer !== null) clearTimeout(measureTimer)
    measureTimer = setTimeout(measureAndSend, 50)
  }

  function scheduleHeal() {
    if (healTries >= HEAL_DELAYS.length) return // budget spent — fall back to event-driven re-measure
    const delay = HEAL_DELAYS[healTries++]
    if (healTimer !== null) clearTimeout(healTimer)
    healTimer = setTimeout(() => {
      healTimer = null
      measureAndSend()
    }, delay)
  }

  function cancelHeal() {
    healTries = 0
    if (healTimer !== null) {
      clearTimeout(healTimer)
      healTimer = null
    }
  }

  // Re-measure whenever the rendered content height changes, even when no
  // Svelte-tracked dep ($effect below) fired — e.g. the layout settling a frame
  // after a session row was added/removed. This is the backstop that keeps the
  // window from getting stuck shorter than its content after a flapping session
  // count. The observed element shrink-wraps its content (.list-inner / .panel),
  // so a window resize doesn't change its height and can't feed back a loop.
  let contentObserver: ResizeObserver | null = null
  let observedEl: Element | null = null
  function observeContent(el: Element | null) {
    if (el === observedEl) return
    if (!contentObserver) contentObserver = new ResizeObserver(() => scheduleMeasure())
    if (observedEl) contentObserver.unobserve(observedEl)
    observedEl = el
    if (el) contentObserver.observe(el)
  }

  function measureAndSend() {
    measureTimer = null
    if (!widgetEl || !config || config.auto_resize === 'none') return
    const headerEl = widgetEl.querySelector('header') as HTMLElement | null
    if (!headerEl) return
    // Measure a single non-stretching content element rather than summing the
    // scroll viewport's children. The list viewport (`.list`) has
    // `flex: 1; min-height: 0`, so its own `scrollHeight` reports the stretched
    // height when the window exceeds its content; and iterating its live
    // children races with Svelte's keyed-each reconciliation (observed: a
    // child count that disagreed with the laid-out scrollHeight, yielding a
    // `desired` one row too small and a stuck scrollbar). `.list-inner`
    // shrink-wraps the rows, so one `getBoundingClientRect()` read gives the
    // true, internally-consistent content height.
    let listH = 0
    const content = widgetEl.querySelector('.list-inner') as HTMLElement | null
    const panel = widgetEl.querySelector('.panel') as HTMLElement | null
    if (content) {
      listH = content.getBoundingClientRect().height
    } else if (panel) {
      // SetupPanel renders one block — measure its intrinsic content height
      // so auto-resize grows to fit the onboarding copy.
      listH = panel.scrollHeight
    } else if (widgetEl.querySelector('.empty')) {
      listH = 36
    }
    observeContent(content ?? panel ?? widgetEl.querySelector('.empty'))
    // Subpixel-accurate getBoundingClientRect(), then ceil the total: rounding
    // down would leave us asking for a hair less than the content needs and the
    // OS would resize to exactly that, surfacing a scrollbar.
    const desired = Math.ceil(headerEl.getBoundingClientRect().height + listH)
    // Always fire when content actually exceeds the viewport — that's the
    // overflow case we're guarding against. Drift sources (DPI shift, OS
    // clamp on a prior request, external resize) leave `lastSentHeight`
    // stale but `window.innerHeight` accurate, so this comparison catches
    // them where a request-based dedup wouldn't.
    const vh = window.innerHeight
    const overflowing = desired > vh + 1
    // For non-overflow cases, dedup against what we last requested. Crucial
    // for the OS-clamp scenario: if we asked for 87 but Windows enforced a
    // ~150 minimum, viewport stays at 150 while desired stays at 87 — and
    // re-asking for 87 every event would feedback-loop. The request-based
    // dedup pins this at one fire per measurement.
    if (!overflowing && Math.abs(desired - lastSentHeight) < 1) {
      cancelHeal() // window already matches its content — stop any heal retries
      return
    }
    lastSentHeight = desired
    // Send PHYSICAL pixels computed from the webview's own devicePixelRatio,
    // not a logical height for Rust to scale. Near a mixed-DPI monitor boundary
    // Rust's window.scale_factor() and the webview's devicePixelRatio disagree,
    // so a logical request lands at the wrong physical size and the viewport
    // never matches `desired` — permanent false-overflow, scrollbar, and drift.
    const dpr = window.devicePixelRatio
    suppressResizeUntil = Date.now() + 150
    frontendLog('trace', 'auto_resize measure', {
      desired,
      dpr,
      inner_height: vh,
      physical: Math.round(desired * dpr),
      heal: healTries,
    }).catch(() => {})
    applyAutoResize(Math.round(desired * dpr)).catch((err) => console.error('apply_auto_resize failed', err))
    // A window less than half the height its content needs is the OS swallowing
    // our resize during a display/DPI transition — not the ~1-frame lag of a
    // normal grow (which clears the half-height bar and settles on its own).
    // Keep re-applying past the transition; it self-cancels once the window
    // catches up. Re-arm if a fresh collapse shows up after the budget is spent.
    const collapsed = overflowing && vh * 2 < desired
    if (overflowing && (collapsed || healTries > 0)) {
      if (collapsed && healTries >= HEAL_DELAYS.length) healTries = 0
      scheduleHeal()
    } else if (!overflowing) {
      cancelHeal()
    }
  }

  // Re-measure on external viewport changes (manual drag, monitor move, DPI
  // shift) — but skip the 'resize' event our own applyAutoResize triggers, so
  // we don't spend a measure pass reacting to a resize we just caused.
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
    let unlistenSetupState: (() => void) | undefined

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
        if (label === 'intensity') {
          intensityMode = true
          return
        }
        // Attach before show_window below: the mount-time getSetupState() can
        // win the race against the backend managing PromptHistoryStore and latch
        // has_history=false (flashing the onboarding panel on a configured
        // install). show_window re-emits the authoritative setup_state (same fix
        // as config_updated), and this listener overwrites the stale snapshot —
        // registered first so it's listening before show_window fires.
        unlistenSetupState = await onSetupState((s) => (setup = s))
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
        // Authoritative config re-read to close the get_config mount race: the
        // getConfig() at the top of mount can beat setup()'s ConfigState
        // management and return Config::default() (auto_resize 'none'), which
        // would disable auto-resize for the session. By here the backend has
        // answered several commands (config / sessions / usage / setup), so it is
        // up — re-read once to correct any raced value. A single sequenced read,
        // not a loop; placed before the finally so a stalled pre-show rAF can't
        // skip it.
        const authoritativeConfig = await getConfig()
        if (authoritativeConfig) config = authoritativeConfig
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
        // Only the main widget auto-reveals on mount. History, About and the
        // intensity chart are shown on demand (history click / Help → About /
        // tray → Work intensity) — initial visibility comes from
        // `visible: false` in tauri.conf.json.
        if (!historyMode && !aboutMode && !intensityMode) {
          try {
            await showWindow()
            // Take one measure now the window is visible and the DOM is committed
            // (tick + two frames above) — the reactive measure can race ahead of
            // the rows laying out, and nothing re-fires it if the list then stays
            // put. The auto_resize='none' get_config race is closed by the
            // authoritative re-read above; later session/usage updates refine the
            // height.
            scheduleMeasure()
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
    // own resize via the `suppressResizeUntil` cooldown — cheap hygiene so a
    // self-induced resize doesn't trigger a redundant measure pass. (Sizing in
    // physical px is what actually keeps `innerHeight` matching `desired`, so
    // the overflow path settles instead of looping.)
    window.addEventListener('resize', onWindowResize)

    return () => {
      clearInterval(tickId)
      document.removeEventListener('visibilitychange', onVisibility)
      window.removeEventListener('resize', onWindowResize)
      unlistenSessions?.()
      unlistenConfig?.()
      unlistenUsage?.()
      unlistenShowSetup?.()
      unlistenSetupState?.()
      if (measureTimer !== null) clearTimeout(measureTimer)
      if (healTimer !== null) clearTimeout(healTimer)
      contentObserver?.disconnect()
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
{:else if intensityMode}
  <IntensityApp />
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

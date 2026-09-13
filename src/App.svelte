<script lang="ts">
  import { onMount, tick } from 'svelte'
  import SessionList from './lib/components/SessionList.svelte'
  import StartApprovals from './lib/components/StartApprovals.svelte'
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
    getStartApprovals,
    getUsageLimits,
    getWindowLabel,
    getWindowLabelSync,
    hideWindow,
    onConfigUpdated,
    onRefitWindow,
    onSessionsUpdated,
    onShowSetupInstructions,
    onStartApprovalsUpdated,
    type PendingStart,
    onUsageLimitsUpdated,
    refreshUsageLimits,
    setCompactWidth,
    showWindow,
  } from './lib/api'
  import type { AgentSession, Config, SetupState, UsageLimits } from './lib/types'

  let historyMode = $state(false)
  let aboutMode = $state(false)
  let intensityMode = $state(false)
  // Auto-resize is main-window-only. Resolved synchronously (not via the async
  // getWindowLabel() in onMount, which lands after the first reactive measure)
  // so the measure subsystem can exclude the secondary windows — history /
  // about / intensity render their own root, never `.widget`, so their
  // widgetEl is permanently absent and would otherwise read as a readiness gap.
  const isMainWindow = getWindowLabelSync() === 'main'

  // Only the widget's window is transparent, so only its page may be.
  //
  // This component is the root of ALL FOUR windows — it routes to HistoryApp,
  // AboutApp and IntensityApp below — so a `:global(html, body)` rule here
  // reaches every one of them. Making that rule transparent outright is what
  // turned the Work intensity chart into light-on-light: its window is opaque,
  // so a transparent page shows the OS's default white behind the text.
  // IntensityApp and AboutApp each set their own `:global(html, body)` to the
  // dark theme colour, which did not save them — the two rules have equal
  // specificity, so the later one in the bundle simply won.
  //
  // The class is set here rather than in `onMount` because it has to be on the
  // element before the first paint, and module-scope runs during component init
  // while `onMount` runs after the first render.
  if (isMainWindow && typeof document !== 'undefined') document.documentElement.classList.add('transparent-shell')
  // Whether the pointer is really over the header — see the `.hide-btn.shown`
  // rule for why `header:hover` cannot answer that on a transparent window.
  //
  // The listeners are attached here rather than written as `onpointerenter` in
  // the markup because svelte-check refuses both spellings of that: a `<header>`
  // carrying pointer handlers "must have an ARIA role", and giving it the
  // `banner` role it already implies is "redundant". Binding the element sides
  // with the linter on both counts and keeps `npm run check` at zero warnings,
  // which is the state this repo's gate is kept in.
  let headerEl: HTMLElement | undefined = $state()
  let headerHovered = $state(false)
  $effect(() => {
    const el = headerEl
    if (!el) return
    // ASKED OF EVERY MOVE, not of crossings. A crossing event is exactly what
    // this window does not get reliably: entering was never observed when the
    // widget was moved under a stationary pointer, and leaving is missed often
    // enough that the button stayed visible after the pointer had gone — with
    // `header:hover` stale-true at the same time, so neither half could clear
    // it. A position test has no such state to lose: every move re-answers the
    // question from where the pointer actually is.
    const update = (e: PointerEvent) => {
      const r = el.getBoundingClientRect()
      headerHovered = e.clientX >= r.left && e.clientX <= r.right && e.clientY >= r.top && e.clientY <= r.bottom
    }
    // The two ways the pointer stops producing moves in here: it left the page,
    // and the window lost focus. Both mean "not hovering" until a move says
    // otherwise, and any real move corrects them immediately.
    const clear = () => (headerHovered = false)
    window.addEventListener('pointermove', update, true)
    document.documentElement.addEventListener('pointerleave', clear)
    window.addEventListener('blur', clear)
    return () => {
      window.removeEventListener('pointermove', update, true)
      document.documentElement.removeEventListener('pointerleave', clear)
      window.removeEventListener('blur', clear)
    }
  })
  let sessions = $state<AgentSession[]>([])
  let startApprovals = $state<PendingStart[]>([])
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
  let lastSentScale = 0
  // Instrumentation (temporary): throttles the deduped-measure trace to one line
  // per distinct (desired, viewport, scale) signature — see measureAndSend.
  let lastDedupSig = ''
  let measureTimer: ReturnType<typeof setTimeout> | null = null
  // Timestamp until which `window` 'resize' events are treated as the echo of
  // our own applyAutoResize and ignored — so we don't re-measure on a resize we
  // just caused. (The DPI-mismatch loop itself is prevented by sizing in
  // physical px; see measureAndSend.)
  let suppressResizeUntil = 0
  // Size the window against the webview's own devicePixelRatio — the ratio it
  // actually rasterizes content at, so `cssHeight * devicePixelRatio` is by
  // construction the physical height that fits the content. We deliberately do
  // NOT size against Rust's window.scale_factor(): it's read at window creation
  // and does not track the window later landing on a different-DPI monitor, so
  // on a mixed-DPI setup it goes stale. Observed in widget.jsonl after a
  // relaunch onto a 150% monitor: devicePixelRatio correctly settled to 1.5
  // within ~2s while scale_factor stayed stuck at 1.0 for 35+min, sizing the
  // window at 1.0 and clipping the 1.5-rendered content to a sliver. (An earlier
  // fix trusted scale_factor to dodge a devicePixelRatio flap; that traded a
  // transient failure for this persistent one — see below and the
  // `auto_resize_dpr_flicker_collapse` memory.)
  //
  // devicePixelRatio's only unreliability is a brief mount-time transient — it
  // can read 1.0 for ~2s before settling — and that self-corrects because
  // *every* dpr change re-fires a measure: the 'resize' listener plus the
  // matchMedia('resolution') listener installed in onMount guarantee a settled
  // dpr always re-sizes the window. That re-measure-on-change is the real cure
  // for the stuck-sliver symptom in both directions. (Width is preserved
  // backend-side, so a `minWidth` floor in tauri.conf.json guards that axis.)
  function effectiveScale(): number {
    return window.devicePixelRatio || 1
  }
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

  // Measure with the dedup disarmed. A plain scheduleMeasure() is not enough
  // after the window comes back from being minimized: minimizing never resizes
  // the webview, so `window.innerHeight` is still the pre-minimize value while
  // the real window is whatever height the restore produced. `desired` then
  // matches both `lastSentHeight` and the stale viewport, the pass dedups, and
  // nothing is applied — the window keeps the wrong height. Clearing the keys
  // forces one real apply through; the backend is the side that knows the two
  // disagree (see lib.rs refit_after_restore).
  function forceMeasure() {
    lastSentHeight = -1
    lastSentScale = 0
    cancelHeal()
    scheduleMeasure()
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

  // Bounded retry for a *missed initial measure*, distinct from the collapse
  // heal above. Every measure trigger (mount, the $effect, the ResizeObserver)
  // fires exactly once; if the first one lands before the widget subtree is
  // committed, measureAndSend early-returns and — because nothing guarantees a
  // later event re-fires it and the collapse heal only rescues too-*short*
  // windows — the window freezes at its default (tall) height with no recovery
  // (observed after a deploy relaunch: zero measures, window stuck ~480px).
  // These retries re-attempt on a short backoff until the first real
  // measurement lands, then go quiet and hand off to the event-driven path.
  const READY_RETRY_DELAYS = [50, 150, 300, 600, 1000, 1500]
  let readyRetryTimer: ReturnType<typeof setTimeout> | null = null
  let readyRetries = 0
  let firstMeasureDone = false
  let warnedNotReady = false
  // One-shot: log the first time auto-resize is skipped for auto_resize='none'.
  // Post-fix the backend can't race this to 'none' (ConfigState is managed before
  // the webview loads), so for a user who enabled auto-resize this must NEVER
  // appear — its presence in widget.jsonl is a positive regression signal. For a
  // user who genuinely left auto-resize off it fires once per session, benign.
  let loggedAutoResizeDisabled = false

  function scheduleReadyRetry() {
    if (firstMeasureDone) return
    if (readyRetries >= READY_RETRY_DELAYS.length) return // budget spent — fall back to event-driven re-measure
    const delay = READY_RETRY_DELAYS[readyRetries++]
    if (readyRetryTimer !== null) clearTimeout(readyRetryTimer)
    readyRetryTimer = setTimeout(() => {
      readyRetryTimer = null
      measureAndSend()
    }, delay)
  }

  function cancelReadyRetry() {
    if (readyRetryTimer !== null) {
      clearTimeout(readyRetryTimer)
      readyRetryTimer = null
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
  // A second, independent observer target. `observeContent` unobserves whatever
  // it watched before — it tracks a single moving element (list / panel / empty)
  // — so reusing it for the approvals block would make the two evict each other.
  let observedApprovals: Element | null = null
  let approvalsObserver: ResizeObserver | null = null
  function observeApprovals(el: Element | null) {
    if (el === observedApprovals) return
    if (!approvalsObserver) approvalsObserver = new ResizeObserver(() => scheduleMeasure())
    if (observedApprovals) approvalsObserver.unobserve(observedApprovals)
    observedApprovals = el
    if (el) approvalsObserver.observe(el)
  }

  function observeContent(el: Element | null) {
    if (el === observedEl) return
    if (!contentObserver) contentObserver = new ResizeObserver(() => scheduleMeasure())
    if (observedEl) contentObserver.unobserve(observedEl)
    observedEl = el
    if (el) contentObserver.observe(el)
  }

  function measureAndSend() {
    measureTimer = null
    // Secondary windows never mount the `.widget` tree, so their permanently
    // absent widgetEl must not be treated as a readiness gap (no log, no retry).
    if (!isMainWindow) {
      cancelReadyRetry()
      return
    }
    // A deliberate disable is not a readiness failure — stop any pending retry
    // and bail. (Guarded on config existing so a null config falls through to
    // the not-ready path below rather than being read as 'not none'.)
    if (config && config.auto_resize === 'none') {
      if (!loggedAutoResizeDisabled) {
        loggedAutoResizeDisabled = true
        frontendLog('debug', 'auto_resize disabled', { auto_resize: config.auto_resize }).catch(() => {})
      }
      cancelReadyRetry()
      return
    }
    // Resolve the subtree once: header plus a content element (SessionList's
    // `.list-inner` or `.empty`, or SetupPanel's `.panel`). SessionList always
    // renders one of the former when `config` is set, so their absence means
    // the tree hasn't committed yet — a readiness gap, not an empty widget.
    const headerEl = widgetEl?.querySelector('header') as HTMLElement | null
    const content = widgetEl?.querySelector('.list-inner') as HTMLElement | null
    const panel = widgetEl?.querySelector('.panel') as HTMLElement | null
    const emptyEl = widgetEl?.querySelector('.empty') as HTMLElement | null
    // A third sibling inside .widget, outside the header/content pair the sum
    // below is built from. Without this term the window is sized short by
    // exactly this block's height, the list loses that space to it, and the
    // resulting scrollbar self-corrects nowhere: `desired` never exceeds the
    // viewport, so neither the overflow path nor the collapse heal fires.
    const approvalsEl = widgetEl?.querySelector('.approvals') as HTMLElement | null
    const contentEl = content ?? panel ?? emptyEl
    if (!widgetEl || !config || !headerEl || !contentEl) {
      // Not ready — retry until the tree commits (see scheduleReadyRetry). Log
      // the specific missing piece once, so a stuck window is self-explaining
      // in widget.jsonl rather than a silent early-return.
      if (!warnedNotReady) {
        warnedNotReady = true
        frontendLog('debug', 'auto_resize measure not ready', {
          has_widget: !!widgetEl,
          has_config: !!config,
          has_header: !!headerEl,
          has_content: !!contentEl,
        }).catch(() => {})
      }
      scheduleReadyRetry()
      return
    }
    // The subtree is committed — the initial-measure race is closed; the
    // event-driven triggers ($effect, ResizeObserver) carry it from here.
    firstMeasureDone = true
    cancelReadyRetry()
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
    if (content) {
      listH = content.getBoundingClientRect().height
    } else if (panel) {
      // SetupPanel renders one block — measure its intrinsic content height
      // so auto-resize grows to fit the onboarding copy.
      listH = panel.scrollHeight
    } else if (emptyEl) {
      listH = 36
    }
    observeContent(contentEl)
    // Subpixel-accurate getBoundingClientRect(), then ceil the total: rounding
    // down would leave us asking for a hair less than the content needs and the
    // OS would resize to exactly that, surfacing a scrollbar.
    // Observed as well as summed: this block changes its own height from state
    // inside the component — a revealed trust warning, a grant error, a button
    // label — and the `$effect` below can only see the array identity, so
    // nothing else would re-measure.
    observeApprovals(approvalsEl)
    const approvalsH = approvalsEl?.getBoundingClientRect().height ?? 0
    const desired = Math.ceil(headerEl.getBoundingClientRect().height + approvalsH + listH)
    // Always fire when content actually exceeds the viewport — that's the
    // overflow case we're guarding against. Drift sources (DPI shift, OS
    // clamp on a prior request, external resize) leave `lastSentHeight`
    // stale but `window.innerHeight` accurate, so this comparison catches
    // them where a request-based dedup wouldn't.
    const vh = window.innerHeight
    const overflowing = desired > vh + 1
    // Resolve the scale to size with BEFORE the dedup: a scale change moves the
    // physical height even when `desired` is unchanged, so it has to be part of
    // the dedup key or a settled DPI change would be silently skipped.
    const scale = effectiveScale()
    // For non-overflow cases, dedup against what we last requested. Crucial
    // for the OS-clamp scenario: if we asked for 87 but Windows enforced a
    // ~150 minimum, viewport stays at 150 while desired stays at 87 — and
    // re-asking for 87 every event would feedback-loop. The request-based
    // dedup pins this at one fire per measurement.
    if (!overflowing && Math.abs(desired - lastSentHeight) < 1 && scale === lastSentScale) {
      cancelHeal() // window already matches its content — stop any heal retries
      // Instrumentation (temporary — remove once the DPI-transient repro is
      // captured): a deduped pass means we believe the window already fits. Log
      // it whenever the (desired, viewport, scale) signature changes, so a
      // stale-metric desync — desired ≈ inner_height here while the window is
      // physically a row short — surfaces in the trace without flooding on the
      // per-second dedup churn. Pairs with the requested-vs-actual client height
      // now logged Rust-side in auto_resize::apply.
      const sig = `${desired}:${Math.round(vh)}:${scale}`
      if (sig !== lastDedupSig) {
        lastDedupSig = sig
        frontendLog('trace', 'auto_resize dedup', {
          desired,
          inner_height: vh,
          dpr: scale,
          raw_dpr: window.devicePixelRatio,
        }).catch(() => {})
      }
      return
    }
    lastSentHeight = desired
    lastSentScale = scale
    // Send PHYSICAL pixels — `desired` (CSS px) times the webview's own
    // devicePixelRatio (see effectiveScale) — not a logical height for Rust to
    // scale. Near a mixed-DPI boundary Rust's window.scale_factor() and the
    // webview's dpr disagree, so a logical request would land at the wrong
    // physical size (false-overflow, scrollbar, drift); sizing in physical px
    // off the ratio the webview actually rasterizes at avoids both that and the
    // stale-scale collapse.
    suppressResizeUntil = Date.now() + 150
    frontendLog('trace', 'auto_resize measure', {
      desired,
      dpr: scale,
      raw_dpr: window.devicePixelRatio,
      inner_height: vh,
      physical: Math.round(desired * scale),
      heal: healTries,
    }).catch(() => {})
    applyAutoResize(Math.round(desired * scale))
      .catch((err) => console.error('apply_auto_resize failed', err))
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
    startApprovals
    usage
    config?.auto_resize
    config?.compact_mode
    showSetup
    scheduleMeasure()
  })

  // --- Compact-view width fit -------------------------------------------------
  // In compact view the window width is fitted to the header's natural content
  // width (backend anchors it to the right edge). The compact width is never
  // persisted; leaving compact restores the non-compact width the backend
  // captured on entry. `prevCompact` gates the one-shot restore so we don't
  // re-send it on every unrelated re-render while non-compact.
  let prevCompact: boolean | undefined = undefined
  let widthTimer: ReturnType<typeof setTimeout> | null = null

  // The header's natural (min-content) width in CSS px, or null if the header
  // subtree isn't laid out yet. Everything up to the last limit bar is
  // left-packed, so that bar's right edge is its true position; the hover-× is
  // pushed to the far right by `margin-left: auto` today, so we place it just
  // after the bars (one header gap) rather than trusting its stretched
  // position. Spacing is read from computed styles so this survives CSS tweaks.
  function measureHeaderWidth(): number | null {
    const header = widgetEl?.querySelector('header') as HTMLElement | null
    const limits = header?.querySelector('.limits') as HTMLElement | null
    const lastBar = limits?.lastElementChild as HTMLElement | null
    const hide = header?.querySelector('.hide-btn') as HTMLElement | null
    if (!header || !limits || !lastBar || !hide) return null
    const cs = getComputedStyle(header)
    const headerGap = parseFloat(cs.columnGap || cs.gap || '0') || 0
    const padRight = parseFloat(cs.paddingRight || '0') || 0
    const left = header.getBoundingClientRect().left
    const barsRight = lastBar.getBoundingClientRect().right
    const hideW = hide.getBoundingClientRect().width
    return Math.ceil(barsRight - left + headerGap + hideW + padRight)
  }

  function scheduleWidthAdjust() {
    if (widthTimer !== null) clearTimeout(widthTimer)
    // Slightly longer than the height debounce so the compact layout (bars
    // re-rendering to their number-only form) has committed before we measure.
    widthTimer = setTimeout(widthAdjust, 60)
  }

  function widthAdjust() {
    widthTimer = null
    if (!isMainWindow || !config) return
    const compact = !!config.compact_mode
    if (compact) {
      const cssW = measureHeaderWidth()
      if (cssW == null) return // header not laid out yet; a later trigger re-fires
      prevCompact = true
      setCompactWidth(true, cssW * effectiveScale()).catch((err) =>
        console.error('set_compact_width failed', err),
      )
    } else {
      // Only send the restore once, on the compact → non-compact transition.
      if (prevCompact) {
        setCompactWidth(false, null).catch((err) =>
          console.error('set_compact_width failed', err),
        )
      }
      prevCompact = false
    }
  }

  // Re-fit compact width when the mode flips or the header content changes
  // (usage numbers alter the compacted bar width). The backend dedups a
  // re-measure that matches the current width, so this can't feed back a loop.
  $effect(() => {
    config?.compact_mode
    usage
    scheduleWidthAdjust()
  })

  onMount(() => {
    let unlistenApprovals: (() => void) | undefined
    let unlistenSessions: (() => void) | undefined
    let unlistenConfig: (() => void) | undefined
    let unlistenUsage: (() => void) | undefined
    let unlistenShowSetup: (() => void) | undefined
    let unlistenRefit: (() => void) | undefined

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
        config = await getConfig()
        sessions = await getSessions()
        startApprovals = await getStartApprovals()
        usage = await getUsageLimits()
        setup = await getSetupState()
        frontendLog('trace', 'mount snapshot', {
          five_hour_present: usage?.five_hour != null,
          seven_day_present: usage?.seven_day != null,
          status: usage?.status,
        }).catch(() => {})
        unlistenApprovals = await onStartApprovalsUpdated((p) => {
          startApprovals = p
        })
        unlistenSessions = await onSessionsUpdated((s) => {
          frontendLog('trace', 'event sessions_updated', { sessions: s.length }).catch(() => {})
          sessions = s
        })
        unlistenConfig = await onConfigUpdated((c) => (config = c))
        unlistenRefit = await onRefitWindow(() => {
          frontendLog('debug', 'refit_window', { inner_height: window.innerHeight }).catch(() => {})
          forceMeasure()
          // The width is fitted from the header's own content, which a
          // minimize/restore doesn't change — but it is applied against the
          // window's live geometry, so re-run it for the same reason.
          scheduleWidthAdjust()
        })
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
            // put. The initial getConfig() above is authoritative (the backend
            // manages ConfigState before the webview loads, so auto_resize can't
            // race to 'none'); later session/usage updates refine the height.
            scheduleMeasure()
            // Same for the compact width fit — close the initial-measure gap so a
            // launch-into-compact shrinks to the header without waiting for a
            // later usage/config trigger.
            scheduleWidthAdjust()
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

    // Re-measure whenever the webview's devicePixelRatio changes — the ~2s
    // mount-time settle, or a monitor move across a DPI boundary. A dpr change
    // doesn't reliably emit a 'resize' event, so without this a settled dpr
    // could leave the window sized against the pre-change ratio until an
    // unrelated trigger (the stuck-sliver symptom). matchMedia fires once per
    // change and the query string pins the current ratio, so re-arm after each.
    let dprMedia: MediaQueryList | null = null
    function armDprListener() {
      dprMedia?.removeEventListener('change', onDprChange)
      dprMedia = window.matchMedia(`(resolution: ${window.devicePixelRatio}dppx)`)
      dprMedia.addEventListener('change', onDprChange)
    }
    function onDprChange() {
      armDprListener()
      scheduleMeasure()
      // Re-fit compact width at the new scale too: the width sent to the backend
      // is physical (cssWidth × dpr), so a dpr transient at mount or a DPI-boundary
      // move would otherwise leave the compact window fitted at the stale scale.
      scheduleWidthAdjust()
    }
    armDprListener()

    return () => {
      clearInterval(tickId)
      document.removeEventListener('visibilitychange', onVisibility)
      window.removeEventListener('resize', onWindowResize)
      dprMedia?.removeEventListener('change', onDprChange)
      unlistenApprovals?.()
      unlistenSessions?.()
      unlistenConfig?.()
      unlistenUsage?.()
      unlistenShowSetup?.()
      unlistenRefit?.()
      if (measureTimer !== null) clearTimeout(measureTimer)
      if (healTimer !== null) clearTimeout(healTimer)
      if (widthTimer !== null) clearTimeout(widthTimer)
      cancelReadyRetry()
      contentObserver?.disconnect()
      approvalsObserver?.disconnect()
    }
  })

  // Usage palette for the limit bars, with a pre-config-load fallback to the
  // built-in defaults (mirrors config.rs UsageColors::default()).
  const usageColors = $derived(
    config?.usage_colors ?? { green: '#5ad278', amber: '#f0c846', red: '#ff5a5a' },
  )

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
  <header bind:this={headerEl} data-tauri-drag-region>
    <span class="title" data-tauri-drag-region>AI AGENTS</span>
    <div class="limits" class:compact={config?.compact_mode} data-tauri-drag-region>
      <LimitBar
        bucket={usage?.five_hour ?? null}
        status={usage?.status ?? 'unavailable'}
        updated={usage?.updated ?? 0}
        {now}
        segments={config?.limit_bar_segments ?? 16}
        format="hm"
        colors={usageColors}
        compact={config?.compact_mode ?? false}
      />
      <LimitBar
        bucket={usage?.seven_day ?? null}
        status={usage?.status ?? 'unavailable'}
        updated={usage?.updated ?? 0}
        {now}
        segments={config?.limit_bar_segments ?? 16}
        format="dhm"
        colors={usageColors}
        compact={config?.compact_mode ?? false}
      />
    </div>
    <button class="hide-btn" class:shown={headerHovered} onclick={onHide} aria-label="Hide to tray" title="Hide to tray">×</button>
  </header>
  <StartApprovals pending={startApprovals} />
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
    /* Dark, not transparent, because this rule reaches all four windows and
       three of them sit on an opaque window where a transparent page shows the
       OS's white. It also keeps WKWebView's first-paint snapshot dark instead of
       flashing white before the Svelte tree composites. */
    background: #1c1c1e;
    overflow: hidden;
    font-family: system-ui, 'Segoe UI', Roboto, sans-serif;
  }
  /* The widget's window alone is `transparent: true`, and this is the layer its
     rounded corners are cut out of: anything opaque here fills them back in.
     The class is put on <html> by the script above, for the main window only. */
  :global(html.transparent-shell),
  :global(html.transparent-shell body) {
    background: transparent;
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
    /* The widget draws its own rounded corners, because nothing else will.
       macOS rounds a *titled* window's frame at the compositor, and this window
       is `decorations: false`, so it got square corners while every comparable
       floating panel on the platform is round — and while the same widget on
       Windows 11, which rounds the window itself, was round already. The two
       builds disagreed about the shape of one widget.

       `overflow: hidden` is what makes the radius bite: the header and the
       session list are opaque and would otherwise paint over the curve. */
    border-radius: 10px;
    overflow: hidden;
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
  /* Compact bars shrink-wrap their two numbers instead of stretching to fill
     the header — without the segmented track there's nothing to fill. */
  .limits.compact > :global(*) {
    flex: 0 0 auto;
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
  /* BOTH a real pointer event and `:hover` are required, and the redundancy is
     deliberate: each one alone fails in a different direction. `:hover` alone
     shows the button when nothing is near it — on a transparent window WebKit's
     hover hit-test cannot be trusted. A JS flag alone would be the only thing
     standing between the user and a button that never appears, on a mechanism
     that could not be verified from here: pointer control is blocked in this
     environment, and moving the window under the pointer produced no crossing
     event, so the positive path was never observed. Requiring both means the
     worst a wrong flag can do is leave the button hidden on a hover, which the
     tray's Show / Hide already covers, rather than publish a stray × on every
     reveal. Measured
     2026-09-11 with the pointer at screen (1354, 58) and the widget at
     (1036, 511, 420x297) — well outside it, and above it: the button rendered
     fully hovered, background and all, every time the window was shown. In
     window coordinates that pointer is x=318, which is exactly this button's
     column, and y=-453, which is not in the window at all, so the negative y
     appears to be clamped onto the header. The same test on an opaque window
     showed no button, so this is transparency's doing and not a pre-existing
     fault. A hit-test artefact still matches `:hover` in the style engine; it
     does not deliver a `pointerenter`, which is why the state moved to JS. */
  header:hover .hide-btn.shown {
    opacity: 1;
  }
  .hide-btn:hover {
    background: #2a2a2d;
    color: #e8e8ea;
  }
</style>

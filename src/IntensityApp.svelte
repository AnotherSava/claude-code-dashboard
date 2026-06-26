<script lang="ts">
  import { onMount } from 'svelte'
  import type { UnlistenFn } from '@tauri-apps/api/event'
  import { closeWindow, getUsageIntensityWeek, getUsageIntensityWeeks, onUsageLimitsUpdated } from './lib/api'
  import type { WeekChart } from './lib/types'

  const BUCKET_MS = 10 * 60 * 1000
  const SLOTS_PER_DAY = 144 // 6 per hour × 24
  const DAYS = 7
  const DAY_MS = 86400000
  const WEEKS_PER_SCREEN = 7 // a screenful of week-rows (mirrors the 7 day-rows)
  const WEEK_GROUP = 3 // weeks view groups N 10-min buckets into one coarser bar

  // 'day' = one week shown as 7 day-rows; 'week' = one row per week (overview).
  let view = $state<'day' | 'week'>('day')
  let weekOffset = $state(0)
  let chart = $state<WeekChart | null>(null)
  let weeks = $state<WeekChart[] | null>(null)
  // How many weeks back from the newest the window's *bottom* row sits. 0 keeps
  // the most recent week pinned to the bottom (newest-at-bottom, like the days).
  let weekBottomOffset = $state(0)
  let error = $state<string | null>(null)
  let canvasEl: HTMLCanvasElement | undefined = $state()
  let hover = $state<{ x: number; y: number; text: string } | null>(null)

  let prevDisabled = $derived(
    chart === null || chart.data_min_ms === null || chart.week_start_ms <= chart.data_min_ms,
  )
  let nextDisabled = $derived(weekOffset >= 0)

  async function load(offset: number) {
    error = null
    try {
      chart = await getUsageIntensityWeek(offset)
      weekOffset = offset
    } catch (e) {
      error = String(e)
    }
  }

  async function loadWeeks() {
    try {
      weeks = await getUsageIntensityWeeks()
      const w = weekWindow()
      if (w) weekBottomOffset = Math.min(weekBottomOffset, w.maxOffset)
    } catch (e) {
      error = String(e)
    }
  }

  function setView(v: 'day' | 'week') {
    view = v
    if (v === 'week' && weeks === null) loadWeeks()
  }

  // The visible slice of the (newest-first) weeks array. `count` is constant
  // once there are at least a screenful of weeks, so rows keep a steady height.
  // `offset` is the clamped `weekBottomOffset`; the bottom row is `all[offset]`.
  function weekWindow(): { all: WeekChart[]; count: number; offset: number; maxOffset: number } | null {
    const all = weeks
    if (!all || all.length === 0) return null
    const count = Math.min(WEEKS_PER_SCREEN, all.length)
    const maxOffset = Math.max(0, all.length - count)
    const offset = Math.min(Math.max(weekBottomOffset, 0), maxOffset)
    return { all, count, offset, maxOffset }
  }

  function step(delta: number) {
    if (delta < 0 && prevDisabled) return
    if (delta > 0 && nextDisabled) return
    load(weekOffset + delta)
  }

  // Scroll the week window. `+delta` moves toward older weeks (up the screen),
  // `-delta` toward newer (down). `weekPage` jumps a whole screenful.
  function weekStep(delta: number) {
    const w = weekWindow()
    if (!w) return
    weekBottomOffset = Math.min(Math.max(weekBottomOffset + delta, 0), w.maxOffset)
  }
  function weekPage(delta: number) {
    const w = weekWindow()
    if (!w) return
    weekBottomOffset = Math.min(Math.max(weekBottomOffset + delta * w.count, 0), w.maxOffset)
  }

  function onKeydown(e: KeyboardEvent) {
    if (e.key === 'Escape') {
      closeWindow()
      return
    }
    if (view === 'day') {
      // Left/right page one week at a time through the single-week view.
      if (e.key === 'ArrowLeft') {
        e.preventDefault()
        step(-1)
      }
      if (e.key === 'ArrowRight') {
        e.preventDefault()
        step(1)
      }
      return
    }
    // Week view: up/down move one week (before/after), left/right a screenful.
    if (e.key === 'ArrowUp') {
      e.preventDefault()
      weekStep(1)
    }
    if (e.key === 'ArrowDown') {
      e.preventDefault()
      weekStep(-1)
    }
    if (e.key === 'ArrowLeft') {
      e.preventDefault()
      weekPage(1)
    }
    if (e.key === 'ArrowRight') {
      e.preventDefault()
      weekPage(-1)
    }
  }

  const rangeLabel = $derived.by(() => {
    if (!chart) return ''
    const s = new Date(chart.week_start_ms)
    const e = new Date(chart.week_end_ms - 1)
    const opts: Intl.DateTimeFormatOptions = { month: 'short', day: 'numeric' }
    return `${s.toLocaleDateString(undefined, opts)} – ${e.toLocaleDateString(undefined, opts)}, ${e.getFullYear()}`
  })

  // Week totals shown beside the selector: active time and the share of the
  // weekly (7-day) quota consumed across the displayed week.
  const weekActiveMin = $derived(chart ? chart.days.reduce((s, d) => s + d.active_minutes, 0) : 0)
  const weekWeeklyPct = $derived(chart ? chart.days.reduce((s, d) => s + d.weekly_pct, 0) : 0)

  // Week-view nav: the visible span (oldest-top week start → newest-bottom week
  // end) and whether each scroll direction has anywhere left to go. Reading
  // `weekBottomOffset` keeps these reactive as the window scrolls.
  const weekRangeLabel = $derived.by(() => {
    weekBottomOffset
    const w = weekWindow()
    if (!w) return ''
    const top = w.all[w.offset + w.count - 1] // oldest visible
    const bottom = w.all[w.offset] // newest visible
    const opts: Intl.DateTimeFormatOptions = { month: 'short', day: 'numeric' }
    const s = new Date(top.week_start_ms).toLocaleDateString(undefined, opts)
    const e = new Date(bottom.week_end_ms - 1).toLocaleDateString(undefined, opts)
    return `${s} – ${e}`
  })
  const weekOlderDisabled = $derived.by(() => {
    weekBottomOffset
    const w = weekWindow()
    return !w || w.offset >= w.maxOffset
  })
  const weekNewerDisabled = $derived.by(() => {
    weekBottomOffset
    const w = weekWindow()
    return !w || w.offset <= 0
  })

  // Averages across the weeks currently visible in the window: mean active time
  // and mean weekly-quota use per displayed week.
  const weekAvg = $derived.by(() => {
    weekBottomOffset
    const w = weekWindow()
    if (!w) return null
    const inView = w.all.slice(w.offset, w.offset + w.count)
    if (inView.length === 0) return null
    const active = inView.reduce((s, wk) => s + wk.days.reduce((a, d) => a + d.active_minutes, 0), 0) / inView.length
    const pct = inView.reduce((s, wk) => s + wk.days.reduce((a, d) => a + d.weekly_pct, 0), 0) / inView.length
    return { active, pct }
  })

  // Bars at/above 2× the full pace are clipped and painted this red to flag them.
  const CLIP_RED = '#e0443a'

  // green → gold → amber, keyed on intensity / full-5h-pace (0..2). Red is
  // reserved for clipped bars so "red" unambiguously means "over the 2× cap".
  function barColor(ratio: number): string {
    const stops: [number, [number, number, number]][] = [
      [0, [58, 124, 74]],   // green
      [1, [214, 161, 58]],  // gold at full pace
      [2, [216, 132, 58]],  // deep amber at the 2× cap
    ]
    const r = Math.max(0, Math.min(2, ratio))
    for (let i = 0; i < stops.length - 1; i++) {
      const [pa, ca] = stops[i]
      const [pb, cb] = stops[i + 1]
      if (r >= pa && r <= pb) {
        const t = (r - pa) / (pb - pa)
        const c = ca.map((v, k) => Math.round(v + (cb[k] - v) * t))
        return `rgb(${c[0]}, ${c[1]}, ${c[2]})`
      }
    }
    return 'rgb(216, 132, 58)'
  }

  let hatchPattern: CanvasPattern | null = null
  function ensureHatch(ctx: CanvasRenderingContext2D): CanvasPattern | null {
    if (hatchPattern) return hatchPattern
    const tile = document.createElement('canvas')
    tile.width = 6
    tile.height = 6
    const tc = tile.getContext('2d')
    if (!tc) return null
    tc.strokeStyle = 'rgba(150, 150, 160, 0.14)'
    tc.lineWidth = 1
    tc.beginPath()
    tc.moveTo(0, 6)
    tc.lineTo(6, 0)
    tc.stroke()
    hatchPattern = ctx.createPattern(tile, 'repeat')
    return hatchPattern
  }

  // Single source of the chart geometry — both draw() and the hover hit-test
  // read it, so the two can't drift apart.
  function computeLayout(cssW: number, cssH: number, rows = DAYS) {
    const padTop = 12
    const gutterLeft = 64
    const gutterRight = 168 // room for the larger "Xh Ym active" + "NN% daily"
    const gutterBottom = 28
    const rowGap = 9
    const plotLeft = gutterLeft
    const plotTop = padTop
    const plotRight = cssW - gutterRight
    const plotBottom = cssH - gutterBottom
    const plotW = plotRight - plotLeft
    const plotH = plotBottom - plotTop
    const rowH = (plotH - rowGap * (rows - 1)) / rows
    const colW = plotW / SLOTS_PER_DAY
    return { gutterLeft, rowGap, plotLeft, plotTop, plotRight, plotBottom, plotW, plotH, rowH, colW }
  }

  function fmtActive(min: number): string {
    if (min <= 0) return '—'
    const h = Math.floor(min / 60)
    const m = min % 60
    if (h === 0) return `${m}m`
    if (m === 0) return `${h}h`
    return `${h}h ${m}m`
  }

  // Local hour 0..24 -> 12-hour label, e.g. 0->"12am", 13->"1pm", 24->"12am".
  function fmtHour(h: number): string {
    const hh = h % 24
    const period = hh < 12 ? 'am' : 'pm'
    const n = hh % 12 === 0 ? 12 : hh % 12
    return `${n}${period}`
  }

  // Prep the canvas for a fresh frame (DPR scaling + clear). Returns null until
  // the canvas has a non-zero size.
  function setupCanvas(): { ctx: CanvasRenderingContext2D; cssW: number; cssH: number } | null {
    const canvas = canvasEl
    if (!canvas) return null
    const ctx = canvas.getContext('2d')
    if (!ctx) return null
    const dpr = window.devicePixelRatio || 1
    const cssW = canvas.clientWidth
    const cssH = canvas.clientHeight
    if (cssW === 0 || cssH === 0) return null
    canvas.width = Math.round(cssW * dpr)
    canvas.height = Math.round(cssH * dpr)
    ctx.setTransform(dpr, 0, 0, dpr, 0, 0)
    ctx.clearRect(0, 0, cssW, cssH)
    return { ctx, cssW, cssH }
  }

  // Shared per-row drawing — used by both the day-row and week-row layouts so
  // the bar shape, 2× clip, red flag and reference line can't diverge.

  function drawRowBand(ctx: CanvasRenderingContext2D, cssW: number, rowTop: number, rowH: number) {
    // The band fill plus the dark gap to the next band separates days; no
    // baseline line (it read as an unwanted white rule under each row).
    ctx.fillStyle = 'rgba(255,255,255,0.04)'
    ctx.fillRect(0, rowTop, cssW, rowH)
  }

  function drawBars(ctx: CanvasRenderingContext2D, buckets: WeekChart['buckets'], offset: number, count: number, plotLeft: number, plotW: number, rowTop: number, rowH: number, full: number, scaleMax: number, hatch: CanvasPattern | null) {
    const colW = plotW / count
    const rowBottom = rowTop + rowH
    for (let s = 0; s < count; s++) {
      const b = buckets[offset + s]
      if (!b) continue
      const x = plotLeft + s * colW
      if (!b.has_data) {
        if (hatch) {
          ctx.fillStyle = hatch
          ctx.fillRect(x, rowTop, colW, rowH)
        }
        continue
      }
      if (b.intensity <= 0) continue // idle: baseline only
      const clipped = b.intensity >= scaleMax // over 2× the full pace
      const barH = Math.max(1, Math.min(rowH, (b.intensity / scaleMax) * rowH))
      ctx.fillStyle = clipped ? CLIP_RED : barColor(b.intensity / full)
      ctx.fillRect(x + 0.3, rowBottom - barH, Math.max(0.6, colW - 0.6), barH)
    }
  }

  function drawReference(ctx: CanvasRenderingContext2D, plotLeft: number, plotRight: number, rowTop: number, rowH: number, full: number, scaleMax: number, label: string) {
    const refY = Math.max(rowTop, rowTop + rowH - (full / scaleMax) * rowH)
    ctx.strokeStyle = 'rgba(230, 200, 120, 0.30)'
    ctx.lineWidth = 1
    ctx.setLineDash([2, 4])
    ctx.beginPath()
    ctx.moveTo(plotLeft, Math.round(refY) + 0.5)
    ctx.lineTo(plotRight, Math.round(refY) + 0.5)
    ctx.stroke()
    ctx.setLineDash([])
    if (label) {
      ctx.font = '12px ui-monospace, Consolas, monospace'
      ctx.fillStyle = 'rgba(230, 200, 120, 0.65)'
      ctx.textAlign = 'right'
      ctx.fillText(label, plotRight - 2, refY - 9)
    }
  }

  function gutterStats(ctx: CanvasRenderingContext2D, cssW: number, rowTop: number, rowH: number, line1: string, line2: string) {
    const rx = cssW - 10
    const midY = rowTop + rowH / 2
    ctx.textAlign = 'right'
    ctx.font = '15px ui-monospace, Consolas, monospace'
    ctx.fillStyle = '#d6d6da'
    ctx.fillText(line1, rx, midY - 9)
    ctx.fillStyle = '#9c9ca0'
    ctx.fillText(line2, rx, midY + 9)
  }

  function draw() {
    if (view === 'week') drawWeeksView()
    else drawDayView()
  }

  // Day-rows: one week, seven day-rows, x = time of day.
  function drawDayView() {
    if (!chart) return
    const s = setupCanvas()
    if (!s) return
    const { ctx, cssW, cssH } = s
    const { gutterLeft, rowGap, plotLeft, plotTop, plotRight, plotBottom, plotW, plotH, rowH, colW } =
      computeLayout(cssW, cssH)
    if (plotW <= 0 || plotH <= 0) return
    const full = chart.full_intensity
    const scaleMax = full * 2
    const hatch = ensureHatch(ctx)
    ctx.textBaseline = 'middle'

    // Gridline at every hour, emphasized every 6h. Labels in 12-hour local time
    // every 2h (an every-hour label would overlap at this font size).
    for (let h = 0; h <= 24; h++) {
      const x = plotLeft + h * 6 * colW
      const emphasized = h % 6 === 0
      ctx.strokeStyle = emphasized ? 'rgba(255,255,255,0.12)' : 'rgba(255,255,255,0.05)'
      ctx.lineWidth = 1
      ctx.beginPath()
      ctx.moveTo(Math.round(x) + 0.5, plotTop)
      ctx.lineTo(Math.round(x) + 0.5, plotBottom)
      ctx.stroke()
      if (h % 2 === 0) {
        ctx.font = '13px ui-monospace, Consolas, monospace'
        ctx.fillStyle = emphasized ? '#bcbcc0' : '#86868a'
        ctx.textAlign = h === 0 ? 'left' : h === 24 ? 'right' : 'center'
        ctx.fillText(fmtHour(h), x, plotBottom + 15)
      }
    }

    for (let d = 0; d < DAYS; d++) {
      const rowTop = plotTop + d * (rowH + rowGap)
      drawRowBand(ctx, cssW, rowTop, rowH)

      // Left label, centered in the gutter: weekday over month+day — "Mon" / "Jun 22".
      const dayDate = new Date(chart.week_start_ms + d * DAY_MS)
      const lx = gutterLeft / 2
      const midY = rowTop + rowH / 2
      ctx.textAlign = 'center'
      ctx.font = '13px ui-monospace, Consolas, monospace'
      ctx.fillStyle = '#9a9a9e'
      ctx.fillText(dayDate.toLocaleDateString(undefined, { weekday: 'short' }), lx, midY - 9)
      ctx.fillStyle = '#b6b6ba'
      ctx.fillText(dayDate.toLocaleDateString(undefined, { month: 'short', day: 'numeric' }), lx, midY + 9)

      drawBars(ctx, chart.buckets, d * SLOTS_PER_DAY, SLOTS_PER_DAY, plotLeft, plotW, rowTop, rowH, full, scaleMax, hatch)
      drawReference(ctx, plotLeft, plotRight, rowTop, rowH, full, scaleMax, d === 0 ? 'full 5h pace' : '')

      // Active time + the day's usage as a % of a daily quota (= the day's share
      // of the 7-day quota × 7; may exceed 100%). Hidden for inactive/future days.
      const sum = chart.days?.[d]
      if (sum && sum.active_minutes > 0) {
        gutterStats(ctx, cssW, rowTop, rowH, `${fmtActive(sum.active_minutes)} active`, `${Math.round(sum.weekly_pct * 7)}% daily`)
      }
    }
  }

  // Coarsen a bucket series by averaging each group of `factor` buckets. The
  // mean is taken over the present (non-gap) buckets only, so a gap dilutes
  // nothing; a group with no data at all stays a gap. Averaging (not summing)
  // keeps the per-10-min scale, so scaleMax / the reference line are unchanged.
  function downsample(buckets: WeekChart['buckets'], factor: number): WeekChart['buckets'] {
    const out: WeekChart['buckets'] = []
    for (let i = 0; i < buckets.length; i += factor) {
      let sum = 0
      let n = 0
      for (let j = i; j < i + factor && j < buckets.length; j++) {
        if (buckets[j].has_data) {
          sum += buckets[j].intensity
          n += 1
        }
      }
      out.push({ intensity: n > 0 ? sum / n : 0, has_data: n > 0 })
    }
    return out
  }

  // Week-rows: a scrollable window of weeks, oldest at top → newest at bottom
  // (the same chronological direction as the day-rows). x = Mon→Sun.
  function drawWeeksView() {
    const win = weekWindow()
    if (!win) {
      setupCanvas()
      return
    }
    const s = setupCanvas()
    if (!s) return
    const { ctx, cssW } = s
    const { gutterLeft, rowGap, plotLeft, plotTop, plotRight, plotBottom, plotW, plotH, rowH } =
      computeLayout(cssW, s.cssH, win.count)
    if (plotW <= 0 || plotH <= 0) return
    const full = win.all[0].full_intensity
    const scaleMax = full * 2
    const hatch = ensureHatch(ctx)
    ctx.textBaseline = 'middle'

    // x-axis: weekday boundaries every 1/7 of the row, labels centered per day.
    const dayNames = ['Mon', 'Tue', 'Wed', 'Thu', 'Fri', 'Sat', 'Sun']
    for (let d = 0; d <= 7; d++) {
      const x = plotLeft + (d / 7) * plotW
      ctx.strokeStyle = 'rgba(255,255,255,0.10)'
      ctx.lineWidth = 1
      ctx.beginPath()
      ctx.moveTo(Math.round(x) + 0.5, plotTop)
      ctx.lineTo(Math.round(x) + 0.5, plotBottom)
      ctx.stroke()
      if (d < 7) {
        ctx.font = '13px ui-monospace, Consolas, monospace'
        ctx.fillStyle = '#9a9a9e'
        ctx.textAlign = 'center'
        ctx.fillText(dayNames[d], plotLeft + ((d + 0.5) / 7) * plotW, plotBottom + 15)
      }
    }

    for (let r = 0; r < win.count; r++) {
      // Top row is the oldest in the window, bottom row the newest.
      const week = win.all[win.offset + (win.count - 1 - r)]
      const rowTop = plotTop + r * (rowH + rowGap)
      drawRowBand(ctx, cssW, rowTop, rowH)

      // Left label, centered in the gutter: week start over week end (Sun), with
      // a divider between — e.g. "Jun 22 / · / Jun 28".
      const opts: Intl.DateTimeFormatOptions = { month: 'short', day: 'numeric' }
      const startStr = new Date(week.week_start_ms).toLocaleDateString(undefined, opts)
      const endStr = new Date(week.week_end_ms - 1).toLocaleDateString(undefined, opts)
      const lx = gutterLeft / 2
      const midY = rowTop + rowH / 2
      ctx.textAlign = 'center'
      ctx.font = '12px ui-monospace, Consolas, monospace'
      ctx.fillStyle = '#b6b6ba'
      ctx.fillText(startStr, lx, midY - 14)
      ctx.fillStyle = '#6e6e72'
      ctx.fillText('·', lx, midY)
      ctx.fillStyle = '#b6b6ba'
      ctx.fillText(endStr, lx, midY + 14)

      const slots = downsample(week.buckets, WEEK_GROUP)
      drawBars(ctx, slots, 0, slots.length, plotLeft, plotW, rowTop, rowH, full, scaleMax, hatch)

      // Faint day separators over the dense bars, to keep the week readable.
      for (let d = 1; d < 7; d++) {
        const x = plotLeft + (d / 7) * plotW
        ctx.strokeStyle = 'rgba(255,255,255,0.08)'
        ctx.lineWidth = 1
        ctx.beginPath()
        ctx.moveTo(Math.round(x) + 0.5, rowTop)
        ctx.lineTo(Math.round(x) + 0.5, rowTop + rowH)
        ctx.stroke()
      }

      drawReference(ctx, plotLeft, plotRight, rowTop, rowH, full, scaleMax, r === 0 ? 'full 5h pace' : '')

      const active = week.days.reduce((a, d) => a + d.active_minutes, 0)
      const wk = week.days.reduce((a, d) => a + d.weekly_pct, 0)
      if (active > 0) {
        gutterStats(ctx, cssW, rowTop, rowH, `${fmtActive(active)} active`, `${Math.round(wk)}% week`)
      }
    }
  }

  // Tooltip text for one bar spanning [startMs, startMs+durationMs).
  function bucketTip(startMs: number, durationMs: number, b: WeekChart['buckets'][number]): string {
    const start = new Date(startMs)
    const end = new Date(startMs + durationMs)
    const hhmm = (dt: Date) =>
      `${dt.getHours().toString().padStart(2, '0')}:${dt.getMinutes().toString().padStart(2, '0')}`
    const when = `${start.toLocaleDateString(undefined, { weekday: 'short', month: 'short', day: 'numeric' })} ${hhmm(start)}–${hhmm(end)}`
    const what = !b.has_data ? 'no data' : b.intensity <= 0 ? 'idle' : `${b.intensity.toFixed(1)}% of 5h limit`
    return `${when} · ${what}`
  }

  function onMouseMove(e: MouseEvent) {
    const canvas = canvasEl
    if (!canvas) {
      hover = null
      return
    }
    const rect = canvas.getBoundingClientRect()
    const cssW = canvas.clientWidth
    const cssH = canvas.clientHeight
    const mx = e.clientX - rect.left
    const my = e.clientY - rect.top

    if (view === 'week') {
      const win = weekWindow()
      if (!win) {
        hover = null
        return
      }
      const { rowGap, plotLeft, plotTop, plotRight, plotBottom, plotW, rowH } = computeLayout(cssW, cssH, win.count)
      if (mx < plotLeft || mx > plotRight || my < plotTop || my > plotBottom) {
        hover = null
        return
      }
      const r = Math.floor((my - plotTop) / (rowH + rowGap))
      const withinRow = my - (plotTop + r * (rowH + rowGap))
      if (r < 0 || r >= win.count || withinRow > rowH) {
        hover = null
        return
      }
      const week = win.all[win.offset + (win.count - 1 - r)]
      const grouped = downsample(week.buckets, WEEK_GROUP)
      const s = Math.floor((mx - plotLeft) / (plotW / grouped.length))
      if (s < 0 || s >= grouped.length) {
        hover = null
        return
      }
      const groupMs = WEEK_GROUP * BUCKET_MS
      hover = { x: mx, y: my, text: bucketTip(week.week_start_ms + s * groupMs, groupMs, grouped[s]) }
      return
    }

    if (!chart) {
      hover = null
      return
    }
    const { rowGap, plotLeft, plotTop, plotRight, plotBottom, rowH, colW } = computeLayout(cssW, cssH)
    if (mx < plotLeft || mx > plotRight || my < plotTop || my > plotBottom) {
      hover = null
      return
    }
    const d = Math.floor((my - plotTop) / (rowH + rowGap))
    const withinRow = my - (plotTop + d * (rowH + rowGap))
    if (d < 0 || d >= DAYS || withinRow > rowH) {
      hover = null
      return
    }
    const s = Math.floor((mx - plotLeft) / colW)
    if (s < 0 || s >= SLOTS_PER_DAY) {
      hover = null
      return
    }
    const idx = d * SLOTS_PER_DAY + s
    hover = { x: mx, y: my, text: bucketTip(chart.week_start_ms + idx * BUCKET_MS, BUCKET_MS, chart.buckets[idx]) }
  }

  function onMouseLeave() {
    hover = null
  }

  // Redraw whenever the active view or its data changes.
  $effect(() => {
    view
    chart
    weeks
    weekBottomOffset
    draw()
  })

  onMount(() => {
    let unlistenUsage: UnlistenFn | undefined
    load(0)
    const ro = new ResizeObserver(() => draw())
    if (canvasEl) ro.observe(canvasEl)
    ;(async () => {
      // Live-refresh as new polls land: the current week (day view) and, if it's
      // been loaded, the weeks overview. Older single weeks are frozen.
      unlistenUsage = await onUsageLimitsUpdated(() => {
        if (weekOffset === 0) load(0)
        if (weeks !== null) loadWeeks()
      })
    })()
    return () => {
      ro.disconnect()
      unlistenUsage?.()
    }
  })
</script>

<svelte:window onkeydown={onKeydown} />

<div class="chart">
  <header>
    {#if view === 'day'}
      <div class="selector">
        <button class="nav" onclick={() => step(-1)} disabled={prevDisabled} title="Previous week (←)">←</button>
        <span class="range">{rangeLabel || 'Loading…'}</span>
        <button class="nav" onclick={() => step(1)} disabled={nextDisabled} title="Next week (→)">→</button>
      </div>
      {#if chart}
        <div class="totals">
          {#if weekActiveMin === 0}
            <span>no activity this week</span>
          {:else}
            <span><span class="field-label">Active:</span> <strong>{fmtActive(weekActiveMin)}</strong></span>
            <span><span class="field-label">Weekly quota usage:</span> <strong>{weekWeeklyPct.toFixed(0)}%</strong></span>
          {/if}
        </div>
      {/if}
    {:else}
      <div class="selector">
        <button class="nav" onclick={() => weekPage(1)} disabled={weekOlderDisabled} title="Older screen (←)">←</button>
        <button class="nav" onclick={() => weekStep(1)} disabled={weekOlderDisabled} title="Older week (↑)">↑</button>
        <span class="range">{weekRangeLabel || 'Loading…'}</span>
        <button class="nav" onclick={() => weekStep(-1)} disabled={weekNewerDisabled} title="Newer week (↓)">↓</button>
        <button class="nav" onclick={() => weekPage(-1)} disabled={weekNewerDisabled} title="Newer screen (→)">→</button>
      </div>
      {#if weekAvg}
        <div class="totals">
          <span><span class="field-label">Active avg:</span> <strong>{fmtActive(Math.round(weekAvg.active))}</strong></span>
          <span><span class="field-label">Weekly quota usage avg:</span> <strong>{weekAvg.pct.toFixed(0)}%</strong></span>
        </div>
      {/if}
    {/if}
    <span class="spacer"></span>
    <span class="hint">
      {#if view === 'day'}
        <span class="hint-group"><span class="field-label">Navigation:</span> <span class="hint-rest"><span class="keycap">←</span><span class="keycap">→</span> one week</span></span>
        <span class="hint-group"><span class="field-label">Legend:</span> <span class="hint-rest">red = over 2× pace</span></span>
      {:else}
        <span class="hint-group"><span class="field-label">Navigation:</span> <span class="hint-rest"><span class="keycap">↑</span><span class="keycap">↓</span> one week, <span class="keycap">←</span><span class="keycap">→</span> a screen</span></span>
        <span class="hint-group"><span class="field-label">Legend:</span> <span class="hint-rest">red = over 2× pace</span></span>
      {/if}
    </span>
    <div class="switch">
      <button class:active={view === 'day'} onclick={() => setView('day')}>Days</button>
      <button class:active={view === 'week'} onclick={() => setView('week')}>Weeks</button>
    </div>
  </header>

  {#if error}
    <div class="message">Could not load usage history: {error}</div>
  {:else if chart && chart.data_min_ms === null}
    <div class="message">No usage history recorded yet.</div>
  {:else}
    <div class="canvas-wrap">
      <canvas bind:this={canvasEl} onmousemove={onMouseMove} onmouseleave={onMouseLeave}></canvas>
      {#if hover}
        <div class="tooltip" style="left: {hover.x}px; top: {hover.y}px;">{hover.text}</div>
      {/if}
    </div>
  {/if}
</div>

<style>
  :global(html, body) {
    margin: 0;
    padding: 0;
    height: 100%;
    background: #1c1c1e;
    color: #d6d6d6;
    font-family: system-ui, 'Segoe UI', Roboto, sans-serif;
    overflow: hidden;
  }
  .chart {
    height: 100vh;
    box-sizing: border-box;
    display: flex;
    flex-direction: column;
    padding: 10px 12px 8px;
  }
  header {
    display: flex;
    align-items: center;
    gap: 6px;
    margin-bottom: 6px;
  }
  .selector {
    display: flex;
    align-items: center;
    gap: 3px;
  }
  .nav {
    background: #2c2c2e;
    color: #d6d6d6;
    border: 1px solid #3a3a3c;
    border-radius: 4px;
    width: 28px;
    height: 26px;
    font-size: 17px;
    line-height: 1;
    cursor: pointer;
    padding: 0;
  }
  .nav:hover:not(:disabled) {
    background: #3a3a3c;
  }
  .nav:disabled {
    opacity: 0.35;
    cursor: default;
  }
  .range {
    font-size: 15px;
    color: #e8e8ea;
    min-width: 180px;
    text-align: center;
  }
  .totals {
    display: flex;
    align-items: baseline;
    gap: 22px;
    margin: 0 28px;
    font-size: 15px;
    color: #9c9ca0;
    white-space: nowrap;
  }
  .totals strong {
    font-weight: 600;
    color: #f0f0f2;
  }
  .switch {
    display: flex;
    margin-left: 28px;
    border: 1px solid #3a3a3c;
    border-radius: 5px;
    overflow: hidden;
    flex: none;
  }
  .switch button {
    background: #2c2c2e;
    color: #b0b0b4;
    border: none;
    padding: 4px 12px;
    font-size: 13px;
    cursor: pointer;
  }
  .switch button.active {
    background: #4a4a4e;
    color: #fff;
  }
  .switch button:not(.active):hover {
    background: #3a3a3c;
  }
  .spacer {
    flex: 1;
  }
  /* Groups sit side by side; the hint shrinks with the header (min-width: 0). */
  .hint {
    display: flex;
    align-items: flex-start;
    gap: 28px;
    min-width: 0;
    font-size: 13px;
    color: #8a8a8e;
  }
  /* One line when there's room. When the header is too narrow the group wraps
     after its colon — label over content — the two groups staying side by side
     (so the wrapped state is two rows total, not four). */
  .hint-group {
    min-width: 0;
  }
  /* Shared label style for Active / Weekly quota usage / Navigation / Legend. */
  .field-label {
    font-weight: 600;
    color: #a6a6aa;
  }
  .hint-rest {
    white-space: nowrap;
  }
  /* Mini key-cap matching the .nav buttons (border + fill) at ~half size. */
  .keycap {
    display: inline-flex;
    align-items: center;
    justify-content: center;
    width: 15px;
    height: 14px;
    margin: 0 1px;
    background: #2c2c2e;
    color: #d6d6d6;
    border: 1px solid #3a3a3c;
    border-radius: 3px;
    font-size: 10px;
    line-height: 1;
    vertical-align: middle;
  }
  .canvas-wrap {
    position: relative;
    flex: 1;
    min-height: 0;
  }
  canvas {
    width: 100%;
    height: 100%;
    display: block;
  }
  .message {
    flex: 1;
    display: flex;
    align-items: center;
    justify-content: center;
    color: #8a8a8e;
    font-size: 13px;
  }
  .tooltip {
    position: absolute;
    transform: translate(-50%, -150%);
    pointer-events: none;
    background: #2c2c2e;
    border: 1px solid #48484a;
    border-radius: 4px;
    padding: 4px 9px;
    font-size: 13px;
    color: #e8e8ea;
    white-space: nowrap;
    font-family: ui-monospace, Consolas, monospace;
  }
</style>

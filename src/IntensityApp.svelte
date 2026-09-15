<script lang="ts">
  import { onMount } from 'svelte'
  import { listen, type UnlistenFn } from '@tauri-apps/api/event'
  import { closeWindow, getIntensityWeek, getIntensityWeeks, onUsageLimitsUpdated } from './lib/api'
  import type { TokenWeekChart } from './lib/types'

  const BUCKET_MS = 10 * 60 * 1000
  const SLOTS_PER_DAY = 144 // 6 per hour × 24
  const DAYS = 7
  const DAY_MS = 86400000
  const WEEKS_PER_SCREEN = 7 // a screenful of week-rows (mirrors the 7 day-rows)
  const WEEK_GROUP = 3 // weeks view groups N 10-min buckets into one coarser bar

  // The widest string each header stat can render, reserved so the names beside
  // them never move. These are ceilings rather than observed maxima: a week holds
  // 168 hours, so that is as long as `fmtActiveHours` gets; three digits covers a
  // quota that has been measured past 140% and a token total measured at 155M,
  // `fmtTokens` giving four characters for anything under a billion. A value that
  // outgrows its reservation still renders in full — the cell grows and the row
  // shifts, which is exactly today's behaviour.
  const WIDEST = { active: '168h', tokens: '999M', quota: '999%' }

  // 'day' = one week shown as 7 day-rows; 'week' = one row per week (overview).
  let view = $state<'day' | 'week'>('day')
  let weekOffset = $state(0)
  let unlistenTarget: UnlistenFn | undefined
  // Bars are token counts; each row also carries its share of the 7-day quota,
  // which the backend joins on before sending (see `TokenDaySummary`).
  let chart = $state<TokenWeekChart | null>(null)
  let weeks = $state<TokenWeekChart[] | null>(null)
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
      chart = await getIntensityWeek(offset)
      weekOffset = offset
    } catch (e) {
      error = String(e)
    }
  }

  async function loadWeeks() {
    try {
      weeks = await getIntensityWeeks()
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
  function weekWindow(): { all: TokenWeekChart[]; count: number; offset: number; maxOffset: number } | null {
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

  // Week totals shown beside the selector: active time, the work done, and the
  // share of the weekly (7-day) quota consumed across the displayed week.
  const weekActiveMin = $derived(chart ? chart.days.reduce((s, d) => s + d.active_minutes, 0) : 0)
  const weekWeeklyPct = $derived(chart ? chart.days.reduce((s, d) => s + d.weekly_pct, 0) : 0)
  const weekTokens = $derived(chart ? chart.days.reduce((s, d) => s + d.tokens, 0) : 0)

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
    const activeAvg = inView.reduce((s, wk) => s + wk.days.reduce((a, d) => a + d.active_minutes, 0), 0) / inView.length
    const pct = inView.reduce((s, wk) => s + wk.days.reduce((a, d) => a + d.weekly_pct, 0), 0) / inView.length
    const tokens = inView.reduce((s, wk) => s + wk.days.reduce((a, d) => a + d.tokens, 0), 0) / inView.length
    return { active: activeAvg, pct, tokens }
  })

  // Bars at or above the top of the scale are clipped and painted this red.
  const CLIP_RED = '#e0443a'

  // green → gold → amber, keyed on value / scaleMax (0..1). Red is reserved for
  // clipped bars so "red" unambiguously means "at or over the top of the scale".
  // Expressed against the scale rather than against any quota pace: tokens have
  // nothing to be a fraction of, so the ramp describes the drawn height and
  // nothing more.
  function barColor(ratio: number): string {
    const stops: [number, [number, number, number]][] = [
      [0, [58, 124, 74]],     // green
      [0.5, [214, 161, 58]],  // gold at half scale
      [1, [216, 132, 58]],    // deep amber at the top of the scale
    ]
    const r = Math.max(0, Math.min(1, ratio))
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
    const gutterRight = 168 // room for the longest stat line, "NNNh NNm active"
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

  // Whole hours, for the header summary. A week's total and a week's average are
  // both tens of hours, where ten minutes either way says nothing and the extra
  // characters cost the header width it does not have. "<1h" rather than a
  // rounded-down "0h", which would read as a bug beside a real token count.
  function fmtActiveHours(min: number): string {
    if (min <= 0) return '—'
    const h = Math.round(min / 60)
    return h === 0 ? '<1h' : `${h}h`
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
  // the bar shape, the clip at the top of the scale and its red flag can't
  // diverge.

  function drawRowBand(ctx: CanvasRenderingContext2D, cssW: number, rowTop: number, rowH: number) {
    // The band fill plus the dark gap to the next band separates days; no
    // baseline line (it read as an unwanted white rule under each row).
    ctx.fillStyle = 'rgba(255,255,255,0.04)'
    ctx.fillRect(0, rowTop, cssW, rowH)
  }

  // A bar is just a value and whether we have data for its slot. `Bar[]` is what
  // each view maps a week's buckets down to.
  type Bar = { value: number; has_data: boolean }

  const barsOf = (c: TokenWeekChart): Bar[] => c.buckets.map((b) => ({ value: b.tokens, has_data: b.has_data }))

  // Compact token counts: 1.2M / 1M / 340k / 900. A whole number of millions
  // drops the trailing ".0" — "1M" rather than "1.0M".
  function fmtTokens(n: number): string {
    if (n >= 1_000_000) return `${(n / 1_000_000).toFixed(n >= 10_000_000 ? 0 : 1).replace(/\.0$/, '')}M`
    if (n >= 1_000) return `${Math.round(n / 1_000)}k`
    return `${Math.round(n)}`
  }

  function drawBars(ctx: CanvasRenderingContext2D, bars: Bar[], offset: number, count: number, plotLeft: number, plotW: number, rowTop: number, rowH: number, scaleMax: number, hatch: CanvasPattern | null) {
    const colW = plotW / count
    const rowBottom = rowTop + rowH
    for (let s = 0; s < count; s++) {
      const b = bars[offset + s]
      if (!b) continue
      const x = plotLeft + s * colW
      if (!b.has_data) {
        if (hatch) {
          ctx.fillStyle = hatch
          ctx.fillRect(x, rowTop, colW, rowH)
        }
        continue
      }
      if (b.value <= 0) continue // idle: baseline only
      const clipped = b.value >= scaleMax
      const barH = Math.max(1, Math.min(rowH, (b.value / scaleMax) * rowH))
      ctx.fillStyle = clipped ? CLIP_RED : barColor(b.value / scaleMax)
      ctx.fillRect(x + 0.3, rowBottom - barH, Math.max(0.6, colW - 0.6), barH)
    }
  }

  // Right-gutter stats for one row, centred on it: the first line is the row's
  // headline (active time) and the rest are its totals. Takes a list rather
  // than fixed lines because a row can legitimately have only some of them —
  // a week the usage poller covered but no surviving transcript does reach
  // here, carrying its quota share and nothing else.
  const GUTTER_LINE_H = 18
  function gutterStats(ctx: CanvasRenderingContext2D, cssW: number, rowTop: number, rowH: number, lines: string[]) {
    const rx = cssW - 10
    const midY = rowTop + rowH / 2
    const top = midY - ((lines.length - 1) * GUTTER_LINE_H) / 2
    ctx.textAlign = 'right'
    ctx.font = '15px ui-monospace, Consolas, monospace'
    lines.forEach((line, i) => {
      ctx.fillStyle = i === 0 ? '#d6d6da' : '#9c9ca0'
      ctx.fillText(line, rx, top + i * GUTTER_LINE_H)
    })
  }

  // The numbers beside one row, in the order they are read: how long the agents
  // were busy, how much work that produced, and what it cost of the weekly
  // quota. Each is dropped when it has nothing to say, so a row shows what is
  // known about it and never a column of zeroes.
  function rowStats(activeMinutes: number, tokens: number, pct: number, pctLabel: string): string[] {
    const lines: string[] = []
    if (activeMinutes > 0) lines.push(`${fmtActive(activeMinutes)} active`)
    if (tokens > 0) lines.push(`${fmtTokens(tokens)} tokens`)
    if (pct > 0) lines.push(`${Math.round(pct)}% ${pctLabel}`)
    return lines
  }

  function draw() {
    if (view === 'week') drawWeeksView()
    else drawDayView()
  }

  // Day-rows: one week, seven day-rows, x = time of day.
  function drawDayView() {
    const week = chart
    if (!week) return
    const s = setupCanvas()
    if (!s) return
    const { ctx, cssW, cssH } = s
    const { gutterLeft, rowGap, plotLeft, plotTop, plotBottom, plotW, plotH, rowH, colW } =
      computeLayout(cssW, cssH)
    if (plotW <= 0 || plotH <= 0) return
    const scaleMax = week.axis_max_tokens
    const bars = barsOf(week)
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
      const dayDate = new Date(week.week_start_ms + d * DAY_MS)
      const lx = gutterLeft / 2
      const midY = rowTop + rowH / 2
      ctx.textAlign = 'center'
      ctx.font = '13px ui-monospace, Consolas, monospace'
      ctx.fillStyle = '#9a9a9e'
      ctx.fillText(dayDate.toLocaleDateString(undefined, { weekday: 'short' }), lx, midY - 9)
      ctx.fillStyle = '#b6b6ba'
      ctx.fillText(dayDate.toLocaleDateString(undefined, { month: 'short', day: 'numeric' }), lx, midY + 9)

      drawBars(ctx, bars, d * SLOTS_PER_DAY, SLOTS_PER_DAY, plotLeft, plotW, rowTop, rowH, scaleMax, hatch)

      // The day's share of a *daily* quota is its share of the 7-day one × 7,
      // so a day that spent a seventh of the week reads 100%, and a heavy day
      // reads past it. An inactive or future day shows nothing at all.
      const sum = week.days?.[d]
      if (sum) {
        const lines = rowStats(sum.active_minutes, sum.tokens, sum.weekly_pct * 7, 'daily')
        if (lines.length > 0) gutterStats(ctx, cssW, rowTop, rowH, lines)
      }
    }
  }

  // Coarsen a bar series by combining each group of `factor` bars, over the
  // present (non-gap) bars only — so a gap dilutes nothing and a group with no
  // data at all stays a gap.
  //
  // The group is summed, which makes a week bar read as the volume of the whole
  // group; its scaleMax is multiplied by the same factor, so a full-height bar
  // means the same rate in both views.
  function downsample(bars: Bar[], factor: number): Bar[] {
    const out: Bar[] = []
    for (let i = 0; i < bars.length; i += factor) {
      let sum = 0
      let n = 0
      for (let j = i; j < i + factor && j < bars.length; j++) {
        if (bars[j].has_data) {
          sum += bars[j].value
          n += 1
        }
      }
      out.push({ value: n > 0 ? sum : 0, has_data: n > 0 })
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
    const { gutterLeft, rowGap, plotLeft, plotTop, plotBottom, plotW, plotH, rowH } =
      computeLayout(cssW, s.cssH, win.count)
    if (plotW <= 0 || plotH <= 0) return
    // Summed groups need a proportionally taller scale, so a full-height week bar
    // means the same rate as a full-height day bar.
    const scaleMax = win.all[0].axis_max_tokens * WEEK_GROUP
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

      const slots = downsample(barsOf(week), WEEK_GROUP)
      drawBars(ctx, slots, 0, slots.length, plotLeft, plotW, rowTop, rowH, scaleMax, hatch)

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

      const lines = rowStats(
        week.days.reduce((a, d) => a + d.active_minutes, 0),
        week.days.reduce((a, d) => a + d.tokens, 0),
        week.days.reduce((a, d) => a + d.weekly_pct, 0),
        'week',
      )
      if (lines.length > 0) gutterStats(ctx, cssW, rowTop, rowH, lines)
    }
  }

  // Tooltip text for one bar spanning [startMs, startMs+durationMs).
  function bucketTip(startMs: number, durationMs: number, b: Bar): string {
    const start = new Date(startMs)
    const end = new Date(startMs + durationMs)
    const hhmm = (dt: Date) =>
      `${dt.getHours().toString().padStart(2, '0')}:${dt.getMinutes().toString().padStart(2, '0')}`
    const when = `${start.toLocaleDateString(undefined, { weekday: 'short', month: 'short', day: 'numeric' })} ${hhmm(start)}–${hhmm(end)}`
    // "no data" is deliberately distinct from "idle": outside the range we hold
    // records for, we don't know that nothing happened.
    const what = !b.has_data ? 'no data' : b.value <= 0 ? 'idle' : `${fmtTokens(b.value)} tokens`
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
      const grouped = downsample(barsOf(week), WEEK_GROUP)
      const s = Math.floor((mx - plotLeft) / (plotW / grouped.length))
      if (s < 0 || s >= grouped.length) {
        hover = null
        return
      }
      const groupMs = WEEK_GROUP * BUCKET_MS
      hover = { x: mx, y: my, text: bucketTip(week.week_start_ms + s * groupMs, groupMs, grouped[s]) }
      return
    }

    const week = chart
    if (!week) {
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
    hover = { x: mx, y: my, text: bucketTip(week.week_start_ms + idx * BUCKET_MS, BUCKET_MS, barsOf(week)[idx]) }
  }

  function onMouseLeave() {
    hover = null
  }

  // Redraw whenever the active view or its data changes.
  //
  // The tooltip is dropped on the same signal, and it has to be dropped *here*
  // rather than in the four callers that swap the chart: `hover` names one
  // bucket of the chart currently drawn, so every one of those swaps invalidates
  // it, and a rule spread over four call sites is one a fifth caller misses.
  // Nothing else would take it down either — `onMouseLeave` is the only other
  // clear, and a pointer that never moves fires no mouse event at all, so a
  // tooltip raised over last week's chart stays painted over this week's. It
  // reached a committed documentation screenshot that way: the frame for the
  // week of Aug 31 carried a tooltip reading "Tue, Sep 1 07:10-07:20", a bucket
  // from the week that had been on screen before.
  $effect(() => {
    view
    chart
    weeks
    weekBottomOffset
    hover = null
    draw()
  })

  onMount(() => {
    let unlistenUsage: UnlistenFn | undefined
    ;(async () => {
      load(0)

      // A caller can open the chart on a particular week and view — the same
      // deep link `history_target` gives the history window. Without it the
      // only way to reach any week but this one is a keypress.
      // Through `setView`, not by assigning `view`: the weeks array is fetched
      // lazily on the first switch to that view, so a deep link that set the
      // field directly left the window on "Loading…" for as long as it stayed
      // open — nothing else fetches it (the live-refresh listener below only
      // reloads an array it already holds).
      unlistenTarget = await listen<{ offset: number; view?: string | null }>('intensity_target', (evt) => {
        const v = evt.payload?.view
        if (v === 'day' || v === 'week') setView(v)
        load(Math.min(0, evt.payload?.offset ?? 0))
      })
    })()
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
      unlistenTarget?.()
      ro.disconnect()
      unlistenUsage?.()
    }
  })
</script>

<svelte:window onkeydown={onKeydown} />

<!-- One stat: its name, then its value in a cell held open by a hidden copy of
     the widest string that value can render as. The reservation is what keeps
     the next name from sliding — both when the view switches (a week's total and
     a week's average are different lengths) and when the data moves under a
     view that is already open. -->
{#snippet stat(name: string, value: string, widest: string)}
  <span class="stat">
    <span class="stat-name">{name}</span>
    <span class="measured">
      <span class="ghost">{widest}</span>
      <span class="row"><strong>{value}</strong></span>
    </span>
  </span>
{/snippet}

<!-- Both scopes are ghosted rather than the longer one, so nothing here has to
     know which that is — the cell is the width of whichever wins in the font
     the platform actually resolved. -->
{#snippet scopeLabel(text: string)}
  <span class="scope measured">
    <span class="ghost">This week:</span>
    <span class="ghost">Week average:</span>
    <span>{text}</span>
  </span>
{/snippet}

<div class="chart">
  <header>
    {#if view === 'day'}
      <div class="selector">
        <button class="nav" onclick={() => step(-1)} disabled={prevDisabled} title="Previous week (←)">←</button>
        <span class="nav-slot"></span>
        <span class="range">{rangeLabel || 'Loading…'}</span>
        <span class="nav-slot"></span>
        <button class="nav" onclick={() => step(1)} disabled={nextDisabled} title="Next week (→)">→</button>
      </div>
      {#if chart}
        <!-- The three share one scope, so it is stated once at the head of the
             group and their own names carry nothing but what they measure. -->
        <div class="totals">
          {@render scopeLabel('This week:')}
          {#if weekActiveMin === 0 && weekWeeklyPct === 0}
            <span>no activity</span>
          {:else}
            {@render stat('Active', fmtActiveHours(weekActiveMin), WIDEST.active)}
            {@render stat('Tokens', fmtTokens(weekTokens), WIDEST.tokens)}
            {@render stat('Quota', `${weekWeeklyPct.toFixed(0)}%`, WIDEST.quota)}
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
          {@render scopeLabel('Week average:')}
          {@render stat('Active', fmtActiveHours(weekAvg.active), WIDEST.active)}
          {@render stat('Tokens', fmtTokens(weekAvg.tokens), WIDEST.tokens)}
          {@render stat('Quota', `${weekAvg.pct.toFixed(0)}%`, WIDEST.quota)}
        </div>
      {/if}
    {/if}
    <span class="spacer"></span>
    <div class="switch">
      <button class:active={view === 'day'} onclick={() => setView('day')}>Days</button>
      <button class:active={view === 'week'} onclick={() => setView('week')}>Weeks</button>
    </div>
  </header>

  {#if error}
    <div class="message">Could not load usage history: {error}</div>
  {:else if chart && chart.data_min_ms === null}
    <div class="message">No work recorded yet.</div>
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
  /* Not selectable: everything below the header is a canvas, so a drag that
     starts on a label has nothing to end on and leaves the totals and the
     legend highlighted until something else clears them — including in a
     documentation screenshot, which is where this was noticed. */
  header {
    display: flex;
    align-items: center;
    gap: 6px;
    margin-bottom: 6px;
    user-select: none;
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
  /* Holds a button's place in the Days view, where there is no screen-at-a-time
     paging to put there, so the ← and → either side of the date sit at the same
     x in both views and switching does not slide them. An empty span rather
     than a hidden button: there is nothing here to reach by Tab or by click. */
  .nav-slot {
    width: 28px;
  }
  .range {
    font-size: 15px;
    color: #e8e8ea;
    min-width: 180px;
    text-align: center;
  }
  /* Tabular figures throughout, which is what makes the reservations below
     exact rather than lucky: Segoe UI's proportional digits run 6px for a "1"
     against 9px for a "4", so a hidden "168h" would be narrower than a real
     "144h" and the stat beside it would move on a week the user paged to. */
  .totals {
    display: flex;
    align-items: baseline;
    margin: 0 28px;
    font-variant-numeric: tabular-nums;
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
  /* Three levels, brightest first: what the numbers are *of*, then the numbers,
     then what each one measures. The scope is the only part that changes with
     the view, so it reads as the heading of the group rather than as a fourth
     stat, and the stat names can drop back to plain weight now that they no
     longer each have to repeat "weekly" or "avg". */
  /* Right-aligned inside its reservation, so the colon sits against the stats
     it introduces rather than a hole the shorter of the two labels leaves. The
     cell is still the width of the longer one, so `Active` does not move. */
  .scope {
    font-weight: 600;
    color: #e8e8ea;
    justify-items: end;
    margin-right: 14px;
  }
  .stat {
    display: flex;
    align-items: baseline;
    gap: 7px;
  }
  .stat-name {
    color: #8a8a8e;
  }
  /* Stack every child in one grid cell, so the cell is as wide as the widest of
     them and the visible one is laid over the hidden reservations. A min-width
     in px or ch was the alternative and is worse on both counts: it is a number
     nothing checks against the string it is meant to hold, and it is wrong on
     the other platform, whose header resolves a different font. */
  .measured {
    display: inline-grid;
  }
  .measured > * {
    grid-area: 1 / 1;
  }
  /* The padding is the separator's room: it is what the reservation holds open
     beyond the widest value, so even a value that fills its cell has somewhere
     for the dot to sit. */
  .ghost {
    visibility: hidden;
    padding-right: 26px;
  }
  /* The dot floats in whatever the value left over, equal space either side —
     so it reads as sitting between this number and the next name rather than
     hard against one of them, and it moves as the numbers do. The stats
     themselves do not move: this is all inside one reserved cell, and the gap
     between cells is zero because the dot's own margins are the gap. */
  .row {
    display: flex;
    align-items: baseline;
  }
  .stat:not(:last-child) .row::after {
    content: '·';
    color: #6e6e72;
    margin: 0 auto;
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

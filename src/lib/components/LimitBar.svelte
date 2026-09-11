<script lang="ts">
  import type { LimitBucket, UsageColors, UsageStatus } from '../types'
  import { formatCompactRemaining, usageColor } from '../types'
  import { refreshUsageLimits } from '../api'

  interface Props {
    bucket: LimitBucket | null
    status: UsageStatus
    updated: number
    now: number
    format: 'hm' | 'dhm'
    segments: number
    colors: UsageColors
    compact?: boolean
  }

  let { bucket, status, updated, now, format, segments, colors, compact = false }: Props = $props()

  // When the window's resets_at has passed but we still hold the old snapshot
  // (the poller fires once per 10 min by default), the displayed remaining
  // goes negative and formatCompactRemaining renders "--:--". Ask the backend
  // for a fresh poll — its 60s floor protects against spam, and a single
  // refresh per stale snapshot brings the new window's resets_at in.
  let lastRefreshForStale = 0
  $effect(() => {
    if (!bucket || bucket.resets_at === null) return
    if (bucket.resets_at - now > 0) return
    if (updated === lastRefreshForStale) return
    lastRefreshForStale = updated
    refreshUsageLimits().catch(() => {})
  })

  const segmentCount = $derived(Math.max(1, Math.floor(segments)))
  const hasData = $derived(bucket !== null)
  const utilization = $derived(hasData && bucket ? bucket.utilization : 0)
  const filledSegments = $derived(
    utilization > 0
      ? Math.max(1, Math.min(segmentCount, Math.round(utilization * segmentCount)))
      : 0,
  )
  // A held bucket shows its figure whatever the status says. The two strings
  // this replaced -- 'NO DATA' and compact's '??' -- could only ever appear
  // WITH a bucket in hand, since a missing one is already '--%' above: the
  // segments were drawn, the reset countdown was ticking, and the number beside
  // them claimed there was nothing. Restarting while the usage endpoint is
  // rate-limiting is the case that made it plain -- the last real reading is
  // replayed from disk precisely so the header keeps saying something, and it
  // arrived to be labelled as absent.
  const stale = $derived(hasData && status !== 'ok')
  const percentText = $derived(
    !hasData || !bucket ? '--%' : `${Math.round(bucket.utilization * 100)}%`,
  )
  const timeText = $derived(
    !hasData || !bucket
      ? formatCompactRemaining(null, format)
      : bucket.resets_at === null
        ? status === 'ok'
          ? 'IDLE'
          : formatCompactRemaining(null, format)
        : formatCompactRemaining(bucket.resets_at - now, format),
  )
  const fillColor = $derived(usageColor(utilization * 100, colors))
  // Usage level (0–100) — the fill point for the compact progress border: the
  // box outline is severity-colored from the left up to this percent and gray
  // beyond, so the number stays neutral. The compact analog of the full-view
  // horizontal bar.
  const levelPct = $derived(Math.min(100, Math.max(0, Math.round(utilization * 100))))
  const longLabel = $derived(format === 'hm' ? '5h limit' : '7d limit')
  const tooltip = $derived(buildTooltip(status, bucket, updated, now, longLabel))

  function buildTooltip(
    s: UsageStatus,
    b: LimitBucket | null,
    u: number,
    n: number,
    label: string,
  ): string {
    const resets = b && b.resets_at !== null ? `Resets ${formatResetTime(b.resets_at)}` : null
    const lines: string[] = [label]
    if (s === 'unavailable') lines.push('Sign in via Claude Code to enable')
    else if (s === 'auth_expired') lines.push('Token expired — run Claude Code to refresh')
    else if (s === 'network_error') {
      if (resets) lines.push(resets)
      // Two different facts, and only one of them is `updated`. With a bucket in
      // hand `updated` is the moment of the READING on show — the backend keeps
      // the previous snapshot's timestamp when it holds its buckets, and replays
      // a stored sample under its own — so the age belongs to the number above
      // and "last try" would date it by something else entirely. With no bucket
      // there is no reading, and `updated` is the failed attempt.
      //
      // It deliberately does not say how long the API has been unreachable.
      // A replayed sample can be from before a weekend the app spent closed,
      // during which nothing was tried and nothing was unreachable; and the
      // commonest case here is not unreachability at all but a 429 whose
      // Retry-After we are honouring, where the endpoint answered and we chose
      // not to call. "Could not refresh" covers both without claiming either.
      lines.push(
        b ? `Reading from ${formatAgo(n - u)} — could not refresh`
          : `Anthropic API unreachable — last try ${formatAgo(n - u)}`,
      )
    } else if (s === 'ok' && b) {
      lines.push(resets ?? 'No usage yet', `updated ${formatAgo(n - u)}`)
    }
    return lines.join('\n')
  }

  function formatAgo(ms: number): string {
    if (ms < 0) return 'just now'
    const s = Math.floor(ms / 1000)
    if (s < 60) return `${s}s ago`
    const m = Math.floor(s / 60)
    if (m < 60) return `${m}m ago`
    const h = Math.floor(m / 60)
    return `${h}h ago`
  }

  function formatResetTime(ms: number): string {
    return new Date(ms).toLocaleString(undefined, {
      month: 'short',
      day: 'numeric',
      hour: '2-digit',
      minute: '2-digit',
      hour12: false,
    })
  }
</script>

<div class="bar" class:compact style:--sev={fillColor} style:--lvl={levelPct + '%'} title={tooltip} data-tauri-drag-region>
  <span class="cap cap-left" class:stale data-tauri-drag-region>{percentText}</span>
  {#if !compact}
    <div
      class="segments"
      style:--n={segmentCount}
      style:--fill-color={fillColor}
      data-tauri-drag-region
    >
      {#if filledSegments > 0}
        <div
          class="fill"
          style:--filled={filledSegments}
          data-tauri-drag-region
        ></div>
      {/if}
    </div>
  {/if}
  <span class="cap cap-right" data-tauri-drag-region>{timeText}</span>
</div>

<style>
  .bar {
    display: flex;
    align-items: center;
    height: 16px;
    min-width: 0;
    font-family: ui-monospace, Consolas, monospace;
    font-variant-numeric: tabular-nums;
    font-size: 10px;
    line-height: 1;
    border: 1px solid rgba(255, 255, 255, 0.18);
    border-radius: 3px;
    overflow: hidden;
    /* Horizontal padding inside each number cap; the caps' min-widths derive
       from it, so tightening it in compact reclaims width for larger text. */
    --cap-pad: 5px;
  }
  /* Compact view swaps the segmented track for a progress border: the box
     outline fills from the left in the severity color up to the usage level, the
     rest in the track gray — a horizontal gauge on the frame that keeps the
     number neutral. Two background layers (fill on padding-box, gradient on
     border-box) so the rounded corners survive, which border-image would square
     off. */
  .bar.compact {
    --track: rgba(255, 255, 255, 0.18);
    border-color: transparent;
    border-width: 2px;
    background:
      linear-gradient(#2a2a2d, #2a2a2d) padding-box,
      linear-gradient(90deg, var(--sev) var(--lvl), var(--track) var(--lvl)) border-box;
  }
  /* A figure nobody has been able to re-check. Dimmed rather than annotated:
     the exact age is in the tooltip, and a second glyph beside the number would
     be the duplicate state signal the row badges are kept free of. */
  .cap.stale {
    opacity: 0.55;
  }
  .segments {
    position: relative;
    flex: 1;
    min-width: 0;
    height: 16px;
    background-color: #17171a;
    background-image: linear-gradient(
      to right,
      #45454a 0,
      #45454a calc(100% - 1px),
      transparent calc(100% - 1px)
    );
    background-size: calc((100% + 1px) / var(--n)) 100%;
    background-repeat: repeat-x;
    overflow: hidden;
  }
  .fill {
    position: absolute;
    top: 0;
    bottom: 0;
    left: 0;
    width: calc(var(--filled) * (100% + 1px) / var(--n) - 1px);
    background-image: linear-gradient(
      to right,
      var(--fill-color) 0,
      var(--fill-color) calc(100% - 1px),
      transparent calc(100% - 1px)
    );
    background-size: calc((100% + 1px) / var(--filled)) 100%;
    background-repeat: repeat-x;
    transition: width 180ms ease;
  }
  .cap {
    display: inline-flex;
    align-items: center;
    justify-content: center;
    height: 16px;
    padding: 0 var(--cap-pad);
    background: #2a2a2d;
    color: #b9b9bc;
    font-weight: 600;
    white-space: nowrap;
    pointer-events: none;
  }
  .cap-left {
    border-right: 1px solid rgba(255, 255, 255, 0.12);
    min-width: calc(4ch + var(--cap-pad) * 2);
  }
  /* Drop cap-left's right border so it doesn't double up with cap-right's left
     border — the single divider between the two values. */
  .bar.compact .cap-left {
    border-right: none;
  }
  .cap-right {
    border-left: 1px solid rgba(255, 255, 255, 0.12);
    min-width: calc(7ch + var(--cap-pad) * 2);
  }
  /* Thicken that divider to 2px to match the progress border, in the same track
     gray so the frame reads as one system. */
  .bar.compact .cap-right {
    border-left: 2px solid var(--track);
  }
</style>

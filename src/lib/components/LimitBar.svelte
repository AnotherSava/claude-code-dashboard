<script lang="ts">
  import type { LimitBucket, UsageStatus } from '../types'
  import { formatCompactRemaining } from '../types'
  import { refreshUsageLimits } from '../api'

  interface Props {
    bucket: LimitBucket | null
    status: UsageStatus
    updated: number
    now: number
    format: 'hm' | 'dhm'
    segments: number
  }

  let { bucket, status, updated, now, format, segments }: Props = $props()

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
  const percentText = $derived(
    !hasData || !bucket
      ? '--%'
      : status === 'ok'
        ? `${Math.round(bucket.utilization * 100)}%`
        : 'NO DATA',
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
  const fillColor = $derived(
    utilization >= 0.85 ? '#b91c1c' : utilization >= 0.5 ? '#b45309' : '#047857',
  )
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
      lines.push(`Anthropic API unreachable — last try ${formatAgo(n - u)}`)
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

<div class="bar" title={tooltip} data-tauri-drag-region>
  <span class="cap cap-left" data-tauri-drag-region>{percentText}</span>
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
    padding: 0 5px;
    background: #2a2a2d;
    color: #b9b9bc;
    font-weight: 600;
    white-space: nowrap;
    pointer-events: none;
  }
  .cap-left {
    border-right: 1px solid rgba(255, 255, 255, 0.12);
    min-width: calc(4ch + 10px);
  }
  .cap-right {
    border-left: 1px solid rgba(255, 255, 255, 0.12);
    min-width: calc(7ch + 10px);
  }
</style>

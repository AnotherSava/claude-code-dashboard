<script lang="ts">
  import type { AgentSession, Config } from '../types'
  import SessionItem from './SessionItem.svelte'

  interface Props {
    sessions: AgentSession[]
    config: Config
    now: number
  }

  let { sessions, config, now }: Props = $props()

  // A CLEAN row has nothing in it to come back to, so after a while it stops
  // earning its place on the widget. The delay is not a rate limit: it is there
  // so a row you have just cleared does not disappear while you are still
  // looking at it.
  //
  // The filter lives here rather than in Rust for two reasons. It needs a
  // ticking clock to make a row leave on time, and `now` already ticks here —
  // a backend filter would only drop the row at the next emit, which for an
  // idle machine may be never. And the backend's display snapshot is also what
  // writes the terminal titles, so hiding a row there would blank the tab of
  // every clean session.
  //
  // Local rows only. For a synced row `state_entered_at` is the sender's clock,
  // and that machine's own widget is where the decision belongs.
  let visible = $derived.by(() => {
    const window = config.clean_hide_after_ms
    if (!window || window <= 0) return sessions
    return sessions.filter((s) => !(s.origin == null && s.status === 'idle' && now - s.state_entered_at >= window))
  })
  let hidden = $derived(sessions.length - visible.length)
</script>

{#if visible.length === 0}
  <!-- Say what the filter hid, never that nothing happened: with every row
       clean, "No active agents" would claim the opposite of the truth. -->
  <div class="empty">{hidden > 0 ? `${hidden} clean ${hidden === 1 ? 'session' : 'sessions'}, nothing waiting` : 'No active agents'}</div>
{:else}
  <div class="list">
    <!-- Inner wrapper shrink-wraps the rows so its measured height is the true
         content height regardless of how tall the scroll viewport (.list) is
         stretched by flex. App.svelte's auto-resize measures this element; a
         single rect read is race-free where summing .list children was not. -->
    <div class="list-inner">
      {#each visible as session (session.id)}
        <SessionItem {session} {config} {now} />
      {/each}
    </div>
  </div>
{/if}

<style>
  .list {
    overflow-y: auto;
    /* Never scroll horizontally — rows truncate with an ellipsis and surface
       full text via the tooltip/history window, so a horizontal scrollbar is
       never wanted. Leaving overflow-x at its default let a narrow window
       trigger a self-feeding cascade: at min-content width the vertical
       scrollbar steals ~15px, the rows overflow sideways → horizontal
       scrollbar → it steals ~15px of height → the vertical bar re-triggers and
       locks in. Auto-resize measures .list-inner (which excludes the
       horizontal scrollbar's height), so the window stays permanently ~15px
       too short with both bars showing. Clipping the x-axis breaks the loop. */
    overflow-x: hidden;
    flex: 1;
    min-height: 0;
  }
  .empty {
    flex: 1;
    display: flex;
    align-items: center;
    justify-content: center;
    font-size: 12px;
    color: #6b7280;
  }
</style>

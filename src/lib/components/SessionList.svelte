<script lang="ts">
  import type { AgentSession, Config } from '../types'
  import SessionItem from './SessionItem.svelte'

  interface Props {
    sessions: AgentSession[]
    config: Config
    now: number
  }

  let { sessions, config, now }: Props = $props()
</script>

{#if sessions.length === 0}
  <div class="empty">No active agents</div>
{:else}
  <div class="list">
    <!-- Inner wrapper shrink-wraps the rows so its measured height is the true
         content height regardless of how tall the scroll viewport (.list) is
         stretched by flex. App.svelte's auto-resize measures this element; a
         single rect read is race-free where summing .list children was not. -->
    <div class="list-inner">
      {#each sessions as session (session.id)}
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

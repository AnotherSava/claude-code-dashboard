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

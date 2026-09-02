<script lang="ts">
  import { approveStart, dismissStart, type PendingStart } from '../api'

  interface Props {
    pending: PendingStart[]
  }

  let { pending }: Props = $props()

  // Which directory each request is currently pointing at. Seeded from the
  // first candidate the peer offered, which is its most likely answer: the
  // backend sorts trusted directories first.
  let chosen = $state<Record<string, string>>({})
  let failed = $state<Record<string, string>>({})
  // The grant hop can take up to its own 20s timeout, during which the row is
  // still on screen. Without this both buttons stay live, and a "Not now" fired
  // while waiting resolves the entry with a dismissal — recording a permission
  // the user then declined, while the approval that is already in flight lands
  // on the peer regardless.
  let busy = $state<Record<string, boolean>>({})

  function dirFor(req: PendingStart): string {
    return chosen[req.id] ?? req.candidates[0]?.dir ?? ''
  }

  function cycle(req: PendingStart) {
    if (req.candidates.length < 2) return
    const i = req.candidates.findIndex((c) => c.dir === dirFor(req))
    chosen[req.id] = req.candidates[(i + 1) % req.candidates.length].dir
  }

  function approve(req: PendingStart) {
    const dir = dirFor(req)
    if (!dir || busy[req.id]) return
    busy[req.id] = true
    failed[req.id] = ''
    // The peer re-checks the directory against its own disk and trust state, so
    // a refusal there is the authority and has to be shown rather than
    // swallowed — the row stays until it actually worked.
    approveStart(req.id, dir)
      .catch((err) => {
        failed[req.id] = String(err)
      })
      .finally(() => {
        busy[req.id] = false
      })
  }

  function candidateFor(req: PendingStart) {
    return req.candidates.find((c) => c.dir === dirFor(req))
  }
</script>

{#if pending.length > 0}
  <div class="approvals">
    {#each pending as req (req.id)}
      <div class="approval">
        <div class="line">
          <span class="who">{req.from_agent}</span>
          <span class="verb">wants to start</span>
          <span class="what">{req.device}/{req.project}</span>
        </div>

        {#if req.candidates.length > 0}
          <button
            class="dir"
            class:multi={req.candidates.length > 1}
            onclick={() => cycle(req)}
            title={req.candidates.length > 1
              ? `${req.candidates.length} folders derive this name — click to switch`
              : dirFor(req)}
          >
            {dirFor(req)}
            {#if req.candidates.length > 1}<span class="more">+{req.candidates.length - 1}</span>{/if}
          </button>
          {#if candidateFor(req) && !candidateFor(req)?.trusted}
            <div class="warn">Claude Code has not been trusted here — open it there once first</div>
          {/if}
        {/if}

        {#if !req.still_waiting}
          <div class="note">The agent stopped waiting. Approving still allows it next time.</div>
        {/if}
        {#if failed[req.id]}
          <div class="warn">{failed[req.id]}</div>
        {/if}

        <div class="actions">
          <button class="approve" onclick={() => approve(req)} disabled={!dirFor(req) || busy[req.id]}>
            {busy[req.id] ? 'Allowing…' : 'Allow'}
          </button>
          <button class="dismiss" onclick={() => dismissStart(req.id).catch(() => {})} disabled={busy[req.id]}>
            Not now
          </button>
        </div>
      </div>
    {/each}
  </div>
{/if}

<style>
  /* `flex: 0 0 auto` because .widget is a flex column whose .list claims
     flex: 1 — without it this block would be shrunk to make room rather than
     pushing the window taller. App.svelte's auto-resize measures .approvals
     explicitly; the class name is part of that contract. */
  .approvals {
    flex: 0 0 auto;
    border-bottom: 1px solid #2a2a2d;
  }

  .approval {
    padding: 6px 10px 8px;
    background: #241f16;
    border-bottom: 1px solid #3a2f1c;
  }

  .approval:last-child {
    border-bottom: none;
  }

  /* One label made of three kinds of information, so each part carries its own
     weight and colour rather than reading as one uniform run.
     The single line is clipped rather than wrapped: the backend caps and
     sanitises the agent name, but this block's height feeds the window size, so
     a long value must not be able to grow the widget even if that cap moves. */
  .line {
    font-size: 11px;
    line-height: 1.4;
    overflow: hidden;
    text-overflow: ellipsis;
    white-space: nowrap;
  }

  .who {
    color: #e8e8ea;
    font-weight: 600;
  }

  .verb {
    color: #a1a1aa;
  }

  .what {
    color: #d9b44a;
    font-weight: 600;
  }

  .dir {
    display: block;
    width: 100%;
    margin-top: 4px;
    padding: 2px 0;
    font-family: inherit;
    font-size: 10px;
    color: #a1a1aa;
    text-align: left;
    background: none;
    border: none;
    cursor: default;
    overflow: hidden;
    text-overflow: ellipsis;
    white-space: nowrap;
  }

  /* Only clickable when there is actually a choice to make — a single
     candidate is a label, and shaping it like a control would promise an
     interaction that does nothing. */
  .dir.multi {
    cursor: pointer;
    color: #c7c7cc;
  }

  .dir.multi:hover {
    color: #e8e8ea;
  }

  .more {
    margin-left: 4px;
    font-size: 9px;
    color: #6b7280;
  }

  .warn,
  .note {
    margin-top: 3px;
    font-size: 10px;
    line-height: 1.35;
    color: #d9b44a;
  }

  .note {
    color: #8a8a90;
  }

  .actions {
    display: flex;
    gap: 6px;
    margin-top: 6px;
  }

  .actions button {
    padding: 2px 10px;
    font-family: inherit;
    font-size: 10px;
    border: none;
    border-radius: 9px;
    cursor: pointer;
  }

  .approve {
    color: #0b1a10;
    background: #86efac;
  }

  .approve:disabled {
    color: #6b7280;
    background: #2f2f33;
    cursor: default;
  }

  .dismiss {
    color: #a1a1aa;
    background: #2f2f33;
  }

  .dismiss:disabled {
    color: #6b7280;
    cursor: default;
  }

  .dismiss:hover {
    color: #e8e8ea;
  }
</style>

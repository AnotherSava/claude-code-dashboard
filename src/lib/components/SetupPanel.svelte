<script lang="ts">
  import { onMount } from 'svelte'
  import { openHookScriptLocation, openSetupDocs, setWindowSize } from '../api'

  interface Props {
    snippet: string
    hookPath: string
    onDismiss?: () => void
  }
  let { snippet, hookPath, onDismiss }: Props = $props()

  let copied = $state(false)
  let copyTimer: ReturnType<typeof setTimeout> | null = null
  let referenceEl: HTMLDivElement | undefined = $state()

  async function copySnippet() {
    try {
      await navigator.clipboard.writeText(snippet)
      copied = true
      if (copyTimer) clearTimeout(copyTimer)
      copyTimer = setTimeout(() => { copied = false }, 1800)
    } catch (e) {
      console.error('clipboard copy failed', e)
    }
  }

  function openLocation() {
    openHookScriptLocation().catch((err) => console.error(err))
  }

  function openDocs() {
    openSetupDocs().catch((err) => console.error(err))
  }

  // Grow the widget to fit the onboarding instructions in one resize. Two
  // dimensions to fit:
  //   - Width: the hook-script path and setup-guide URL are forced onto one
  //     line each, so we widen the window to the longest .meta line.
  //   - Height: `.panel` is `flex: 1; overflow-y: auto;`, so a too-short
  //     window just scrolls the instructions instead of growing — we read
  //     the panel's natural `scrollHeight` and grow to fit.
  // We measure BOTH dimensions before issuing a single `setWindowSize` call.
  // The height read at the current (narrow) width slightly overshoots — text
  // wraps more on a narrow line, making each paragraph one or two rows
  // taller than it'll be after the widen — but a single resize with a few
  // pixels of dead bottom space is far better than the visible flicker of
  // two separate resizes 1.5s apart. Both axes capped at 2/3 of the screen.
  // `set_window_size`'s WorkAreaBounds clamp moves the window up/left if the
  // new bottom/right would fall off-screen. Never shrinks below the current
  // viewport — manual user resizes stick, and the panel disappears as soon
  // as the first hook event arrives.
  onMount(() => {
    if (!referenceEl) return
    // Measure widening need. `scrollWidth` returns max(content, clientWidth)
    // per MDN, so once a line fits, scrollWidth === clientWidth and adding a
    // safety pad would request `innerWidth + pad` — making the window grow
    // a few pixels every hide/show cycle. So treat a line as "needs widening"
    // ONLY when it actually overflows (scrollWidth > clientWidth). When no
    // line overflows, widthTarget is left at the current viewport width, so
    // the Math.max idempotency check below short-circuits with no resize.
    const metas = referenceEl.querySelectorAll<HTMLElement>('.meta')
    let overflowingContent = 0
    for (const el of metas) {
      const prior = el.style.whiteSpace
      el.style.whiteSpace = 'nowrap'
      if (el.scrollWidth > el.clientWidth) {
        overflowingContent = Math.max(overflowingContent, el.scrollWidth)
      }
      el.style.whiteSpace = prior
    }
    const widthCap = Math.floor((window.screen.availWidth * 2) / 3)
    const widthTarget =
      overflowingContent > 0
        ? Math.min(Math.ceil(overflowingContent + 32 + 4), widthCap)
        : window.innerWidth

    const panelEl = document.querySelector('.panel') as HTMLElement | null
    const headerEl = document.querySelector('header') as HTMLElement | null
    let heightTarget = window.innerHeight
    if (panelEl && headerEl) {
      const naturalHeight =
        headerEl.getBoundingClientRect().height + panelEl.scrollHeight
      const heightCap = Math.floor((window.screen.availHeight * 2) / 3)
      heightTarget = Math.min(Math.ceil(naturalHeight), heightCap)
    }

    const finalWidth = Math.max(widthTarget, window.innerWidth)
    const finalHeight = Math.max(heightTarget, window.innerHeight)
    if (
      finalWidth !== window.innerWidth ||
      finalHeight !== window.innerHeight
    ) {
      setWindowSize('main', finalWidth, finalHeight, false).catch((err) =>
        console.error('setSize failed', err),
      )
    }
  })
</script>

<div class="panel">
  {#if onDismiss}
    <button type="button" class="dismiss" onclick={onDismiss}>Hide instructions</button>
  {/if}
  <h2>Instructions to connect Claude Code</h2>
  <p class="meta intro">Waiting for the first event from a Claude Code session — this panel goes away as soon as the dashboard receives one. Perform the steps below yourself, or ask Claude Code to do them for you.</p>

  <ol>
    <li>Open <code>~/.claude/settings.json</code> (create it if missing).</li>
    <li>
      Paste in the following:
      <div class="snippet-wrap">
        <pre class="snippet"><code>{snippet}</code></pre>
        <button type="button" class="copy" onclick={copySnippet} class:copied>
          {copied ? 'Copied' : 'Copy'}
        </button>
      </div>
    </li>
    <li>Restart Claude Code, then start a session.</li>
  </ol>

  <div class="reference" bind:this={referenceEl}>
    <p class="meta">
      <i>Hook script:</i>
      <button type="button" class="path-link" onclick={openLocation} title="Open containing folder">{hookPath}</button>
    </p>
    <p class="meta docs-line">
      <i>Full setup guide:</i>
      <button type="button" class="docs-link" onclick={openDocs} title="Open in browser">anothersava.github.io/claude-code-dashboard/pages/install</button>
    </p>
  </div>
</div>

<style>
  .panel {
    position: relative;
    flex: 1;
    min-height: 0;
    overflow-y: auto;
    padding: 10px 16px 6px;
    color: #d6d6d6;
    font-size: 12px;
    line-height: 1.5;
    user-select: text;
    -webkit-user-select: text;
    cursor: text;
  }
  .dismiss {
    position: absolute;
    top: 8px;
    right: 10px;
    background: #2a2a2d;
    border: 1px solid #3a3a3d;
    border-radius: 3px;
    padding: 3px 9px;
    color: #d6d6d6;
    font-size: 11px;
    font-weight: 500;
    letter-spacing: 0.2px;
    line-height: 1;
    cursor: pointer;
    opacity: 0;
    transition: opacity 120ms ease, background 120ms ease, color 120ms ease;
    z-index: 1;
  }
  .panel:hover .dismiss {
    opacity: 0.9;
  }
  .dismiss:hover {
    opacity: 1;
    background: #353539;
    color: #fff;
  }
  .panel button {
    cursor: pointer;
  }
  h2 {
    font-size: 14px;
    font-weight: 600;
    margin: 0 0 6px;
    color: #e8e8ea;
    letter-spacing: 0.1px;
  }
  .meta.intro {
    margin-bottom: 16px;
  }
  ol {
    margin: 0 0 16px;
    padding-left: 20px;
    display: flex;
    flex-direction: column;
    gap: 10px;
  }
  ol li {
    color: #d6d6d6;
  }
  code {
    font-family: ui-monospace, Consolas, monospace;
    font-size: 11px;
    background: rgba(255, 255, 255, 0.06);
    padding: 1px 5px;
    border-radius: 3px;
  }
  .snippet-wrap {
    position: relative;
    margin-top: 6px;
  }
  .snippet {
    margin: 0;
    padding: 10px 12px;
    background: #0f0f12;
    border: 1px solid #2a2a2d;
    border-radius: 4px;
    overflow-x: auto;
    font-family: ui-monospace, Consolas, monospace;
    font-size: 11px;
    line-height: 1.45;
    color: #d6d6d6;
    scrollbar-width: thin;
    scrollbar-color: rgba(255, 255, 255, 0.15) transparent;
  }
  .snippet::-webkit-scrollbar {
    width: 8px;
    height: 8px;
  }
  .snippet::-webkit-scrollbar-track {
    background: transparent;
  }
  .snippet::-webkit-scrollbar-thumb {
    background: rgba(255, 255, 255, 0.12);
    border-radius: 4px;
  }
  .snippet::-webkit-scrollbar-thumb:hover {
    background: rgba(255, 255, 255, 0.22);
  }
  .snippet code {
    background: none;
    padding: 0;
    border-radius: 0;
    font-size: inherit;
    color: inherit;
  }
  .copy {
    position: absolute;
    top: 6px;
    right: 6px;
    background: #2a2a2d;
    color: #d6d6d6;
    border: 1px solid #3a3a3d;
    border-radius: 3px;
    padding: 2px 8px;
    font-size: 10px;
    font-weight: 500;
    letter-spacing: 0.2px;
    opacity: 0.85;
    transition: opacity 120ms ease, background 120ms ease, color 120ms ease;
  }
  .copy:hover {
    opacity: 1;
    background: #353539;
    color: #fff;
  }
  .copy.copied {
    background: #3a7c4a;
    border-color: #3a7c4a;
    color: #fff;
    opacity: 1;
  }
  .reference {
    margin-top: 0;
  }
  .meta {
    margin: 0;
    color: #8a8a8e;
    font-size: 11px;
  }
  .docs-line {
    margin-top: 6px;
  }
  .path-link, .docs-link {
    background: none;
    border: 0;
    padding: 0;
    color: #7aa2f7;
    font: inherit;
    font-family: ui-monospace, Consolas, monospace;
    text-align: left;
    word-break: break-all;
  }
  .path-link:hover, .docs-link:hover {
    text-decoration: underline;
  }
</style>

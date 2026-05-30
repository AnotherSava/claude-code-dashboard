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

  // Grow the widget wide enough that the reference lines (hook script path,
  // setup-guide URL) fit on a single row each. We measure the natural width
  // each .meta paragraph wants by temporarily forcing nowrap, then ask the
  // backend to setSize. Capped at 2/3 of the screen's available width so the
  // widget can't overflow the desktop on tiny screens. Never shrinks below
  // the current viewport — manual user resizes upward stick.
  onMount(() => {
    if (!referenceEl) return
    const metas = referenceEl.querySelectorAll<HTMLElement>('.meta')
    let contentWidth = 0
    for (const el of metas) {
      const prior = el.style.whiteSpace
      el.style.whiteSpace = 'nowrap'
      contentWidth = Math.max(contentWidth, el.scrollWidth)
      el.style.whiteSpace = prior
    }
    if (contentWidth === 0) return
    // Panel padding (16 each side) + a few px for scrollbar / margin safety.
    const needed = Math.ceil(contentWidth + 32 + 4)
    const cap = Math.floor((window.screen.availWidth * 2) / 3)
    const target = Math.min(needed, cap)
    if (target > window.innerWidth) {
      // Height stays at whatever the user (or auto_resize) had it at.
      setWindowSize('main', target, window.innerHeight, false).catch((err) =>
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

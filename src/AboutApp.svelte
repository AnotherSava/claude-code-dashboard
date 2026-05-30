<script lang="ts">
  import { onMount, tick } from 'svelte'
  import { closeWindow, getAboutInfo, openDocsHome, setWindowSize } from './lib/api'
  import type { AboutInfo } from './lib/api'

  let info = $state<AboutInfo | null>(null)
  let aboutEl: HTMLDivElement | undefined = $state()

  onMount(() => {
    ;(async () => {
      try {
        info = await getAboutInfo()
        // Wait for Svelte to render the version + docs lines before we
        // measure their natural widths. Same nowrap-scrollWidth pattern as
        // SetupPanel's reference-line measurement; reuses set_window_width
        // on the backend with `recenter: true` so growth keeps the modal
        // centered on its monitor rather than sliding right.
        await tick()
        if (!aboutEl) return
        // Step 1 — minimum width: the Documentation URL line carries
        // `white-space: nowrap` so it must fit on one line. Other content
        // (title, description, version) wraps freely below that width.
        const docsLine = aboutEl.querySelector<HTMLElement>('.docs')
        if (!docsLine) return
        const priorWs = docsLine.style.whiteSpace
        docsLine.style.whiteSpace = 'nowrap'
        const docsContentWidth = docsLine.scrollWidth
        docsLine.style.whiteSpace = priorWs
        if (docsContentWidth === 0) return
        const styles = window.getComputedStyle(aboutEl)
        const padLeft = parseFloat(styles.paddingLeft) || 0
        const padRight = parseFloat(styles.paddingRight) || 0
        const padTop = parseFloat(styles.paddingTop) || 0
        const padBottom = parseFloat(styles.paddingBottom) || 0
        const widthCap = Math.floor((window.screen.availWidth * 2) / 3)
        const targetWidth = Math.min(
          Math.ceil(docsContentWidth + padLeft + padRight + 4),
          widthCap,
        )

        // Step 2 — content height at the *target* width. Temporarily pin
        // `aboutEl` to the new width so the description re-wraps as it will
        // post-resize; measure the bottom of the last child relative to
        // `aboutEl`'s top, then add padding-bottom (matching padding-top).
        const priorWidth = aboutEl.style.width
        const priorHeight = aboutEl.style.height
        const priorBoxSizing = aboutEl.style.boxSizing
        aboutEl.style.boxSizing = 'border-box'
        aboutEl.style.width = `${targetWidth}px`
        aboutEl.style.height = 'auto'
        void aboutEl.offsetHeight // force reflow
        const last = aboutEl.lastElementChild as HTMLElement | null
        let contentBottom = padTop
        if (last) {
          const aboutRect = aboutEl.getBoundingClientRect()
          const lastRect = last.getBoundingClientRect()
          contentBottom = lastRect.bottom - aboutRect.top
        }
        aboutEl.style.width = priorWidth
        aboutEl.style.height = priorHeight
        aboutEl.style.boxSizing = priorBoxSizing
        const heightCap = Math.floor((window.screen.availHeight * 2) / 3)
        const targetHeight = Math.min(
          Math.ceil(contentBottom + padBottom),
          heightCap,
        )

        if (targetWidth !== window.innerWidth || targetHeight !== window.innerHeight) {
          setWindowSize('about', targetWidth, targetHeight, true).catch((err) =>
            console.error('setSize failed', err),
          )
        }
      } catch (e) {
        console.error('get_about_info failed', e)
      }
    })()
  })

  function onDocsClick(e: Event) {
    e.preventDefault()
    openDocsHome().catch((err) => console.error(err))
  }

  function onKeydown(e: KeyboardEvent) {
    if (e.key === 'Escape') closeWindow()
  }
</script>

<svelte:window onkeydown={onKeydown} />

<div class="about" bind:this={aboutEl}>
  <h1>Claude Code Dashboard</h1>
  <p class="desc">A live monitor for your Claude Code sessions.</p>
  <p class="desc">
    You can track agent's current state and task, model's context usage, and a
    conversation history. Optional notifications ping you when an agent is blocked
    on input.
  </p>
  <p class="desc">
    The header tracks your 5-hour and 7-day Anthropic usage limits.
  </p>
  {#if info}
    <p class="version-line">
      Version {info.version}{info.release_date ? ` from ${info.release_date}` : ''}
    </p>
    <p class="docs">
      Documentation: <a href="#" onclick={onDocsClick}>{info.docs_url}</a>
    </p>
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
  .about {
    height: 100vh;
    box-sizing: border-box;
    padding: 12px 22px 18px;
    display: flex;
    flex-direction: column;
    gap: 10px;
    user-select: text;
    -webkit-user-select: text;
  }
  h1 {
    font-size: 14px;
    font-weight: 600;
    margin: 0;
    color: #e8e8ea;
  }
  .desc {
    font-size: 12px;
    color: #a0a0a4;
    margin: 0;
    line-height: 1.45;
  }
  /* Title → first paragraph: 1.5× the inter-paragraph gap. The flex layout
     supplies the base 10px via `gap`; this margin adds 5px on top of that. */
  .desc:first-of-type {
    margin-top: 5px;
  }
  /* Last paragraph → version line: 2× the inter-paragraph gap (10 + 10 = 20). */
  .version-line {
    font-size: 11px;
    color: #8a8a8e;
    margin: 10px 0 0;
  }
  .docs {
    font-size: 11px;
    color: #8a8a8e;
    margin: 0;
    white-space: nowrap;
  }
  .docs a {
    color: #7aa2f7;
    text-decoration: none;
    font-family: ui-monospace, Consolas, monospace;
  }
  .docs a:hover {
    text-decoration: underline;
  }
</style>

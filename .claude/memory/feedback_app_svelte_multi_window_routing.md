---
name: App.svelte multi-window finally-block trap
description: App.svelte routes via getWindowLabel and runs the same finally{showWindow} block for every window — must guard with !historyMode && !aboutMode or hidden secondaries auto-reveal
type: feedback
---

`src/App.svelte` is the entry component for every Tauri window in this project (main, history, about). It branches by `getWindowLabel()`, sets a mode flag (`historyMode`, `aboutMode`), and early-returns to render the right root (`HistoryApp`, `AboutApp`, or the main widget). But the `try/catch/finally` wrapping that detection has a `finally { if (!historyMode) await showWindow() }` that runs **after** the early return — JavaScript's finally always fires.

**Why:** Without an explicit guard, the about window (configured `visible: false` in `tauri.conf.json`) auto-shows on every app launch because `historyMode` is false in its webview. Same trap will apply to any future secondary window.

**How to apply:** Whenever a new window label is added, update the `finally`'s reveal guard to exclude it. Current form:

```ts
if (!historyMode && !aboutMode) {
  await showWindow()
}
```

Any new mode must be added to that conjunction. The fix is one line — but the bug is invisible until you notice the rogue window flashing open on each launch.

---
name: auto_resize_scrollbar_instrumented
description: Intermittent auto-resize scrollbar on mixed-DPI — ground-truth instrumentation deployed 2026-07-24; the periodic-backstop fix is designed but DEFERRED until a real drift is captured in the log
metadata:
  type: project
---

Reported 2026-07-24: a vertical scrollbar on the auto-resizing widget (mixed-DPI multi-monitor). Live investigation found the window **currently sized correctly** — the "stuck short" readings were a DPI-unaware-probe artifact ([[debug_dpi_unaware_probe_virtualization]]). The webview's own `inner_height` logs show only **brief** short states (66/89 CSS vs 105 desired) that each self-correct on the next measure — i.e. the ~1-frame grow lag plus mixed-DPI transition transients. The screenshot caught one such transient; no *persistent* stuck state was reproducible.

**Chosen path: instrument first, then fix.** Instrumentation deployed (app 1.6.3, uncommitted as of writing):
- Rust `auto_resize::apply` now logs `os_dpi` (fresh `GetDpiForWindow`, can't go stale like Tao's cached `scale_factor`) + `actual_client_h` (real client height read back *after* `set_size`). Next-drift signatures: `actual_client_h != new_height_phys` = OS swallowed/rescaled the resize; `os_dpi/96 != scale` = Tao scale stale.
- Frontend `measureAndSend` emits a throttled `auto_resize dedup` trace (one line per changed `desired:inner_height:scale` signature) — a dedup with `inner_height >= desired` while the window is visibly short = the webview's `innerHeight` is the stale metric. Marked temporary in-code; remove once the repro lands.

**When the scrollbar recurs:** `grep 'auto_resize::apply\|auto_resize dedup' widget.jsonl` around that time and read the new fields to pin which layer failed.

**Designed-but-deferred fix (do NOT ship without a captured repro):** add an unconditional low-frequency backstop re-measure + re-measure on `visibilitychange`→visible and window `focus` (the moments a throttled DPI update lands per the WebView2/Chromium occlusion research: [WebView2Feedback #4826]). This ends the recurring "nothing re-fires a measure → stuck until an unrelated later trigger" class every auto-resize note cites, and is safe because `measureAndSend` dedups to a no-op when already correct. Do NOT re-touch which scale source to trust (`devicePixelRatio` vs `scale_factor` both proven able to go stale — see [[auto_resize_dpr_flicker_collapse]]); the cure is guaranteeing a re-measure, not picking a scale. Related: [[debug_auto_resize_display_disable_collapse]] (the bounded heal retry, same family).

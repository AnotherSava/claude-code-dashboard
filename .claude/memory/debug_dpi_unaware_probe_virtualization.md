---
name: debug_dpi_unaware_probe_virtualization
description: External GetClientRect/GetWindowRect probes return ÷scale VIRTUALIZED values for the per-monitor-aware widget on a HiDPI monitor unless the probe thread sets PerMonitorV2 awareness first — a correctly-sized window reads back one-row-short and fakes a bug
metadata:
  type: project
---

When probing the dashboard window's geometry from an **external** process (PowerShell, a debug tool), `GetClientRect`/`GetWindowRect` return **DPI-virtualized** coordinates — scaled by `96/os_dpi` — because Windows PowerShell launches **DPI-unaware** (`GetAwarenessFromDpiAwarenessContext == 0`). On a 144-DPI (1.5×) monitor a genuinely-correct **158-physical** window reads back as **105** (`158 × 96/144`), and its "CSS height" computes to **70** (`105/1.5`) instead of the true **105.33** (`158/1.5`). That fakes a "window stuck one row short / scrollbar" bug on a window that is actually sized perfectly.

This cost a wrong diagnosis on 2026-07-24: an elaborate "devicePixelRatio stale → dedup keeps window short" story built entirely on virtualized 105/70 readings. The tell that should have caught it sooner: the app's own `auto_resize::apply` log read `scale:1.5, new_height_phys:158` (sizing correctly) while the external probe insisted the window was 105 — a contradiction that means the *probe* is lying, not the app.

**Probe correctly:** call `SetThreadDpiAwarenessContext((IntPtr)-4)` (`DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2`) at the top of the probe, *then* `GetClientRect` returns true physical px. Confirm with `GetAwarenessFromDpiAwarenessContext(GetThreadDpiAwarenessContext())` (0=unaware, 1=system, 2=permonitor).

**`GetDpiForWindow` is NOT virtualized** — it returns the real per-monitor DPI regardless of the caller's awareness (read 144 correctly even from the unaware probe). So the mismatch between an honest `GetDpiForWindow` and a virtualized `GetClientRect` is itself the diagnostic.

**Prefer the app's own signals over an external probe:** the frontend `inner_height` is webview CSS px (never virtualized), and `auto_resize::apply` now logs `os_dpi` (GetDpiForWindow) + `actual_client_h` (GetClientRect from *inside* the per-monitor-aware app process = true physical). See [[auto_resize_scrollbar_instrumented]]. Related: [[auto_resize_dpr_flicker_collapse]], [[debug_auto_resize_dpi_drift]], [[debug_state_transitions_via_widget_jsonl]].

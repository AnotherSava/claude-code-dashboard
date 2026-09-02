---
name: notification-delivery-channel
description: If the dashboard ever delivers notifications, write OSC 777 to the session tty — not agtermctl notify, not tauri-plugin-notification
metadata:
  type: project
---

The notification decision engine (`notifications.rs`) has always had a delivery gap: `TelegramNotifier` is the only `Notifier`, and the user's Telegram credentials are empty. Three delivery routes were investigated on 2026-09-01 and two were ruled out on verified grounds.

**Ruled out — `tauri-plugin-notification`.** Its desktop `impl` is three methods (`builder`/`request_permission`/`permission_state`). `remove_active`, `remove_active_by_id`, `active`, `cancel`, `register_action_types` are all `#[cfg(mobile)]`; the JS twins call commands `init()` never registers. `NotificationBuilder::id()` compiles on desktop and is never read. `show()` is `spawn(async move { let _ = notification.show(); }); Ok(())` — no handle, no error, no delivery signal — and desktop `permission_state()` is a hardcoded `Granted` stub. That breaks `Notifier::dismiss`, `SendError.maybe_delivered` and the `Outstanding` handle at once. The ceiling is the plugin, not macOS: `mac-usernotifications` 0.3.1 (UNUserNotificationCenter) has caller-set ids, `close_delivered`, action buttons and real auth state.

**Ruled out — `agtermctl notify`.** It increments `Session.unseenCount` *before* the banner gate and runs no focus suppression by design ("the caller asked for it"), so it badges the pane the user is looking at. Routing through it swaps which program raises the red pill for exactly the pill this work exists to remove. It also returns no message id, and `session seen` is session-scoped, so it would cross-dismiss the three concurrent per-session alert maps.

**The route, if it is ever built — OSC 777 written to the session tty.** libghostty parses the pty byte stream, so `GHOSTTY_ACTION_DESKTOP_NOTIFICATION` cannot tell our bytes from Claude Code's; agterm's handler then gates on `!(firingIsFocused && appActive)` and drops the notification entirely for the focused pane. That yields per-session attribution, click-to-reveal, *and* the focus gate at zero coupling and zero new dependency — through the same `push_title` path `terminal_title.rs` already uses for OSC 0. Notification identity is `<windowID>:<sessionID>:<pane>`, which coalesces repeats from one pane, so a re-fire replaces rather than stacks and may cover dismissal with no handle at all. (Verified statically in agterm by its own session; never executed from here.)

**Why:** the whole point was removing Claude Code's badge logic, so any route that re-raises an unseen-count pill for a pane the user is watching fails the actual requirement, however good its API looks.

**How to apply:**
- `reconcile` (`notifications.rs:483`) is already channel-generic and the test `Mock` proves it — a per-state second channel needs **no** trait change. Full parity needs one free-text method (context/drift/limit-reset call the inherent `TelegramNotifier::send_raw_tracked`); do not name it `send_text`, which collides with a private inherent in `telegram.rs`.
- Give a non-dismissing channel only the families where dismissal is moot (per-state, limit-reset). Context and drift are *defined* by dismiss-on-clear — keep them Telegram-only rather than adding a `can_dismiss()` flag.
- In `NotificationManager::spawn`, `reset_tracker` and `last_usage_updated` are channel-*independent* (they observe the usage poller) and must not be split per channel; everything else in that block must.
- Unverified and gating any desktop-banner path: whether an `ActivationPolicy::Accessory`, ad-hoc-signed bundle can post macOS notifications at all, and whether authorization survives an ad-hoc rebuild (the cdhash changes per build).

Related: [[agterm-status-coexistence]], [[notification-engine-never-fired]], [[macos-signing-strategy]].

use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Mutex;

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(default)]
pub struct Config {
    pub server_port: u16,
    pub always_on_top: bool,
    pub save_window_position: bool,
    pub window_position: Option<WindowPosition>,
    pub context_window_tokens: HashMap<String, u64>,
    /// Green/amber/red palette for the usage numbers — the limit bars (fill +
    /// percentage) and the per-session token counter. Frontend-only: the tray
    /// icon keeps its own brighter, icon-tuned shades.
    pub usage_colors: UsageColors,
    /// When true, the token counter interpolates between `usage_colors` as a
    /// smooth gradient (green@0 → amber@50 → red@85) instead of the 3-color step
    /// the limit bars use. The limit bars are always a step.
    pub token_gradient: bool,
    /// Read by `adapters::claude`: conversational closers that end with '?'
    /// but shouldn't register as blocked (e.g. "What's next?"). Matched
    /// case-insensitively as a *suffix* of the final question.
    pub benign_closers: Vec<String>,
    /// Read by `adapters::claude`: conversational *openers* of the final
    /// question that mark it an optional offer rather than a hand-back (e.g.
    /// "anything" → "Anything you'd like to look at?"). Matched
    /// case-insensitively as a *prefix* of the last sentence. An embedded real
    /// ask still registers via the permission-seeking path, so
    /// "Anything else, or shall I commit?" still awaits.
    pub benign_openers: Vec<String>,
    /// Read by `adapters::claude`: used to derive a friendly chat_id from
    /// `cwd`. When a Claude session starts under this directory, the relative
    /// path is used as the session id. None = always use the basename of cwd.
    pub projects_root: Option<String>,
    /// Channel notifications (Telegram today, desktop later). Missing object =
    /// disabled entirely; missing channel inside = that channel disabled.
    pub notifications: Option<NotificationsConfig>,
    /// How often to poll Anthropic's /api/oauth/usage endpoint. Anthropic
    /// rate-limits this endpoint aggressively (see claude-code#31637), so 10
    /// minutes is the conservative default. Clamped to 60s minimum at runtime.
    pub usage_limits_poll_interval_seconds: u64,
    /// Seconds after a usage window's reset (5h or 7d `resets_at`) to land the
    /// next poll. When a reset falls within one poll interval, `usage_limits`
    /// skips its regular poll and instead wakes this many seconds *after* the
    /// reset — past the ±1min `resets_at` jitter and server propagation — so the
    /// bar refreshes off its stale pre-reset value promptly (within this delay)
    /// instead of up to a full interval later. Skipping the pre-reset poll also
    /// keeps the two polls from firing close together and tripping the endpoint's
    /// aggressive 429. Clamped to 60s minimum at runtime (with the interval).
    pub usage_reset_poll_delay_seconds: u64,
    /// Number of segments in the 5h / 7d usage limit bars. Segments scale to
    /// fit the available track width; higher values give finer resolution but
    /// thinner individual segments.
    pub limit_bar_segments: u32,
    /// Auto-resize the window to fit content height. When set to Up, the
    /// bottom edge stays put and the window grows upward; Down keeps the top
    /// edge fixed; None leaves the window manually sized.
    pub auto_resize: AutoResize,
    pub history_font_size: HistoryFontSize,
    /// Which unit the Work intensity chart plots — see [`IntensityUnit`]. Set
    /// from the chart's own Percent|Tokens control, so it has no tray item.
    pub intensity_unit: IntensityUnit,
    pub history_window_position: Option<WindowPosition>,
    /// Whether the history window was maximized when last closed. Persisted
    /// separately from `history_window_position` because a maximized window's
    /// outer rect is inflated by the frame (it sits ~8px past each work-area
    /// edge), so capturing it as the restore bounds would grow the window on
    /// every reopen. Instead we keep the last *unmaximized* bounds in
    /// `history_window_position` and re-maximize on open when this is set.
    pub history_window_maximized: bool,
    /// When the app is auto-launched at login (the "Open to tray" mode), keep
    /// the main window hidden so only the tray icon appears. Read at startup in
    /// `lib.rs`, but only honored when the launch actually came from autostart
    /// (signaled by the `--autostarted` arg) — a manual launch always reveals
    /// the window regardless of this flag.
    pub start_minimized: bool,
    /// Whether autostart is *wanted*. The OS entry (registry / LaunchAgent) is
    /// still the mechanism, but it is no longer the only record of the choice:
    /// `brew uninstall`/`brew upgrade` delete the LaunchAgent via the cask's
    /// `uninstall launchctl:` stanza, and without this field the app had no way
    /// to tell "the user turned it off" from "something else removed it", so
    /// autostart silently stayed off after every brew cycle.
    ///
    /// `None` means "not yet recorded" — `lib.rs` seeds it from the live OS
    /// state on first launch after upgrading, so an existing user who had
    /// deliberately turned autostart off does not get it switched back on.
    /// Once set, `lib.rs` re-creates a missing OS entry when this is `true`;
    /// it deliberately never removes one when this is `false`, since disabling
    /// via System Settings → Login Items leaves the plist in place and would
    /// otherwise be fought.
    pub autostart: Option<bool>,
    /// Read by `state::apply_set`: prompts that suppress the `done`/`idle` →
    /// `working` task boundary. When the user types one of these as a fresh
    /// prompt after the agent has finished, treat it as a continuation of
    /// the previous task rather than a new one — preserve `original_prompt`
    /// and the working timer instead of resetting them. Match is exact,
    /// case-insensitive, after trimming whitespace.
    pub continuation_prompts: Vec<String>,
    /// Mirror each session's status onto its terminal tab title as
    /// "<colored circle> <name>" (e.g. "🔵 ai-dashboard"). Read by
    /// `terminal_title::sync`; Windows-only today.
    pub terminal_titles: bool,
    /// When a session's context usage reaches this percent of its model's
    /// window, append it to the terminal tab title as " [N%]" (e.g.
    /// "🔵 printlab [67%]") — a glanceable "this one's filling up" cue right in
    /// the tab. `null` or `0` disables the suffix (the glyph + name still show).
    /// Read by `terminal_title::sync`; needs `terminal_titles` on to appear at
    /// all. Same percentage as the token counter (`notifications::context_percent`).
    pub terminal_title_context_percent: Option<f32>,
    /// Full-height value for one 10-minute bar of the Work intensity chart's
    /// token view. Unlike the percentage view there is no quota to be a fraction
    /// of, so this is a stated ceiling rather than a derived one: bars at or
    /// above it clip. The default clips 4.5% of active buckets (measured p50
    /// 77k, p90 666k, p99 1517k) — raise it to flatten the chart, lower it for
    /// more resolution on ordinary work, at the cost of clipping more of a busy
    /// day. The Weeks view sums groups of three buckets
    /// and scales this by the same factor, so a full-height bar means the same
    /// rate in both views. `null` or `0` falls back to the default.
    pub intensity_axis_max_tokens: Option<f64>,
    /// Revert a `Working` row to its pre-prompt status when its turn was
    /// cancelled with Esc — which emits no lifecycle hook. Gated by this flag,
    /// `log_watcher` detects the cancel from the "[Request interrupted by user]"
    /// transcript marker (cross-platform), which Claude Code writes even for an
    /// instant cancel with no output. Off keeps the row `Working` until the next
    /// prompt.
    pub detect_cancelled_turns: bool,
    /// Remove a session's row once its owning Claude process has exited without
    /// a `SessionEnd` — which Claude Code fails to deliver on `exit` / Ctrl-D /
    /// terminal close (unlike `/clear`), stranding the row in its last state
    /// (often `Working` if the user exited mid-turn). Read by `liveness_reaper`,
    /// which removes the row only once the owning pid is positively confirmed
    /// gone. Off keeps the stranded row until the next `/clear` or app restart.
    pub reap_exited_sessions: bool,
    /// Track which finished sessions the user has actually looked at, so a row
    /// that finished and hasn't been read stands apart from one already read.
    /// Read by `commands::resolved_snapshot` (which stamps the verdict) and by
    /// `attention`, which observes the two things that count as looking: opening
    /// a row's history window, and — on macOS with agterm — producing input in
    /// the terminal while that session is the selected one.
    ///
    /// This is the axis `notifications`' `afk_window_ms` cannot reach: that
    /// measures input anywhere on the desktop, so it reads "at the desk" while
    /// the user types in a different session entirely. Off makes every row render
    /// as it did before the feature existed.
    pub attention_tracking: bool,
    /// Give a live session its row back after the dashboard restarts, instead of
    /// leaving it invisible until it next acts — which a session parked on a
    /// question cannot do, because it is waiting for the user.
    ///
    /// Read by `session_restore`, which asks Claude Code's session registry which
    /// agents are alive here and the terminal what each of their tabs says, and
    /// creates only the rows that both answer for. Off leaves the dashboard empty
    /// on a cold start, as it was before the feature existed. Requires a terminal
    /// adapter (`terminals::for_platform`), so it does nothing on Windows yet, and
    /// requires `terminal_titles` — the tab title is the only place a row's status
    /// outlives this process.
    pub restore_sessions: bool,
    /// Grace window (ms) after which a `Waiting` row that hasn't changed status
    /// is settled to `Done`. `Waiting` ("looks done but isn't") is entered at
    /// `Stop` from the hook's `background_tasks` and normally left when the
    /// background work finishes and the follow-up turn settles the row — but a
    /// background task the user *kills* (e.g. a dev server via the Claude UI)
    /// ends silently (no hook, nothing in the transcript), so nothing clears the
    /// row and it sits in WAIT until the next prompt. Read by `waiting_settle`,
    /// the backstop that settles it. It's pure time-in-state, so legitimate
    /// background work — which self-resolves well within the window (finite
    /// shell tasks and subagents both cap ~9 min in practice) — is never reached;
    /// only a stuck, killed-task WAIT ages past it. `None`/`0` disables.
    pub waiting_settle_ms: Option<u64>,
    /// Multi-device session sync (see `sync.rs`). Disabled by default:
    /// `listen=false` and empty `peers` make every sync task a no-op.
    pub sync: SyncConfig,
    /// What the tray icon shows for a usage bucket. `None` keeps the plain app
    /// icon. The `*Light` modes recolor the traffic-light icon by usage; the
    /// `*Number` modes draw the percentage (and the all-red light at 100%).
    /// Read by `tray_badge::refresh`; the tray's "Tray usage badge" submenu
    /// writes it. The hover tooltip always shows both buckets regardless.
    pub tray_badge: TrayBadge,
    /// Whether the tray icon flags high context usage at all — the user toggle
    /// behind the "Show high context usage" tray checkbox (on by default). When
    /// off, no alert is drawn regardless of `tray_context_alert_percent`, which
    /// is preserved so re-enabling restores the same threshold.
    pub tray_context_alert_enabled: bool,
    /// The threshold for the high-context-usage alert: when enabled and at least
    /// one local session's context usage reaches this percent of its model's
    /// window, the tray icon flags it — an at-a-glance "an agent is filling its
    /// context" warning that overlays whichever badge style is active (the
    /// icon-bearing styles recolor the icon's border red; the number styles draw
    /// the digits over a red background). `null`/`0` disables it; the alert never
    /// shows when `tray_badge` is `None` (no badge to frame), nor when
    /// `tray_context_alert_enabled` is off. Read by `tray_badge::refresh`.
    pub tray_context_alert_percent: Option<f32>,
    /// "High alert" mode: deliver every *enabled* per-state notification
    /// (blocked / done / error) to Telegram the instant the state is entered,
    /// bypassing both the per-state AFK / reaction windows and the reading-time
    /// budget. A state disabled by zeroing both its windows stays silent — high
    /// alert speeds up pings that are already configured, it doesn't resurrect
    /// disabled ones. Does *not* affect the high-context-usage or usage-limit-
    /// reset alerts (those keep their own lifecycles). Toggled from the tray's
    /// "High alert" checkbox; read by `notifications::reconcile`. Off by default.
    pub high_alert: bool,
    /// Instruction-adherence canary. When on, each session is issued a rotating
    /// nonce at `SessionStart` (returned to the hook, which injects an
    /// instruction to end every reply with the marker below), and every `Stop`
    /// checks the final assistant message for that exact marker. A miss flags the
    /// row with an orthogonal "instruction drift" warning — a dashboard badge, a
    /// `⚠` in the terminal tab title, and a Telegram ping — without touching the
    /// real status. A dropped marker means the agent has stopped honoring its
    /// standing instructions: a cue to stop trusting the output and compact /
    /// re-anchor. Off by default (it injects an instruction into every session and
    /// can ping). Read by `http_server` (mint + check), `notifications` (ping),
    /// and `terminal_title` + the row (render).
    pub instruction_canary_enabled: bool,
    /// Compact view: hide each row's current prompt and time-in-state, and
    /// collapse the 5h/7d usage bars down to their bare percentage — the
    /// "just the numbers" density mode. Read by the frontend (`SessionItem`,
    /// `LimitBar`); toggled from the tray's **Compact view** checkbox. Off by
    /// default.
    pub compact_mode: bool,
    /// Keep macOS awake with the lid closed while a local agent is working — the
    /// "carry the laptop between locations mid-task" case. macOS offers no API
    /// for this: every IOKit assertion carrying `AppliesOnLidClose` is refused
    /// (`kIOReturnNotPrivileged`) without an Apple-private entitlement, so the
    /// only lever is the root-only, system-wide `pmset -a disablesleep` kill
    /// switch, reached through a one-time scoped sudoers grant (see
    /// `lid_awake::install`). Because that switch disables *every* sleep —
    /// including thermal-emergency and low-battery sleep — the hold is always
    /// bounded by `lid_awake_minutes` and floored by
    /// `lid_awake_battery_floor_pct`. macOS-only; a no-op elsewhere.
    /// Read by `lid_awake::sync`; the tray's "Keep awake with lid closed"
    /// submenu writes it.
    pub lid_awake_mode: LidAwakeMode,
    /// How long the lid may stay closed before the veto is released and the Mac
    /// is allowed to sleep. The countdown is anchored to the *lid close*, not to
    /// the start of the work — closing the lid always buys a full window, and
    /// reopening resets it. This is the cap on how long thermal and low-battery
    /// safety sleep stay disabled, so it is deliberately short. `0` disables the
    /// feature outright (same as `LidAwakeMode::Off`).
    pub lid_awake_minutes: u64,
    /// Grace period after the last busy session before the veto is released.
    /// Agents flap Working → Done → Working between turns, and releasing on the
    /// first `Done` would sleep the Mac out from under a session that was about
    /// to continue. Read by `lid_awake::should_release`.
    pub lid_awake_release_grace_ms: u64,
    /// Battery floor: never arm the veto below this percentage, and release it
    /// if the battery falls below while armed. `disablesleep` suppresses the
    /// low-battery emergency sleep, so without this floor a closed laptop could
    /// run itself flat instead of sleeping. `0` disables the floor.
    pub lid_awake_battery_floor_pct: u8,
}

/// The standing policy for when the lid-closed sleep veto arms by itself. See
/// `Config::lid_awake_mode`.
///
/// This covers *automatic* arming only. The tray's "Start now" is a one-shot
/// action that overrides whatever policy is set — `Off` included — so there is
/// deliberately no `Manual` variant: a mode whose only behavior is "don't arm
/// automatically" would be indistinguishable from `Off`.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum LidAwakeMode {
    /// Never arm on its own. "Start now" still works.
    #[default]
    Off,
    /// Arm while a local agent is busy *and* the Mac is running on battery. The
    /// narrow policy: a docked machine on AC isn't about to be carried
    /// anywhere, so this keeps the veto — and its suppressed thermal safety
    /// sleep — off during desk work.
    OnBattery,
    /// Arm whenever a local agent is busy, on any power source.
    Always,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TrayBadge {
    #[default]
    None,
    FiveHourLight,
    SevenDayLight,
    // `five_hour` / `seven_day` were the pre-light numeric-only values.
    #[serde(alias = "five_hour")]
    FiveHourNumber,
    #[serde(alias = "seven_day")]
    SevenDayNumber,
}

impl TrayBadge {
    /// True for the traffic-light modes (vs the numeric modes or `None`).
    pub fn is_light(self) -> bool {
        matches!(self, Self::FiveHourLight | Self::SevenDayLight)
    }
}

/// How far the sync listener opens up: which interfaces it binds and which
/// source addresses it will talk to. Both halves move together on purpose —
/// a user who has to reach the listener from outside the tailnet needs the
/// socket *and* the guard to agree, and two knobs that must be set to the same
/// thing are one knob with a way to get it wrong.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SyncBindScope {
    /// Default. Bind this device's Tailscale addresses (plus loopback) and
    /// refuse any request whose source is outside 100.64.0.0/10,
    /// fd7a:115c:a1e0::/48 or loopback. When no Tailscale address can be found
    /// the listener still starts — bound to all interfaces, logged as degraded
    /// — because a listener that silently never came up is a worse outcome than
    /// a wide socket that still rejects every non-tailnet caller.
    #[default]
    Tailnet,
    /// Bind all interfaces and accept any source, leaving the bearer token as
    /// the only gate. For syncing over something that isn't Tailscale (another
    /// VPN, a trusted LAN). It is an escape hatch, not a tuning knob: it puts
    /// the port and the token in front of every host that can route here.
    Any,
}

/// Settings for syncing sessions between dashboards on different devices
/// (reachable over a VPN such as Tailscale). `peers`/`token`/`device_name`
/// hot-reload (the pusher re-reads config each cycle); `listen`/`listen_port`/
/// `bind_scope` need a restart, like `server_port` — they are read once when
/// the listener is spawned and there is no rebind path (see `sync::run_listener`).
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(default)]
pub struct SyncConfig {
    /// Name other dashboards show on this device's session badges. Empty =
    /// resolved once at startup from the hostname and written back.
    pub device_name: String,
    /// Accept session pushes from peers on `listen_port`. Off by default; when
    /// on, `bind_scope` decides how far the listener opens up.
    pub listen: bool,
    pub listen_port: u16,
    /// Interfaces the listener binds and sources it answers. Defaults to
    /// `Tailnet` — the bearer `token` is the credential, not the perimeter.
    pub bind_scope: SyncBindScope,
    /// Peer sync listeners to push local sessions to, e.g.
    /// "http://my-laptop:9078". Empty = push nothing.
    pub peers: Vec<String>,
    /// Shared secret across all of the user's devices. With `None`, sync is
    /// fully disabled (no listener, no pushes) even if `listen`/`peers` are
    /// set — never run unauthenticated.
    pub token: Option<String>,
    /// Accept relayed peer messages, which **start a turn** in a live local
    /// agent at that agent's permission level.
    ///
    /// Separate from `listen`, and off by default, because the two grant
    /// materially different things. A user who turned sync on was buying a
    /// read-only view of session state on another screen; delivery turns the
    /// same token into the power to run prompts inside their agents. Folding
    /// that into `listen` would widen an existing deployment the moment it
    /// updated, silently and without the user choosing it — which is precisely
    /// the shape this project refuses elsewhere. Requires `listen` and a
    /// `token`: it is an additional gate, never a replacement for them.
    #[serde(default)]
    pub accept_messages: bool,
    /// `device_name` → Tailscale node name, binding a name this dashboard
    /// *addresses* to a machine Tailscale can *identify*.
    ///
    /// It has to live here, in local config on the receiver, rather than ride
    /// the wire: a sender controls every field of its own envelope, so a hostile
    /// node would claim `device_name = "CHROME"` beside its own truthful node
    /// name and attest itself. An out-of-band binding is what makes the check
    /// non-circular.
    ///
    /// Empty by default, and an unlisted device is `Claimed` rather than
    /// refused — see `tailnet::attest`. The names genuinely differ in practice
    /// (`CHROME` against node `chrome`, `Some-Laptop.local` against node
    /// `some-laptop`), which is why this is explicit config and not a
    /// derivation.
    #[serde(default)]
    pub peer_identity: std::collections::BTreeMap<String, String>,
}

impl Default for SyncConfig {
    fn default() -> Self {
        Self {
            device_name: String::new(),
            listen: false,
            listen_port: 9078,
            bind_scope: SyncBindScope::Tailnet,
            peers: Vec::new(),
            token: None,
            accept_messages: false,
            peer_identity: std::collections::BTreeMap::new(),
        }
    }
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AutoResize {
    #[default]
    None,
    Up,
    Down,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum HistoryFontSize {
    Smallest,
    Small,
    #[default]
    Regular,
    Large,
    Largest,
}

/// Which unit the Work intensity chart plots. Orthogonal to its Days/Weeks
/// toggle. `Percent` is the default: it keeps the chart answering "am I burning
/// my quota", the question the reference line and the clip threshold were built
/// around. `Tokens` answers "how much work happened" instead — immune to a plan
/// change, but only covering the span transcripts reach back to.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum IntensityUnit {
    #[default]
    Percent,
    Tokens,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(default)]
pub struct NotificationsConfig {
    pub telegram: Option<TelegramConfig>,
}

impl Default for NotificationsConfig {
    fn default() -> Self {
        Self { telegram: Some(TelegramConfig::default()) }
    }
}

/// Per-state notification rule. Both windows are independent and optional:
/// the reconciler fires when *either* the AFK criterion or the reaction
/// backstop is met (see `notifications::reconcile`).
///
/// - `afk_window_ms`: fire once the user has been idle this long *and* has had
///   no input since the state began (the "saw it" guard). Unset/0 = no AFK
///   trigger for this state.
/// - `reaction_window_ms`: fire once the state has lasted this long regardless
///   of presence — the "you didn't react in time" backstop. Unset/0 = no
///   backstop.
///
/// A state with neither window set never notifies.
#[derive(Clone, Copy, Debug, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct StateNotify {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub afk_window_ms: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reaction_window_ms: Option<u64>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(default)]
pub struct TelegramConfig {
    pub bot_token: Option<String>,
    pub chat_id: Option<String>,
    /// Per-state notification rules, keyed by status: "idle" | "working" |
    /// "blocked" | "done" | "error". Missing key = silent for that state.
    pub states: HashMap<String, StateNotify>,
    /// Context-usage alert: fire a one-shot message when a session's
    /// `input_tokens` over its model's window (longest-prefix lookup in
    /// `context_window_tokens`) crosses this percent.
    /// `null` or `0` disables it. The alert follows the same lifecycle as the
    /// per-state notifications: it fires once on crossing, then the message is
    /// deleted once usage drops back below the threshold (a new task or `/clear`
    /// resets the token count), the session vanishes, or the feature is turned
    /// off — and it re-arms after a drop so a later crossing alerts again.
    pub context_alert_percent: Option<f32>,
    /// Fire a one-shot Telegram message when the 5-hour or 7-day usage window
    /// resets, if the window that just ended had been used to at least this
    /// percent. `null` or `0` disables it. A reset is detected from the usage
    /// poller's `resets_at` jumping forward by ~a window length (never the
    /// ±1min-jittery value directly), so the post-cap transient that briefly
    /// zeroes both buckets' percentages doesn't false-fire. Unlike the other
    /// alerts this is a point event — it's sent once and never auto-deleted.
    /// Account-wide, so with the dashboard running on several devices each one
    /// pings independently; set this to `null` on the extras to avoid dupes.
    pub limit_reset_percent: Option<f32>,
    /// Alert when a session's terminal tab stops showing its status.
    ///
    /// The dashboard writes each row's status onto its terminal tab, and a
    /// Windows Terminal tab given a custom name ignores that write from then on —
    /// so a tab can sit on `🟢` while the agent is working, with nothing on screen
    /// admitting it. Nothing outside the terminal can undo that, so this reports
    /// rather than repairs. Follows the same lifecycle as the context-usage alert:
    /// sent once when the tab is confirmed stuck, and the message deleted once it
    /// follows again, the session vanishes, or this is turned off.
    ///
    /// How long the tab must have been stale before the message goes out. `null`
    /// or `0` disables the alert, so the value is also the switch — the same shape
    /// as `context_alert_percent` and `limit_reset_percent`, and no separate
    /// boolean to keep in step with it.
    ///
    /// The delay earns its place rather than being a rate limit: the row's `≠`
    /// badge appears the moment the tab is caught, so this is the window in which
    /// you can notice it on screen and reset the tab yourself before a phone
    /// buzzes about it — `reaction_window_ms`'s idea applied to a condition
    /// instead of a status. It runs from when the tab was *first* caught, and a
    /// repeat confirmation does not restart it.
    ///
    /// The default depends on how the binary was built ([`built_for_release`]):
    /// ten minutes in a build you deployed yourself, and **off** in the released
    /// installers. The alert reports a fault in another application that this one
    /// cannot repair, which is worth knowing about when you already understand the
    /// feature and is an unexplained buzz when you have just installed the thing.
    /// Setting the field explicitly wins either way.
    pub stale_tab_alert_ms: Option<u64>,
    /// Reading pace, in characters per second, used to defer a notification by
    /// how long the final assistant message takes to read. The reconciler adds
    /// `chars / reading_speed_cps` (capped, see `notifications::READING_CAP_MS`)
    /// to *both* the AFK and reaction windows, so a present user reading a long
    /// answer isn't pinged mid-read while a one-line "push?" still fires at the
    /// base delay. `null` or `0` disables the scaling (fixed windows, the
    /// pre-feature behavior). Read by `notifications::reconcile`.
    pub reading_speed_cps: Option<u64>,
}

/// The stale-tab alert's delay wherever it is on by default.
const STALE_TAB_ALERT_MS: u64 = 600_000;

/// Whether this binary came out of the release workflow — the build people
/// download — rather than one built and deployed by hand.
///
/// The flag is baked in by `build.rs` from an environment variable that workflow
/// sets and nothing else does, so this needs no runtime probe and cannot be wrong
/// about the machine it happens to be running on. Its only job is to let a default
/// be useful to someone who built the thing without being a surprise to someone
/// who just installed it; every setting stays explicitly settable either way.
fn built_for_release() -> bool {
    env!("CCDASH_RELEASE_BUILD") == "1"
}

impl Default for TelegramConfig {
    fn default() -> Self {
        Self {
            bot_token: None,
            chat_id: None,
            states: [
                // done: informational — only ping if the user was away when it
                // finished and never came back (AFK-only, no backstop).
                ("done".to_string(), StateNotify { afk_window_ms: Some(60_000), reaction_window_ms: None }),
                // blocked / error: actionable — ping early if away, but also
                // after the reaction window regardless of presence.
                ("blocked".to_string(), StateNotify { afk_window_ms: Some(60_000), reaction_window_ms: Some(120_000) }),
                ("error".to_string(), StateNotify { afk_window_ms: Some(60_000), reaction_window_ms: Some(60_000) }),
            ]
            .into_iter()
            .collect(),
            context_alert_percent: Some(80.0),
            limit_reset_percent: Some(90.0),
            stale_tab_alert_ms: (!built_for_release()).then_some(STALE_TAB_ALERT_MS),
            reading_speed_cps: Some(10),
        }
    }
}

/// Persisted window geometry. `width` / `height` are optional so configs
/// written by older builds (which only stored x/y) keep deserializing.
/// Stored in physical pixels; restoration uses `PhysicalPosition` /
/// `PhysicalSize` so the same monitor reproduces the same window — DPR
/// differences across monitors are an accepted edge case.
#[derive(Clone, Copy, Debug, Serialize, Deserialize)]
pub struct WindowPosition {
    pub x: i32,
    pub y: i32,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub width: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub height: Option<u32>,
}

/// The three usage-severity colors, shared by every usage number in the
/// widget. Per-field `#[serde(default)]` so a partial `{"green": "..."}`
/// override keeps the other two at their defaults.
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(default)]
pub struct UsageColors {
    pub green: String,
    pub amber: String,
    pub red: String,
}

impl Default for UsageColors {
    fn default() -> Self {
        // The tray icon's palette — bright enough to read as a ~16px digit and
        // shared with the in-app bars/tokens so everything agrees.
        Self {
            green: "#5ad278".into(),
            amber: "#f0c846".into(),
            red: "#ff5a5a".into(),
        }
    }
}

impl Default for Config {
    fn default() -> Self {
        Self {
            server_port: 9077,
            always_on_top: true,
            save_window_position: true,
            window_position: None,
            // Keys are matched by longest prefix (see notifications::window_for),
            // so family-level entries cover every model without per-release
            // updates; exact model ids can still be added to override a family.
            // The Claude 5 generation (opus/sonnet/fable) ships a 1M window as
            // the default, not a beta opt-in like the 4.x generation's 1M —
            // "claude-sonnet"/"claude-fable" alone would wrongly also catch
            // 4.x sonnet models, which stay on the 200k default window.
            context_window_tokens: [
                ("claude-opus".to_string(), 1_000_000),
                ("claude-sonnet-5".to_string(), 1_000_000),
                ("claude-fable".to_string(), 1_000_000),
                ("claude".to_string(), 200_000),
            ]
            .into_iter()
            .collect(),
            usage_colors: UsageColors::default(),
            token_gradient: false,
            benign_closers: vec!["What's next?".into(), "or are you good?".into(), "or leave it?".into(), "or leave it parked?".into(), "or leave that to you?".into(), "or are you set to check it yourself?".into(), "what would you like to work on?".into(), "what would you like to work on next?".into()],
            benign_openers: vec!["anything".into()],
            projects_root: None,
            notifications: Some(NotificationsConfig::default()),
            usage_limits_poll_interval_seconds: 600,
            usage_reset_poll_delay_seconds: 30,
            limit_bar_segments: 16,
            auto_resize: AutoResize::None,
            history_font_size: HistoryFontSize::Regular,
            intensity_unit: IntensityUnit::Percent,
            history_window_position: None,
            history_window_maximized: false,
            start_minimized: false,
            autostart: None,
            continuation_prompts: ["go", "continue", "proceed", "yes", "y", "yeah", "yep", "yup", "ok", "okay", "sure", "go ahead", "do it"].iter().map(|s| s.to_string()).collect(),
            terminal_titles: true,
            terminal_title_context_percent: Some(50.0),
            intensity_axis_max_tokens: Some(crate::token_history::DEFAULT_AXIS_MAX_TOKENS),
            detect_cancelled_turns: true,
            reap_exited_sessions: true,
            attention_tracking: true,
            restore_sessions: true,
            waiting_settle_ms: Some(600_000),
            sync: SyncConfig::default(),
            tray_badge: TrayBadge::None,
            tray_context_alert_enabled: true,
            tray_context_alert_percent: Some(80.0),
            high_alert: false,
            instruction_canary_enabled: false,
            compact_mode: false,
            lid_awake_mode: LidAwakeMode::Off,
            lid_awake_minutes: 15,
            lid_awake_release_grace_ms: 60_000,
            lid_awake_battery_floor_pct: 20,
        }
    }
}

impl Config {
    pub fn load_or_default(path: &Path) -> Self {
        match std::fs::read_to_string(path) {
            Ok(s) => serde_json::from_str(&s).unwrap_or_else(|e| {
                eprintln!("[config] failed to parse {path:?}: {e}; using defaults");
                Config::default()
            }),
            Err(_) => Config::default(),
        }
    }

    pub fn save(&self, path: &Path) -> std::io::Result<()> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let json = serde_json::to_string_pretty(self)
            .unwrap_or_else(|_| "{}".to_string());
        std::fs::write(path, json)
    }
}

pub struct ConfigState {
    pub config: Mutex<Config>,
    pub path: PathBuf,
}

impl ConfigState {
    pub fn new(path: PathBuf) -> Self {
        let config = Config::load_or_default(&path);
        Self {
            config: Mutex::new(config),
            path,
        }
    }

    pub fn snapshot(&self) -> Config {
        self.config.lock().unwrap().clone()
    }

    pub fn with_mut<F, R>(&self, f: F) -> R
    where
        F: FnOnce(&mut Config) -> R,
    {
        let mut guard = self.config.lock().unwrap();
        f(&mut guard)
    }

    pub fn save_to_disk(&self) -> std::io::Result<()> {
        let snapshot = self.config.lock().unwrap().clone();
        snapshot.save(&self.path)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn partial_json_with_only_telegram_creds_backfills_everything_else() {
        // This is the shape of a typical `config/local.json` override —
        // schema evolution must keep this working.
        let partial = r#"{
            "notifications": {
                "telegram": {
                    "bot_token": "t",
                    "chat_id": "c"
                }
            }
        }"#;
        let cfg: Config = serde_json::from_str(partial).expect("partial parse");
        assert_eq!(cfg.server_port, 9077, "default server_port survives");
        assert!(cfg.always_on_top, "default always_on_top survives");
        assert!(
            !cfg.context_window_tokens.is_empty(),
            "default context_window_tokens survives"
        );
        assert!(cfg.detect_cancelled_turns, "default detect_cancelled_turns survives");
        let tg = cfg
            .notifications
            .as_ref()
            .and_then(|n| n.telegram.as_ref())
            .expect("telegram block");
        assert_eq!(tg.bot_token.as_deref(), Some("t"));
        assert_eq!(tg.chat_id.as_deref(), Some("c"));
        assert_eq!(
            tg.states.get("blocked").and_then(|s| s.reaction_window_ms),
            Some(120_000),
            "default state rules survive when caller only supplies creds"
        );
        assert_eq!(tg.states.get("blocked").and_then(|s| s.afk_window_ms), Some(60_000));
        assert_eq!(tg.states.get("error").and_then(|s| s.reaction_window_ms), Some(60_000));
        assert_eq!(
            tg.states.get("done").map(|s| (s.afk_window_ms, s.reaction_window_ms)),
            Some((Some(60_000), None)),
            "done is AFK-only with no backstop"
        );
        assert_eq!(
            tg.context_alert_percent,
            Some(80.0),
            "default context_alert_percent survives when caller only supplies creds"
        );
        assert_eq!(
            tg.reading_speed_cps,
            Some(10),
            "default reading_speed_cps survives when caller only supplies creds"
        );
        assert_eq!(
            tg.limit_reset_percent,
            Some(90.0),
            "default limit_reset_percent survives when caller only supplies creds"
        );
    }

    #[test]
    fn reading_speed_cps_defaults_can_be_overridden_and_disabled() {
        let cfg: Config = serde_json::from_str("{}").unwrap();
        assert_eq!(
            cfg.notifications.unwrap().telegram.unwrap().reading_speed_cps,
            Some(10),
            "default reading_speed_cps survives an empty config"
        );
        let set: Config = serde_json::from_str(
            r#"{ "notifications": { "telegram": { "reading_speed_cps": 20 } } }"#,
        )
        .unwrap();
        assert_eq!(set.notifications.unwrap().telegram.unwrap().reading_speed_cps, Some(20));
        // null disables the scaling (fixed windows).
        let off: Config = serde_json::from_str(
            r#"{ "notifications": { "telegram": { "reading_speed_cps": null } } }"#,
        )
        .unwrap();
        assert_eq!(off.notifications.unwrap().telegram.unwrap().reading_speed_cps, None);
    }

    #[test]
    fn context_alert_percent_can_be_overridden_and_disabled() {
        let set: Config = serde_json::from_str(
            r#"{ "notifications": { "telegram": { "context_alert_percent": 70 } } }"#,
        )
        .unwrap();
        let tg = set.notifications.unwrap().telegram.unwrap();
        assert_eq!(tg.context_alert_percent, Some(70.0));

        let off: Config = serde_json::from_str(
            r#"{ "notifications": { "telegram": { "context_alert_percent": null } } }"#,
        )
        .unwrap();
        let tg = off.notifications.unwrap().telegram.unwrap();
        assert_eq!(tg.context_alert_percent, None);
    }

    #[test]
    fn limit_reset_percent_defaults_and_can_be_overridden_and_disabled() {
        let cfg: Config = serde_json::from_str("{}").unwrap();
        assert_eq!(
            cfg.notifications.unwrap().telegram.unwrap().limit_reset_percent,
            Some(90.0),
            "default limit_reset_percent survives an empty config"
        );
        let set: Config = serde_json::from_str(
            r#"{ "notifications": { "telegram": { "limit_reset_percent": 75 } } }"#,
        )
        .unwrap();
        assert_eq!(set.notifications.unwrap().telegram.unwrap().limit_reset_percent, Some(75.0));
        // null disables it.
        let off: Config = serde_json::from_str(
            r#"{ "notifications": { "telegram": { "limit_reset_percent": null } } }"#,
        )
        .unwrap();
        assert_eq!(off.notifications.unwrap().telegram.unwrap().limit_reset_percent, None);
    }

    #[test]
    fn states_parse_with_independent_optional_windows() {
        // A user override supplying only some states / only some windows; each
        // window stays None when absent (no AFK / no backstop respectively).
        let json = r#"{ "notifications": { "telegram": { "states": {
            "done": { "afk_window_ms": 30000 },
            "blocked": { "reaction_window_ms": 90000 }
        } } } }"#;
        let cfg: Config = serde_json::from_str(json).unwrap();
        let tg = cfg.notifications.unwrap().telegram.unwrap();
        let done = tg.states.get("done").unwrap();
        assert_eq!((done.afk_window_ms, done.reaction_window_ms), (Some(30_000), None));
        let blocked = tg.states.get("blocked").unwrap();
        assert_eq!((blocked.afk_window_ms, blocked.reaction_window_ms), (None, Some(90_000)));
        // Supplying `states` at all replaces the default map wholesale.
        assert!(tg.states.get("error").is_none());
    }

    #[test]
    fn empty_json_object_gives_full_defaults() {
        let cfg: Config = serde_json::from_str("{}").unwrap();
        let tg = cfg.notifications.unwrap().telegram.unwrap();
        assert!(tg.bot_token.is_none());
        assert_eq!(tg.states.get("blocked").and_then(|s| s.reaction_window_ms), Some(120_000));
    }

    #[test]
    fn auto_resize_defaults_to_none_when_field_missing() {
        let cfg: Config = serde_json::from_str("{}").unwrap();
        assert_eq!(cfg.auto_resize, AutoResize::None);
    }

    #[test]
    fn auto_resize_parses_snake_case() {
        let up: Config = serde_json::from_str(r#"{ "auto_resize": "up" }"#).unwrap();
        assert_eq!(up.auto_resize, AutoResize::Up);
        let down: Config = serde_json::from_str(r#"{ "auto_resize": "down" }"#).unwrap();
        assert_eq!(down.auto_resize, AutoResize::Down);
    }

    #[test]
    fn unknown_fields_are_silently_ignored_so_renames_are_survivable() {
        let with_extra = r#"{ "this_key_does_not_exist_on_config": 42 }"#;
        let cfg: Config = serde_json::from_str(with_extra).unwrap();
        assert_eq!(cfg.server_port, 9077);
    }

    #[test]
    fn history_window_maximized_defaults_false_and_parses() {
        let cfg: Config = serde_json::from_str("{}").unwrap();
        assert!(!cfg.history_window_maximized);
        let on: Config = serde_json::from_str(r#"{ "history_window_maximized": true }"#).unwrap();
        assert!(on.history_window_maximized);
    }

    #[test]
    fn the_stale_tab_alert_default_follows_the_build_channel() {
        // On in a build you deployed yourself, off in the one people download.
        // The alert reports a fault in another application that this one cannot
        // repair: worth knowing when you already understand the feature, an
        // unexplained buzz when you have just installed the thing. Pins the
        // polarity, so flipping it has to be deliberate.
        let expected = if built_for_release() { None } else { Some(STALE_TAB_ALERT_MS) };
        assert_eq!(TelegramConfig::default().stale_tab_alert_ms, expected);
    }

    #[test]
    fn an_explicit_stale_tab_delay_wins_in_either_build() {
        // Whatever the build decided, a config file that states a value is the
        // answer — including stating `null` to turn the alert off.
        let set: TelegramConfig = serde_json::from_str(r#"{ "stale_tab_alert_ms": 1234 }"#).unwrap();
        assert_eq!(set.stale_tab_alert_ms, Some(1234));
        let off: TelegramConfig = serde_json::from_str(r#"{ "stale_tab_alert_ms": null }"#).unwrap();
        assert_eq!(off.stale_tab_alert_ms, None);
    }

    #[test]
    fn restore_sessions_defaults_on_when_field_missing() {
        let cfg: Config = serde_json::from_str("{}").unwrap();
        assert!(cfg.restore_sessions, "a config written before the field existed still restores");
        let off: Config = serde_json::from_str(r#"{ "restore_sessions": false }"#).unwrap();
        assert!(!off.restore_sessions);
    }

    #[test]
    fn terminal_titles_defaults_on_when_field_missing() {
        let cfg: Config = serde_json::from_str("{}").unwrap();
        assert!(cfg.terminal_titles);
        let off: Config = serde_json::from_str(r#"{ "terminal_titles": false }"#).unwrap();
        assert!(!off.terminal_titles);
    }

    #[test]
    fn terminal_title_context_percent_defaults_and_parses() {
        let cfg: Config = serde_json::from_str("{}").unwrap();
        assert_eq!(cfg.terminal_title_context_percent, Some(50.0), "default survives an empty config");
        let set: Config = serde_json::from_str(r#"{ "terminal_title_context_percent": 70 }"#).unwrap();
        assert_eq!(set.terminal_title_context_percent, Some(70.0));
        let off: Config = serde_json::from_str(r#"{ "terminal_title_context_percent": null }"#).unwrap();
        assert_eq!(off.terminal_title_context_percent, None);
    }

    #[test]
    fn waiting_settle_ms_defaults_and_parses() {
        let cfg: Config = serde_json::from_str("{}").unwrap();
        assert_eq!(cfg.waiting_settle_ms, Some(600_000), "default 10-min window survives an empty config");
        let set: Config = serde_json::from_str(r#"{ "waiting_settle_ms": 900000 }"#).unwrap();
        assert_eq!(set.waiting_settle_ms, Some(900_000));
        let off: Config = serde_json::from_str(r#"{ "waiting_settle_ms": null }"#).unwrap();
        assert_eq!(off.waiting_settle_ms, None);
    }

    #[test]
    fn usage_reset_poll_delay_defaults_and_parses() {
        let cfg: Config = serde_json::from_str("{}").unwrap();
        assert_eq!(cfg.usage_reset_poll_delay_seconds, 30, "default 30s post-reset delay survives an empty config");
        let set: Config = serde_json::from_str(r#"{ "usage_reset_poll_delay_seconds": 90 }"#).unwrap();
        assert_eq!(set.usage_reset_poll_delay_seconds, 90);
    }

    #[test]
    fn sync_defaults_to_disabled() {
        let cfg: Config = serde_json::from_str("{}").unwrap();
        assert!(!cfg.sync.listen);
        assert!(cfg.sync.peers.is_empty());
        assert!(cfg.sync.token.is_none());
        assert_eq!(cfg.sync.listen_port, 9078);
        assert_eq!(cfg.sync.device_name, "", "resolved at startup, not here");
    }

    #[test]
    fn sync_bind_scope_defaults_to_tailnet() {
        let cfg: Config = serde_json::from_str("{}").unwrap();
        assert_eq!(cfg.sync.bind_scope, SyncBindScope::Tailnet, "narrow by default; `any` is opt-in");
        let opened: Config = serde_json::from_str(r#"{ "sync": { "bind_scope": "any" } }"#).unwrap();
        assert_eq!(opened.sync.bind_scope, SyncBindScope::Any);
    }

    #[test]
    fn partial_sync_block_backfills_the_rest() {
        let partial = r#"{ "sync": { "peers": ["http://laptop:9078"], "token": "s3cret" } }"#;
        let cfg: Config = serde_json::from_str(partial).unwrap();
        assert_eq!(cfg.sync.peers, vec!["http://laptop:9078".to_string()]);
        assert_eq!(cfg.sync.token.as_deref(), Some("s3cret"));
        assert!(!cfg.sync.listen, "default listen survives");
        assert_eq!(cfg.sync.listen_port, 9078, "default port survives");
        // A config.json written before bind_scope existed — deploy overwrites
        // config.json, so an in-place upgrade must not fail to parse.
        assert_eq!(cfg.sync.bind_scope, SyncBindScope::Tailnet, "a sync block without the key takes the narrow default");
        // unrelated defaults still survive
        assert_eq!(cfg.server_port, 9077);
    }

    #[test]
    fn continuation_prompts_default_includes_common_phrases() {
        let cfg = Config::default();
        assert!(cfg.continuation_prompts.iter().any(|s| s == "go"));
        assert!(cfg.continuation_prompts.iter().any(|s| s == "continue"));
        assert!(cfg.continuation_prompts.iter().any(|s| s == "proceed"));
    }

    #[test]
    fn continuation_prompts_default_includes_short_affirmations() {
        // Approval replies like "y"/"yes" arrive when the agent's closing
        // question wasn't detected (row sits at Done, not Blocked); without
        // these in the default list they'd clobber original_prompt and the row
        // would show "y" as the task. Regression guard for that recurring bug.
        let cfg = Config::default();
        for phrase in ["y", "yes", "yeah", "yep", "yup", "ok", "okay", "sure"] {
            assert!(
                cfg.continuation_prompts.iter().any(|s| s == phrase),
                "default continuation_prompts must include the affirmation {phrase:?}"
            );
        }
    }

    #[test]
    fn tray_badge_defaults_to_none_and_parses_snake_case() {
        let cfg: Config = serde_json::from_str("{}").unwrap();
        assert_eq!(cfg.tray_badge, TrayBadge::None);
        let light: Config = serde_json::from_str(r#"{ "tray_badge": "five_hour_light" }"#).unwrap();
        assert_eq!(light.tray_badge, TrayBadge::FiveHourLight);
        let num: Config = serde_json::from_str(r#"{ "tray_badge": "seven_day_number" }"#).unwrap();
        assert_eq!(num.tray_badge, TrayBadge::SevenDayNumber);
    }

    #[test]
    fn tray_context_alert_percent_defaults_and_parses() {
        let cfg: Config = serde_json::from_str("{}").unwrap();
        assert_eq!(cfg.tray_context_alert_percent, Some(80.0), "default survives an empty config");
        let set: Config = serde_json::from_str(r#"{ "tray_context_alert_percent": 70 }"#).unwrap();
        assert_eq!(set.tray_context_alert_percent, Some(70.0));
        let off: Config = serde_json::from_str(r#"{ "tray_context_alert_percent": null }"#).unwrap();
        assert_eq!(off.tray_context_alert_percent, None);
    }

    #[test]
    fn high_alert_defaults_off_and_parses() {
        let cfg: Config = serde_json::from_str("{}").unwrap();
        assert!(!cfg.high_alert, "off by default");
        let on: Config = serde_json::from_str(r#"{ "high_alert": true }"#).unwrap();
        assert!(on.high_alert);
    }

    #[test]
    fn compact_mode_defaults_off_and_parses() {
        let cfg: Config = serde_json::from_str("{}").unwrap();
        assert!(!cfg.compact_mode, "off by default");
        let on: Config = serde_json::from_str(r#"{ "compact_mode": true }"#).unwrap();
        assert!(on.compact_mode);
    }

    #[test]
    fn instruction_canary_defaults_off() {
        let cfg: Config = serde_json::from_str("{}").unwrap();
        assert!(!cfg.instruction_canary_enabled, "off by default");
        let on: Config = serde_json::from_str(r#"{ "instruction_canary_enabled": true }"#).unwrap();
        assert!(on.instruction_canary_enabled);
    }

    #[test]
    fn tray_context_alert_enabled_defaults_on_and_parses() {
        let cfg: Config = serde_json::from_str("{}").unwrap();
        assert!(cfg.tray_context_alert_enabled, "checked by default");
        let off: Config = serde_json::from_str(r#"{ "tray_context_alert_enabled": false }"#).unwrap();
        assert!(!off.tray_context_alert_enabled);
    }

    #[test]
    fn lid_awake_defaults_off_with_a_bounded_window() {
        let cfg = Config::default();
        assert_eq!(cfg.lid_awake_mode, LidAwakeMode::Off, "privileged feature stays opt-in");
        assert_eq!(cfg.lid_awake_minutes, 15);
        assert_eq!(cfg.lid_awake_release_grace_ms, 60_000);
        assert_eq!(cfg.lid_awake_battery_floor_pct, 20);
    }

    #[test]
    fn lid_awake_mode_parses_and_classifies() {
        let on_batt: Config = serde_json::from_str(r#"{ "lid_awake_mode": "on_battery" }"#).unwrap();
        assert_eq!(on_batt.lid_awake_mode, LidAwakeMode::OnBattery);
        assert_eq!(serde_json::to_string(&LidAwakeMode::Always).unwrap(), r#""always""#);
        // An unknown value (e.g. the retired "manual") must not wedge the whole
        // config — `load_or_default` falls back rather than losing every setting.
        assert!(serde_json::from_str::<Config>(r#"{ "lid_awake_mode": "manual" }"#).is_err());
    }

    #[test]
    fn tray_badge_legacy_values_map_to_numeric_modes() {
        let five: Config = serde_json::from_str(r#"{ "tray_badge": "five_hour" }"#).unwrap();
        assert_eq!(five.tray_badge, TrayBadge::FiveHourNumber);
        let seven: Config = serde_json::from_str(r#"{ "tray_badge": "seven_day" }"#).unwrap();
        assert_eq!(seven.tray_badge, TrayBadge::SevenDayNumber);
    }

    #[test]
    fn continuation_prompts_can_be_overridden_by_partial_json() {
        let partial = r#"{ "continuation_prompts": ["yes", "go ahead"] }"#;
        let cfg: Config = serde_json::from_str(partial).unwrap();
        assert_eq!(cfg.continuation_prompts, vec!["yes".to_string(), "go ahead".to_string()]);
        // unrelated defaults still survive
        assert_eq!(cfg.server_port, 9077);
    }
}

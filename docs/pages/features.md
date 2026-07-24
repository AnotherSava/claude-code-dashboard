---
layout: default
title: Features
parent: Home
nav_order: 2
---

A compact desktop widget that helps you keep an eye on your Claude Code sessions.

## Session identity

Each Claude Code session becomes one row. The row's `id` is *initially* derived from the working directory of the session's **first** event — if `cwd` sits under the configured `projects_root`, the relative path becomes the id with slashes, dashes, and underscores replaced by spaces. The id is then locked to the session, even if the agent `cd`s into a different folder mid-conversation.

**Renaming a session.** Double-click a row's name to edit it — Enter saves, Esc cancels. The custom name is persisted, so a later Claude session in the same directory shows the same name.

## Live status

The row's status badge tracks the agent in real time:

- **WORK** — Claude is working on your task. Timer accumulates total time spent working on the same prompt across approval cycles.
- **WAIT** — the main turn finished but background work Claude started (a subagent, or a background command like a dev server) is still running, so the row stays active (light-blue) rather than dropping to DONE while work continues. If you stop that work yourself instead of letting it finish, the row settles to DONE on its own after a while rather than staying stuck.
- **BLOCK** — Claude is blocked on you. The row shows the agent's current question or permission request.
- **IDLE** — the session is alive but not actively working. A task you cancel with Esc usually settles here too: cancelling sends no event of its own, but the dashboard notices the turn ended and settles the row back on its own — to idle, or back to a question it was waiting on (on by default, see [Settings](settings#behavior)).
- **DONE** — Claude finished the task and isn't waiting on you. Timer shows time since it finished.
- **ERROR** — the hook reported an error; the row shows the error text.

Each badge is color-coded, and BLOCK and ERROR pulse to draw your eye when a session needs attention.

**Rows clear when a session ends.** Clearing a session (`/clear`) or quitting it — typing `exit`, pressing Ctrl-D, or closing the terminal — removes its row, so the dashboard only shows sessions that are still around. Quitting doesn't always announce itself, so such a row could otherwise linger (often stuck on WORK if you quit mid-turn); the dashboard drops it once it confirms the session is gone. Reopen the same project and the row returns with its history. On by default — see [Settings](settings#behavior).

## Color terminal tabs

Each session's status is mirrored onto the terminal tab it runs in, as a colored circle next to the session name — 🔵 working (also background-agent WAIT), 🟠 blocked on you, 🟢 done, 🔴 error. A glance at your terminal tabs shows which session needs attention, even without the widget on screen. The title updates the moment the status changes and clears when the session ends. On by default; the tray's **Color terminal tabs** toggle turns it off.

Once a session's context usage climbs past a threshold (50% by default), the tab title also shows it — `🔵 printlab [67%]` — so a tab that's filling toward a `/compact` stands out among the rest. The number falls off again when a new task or `/clear` frees the context. See [Settings](settings#color-terminal-tabs) to change the threshold or turn the number off.

## Focus on the task

While Claude is blocked on you (BLOCK), the row shows the question or approval request, so you know what it needs. Once you answer and Claude resumes (WORK), the row goes back to showing your **original request** rather than the *yes* you typed — so a quick approval or a *continue* never replaces your task on screen. The work timer pauses during BLOCK — replaced by a timer counting how long Claude has been blocked on you — and resumes once the agent continues working on the task. A new top-level prompt after DONE / IDLE starts a fresh task.

For the full state machine and the rules that pick between the current text and the original request, see [Sticky labels](development/sticky-labels) in the Development section.

## Tracking the conversation flow

The dashboard doesn't just relay raw events — it reads the conversation to keep each row's status and the text it shows accurate. It tells a genuine question apart from a rhetorical sign-off, so a closing *"What's next?"* doesn't flip a finished session into WAIT. It recognizes permission and plan-approval prompts as blocked states. It treats short replies like *"continue"* as resuming the current task rather than starting a new one. And it cleans up Claude's formatting so the text reads cleanly.

Several of these rules are tunable — see [Settings](settings) — and the full ruleset is documented under [Classification](development/classification).

## Instruction adherence

Over a long conversation an agent can gradually stop honoring its standing instructions. As an early-warning tripwire, the dashboard can hand each session a private one-time token when it starts and ask the agent to end every reply with it. Each time the agent finishes, the dashboard checks that the token is there — if it stays missing, the row is flagged with a ⚠ (on the dashboard, on the terminal tab, and as a Telegram ping), a cue to look closely at that session's output before trusting it, and perhaps to compact or re-anchor the conversation. The flag clears itself as soon as the agent's next reply carries the token again. The agent's name is also tinted by its canary status — green once it's confirmed following, amber while still unconfirmed, red if it's drifted. The token appears as a small, unobtrusive tag at the end of each reply in the agent's terminal; the dashboard strips it from its own history and notifications, so it never clutters what you read there. Off by default; see [Settings](settings#instruction-adherence).

## Context usage

The row shows the session's live context usage, updated as Claude works. The count is colored green → amber → red as it climbs toward the model's context window, so you can tell at a glance whether `/compact` is due.

## History window

Hover a session row for a quick tooltip listing its task prompts so far — one per line, with the current task marked. For the whole story, click the text below a session's name to open a History window — a chronological recap of your prompts and Claude's reply to each, with a separator marking the start of a new session. Useful for scrolling back through a long-running conversation without leaving the dashboard. The window opens maximized on the dashboard's screen; with **Save window position** enabled it reopens where you last left it.

Ctrl+`+` and Ctrl+`-` cycle through five font sizes; Esc closes the window. The choice persists to `config.json`.

## Notifications

Get pinged when a session needs you — for example, when it finishes or sits waiting while you're away from your machine. The widget watches your keyboard and mouse, so a session that finishes while you're sitting right there stays quiet (you already saw it), and you only get a ping once you've stepped away. For things you need to act on, like a pending question, it also pings after a set time even if you're present, so nothing waits on you forever. The delay scales with how much there is to read: reacting to a one-line "push?" is quick, but a page-full answer takes a while to get through, and to your machine reading it looks the same as being away — you're not touching the keyboard either way. So the widget waits longer before pinging when the agent's last message is long, giving you time to finish reading before it decides you've missed it. Once the agent moves on — you answer the prompt and it resumes work — the message is deleted automatically, so your Telegram chat shows only the sessions still waiting on you. You can also get a heads-up when a session's context fills past a percentage you choose, so a long run doesn't quietly run out of room — and like the other alerts, that message clears itself once the context usage drops back down (a new task or `/clear`). And when you've burned through your 5-hour or 7-day usage limit, the widget can ping you the moment that window resets, so you know you're clear to pick back up without watching the bars — it only fires when you'd actually run the window most of the way down, not on every routine reset. Notifications are delivered via Telegram and stay off until you configure them. See [Settings](settings) for setup.

When you want nothing held back — you're watching for anything at all — the tray's **High alert** toggle sends every notification the moment it happens, skipping the away-detection and reading delays entirely. It applies to the session states (finished, blocked, error) and leaves the context-usage and usage-limit pings on their own schedule.

## Usage limits

The header shows two bars tracking your Anthropic usage against the rolling 5-hour and 7-day rate limits, so you can see how much headroom is left before you hit a cap.

You can also surface a limit right on the tray icon, via the tray's **Tray usage badge** submenu — for the 5-hour or 7-day bucket, in one of two styles: **lights** recolor the dashboard's traffic-light icon, its three lamps stepping from green through amber to red as the bucket fills; or **number** shows the percentage itself, switching to the all-red light at 100%. Either way the icon's hover tooltip shows both figures. Off by default.

When the badge is on, the tray icon also flags the moment any session's context usage crosses a threshold you set — an at-a-glance warning that an agent is filling its context, right in the tray. The light styles gain a red border; the number style draws the digits over a red background. It's on by default — the tray's **Show high context usage** checkbox turns it off. See [Settings](settings) for the threshold.

## Work intensity

A separate window — opened from the tray's **Work intensity** item — charts how hard your agents have been working over time, drawn from the same 5-hour usage data the limit bars track. Each bar covers a short slice of time and grows taller and warmer the more of the 5-hour limit was burned in it, with a reference line marking the pace that would use up the whole limit in five hours straight; anything past twice that pace is flagged red. A **Days** view lays out one week as seven rows, one per day; a **Weeks** view gives one row per week and scrolls back through your history. Each view also totals the active time and how much of the quota it used. With [multi-device sync](#multi-device-sync) configured, the chart also fills any gaps from your other devices — so the windows when this machine's app was closed still show the work done on the account elsewhere.

## Multi-device sync

Run the dashboard on more than one computer and each one can show the sessions from all of them. Sessions from another device appear in the same list with a small badge carrying that device's name, with everything a local row has — live status, the task in view, the context usage, and the History window recap. Renaming a remote row changes the name on the device where you renamed it, while alerts for a session fire only on the device it runs on, so you never get the same ping twice. When a device goes offline, its rows disappear from the other dashboards shortly after. Each device also shares its 5-hour usage history, which the others fold into their [Work intensity](#work-intensity) chart to fill the stretches their own app wasn't running.

The devices need to reach each other over the network — the simplest way across different networks is a VPN like [Tailscale](https://tailscale.com/). Sync stays off until you configure it; see [Settings → multi-device sync](settings#multi-device-sync).

## Configuration

The common toggles — always on top, save position on exit, start with the system, history font size, and more — are right-click items in the tray menu. They're backed by a `config.json` file in the app data directory, which the widget reloads as soon as you save it (no restart needed, except for the server port). The file also holds settings that aren't in the tray, like color thresholds, notification options, and conversation-parsing tweaks.

See [Settings](settings) for the tray menu and the full `config.json` reference.

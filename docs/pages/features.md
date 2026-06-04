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
- **WAIT** — Claude is blocked on you. The row shows the agent's current question or permission request.
- **IDLE** — the session is alive but not actively working.
- **DONE** — Claude finished the task and isn't waiting on you. Timer shows time since it finished.
- **ERROR** — the hook reported an error; the row shows the error text.

Each badge is color-coded, and WAIT and ERROR pulse to draw your eye when a session needs attention.

## Terminal tab titles

Each session's status is mirrored onto the terminal tab it runs in, as a colored circle next to the session name — 🔵 working, 🟠 waiting on you, 🟢 done, 🔴 error. A glance at your terminal tabs shows which session needs attention, even without the widget on screen. The title updates the moment the status changes and clears when the session ends. On by default; the tray's **Terminal tab titles** toggle turns it off. Windows only for now.

## Your task stays in view

While Claude is blocked on you (WAIT), the row shows the question or approval request, so you know what it needs. Once you answer and Claude resumes (WORK), the row goes back to showing your **original request** rather than the *yes* you typed — so a quick approval or a *continue* never replaces your task on screen. The work timer pauses during WAIT — replaced by a timer counting how long Claude has been blocked on you — and resumes once the agent continues working on the task. A new top-level prompt after DONE / IDLE starts a fresh task.

For the full state machine and the rules that pick between the current text and the original request, see [Sticky labels](development/sticky-labels) in the Development section.

## Tracking the conversation flow

The dashboard doesn't just relay raw events — it reads the conversation to keep each row's status and the text it shows accurate. It tells a genuine question apart from a rhetorical sign-off, so a closing *"What's next?"* doesn't flip a finished session into WAIT. It recognizes permission and plan-approval prompts as blocked states. It treats short replies like *"continue"* as resuming the current task rather than starting a new one. And it cleans up Claude's formatting so the text reads cleanly.

Several of these rules are tunable — see [Settings](settings) — and the full ruleset is documented under [Classification](development/classification).

## Live token count

The row shows the session's live context usage, updated as Claude works. The count is colored green → amber → red as it climbs toward the model's context window, so you can tell at a glance whether `/compact` is due.

## History window

Hover a session row for a quick tooltip listing its task prompts so far — one per line, with the current task marked. For the full conversation, click the text below a session's name to open a History window — a chronological view of every user prompt, assistant reply, and a separator marking the start of a new session. Useful for scrolling back through a long-running conversation without leaving the dashboard. The window opens maximized on the dashboard's screen; with **Save window position** enabled it reopens where you last left it.

Ctrl+`+` and Ctrl+`-` cycle through five font sizes; Esc closes the window. The choice persists to `config.json`.

## Notifications

Get pinged when a session needs you — for example, when it sits in WAIT longer than a threshold you set. Once the agent moves on — you answer the prompt and it resumes work — the message is deleted automatically, so your Telegram chat shows only the sessions still waiting on you. You can also get a heads-up when a session's context fills past a percentage you choose, so a long run doesn't quietly run out of room. Notifications are delivered via Telegram and stay off until you configure them. See [Settings](settings) for setup and thresholds.

## Usage limits

The header shows two bars tracking your Anthropic usage against the rolling 5-hour and 7-day rate limits, so you can see how much headroom is left before you hit a cap.

## Configuration

The common toggles — always on top, save position on exit, start with the system, history font size, and more — are right-click items in the tray menu. They're backed by a `config.json` file in the app data directory, which the widget reloads as soon as you save it (no restart needed, except for the server port). The file also holds settings that aren't in the tray, like color thresholds, notification options, and conversation-parsing tweaks.

See [Settings](settings) for the tray menu and the full `config.json` reference.

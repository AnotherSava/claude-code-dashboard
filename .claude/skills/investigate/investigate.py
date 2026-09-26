#!/usr/bin/env python3
"""Explain why a dashboard agent is in its current state, from the decision log.

Reads `widget.jsonl` — the permanent decision log the Rust backend writes — and
reconstructs each agent's current status plus the chain of decisions that led
there. Every classification and state correction is logged with a `decision`
field and a human `reason` (including the matched question-rule and a text
snippet), so this never needs the transcript or the source.

Usage:
    python3 investigate.py                 # list active agents to choose from
    python3 investigate.py <agent-name>    # explain one agent
    python3 investigate.py --dir <path> <agent-name>   # override app data dir
"""

import json
import os
import sys
from collections import defaultdict, deque
from datetime import datetime

# The decision-log `reason` carries arbitrary assistant text — arrows, em
# dashes, ellipses, emoji — so force UTF-8 output regardless of the console
# codepage. A Windows console on a non-UTF-8 codepage (e.g. cp1251) otherwise
# crashes on the first non-encodable glyph, such as the '→' in a rename note.
# errors="replace" is a last-resort guard so a stray unencodable char degrades
# to '?' rather than a traceback that hides the answer.
for _stream in (sys.stdout, sys.stderr):
    if hasattr(_stream, "reconfigure"):
        try:
            _stream.reconfigure(encoding="utf-8", errors="replace")
        except Exception:
            pass

# Status -> the chip label the dashboard shows.
CHIP = {"Working": "WORK", "Waiting": "WAIT", "Blocked": "BLOCK", "Done": "DONE", "Idle": "IDLE", "Error": "ERROR"}

# Per-agent rolling window of decisions kept in memory (newest-biased).
TRAIL = 60


def app_dir(argv):
    if "--dir" in argv:
        i = argv.index("--dir")
        return argv[i + 1], argv[:i] + argv[i + 2:]
    if sys.platform == "win32":
        base = os.environ.get("APPDATA", "")
    elif sys.platform == "darwin":
        base = os.path.join(os.path.expanduser("~"), "Library", "Application Support")
    else:
        base = os.path.join(os.path.expanduser("~"), ".local", "share")
    return os.path.join(base, "com.anothersava.claude-code-dashboard"), argv


def agent_of(fields):
    return fields.get("chat_id") or fields.get("id")


def status_from(fields):
    """The status a decision line lands the row on, or None if it doesn't move it.

    Read on its own, without the lines around it. `replay` is what accounts for
    an open subagent prompt, under which these moves reach only the base."""
    d = fields.get("decision")
    if d == "subagent_prompt_open":
        return "Blocked"
    if d == "subagent_prompt_settled":
        return fields.get("status")
    if d == "subagent_prompt_unmatched":
        return None
    if d == "classify":
        return fields.get("status")
    if d == "apply_set":
        return fields.get("new_status")
    if d == "resume_working":
        return "Working"
    if d == "enter_waiting":
        return "Waiting"
    if d == "correct_to_blocked":
        return "Blocked"
    if d == "correct_to_done":
        return "Done"
    if d == "revert_cancelled":
        return fields.get("status")
    # A row put back after a restart bypasses `apply_set` entirely (it goes
    # through `AppState::restore_row`), so this line is the *only* thing that
    # establishes its status — without it a restored row is attributed to
    # whatever pre-restart line happens to still be in the trail.
    if d == "restore_row":
        return fields.get("status")
    if d == "settle_waiting":
        return "Done"
    if d == "session_clear":
        return "(cleared)"
    return None


def replay(trail):
    """Each line with the status it put on the *visible* row, or None.

    A subagent's permission prompt overlays the row: from `subagent_prompt_open`
    until a `subagent_prompt_settled` with `released: true`, the row shows BLOCK
    and the main agent's own transitions — its `classify` lines, and the
    `apply_set`/`resume_working`/`revert_cancelled` lines logged `gated: true` —
    move only the state underneath. Crediting one of those as the line that set
    the BLOCK would explain it with a reason that says the opposite. A settle
    that leaves other prompts open moves nothing either."""
    gated = False
    out = []
    for ts, fields in trail:
        d = fields.get("decision")
        s = status_from(fields)
        if d == "subagent_prompt_open":
            gated = True
        elif d == "subagent_prompt_settled":
            if fields.get("released") is True:
                gated = False
            else:
                s = None
        elif d in ("session_clear", "restore_row"):
            # The row is gone or rebuilt from scratch; either way no gate survives.
            gated = False
        elif gated or fields.get("gated") is True:
            gated = True
            s = None
        out.append((ts, fields, s))
    return out


def local_ts(stamp):
    """The log's UTC stamp in this machine's local time, to the second."""
    try:
        return datetime.fromisoformat(stamp.replace("Z", "+00:00")).astimezone().strftime("%Y-%m-%dT%H:%M:%S")
    except ValueError:
        return stamp[:19]


def load_trails(wpath):
    trails = defaultdict(lambda: deque(maxlen=TRAIL))
    with open(wpath, encoding="utf-8", errors="replace") as fh:
        for line in fh:
            if '"decision"' not in line:
                continue
            try:
                entry = json.loads(line)
            except ValueError:
                continue
            fields = entry.get("fields", {})
            if "decision" not in fields:
                continue
            agent = agent_of(fields)
            if not agent:
                continue
            trails[agent].append((local_ts(entry.get("timestamp", "")), fields))
    return trails


def current(trail):
    """Reconstruct (status, setter, revealed) by replaying the trail.

    `setter` is (ts, fields) of the most recent decision that established the
    status, preferring one that carries a `reason` (a `classify`/correction line)
    over the reason-less `apply_set` mirror that immediately follows it.

    When the status was uncovered by a subagent prompt's release, the release
    only revealed it: the setter is then the line that put that status under the
    prompt (the `Stop` that made the row WAIT, say), and `revealed` is the
    release line. Otherwise `revealed` is None."""
    lines = replay(trail)
    status = None
    for _, _, s in lines:
        if s is not None:
            status = s
    if status is None:
        return None, None, None

    def newest(match):
        last_any = last_reason = None
        for ts, fields, s in lines:
            if match(fields, s):
                last_any = (ts, fields)
                if fields.get("reason"):
                    last_reason = (ts, fields)
        return last_reason or last_any

    setter = newest(lambda fields, s: s == status)
    if setter and setter[1].get("decision") == "subagent_prompt_settled":
        before = lines[: next(i for i, (ts, f, _) in enumerate(lines) if f is setter[1])]
        underneath = None
        for ts, fields, _ in before:
            if status_from(fields) == status and fields.get("decision") != "subagent_prompt_open" and fields.get("reason"):
                underneath = (ts, fields)
        if underneath:
            return status, underneath, setter
    return status, setter, None


def chip(status):
    return CHIP.get(status, status or "?")


def list_mode(trails, aliases):
    rows = []
    for agent, trail in trails.items():
        status, setter, _ = current(trail)
        if status == "(cleared)":
            continue  # session ended — not on the dashboard
        last_ts = setter[0] if setter else (trail[-1][0] if trail else "")
        rows.append((last_ts, agent, status))
    rows.sort(reverse=True)
    print(f"{'last change (local)':21} {'agent':26} {'state':6} display-name")
    print("-" * 70)
    for ts, agent, status in rows:
        disp = aliases.get(agent, "")
        print(f"{ts:21} {agent:26} {chip(status):6} {disp}")
    if not rows:
        print("(no active agents in the decision log)")


def resolve(name, trails, aliases):
    if name in trails:
        return name
    rev = {v: k for k, v in aliases.items()}  # display-name -> chat_id
    if name in rev and rev[name] in trails:
        return rev[name]
    matches = [a for a in trails if name.lower() in a.lower()]
    return matches[0] if len(matches) == 1 else None


def explain(agent, trails, aliases):
    trail = trails[agent]
    status, setter, revealed = current(trail)
    disp = aliases.get(agent)
    title = agent + (f"  (display: {disp})" if disp else "")
    print(f"Agent:          {title}")
    print(f"Current state:  {chip(status)}  ({status})")
    if setter:
        ts, f = setter
        ev = f.get("event")
        ev = f" event={ev}" if ev else ""
        print(f"Set by:         {ts}  {f.get('decision')}{ev}")
        reason = f.get("reason")
        if reason:
            print(f"Reason:         {reason}")
        label = f.get("label")
        if label and label not in ("None",):
            print(f"Label/row text: {label}")
    if revealed:
        ts, f = revealed
        print(f"Revealed by:    {ts}  {f.get('decision')} via={f.get('via')}  (the subagent prompt that covered it closed)")
    print()
    print("Recent decisions (oldest -> newest, local time; a (chip) moved only the state under an open subagent prompt):")
    for ts, f, s in replay(trail)[-14:]:
        under = status_from(f)
        s = chip(s) if s else (f"({chip(under)})" if under else "  ·  ")
        print(f"  {ts}  {f.get('decision'):20} {s:6} {f.get('reason', '')}")


def main():
    target, argv = app_dir(sys.argv[1:])
    wpath = os.path.join(target, "widget.jsonl")
    if not os.path.exists(wpath):
        sys.exit(f"decision log not found: {wpath}")
    apath = os.path.join(target, "custom_names.json")
    aliases = {}
    if os.path.exists(apath):
        try:
            aliases = json.load(open(apath, encoding="utf-8"))
        except ValueError:
            pass

    trails = load_trails(wpath)
    name = argv[0].strip() if argv else ""
    if not name:
        list_mode(trails, aliases)
        return
    agent = resolve(name, trails, aliases)
    if not agent:
        print(f"No agent matching '{name}'. Active agents:\n")
        list_mode(trails, aliases)
        return
    explain(agent, trails, aliases)


if __name__ == "__main__":
    main()

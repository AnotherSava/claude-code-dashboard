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

# Status -> the chip label the dashboard shows.
CHIP = {"Working": "WORK", "Awaiting": "WAIT", "Done": "DONE", "Idle": "IDLE", "Error": "ERROR"}

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
    """The status a decision line lands the row on, or None if it doesn't move it."""
    d = fields.get("decision")
    if d == "classify":
        return fields.get("status")
    if d == "apply_set":
        return fields.get("new_status")
    if d == "resume_working":
        return "Working"
    if d == "correct_to_awaiting":
        return "Awaiting"
    if d == "correct_to_done":
        return "Done"
    if d == "revert_cancelled":
        return fields.get("status")
    if d == "session_clear":
        return "(cleared)"
    return None


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
            trails[agent].append((entry.get("timestamp", "")[:19], fields))
    return trails


def current(trail):
    """Reconstruct (status, (setter_ts, setter_fields)) by replaying the trail.

    Attributes the status to the most recent decision that established it,
    preferring one that carries a `reason` (a `classify`/correction line) over
    the reason-less `apply_set` mirror that immediately follows it."""
    status = None
    for _, fields in trail:
        s = status_from(fields)
        if s is not None:
            status = s
    if status is None:
        return None, None
    last_any = last_reason = None
    for ts, fields in trail:
        if status_from(fields) == status:
            last_any = (ts, fields)
            if fields.get("reason"):
                last_reason = (ts, fields)
    return status, (last_reason or last_any)


def chip(status):
    return CHIP.get(status, status or "?")


def list_mode(trails, aliases):
    rows = []
    for agent, trail in trails.items():
        status, setter = current(trail)
        if status == "(cleared)":
            continue  # session ended — not on the dashboard
        last_ts = setter[0] if setter else (trail[-1][0] if trail else "")
        rows.append((last_ts, agent, status))
    rows.sort(reverse=True)
    print(f"{'last change (UTC)':21} {'agent':26} {'state':6} display-name")
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
    status, setter = current(trail)
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
    print()
    print("Recent decisions (oldest -> newest):")
    for ts, f in list(trail)[-14:]:
        s = status_from(f)
        s = chip(s) if s else "  ·  "
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

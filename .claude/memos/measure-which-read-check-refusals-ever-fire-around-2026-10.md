---
created: 2026-10-03 00:09:38
---

# Measure which read-check refusals ever fire, around 2026-10-09, before deleting any

terminals::person_verdict (one verdict for every terminal since 2026-10-02) has 18 refusal reasons; at commit time only 16 departures had been judged and 15 reasons had never fired (SelectionLagging and CoverUnknown first in line). After about a week of mixed Windows Terminal, agwinterm and Mac (agterm) use, count attention_poll outcomes per refusal slug in widget.jsonl on BOTH machines (grep decision=attention_poll, group by outcome), and delete or merge the reasons that never fired or only ever refused real reads. Read-only measurement first; deletions go through a normal change. Also worth one code review of the committed range 90162e1..1e0e9c0 at the same time.

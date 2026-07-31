---
name: sync-device-pair
description: Real-device sync setup (CHROME ↔ Olegs-MacBook-Air.local:9078) and how to tell a sleeping Mac from a firewall problem
metadata:
  type: project
---

**Real-device sync pair.** Windows device `CHROME` ↔ Mac `Olegs-MacBook-Air.local`, both listen on :9078, shared bearer token lives in each machine's gitignored `config/local.json` (deploy applies it to the app-data config). Prefer the mDNS hostname in peer URLs — `192.168.1.69` is DHCP.

**Sleeping Mac signature:** answers ping (Power Nap) but TCP to 9078 *times out* — "pingable but port timeout" means asleep, not firewalled (a refused connection would mean app not running). Expected UX: peer rows vanish ~90–120s after sleep (TTL reaper), reappear ≤30s after wake (heartbeat); a returning peer re-advertises its tips and the missed content is pulled, so offline windows are lossless without any sender-side bookkeeping.

**Version-skew signature:** metadata syncs (rows appear, statuses live) but dialogs never arrive. The wire format changed on 2026-07-31 from pushed deltas to advertised tips + receiver pulls; an old build sends `dialog_delta`/`delta_from` that a new receiver ignores, and advertises no `dialog_tip`, so nothing is ever requested.

Both devices run this repo's build — after changing sync code, redeploy both. See [[debug_sync_fake_peer]] for the synthetic-peer e2e alternative.

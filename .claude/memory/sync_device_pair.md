---
name: sync-device-pair
description: Real-device sync setup (CHROME ↔ Olegs-MacBook-Air.local:9078) and how to tell a sleeping Mac from a firewall problem
metadata:
  type: project
---

**Real-device sync pair.** Windows device `CHROME` ↔ Mac `Olegs-MacBook-Air.local`, both listen on :9078, shared bearer token lives in each machine's gitignored `config/local.json` (deploy applies it to the app-data config). Prefer the mDNS hostname in peer URLs — `192.168.1.69` is DHCP.

**Sleeping Mac signature:** answers ping (Power Nap) but TCP to 9078 *times out* — "pingable but port timeout" means asleep, not firewalled (a refused connection would mean app not running). Expected UX: peer rows vanish ~90–120s after sleep (TTL reaper), reappear ≤30s after wake (heartbeat); watermarks make offline windows lossless.

Both devices run this repo's build — after changing sync code, redeploy both. See [[debug_sync_fake_peer]] for the synthetic-peer e2e alternative.

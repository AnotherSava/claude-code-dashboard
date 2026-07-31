---
name: sync-device-pair
description: Real-device sync setup (Tailscale tailnet, chrome ↔ air on :9078) and how to tell a sleeping Mac from a firewall problem
metadata:
  type: project
---

**Real-device sync pair.** Windows device `CHROME` ↔ Mac (dashboard `device_name` still `Olegs-MacBook-Air.local`), both listen on :9078, shared bearer token lives in each machine's gitignored `config/local.json` (deploy applies it to the app-data config).

**Peer URLs use Tailscale MagicDNS names, not mDNS** (set up 2026-07-31): `http://chrome:9078` and `http://air:9078` — note the Mac registered as **`air`**, not `olegs-macbook-air`, so its tailnet name does not match its `device_name`. Tailnet addresses if a name ever fails: chrome `100.86.97.31`, air `100.67.137.90`. The old `*.local` mDNS names only ever worked on a shared LAN and resolve nowhere else; DHCP IPs are worse still.

**Path selection is automatic and continuously re-evaluated:** same LAN → direct over the LAN (sub-millisecond, never leaves the network); different networks → direct peer-to-peer via NAT hole-punching (~70ms measured, `netcheck` showing `UDP: true` + `MappingVariesByDestIP: false`); DERP relay only as bootstrap or where UDP is genuinely blocked. A connection can *start* on DERP and upgrade to direct minutes later, so a one-off `tailscale ping` reporting DERP is **not** evidence that relaying is permanent — re-read `tailscale status` for the live path before concluding anything about the transport.

**Sleeping Mac signature:** answers ping (Power Nap) but TCP to 9078 *times out* — "pingable but port timeout" means asleep, not firewalled (a refused connection would mean app not running). Expected UX: peer rows vanish ~90–120s after sleep (TTL reaper), reappear ≤30s after wake (heartbeat); a returning peer re-advertises its tips and the missed content is pulled, so offline windows are lossless without any sender-side bookkeeping.

**Version-skew signature:** metadata syncs (rows appear, statuses live) but dialogs never arrive. The wire format changed on 2026-07-31 from pushed deltas to advertised tips + receiver pulls; an old build sends `dialog_delta`/`delta_from` that a new receiver ignores, and advertises no `dialog_tip`, so nothing is ever requested.

Both devices run this repo's build — after changing sync code, redeploy both. See [[debug_sync_fake_peer]] for the synthetic-peer e2e alternative.

---
name: sync-device-pair
description: Real-device sync setup (Tailscale tailnet, chrome ↔ air on :9078) and how to tell a sleeping Mac from a firewall problem
metadata:
  type: project
---

**Real-device sync pair.** Windows device `CHROME` ↔ Mac device `AIR` (renamed 2026-09-02 from the hostname-derived `Olegs-MacBook-Air.local`, which the 80px session badge clipped to "Olegs-MacBo…"; `sync.device_name` exists for that badge and is empty-by-default, resolved once from the hostname). Both listen on :9078, shared bearer token lives in each machine's gitignored `config/local.json` (deploy applies it to the app-data config).

**Peer URLs use Tailscale MagicDNS names, not mDNS** (set up 2026-07-31): `http://chrome:9078` and `http://air:9078`. If a MagicDNS name ever fails to resolve, both machines' tailnet addresses are in the encrypted `machines-private` global memory — they are deliberately not written here, since this repo is public. The receiver-local `sync.peer_identity` map binds `device_name` → tailnet node name (`AIR` → `air`) and is what `tailnet::attest` compares against; it is matched case-insensitively, so the two only have to agree in spelling. The old `*.local` mDNS names only ever worked on a shared LAN and resolve nowhere else; DHCP IPs are worse still.

**Path selection is automatic and continuously re-evaluated:** same LAN → direct over the LAN (sub-millisecond, never leaves the network); different networks → direct peer-to-peer via NAT hole-punching (~70ms measured, `netcheck` showing `UDP: true` + `MappingVariesByDestIP: false`); DERP relay only as bootstrap or where UDP is genuinely blocked. A connection can *start* on DERP and upgrade to direct minutes later, so a one-off `tailscale ping` reporting DERP is **not** evidence that relaying is permanent — re-read `tailscale status` for the live path before concluding anything about the transport.

**Sleeping Mac signature:** answers ping (Power Nap) but TCP to 9078 *times out* — "pingable but port timeout" means asleep, not firewalled (a refused connection would mean app not running). Expected UX: peer rows vanish ~90–120s after sleep (TTL reaper), reappear ≤30s after wake (heartbeat); a returning peer re-advertises its tips and the missed content is pulled, so offline windows are lossless without any sender-side bookkeeping.

**Version-skew signature:** metadata syncs (rows appear, statuses live) but dialogs never arrive. The wire format changed on 2026-07-31 from pushed deltas to advertised tips + receiver pulls; an old build sends `dialog_delta`/`delta_from` that a new receiver ignores, and advertises no `dialog_tip`, so nothing is ever requested.

Both devices run this repo's build — after changing sync code, redeploy both. See [[debug_sync_fake_peer]] for the synthetic-peer e2e alternative.

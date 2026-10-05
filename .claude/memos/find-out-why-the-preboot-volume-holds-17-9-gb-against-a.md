---
created: 2026-10-04 15:03:08
platform: macos
---

# Find out why the Preboot volume holds 17.9 GB against a typical 1-2

Measured 2026-10-04 while triaging this Mac filling to 510 MiB free: the APFS Preboot volume reports 17.9 GB. A typical Preboot is 1-2 GB — it holds per-volume-group boot assets (the sealed system's boot kernel collections, FileVault unlock material, recovery). An extra ~16 GB there is as much as the whole disk-reclaim sweep recovered, and none of it is visible to the usual tools: it is not in `~`, so neither `du` over the home directory nor the broom script's rules ever look at it.

NOT a deletion target and nothing should `rm` inside it — a damaged Preboot can leave the machine unbootable. The work is diagnosis: list it per volume group (`diskutil apfs list`, then `sudo du -sh /System/Volumes/Preboot/*`) and see whether the bulk is stale boot assets from superseded macOS versions or an orphaned volume group left by an upgrade or a removed external install. Apple's own route for reclaiming it is a reinstall or `bless`-level repair, so the likely outcome is a documented explanation rather than a fix.

Next step: get the per-UUID breakdown, compare the UUIDs against `diskutil apfs list`'s live volume groups, and record the finding either way — if it is explained, that closes it; if a group is orphaned, decide separately whether removing it is worth the risk.

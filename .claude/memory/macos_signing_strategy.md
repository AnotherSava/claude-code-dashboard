---
name: macos-distribution-uses-ad-hoc-signing-not-notarization
description: "Project intentionally ships ad-hoc-signed macOS DMGs (signingIdentity = \"-\") with a documented user workaround, not Apple Developer ID notarization. Don't push for notarization unprompted."
metadata: 
  node_type: memory
  type: project
---

`src-tauri/tauri.conf.json` sets `bundle.macOS.signingIdentity: "-"` (ad-hoc deep signing, no Apple cert). The README and `docs/index.md` document the first-launch workaround for end users: **System Settings → Privacy & Security → Open Anyway**, or `xattr -cr "/Applications/Claude Code Dashboard.app"`.

**Why:** Apple Developer Program is $99/year. The project is hobby-scale; the cost wasn't justified relative to a one-time documented workaround for end users. The trade-off was deliberated explicitly — see the conversation that introduced this memory.

**How to apply:** Don't suggest "you should get a Developer ID and notarize" unprompted, and don't treat the ad-hoc signing config as a bug to fix. If distribution scale changes later (broader user base, complaints about the workaround), the conversation is revisitable — but the current trade-off is intentional. Related learning: [[tauri-macos-native]] documents the technical detail of why `signingIdentity: "-"` is required for the deep signing to actually happen.

---
created: 2026-11-23 19:02:31
---

# webview2 manager on windows eats more than 2 gb

## Resolution

Fixed by b9da3b9 (stop sending dialogs on every emit), deployed on 2026-11-25. Private memory of the build's msedgewebview2 processes, measured on the Windows machine afterwards:

| Uptime | All WebView2 | Renderer | GPU | Main app |
|---|---|---|---|---|
| launch | 278 MB | | | |
| 3 h 19 min | 357 MB | 50 MB | 111 MB | |
| 15.7 h | 433 MB | 114 MB | 116 MB | 284 MB |
| 17 h | 359 MB | 52 MB | 105 MB | 274 MB |
| 20 h | 360 MB | 57 MB | 106 MB | 279 MB |
| 23 h | 369 MB | 64 MB | 105 MB | 295 MB |

The renderer gave back about 60 MB between 15.7 h and 17 h, which garbage collection does and a leak does not, and the total then held flat at 358 to 375 MB over six hourly readings. The series was taken with the user away and few sessions active, so it does not cover a day of use with the window in view.

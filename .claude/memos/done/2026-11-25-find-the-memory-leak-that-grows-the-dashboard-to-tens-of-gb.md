---
created: 2026-11-13 18:14:04
---

# Find the memory leak that grows the dashboard to tens of GB in a day

Measured on the Mac (air) 2026-11-13 18:11 with top -o mem, the dashboard process (claude-code-dashboard, pid 28648) had been up 31 h (etime 01-07:09) and held:
- the main process: 23 GB footprint, 97 MB compressed
- its two WebKit WebContent processes (started one second after it, so they are its webviews): 1.5 GB + 15 GB compressed and 1.4 GB + 10 GB compressed

Per top's columns that is about 26 GB of footprint (MEM: 23 + 1.5 + 1.4) and about 25 GB compressed (CMPRS: 0.1 + 15 + 10); which of the two should be summed for the webviews is unclear, since their CMPRS far exceeds their MEM. That filled swap (30 GB used of 30, VM APFS volume at 33.3 GB) and cut free space on the Data volume to 2.3 GiB. Quitting and relaunching the app brought swap down to 9.9 GB.

Unknown: whether the growth is in the Rust process (23 GB footprint) or the webview JS heap (the compressed GBs), or both. Two leads: anything that accumulates per message or per session without eviction, and a frontend polling loop that keeps old responses. Reproduce by sampling footprint and compressed size per process every hour after a fresh launch (top -l 1 -pid <pid> -stats pid,mem,cmprs), and use the webviews' Web Inspector memory timeline if it is the JS side.

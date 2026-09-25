---
created: 2026-09-24 18:52:47
platform: windows
---

# Start Windows sessions for session_launcher inside the remote-session holder's tmux

session_launcher::launch refuses on Windows with start_no_launcher, because it was not established which shell wrapper leaves a live prompt when claude exits. The claude repo's remote-session setup now answers that on this machine: every interactive session runs in the WSL tmux server a scheduled-task holder owns, with a known pane command (lib.sh remote_session_resume_command: bash -c '<claude.exe> --continue || exec <claude.exe>'). A windowless wsl.exe -d <distro> -- tmux new-session -d -s cc-<project> -c <WSL_PROJECT_ROOT>/<project> '<resume command>', spawned CREATE_NO_WINDOW, would host a session nobody has opened a tab for.

Two conditions from the 2026-09-24 review. It must refuse when the holder is down, as cc-session.sh does via remote_session_holder_up: a tmux new-session with no holder starts a server owned by that wsl.exe, and claude.exe then dies with it. And it produces no terminal surface, which session_launcher::start_and_wait assumes a launch does (a launch that yields no surface is closed). The session name also has to come from the remote-session naming rule (remote_session_name), or picker and dashboard name the same project differently.

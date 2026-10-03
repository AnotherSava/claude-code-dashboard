---
created: 2026-10-02 04:46:07
platform: windows
---

# Start Windows sessions through agwinterm's session new, so a relayed message to an idle Windows project starts one

session_launcher has no Windows launcher: a relay or start aimed at a Windows project with nothing running ends in start_no_launcher. agwinterm (now wired as a label writer, terminals/agwinterm.rs + agwinterm_wire.rs) exposes a pipe verb that fills the gap.

Verb: session new --cwd <dir> --no-select --command-mode direct --command "C:\WINDOWS\system32\wsl.exe -d Ubuntu -- /mnt/d/projects/claude/claude/remote-session/wsl/start-here.sh" (the user's panes all run exactly that; tree's capturedCommands shows it). It returns the new session id; --no-select avoids stealing focus. Default mode (no --command-mode) is powershell.exe -NoLogo -NoExit -Command <cmd>; direct mode refuses --wait.

Shape: reuse the existing pipe client; give the agwinterm adapter a launch capability and wire session_launcher::start_and_wait's Windows arm to it, then wait for the session registry record as macOS does. The untrusted-directory refusal still applies before launch.

To verify first:
- whether --cwd reaches WSL as the right /mnt/... directory (start-here.sh may cd itself)
- what start-here.sh and its tmux server leave behind when claude exits (the 'exec zsh -i' question macOS answered)
- a WSL-launched claude may write no Windows session-registry record at all, which is what start_and_wait polls for; find the equivalent signal (a 'session' created event from agwinterm's events feed plus paneCwds, or the dashboard's own hook stream)
- an equivalent of agterm's unrealized-session check

Source: the agwinterm skill survey in this session (agwinterm's ControlServer.cs case session.new, SessionCommand.TryCreate). Medium effort.

---
created: 2026-10-03 00:09:38
---

# Keep the person's task when a local SendMessage reply lands on a finished row

apply_set opens no task boundary for an agent message arriving mid-turn (Working/Waiting) or for a RELAYED reply (peer_message's is_reply, read off the dashboard's reply line), but a local Claude Code SendMessage reply (<cross-session-message from="uds:...">) arriving on a Done/Idle row still starts a task, because that envelope carries no reply mark. The row's task line and agwinterm context then switch from the person's task to the replier's task (or the reply's first line). The relayed form of this happened 38 times in widget.jsonl before it was fixed on 2026-10-02 (apply_set lines whose input_label carried 'This is a reply to your message' with task_boundary=true on a Done prior); local replies are at least as common here. Next step: find a signal that a local message answers this row's own earlier message (e.g. this row's session recently sent a SendMessage to the replier — prompt_origin already resolves the sender's inbox to a registry record; or a recent outgoing message recorded per row) and treat it like is_reply; measure the frequency in widget.jsonl first.

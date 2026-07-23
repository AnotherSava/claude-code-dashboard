//! Per-session adherence-canary nonces (see `Config::instruction_canary_enabled`).
//!
//! In-memory, keyed by the registry-resolved chat_id, with overwrite semantics so
//! a `/clear` or a same-cwd restart mints a fresh token that supersedes the old
//! one — a stale marker echoed by rote is then a miss. Not persisted: an app
//! restart simply pauses drift-checking for in-flight sessions until their next
//! `SessionStart` (the same accepted trade-off the in-memory `context_outstanding`
//! alert tracking makes). Cleared on `SessionEnd`.
//!
//! Each entry also tracks a `seen` bit — whether the session's marker has been
//! observed at least once. The Stop drift-check flags drift only after `seen` is
//! set, so a session whose `SessionStart` response was lost (the marker
//! instruction never reached the model) can't manufacture a permanent false
//! drift, and only a genuine mid-session drop after prior adherence flags.

use std::collections::hash_map::DefaultHasher;
use std::collections::HashMap;
use std::hash::{Hash, Hasher};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Mutex;

/// Monotonic salt so two mints in the same millisecond still differ.
static COUNTER: AtomicU64 = AtomicU64::new(0);

/// A session's nonce plus whether its marker has been observed yet this session
/// (the "confirmed adherent" bit that arms drift detection).
#[derive(Clone, Debug)]
struct NonceEntry {
    nonce: String,
    seen: bool,
}

#[derive(Default)]
pub struct NonceStore {
    entries: Mutex<HashMap<String, NonceEntry>>,
}

impl NonceStore {
    pub fn new() -> Self {
        Self::default()
    }

    /// Mint a fresh 4-hex nonce for `chat_id` (resetting `seen`), replacing any
    /// prior one, and return it. Derived — non-cryptographically, since this is a
    /// rote-echo tripwire and not a secret — from chat_id + `now_ms` + a process
    /// counter, so consecutive sessions (even a same-cwd `/clear`) never reuse a
    /// token.
    pub fn mint(&self, chat_id: &str, now_ms: i64) -> String {
        let mut h = DefaultHasher::new();
        chat_id.hash(&mut h);
        now_ms.hash(&mut h);
        COUNTER.fetch_add(1, Ordering::Relaxed).hash(&mut h);
        // `{:04x}` of a 16-bit value → always exactly 4 hex digits, matching
        // `adapters::claude::MARKER_HEX_LEN` (the strip length).
        let nonce = format!("{:04x}", h.finish() & 0xffff);
        self.entries
            .lock()
            .unwrap()
            .insert(chat_id.to_string(), NonceEntry { nonce: nonce.clone(), seen: false });
        nonce
    }

    /// The current `(nonce, seen)` for `chat_id`, if one was minted this session.
    pub fn get(&self, chat_id: &str) -> Option<(String, bool)> {
        self.entries.lock().unwrap().get(chat_id).map(|e| (e.nonce.clone(), e.seen))
    }

    /// Record that this session's marker has been observed, arming drift
    /// detection — a later drop then reads as genuine drift rather than an
    /// undelivered instruction. Idempotent; no-op if the session has no nonce.
    pub fn mark_seen(&self, chat_id: &str) {
        if let Some(e) = self.entries.lock().unwrap().get_mut(chat_id) {
            e.seen = true;
        }
    }

    /// Drop a session's nonce on `SessionEnd` so a `/clear`-recreated row starts
    /// clean (its next `SessionStart` mints anew).
    pub fn forget(&self, chat_id: &str) {
        self.entries.lock().unwrap().remove(chat_id);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn mint_stores_a_four_hex_unseen_nonce_and_get_returns_it() {
        let s = NonceStore::new();
        let n = s.mint("proj", 1000);
        assert_eq!(n.len(), 4);
        assert!(n.chars().all(|c| c.is_ascii_hexdigit()), "nonce is hex: {n}");
        assert_eq!(s.get("proj"), Some((n, false)), "a fresh nonce starts unseen");
    }

    #[test]
    fn mark_seen_arms_the_session_and_is_safe_for_unknown() {
        let s = NonceStore::new();
        let n = s.mint("proj", 1000);
        s.mark_seen("proj");
        assert_eq!(s.get("proj"), Some((n, true)));
        s.mark_seen("nope"); // unknown session — no panic, no-op
    }

    #[test]
    fn mint_overwrites_the_prior_nonce_and_resets_seen() {
        let s = NonceStore::new();
        s.mint("proj", 1000);
        s.mark_seen("proj");
        let second = s.mint("proj", 2000);
        assert_eq!(s.get("proj"), Some((second, false)), "re-mint replaces the token and clears seen");
    }

    #[test]
    fn forget_removes_the_nonce() {
        let s = NonceStore::new();
        s.mint("proj", 1000);
        s.forget("proj");
        assert_eq!(s.get("proj"), None);
    }

    #[test]
    fn get_is_none_for_unknown_session() {
        assert_eq!(NonceStore::new().get("nope"), None);
    }
}

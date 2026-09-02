//! Asking Tailscale which machine a connection came from, instead of asking the
//! sender.
//!
//! # Why this exists
//!
//! The sync listener is tailnet-scoped, so a packet only arrives because
//! WireGuard authenticated the node that sent it. That authentication already
//! happened; the dashboard was throwing the result away and reading a
//! self-declared `origin_device` string instead.
//!
//! The temptation is to lean on the bearer token for this. It does not carry the
//! answer: `config.sync.token` is **one shared secret for the whole fleet**, so a
//! valid hop proves *a token-holder*, not *which machine*. `sync::post_message`
//! already knew this — it logs `peer.ip()` rather than the claimed device name,
//! precisely because "anyone holding the token can attribute a send to the
//! user's own laptop".
//!
//! Per-device tokens were considered and rejected: still a bearer secret (so
//! reading one machine's config impersonates it completely), rotation touches
//! every machine, and it reimplements — with new failure modes — an identity
//! Tailscale is already asserting underneath us. Worth revisiting only if the
//! fleet ever leaves the tailnet.
//!
//! # What this is, and what it is not
//!
//! `tailscale whois` maps a tailnet address to the node and the user behind it,
//! from the local `tailscaled`. Verified against a live peer on 2026-08-30:
//!
//! ```text
//! $ tailscale whois --json 100.x.y.z:9078
//! Node.ComputedName : peer-device
//! UserProfile       : you@example.com
//! $ tailscale whois 8.8.8.8:443     -> peer not found
//! $ tailscale whois 127.0.0.1:9078  -> peer not found
//! ```
//!
//! So it is **attestation**, not verification, and the wording everywhere
//! downstream says so. It inherits the tailnet's ACLs and Tailscale's control
//! plane, and it says nothing at all about the *agent* half of the identity —
//! `from_agent` is still whatever a loopback caller on that machine typed. What
//! it does establish is that the message really came from that node, owned by
//! that tailnet user.
//!
//! Loopback answering `peer not found` is correct and expected: the localhost
//! observer-peer test harness is not a tailnet peer. That path degrades to
//! [`Attestation::Claimed`], never to a failure.

use std::collections::{BTreeMap, HashMap};
use std::net::IpAddr;
use std::sync::Mutex;
use std::time::{Duration, Instant};

/// How long a resolved address is trusted before asking `tailscaled` again. The
/// address→node binding is stable for the life of a node, so this is about
/// bounding subprocess spawns, not freshness.
const CACHE_TTL: Duration = Duration::from_secs(60);

/// Longest we wait for `tailscale whois` before killing it. A local socket
/// round-trip; anything slower is a hung daemon, and the answer degrades to
/// "no answer" rather than tying up a blocking thread.
const WHOIS_TIMEOUT: Duration = Duration::from_secs(2);

/// What `tailscaled` says about the far end of a connection.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TailnetPeer {
    /// `Node.ComputedName` — the short node name (`chrome`), not the FQDN.
    pub node: String,
    /// The tailnet user who owns the node. On a shared tailnet this is what
    /// distinguishes "a machine of mine" from "a machine of someone else's".
    pub user: Option<String>,
}

/// How a claimed device name stands up to what Tailscale says.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize)]
#[serde(rename_all = "lowercase")]
pub enum Attestation {
    /// The connection comes from the tailnet node this device is bound to.
    Attested,
    /// No answer from `tailscaled`, or no binding configured for this device.
    /// The status quo: the name is taken at its word, and said to be.
    Claimed,
    /// A binding exists and the connection does **not** come from that node.
    /// The only outcome that refuses.
    Mismatch,
}

/// Judge a claimed device name. Pure, so the whole rule is testable without a
/// tailnet.
///
/// `bindings` maps `sync.device_name` to a tailnet node name. It has to be
/// **local config on the receiver** and cannot come off the wire: a sender
/// controls both halves of its own envelope, so a hostile node would simply
/// claim `device_name = "CHROME"` alongside its own truthful node name and
/// attest itself. An out-of-band binding is what makes the check non-circular.
///
/// Absence is `Claimed`, never `Attested` — a check that did not run must not
/// read as a check that passed.
pub fn attest(claimed_device: &str, peer: Option<&TailnetPeer>, bindings: &BTreeMap<String, String>) -> Attestation {
    let Some(peer) = peer else {
        return Attestation::Claimed;
    };
    // Case-insensitive throughout: whois reports `chrome` while `device_name` on
    // that box is `CHROME` (it bootstraps from `COMPUTERNAME`). Note this is the
    // *only* place the two namespaces are compared loosely —
    // `resolve_message_target` still matches device names exactly, because there
    // it is addressing a row, not identifying a machine.
    match bindings.iter().find(|(device, _)| device.eq_ignore_ascii_case(claimed_device)) {
        Some((_, node)) if node.eq_ignore_ascii_case(&peer.node) => Attestation::Attested,
        Some(_) => Attestation::Mismatch,
        // No explicit binding: accept the happy coincidence that the names
        // already agree, and otherwise say `Claimed` rather than guessing. A
        // heuristic would be wrong on this very fleet — the Mac's device name is
        // `Some-Laptop.local` against a node name of `some-laptop`.
        None if claimed_device.eq_ignore_ascii_case(&peer.node) => Attestation::Attested,
        None => Attestation::Claimed,
    }
}

/// Whether an explicit `peer_identity` entry binds `claimed_device` to the node
/// this connection actually came from.
///
/// Strictly stronger than [`attest`] answering [`Attestation::Attested`], and
/// the gap is the whole point for a route that **authorises** rather than
/// attributes. `attest` also answers `Attested` on the happy coincidence that an
/// *unbound* claimed name already equals the node's own name — but the sender
/// picks that name, so it chooses both sides of the comparison and any node
/// holding the fleet token self-attests by simply telling the truth about
/// itself. That is a fair corroboration for deciding whose rows a push becomes,
/// which is all `attest` was written for. It is not a gate.
///
/// So starting a process, recording a standing permission, and disclosing this
/// machine's directory layout all require a binding the *receiver* wrote down —
/// which is what makes the check non-circular, exactly as `attest`'s own doc
/// says about the map living locally.
pub fn bound(claimed_device: &str, peer: Option<&TailnetPeer>, bindings: &BTreeMap<String, String>) -> bool {
    let Some(peer) = peer else { return false };
    bindings
        .iter()
        .find(|(device, _)| device.eq_ignore_ascii_case(claimed_device))
        .is_some_and(|(_, node)| node.eq_ignore_ascii_case(&peer.node))
}

/// Cached `tailscale whois` lookups, managed by Tauri so the cache outlives one
/// request.
#[derive(Default)]
pub struct TailnetResolver {
    cache: Mutex<HashMap<IpAddr, (Option<TailnetPeer>, Instant)>>,
}

impl TailnetResolver {
    /// Resolve one address, or `None` when Tailscale has no answer — the peer is
    /// not on the tailnet, the binary is absent, or the daemon did not reply.
    /// Every one of those is "no answer", and none of them is an error the
    /// caller should surface as a failure.
    pub fn whois(&self, ip: IpAddr) -> Option<TailnetPeer> {
        if let Some((hit, at)) = self.cache.lock().unwrap().get(&ip) {
            if at.elapsed() < CACHE_TTL {
                return hit.clone();
            }
        }
        let fresh = whois_uncached(ip);
        self.cache.lock().unwrap().insert(ip, (fresh.clone(), Instant::now()));
        fresh
    }

    /// Resolve an address and judge the name claimed over it, in one call.
    ///
    /// The two exist as a pair because every caller needs both halves — the
    /// standing to act on, and the peer itself for the envelope — and pairing
    /// them here is what stops the two call sites (the message route and the
    /// push handler) from drifting into different spellings of the same
    /// security check. They already had: one folded a missing resolver in with
    /// `and_then`, the other with a `match`.
    pub fn attest_peer(&self, ip: IpAddr, claimed_device: &str, bindings: &BTreeMap<String, String>) -> (Attestation, Option<TailnetPeer>) {
        let peer = self.whois(ip);
        (attest(claimed_device, peer.as_ref(), bindings), peer)
    }

    /// [`bound`] against this connection's real node. The gate for anything that
    /// authorises rather than attributes.
    pub fn peer_is_bound(&self, ip: IpAddr, claimed_device: &str, bindings: &BTreeMap<String, String>) -> bool {
        bound(claimed_device, self.whois(ip).as_ref(), bindings)
    }
}

/// Where the CLI lives. On PATH for a normal Linux/Homebrew install; the app
/// bundles put it somewhere PATH does not reach, and a Tauri app on macOS
/// inherits no shell profile — the same trap `token_scan` hit resolving
/// `CLAUDE_CONFIG_DIR`, so the fallbacks are spelled out rather than assumed.
fn tailscale_bin() -> Option<std::path::PathBuf> {
    let candidates: [&str; 3] = if cfg!(target_os = "macos") {
        ["/Applications/Tailscale.app/Contents/MacOS/Tailscale", "/usr/local/bin/tailscale", "/opt/homebrew/bin/tailscale"]
    } else if cfg!(windows) {
        [r"C:\Program Files\Tailscale\tailscale.exe", r"C:\Program Files (x86)\Tailscale\tailscale.exe", "tailscale.exe"]
    } else {
        ["/usr/bin/tailscale", "/usr/local/bin/tailscale", "tailscale"]
    };
    candidates.iter().map(std::path::PathBuf::from).find(|p| p.is_absolute() && p.exists()).or_else(|| {
        // Last resort: let the OS search PATH. `Command` does that for a bare
        // name, so hand back the bare name rather than probing for it.
        Some(std::path::PathBuf::from(if cfg!(windows) { "tailscale.exe" } else { "tailscale" }))
    })
}

fn whois_uncached(ip: IpAddr) -> Option<TailnetPeer> {
    use std::process::{Command, Stdio};

    let bin = tailscale_bin()?;
    // The port is required by the argument shape and irrelevant to the answer.
    let addr = match ip {
        IpAddr::V4(v4) => format!("{v4}:0"),
        IpAddr::V6(v6) => format!("[{v6}]:0"),
    };
    let mut cmd = Command::new(&bin);
    cmd.args(["whois", "--json", &addr]).stdout(Stdio::piped()).stderr(Stdio::null()).stdin(Stdio::null());
    // Redirecting all three stdio handles is NOT enough on Windows. The
    // dashboard is a GUI process and owns no console, so spawning a console
    // binary makes Windows allocate a fresh one for the child — which under
    // Windows Terminal is a real window that flashes open and shut. Reported
    // from the Windows box 2026-08-31: one window per minute, in lockstep with
    // `CACHE_TTL` expiring against the peer's 30 s sync heartbeat. Invisible
    // from macOS, where a spawn has no console to allocate.
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        /// `CREATE_NO_WINDOW` — spelled out rather than pulled from `winapi`,
        /// which this crate does not depend on for one constant.
        const CREATE_NO_WINDOW: u32 = 0x0800_0000;
        cmd.creation_flags(CREATE_NO_WINDOW);
    }
    let mut child = cmd.spawn().ok()?;

    // `Command` has no timeout, and a wedged daemon would otherwise hold a
    // blocking thread indefinitely. Poll, then kill.
    let deadline = Instant::now() + WHOIS_TIMEOUT;
    loop {
        match child.try_wait() {
            Ok(Some(_)) => break,
            Ok(None) if Instant::now() < deadline => std::thread::sleep(Duration::from_millis(20)),
            Ok(None) => {
                let _ = child.kill();
                let _ = child.wait();
                tracing::warn!(%ip, "tailscale whois timed out; treating the device as claimed");
                return None;
            }
            Err(_) => return None,
        }
    }
    let out = child.wait_with_output().ok()?;
    if !out.status.success() {
        // "peer not found" for anything off the tailnet, including loopback.
        return None;
    }
    parse_whois(&String::from_utf8_lossy(&out.stdout))
}

/// Pull the node and user out of `tailscale whois --json`.
///
/// Every field is optional and a missing one degrades to `None`: this is another
/// program's output shape, so a rename upstream must cost an attestation, never
/// a panic or a refused message.
fn parse_whois(stdout: &str) -> Option<TailnetPeer> {
    let v: serde_json::Value = serde_json::from_str(stdout).ok()?;
    let node = v.get("Node")?.get("ComputedName")?.as_str()?.trim().to_string();
    if node.is_empty() {
        return None;
    }
    let user = v
        .get("UserProfile")
        .and_then(|u| u.get("LoginName"))
        .and_then(|l| l.as_str())
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_string);
    Some(TailnetPeer { node, user })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn bindings(pairs: &[(&str, &str)]) -> BTreeMap<String, String> {
        pairs.iter().map(|(a, b)| (a.to_string(), b.to_string())).collect()
    }

    fn peer(node: &str) -> TailnetPeer {
        TailnetPeer { node: node.to_string(), user: Some("you@example.com".into()) }
    }

    /// The gap between attributing and authorising, pinned.
    ///
    /// `attest` accepts an unbound name that happens to equal the node's own —
    /// which the sender picks, so it chooses both sides of the comparison and
    /// any node holding the fleet token self-attests by truthfully naming
    /// itself. That is fine for deciding whose rows a push becomes. It is not a
    /// gate, and starting a process on a peer's word is a gate.
    #[test]
    fn an_unbound_name_attests_but_is_never_bound() {
        let none = BTreeMap::new();
        assert_eq!(attest("evil", Some(&peer("evil")), &none), Attestation::Attested, "the coincidence branch, unchanged");
        assert!(!bound("evil", Some(&peer("evil")), &none), "but nothing the receiver wrote down says so");

        let bindings: BTreeMap<String, String> = [("CHROME".to_string(), "chrome".to_string())].into_iter().collect();
        assert!(bound("CHROME", Some(&peer("chrome")), &bindings), "an explicit binding, matched case-insensitively like attest");
        assert!(!bound("CHROME", Some(&peer("air")), &bindings), "bound to a different node");
        assert!(!bound("air", Some(&peer("air")), &bindings), "a device with no entry is never bound, even truthfully named");
        assert!(!bound("CHROME", None, &bindings), "no answer from tailscaled is not a pass");
    }

    /// The real fleet's shape: `device_name` is `CHROME` (from `COMPUTERNAME`)
    /// and the node name is `chrome`. An exact comparison would refuse every
    /// genuine message from that machine.
    #[test]
    fn a_bound_device_attests_case_insensitively() {
        let b = bindings(&[("CHROME", "chrome")]);
        assert_eq!(attest("CHROME", Some(&peer("chrome")), &b), Attestation::Attested);
    }

    /// The check that makes the whole thing worth having: a node claiming to be
    /// somebody else. It holds the fleet token — that is exactly the attacker
    /// the shared secret cannot stop — but it cannot make whois lie about its
    /// own address.
    #[test]
    fn a_bound_device_claimed_from_the_wrong_node_is_a_mismatch() {
        let b = bindings(&[("CHROME", "chrome")]);
        assert_eq!(attest("CHROME", Some(&peer("someone-elses-laptop")), &b), Attestation::Mismatch);
    }

    /// No answer from Tailscale must never read as a passed check. Loopback (the
    /// localhost observer-peer harness) and a missing binary both land here.
    #[test]
    fn no_whois_answer_is_claimed_not_attested() {
        let b = bindings(&[("CHROME", "chrome")]);
        assert_eq!(attest("CHROME", None, &b), Attestation::Claimed);
        assert_eq!(attest("CHROME", None, &BTreeMap::new()), Attestation::Claimed);
    }

    /// With no binding configured, names that already agree are accepted and
    /// names that do not are merely unattested — never a refusal. Refusing here
    /// would break every fleet that has not configured the map yet, which on the
    /// day this ships is all of them.
    #[test]
    fn an_unbound_device_falls_back_to_name_agreement_and_never_refuses() {
        let none = BTreeMap::new();
        assert_eq!(attest("chrome", Some(&peer("chrome")), &none), Attestation::Attested);
        assert_eq!(attest("Some-Laptop.local", Some(&peer("some-laptop")), &none), Attestation::Claimed);
    }

    /// A binding is the authority: once one exists, name agreement is not
    /// consulted, so a device cannot slip past its own binding by renaming.
    #[test]
    fn a_binding_overrides_incidental_name_agreement() {
        let b = bindings(&[("chrome", "some-other-node")]);
        assert_eq!(attest("chrome", Some(&peer("chrome")), &b), Attestation::Mismatch);
    }

    /// A console spawn from this GUI process must carry `CREATE_NO_WINDOW` on
    /// Windows, or it flashes a terminal window at the user — the bug reported
    /// 2026-08-31, once a minute, for a subprocess whose output nobody sees.
    ///
    /// Asserted over the source text rather than behind a helper: this is the
    /// *only* Windows-reachable `Command::new` in the crate (every other one is
    /// `#[cfg(not(windows))]` or macOS-only), and wrapping a single call site
    /// would be abstraction ahead of need. A test scales to the next one without
    /// that — and unlike a helper, it cannot be bypassed by someone calling
    /// `Command::new` directly, which is exactly how this would recur.
    #[test]
    fn the_whois_spawn_creates_no_console_window() {
        let src = include_str!("tailnet.rs");
        // Search the PRODUCTION half only. Every string this test navigates by
        // — the anchor, `.spawn()`, and the flag it asserts on — also appears
        // in the test's own body, so scanning the whole file lets a deleted
        // call site be "found" inside this function and judged against this
        // function. That reads as a pass while guarding nothing. It happens not
        // to today, because the real spawn sits earlier and `find` takes the
        // first match — positional luck, not a property worth relying on.
        let src = &src[..src.find("#[cfg(test)]").expect("test module marker")];
        let at = src.find("let mut cmd = Command::new(&bin);").expect("the whois spawn moved — update this test");
        let spawn_at = src[at..].find(".spawn()").expect("spawn call") + at;
        let body = &src[at..spawn_at];
        assert!(body.contains("creation_flags(CREATE_NO_WINDOW)"), "the whois spawn must set CREATE_NO_WINDOW, or it flashes a console window on Windows once per cache expiry");
        assert!(body.contains("#[cfg(windows)]"), "the flag is Windows-only API and must stay cfg-gated");
    }

    /// Parsed against the real output shape, captured from a live peer.
    #[test]
    fn whois_json_yields_the_node_and_the_user() {
        let out = r#"{"Node":{"ID":1234567890,"Name":"peer-device.tailnet.ts.net.","ComputedName":"peer-device"},"UserProfile":{"LoginName":"you@example.com"}}"#;
        assert_eq!(parse_whois(out), Some(TailnetPeer { node: "peer-device".into(), user: Some("you@example.com".into()) }));
    }

    /// Another program's output shape: a rename upstream costs an attestation,
    /// never a panic and never a refusal.
    #[test]
    fn unparseable_whois_output_is_no_answer() {
        assert_eq!(parse_whois("peer not found"), None);
        assert_eq!(parse_whois("{}"), None);
        assert_eq!(parse_whois(r#"{"Node":{"ComputedName":""}}"#), None);
        assert_eq!(parse_whois(r#"{"Node":{"ComputedName":"chrome"}}"#), Some(TailnetPeer { node: "chrome".into(), user: None }));
    }
}

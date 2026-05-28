//! Per-VM egress allow-list rule generation (SEC-263).
//!
//! Translates a [`VmEgressConfig`] into the iptables FORWARD-chain
//! rules that enforce the policy for one VM. Generation is pure:
//! `(config, tap_ip, wan_iface, comment, resolver) -> Vec<EgressRule>`.
//! The resulting [`EgressRule`]s carry argv-style argument lists; the
//! caller (the network backend) is responsible for running iptables.
//!
//! ## Why a pure layer
//!
//! The wire-up (add at VM start, remove at VM stop) is symmetric and
//! best-effort, but the rule shape is the security-critical part. By
//! keeping generation pure we can unit-test exactly what would be
//! handed to iptables on a real host — allow-entries become ACCEPTs,
//! `deny_all_other` becomes a trailing DROP, hostnames are resolved
//! once at generation time, and rules are tagged with the install's
//! iptables comment so cleanup scopes to *this* install.
//!
//! ## DNS resolution
//!
//! Hostnames are resolved at generation time only. A name that
//! resolves to multiple IPs produces one ACCEPT rule per IP. Records
//! that change after the VM starts won't propagate — restart the VM
//! to pick up new IPs. This limitation is documented on
//! [`VmEgressConfig`].

use std::net::ToSocketAddrs;

use ember_core::config::vm::VmEgressConfig;
use ember_core::error::Result;

use crate::network::nat;

/// A single iptables FORWARD-chain rule generated from an egress policy.
///
/// Holds the argv as owned strings so the rule can be passed across
/// thread / module boundaries freely. Use [`Self::iptables_args`] to
/// get an `&str` slice suitable for `Command::args`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EgressRule {
    args: Vec<String>,
}

impl EgressRule {
    /// Borrow the argv for handing to `Command::args`.
    pub fn iptables_args(&self) -> Vec<&str> {
        self.args.iter().map(String::as_str).collect()
    }

    /// Owned argv (used by the delete path, which substitutes `-A` →
    /// `-D` before invoking iptables).
    pub fn into_args(self) -> Vec<String> {
        self.args
    }

    /// Borrow the argv directly.
    pub fn args(&self) -> &[String] {
        &self.args
    }
}

/// Trait the rule generator uses to resolve hostnames to IPv4 addresses.
///
/// Lifted out so tests can use a deterministic fixture instead of real
/// DNS. The default implementation in [`SystemResolver`] uses
/// `ToSocketAddrs` (which goes through libc's `getaddrinfo`, so it
/// honors `nsswitch.conf` and the same DNS path the host normally
/// uses).
pub trait HostResolver {
    /// Resolve a hostname to zero or more IPv4 addresses.
    /// Returns an empty vec when the host cannot be resolved — the
    /// caller logs and skips, never erroring out the whole VM start.
    fn resolve(&self, host: &str) -> Vec<String>;
}

/// `getaddrinfo`-backed resolver. Used at runtime.
pub struct SystemResolver;

impl HostResolver for SystemResolver {
    fn resolve(&self, host: &str) -> Vec<String> {
        // `ToSocketAddrs` needs a port; the port is meaningless for
        // address resolution but `getaddrinfo` insists on one. Filter
        // to IPv4 since the iptables we drive is the legacy v4 binary
        // (the WAN setup elsewhere is IPv4-only too — see SEC-263).
        match (host, 0u16).to_socket_addrs() {
            Ok(iter) => iter
                .filter_map(|sa| match sa {
                    std::net::SocketAddr::V4(v4) => Some(v4.ip().to_string()),
                    std::net::SocketAddr::V6(_) => None,
                })
                .collect(),
            Err(_) => Vec::new(),
        }
    }
}

/// Classification of a single `allow` entry.
///
/// Lifted out so the parsing logic is testable and the rule generator
/// can branch on the variant without re-parsing.
#[derive(Debug, PartialEq, Eq)]
enum AllowEntry {
    /// IPv4 CIDR or bare IPv4 (passed to iptables verbatim).
    Literal(String),
    /// Hostname requiring DNS resolution.
    Hostname(String),
}

/// Classify an entry as an IP/CIDR literal or a hostname.
///
/// We treat any string containing a `/` *or* parseable as an IPv4
/// address as a literal; everything else as a hostname. This keeps
/// us from sending bad strings into `getaddrinfo` for the common case
/// where the user wrote a CIDR.
fn classify(entry: &str) -> AllowEntry {
    let trimmed = entry.trim();
    if trimmed.contains('/') {
        return AllowEntry::Literal(trimmed.to_string());
    }
    if trimmed.parse::<std::net::Ipv4Addr>().is_ok() {
        return AllowEntry::Literal(format!("{trimmed}/32"));
    }
    AllowEntry::Hostname(trimmed.to_string())
}

/// Generate the FORWARD rules implementing the given policy for one VM.
///
/// The returned rules are in the order they should be applied:
///   1. One `ACCEPT` per resolved destination, keyed on the VM's TAP
///      source IP and the WAN egress interface.
///   2. If `policy.deny_all_other` is set, a final `DROP` of all
///      remaining traffic from that source IP out the WAN.
///
/// Returns `Vec::new()` when the policy is empty (no allow entries
/// and no default-deny) — the caller can skip the iptables pass
/// entirely.
///
/// `tap_ip` is the guest's /30 address (the source iptables matches
/// on). `wan_iface` is the WAN interface name (matches `-o`).
/// `comment` is the per-install iptables comment tag (e.g.
/// `ember:a3f4`); empty string omits the comment match to stay
/// byte-for-byte identical to rules added by older binaries (so
/// `-D` matches them).
pub fn generate_rules<R: HostResolver>(
    policy: &VmEgressConfig,
    tap_ip: &str,
    wan_iface: &str,
    comment: &str,
    resolver: &R,
) -> Vec<EgressRule> {
    if policy.is_empty() {
        return Vec::new();
    }

    let guest_cidr = format!("{tap_ip}/32");
    let mut rules: Vec<EgressRule> = Vec::new();
    // Dedup destinations so a hostname that resolves to a CIDR-already-
    // covered IP, or two hostnames that share an IP, only get one rule.
    let mut seen_dests: std::collections::HashSet<String> = std::collections::HashSet::new();

    for entry in &policy.allow {
        let trimmed = entry.trim();
        if trimmed.is_empty() {
            continue;
        }
        match classify(trimmed) {
            AllowEntry::Literal(cidr) => {
                if seen_dests.insert(cidr.clone()) {
                    rules.push(accept_rule(&guest_cidr, &cidr, wan_iface, comment));
                }
            }
            AllowEntry::Hostname(host) => {
                let ips = resolver.resolve(&host);
                for ip in ips {
                    let dest = format!("{ip}/32");
                    if seen_dests.insert(dest.clone()) {
                        rules.push(accept_rule(&guest_cidr, &dest, wan_iface, comment));
                    }
                }
            }
        }
    }

    if policy.deny_all_other {
        rules.push(drop_rule(&guest_cidr, wan_iface, comment));
    }

    rules
}

/// Build an ACCEPT rule for a single destination.
///
/// `-I FORWARD 1` (insert at top) so allows precede the trailing DROP
/// when both are applied — order matters in FORWARD, and we don't
/// want to depend on iptables internals to keep allow-before-deny.
fn accept_rule(guest_cidr: &str, dest_cidr: &str, wan_iface: &str, comment: &str) -> EgressRule {
    let head: &[&str] = &[
        "-I", "FORWARD", "1", "-s", guest_cidr, "-d", dest_cidr, "-o", wan_iface,
    ];
    let tail: &[&str] = &["-j", "ACCEPT"];
    EgressRule {
        args: with_comment_owned(head, comment, tail),
    }
}

/// Build the trailing DROP rule for `deny_all_other`.
///
/// Appended (`-A`) so it runs after the inserted ACCEPTs.
fn drop_rule(guest_cidr: &str, wan_iface: &str, comment: &str) -> EgressRule {
    let head: &[&str] = &["-A", "FORWARD", "-s", guest_cidr, "-o", wan_iface];
    let tail: &[&str] = &["-j", "DROP"];
    EgressRule {
        args: with_comment_owned(head, comment, tail),
    }
}

/// Apply a previously-generated rule set to iptables.
///
/// Bails on the first failure (after rolling back the rules that
/// succeeded so we don't leave a half-applied policy). Best-effort
/// rollback — a rollback failure logs but does not mask the original
/// error, so the caller sees the policy-apply failure that prompted
/// teardown.
pub fn apply_rules(rules: &[EgressRule]) -> Result<()> {
    for (i, rule) in rules.iter().enumerate() {
        let args = rule.iptables_args();
        if let Err(e) = nat::iptables(&args) {
            // Roll back what we applied so the chain doesn't drift.
            for applied in &rules[..i] {
                let _ = remove_one(applied);
            }
            return Err(e);
        }
    }
    Ok(())
}

/// Remove a previously-applied rule set. Best-effort — keeps going
/// past failures so cleanup makes maximum progress.
pub fn remove_rules(rules: &[EgressRule]) {
    for r in rules {
        let _ = remove_one(r);
    }
}

/// Delete one rule by substituting the leading `-A`/`-I FORWARD 1`
/// with the matching `-D FORWARD` shape iptables expects.
///
/// `-I FORWARD 1` (insert-at-1) becomes `-D FORWARD`; `-A FORWARD`
/// also becomes `-D FORWARD`. The rest of the rule must match
/// byte-for-byte or iptables won't find it.
fn remove_one(rule: &EgressRule) -> Result<()> {
    let owned = rule.clone().into_args();
    let mut del: Vec<String> = Vec::with_capacity(owned.len());
    let mut it = owned.into_iter();
    match it.next().as_deref() {
        Some("-I") => {
            // `-I FORWARD <pos> …` → `-D FORWARD …` (skip the pos).
            del.push("-D".into());
            if let Some(chain) = it.next() {
                del.push(chain);
            }
            // Drop the position argument.
            let _ = it.next();
        }
        Some("-A") => {
            // `-A FORWARD …` → `-D FORWARD …`.
            del.push("-D".into());
            if let Some(chain) = it.next() {
                del.push(chain);
            }
        }
        Some(other) => {
            // Unknown shape; just pass through so a future reader sees
            // exactly what got generated rather than a silent mangle.
            del.push(other.to_string());
        }
        None => return Ok(()),
    }
    del.extend(it);
    let borrowed: Vec<&str> = del.iter().map(String::as_str).collect();
    nat::iptables_delete(&borrowed)
}

/// Same shape as `nat::with_comment` but produces owned `String`s.
///
/// Egress rules need to outlive the call (they're stored in
/// [`EgressRule`] and consumed asynchronously), so we can't borrow
/// from short-lived slice literals like NAT does.
fn with_comment_owned(head: &[&str], comment: &str, tail: &[&str]) -> Vec<String> {
    let mut out: Vec<String> = Vec::with_capacity(head.len() + tail.len() + 4);
    out.extend(head.iter().map(|s| (*s).to_string()));
    if !comment.is_empty() {
        out.push("-m".into());
        out.push("comment".into());
        out.push("--comment".into());
        out.push(comment.to_string());
    }
    out.extend(tail.iter().map(|s| (*s).to_string()));
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;

    /// Deterministic resolver. Returns whatever was pre-loaded; unknown
    /// hosts resolve to empty (matching the SystemResolver's NXDOMAIN
    /// behavior).
    struct StubResolver {
        map: HashMap<String, Vec<String>>,
    }

    impl StubResolver {
        fn new(pairs: &[(&str, &[&str])]) -> Self {
            let mut map = HashMap::new();
            for (host, ips) in pairs {
                map.insert(
                    host.to_string(),
                    ips.iter().map(|s| s.to_string()).collect(),
                );
            }
            Self { map }
        }
    }

    impl HostResolver for StubResolver {
        fn resolve(&self, host: &str) -> Vec<String> {
            self.map.get(host).cloned().unwrap_or_default()
        }
    }

    /// Helper to flatten a Vec<EgressRule> into Vec<Vec<String>>.
    fn rule_args(rules: &[EgressRule]) -> Vec<Vec<String>> {
        rules.iter().map(|r| r.args.clone()).collect()
    }

    #[test]
    fn classify_handles_cidr() {
        assert_eq!(
            classify("10.0.0.0/8"),
            AllowEntry::Literal("10.0.0.0/8".to_string())
        );
        assert_eq!(
            classify("  192.168.1.0/24  "),
            AllowEntry::Literal("192.168.1.0/24".to_string())
        );
    }

    #[test]
    fn classify_handles_bare_ipv4() {
        assert_eq!(
            classify("1.2.3.4"),
            AllowEntry::Literal("1.2.3.4/32".to_string())
        );
    }

    #[test]
    fn classify_handles_hostname() {
        assert_eq!(
            classify("api.anthropic.com"),
            AllowEntry::Hostname("api.anthropic.com".to_string())
        );
        assert_eq!(
            classify("github.com"),
            AllowEntry::Hostname("github.com".to_string())
        );
    }

    #[test]
    fn empty_policy_generates_no_rules() {
        let policy = VmEgressConfig::default();
        let resolver = StubResolver::new(&[]);
        let rules = generate_rules(&policy, "10.100.0.2", "eth0", "ember:a3f4", &resolver);
        assert!(rules.is_empty());
    }

    #[test]
    fn allow_only_no_deny_generates_only_accepts() {
        let policy = VmEgressConfig {
            allow: vec!["10.0.0.0/8".into()],
            deny_all_other: false,
        };
        let resolver = StubResolver::new(&[]);
        let rules = generate_rules(&policy, "10.100.0.2", "eth0", "ember:a3f4", &resolver);
        assert_eq!(rules.len(), 1);
        assert!(rules[0].args.contains(&"ACCEPT".to_string()));
        assert!(rules[0].args.iter().all(|s| s != "DROP"));
    }

    #[test]
    fn deny_only_no_allow_generates_only_drop() {
        let policy = VmEgressConfig {
            allow: vec![],
            deny_all_other: true,
        };
        let resolver = StubResolver::new(&[]);
        let rules = generate_rules(&policy, "10.100.0.2", "eth0", "ember:a3f4", &resolver);
        assert_eq!(rules.len(), 1);
        assert!(rules[0].args.contains(&"DROP".to_string()));
    }

    #[test]
    fn allow_cidr_emits_expected_rule_shape() {
        let policy = VmEgressConfig {
            allow: vec!["10.0.0.0/8".into()],
            deny_all_other: false,
        };
        let resolver = StubResolver::new(&[]);
        let rules = generate_rules(&policy, "10.100.0.2", "eth0", "ember:a3f4", &resolver);
        assert_eq!(
            rule_args(&rules),
            vec![vec![
                "-I",
                "FORWARD",
                "1",
                "-s",
                "10.100.0.2/32",
                "-d",
                "10.0.0.0/8",
                "-o",
                "eth0",
                "-m",
                "comment",
                "--comment",
                "ember:a3f4",
                "-j",
                "ACCEPT"
            ]
            .into_iter()
            .map(String::from)
            .collect::<Vec<_>>()]
        );
    }

    #[test]
    fn empty_comment_omits_comment_match() {
        // Legacy/no-namespace install must produce byte-for-byte the
        // same shape an older binary would, so `-D` matches existing
        // rules on upgrade.
        let policy = VmEgressConfig {
            allow: vec!["1.2.3.4".into()],
            deny_all_other: true,
        };
        let resolver = StubResolver::new(&[]);
        let rules = generate_rules(&policy, "10.100.0.2", "eth0", "", &resolver);
        for r in &rules {
            assert!(
                !r.args.iter().any(|s| s == "-m" || s == "--comment"),
                "empty comment must not insert -m comment match: {:?}",
                r.args
            );
        }
    }

    #[test]
    fn hostname_resolved_at_generation_one_rule_per_ip() {
        let policy = VmEgressConfig {
            allow: vec!["github.com".into()],
            deny_all_other: true,
        };
        let resolver = StubResolver::new(&[("github.com", &["140.82.121.3", "140.82.121.4"])]);
        let rules = generate_rules(&policy, "10.100.0.2", "eth0", "ember:a3f4", &resolver);
        // 2 ACCEPTs (one per resolved IP) + 1 DROP.
        assert_eq!(rules.len(), 3);
        assert!(rules[0]
            .args
            .windows(2)
            .any(|w| w == ["-d", "140.82.121.3/32"]));
        assert!(rules[1]
            .args
            .windows(2)
            .any(|w| w == ["-d", "140.82.121.4/32"]));
        assert!(rules[2].args.contains(&"DROP".to_string()));
    }

    #[test]
    fn unresolvable_hostname_produces_no_accept_but_keeps_drop() {
        // Documented behavior: an unresolvable name is a no-op for
        // its allow entry. With `deny_all_other: true`, the DROP
        // still locks the policy — better fail-closed than open up
        // egress because DNS was flaky at start.
        let policy = VmEgressConfig {
            allow: vec!["does-not-exist.invalid".into()],
            deny_all_other: true,
        };
        let resolver = StubResolver::new(&[]);
        let rules = generate_rules(&policy, "10.100.0.2", "eth0", "ember:a3f4", &resolver);
        assert_eq!(rules.len(), 1);
        assert!(rules[0].args.contains(&"DROP".to_string()));
    }

    #[test]
    fn duplicate_destinations_deduplicated() {
        // Two hostnames resolving to the same IP, plus the same IP as
        // a literal, should only produce one ACCEPT rule.
        let policy = VmEgressConfig {
            allow: vec![
                "api.anthropic.com".into(),
                "api2.anthropic.com".into(),
                "1.2.3.4".into(),
            ],
            deny_all_other: false,
        };
        let resolver = StubResolver::new(&[
            ("api.anthropic.com", &["1.2.3.4"]),
            ("api2.anthropic.com", &["1.2.3.4"]),
        ]);
        let rules = generate_rules(&policy, "10.100.0.2", "eth0", "ember:a3f4", &resolver);
        assert_eq!(rules.len(), 1, "should dedupe to a single ACCEPT");
    }

    #[test]
    fn order_is_accepts_then_drop() {
        let policy = VmEgressConfig {
            allow: vec!["10.0.0.0/8".into(), "1.2.3.4".into()],
            deny_all_other: true,
        };
        let resolver = StubResolver::new(&[]);
        let rules = generate_rules(&policy, "10.100.0.2", "eth0", "ember:a3f4", &resolver);
        // First two are ACCEPT-flavored, last is DROP.
        assert!(rules[0].args.contains(&"ACCEPT".to_string()));
        assert!(rules[1].args.contains(&"ACCEPT".to_string()));
        assert!(rules[2].args.contains(&"DROP".to_string()));
    }

    #[test]
    fn whitespace_and_empty_entries_skipped() {
        let policy = VmEgressConfig {
            allow: vec!["".into(), "   ".into(), "1.2.3.4".into()],
            deny_all_other: false,
        };
        let resolver = StubResolver::new(&[]);
        let rules = generate_rules(&policy, "10.100.0.2", "eth0", "ember:a3f4", &resolver);
        assert_eq!(rules.len(), 1);
    }

    /// Helper that exercises the same arg transform `remove_one` uses,
    /// without actually invoking iptables. Lets us assert the shape
    /// is right (correctness of the `-A/-I → -D` substitution is the
    /// security-relevant property: if the delete arg shape is wrong,
    /// rules leak forever on every VM stop).
    fn delete_shape(rule: &EgressRule) -> Vec<String> {
        let owned = rule.clone().into_args();
        let mut del: Vec<String> = Vec::with_capacity(owned.len());
        let mut it = owned.into_iter();
        match it.next().as_deref() {
            Some("-I") => {
                del.push("-D".into());
                if let Some(chain) = it.next() {
                    del.push(chain);
                }
                let _ = it.next();
            }
            Some("-A") => {
                del.push("-D".into());
                if let Some(chain) = it.next() {
                    del.push(chain);
                }
            }
            Some(other) => del.push(other.to_string()),
            None => {}
        }
        del.extend(it);
        del
    }

    #[test]
    fn delete_shape_strips_insert_position() {
        let r = accept_rule("10.100.0.2/32", "10.0.0.0/8", "eth0", "ember:a3f4");
        let del = delete_shape(&r);
        // First two args become `-D FORWARD` (no `1` position).
        assert_eq!(&del[..2], &["-D", "FORWARD"]);
        // The remainder matches the original from `-s` onwards.
        let orig = r.into_args();
        // orig is `-I FORWARD 1 -s …`; the delete-form drops the
        // three-token `-I FORWARD <pos>` head, so positions 3.. match.
        assert_eq!(&del[2..], &orig[3..]);
    }

    #[test]
    fn delete_shape_converts_append_drop() {
        let r = drop_rule("10.100.0.2/32", "eth0", "ember:a3f4");
        let del = delete_shape(&r);
        assert_eq!(&del[..2], &["-D", "FORWARD"]);
        let orig = r.into_args();
        // `-A FORWARD …` → `-D FORWARD …`; positions 2.. unchanged.
        assert_eq!(&del[2..], &orig[2..]);
    }

    #[test]
    fn comment_appears_exactly_once_per_rule() {
        let policy = VmEgressConfig {
            allow: vec!["10.0.0.0/8".into()],
            deny_all_other: true,
        };
        let resolver = StubResolver::new(&[]);
        let rules = generate_rules(&policy, "10.100.0.2", "eth0", "ember:a3f4", &resolver);
        for r in &rules {
            let n = r.args.iter().filter(|s| s.as_str() == "--comment").count();
            assert_eq!(
                n, 1,
                "rule should carry exactly one --comment: {:?}",
                r.args
            );
            assert!(r.args.contains(&"ember:a3f4".to_string()));
        }
    }
}

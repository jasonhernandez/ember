//! iptables NAT/masquerade rule management.
//!
//! Each VM gets three iptables rules for outbound network connectivity:
//!
//! 1. **POSTROUTING MASQUERADE** — rewrites guest source IP for outbound traffic
//! 2. **FORWARD (outbound)** — allows traffic from TAP device to WAN interface
//! 3. **FORWARD (inbound)** — allows established/related return traffic from WAN to TAP
//!
//! Rules are added on VM start and removed on VM stop/delete. The `remove_rules`
//! function is idempotent — it silently ignores errors when rules don't exist.

use std::process::Command;

use ember_core::error::{Error, Result};

/// iptables comment that scopes rule cleanup to one ember install.
///
/// `Some(ns)` → `ember:{ns}`, embedded via `-m comment --comment` in
/// every rule so `-D` only matches *this* install's rules. `None`
/// returns the empty string, which [`with_comment`] uses as the
/// signal to omit the `-m comment` match entirely — older binaries
/// added rules without a comment match, so emitting one on legacy
/// installs would make `iptables -D` silently no-op and rules would
/// accumulate forever. Empty preserves the original rule shape.
pub fn comment(instance_id: Option<&str>) -> String {
    match instance_id {
        None => String::new(),
        Some(id) => format!("ember:{id}"),
    }
}

/// Add iptables NAT and forwarding rules for a VM.
///
/// Creates rules that together give the guest outbound internet access
/// through the host's WAN interface via masquerading (SNAT):
///
/// ```text
/// -t nat -A POSTROUTING -s <guest_ip>/32 -o <wan_iface> [-m comment --comment <c>] -j MASQUERADE
/// -A FORWARD -i <tap_device> -o <wan_iface> [-m comment --comment <c>] -j ACCEPT       (omitted when policy_enforced)
/// -A FORWARD -i <wan_iface> -o <tap_device> -m conntrack --ctstate RELATED,ESTABLISHED [-m comment --comment <c>] -j ACCEPT
/// ```
///
/// `comment` is a per-installation tag (e.g. `ember:a3f4`) embedded in
/// every rule via the `comment` match. It lets cleanup scope deletions
/// to this installation's rules and lets users grep `iptables-save` for
/// "rules ember put here". An empty `comment` skips the match entirely
/// so rules added by older ember binaries (which never tagged anything)
/// stay byte-for-byte identical and remain matchable by `remove_rules`.
///
/// `policy_enforced` (SEC-263): when the VM has an egress policy with
/// `deny_all_other` active, the egress module already supplies per-VM
/// allow ACCEPTs (inserted at `-I FORWARD 1`) and a trailing
/// `-A FORWARD … -j DROP`. NAT's own per-VM blanket `-A FORWARD -i tap
/// -o wan -j ACCEPT` would otherwise sit between them and short-circuit
/// the DROP, silently turning `deny_all_other` into a no-op. Set
/// `policy_enforced = true` to skip that blanket rule; the egress
/// allow rules carry approved traffic and the DROP catches the rest.
/// The conntrack return rule is unaffected — return traffic for
/// allowed flows still needs to be accepted.
pub fn add_rules(
    tap_device: &str,
    guest_ip: &str,
    wan_iface: &str,
    comment: &str,
    policy_enforced: bool,
) -> Result<()> {
    for rule in plan_add(tap_device, guest_ip, wan_iface, comment, policy_enforced) {
        let borrowed: Vec<&str> = rule.iter().map(String::as_str).collect();
        iptables(&borrowed)?;
    }
    Ok(())
}

/// Pure: the argv list `add_rules` would hand to iptables, in order.
///
/// Factored out so the chain-order regression for SEC-263 (the blanket
/// FORWARD ACCEPT must not be present when `policy_enforced`) can be
/// asserted without driving iptables. Same parameters and semantics as
/// [`add_rules`].
pub fn plan_add(
    tap_device: &str,
    guest_ip: &str,
    wan_iface: &str,
    comment: &str,
    policy_enforced: bool,
) -> Vec<Vec<String>> {
    let guest_cidr = format!("{guest_ip}/32");
    let mut out: Vec<Vec<String>> = Vec::new();

    out.push(
        with_comment(
            &[
                "-t",
                "nat",
                "-A",
                "POSTROUTING",
                "-s",
                &guest_cidr,
                "-o",
                wan_iface,
            ],
            comment,
            &["-j", "MASQUERADE"],
        )
        .iter()
        .map(|s| (*s).to_string())
        .collect(),
    );

    if !policy_enforced {
        out.push(
            with_comment(
                &["-A", "FORWARD", "-i", tap_device, "-o", wan_iface],
                comment,
                &["-j", "ACCEPT"],
            )
            .iter()
            .map(|s| (*s).to_string())
            .collect(),
        );
    }

    out.push(
        with_comment(
            &[
                "-A",
                "FORWARD",
                "-i",
                wan_iface,
                "-o",
                tap_device,
                "-m",
                "conntrack",
                "--ctstate",
                "RELATED,ESTABLISHED",
            ],
            comment,
            &["-j", "ACCEPT"],
        )
        .iter()
        .map(|s| (*s).to_string())
        .collect(),
    );

    out
}

/// Remove iptables NAT and forwarding rules for a VM.
///
/// Mirrors [`add_rules`] but uses `-D` (delete) instead of `-A` (append).
/// Idempotent — silently ignores errors when rules don't exist. The
/// `comment` argument must match the value passed to [`add_rules`];
/// iptables compares the full rule including the comment match, so a
/// wrong tag turns the delete into a no-op rather than removing
/// another install's rule.
///
/// The blanket forward `-D` is always attempted regardless of
/// `policy_enforced` — `iptables -D` of a non-existent rule is a
/// silent no-op (handled in [`iptables_delete`]), so an over-broad
/// delete is harmless. This also self-heals VMs that pre-date the
/// SEC-263 fix where the blanket rule may still be present.
pub fn remove_rules(
    tap_device: &str,
    guest_ip: &str,
    wan_iface: &str,
    comment: &str,
) -> Result<()> {
    let guest_cidr = format!("{guest_ip}/32");

    let _ = iptables_delete(&with_comment(
        &[
            "-t",
            "nat",
            "-D",
            "POSTROUTING",
            "-s",
            &guest_cidr,
            "-o",
            wan_iface,
        ],
        comment,
        &["-j", "MASQUERADE"],
    ));

    let _ = iptables_delete(&with_comment(
        &["-D", "FORWARD", "-i", tap_device, "-o", wan_iface],
        comment,
        &["-j", "ACCEPT"],
    ));

    let _ = iptables_delete(&with_comment(
        &[
            "-D",
            "FORWARD",
            "-i",
            wan_iface,
            "-o",
            tap_device,
            "-m",
            "conntrack",
            "--ctstate",
            "RELATED,ESTABLISHED",
        ],
        comment,
        &["-j", "ACCEPT"],
    ));

    Ok(())
}

/// Splice `-m comment --comment <comment>` between rule head and tail
/// when `comment` is non-empty. Empty comment yields the unwrapped
/// rule, matching what older ember binaries emitted byte-for-byte —
/// crucial because iptables compares full rules during `-D`.
fn with_comment<'a>(head: &[&'a str], comment: &'a str, tail: &[&'a str]) -> Vec<&'a str> {
    let mut out = Vec::with_capacity(head.len() + tail.len() + 4);
    out.extend_from_slice(head);
    if !comment.is_empty() {
        out.extend_from_slice(&["-m", "comment", "--comment", comment]);
    }
    out.extend_from_slice(tail);
    out
}

/// Enable IPv4 forwarding via sysctl.
///
/// This is required once before any VM can route traffic through the host.
/// Safe to call multiple times — sysctl is idempotent.
pub fn enable_ip_forwarding() -> Result<()> {
    let output = Command::new("sysctl")
        .args(["-w", "net.ipv4.ip_forward=1"])
        .output()
        .map_err(|e| Error::CommandExec {
            command: "sysctl".into(),
            source: e,
        })?;
    Error::check_command("sysctl", output)?;
    Ok(())
}

/// Run an iptables command, returning an error on failure.
///
/// Exposed within the crate so the egress module can drive
/// pre-generated rules without re-implementing process wiring.
pub(crate) fn iptables(args: &[&str]) -> Result<()> {
    let output = Command::new("iptables")
        .args(args)
        .output()
        .map_err(|e| Error::CommandExec {
            command: "iptables".into(),
            source: e,
        })?;
    Error::check_command("iptables", output)?;
    Ok(())
}

/// Run an iptables delete command, removing ALL matching instances.
///
/// `iptables -D` only removes the first match. If the same rule was
/// added multiple times (e.g. a test VM and a manual VM both at the
/// same IP), we need to loop until all copies are gone.
///
/// Silently ignores "rule doesn't exist" errors for idempotent cleanup.
pub(crate) fn iptables_delete(args: &[&str]) -> Result<()> {
    loop {
        let output =
            Command::new("iptables")
                .args(args)
                .output()
                .map_err(|e| Error::CommandExec {
                    command: "iptables".into(),
                    source: e,
                })?;

        if output.status.success() {
            // Deleted one instance — loop to catch duplicates.
            continue;
        }

        let stderr = String::from_utf8_lossy(&output.stderr);
        if stderr.contains("does a matching rule exist") || stderr.contains("No chain/target/match")
        {
            // No more matching rules — done.
            return Ok(());
        }
        return Err(Error::Network(format!(
            "iptables failed: {}",
            stderr.trim()
        )));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn comment_for_new_install_tags_namespace() {
        assert_eq!(comment(Some("a3f4")), "ember:a3f4");
    }

    /// Locked: legacy mode must return an empty string so the rule
    /// shape stays byte-for-byte identical to what older binaries
    /// emitted (no `-m comment` match), or `iptables -D` silently
    /// no-ops on upgraded hosts.
    #[test]
    fn comment_for_legacy_install_is_empty() {
        assert_eq!(comment(None), "");
    }

    #[test]
    fn with_comment_skips_match_when_empty() {
        // Legacy mode (empty comment) must produce byte-for-byte the
        // same rule the old binary added, otherwise `iptables -D`
        // won't match existing rules on upgraded hosts.
        let args = with_comment(&["-A", "FORWARD", "-i", "tap0"], "", &["-j", "ACCEPT"]);
        assert_eq!(args, vec!["-A", "FORWARD", "-i", "tap0", "-j", "ACCEPT"]);
    }

    /// SEC-263 regression guard: when an egress policy with
    /// `deny_all_other` is active, NAT must not append its per-VM
    /// blanket `-A FORWARD -i tap -o wan -j ACCEPT`. If it does, it
    /// sits ahead of the egress DROP and silently turns
    /// `deny_all_other` into a no-op.
    #[test]
    fn plan_add_omits_blanket_forward_when_policy_enforced() {
        let plan = plan_add("tap0", "10.100.0.2", "eth0", "ember:a3f4", true);
        // The blanket rule has `-i tap0 -o eth0` (forward direction).
        // The conntrack return rule has `-i eth0 -o tap0` — we want
        // that one to remain so return traffic for allowed flows is
        // accepted.
        let has_blanket = plan.iter().any(|r| {
            let s = r.as_slice();
            // Look for the exact "forward direction" pattern.
            s.windows(2).any(|w| w == ["-i", "tap0"])
                && s.windows(2).any(|w| w == ["-o", "eth0"])
                && !s.iter().any(|tok| tok == "conntrack")
        });
        assert!(
            !has_blanket,
            "blanket per-VM forward ACCEPT must be omitted when egress \
             policy enforces deny_all_other: {:?}",
            plan
        );
        // Conntrack return rule must still be present.
        let has_return = plan.iter().any(|r| r.iter().any(|tok| tok == "conntrack"));
        assert!(
            has_return,
            "conntrack return ACCEPT must be kept even when policy_enforced: {:?}",
            plan
        );
    }

    /// And without policy enforcement, the blanket forward ACCEPT
    /// must still be present (no behavior change for VMs without an
    /// egress policy or with `deny_all_other: false`).
    #[test]
    fn plan_add_keeps_blanket_forward_when_not_policy_enforced() {
        let plan = plan_add("tap0", "10.100.0.2", "eth0", "ember:a3f4", false);
        let has_blanket = plan.iter().any(|r| {
            let s = r.as_slice();
            s.windows(2).any(|w| w == ["-i", "tap0"])
                && s.windows(2).any(|w| w == ["-o", "eth0"])
                && !s.iter().any(|tok| tok == "conntrack")
        });
        assert!(
            has_blanket,
            "blanket per-VM forward ACCEPT must be present without policy enforcement: {:?}",
            plan
        );
    }

    #[test]
    fn with_comment_inserts_comment_match_when_non_empty() {
        let args = with_comment(
            &["-A", "FORWARD", "-i", "tap0"],
            "ember:a3f4",
            &["-j", "ACCEPT"],
        );
        assert_eq!(
            args,
            vec![
                "-A",
                "FORWARD",
                "-i",
                "tap0",
                "-m",
                "comment",
                "--comment",
                "ember:a3f4",
                "-j",
                "ACCEPT"
            ]
        );
    }
}

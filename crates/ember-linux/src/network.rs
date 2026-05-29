pub mod dns;
pub mod egress;
pub mod ip;
pub mod nat;
pub mod tap;
pub mod wan;

use ember_core::config::GlobalConfig;
use ember_core::state::store::StateStore;
use ember_core::state::vm::VmMetadata;

/// Best-effort cleanup of networking resources for a VM (Linux only).
///
/// The iptables comment is derived via [`nat::comment`] from the
/// install's namespace so the `-D` calls only match this
/// installation's rules even when another ember install on the same
/// host has rules for the same TAP/IP.
///
/// If the VM has a persisted egress policy (SEC-263), its rules are
/// regenerated and deleted using the same `(tap_ip, wan_iface,
/// comment)` triple used at setup. DNS is re-resolved at this point
/// — a name whose records changed between start and stop may leak
/// the obsolete IPs as orphaned ACCEPT rules; the trailing DROP is
/// shape-stable and always gets cleaned.
///
/// TODO(SEC-263): replace the re-resolve+regenerate cleanup with a
/// deletion keyed on a stable per-VM iptables comment tag (mirroring
/// what `nat::remove_rules` does), so cleanup is hostname-independent
/// and orphaned ACCEPT rules can't leak when a record changes between
/// start and stop. Out of scope for the deny-all-other bug fix.
pub fn cleanup(store: &StateStore, config: &GlobalConfig, vm: &VmMetadata) {
    let net_info = match vm.network.as_ref() {
        Some(n) => n,
        None => return,
    };
    let wan_iface = net_info.wan_iface.clone().or_else(|| wan::detect().ok());
    let comment = nat::comment(config.instance_namespace());

    if let Some(ref wan_iface) = wan_iface {
        if let Some(ref policy) = vm.egress {
            if !policy.is_empty() {
                let resolver = egress::SystemResolver;
                let rules = egress::generate_rules(
                    policy,
                    &net_info.guest_ip,
                    wan_iface,
                    &comment,
                    &resolver,
                );
                egress::remove_rules(&rules);
            }
        }
        let _ = nat::remove_rules(
            &net_info.tap_device,
            &net_info.guest_ip,
            wan_iface,
            &comment,
        );
    }
    let _ = tap::delete(&net_info.tap_device);
    let _ = ip::release(store, &vm.name);
}

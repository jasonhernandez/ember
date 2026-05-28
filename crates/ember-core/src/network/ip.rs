//! IP allocation for VM networking.
//!
//! Two allocation strategies, picked per backend:
//!
//! * [`allocate`] — Linux: hand out `/30` blocks for point-to-point TAP
//!   routing. Each VM gets its own host (.1) and guest (.2) IPs on a
//!   dedicated /30. With a `/16` base, ~16,384 VMs fit.
//! * [`allocate_single`] — macOS: hand out single `/32` addresses on a
//!   shared subnet (vmnet's `192.168.64.0/24`, optionally sub-sliced
//!   into per-installation /27s). All VMs sit on the same L2 segment
//!   behind one shared gateway, so a /30 P2P link per VM would waste
//!   75% of the address space.
//!
//! Both persist into the same `state.db` SQLite database (see
//! [`crate::state::db`]). The schema's `(subnet, block_index) PRIMARY KEY`
//! plus `vm_name UNIQUE` constraint makes double-allocation structurally
//! impossible — a second `INSERT` for the same slot fails with a
//! constraint violation, so the allocator can't accidentally hand out
//! the same IP twice even if the read-modify-write logic regresses.
//!
//! An installation uses exactly one strategy across its lifetime. The
//! persisted `subnet` column (singleton invariant: every row's subnet
//! must match the caller's subnet) plus the `single_address` boolean
//! enforce this — switching strategies or subnets mid-install would
//! re-stamp every existing entry's IPs, so the allocator rejects both.
//!
//! Concurrency model: each call opens a fresh SQLite connection and runs
//! the allocation under `BEGIN IMMEDIATE`, which acquires the write lock
//! at transaction start (not lazily on first write). This eliminates the
//! SELECT/INSERT TOCTOU window that the prior JSON-with-flock store had,
//! which could hand the same /30 block to multiple parallel `vm start`
//! invocations after a crash recovery (SEC-459).

use std::collections::HashSet;
use std::net::Ipv4Addr;

use rusqlite::params;

use crate::error::{Error, Result};
use crate::state::db;
use crate::state::store::StateStore;

/// A single IP allocation for one VM.
#[derive(Debug, Clone, PartialEq)]
pub struct IpAllocation {
    /// Index within the base subnet. Unit is strategy-dependent:
    /// 4 addresses for [`allocate`] (/30 P2P), 1 for [`allocate_single`].
    pub block_index: u32,
    /// Host-side IP. For [`allocate`]: first usable address in the /30
    /// (e.g., "10.100.0.1"). For [`allocate_single`]: the shared gateway
    /// passed by the caller.
    pub host_ip: String,
    /// Guest-side IP.
    pub guest_ip: String,
    /// Netmask for the link.
    pub netmask: String,
}

/// Default base subnet when none is configured.
pub const DEFAULT_SUBNET: &str = "10.100.0.0/16";

/// Netmask for a /30 subnet.
const NETMASK_30: &str = "255.255.255.252";

/// Strategy column values for `network_allocations.single_address`.
const STRATEGY_P2P: i64 = 0;
const STRATEGY_SINGLE: i64 = 1;

/// Parse a CIDR subnet string into (base address, prefix length).
///
/// Accepts any prefix `/0`..`/32`; per-strategy constraints (e.g.
/// `/30` minimum for [`allocate`]) are enforced at the call site.
fn parse_cidr(cidr: &str) -> Result<(Ipv4Addr, u8)> {
    let (ip_str, prefix_str) = cidr
        .split_once('/')
        .ok_or_else(|| Error::Network(format!("invalid CIDR notation: {cidr}")))?;

    let ip: Ipv4Addr = ip_str
        .parse()
        .map_err(|e| Error::Network(format!("invalid IP in CIDR '{cidr}': {e}")))?;

    let prefix: u8 = prefix_str
        .parse()
        .map_err(|e| Error::Network(format!("invalid prefix in CIDR '{cidr}': {e}")))?;

    if prefix > 32 {
        return Err(Error::Network(format!(
            "invalid CIDR prefix /{prefix}: must be 0..=32"
        )));
    }

    // Verify the IP is properly masked (no host bits set).
    let ip_u32 = u32::from(ip);
    let mask = if prefix == 0 {
        0u32
    } else if prefix == 32 {
        u32::MAX
    } else {
        !((1u32 << (32 - prefix)) - 1)
    };
    if ip_u32 & mask != ip_u32 {
        return Err(Error::Network(format!(
            "IP {ip} has host bits set for /{prefix}"
        )));
    }

    Ok((ip, prefix))
}

/// Maximum number of /30 blocks that fit in a given prefix.
fn max_blocks(prefix_len: u8) -> u32 {
    // A /30 has 4 addresses. A /prefix has 2^(32-prefix) addresses.
    // max_blocks = 2^(32-prefix) / 4 = 2^(30-prefix)
    1u32 << (30 - prefix_len)
}

/// Compute the IP addresses for a given /30 block.
fn block_ips(base: Ipv4Addr, block_index: u32) -> IpAllocation {
    let base_u32 = u32::from(base);
    let network = base_u32 + block_index * 4;
    let host = Ipv4Addr::from(network + 1);
    let guest = Ipv4Addr::from(network + 2);

    IpAllocation {
        block_index,
        host_ip: host.to_string(),
        guest_ip: guest.to_string(),
        netmask: NETMASK_30.to_string(),
    }
}

/// Verify the persisted subnet (and the strategy column) match the
/// caller's. Returns `Err` on mismatch — the install would otherwise
/// re-stamp every existing entry's IPs.
fn verify_singleton_subnet(
    tx: &rusqlite::Transaction<'_>,
    subnet: &str,
    expected_strategy: i64,
) -> Result<()> {
    let row: Option<(String, i64)> = tx
        .query_row(
            "SELECT subnet, single_address FROM network_allocations LIMIT 1",
            [],
            |r| Ok((r.get::<_, String>(0)?, r.get::<_, i64>(1)?)),
        )
        .ok();
    if let Some((existing_subnet, existing_strategy)) = row {
        if existing_subnet != subnet {
            return Err(Error::Network(format!(
                "subnet mismatch: state has '{existing_subnet}', requested '{subnet}'"
            )));
        }
        if existing_strategy != expected_strategy {
            return Err(Error::Network(format!(
                "allocation strategy mismatch: state uses {} but caller requested {}",
                strategy_name(existing_strategy),
                strategy_name(expected_strategy),
            )));
        }
    }
    Ok(())
}

fn strategy_name(s: i64) -> &'static str {
    match s {
        STRATEGY_P2P => "/30 P2P (allocate)",
        STRATEGY_SINGLE => "single-address (allocate_single)",
        _ => "unknown",
    }
}

/// Allocate the lowest free /30 block for a VM, skipping any `exclude`d
/// block indexes.
///
/// `exclude` lets the VM-start retry path (SEC-419) route around a "poisoned"
/// slot: when a VM fails to boot with a transient vmnet/VZ crash, the macOS
/// vmnet framework can keep stale state for that slot so every VM assigned to
/// it crashes the same way. The retry releases the slot, adds its block index
/// to `exclude`, and re-allocates — getting the *next* free block instead of
/// the same poisoned one. Callers that don't need poisoning pass
/// `&HashSet::new()`.
///
/// `exclude` is in-memory and supplied per call: poisoning is scoped to the
/// retry loop within a single `vm start`/`fork` invocation and is not
/// persisted.
///
/// The full read-modify-write runs under a single `BEGIN IMMEDIATE`
/// transaction, so parallel allocators serialize at the database layer
/// rather than racing on a JSON file. The `(subnet, block_index)` primary
/// key plus the `vm_name UNIQUE` constraint make double-allocation
/// structurally impossible: even if the `find` logic regressed, a duplicate
/// `INSERT` would fail with `SQLITE_CONSTRAINT_PRIMARYKEY`.
pub fn allocate(
    store: &StateStore,
    subnet: &str,
    vm_name: &str,
    exclude: &HashSet<u32>,
) -> Result<IpAllocation> {
    let (base, prefix) = parse_cidr(subnet)?;
    if prefix > 30 {
        return Err(Error::Network(format!(
            "subnet /{prefix} is too small for /30 allocations"
        )));
    }
    let max = max_blocks(prefix);

    let mut conn = db::open(store.root())?;
    let tx = conn.transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;

    verify_singleton_subnet(&tx, subnet, STRATEGY_P2P)?;

    let used: HashSet<u32> = tx
        .prepare("SELECT block_index FROM network_allocations WHERE subnet = ?1")?
        .query_map(params![subnet], |r| r.get::<_, u32>(0))?
        .collect::<std::result::Result<_, _>>()?;

    let block_index = (0..max)
        .find(|i| !used.contains(i) && !exclude.contains(i))
        .ok_or_else(|| {
            // Distinguish "subnet full" from "all free blocks poisoned" so the
            // operator can tell a capacity problem from a vmnet-state problem.
            if exclude.is_empty() {
                Error::Network(format!(
                    "no free /30 blocks in {subnet} (all {max} blocks allocated)"
                ))
            } else {
                Error::Network(format!(
                    "no usable /30 blocks in {subnet}: {} allocated, {} poisoned \
                     (transient VZ start failures) — try 'ember network reset' or reboot",
                    used.len(),
                    exclude.len()
                ))
            }
        })?;

    tx.execute(
        "INSERT INTO network_allocations (block_index, subnet, vm_name, single_address) \
         VALUES (?1, ?2, ?3, ?4)",
        params![block_index, subnet, vm_name, STRATEGY_P2P],
    )?;
    tx.commit()?;

    Ok(block_ips(base, block_index))
}

/// Allocate a single /32 address for a VM in a shared subnet, skipping any
/// `exclude`d slots and the addresses in `reserved`.
///
/// Used by macOS where vmnet provides a shared L2 bridge — every guest
/// sits on the same subnet behind one gateway, so a /30 P2P link per
/// VM (the [`allocate`] strategy) would waste 75% of the address
/// space. `block_index` here means "address offset from the subnet
/// base", so a /27 holds 32 candidate slots.
///
/// `host_ip` is returned to the caller as-is and conventionally
/// contains the shared gateway. `reserved` lists addresses the
/// allocator must never hand out — typically the surrounding /24's
/// network, broadcast, and gateway when the caller carved a /27 out
/// of vmnet's /24. `exclude` carries the poisoning set (SEC-419);
/// callers that don't need poisoning pass `&HashSet::new()`.
pub fn allocate_single(
    store: &StateStore,
    subnet: &str,
    vm_name: &str,
    host_ip: &str,
    netmask: &str,
    reserved: &[Ipv4Addr],
    exclude: &HashSet<u32>,
) -> Result<IpAllocation> {
    let (base, prefix) = parse_cidr(subnet)?;
    let max = 1u32 << (32 - prefix);
    let base_u32 = u32::from(base);

    let mut conn = db::open(store.root())?;
    let tx = conn.transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;

    verify_singleton_subnet(&tx, subnet, STRATEGY_SINGLE)?;

    let used: HashSet<u32> = tx
        .prepare("SELECT block_index FROM network_allocations WHERE subnet = ?1")?
        .query_map(params![subnet], |r| r.get::<_, u32>(0))?
        .collect::<std::result::Result<_, _>>()?;

    // Walk the subnet looking for an unallocated, non-reserved, non-excluded
    // slot. Skipping reserved addresses keeps the gateway (and the wider
    // /24's network and broadcast when carved into /27s) un-handout-able
    // without the caller having to seed the table.
    let block_index = (0..max)
        .find(|i| {
            if used.contains(i) || exclude.contains(i) {
                return false;
            }
            let addr = Ipv4Addr::from(base_u32 + i);
            !reserved.contains(&addr)
        })
        .ok_or_else(|| {
            if exclude.is_empty() {
                Error::Network(format!(
                    "no free addresses in {subnet} ({} allocated, {} reserved)",
                    used.len(),
                    reserved.len()
                ))
            } else {
                Error::Network(format!(
                    "no usable addresses in {subnet}: {} allocated, {} reserved, {} poisoned \
                     (transient VZ start failures) — try 'ember network reset' or reboot",
                    used.len(),
                    reserved.len(),
                    exclude.len()
                ))
            }
        })?;

    tx.execute(
        "INSERT INTO network_allocations (block_index, subnet, vm_name, single_address) \
         VALUES (?1, ?2, ?3, ?4)",
        params![block_index, subnet, vm_name, STRATEGY_SINGLE],
    )?;
    tx.commit()?;

    let guest_ip = Ipv4Addr::from(base_u32 + block_index);
    Ok(IpAllocation {
        block_index,
        host_ip: host_ip.to_string(),
        guest_ip: guest_ip.to_string(),
        netmask: netmask.to_string(),
    })
}

/// Look up the block index currently allocated to `vm_name`, if any.
///
/// Used by the start-retry path (SEC-419) to learn which slot just failed —
/// it must be added to the poison set *before* the allocation is released,
/// otherwise the next [`allocate`]/[`allocate_single`] would hand back the
/// same slot.
pub fn allocated_block(store: &StateStore, vm_name: &str) -> Result<Option<u32>> {
    let conn = db::open(store.root())?;
    let block: Option<u32> = conn
        .query_row(
            "SELECT block_index FROM network_allocations WHERE vm_name = ?1",
            params![vm_name],
            |r| r.get::<_, u32>(0),
        )
        .ok();
    Ok(block)
}

/// One row of the network allocation table, with IPs resolved.
///
/// Returned by [`list_allocations`] for `ember network status`.
#[derive(Debug, Clone, PartialEq)]
pub struct AllocationRow {
    pub block_index: u32,
    pub subnet: String,
    pub vm_name: String,
    pub host_ip: String,
    pub guest_ip: String,
    /// True if this row was created via [`allocate_single`] (single-address
    /// macOS path), false for /30 P2P.
    pub single_address: bool,
}

/// List every recorded allocation, ordered by subnet then block index.
///
/// Powers `ember network status` (SEC-419): operators can see which slots
/// are in use and by which VM without reading the state DB directly.
///
/// IPs are recomputed from `(subnet, block_index, single_address)`. For
/// single-address rows the host IP is reported as the subnet base + 1
/// (a best-effort hint — the actual host IP was supplied by the caller
/// at allocate time and is not persisted; the rendered value is only
/// meaningful for operators' situational awareness).
pub fn list_allocations(store: &StateStore) -> Result<Vec<AllocationRow>> {
    let conn = db::open(store.root())?;
    let mut stmt = conn.prepare(
        "SELECT block_index, subnet, vm_name, single_address FROM network_allocations \
         ORDER BY subnet, block_index",
    )?;
    let rows: Vec<(u32, String, String, i64)> = stmt
        .query_map([], |r| {
            Ok((
                r.get::<_, u32>(0)?,
                r.get::<_, String>(1)?,
                r.get::<_, String>(2)?,
                r.get::<_, i64>(3)?,
            ))
        })?
        .collect::<std::result::Result<_, _>>()?;

    let mut out = Vec::with_capacity(rows.len());
    for (block_index, subnet, vm_name, single_address) in rows {
        let (base, _prefix) = parse_cidr(&subnet)?;
        let (host_ip, guest_ip) = if single_address == STRATEGY_SINGLE {
            let base_u32 = u32::from(base);
            // Host IP isn't persisted on single-address rows; report the
            // subnet base + 1 (the conventional gateway slot) as a hint.
            let host = Ipv4Addr::from(base_u32 + 1);
            let guest = Ipv4Addr::from(base_u32 + block_index);
            (host.to_string(), guest.to_string())
        } else {
            let ips = block_ips(base, block_index);
            (ips.host_ip, ips.guest_ip)
        };
        out.push(AllocationRow {
            block_index,
            subnet,
            vm_name,
            host_ip,
            guest_ip,
            single_address: single_address == STRATEGY_SINGLE,
        });
    }
    Ok(out)
}

/// Release a VM's IP allocation.
///
/// Removes all allocation entries for the given VM name, making the slot
/// available for reuse. Idempotent — does nothing if the VM has no
/// allocation.
pub fn release(store: &StateStore, vm_name: &str) -> Result<()> {
    let conn = db::open(store.root())?;
    conn.execute(
        "DELETE FROM network_allocations WHERE vm_name = ?1",
        params![vm_name],
    )?;
    Ok(())
}

/// Structured description of a single allocator-state divergence.
///
/// Replaces the prior `Vec<String>` representation (SEC-460): the old free-form
/// messages forced consumers to scrape VM names out of strings, which under-
/// flagged multi-VM cases and missed subnet-level anomalies entirely.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Anomaly {
    pub kind: AnomalyKind,
    /// Human-readable rendering, suitable for `eprintln!` to operators.
    pub message: String,
    /// Every VM implicated by this anomaly. Consumers (e.g. `ember vm list`)
    /// flag *all* of these, not just the first one — the prior split-the-
    /// message-on-whitespace heuristic only ever flagged the first match.
    /// Empty for subnet-level anomalies that don't map to a single VM set.
    pub vm_names: Vec<String>,
}

/// What kind of divergence the [`Anomaly`] describes. Carries enough
/// structured detail for callers to render their own messages, key into
/// other state, or surface in machine-readable form.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AnomalyKind {
    /// Two or more allocator rows share the same `(subnet, block_index)`.
    /// Should be impossible under the schema's PRIMARY KEY constraint.
    DuplicateSlot {
        subnet: String,
        block_index: u32,
        count: u32,
    },
    /// The same `vm_name` appears in more than one allocator row. Should
    /// be impossible under the schema's UNIQUE constraint on `vm_name`.
    DuplicateVmName { vm_name: String, count: u32 },
    /// Two VMs report the same `guest_ip` in their metadata. Detected by
    /// `ember vm list` (cross-references `vm_metadata.network.guest_ip`,
    /// which lives outside `network_allocations`), so this kind isn't
    /// produced by `check_invariants` itself — it's part of the public
    /// vocabulary so consumers and renderers can share one type.
    DuplicateGuestIp { ip: String },
}

/// Check that the allocator state is internally consistent.
///
/// Returns the list of detected anomalies (empty list = healthy). Used by
/// `ember vm list` to flag corrupted state — see SEC-459 for the failure
/// mode this catches (allocator-state-vs-running-VM divergence after a
/// crash that bypassed the proper allocator path).
///
/// Returns `Err` if the underlying SQLite store can't be opened or queried.
/// Callers MUST surface this — a swallowed error in a corruption-detection
/// codepath is the worst possible failure mode (silent "all clear" while
/// the very thing we're checking is broken). See SEC-460 for the bug the
/// prior `unwrap_or_default()` callers exhibited.
pub fn check_invariants(store: &StateStore) -> Result<Vec<Anomaly>> {
    let conn = db::open(store.root())?;
    let mut anomalies = Vec::new();

    // Duplicate (subnet, block_index): aggregate vm_names per offending
    // slot via GROUP_CONCAT so we can attribute the anomaly to *every* VM
    // that landed in that slot, not just one (which the prior split-the-
    // message heuristic in `vm list` was forced to settle for).
    let dup_slots: Vec<(String, u32, i64, String)> = conn
        .prepare(
            "SELECT subnet, block_index, COUNT(*) AS n, GROUP_CONCAT(vm_name) AS names
             FROM network_allocations
             GROUP BY subnet, block_index
             HAVING n > 1",
        )?
        .query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)))?
        .collect::<std::result::Result<_, _>>()?;
    for (subnet, block_index, n, names) in dup_slots {
        let vm_names: Vec<String> = names
            .split(',')
            .filter(|s| !s.is_empty())
            .map(|s| s.to_string())
            .collect();
        anomalies.push(Anomaly {
            kind: AnomalyKind::DuplicateSlot {
                subnet: subnet.clone(),
                block_index,
                count: n as u32,
            },
            message: format!(
                "duplicate slot: subnet={subnet} block_index={block_index} count={n} vms={}",
                vm_names.join(",")
            ),
            vm_names,
        });
    }

    let dup_names: Vec<(String, i64)> = conn
        .prepare(
            "SELECT vm_name, COUNT(*) AS n
             FROM network_allocations
             GROUP BY vm_name
             HAVING n > 1",
        )?
        .query_map([], |r| Ok((r.get(0)?, r.get(1)?)))?
        .collect::<std::result::Result<_, _>>()?;
    for (vm_name, n) in dup_names {
        anomalies.push(Anomaly {
            kind: AnomalyKind::DuplicateVmName {
                vm_name: vm_name.clone(),
                count: n as u32,
            },
            message: format!("duplicate vm_name: {vm_name} count={n}"),
            vm_names: vec![vm_name],
        });
    }

    Ok(anomalies)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn no_exclude() -> HashSet<u32> {
        HashSet::new()
    }

    // --- CIDR parsing ---

    #[test]
    fn parse_cidr_valid() {
        let (ip, prefix) = parse_cidr("10.100.0.0/16").unwrap();
        assert_eq!(ip, Ipv4Addr::new(10, 100, 0, 0));
        assert_eq!(prefix, 16);
    }

    #[test]
    fn parse_cidr_slash_30() {
        let (ip, prefix) = parse_cidr("192.168.1.0/30").unwrap();
        assert_eq!(ip, Ipv4Addr::new(192, 168, 1, 0));
        assert_eq!(prefix, 30);
    }

    #[test]
    fn parse_cidr_accepts_up_to_slash_32() {
        assert!(parse_cidr("10.0.0.0/31").is_ok());
        assert!(parse_cidr("192.168.64.0/27").is_ok());
        assert!(parse_cidr("10.0.0.5/32").is_ok());
    }

    #[test]
    fn parse_cidr_rejects_slash_above_32() {
        assert!(parse_cidr("10.0.0.0/33").is_err());
    }

    #[test]
    fn parse_cidr_rejects_host_bits() {
        assert!(parse_cidr("10.100.0.1/16").is_err());
    }

    #[test]
    fn parse_cidr_rejects_no_slash() {
        assert!(parse_cidr("10.100.0.0").is_err());
    }

    // --- Block math ---

    #[test]
    fn max_blocks_slash_16() {
        assert_eq!(max_blocks(16), 16384);
    }

    #[test]
    fn max_blocks_slash_24() {
        assert_eq!(max_blocks(24), 64);
    }

    #[test]
    fn max_blocks_slash_30() {
        assert_eq!(max_blocks(30), 1);
    }

    #[test]
    fn block_ips_first() {
        let alloc = block_ips(Ipv4Addr::new(10, 100, 0, 0), 0);
        assert_eq!(alloc.host_ip, "10.100.0.1");
        assert_eq!(alloc.guest_ip, "10.100.0.2");
        assert_eq!(alloc.netmask, "255.255.255.252");
    }

    #[test]
    fn block_ips_second() {
        let alloc = block_ips(Ipv4Addr::new(10, 100, 0, 0), 1);
        assert_eq!(alloc.host_ip, "10.100.0.5");
        assert_eq!(alloc.guest_ip, "10.100.0.6");
    }

    #[test]
    fn block_ips_wraps_octet() {
        let alloc = block_ips(Ipv4Addr::new(10, 100, 0, 0), 64);
        assert_eq!(alloc.host_ip, "10.100.1.1");
        assert_eq!(alloc.guest_ip, "10.100.1.2");
    }

    // --- Allocate / release with state store (P2P strategy) ---

    fn test_store() -> (tempfile::TempDir, StateStore) {
        let dir = tempfile::tempdir().unwrap();
        let store = StateStore::new(dir.path().to_path_buf());
        store.init().unwrap();
        (dir, store)
    }

    #[test]
    fn allocate_first_block() {
        let (_dir, store) = test_store();
        let alloc = allocate(&store, "10.100.0.0/16", "vm1", &no_exclude()).unwrap();
        assert_eq!(alloc.block_index, 0);
        assert_eq!(alloc.host_ip, "10.100.0.1");
        assert_eq!(alloc.guest_ip, "10.100.0.2");
    }

    #[test]
    fn allocate_sequential() {
        let (_dir, store) = test_store();
        let a1 = allocate(&store, "10.100.0.0/16", "vm1", &no_exclude()).unwrap();
        let a2 = allocate(&store, "10.100.0.0/16", "vm2", &no_exclude()).unwrap();
        let a3 = allocate(&store, "10.100.0.0/16", "vm3", &no_exclude()).unwrap();

        assert_eq!(a1.block_index, 0);
        assert_eq!(a2.block_index, 1);
        assert_eq!(a3.block_index, 2);

        assert_eq!(a1.host_ip, "10.100.0.1");
        assert_eq!(a2.host_ip, "10.100.0.5");
        assert_eq!(a3.host_ip, "10.100.0.9");
    }

    #[test]
    fn allocate_reuses_released_block() {
        let (_dir, store) = test_store();
        allocate(&store, "10.100.0.0/16", "vm1", &no_exclude()).unwrap();
        allocate(&store, "10.100.0.0/16", "vm2", &no_exclude()).unwrap();
        allocate(&store, "10.100.0.0/16", "vm3", &no_exclude()).unwrap();

        release(&store, "vm2").unwrap();

        let a4 = allocate(&store, "10.100.0.0/16", "vm4", &no_exclude()).unwrap();
        assert_eq!(a4.block_index, 1);
        assert_eq!(a4.host_ip, "10.100.0.5");
    }

    #[test]
    fn allocate_exhausts_small_subnet() {
        let (_dir, store) = test_store();
        allocate(&store, "192.168.1.0/30", "vm1", &no_exclude()).unwrap();
        let err = allocate(&store, "192.168.1.0/30", "vm2", &no_exclude()).unwrap_err();
        assert!(err.to_string().contains("no free /30 blocks"));
    }

    #[test]
    fn allocate_rejects_too_narrow_subnet() {
        let (_dir, store) = test_store();
        let err = allocate(&store, "10.0.0.0/31", "vm1", &no_exclude()).unwrap_err();
        assert!(matches!(err, Error::Network(_)));
    }

    // --- SEC-419: poison-aware allocation + slot lookup ---

    #[test]
    fn allocate_skips_poisoned_slot() {
        let (_dir, store) = test_store();
        let poisoned = HashSet::from([0u32]);
        let alloc = allocate(&store, "10.100.0.0/16", "vm1", &poisoned).unwrap();
        assert_eq!(alloc.block_index, 1);
        assert_eq!(alloc.host_ip, "10.100.0.5");
    }

    #[test]
    fn allocate_skips_multiple_poisoned_slots() {
        let (_dir, store) = test_store();
        let poisoned = HashSet::from([0u32, 1, 2]);
        let alloc = allocate(&store, "10.100.0.0/16", "vm1", &poisoned).unwrap();
        assert_eq!(alloc.block_index, 3);
    }

    #[test]
    fn allocate_all_poisoned_errors_distinctly() {
        let (_dir, store) = test_store();
        let poisoned = HashSet::from([0u32]);
        let err = allocate(&store, "192.168.1.0/30", "vm1", &poisoned).unwrap_err();
        let msg = err.to_string();
        assert!(msg.contains("poisoned"), "got: {msg}");
        assert!(!msg.contains("all 1 blocks allocated"), "got: {msg}");
    }

    #[test]
    fn allocated_block_reports_then_clears_on_release() {
        let (_dir, store) = test_store();
        allocate(&store, "10.100.0.0/16", "vm1", &no_exclude()).unwrap();
        let a2 = allocate(&store, "10.100.0.0/16", "vm2", &no_exclude()).unwrap();

        assert_eq!(
            allocated_block(&store, "vm2").unwrap(),
            Some(a2.block_index)
        );
        assert_eq!(allocated_block(&store, "nonexistent").unwrap(), None);

        release(&store, "vm2").unwrap();
        assert_eq!(allocated_block(&store, "vm2").unwrap(), None);
    }

    #[test]
    fn list_allocations_reports_rows_with_resolved_ips() {
        let (_dir, store) = test_store();
        allocate(&store, "10.100.0.0/16", "vm1", &no_exclude()).unwrap();
        allocate(&store, "10.100.0.0/16", "vm2", &no_exclude()).unwrap();
        release(&store, "vm1").unwrap();

        let rows = list_allocations(&store).unwrap();
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].vm_name, "vm2");
        assert_eq!(rows[0].block_index, 1);
        assert_eq!(rows[0].guest_ip, "10.100.0.6");
        assert_eq!(rows[0].host_ip, "10.100.0.5");
        assert!(!rows[0].single_address);
    }

    #[test]
    fn list_allocations_empty_when_none() {
        let (_dir, store) = test_store();
        assert!(list_allocations(&store).unwrap().is_empty());
    }

    #[test]
    fn poisoned_slot_release_and_retry_does_not_disturb_healthy_vms() {
        // SEC-345 rollback-isolation shield.
        let (_dir, store) = test_store();
        let healthy1 = allocate(&store, "10.100.0.0/16", "healthy1", &no_exclude()).unwrap();
        let healthy2 = allocate(&store, "10.100.0.0/16", "healthy2", &no_exclude()).unwrap();

        let failed = allocate(&store, "10.100.0.0/16", "failing", &no_exclude()).unwrap();
        assert_eq!(failed.block_index, 2);
        release(&store, "failing").unwrap();
        let poisoned = HashSet::from([failed.block_index]);
        let retry = allocate(&store, "10.100.0.0/16", "failing", &poisoned).unwrap();
        assert_eq!(retry.block_index, 3);

        assert_eq!(
            allocated_block(&store, "healthy1").unwrap(),
            Some(healthy1.block_index)
        );
        assert_eq!(
            allocated_block(&store, "healthy2").unwrap(),
            Some(healthy2.block_index)
        );
    }

    #[test]
    fn allocate_rejects_subnet_mismatch() {
        let (_dir, store) = test_store();
        allocate(&store, "10.100.0.0/16", "vm1", &no_exclude()).unwrap();
        let err = allocate(&store, "10.200.0.0/16", "vm2", &no_exclude()).unwrap_err();
        assert!(err.to_string().contains("subnet mismatch"));
    }

    #[test]
    fn allocate_rejects_duplicate_vm_name() {
        let (_dir, store) = test_store();
        allocate(&store, "10.100.0.0/16", "vm1", &no_exclude()).unwrap();
        let err = allocate(&store, "10.100.0.0/16", "vm1", &no_exclude()).unwrap_err();
        assert!(err.to_string().contains("UNIQUE") || err.to_string().contains("sqlite"));
    }

    #[test]
    fn release_idempotent() {
        let (_dir, store) = test_store();
        release(&store, "nonexistent").unwrap();

        allocate(&store, "10.100.0.0/16", "vm1", &no_exclude()).unwrap();
        release(&store, "vm1").unwrap();
        release(&store, "vm1").unwrap();
    }

    #[test]
    fn release_only_removes_target_vm() {
        let (_dir, store) = test_store();
        allocate(&store, "10.100.0.0/16", "vm1", &no_exclude()).unwrap();
        allocate(&store, "10.100.0.0/16", "vm2", &no_exclude()).unwrap();
        allocate(&store, "10.100.0.0/16", "vm3", &no_exclude()).unwrap();

        release(&store, "vm2").unwrap();

        let conn = db::open(store.root()).unwrap();
        let rows: Vec<(u32, String)> = conn
            .prepare("SELECT block_index, vm_name FROM network_allocations ORDER BY block_index")
            .unwrap()
            .query_map([], |r| Ok((r.get(0)?, r.get(1)?)))
            .unwrap()
            .collect::<std::result::Result<_, _>>()
            .unwrap();
        assert_eq!(rows, vec![(0, "vm1".to_string()), (2, "vm3".to_string())]);
    }

    // --- allocate_single (macOS shared-subnet path) ---

    /// Helper: vmnet's host-global reservations carved out of the /24.
    fn vmnet_reserved() -> [Ipv4Addr; 3] {
        [
            Ipv4Addr::new(192, 168, 64, 0),
            Ipv4Addr::new(192, 168, 64, 1),
            Ipv4Addr::new(192, 168, 64, 255),
        ]
    }

    #[test]
    fn allocate_single_skips_network_and_gateway_in_slot_zero() {
        let (_dir, store) = test_store();
        let reserved = vmnet_reserved();
        let alloc = allocate_single(
            &store,
            "192.168.64.0/27",
            "vm1",
            "192.168.64.1",
            "255.255.255.0",
            &reserved,
            &no_exclude(),
        )
        .unwrap();
        assert_eq!(alloc.guest_ip, "192.168.64.2");
        assert_eq!(alloc.host_ip, "192.168.64.1");
        assert_eq!(alloc.netmask, "255.255.255.0");
        assert_eq!(alloc.block_index, 2);
    }

    #[test]
    fn allocate_single_packs_addresses_one_per_vm() {
        let (_dir, store) = test_store();
        let reserved = vmnet_reserved();
        let mut last_octet = None;
        for i in 0..30 {
            let alloc = allocate_single(
                &store,
                "192.168.64.0/27",
                &format!("vm{i}"),
                "192.168.64.1",
                "255.255.255.0",
                &reserved,
                &no_exclude(),
            )
            .unwrap();
            let octet: u8 = alloc.guest_ip.split('.').nth(3).unwrap().parse().unwrap();
            assert!(octet >= 2);
            if let Some(prev) = last_octet {
                assert!(octet > prev, "expected strictly monotonic guest IPs");
            }
            last_octet = Some(octet);
        }
    }

    #[test]
    fn allocate_single_skips_broadcast_in_top_slot() {
        let (_dir, store) = test_store();
        let reserved = vmnet_reserved();
        for i in 0..31 {
            allocate_single(
                &store,
                "192.168.64.224/27",
                &format!("vm{i}"),
                "192.168.64.1",
                "255.255.255.0",
                &reserved,
                &no_exclude(),
            )
            .unwrap();
        }
        let err = allocate_single(
            &store,
            "192.168.64.224/27",
            "overflow",
            "192.168.64.1",
            "255.255.255.0",
            &reserved,
            &no_exclude(),
        )
        .unwrap_err();
        assert!(matches!(err, Error::Network(_)));
    }

    #[test]
    fn allocate_single_reuses_released_addresses() {
        let (_dir, store) = test_store();
        let reserved = vmnet_reserved();
        let a1 = allocate_single(
            &store,
            "192.168.64.32/27",
            "vm1",
            "192.168.64.1",
            "255.255.255.0",
            &reserved,
            &no_exclude(),
        )
        .unwrap();
        let a2 = allocate_single(
            &store,
            "192.168.64.32/27",
            "vm2",
            "192.168.64.1",
            "255.255.255.0",
            &reserved,
            &no_exclude(),
        )
        .unwrap();
        let _a3 = allocate_single(
            &store,
            "192.168.64.32/27",
            "vm3",
            "192.168.64.1",
            "255.255.255.0",
            &reserved,
            &no_exclude(),
        )
        .unwrap();
        assert_ne!(a1.guest_ip, a2.guest_ip);

        release(&store, "vm2").unwrap();

        let a4 = allocate_single(
            &store,
            "192.168.64.32/27",
            "vm4",
            "192.168.64.1",
            "255.255.255.0",
            &reserved,
            &no_exclude(),
        )
        .unwrap();
        assert_eq!(a4.guest_ip, a2.guest_ip);
    }

    #[test]
    fn allocate_single_rejects_subnet_mismatch_on_reread() {
        let (_dir, store) = test_store();
        let reserved = vmnet_reserved();
        allocate_single(
            &store,
            "192.168.64.32/27",
            "vm1",
            "192.168.64.1",
            "255.255.255.0",
            &reserved,
            &no_exclude(),
        )
        .unwrap();
        let err = allocate_single(
            &store,
            "192.168.64.64/27",
            "vm2",
            "192.168.64.1",
            "255.255.255.0",
            &reserved,
            &no_exclude(),
        )
        .unwrap_err();
        assert!(matches!(err, Error::Network(msg) if msg.contains("subnet mismatch")));
    }

    #[test]
    fn allocate_single_skips_poisoned_slot() {
        let (_dir, store) = test_store();
        let reserved = vmnet_reserved();
        let poisoned = HashSet::from([2u32]);
        let alloc = allocate_single(
            &store,
            "192.168.64.0/27",
            "vm1",
            "192.168.64.1",
            "255.255.255.0",
            &reserved,
            &poisoned,
        )
        .unwrap();
        assert_eq!(alloc.guest_ip, "192.168.64.3");
        assert_eq!(alloc.block_index, 3);
    }

    #[test]
    fn list_allocations_resolves_single_address_rows() {
        let (_dir, store) = test_store();
        let reserved = vmnet_reserved();
        allocate_single(
            &store,
            "192.168.64.0/27",
            "vm1",
            "192.168.64.1",
            "255.255.255.0",
            &reserved,
            &no_exclude(),
        )
        .unwrap();
        let rows = list_allocations(&store).unwrap();
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].vm_name, "vm1");
        assert_eq!(rows[0].guest_ip, "192.168.64.2");
        assert!(rows[0].single_address);
    }

    #[test]
    fn strategy_mismatch_rejected_on_reread() {
        let (_dir, store) = test_store();
        allocate(&store, "10.100.0.0/16", "vm1", &no_exclude()).unwrap();
        let reserved: [Ipv4Addr; 0] = [];
        let err = allocate_single(
            &store,
            "10.100.0.0/16",
            "vm2",
            "10.100.0.1",
            "255.255.0.0",
            &reserved,
            &no_exclude(),
        )
        .unwrap_err();
        assert!(
            err.to_string().contains("strategy mismatch"),
            "got: {err}"
        );
    }

    // --- Concurrency stress test (SEC-459 regression) ---

    #[test]
    fn parallel_allocate_produces_unique_slots() {
        use std::sync::{Arc, Barrier};
        use std::thread;

        const N: usize = 50;

        let dir = tempfile::tempdir().unwrap();
        let store = StateStore::new(dir.path().to_path_buf());
        store.init().unwrap();
        let store = Arc::new(store);
        let barrier = Arc::new(Barrier::new(N));

        let mut handles = Vec::with_capacity(N);
        for i in 0..N {
            let store = Arc::clone(&store);
            let barrier = Arc::clone(&barrier);
            handles.push(thread::spawn(move || {
                barrier.wait();
                allocate(&store, "10.100.0.0/16", &format!("vm{i}"), &HashSet::new()).unwrap()
            }));
        }

        let results: Vec<IpAllocation> = handles.into_iter().map(|h| h.join().unwrap()).collect();

        let unique_slots: HashSet<u32> = results.iter().map(|a| a.block_index).collect();
        assert_eq!(unique_slots.len(), N);

        let conn = db::open(store.root()).unwrap();
        let count: i64 = conn
            .query_row("SELECT COUNT(*) FROM network_allocations", [], |r| r.get(0))
            .unwrap();
        assert_eq!(count, N as i64);
    }

    // --- Invariant checker ---

    #[test]
    fn check_invariants_clean_store() {
        let (_dir, store) = test_store();
        allocate(&store, "10.100.0.0/16", "vm1", &no_exclude()).unwrap();
        allocate(&store, "10.100.0.0/16", "vm2", &no_exclude()).unwrap();
        let anomalies = check_invariants(&store).unwrap();
        assert!(anomalies.is_empty());
    }

    fn drop_alloc_constraints(store: &StateStore) {
        let conn = db::open(store.root()).unwrap();
        conn.execute_batch(
            "
            BEGIN;
            CREATE TABLE network_allocations_tmp (
                block_index    INTEGER NOT NULL,
                subnet         TEXT    NOT NULL,
                vm_name        TEXT    NOT NULL,
                single_address INTEGER NOT NULL DEFAULT 0
            ) STRICT;
            INSERT INTO network_allocations_tmp
                SELECT * FROM network_allocations;
            DROP TABLE network_allocations;
            ALTER TABLE network_allocations_tmp RENAME TO network_allocations;
            COMMIT;
            ",
        )
        .unwrap();
    }

    fn force_duplicate_slot(store: &StateStore, subnet: &str, block_index: u32, vm_name: &str) {
        drop_alloc_constraints(store);
        let conn = db::open(store.root()).unwrap();
        conn.execute(
            "INSERT INTO network_allocations (block_index, subnet, vm_name, single_address)
             VALUES (?1, ?2, ?3, 0)",
            params![block_index, subnet, vm_name],
        )
        .unwrap();
    }

    #[test]
    fn check_invariants_duplicate_slot_attributes_all_vms() {
        let (_dir, store) = test_store();
        let alloc1 = allocate(&store, "10.100.0.0/16", "vm1", &no_exclude()).unwrap();
        force_duplicate_slot(&store, "10.100.0.0/16", alloc1.block_index, "vm2");

        let anomalies = check_invariants(&store).unwrap();
        let dup_slot: Vec<&Anomaly> = anomalies
            .iter()
            .filter(|a| matches!(a.kind, AnomalyKind::DuplicateSlot { .. }))
            .collect();
        assert_eq!(dup_slot.len(), 1);
        let a = dup_slot[0];
        assert!(a.vm_names.contains(&"vm1".to_string()));
        assert!(a.vm_names.contains(&"vm2".to_string()));
        assert_eq!(a.vm_names.len(), 2);
        match &a.kind {
            AnomalyKind::DuplicateSlot {
                subnet,
                block_index,
                count,
            } => {
                assert_eq!(subnet, "10.100.0.0/16");
                assert_eq!(*block_index, alloc1.block_index);
                assert_eq!(*count, 2);
            }
            other => panic!("expected DuplicateSlot, got {other:?}"),
        }
    }

    #[test]
    fn check_invariants_duplicate_vm_name_lists_one_vm() {
        let (_dir, store) = test_store();
        allocate(&store, "10.100.0.0/16", "vm1", &no_exclude()).unwrap();
        drop_alloc_constraints(&store);
        let conn = db::open(store.root()).unwrap();
        conn.execute(
            "INSERT INTO network_allocations (block_index, subnet, vm_name, single_address)
             VALUES (1, '10.100.0.0/16', 'vm1', 0)",
            [],
        )
        .unwrap();

        let anomalies = check_invariants(&store).unwrap();
        let dup_name: Vec<&Anomaly> = anomalies
            .iter()
            .filter(|a| matches!(a.kind, AnomalyKind::DuplicateVmName { .. }))
            .collect();
        assert_eq!(dup_name.len(), 1);
        let a = dup_name[0];
        assert_eq!(a.vm_names, vec!["vm1".to_string()]);
        match &a.kind {
            AnomalyKind::DuplicateVmName { vm_name, count } => {
                assert_eq!(vm_name, "vm1");
                assert_eq!(*count, 2);
            }
            other => panic!("expected DuplicateVmName, got {other:?}"),
        }
    }

    #[test]
    fn check_invariants_propagates_db_open_failure() {
        let dir = tempfile::tempdir().unwrap();
        let blocking_file = dir.path().join("state");
        std::fs::write(&blocking_file, b"not a directory").unwrap();
        let store = StateStore::new(blocking_file);
        let result = check_invariants(&store);
        assert!(result.is_err());
    }
}

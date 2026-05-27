//! Network allocation inspection and cleanup (SEC-419).
//!
//! `ember network status` shows which /30 slots are allocated and to which
//! VM; `ember network reset` releases allocations for VMs that no longer
//! exist (orphaned slots), reconciling the allocator with on-disk VM state.
//!
//! These operate on ember's *own* allocation records. A slot the macOS vmnet
//! framework is holding stale state for (a truly "poisoned" slot) is a
//! kernel-level condition that a host reboot clears; the `vm start` retry path
//! routes around it in the meantime by re-allocating on the next free slot.

use std::collections::HashSet;
use std::path::Path;

use clap::{Args, Subcommand};

use ember_core::network::ip;
use ember_core::state::store::StateStore;

use super::vm::OutputFormat;

#[derive(Subcommand)]
pub enum NetworkCommand {
    /// Show /30 slot allocations (which slots are in use, by which VM)
    Status(StatusArgs),

    /// Release allocations for VMs that no longer exist (orphaned slots)
    Reset(ResetArgs),
}

#[derive(Args)]
pub struct StatusArgs {
    /// Output format
    #[arg(long, default_value = "table")]
    pub format: OutputFormat,
}

#[derive(Args)]
pub struct ResetArgs {
    /// Show what would be released without changing anything
    #[arg(long)]
    pub dry_run: bool,
}

pub fn run(cmd: &NetworkCommand, state_dir: &Path) -> anyhow::Result<()> {
    match cmd {
        NetworkCommand::Status(args) => status(args, state_dir),
        NetworkCommand::Reset(args) => reset(args, state_dir),
    }
}

fn status(args: &StatusArgs, state_dir: &Path) -> anyhow::Result<()> {
    let store = StateStore::new(state_dir.to_path_buf());
    let rows = ip::list_allocations(&store)?;

    match args.format {
        OutputFormat::Json => {
            let items: Vec<String> = rows
                .iter()
                .map(|r| {
                    format!(
                        r#"{{"block_index":{},"subnet":"{}","vm_name":"{}","guest_ip":"{}","host_ip":"{}"}}"#,
                        r.block_index, r.subnet, r.vm_name, r.guest_ip, r.host_ip
                    )
                })
                .collect();
            println!("[{}]", items.join(","));
        }
        OutputFormat::Table => {
            if rows.is_empty() {
                println!("No network allocations.");
                return Ok(());
            }
            println!("{:<6} {:<18} {:<16} VM", "SLOT", "SUBNET", "GUEST IP");
            for r in &rows {
                println!(
                    "{:<6} {:<18} {:<16} {}",
                    r.block_index, r.subnet, r.guest_ip, r.vm_name
                );
            }
        }
    }
    Ok(())
}

fn reset(args: &ResetArgs, state_dir: &Path) -> anyhow::Result<()> {
    let store = StateStore::new(state_dir.to_path_buf());
    let rows = ip::list_allocations(&store)?;
    let existing = existing_vm_names(state_dir);

    let orphans: Vec<&ip::AllocationRow> = rows
        .iter()
        .filter(|r| !existing.contains(&r.vm_name))
        .collect();

    if orphans.is_empty() {
        println!("No orphaned allocations — every slot maps to an existing VM.");
        return Ok(());
    }

    for r in &orphans {
        if args.dry_run {
            println!(
                "would release slot {} ({}) held by missing VM '{}'",
                r.block_index, r.guest_ip, r.vm_name
            );
        } else {
            ip::release(&store, &r.vm_name)?;
            println!(
                "released slot {} ({}) held by missing VM '{}'",
                r.block_index, r.guest_ip, r.vm_name
            );
        }
    }

    if args.dry_run {
        println!(
            "\n{} orphaned slot(s); re-run without --dry-run to release.",
            orphans.len()
        );
    } else {
        println!("\nReleased {} orphaned slot(s).", orphans.len());
    }
    Ok(())
}

/// Names of VMs that currently exist on disk (one directory per VM under
/// `state_dir/vms/`). An allocation whose VM name is absent here is orphaned.
fn existing_vm_names(state_dir: &Path) -> HashSet<String> {
    let mut names = HashSet::new();
    let vms_dir = state_dir.join("vms");
    if let Ok(entries) = std::fs::read_dir(&vms_dir) {
        for entry in entries.flatten() {
            if entry.path().is_dir() {
                if let Some(name) = entry.file_name().to_str() {
                    names.insert(name.to_string());
                }
            }
        }
    }
    names
}

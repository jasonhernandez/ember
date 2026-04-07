//! Pool management commands — create, assign, complete, release, status, destroy.
//!
//! Pools are named groups of VMs created from the same image. Thermite uses
//! pools to manage worker VM lifecycles: create a pool per wave, assign tasks
//! to available VMs, mark them complete/failed, release for reuse.

use std::path::Path;

use clap::{Args, Subcommand};

use crate::cli::vm::OutputFormat;
use crate::state::pool::{self, PoolState, PoolVm, PoolVmStatus};
use crate::state::store::StateStore;
use crate::state::vm;

#[derive(Subcommand)]
pub enum PoolCommand {
    /// Create a pool of VMs from an image
    Create(CreateArgs),

    /// Assign a task to the next available VM in a pool
    Assign(AssignArgs),

    /// Mark a VM's task as complete
    Complete(CompleteArgs),

    /// Release a VM back to available state
    Release(ReleaseArgs),

    /// Show pool status
    Status(StatusArgs),

    /// Destroy a pool and all its VMs
    Destroy(DestroyArgs),
}

#[derive(Args)]
pub struct CreateArgs {
    /// Pool name
    pub name: String,

    /// Number of VMs to create
    #[arg(long)]
    pub count: u32,

    /// Base image reference
    #[arg(long)]
    pub image: String,

    /// Number of vCPUs per VM (default: 1)
    #[arg(long)]
    pub cpus: Option<u32>,

    /// Memory size per VM, e.g. 512M, 16G (default: 16G)
    #[arg(long)]
    pub memory: Option<crate::config::size::ByteSize>,

    /// Disk size per VM, e.g. 8G (default: 8G)
    #[arg(long)]
    pub disk_size: Option<crate::config::size::ByteSize>,

    /// Enable vsock device on all pool VMs
    #[arg(long)]
    pub vsock: bool,

    /// Output format
    #[arg(long, default_value = "table")]
    pub format: OutputFormat,
}

#[derive(Args)]
pub struct AssignArgs {
    /// Pool name
    pub pool: String,

    /// Task ID to assign
    #[arg(long)]
    pub task: String,

    /// Output format
    #[arg(long, default_value = "table")]
    pub format: OutputFormat,
}

#[derive(Args)]
pub struct CompleteArgs {
    /// Pool name
    pub pool: String,

    /// VM name
    pub vm_name: String,

    /// Mark the task as failed
    #[arg(long)]
    pub failed: bool,
}

#[derive(Args)]
pub struct ReleaseArgs {
    /// Pool name
    pub pool: String,

    /// VM name
    pub vm_name: String,
}

#[derive(Args)]
pub struct StatusArgs {
    /// Pool name
    pub pool: String,

    /// Output format
    #[arg(long, default_value = "table")]
    pub format: OutputFormat,
}

#[derive(Args)]
pub struct DestroyArgs {
    /// Pool name
    pub pool: String,
}

pub fn run(cmd: &PoolCommand, state_dir: &Path) -> anyhow::Result<()> {
    match cmd {
        PoolCommand::Create(args) => create(args, state_dir),
        PoolCommand::Assign(args) => assign(args, state_dir),
        PoolCommand::Complete(args) => complete(args, state_dir),
        PoolCommand::Release(args) => release(args, state_dir),
        PoolCommand::Status(args) => status(args, state_dir),
        PoolCommand::Destroy(args) => destroy(args, state_dir),
    }
}

// ---------------------------------------------------------------------------
// Command implementations
// ---------------------------------------------------------------------------

/// Create a pool of N VMs from an image.
///
/// Creates each VM via `ember vm create` internally, then records the pool
/// state. All VMs are started and SSH-ready before the command returns.
fn create(args: &CreateArgs, state_dir: &Path) -> anyhow::Result<()> {
    let store = StateStore::new(state_dir.to_path_buf());

    if pool::exists(&store, &args.name) {
        anyhow::bail!("pool '{}' already exists", args.name);
    }

    let vm_names = pool::vm_names(&args.name, args.count);

    println!(
        "Creating pool '{}' with {} VMs from '{}'...",
        args.name, args.count, args.image
    );

    // Create each VM using the existing vm create infrastructure.
    for vm_name in &vm_names {
        let create_args = super::vm::CreateArgs {
            name: vm_name.clone(),
            image: Some(args.image.clone()),
            cpus: args.cpus,
            memory: args.memory,
            disk_size: args.disk_size,
            kernel: None,
            network: None,
            vm_config: None,
            vsock: args.vsock,
            no_start: false,
        };
        super::vm::run(&super::vm::VmCommand::Create(create_args), state_dir)?;
    }

    // Save pool state.
    let pool_state = PoolState {
        name: args.name.clone(),
        image: args.image.clone(),
        vms: vm_names
            .iter()
            .map(|name| PoolVm {
                vm_name: name.clone(),
                status: PoolVmStatus::Available,
                task_id: None,
                assigned_at: None,
                completed_at: None,
            })
            .collect(),
        created_at: vm::now_iso8601(),
    };
    pool::save(&store, &pool_state)?;

    match args.format {
        OutputFormat::Json => {
            println!("{}", serde_json::to_string_pretty(&pool_state)?);
        }
        OutputFormat::Table => {
            println!("Pool '{}' created with {} VMs.", args.name, args.count);
        }
    }

    Ok(())
}

/// Assign a task to the next available VM in a pool.
fn assign(args: &AssignArgs, state_dir: &Path) -> anyhow::Result<()> {
    let store = StateStore::new(state_dir.to_path_buf());

    let assigned = pool::assign(&store, &args.pool, &args.task)?;

    #[derive(serde::Serialize)]
    struct AssignResult {
        vm_name: String,
        task_id: String,
    }

    let result = AssignResult {
        vm_name: assigned.vm_name.clone(),
        task_id: args.task.clone(),
    };

    match args.format {
        OutputFormat::Json => {
            println!("{}", serde_json::to_string_pretty(&result)?);
        }
        OutputFormat::Table => {
            println!(
                "Assigned task '{}' to VM '{}'.",
                args.task, assigned.vm_name
            );
        }
    }

    Ok(())
}

/// Mark a VM's task as complete (success or failure).
fn complete(args: &CompleteArgs, state_dir: &Path) -> anyhow::Result<()> {
    let store = StateStore::new(state_dir.to_path_buf());

    pool::complete(&store, &args.pool, &args.vm_name, args.failed)?;

    let status = if args.failed { "failed" } else { "completed" };
    println!(
        "VM '{}' in pool '{}' marked as {}.",
        args.vm_name, args.pool, status
    );

    Ok(())
}

/// Release a VM back to available state.
fn release(args: &ReleaseArgs, state_dir: &Path) -> anyhow::Result<()> {
    let store = StateStore::new(state_dir.to_path_buf());

    pool::release(&store, &args.pool, &args.vm_name)?;

    println!(
        "VM '{}' in pool '{}' released (available).",
        args.vm_name, args.pool
    );

    Ok(())
}

/// Show pool status.
fn status(args: &StatusArgs, state_dir: &Path) -> anyhow::Result<()> {
    let store = StateStore::new(state_dir.to_path_buf());

    let pool_state = pool::load(&store, &args.pool)?;

    match args.format {
        OutputFormat::Json => {
            // Output the VMs array with status info, matching Thermite's expected format.
            #[derive(serde::Serialize)]
            struct VmStatus {
                vm: String,
                task: Option<String>,
                status: String,
            }

            let vms: Vec<VmStatus> = pool_state
                .vms
                .iter()
                .map(|v| VmStatus {
                    vm: v.vm_name.clone(),
                    task: v.task_id.clone(),
                    status: v.status.to_string(),
                })
                .collect();

            println!("{}", serde_json::to_string_pretty(&vms)?);
        }
        OutputFormat::Table => {
            println!("Pool: {}  (image: {})", pool_state.name, pool_state.image);
            println!("{:<30} {:<12} {:<20}", "VM", "STATUS", "TASK");
            for vm in &pool_state.vms {
                println!(
                    "{:<30} {:<12} {:<20}",
                    vm.vm_name,
                    vm.status,
                    vm.task_id.as_deref().unwrap_or("-"),
                );
            }
        }
    }

    Ok(())
}

/// Destroy a pool and all its VMs.
///
/// Stops and deletes each VM in the pool, then removes the pool state.
fn destroy(args: &DestroyArgs, state_dir: &Path) -> anyhow::Result<()> {
    let store = StateStore::new(state_dir.to_path_buf());

    let pool_state = pool::load(&store, &args.pool)?;

    println!("Destroying pool '{}'...", args.pool);

    // Delete each VM (stop if running, then delete storage + state).
    for pool_vm in &pool_state.vms {
        if vm::exists(&store, &pool_vm.vm_name) {
            let delete_args = super::vm::DeleteArgs {
                name: Some(pool_vm.vm_name.clone()),
                all: false,
                force: true,
            };
            // Ignore errors from individual VM deletions — best effort cleanup.
            if let Err(e) = super::vm::run(&super::vm::VmCommand::Delete(delete_args), state_dir) {
                eprintln!("warning: failed to delete VM '{}': {}", pool_vm.vm_name, e);
            }
        }
    }

    // Remove pool state.
    pool::delete(&store, &args.pool)?;

    println!("Pool '{}' destroyed.", args.pool);

    Ok(())
}

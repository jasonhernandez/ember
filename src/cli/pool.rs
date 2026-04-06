//! Pool commands: batch VM creation and group management.
//!
//! A pool is a named group of identical VMs created from one image.
//! Thermite (or any consumer) handles task assignment above this layer;
//! Ember only manages the VM lifecycle and grouping.

use std::path::Path;

use clap::{Args, Subcommand};

use super::init::GlobalConfig;
use crate::backend::{Network, NetworkBackend, Storage, StorageBackend, Vm, VmBackend};
use crate::cleanup::Rollback;
use crate::config::size::ByteSize;
use crate::image;
use crate::image::registry::ImageRegistry;
use crate::state::pool::{self, PoolMetadata, PoolVmStatus};
use crate::state::store::StateStore;
use crate::state::vm::{self, SshConfig, VmMetadata, VmStatus};
use uuid::Uuid;

#[derive(Subcommand)]
pub enum PoolCommand {
    /// Create a pool of identical VMs from one image
    Create(CreateArgs),

    /// List all pools
    List(ListArgs),

    /// Show status of VMs in a pool
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
    pub memory: Option<ByteSize>,

    /// Disk size per VM, e.g. 8G (default: 8G)
    #[arg(long)]
    pub disk_size: Option<ByteSize>,

    /// Kernel preset or file path [presets: stock]
    #[arg(long)]
    pub kernel: Option<crate::kernel::KernelSpec>,

    /// Network subnet
    #[arg(long)]
    pub network: Option<String>,

    /// Don't start VMs after creation
    #[arg(long)]
    pub no_start: bool,

    /// Output format
    #[arg(long, default_value = "table")]
    pub format: OutputFormat,
}

#[derive(Args)]
pub struct ListArgs {
    /// Output format
    #[arg(long, default_value = "table")]
    pub format: OutputFormat,
}

#[derive(Args)]
pub struct StatusArgs {
    /// Pool name
    pub name: String,

    /// Output format
    #[arg(long, default_value = "table")]
    pub format: OutputFormat,
}

#[derive(Args)]
pub struct DestroyArgs {
    /// Pool name
    pub name: String,
}

#[derive(Clone, clap::ValueEnum)]
pub enum OutputFormat {
    Table,
    Json,
}

pub fn run(cmd: &PoolCommand, state_dir: &Path) -> anyhow::Result<()> {
    match cmd {
        PoolCommand::Create(args) => create(args, state_dir),
        PoolCommand::List(args) => list(args, state_dir),
        PoolCommand::Status(args) => status(args, state_dir),
        PoolCommand::Destroy(args) => destroy(args, state_dir),
    }
}

// ---------------------------------------------------------------------------
// Default VM configuration (same as vm create defaults)
// ---------------------------------------------------------------------------

const DEFAULT_CPUS: u32 = 1;
const DEFAULT_MEMORY_GIB: u32 = 16;
const DEFAULT_DISK_SIZE_GIB: u32 = 8;

// ---------------------------------------------------------------------------
// pool create
// ---------------------------------------------------------------------------

/// Create a pool of N identical VMs from one image.
///
/// Each VM is named `<pool>-1` through `<pool>-N`. All VMs share the same
/// image, CPU, memory, and disk configuration. VMs are started by default.
fn create(args: &CreateArgs, state_dir: &Path) -> anyhow::Result<()> {
    let json_mode = matches!(args.format, OutputFormat::Json);

    if args.count == 0 {
        anyhow::bail!("--count must be at least 1");
    }

    let store = StateStore::new(state_dir.to_path_buf());
    let mut global_config: GlobalConfig = store.read(&store.config_path())?;

    // Check pool doesn't already exist.
    if pool::exists(&store, &args.name) {
        anyhow::bail!("pool '{}' already exists", args.name);
    }

    // Look up image in local registry.
    let registry = ImageRegistry::load(&store)?;
    let image_entry = registry
        .find_by_reference(&args.image)?
        .ok_or_else(|| {
            anyhow::anyhow!(
                "image '{}' not found locally — pull it first with: ember image pull {}",
                args.image,
                args.image
            )
        })?;

    let image_name = image_entry.local_name.clone();
    let image_ref = image_entry.reference.clone();
    let image_size_mib = image_entry.size_mib;

    // Resolve per-VM configuration.
    let cpus = args.cpus.unwrap_or(DEFAULT_CPUS);
    let memory_mib = match &args.memory {
        Some(m) => m
            .to_mib()
            .map_err(|e| anyhow::anyhow!("invalid memory size: {e}"))?,
        None => DEFAULT_MEMORY_GIB * 1024,
    };
    let disk_size_gib = match &args.disk_size {
        Some(d) => d
            .to_gib()
            .map_err(|e| anyhow::anyhow!("invalid disk size: {e}"))?,
        None => DEFAULT_DISK_SIZE_GIB,
    };

    let storage = Storage::new(&global_config);
    let vm_names = pool::vm_names(&args.name, args.count);

    // Check no VM names conflict with existing VMs.
    for name in &vm_names {
        if vm::exists(&store, name) {
            anyhow::bail!("vm '{name}' already exists — cannot create pool '{}'", args.name);
        }
    }

    // Resolve kernel once for all VMs.
    let kernel_path = super::vm::ensure_kernel(&args.kernel, &mut global_config, &store)?;

    // Find SSH key once.
    let pubkey_path = image::inject::default_ssh_pubkey_path().ok_or_else(|| {
        anyhow::anyhow!(
            "no SSH public key found at ~/.ssh/id_ed25519.pub or ~/.ssh/id_rsa.pub\n\
             Hint: create one with: ssh-keygen -t ed25519"
        )
    })?;

    let requested_size_mib = disk_size_gib as u64 * 1024;
    let needs_resize = requested_size_mib > image_size_mib;

    // ── Create all VMs ──────────────────────────────────────────────

    // Track created VMs for rollback on failure.
    let mut created_vms: Vec<String> = Vec::new();
    let log = |msg: &str| {
        if !json_mode {
            println!("{msg}");
        } else {
            eprintln!("{msg}");
        }
    };

    log(&format!(
        "Creating pool '{}' with {} VMs from image '{}'...",
        args.name, args.count, args.image
    ));

    for vm_name in &vm_names {
        log(&format!("  Creating VM '{vm_name}'..."));

        let mut rollback = Rollback::new();

        // Clone storage.
        let vm_disk_path = storage.clone_for_vm(&image_name, vm_name)?;
        let vm_disk = vm_disk_path.to_string_lossy().to_string();
        {
            let storage = storage.clone();
            let sd = state_dir.to_path_buf();
            let name = vm_name.clone();
            rollback.push("VM storage clone", move || {
                let _ = storage.destroy_vm_storage(&name);
                let _ = vm::delete(&StateStore::new(sd), &name);
            });
        }

        // Grow disk if needed.
        if needs_resize {
            storage.resize(vm_name, ByteSize::from_gib(disk_size_gib as u64))?;
        }

        // Inject SSH key.
        let dev_path = storage.disk_device_path(vm_name);
        let detected_ssh_user = storage.inject_ssh_key(&dev_path, &pubkey_path)?;

        let ssh_key = image::inject::default_ssh_privkey_path()
            .unwrap_or_else(|| std::path::PathBuf::from("/root/.ssh/id_ed25519"));

        // Build and save VM metadata.
        let metadata = VmMetadata {
            name: vm_name.clone(),
            id: Uuid::new_v4(),
            status: VmStatus::Created,
            image: image_ref.clone(),
            cpus,
            memory_mib,
            disk_size_gib,
            kernel_path: kernel_path.clone(),
            disk_path: vm_disk,
            boot_args: None,
            subnet: args.network.clone(),
            network: None,
            pid: None,
            api_socket: store.vm_dir(vm_name).join("firecracker.sock"),
            created_at: vm::now_iso8601(),
            ssh: SshConfig {
                user: detected_ssh_user,
                key: ssh_key,
            },
            forked_from: None,
        };

        vm::save(&store, &metadata)?;
        rollback.commit();
        created_vms.push(vm_name.clone());
    }

    // ── Start VMs ────────────────────────────────────────────────────

    if !args.no_start {
        for vm_name in &vm_names {
            log(&format!("  Starting VM '{vm_name}'..."));
            start_vm(&store, &global_config, vm_name, state_dir)?;
        }
    }

    // ── Save pool metadata ───────────────────────────────────────────

    let pool_meta = PoolMetadata {
        name: args.name.clone(),
        image: image_ref,
        count: args.count,
        vms: vm_names.clone(),
        created_at: vm::now_iso8601(),
    };
    pool::save(&store, &pool_meta)?;

    // ── Output ───────────────────────────────────────────────────────

    match args.format {
        OutputFormat::Json => {
            let output = serde_json::json!({
                "name": args.name,
                "count": args.count,
                "image": args.image,
                "vms": vm_names,
            });
            println!("{}", serde_json::to_string_pretty(&output)?);
        }
        OutputFormat::Table => {
            println!(
                "Pool '{}' created with {} VMs.",
                args.name, args.count
            );
        }
    }

    Ok(())
}

/// Start a single VM (networking + hypervisor). Extracted from vm::start
/// so pool create can start VMs without going through CLI dispatch.
fn start_vm(
    store: &StateStore,
    config: &GlobalConfig,
    vm_name: &str,
    state_dir: &Path,
) -> anyhow::Result<()> {
    let mut metadata = vm::load(store, vm_name)?;
    match metadata.status {
        VmStatus::Created | VmStatus::Stopped => {}
        _ => {
            anyhow::bail!(
                "vm '{vm_name}' is {} — expected created or stopped",
                metadata.status
            );
        }
    }

    let mut rollback = Rollback::new();

    // Set up networking.
    let net_backend = Network::new(store.clone());
    let net_info = net_backend.setup(&metadata, config)?;
    {
        let net = Network::new(StateStore::new(state_dir.to_path_buf()));
        let meta_name = metadata.name.clone();
        let net_info_clone = net_info.clone();
        rollback.push("network", move || {
            let teardown_meta = VmMetadata {
                name: meta_name,
                network: Some(net_info_clone),
                ..VmMetadata::default_for_teardown()
            };
            let _ = net.teardown(&teardown_meta);
        });
    }

    metadata.network = Some(net_info);

    // Start hypervisor.
    let started = Vm::start(&metadata, config)?;
    let pid = started.pid;
    {
        let meta = metadata.clone();
        rollback.push("VM process", move || {
            let _ = Vm::force_stop(&meta);
        });
    }

    // Merge network info from backend (includes MAC assigned by hypervisor).
    metadata.network = Some(started.network.clone());

    // Persist running state.
    metadata.status = VmStatus::Running;
    metadata.pid = Some(pid);
    vm::save(store, &metadata)?;

    rollback.commit();
    Ok(())
}

// ---------------------------------------------------------------------------
// pool list
// ---------------------------------------------------------------------------

fn list(args: &ListArgs, state_dir: &Path) -> anyhow::Result<()> {
    let store = StateStore::new(state_dir.to_path_buf());
    let pools = pool::list(&store)?;

    match args.format {
        OutputFormat::Json => {
            println!("{}", serde_json::to_string_pretty(&pools)?);
        }
        OutputFormat::Table => {
            if pools.is_empty() {
                println!("No pools found. Create one with: ember pool create <name> --count N --image <image>");
                return Ok(());
            }

            println!("{:<20} {:>5} {:<40} {:<20}", "NAME", "VMS", "IMAGE", "CREATED");
            for p in &pools {
                println!(
                    "{:<20} {:>5} {:<40} {:<20}",
                    p.name, p.count, p.image, p.created_at
                );
            }
        }
    }

    Ok(())
}

// ---------------------------------------------------------------------------
// pool status
// ---------------------------------------------------------------------------

fn status(args: &StatusArgs, state_dir: &Path) -> anyhow::Result<()> {
    let store = StateStore::new(state_dir.to_path_buf());
    let pool_meta = pool::load(&store, &args.name)?;

    let mut statuses: Vec<PoolVmStatus> = Vec::new();
    for vm_name in &pool_meta.vms {
        let vm_status = match vm::load(&store, vm_name) {
            Ok(metadata) => PoolVmStatus {
                vm_name: vm_name.clone(),
                status: metadata.status.to_string(),
                pid: metadata.pid,
                guest_ip: metadata.network.as_ref().map(|n| n.guest_ip.clone()),
            },
            Err(_) => PoolVmStatus {
                vm_name: vm_name.clone(),
                status: "missing".to_string(),
                pid: None,
                guest_ip: None,
            },
        };
        statuses.push(vm_status);
    }

    match args.format {
        OutputFormat::Json => {
            let output = serde_json::json!({
                "name": pool_meta.name,
                "image": pool_meta.image,
                "count": pool_meta.count,
                "vms": statuses,
            });
            println!("{}", serde_json::to_string_pretty(&output)?);
        }
        OutputFormat::Table => {
            println!("Pool: {} ({} VMs from {})", pool_meta.name, pool_meta.count, pool_meta.image);
            println!();
            println!("{:<20} {:<10} {:>8} {:<16}", "VM", "STATUS", "PID", "GUEST IP");
            for s in &statuses {
                println!(
                    "{:<20} {:<10} {:>8} {:<16}",
                    s.vm_name,
                    s.status,
                    s.pid.map_or("-".to_string(), |p| p.to_string()),
                    s.guest_ip.as_deref().unwrap_or("-"),
                );
            }
        }
    }

    Ok(())
}

// ---------------------------------------------------------------------------
// pool destroy
// ---------------------------------------------------------------------------

fn destroy(args: &DestroyArgs, state_dir: &Path) -> anyhow::Result<()> {
    let store = StateStore::new(state_dir.to_path_buf());
    let pool_meta = pool::load(&store, &args.name)?;

    println!("Destroying pool '{}' ({} VMs)...", args.name, pool_meta.count);

    for vm_name in &pool_meta.vms {
        match vm::load(&store, vm_name) {
            Ok(metadata) => {
                println!("  Deleting VM '{vm_name}'...");
                super::vm::force_delete_vm(&store, &metadata)?;
            }
            Err(_) => {
                eprintln!("  Warning: VM '{vm_name}' not found, skipping.");
            }
        }
    }

    // Remove pool state.
    pool::delete(&store, &args.name)?;

    println!("Pool '{}' destroyed.", args.name);
    Ok(())
}

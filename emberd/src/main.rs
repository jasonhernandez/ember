//! emberd — lightweight in-VM daemon for Ember VMs.
//!
//! Listens on vsock port 1024 (production, Linux) or a Unix domain socket
//! (testing) and serves JSON-lines requests. Matches the protocol expected
//! by Thermite's `EmberdClient` (`daemon_client.py`).
//!
//! Operations: ping, exec, read_file, write_file, agent_status, agent_reap,
//! vm_stats, workspace_reset.

use base64::{engine::general_purpose::STANDARD as B64, Engine};
use clap::Parser;
use serde_json::{json, Value};
use std::io::{BufRead, BufReader, Read, Write};
use std::os::unix::net::UnixListener;
use std::path::Path;
use std::sync::OnceLock;
use std::time::Instant;

#[cfg(target_os = "linux")]
use std::os::fd::{AsRawFd, FromRawFd};

/// Process start time, used as fallback when /proc/uptime is unavailable.
static START_TIME: OnceLock<Instant> = OnceLock::new();

/// Default vsock port for worker communication (matches Thermite VsockChannel.WORKER).
const DEFAULT_PORT: u32 = 1024;

#[derive(Parser)]
#[command(
    name = "emberd",
    version,
    about = "Lightweight in-VM daemon for Ember VMs"
)]
struct Args {
    /// vsock port to listen on (Linux only).
    #[arg(long, default_value_t = DEFAULT_PORT)]
    port: u32,

    /// Listen on a Unix domain socket instead of vsock (for testing).
    #[arg(long)]
    uds: Option<String>,
}

fn main() -> Result<(), Box<dyn std::error::Error>> {
    START_TIME.get_or_init(Instant::now);
    let args = Args::parse();
    eprintln!("emberd v{}", env!("CARGO_PKG_VERSION"));

    if let Some(ref path) = args.uds {
        let _ = std::fs::remove_file(path);
        let listener = UnixListener::bind(path)?;
        eprintln!("listening on UDS: {path}");
        accept_loop_uds(listener)
    } else {
        #[cfg(target_os = "linux")]
        {
            listen_vsock(args.port)
        }
        #[cfg(not(target_os = "linux"))]
        {
            eprintln!("vsock requires Linux. Use --uds for testing.");
            std::process::exit(1);
        }
    }
}

// ---------------------------------------------------------------------------
// Listeners
// ---------------------------------------------------------------------------

fn accept_loop_uds(listener: UnixListener) -> Result<(), Box<dyn std::error::Error>> {
    for stream in listener.incoming() {
        let stream = stream?;
        std::thread::spawn(move || {
            if let Err(e) = handle_connection(stream) {
                eprintln!("connection error: {e}");
            }
        });
    }
    Ok(())
}

#[cfg(target_os = "linux")]
fn listen_vsock(port: u32) -> Result<(), Box<dyn std::error::Error>> {
    use nix::sys::socket::{
        accept, bind, listen, socket, AddressFamily, Backlog, SockFlag, SockType, VsockAddr,
    };

    let fd = socket(
        AddressFamily::Vsock,
        SockType::Stream,
        SockFlag::empty(),
        None,
    )?;
    // VMADDR_CID_ANY = 0xFFFFFFFF — accept connections from any CID.
    let addr = VsockAddr::new(0xFFFFFFFF, port);
    bind(fd.as_raw_fd(), &addr)?;
    listen(&fd, Backlog::new(128)?)?;
    eprintln!("listening on vsock port {port}");

    loop {
        let client_fd = accept(fd.as_raw_fd())?;
        std::thread::spawn(move || {
            // Safety: client_fd is a valid open fd returned by accept().
            let file = unsafe { std::fs::File::from_raw_fd(client_fd) };
            if let Err(e) = handle_connection(file) {
                eprintln!("connection error: {e}");
            }
        });
    }
}

// ---------------------------------------------------------------------------
// Connection handler — generic over any Read + Write stream
// ---------------------------------------------------------------------------

fn handle_connection<S: Read + Write>(stream: S) -> Result<(), Box<dyn std::error::Error>> {
    let mut reader = BufReader::new(stream);
    let mut line = String::new();

    loop {
        line.clear();
        if reader.read_line(&mut line)? == 0 {
            break; // EOF
        }
        let trimmed = line.trim();
        if trimmed.is_empty() {
            continue;
        }

        let req: Value = match serde_json::from_str(trimmed) {
            Ok(v) => v,
            Err(e) => {
                let resp = json!({"error": format!("invalid JSON: {e}")});
                write_response(reader.get_mut(), &resp)?;
                continue;
            }
        };

        let resp = dispatch(&req);
        write_response(reader.get_mut(), &resp)?;
    }

    Ok(())
}

fn write_response<W: Write>(w: &mut W, resp: &Value) -> std::io::Result<()> {
    serde_json::to_writer(&mut *w, resp)?;
    w.write_all(b"\n")?;
    w.flush()
}

// ---------------------------------------------------------------------------
// Dispatch
// ---------------------------------------------------------------------------

fn dispatch(req: &Value) -> Value {
    let op = req.get("op").and_then(Value::as_str).unwrap_or("");
    match op {
        "ping" => op_ping(),
        "exec" => op_exec(req),
        "read_file" => op_read_file(req),
        "write_file" => op_write_file(req),
        "agent_status" => op_agent_status(),
        "agent_reap" => op_agent_reap(),
        "vm_stats" => op_vm_stats(),
        "workspace_reset" => op_workspace_reset(req),
        _ => json!({"error": format!("unknown op: {op}")}),
    }
}

// ---------------------------------------------------------------------------
// Operations
// ---------------------------------------------------------------------------

fn op_ping() -> Value {
    let uptime = read_proc_uptime()
        .unwrap_or_else(|| START_TIME.get().map_or(0.0, |t| t.elapsed().as_secs_f64()));
    json!({"ok": true, "uptime_seconds": uptime})
}

fn op_exec(req: &Value) -> Value {
    let Some(command) = req.get("command").and_then(Value::as_str) else {
        return json!({"error": "missing 'command' field"});
    };

    let mut cmd = std::process::Command::new("sh");
    cmd.arg("-c").arg(command);

    if let Some(env) = req.get("env").and_then(Value::as_object) {
        for (k, v) in env {
            if let Some(v_str) = v.as_str() {
                cmd.env(k, v_str);
            }
        }
    }

    match cmd.output() {
        Ok(output) => json!({
            "exit_code": output.status.code().unwrap_or(-1),
            "stdout": String::from_utf8_lossy(&output.stdout),
            "stderr": String::from_utf8_lossy(&output.stderr),
        }),
        Err(e) => json!({
            "exit_code": -1,
            "stdout": "",
            "stderr": format!("exec error: {e}"),
        }),
    }
}

fn op_read_file(req: &Value) -> Value {
    let Some(path) = req.get("path").and_then(Value::as_str) else {
        return json!({"error": "missing 'path' field"});
    };
    match std::fs::read(path) {
        Ok(data) => json!({"data": B64.encode(data)}),
        Err(e) => json!({"error": format!("read_file: {e}")}),
    }
}

fn op_write_file(req: &Value) -> Value {
    let Some(path) = req.get("path").and_then(Value::as_str) else {
        return json!({"error": "missing 'path' field"});
    };
    let Some(data_b64) = req.get("data").and_then(Value::as_str) else {
        return json!({"error": "missing 'data' field"});
    };
    let data = match B64.decode(data_b64) {
        Ok(d) => d,
        Err(e) => return json!({"error": format!("base64 decode: {e}")}),
    };
    match std::fs::write(path, data) {
        Ok(()) => json!({"ok": true}),
        Err(e) => json!({"error": format!("write_file: {e}")}),
    }
}

fn op_agent_status() -> Value {
    let pid = find_agent_pid();
    let task_id = std::fs::read_to_string("/tmp/thermite-task-id")
        .ok()
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty());

    match pid {
        Some(pid) => json!({
            "running": true,
            "pid": pid,
            "task_id": task_id,
        }),
        None => json!({
            "running": false,
            "pid": null,
            "task_id": null,
        }),
    }
}

/// Kill all `claude` agent subprocesses (matching the `claude --model` argv
/// pattern): SIGTERM, wait up to 5s, then SIGKILL any stragglers. Returns the
/// PIDs that were targeted for observability. No-op safe: when no claude
/// processes are running, returns an empty list with success.
fn op_agent_reap() -> Value {
    let pids = find_claude_pids();
    let killed = reap_pids(&pids);
    json!({
        "killed_pids": killed,
        "process_count": killed.len(),
    })
}

fn op_vm_stats() -> Value {
    let cpu_pct = measure_cpu_pct();
    let (memory_used_mb, memory_total_mb) = read_memory_mb().unwrap_or((0, 0));
    let disk_used_gb = read_disk_used_gb().unwrap_or(0.0);
    let (net_rx_bytes, net_tx_bytes) = read_net_bytes().unwrap_or((0, 0));

    json!({
        "cpu_pct": cpu_pct,
        "memory_used_mb": memory_used_mb,
        "memory_total_mb": memory_total_mb,
        "disk_used_gb": disk_used_gb,
        "net_rx_bytes": net_rx_bytes,
        "net_tx_bytes": net_tx_bytes,
    })
}

/// Atomically reset a workspace directory. In order:
///   1. Kill every process whose CWD is under `path` (walk `/proc/*/cwd`).
///   2. Unmount any bind/overlay mounts at or under `path` (deepest first).
///   3. `rm -rf path`.
///   4. Verify `path` is gone; error if it still exists.
///
/// Because this performs an unconditional recursive delete, `path` is validated
/// hard and fails closed: it must be an absolute path with no `..` components,
/// strictly under `/home/ubuntu/` or `/tmp/`. Replaces the SSH
/// `pkill + rm -rf + verify` dance in `agents/scripts/reset_workspace.sh`.
fn op_workspace_reset(req: &Value) -> Value {
    let Some(path) = req.get("path").and_then(Value::as_str) else {
        return json!({"error": "missing 'path' field"});
    };
    let start = Instant::now();

    if let Err(e) = validate_reset_path(path) {
        return json!({"error": format!("workspace_reset: {e}")});
    }

    // 1. Kill processes whose CWD is under `path` so nothing races the delete.
    let pids = find_pids_with_cwd_under(path);
    reap_pids(&pids);

    // 2. Unmount bind/overlay mounts inside `path` before removing the tree.
    if let Err(e) = unmount_under(path) {
        return json!({"error": format!("workspace_reset: {e}")});
    }

    // 3. Count entries (for observability), then remove the tree.
    let removed_count = count_entries(Path::new(path));
    if let Err(e) = std::fs::remove_dir_all(path) {
        // Already-absent is fine; anything else is a hard failure.
        if e.kind() != std::io::ErrorKind::NotFound {
            return json!({"error": format!("workspace_reset: rm -rf {path}: {e}")});
        }
    }

    // 4. Verify the path is actually gone.
    if Path::new(path).exists() {
        return json!({"error": format!("workspace_reset: path still exists after delete: {path}")});
    }

    json!({
        "ok": true,
        "removed_count": removed_count,
        "duration_ms": start.elapsed().as_millis() as u64,
    })
}

// ---------------------------------------------------------------------------
// vm_stats helpers
// ---------------------------------------------------------------------------

/// Raw CPU counters from /proc/stat's aggregate `cpu` line.
#[derive(Clone, Copy, Default)]
struct CpuSample {
    total: u64,
    idle: u64,
}

fn read_cpu_sample() -> Option<CpuSample> {
    let content = std::fs::read_to_string("/proc/stat").ok()?;
    for line in content.lines() {
        // The aggregate line starts with "cpu " (with a space), not "cpu0" etc.
        if !line.starts_with("cpu ") {
            continue;
        }
        let nums: Vec<u64> = line
            .split_whitespace()
            .skip(1) // skip "cpu"
            .filter_map(|s| s.parse().ok())
            .collect();
        // Fields: user nice system idle iowait irq softirq steal guest guest_nice
        if nums.len() < 4 {
            return None;
        }
        let idle = nums[3] + nums.get(4).copied().unwrap_or(0); // idle + iowait
        let total: u64 = nums.iter().sum();
        return Some(CpuSample { total, idle });
    }
    None
}

/// Measure CPU utilisation by sampling /proc/stat twice with a 100 ms gap.
/// Returns 0.0 if /proc/stat is unavailable or the delta is zero.
fn measure_cpu_pct() -> f64 {
    let s1 = read_cpu_sample().unwrap_or_default();
    std::thread::sleep(std::time::Duration::from_millis(100));
    let s2 = read_cpu_sample().unwrap_or_default();

    let total_delta = s2.total.saturating_sub(s1.total);
    let idle_delta = s2.idle.saturating_sub(s1.idle);

    if total_delta == 0 {
        return 0.0;
    }
    let busy = total_delta.saturating_sub(idle_delta);
    (busy as f64 / total_delta as f64 * 100.0).clamp(0.0, 100.0)
}

/// Parse MemTotal and MemAvailable from /proc/meminfo.
/// Returns (used_mb, total_mb).
fn read_memory_mb() -> Option<(u64, u64)> {
    let content = std::fs::read_to_string("/proc/meminfo").ok()?;
    let mut total_kb: Option<u64> = None;
    let mut available_kb: Option<u64> = None;

    for line in content.lines() {
        if let Some(rest) = line.strip_prefix("MemTotal:") {
            total_kb = rest.split_whitespace().next().and_then(|s| s.parse().ok());
        } else if let Some(rest) = line.strip_prefix("MemAvailable:") {
            available_kb = rest.split_whitespace().next().and_then(|s| s.parse().ok());
        }
        if total_kb.is_some() && available_kb.is_some() {
            break;
        }
    }

    let total_kb = total_kb?;
    let available_kb = available_kb?;
    let used_kb = total_kb.saturating_sub(available_kb);
    Some((used_kb / 1024, total_kb / 1024))
}

/// Return disk used for / in whole gigabytes (float) via statvfs.
fn read_disk_used_gb() -> Option<f64> {
    #[cfg(target_os = "linux")]
    {
        use nix::sys::statvfs::statvfs;
        let stat = statvfs("/").ok()?;
        let block_size = stat.block_size() as u64;
        let used_blocks = (stat.blocks() as u64).saturating_sub(stat.blocks_free() as u64);
        let used_bytes = used_blocks * block_size;
        Some(used_bytes as f64 / (1024.0 * 1024.0 * 1024.0))
    }
    #[cfg(not(target_os = "linux"))]
    {
        None
    }
}

/// Sum rx_bytes and tx_bytes across all non-loopback interfaces from /proc/net/dev.
/// Returns (rx_bytes, tx_bytes).
pub fn parse_net_dev(content: &str) -> (u64, u64) {
    let mut rx: u64 = 0;
    let mut tx: u64 = 0;

    // /proc/net/dev has two header lines, then one line per interface.
    for line in content.lines().skip(2) {
        // Each line: "  iface: rx_bytes rx_pkts ... tx_bytes ..."
        let Some(colon_pos) = line.find(':') else {
            continue;
        };
        let iface = line[..colon_pos].trim();
        if iface == "lo" {
            continue;
        }
        let fields: Vec<u64> = line[colon_pos + 1..]
            .split_whitespace()
            .filter_map(|s| s.parse().ok())
            .collect();
        // Column 0 = rx_bytes, column 8 = tx_bytes
        if let Some(&r) = fields.first() {
            rx = rx.saturating_add(r);
        }
        if let Some(&t) = fields.get(8) {
            tx = tx.saturating_add(t);
        }
    }
    (rx, tx)
}

fn read_net_bytes() -> Option<(u64, u64)> {
    let content = std::fs::read_to_string("/proc/net/dev").ok()?;
    Some(parse_net_dev(&content))
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

fn read_proc_uptime() -> Option<f64> {
    std::fs::read_to_string("/proc/uptime")
        .ok()?
        .split_whitespace()
        .next()?
        .parse()
        .ok()
}

/// Scan /proc for a process whose cmdline contains "thermite-entrypoint".
fn find_agent_pid() -> Option<u32> {
    let proc_dir = std::fs::read_dir("/proc").ok()?;
    for entry in proc_dir.flatten() {
        let name = entry.file_name();
        let Some(name_str) = name.to_str() else {
            continue;
        };
        let Ok(pid) = name_str.parse::<u32>() else {
            continue;
        };
        let Ok(cmdline) = std::fs::read_to_string(format!("/proc/{pid}/cmdline")) else {
            continue;
        };
        if cmdline.contains("thermite-entrypoint") {
            return Some(pid);
        }
    }
    None
}

/// True if a `/proc/<pid>/cmdline` (NUL-separated argv) looks like a `claude`
/// agent invocation: argv[0] basename is `claude` and `--model` is present.
fn cmdline_matches_claude(cmdline: &str) -> bool {
    let args: Vec<&str> = cmdline.split('\0').filter(|s| !s.is_empty()).collect();
    let has_claude = args.iter().any(|a| {
        let base = a.rsplit('/').next().unwrap_or(a);
        base == "claude"
    });
    let has_model = args.contains(&"--model");
    has_claude && has_model
}

/// Scan /proc for all processes whose cmdline matches the claude agent pattern.
/// Excludes our own pid. Returns an empty vec when /proc is unavailable.
fn find_claude_pids() -> Vec<u32> {
    let mut pids = Vec::new();
    let Ok(proc_dir) = std::fs::read_dir("/proc") else {
        return pids;
    };
    let self_pid = std::process::id();
    for entry in proc_dir.flatten() {
        let name = entry.file_name();
        let Some(name_str) = name.to_str() else {
            continue;
        };
        let Ok(pid) = name_str.parse::<u32>() else {
            continue;
        };
        if pid == self_pid {
            continue;
        }
        let Ok(cmdline) = std::fs::read_to_string(format!("/proc/{pid}/cmdline")) else {
            continue;
        };
        if cmdline_matches_claude(&cmdline) {
            pids.push(pid);
        }
    }
    pids
}

/// SIGTERM the given pids, wait up to `grace` for them to exit, then SIGKILL
/// any that remain. Returns the pids that were targeted.
#[cfg(target_os = "linux")]
fn reap_pids_with_grace(pids: &[u32], grace: std::time::Duration) -> Vec<u32> {
    use nix::sys::signal::{kill, Signal};
    use nix::unistd::Pid;
    use std::time::{Duration, Instant};

    if pids.is_empty() {
        return Vec::new();
    }

    for &pid in pids {
        let _ = kill(Pid::from_raw(pid as i32), Signal::SIGTERM);
    }

    let deadline = Instant::now() + grace;
    loop {
        let alive: Vec<u32> = pids.iter().copied().filter(|&p| process_alive(p)).collect();
        if alive.is_empty() {
            break;
        }
        if Instant::now() >= deadline {
            for &pid in &alive {
                let _ = kill(Pid::from_raw(pid as i32), Signal::SIGKILL);
            }
            break;
        }
        std::thread::sleep(Duration::from_millis(100));
    }

    pids.to_vec()
}

#[cfg(target_os = "linux")]
fn reap_pids(pids: &[u32]) -> Vec<u32> {
    reap_pids_with_grace(pids, std::time::Duration::from_secs(5))
}

/// True if a process with the given pid currently exists (signal 0 probe).
#[cfg(target_os = "linux")]
fn process_alive(pid: u32) -> bool {
    use nix::sys::signal::kill;
    use nix::unistd::Pid;
    kill(Pid::from_raw(pid as i32), None).is_ok()
}

/// Non-Linux fallback: no signal support (nix is a Linux-only dependency), and
/// /proc scanning yields no pids, so reaping is a no-op.
#[cfg(not(target_os = "linux"))]
fn reap_pids(pids: &[u32]) -> Vec<u32> {
    let _ = pids;
    Vec::new()
}

// ---------------------------------------------------------------------------
// workspace_reset helpers
// ---------------------------------------------------------------------------

/// Allowed roots for `workspace_reset`. The path must be strictly *under* one
/// of these (a bare root is rejected).
const RESET_ALLOWED_ROOTS: [&str; 2] = ["/home/ubuntu/", "/tmp/"];

/// Validate a `workspace_reset` target. Fails closed: the path must be
/// absolute, contain no `..` component, and sit strictly under an allowed root.
fn validate_reset_path(path: &str) -> Result<(), String> {
    let p = Path::new(path);
    if !p.is_absolute() {
        return Err(format!("path must be absolute: {path}"));
    }
    if p.components()
        .any(|c| matches!(c, std::path::Component::ParentDir))
    {
        return Err(format!("path must not contain '..': {path}"));
    }
    let under_allowed = RESET_ALLOWED_ROOTS
        .iter()
        .any(|root| path.starts_with(root) && path.len() > root.len());
    if !under_allowed {
        return Err(format!(
            "path outside allowed roots (/home/ubuntu/, /tmp/): {path}"
        ));
    }
    Ok(())
}

/// `path` with a single trailing slash, for prefix matching of descendants.
#[cfg(target_os = "linux")]
fn path_with_trailing_slash(path: &str) -> String {
    if path.ends_with('/') {
        path.to_string()
    } else {
        format!("{path}/")
    }
}

/// Recursively count filesystem entries at `path`, including `path` itself.
/// Does not follow symlinks (a symlink counts as one entry, not its target).
/// Returns 0 if `path` does not exist.
fn count_entries(path: &Path) -> u64 {
    let Ok(meta) = std::fs::symlink_metadata(path) else {
        return 0;
    };
    let mut count = 1;
    if meta.file_type().is_dir() {
        if let Ok(rd) = std::fs::read_dir(path) {
            for entry in rd.flatten() {
                count += count_entries(&entry.path());
            }
        }
    }
    count
}

/// Scan `/proc/*/cwd` for processes whose current working directory is `path`
/// or a descendant of it. Excludes our own pid. Empty when `/proc` is absent.
#[cfg(target_os = "linux")]
fn find_pids_with_cwd_under(path: &str) -> Vec<u32> {
    let mut pids = Vec::new();
    let Ok(proc_dir) = std::fs::read_dir("/proc") else {
        return pids;
    };
    let self_pid = std::process::id();
    let prefix = path_with_trailing_slash(path);
    for entry in proc_dir.flatten() {
        let name = entry.file_name();
        let Some(name_str) = name.to_str() else {
            continue;
        };
        let Ok(pid) = name_str.parse::<u32>() else {
            continue;
        };
        if pid == self_pid {
            continue;
        }
        let Ok(target) = std::fs::read_link(format!("/proc/{pid}/cwd")) else {
            continue;
        };
        let target = target.to_string_lossy();
        if target == path || target.starts_with(&prefix) {
            pids.push(pid);
        }
    }
    pids
}

#[cfg(not(target_os = "linux"))]
fn find_pids_with_cwd_under(path: &str) -> Vec<u32> {
    let _ = path;
    Vec::new()
}

/// Decode the octal escapes (`\040` space, `\011` tab, `\012` newline,
/// `\134` backslash) that `/proc/mounts` uses in mount-point fields.
#[cfg(target_os = "linux")]
fn decode_mount_field(raw: &str) -> String {
    let mut out = String::with_capacity(raw.len());
    let bytes = raw.as_bytes();
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'\\' && i + 3 < bytes.len() {
            let octal = &raw[i + 1..i + 4];
            if let Ok(code) = u8::from_str_radix(octal, 8) {
                out.push(code as char);
                i += 4;
                continue;
            }
        }
        out.push(bytes[i] as char);
        i += 1;
    }
    out
}

/// Unmount every mount at or under `path`, deepest first so nested mounts
/// unwind cleanly. Uses a lazy unmount (`umount -l`) so a busy mount detaches
/// rather than blocking the reset. No-op when `/proc/mounts` is unreadable.
#[cfg(target_os = "linux")]
fn unmount_under(path: &str) -> Result<(), String> {
    let Ok(content) = std::fs::read_to_string("/proc/mounts") else {
        return Ok(());
    };
    let prefix = path_with_trailing_slash(path);
    let mut mountpoints: Vec<String> = Vec::new();
    for line in content.lines() {
        // Fields: device mountpoint fstype options dump pass
        let mut fields = line.split_whitespace();
        let _device = fields.next();
        let Some(mp_raw) = fields.next() else {
            continue;
        };
        let mp = decode_mount_field(mp_raw);
        if mp == path || mp.starts_with(&prefix) {
            mountpoints.push(mp);
        }
    }
    // Deepest paths first.
    mountpoints.sort_by_key(|mp| std::cmp::Reverse(mp.len()));
    for mp in mountpoints {
        match std::process::Command::new("umount")
            .arg("-l")
            .arg(&mp)
            .output()
        {
            Ok(o) if o.status.success() => {}
            Ok(o) => {
                return Err(format!(
                    "umount {mp}: {}",
                    String::from_utf8_lossy(&o.stderr).trim()
                ))
            }
            Err(e) => return Err(format!("umount {mp}: {e}")),
        }
    }
    Ok(())
}

#[cfg(not(target_os = "linux"))]
fn unmount_under(path: &str) -> Result<(), String> {
    let _ = path;
    Ok(())
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ping_returns_ok_and_uptime() {
        START_TIME.get_or_init(Instant::now);
        let resp = dispatch(&json!({"op": "ping"}));
        assert_eq!(resp["ok"], true);
        assert!(resp["uptime_seconds"].as_f64().unwrap() >= 0.0);
    }

    #[test]
    fn exec_echo() {
        let resp = dispatch(&json!({"op": "exec", "command": "echo hello"}));
        assert_eq!(resp["exit_code"], 0);
        assert_eq!(resp["stdout"], "hello\n");
        assert_eq!(resp["stderr"], "");
    }

    #[test]
    fn exec_with_env() {
        let resp = dispatch(&json!({
            "op": "exec",
            "command": "echo $TEST_VAR",
            "env": {"TEST_VAR": "hello_from_env"}
        }));
        assert_eq!(resp["exit_code"], 0);
        assert_eq!(resp["stdout"], "hello_from_env\n");
    }

    #[test]
    fn exec_nonzero_exit() {
        let resp = dispatch(&json!({"op": "exec", "command": "exit 42"}));
        assert_eq!(resp["exit_code"], 42);
    }

    #[test]
    fn exec_missing_command() {
        let resp = dispatch(&json!({"op": "exec"}));
        assert!(resp["error"].as_str().unwrap().contains("command"));
    }

    #[test]
    fn exec_stderr() {
        let resp = dispatch(&json!({"op": "exec", "command": "echo err >&2"}));
        assert_eq!(resp["exit_code"], 0);
        assert_eq!(resp["stderr"], "err\n");
    }

    #[test]
    fn read_write_file_roundtrip() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("test.txt");
        let path_str = path.to_str().unwrap();

        let original = b"hello world\xfe\xff";
        let encoded = B64.encode(original);

        // Write
        let resp = dispatch(&json!({
            "op": "write_file",
            "path": path_str,
            "data": encoded,
        }));
        assert_eq!(resp["ok"], true);

        // Read back
        let resp = dispatch(&json!({"op": "read_file", "path": path_str}));
        let decoded = B64.decode(resp["data"].as_str().unwrap()).unwrap();
        assert_eq!(decoded, original);
    }

    #[test]
    fn read_file_not_found() {
        let resp = dispatch(&json!({"op": "read_file", "path": "/tmp/emberd-nonexistent-file"}));
        assert!(resp["error"].as_str().unwrap().contains("read_file"));
    }

    #[test]
    fn write_file_missing_path() {
        let resp = dispatch(&json!({"op": "write_file", "data": "aGVsbG8="}));
        assert!(resp["error"].as_str().unwrap().contains("path"));
    }

    #[test]
    fn write_file_bad_base64() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("bad.txt");
        let resp = dispatch(&json!({
            "op": "write_file",
            "path": path.to_str().unwrap(),
            "data": "not-valid-base64!!!",
        }));
        assert!(resp["error"].as_str().unwrap().contains("base64"));
    }

    #[test]
    fn agent_status_not_running() {
        // No thermite-entrypoint process is running during tests.
        let resp = dispatch(&json!({"op": "agent_status"}));
        assert_eq!(resp["running"], false);
        assert!(resp["pid"].is_null());
    }

    // -- agent_reap tests --
    //
    // NOTE: these tests deliberately never call op_agent_reap()/find_claude_pids()
    // against the live /proc. On a real agent host an actual `claude --model`
    // process is running, and reaping it would kill the test runner itself. We
    // exercise the pure matcher and the reap logic against spawned dummy pids.

    #[test]
    fn cmdline_matches_claude_positive() {
        assert!(cmdline_matches_claude("claude\0--model\0opus\0"));
        assert!(cmdline_matches_claude(
            "/usr/local/bin/claude\0--model\0sonnet\0--verbose\0"
        ));
    }

    #[test]
    fn cmdline_matches_claude_negative() {
        assert!(!cmdline_matches_claude(""));
        assert!(!cmdline_matches_claude("bash\0-c\0echo hi\0"));
        // claude without --model
        assert!(!cmdline_matches_claude("claude\0--help\0"));
        // --model without a claude binary
        assert!(!cmdline_matches_claude("python\0--model\0foo\0"));
        // substring that is not the binary basename
        assert!(!cmdline_matches_claude("claudette\0--model\0x\0"));
    }

    #[test]
    fn reap_empty_is_noop() {
        // No-op safe: empty input yields an empty list, no signals sent.
        assert!(reap_pids(&[]).is_empty());
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn reap_terminates_child_via_sigterm() {
        use std::process::Command;
        use std::time::Duration;

        let mut child = Command::new("sleep").arg("300").spawn().unwrap();
        let pid = child.id();
        assert!(process_alive(pid));

        let killed = reap_pids_with_grace(&[pid], Duration::from_millis(500));
        assert_eq!(killed, vec![pid]);

        let _ = child.wait(); // reap the zombie now that it has exited
        assert!(!process_alive(pid));
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn reap_escalates_to_sigkill_for_sigterm_ignorer() {
        use std::process::Command;
        use std::time::Duration;

        // This child ignores SIGTERM; only SIGKILL can stop it.
        let mut child = Command::new("sh")
            .arg("-c")
            .arg("trap '' TERM; sleep 300")
            .spawn()
            .unwrap();
        let pid = child.id();
        assert!(process_alive(pid));

        let killed = reap_pids_with_grace(&[pid], Duration::from_millis(300));
        assert_eq!(killed, vec![pid]);

        let _ = child.wait();
        assert!(!process_alive(pid));
    }

    // -- workspace_reset tests --

    #[test]
    fn validate_reset_path_accepts_under_roots() {
        assert!(validate_reset_path("/home/ubuntu/workspace").is_ok());
        assert!(validate_reset_path("/home/ubuntu/a/b/c").is_ok());
        assert!(validate_reset_path("/tmp/foo").is_ok());
        assert!(validate_reset_path("/tmp/foo/bar").is_ok());
    }

    #[test]
    fn validate_reset_path_rejects_unsafe() {
        // Outside allowed roots.
        assert!(validate_reset_path("/etc/passwd").is_err());
        assert!(validate_reset_path("/var/lib").is_err());
        // Bare roots are rejected (must be strictly under).
        assert!(validate_reset_path("/home/ubuntu").is_err());
        assert!(validate_reset_path("/tmp").is_err());
        assert!(validate_reset_path("/").is_err());
        // Prefix look-alikes that are not real descendants.
        assert!(validate_reset_path("/home/ubuntuevil").is_err());
        assert!(validate_reset_path("/tmpfoo").is_err());
        // Traversal and relative paths.
        assert!(validate_reset_path("/home/ubuntu/../etc").is_err());
        assert!(validate_reset_path("relative/path").is_err());
    }

    #[test]
    fn count_entries_counts_recursively() {
        let dir = tempfile::tempdir().unwrap();
        let base = dir.path();
        std::fs::create_dir_all(base.join("a/b")).unwrap();
        std::fs::write(base.join("a/f1"), b"x").unwrap();
        std::fs::write(base.join("a/b/f2"), b"y").unwrap();
        // base + a + a/b + a/f1 + a/b/f2 = 5
        assert_eq!(count_entries(base), 5);
    }

    #[test]
    fn count_entries_missing_is_zero() {
        assert_eq!(count_entries(Path::new("/tmp/emberd-nonexistent-xyz")), 0);
    }

    #[test]
    fn workspace_reset_missing_path() {
        let resp = dispatch(&json!({"op": "workspace_reset"}));
        assert!(resp["error"].as_str().unwrap().contains("path"));
    }

    #[test]
    fn workspace_reset_rejects_unsafe_path() {
        let resp = dispatch(&json!({"op": "workspace_reset", "path": "/etc"}));
        assert!(resp["error"].as_str().unwrap().contains("workspace_reset"));
    }

    #[test]
    fn workspace_reset_removes_tree_and_counts() {
        let base = std::env::temp_dir().join(format!("emberd-reset-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&base);
        std::fs::create_dir_all(base.join("a/b")).unwrap();
        std::fs::write(base.join("a/f1"), b"x").unwrap();
        std::fs::write(base.join("a/b/f2"), b"y").unwrap();

        let resp = dispatch(&json!({"op": "workspace_reset", "path": base.to_str().unwrap()}));
        assert_eq!(resp["ok"], true, "resp: {resp}");
        // base + a + a/b + a/f1 + a/b/f2 = 5
        assert_eq!(resp["removed_count"].as_u64().unwrap(), 5);
        assert!(resp["duration_ms"].as_u64().is_some());
        assert!(!base.exists(), "path should be gone after reset");
    }

    #[test]
    fn workspace_reset_absent_path_is_ok() {
        // A path that does not exist resets to "already clean" with count 0.
        let base = std::env::temp_dir().join(format!("emberd-reset-absent-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&base);
        let resp = dispatch(&json!({"op": "workspace_reset", "path": base.to_str().unwrap()}));
        assert_eq!(resp["ok"], true, "resp: {resp}");
        assert_eq!(resp["removed_count"].as_u64().unwrap(), 0);
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn workspace_reset_kills_process_with_cwd_inside() {
        use std::process::Command;

        let base = std::env::temp_dir().join(format!("emberd-reset-cwd-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&base);
        std::fs::create_dir_all(&base).unwrap();

        let mut child = Command::new("sleep")
            .arg("300")
            .current_dir(&base)
            .spawn()
            .unwrap();
        let pid = child.id();
        assert!(process_alive(pid));
        assert!(find_pids_with_cwd_under(base.to_str().unwrap()).contains(&pid));

        let resp = dispatch(&json!({"op": "workspace_reset", "path": base.to_str().unwrap()}));
        assert_eq!(resp["ok"], true, "resp: {resp}");

        let _ = child.wait();
        assert!(
            !process_alive(pid),
            "process with cwd inside should be killed"
        );
        assert!(!base.exists());
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn decode_mount_field_decodes_octal_escapes() {
        assert_eq!(decode_mount_field("/mnt/foo"), "/mnt/foo");
        assert_eq!(decode_mount_field("/mnt/a\\040b"), "/mnt/a b");
        assert_eq!(decode_mount_field("/mnt/a\\011b"), "/mnt/a\tb");
    }

    #[test]
    fn unknown_op() {
        let resp = dispatch(&json!({"op": "foobar"}));
        assert!(resp["error"].as_str().unwrap().contains("unknown op"));
    }

    #[test]
    fn missing_op() {
        let resp = dispatch(&json!({"hello": "world"}));
        assert!(resp["error"].as_str().unwrap().contains("unknown op"));
    }

    // -- vm_stats tests --

    // /proc/net/dev columns after the colon:
    // Receive (8): bytes packets errs drop fifo frame compressed multicast
    // Transmit (8): bytes packets errs drop fifo colls carrier compressed
    // tx_bytes is therefore at index 8 (0-indexed).

    #[test]
    fn parse_net_dev_sums_non_lo() {
        let content = "Inter-|   Receive                                                |  Transmit\n\
 face |bytes    packets errs drop fifo frame compressed multicast|bytes    packets errs drop fifo colls carrier compressed\n\
    lo:  1000      10    0    0    0     0          0         0  2000      20    0    0    0     0       0          0\n\
  eth0: 10000     100    0    0    0     0          0         0 20000     200    0    0    0     0       0          0\n\
  eth1:  5000      50    0    0    0     0          0         0  8000      80    0    0    0     0       0          0\n";
        let (rx, tx) = parse_net_dev(content);
        assert_eq!(rx, 15000);
        assert_eq!(tx, 28000);
    }

    #[test]
    fn parse_net_dev_no_interfaces() {
        let content = "Inter-|   Receive                                                |  Transmit\n\
 face |bytes    packets errs drop fifo frame compressed multicast|bytes    packets errs drop fifo colls carrier compressed\n\
    lo:  1000      10    0    0    0     0          0         0  2000      20    0    0    0     0       0          0\n";
        let (rx, tx) = parse_net_dev(content);
        assert_eq!(rx, 0);
        assert_eq!(tx, 0);
    }

    #[test]
    fn parse_net_dev_empty() {
        let (rx, tx) = parse_net_dev("Inter-|\n face |\n");
        assert_eq!(rx, 0);
        assert_eq!(tx, 0);
    }

    #[test]
    fn vm_stats_dispatch_returns_expected_fields() {
        let resp = dispatch(&json!({"op": "vm_stats"}));
        assert!(resp.get("cpu_pct").is_some(), "missing cpu_pct");
        assert!(
            resp.get("memory_used_mb").is_some(),
            "missing memory_used_mb"
        );
        assert!(
            resp.get("memory_total_mb").is_some(),
            "missing memory_total_mb"
        );
        assert!(resp.get("disk_used_gb").is_some(), "missing disk_used_gb");
        assert!(resp.get("net_rx_bytes").is_some(), "missing net_rx_bytes");
        assert!(resp.get("net_tx_bytes").is_some(), "missing net_tx_bytes");
        assert!(resp["cpu_pct"].as_f64().unwrap() >= 0.0);
        assert!(resp["cpu_pct"].as_f64().unwrap() <= 100.0);
    }

    #[test]
    fn read_cpu_sample_parses_proc_stat() {
        // Verify that on Linux the sample returns non-zero totals.
        if !std::path::Path::new("/proc/stat").exists() {
            return;
        }
        let s = read_cpu_sample().expect("should parse /proc/stat");
        assert!(s.total > 0, "total CPU ticks should be > 0");
    }

    #[test]
    fn read_memory_mb_returns_positive() {
        if !std::path::Path::new("/proc/meminfo").exists() {
            return;
        }
        let (used, total) = read_memory_mb().expect("should parse /proc/meminfo");
        assert!(total > 0, "total memory should be > 0");
        assert!(used <= total, "used memory should be <= total");
    }

    // -- Integration test via UDS --

    #[test]
    fn uds_roundtrip() {
        let dir = tempfile::tempdir().unwrap();
        let sock_path = dir.path().join("emberd.sock");

        let listener = UnixListener::bind(&sock_path).unwrap();
        let handle = std::thread::spawn(move || {
            let (stream, _) = listener.accept().unwrap();
            handle_connection(stream).unwrap();
        });

        let stream = std::os::unix::net::UnixStream::connect(&sock_path).unwrap();
        let mut writer = &stream;
        let mut reader = BufReader::new(&stream);

        // ping
        writer.write_all(b"{\"op\":\"ping\"}\n").unwrap();
        writer.flush().unwrap();
        let mut line = String::new();
        reader.read_line(&mut line).unwrap();
        let resp: Value = serde_json::from_str(&line).unwrap();
        assert_eq!(resp["ok"], true);

        // exec
        line.clear();
        writer
            .write_all(b"{\"op\":\"exec\",\"command\":\"echo uds_test\"}\n")
            .unwrap();
        writer.flush().unwrap();
        reader.read_line(&mut line).unwrap();
        let resp: Value = serde_json::from_str(&line).unwrap();
        assert_eq!(resp["exit_code"], 0);
        assert_eq!(resp["stdout"], "uds_test\n");

        // read/write roundtrip
        let tmp = dir.path().join("uds_file.txt");
        let tmp_str = tmp.to_str().unwrap();
        let b64 = B64.encode(b"uds roundtrip data");

        line.clear();
        let msg = format!("{{\"op\":\"write_file\",\"path\":\"{tmp_str}\",\"data\":\"{b64}\"}}\n");
        writer.write_all(msg.as_bytes()).unwrap();
        writer.flush().unwrap();
        reader.read_line(&mut line).unwrap();
        let resp: Value = serde_json::from_str(&line).unwrap();
        assert_eq!(resp["ok"], true);

        line.clear();
        let msg = format!("{{\"op\":\"read_file\",\"path\":\"{tmp_str}\"}}\n");
        writer.write_all(msg.as_bytes()).unwrap();
        writer.flush().unwrap();
        reader.read_line(&mut line).unwrap();
        let resp: Value = serde_json::from_str(&line).unwrap();
        let decoded = B64.decode(resp["data"].as_str().unwrap()).unwrap();
        assert_eq!(decoded, b"uds roundtrip data");

        // Close connection to let handle_connection exit
        drop(stream);
        handle.join().unwrap();
    }

    #[test]
    fn invalid_json_returns_error() {
        let dir = tempfile::tempdir().unwrap();
        let sock_path = dir.path().join("bad.sock");

        let listener = UnixListener::bind(&sock_path).unwrap();
        let handle = std::thread::spawn(move || {
            let (stream, _) = listener.accept().unwrap();
            handle_connection(stream).unwrap();
        });

        let stream = std::os::unix::net::UnixStream::connect(&sock_path).unwrap();
        let mut writer = &stream;
        let mut reader = BufReader::new(&stream);

        writer.write_all(b"not json\n").unwrap();
        writer.flush().unwrap();

        let mut line = String::new();
        reader.read_line(&mut line).unwrap();
        let resp: Value = serde_json::from_str(&line).unwrap();
        assert!(resp["error"].as_str().unwrap().contains("invalid JSON"));

        drop(stream);
        handle.join().unwrap();
    }
}

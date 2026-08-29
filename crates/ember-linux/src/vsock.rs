//! Host-side permissions for the Firecracker vsock unix socket.
//!
//! Firecracker binds the host end of a vsock device as a unix socket and
//! creates it with the hypervisor's own identity and the hypervisor's
//! umask. ember runs firecracker as root (the `ember` shim on `PATH`
//! execs `sudo -n ember.real`), so the socket lands as `root:root 0755`.
//!
//! `connect(2)` on a unix socket requires **write** permission on the
//! socket inode, and `0755` grants write to root alone. Every non-root
//! client therefore gets `EACCES`, which clients typically treat as
//! "vsock unavailable" and paper over with an SSH fallback — the failure
//! is silent and the vsock fast path is inert on the whole host.
//!
//! # Permission model
//!
//! We hand the socket to the user who invoked ember and to nobody else:
//!
//! - **Invoked via sudo** (`SUDO_UID`/`SUDO_GID` present and non-root):
//!   `chmod 0600` then `chown` to that uid/gid. Exactly one unprivileged
//!   account can connect.
//! - **Invoked as root for real** (no `SUDO_UID`, or `SUDO_UID=0`, or a
//!   value that does not parse): leave the socket owned by root and
//!   narrow it to `0600`. This *fails closed* — root keeps access,
//!   nobody else gains any.
//!
//! The tempting alternative, `chmod 0666`, is wrong. The guest end of
//! this socket is emberd, which executes commands inside the VM. A
//! world-writable socket makes that a local privilege-escalation vector:
//! any account on the host — a compromised service user, another
//! tenant — could run code in every VM. Narrowing to the invoking user
//! keeps the vsock path exactly as privileged as the `sudo` that created
//! the VM, and no more. Do not widen this without a reason that survives
//! that sentence.

use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::Path;
use std::thread;
use std::time::{Duration, Instant};

use ember_core::error::{Error, Result};

/// How long to wait for firecracker to bind the host-side socket.
///
/// Firecracker creates the socket while building the microVM, i.e. during
/// the `InstanceStart` action, so it is normally already present by the
/// time this runs. The wait covers the window where `InstanceStart` has
/// been acknowledged but the bind has not yet landed on the filesystem.
const SOCKET_TIMEOUT: Duration = Duration::from_secs(5);

/// Poll interval while waiting for the socket to appear. Sleeping (rather
/// than spinning) keeps this off the CPU on a host that is busy booting a VM.
const SOCKET_POLL_INTERVAL: Duration = Duration::from_millis(20);

/// Socket mode: owner read/write only. Owner-write is what `connect(2)`
/// checks; group and other get nothing.
const SOCKET_MODE: u32 = 0o600;

/// Search bit for "other" on the VM directory. Traversing a directory to
/// reach the socket needs execute (search) permission, and nothing else —
/// not read (no listing) and emphatically not write (no creating or
/// unlinking siblings such as `firecracker.sock`).
const DIR_SEARCH_BIT: u32 = 0o001;

/// Grant the invoking user access to a freshly-created vsock socket.
///
/// **Ordering matters**: firecracker creates the socket during
/// `InstanceStart`, so this must run *after* the VM has been configured
/// and booted. Calling it earlier finds no file and times out.
///
/// Errors are the caller's to soften: by the time this runs the VM is
/// already up, so a permission failure degrades vsock rather than
/// invalidating the boot. It must never be swallowed silently, though —
/// silence is precisely what let a host run with vsock inert.
pub fn secure_host_socket(path: &Path) -> Result<()> {
    wait_for_socket(path)?;

    // Narrow before handing over. If the chown below fails we are left
    // with `root:root 0600`, which is tighter than what firecracker
    // created, never looser.
    fs::set_permissions(path, fs::Permissions::from_mode(SOCKET_MODE)).map_err(|e| Error::Io {
        path: path.to_path_buf(),
        source: e,
    })?;

    match invoking_user() {
        Some((uid, gid)) => {
            nix::unistd::chown(
                path,
                Some(nix::unistd::Uid::from_raw(uid)),
                Some(nix::unistd::Gid::from_raw(gid)),
            )
            .map_err(|e| Error::Io {
                path: path.to_path_buf(),
                source: std::io::Error::from_raw_os_error(e as i32),
            })?;

            if let Some(dir) = path.parent() {
                ensure_dir_searchable(dir)?;
            }
        }
        None => {
            // Genuine root invocation (or a service context that dropped
            // SUDO_UID). We cannot tell which unprivileged account, if
            // any, is meant to have access, so we grant none.
            eprintln!(
                "note: vsock socket {} left root-only (no SUDO_UID in environment)",
                path.display()
            );
        }
    }

    Ok(())
}

/// Wait for firecracker to bind the host-side socket.
fn wait_for_socket(path: &Path) -> Result<()> {
    wait_for_socket_within(path, SOCKET_TIMEOUT)
}

/// [`wait_for_socket`] with an explicit deadline, so the timeout path can
/// be tested without a five-second sleep.
fn wait_for_socket_within(path: &Path, timeout: Duration) -> Result<()> {
    let start = Instant::now();
    while start.elapsed() < timeout {
        // `exists()` follows symlinks and returns false on a dangling
        // path; a bound unix socket is a real inode, so this is enough.
        if path.exists() {
            return Ok(());
        }
        thread::sleep(SOCKET_POLL_INTERVAL);
    }
    Err(Error::Vsock(format!(
        "vsock socket did not appear at {} within {:?}",
        path.display(),
        timeout,
    )))
}

/// Ensure a directory can be traversed to reach the socket inside it.
///
/// Adds the "other" search bit and *only* that bit, and only when it is
/// missing (a default root umask of 022 already yields `0755`, so this is
/// normally a no-op). We deliberately do not chown the directory to the
/// invoking user: it also holds the firecracker API socket and the VM's
/// state files, and directory write permission would let that user
/// unlink or replace them.
fn ensure_dir_searchable(dir: &Path) -> Result<()> {
    let meta = fs::metadata(dir).map_err(|e| Error::Io {
        path: dir.to_path_buf(),
        source: e,
    })?;
    let mode = meta.permissions().mode() & 0o7777;
    if mode & DIR_SEARCH_BIT != 0 {
        return Ok(());
    }
    fs::set_permissions(dir, fs::Permissions::from_mode(mode | DIR_SEARCH_BIT)).map_err(|e| {
        Error::Io {
            path: dir.to_path_buf(),
            source: e,
        }
    })
}

/// Resolve the pre-sudo user from the environment.
///
/// Returns `None` for a genuine root invocation, so the caller fails
/// closed and leaves the socket root-only.
fn invoking_user() -> Option<(u32, u32)> {
    parse_invoking_user(
        std::env::var("SUDO_UID").ok().as_deref(),
        std::env::var("SUDO_GID").ok().as_deref(),
    )
}

/// Pure half of [`invoking_user`], split out so it is testable without
/// mutating process-global environment state.
///
/// Both variables must be present and parse. `SUDO_UID=0` means root
/// sudo'd to root: there is no unprivileged user to hand the socket to,
/// so it is treated the same as no sudo at all.
fn parse_invoking_user(uid: Option<&str>, gid: Option<&str>) -> Option<(u32, u32)> {
    let uid: u32 = uid?.trim().parse().ok()?;
    let gid: u32 = gid?.trim().parse().ok()?;
    if uid == 0 {
        return None;
    }
    Some((uid, gid))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::net::UnixListener;

    #[test]
    fn parse_invoking_user_accepts_normal_sudo() {
        assert_eq!(
            parse_invoking_user(Some("1000"), Some("1000")),
            Some((1000, 1000))
        );
    }

    #[test]
    fn parse_invoking_user_tolerates_whitespace() {
        assert_eq!(
            parse_invoking_user(Some(" 1000\n"), Some("1000")),
            Some((1000, 1000))
        );
    }

    #[test]
    fn parse_invoking_user_rejects_root_target() {
        // root -> root sudo: no unprivileged user to hand the socket to.
        assert_eq!(parse_invoking_user(Some("0"), Some("0")), None);
    }

    #[test]
    fn parse_invoking_user_fails_closed_on_missing_vars() {
        assert_eq!(parse_invoking_user(None, None), None);
        assert_eq!(parse_invoking_user(Some("1000"), None), None);
        assert_eq!(parse_invoking_user(None, Some("1000")), None);
    }

    #[test]
    fn parse_invoking_user_fails_closed_on_garbage() {
        assert_eq!(parse_invoking_user(Some("nobody"), Some("1000")), None);
        assert_eq!(parse_invoking_user(Some("-1"), Some("1000")), None);
        assert_eq!(parse_invoking_user(Some(""), Some("")), None);
    }

    #[test]
    fn secure_host_socket_narrows_mode() {
        let dir = tempfile::tempdir().unwrap();
        let sock = dir.path().join("vsock.sock");
        let _listener = UnixListener::bind(&sock).unwrap();
        // Reproduce what firecracker leaves behind under a 022 umask.
        fs::set_permissions(&sock, fs::Permissions::from_mode(0o755)).unwrap();

        secure_host_socket(&sock).unwrap();

        let mode = fs::metadata(&sock).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, SOCKET_MODE, "group/other must not retain any access");
    }

    #[test]
    fn secure_host_socket_times_out_when_socket_never_appears() {
        // Not a real firecracker path: assert the error is reported rather
        // than the missing socket being silently accepted.
        let dir = tempfile::tempdir().unwrap();
        let err = wait_for_socket_within(&dir.path().join("nope.sock"), Duration::from_millis(50))
            .unwrap_err();
        assert!(err.to_string().contains("did not appear"), "{err}");
    }

    #[test]
    fn ensure_dir_searchable_adds_only_the_search_bit() {
        let dir = tempfile::tempdir().unwrap();
        let sub = dir.path().join("vm");
        fs::create_dir(&sub).unwrap();
        fs::set_permissions(&sub, fs::Permissions::from_mode(0o700)).unwrap();

        ensure_dir_searchable(&sub).unwrap();

        let mode = fs::metadata(&sub).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o701, "must grant search only — no read, no write");
    }

    #[test]
    fn ensure_dir_searchable_is_a_noop_when_already_traversable() {
        let dir = tempfile::tempdir().unwrap();
        let sub = dir.path().join("vm");
        fs::create_dir(&sub).unwrap();
        fs::set_permissions(&sub, fs::Permissions::from_mode(0o755)).unwrap();

        ensure_dir_searchable(&sub).unwrap();

        let mode = fs::metadata(&sub).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o755);
    }
}

//! Post-import rootfs inspection helpers.
//!
//! Checks the unpacked rootfs directory for binaries that Ember requires at
//! runtime (currently: an SSH server). Called after layer extraction but
//! before ext4 image creation, while the rootfs is still a plain directory.

use std::path::Path;

/// SSH server binaries that Ember can use to connect to a VM.
const SSH_SERVER_PATHS: &[&str] = &[
    "usr/sbin/sshd",
    "usr/bin/sshd",
    "usr/sbin/dropbear",
    "usr/bin/dropbear",
];

/// Return `true` if the rootfs contains a recognisable SSH server binary.
///
/// Checks for OpenSSH (`sshd`) and Dropbear at their conventional install
/// locations. The rootfs must be an unpacked directory (not a mounted image).
pub fn rootfs_has_ssh_server(rootfs_dir: &Path) -> bool {
    SSH_SERVER_PATHS
        .iter()
        .any(|rel| rootfs_dir.join(rel).exists())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    fn make_rootfs(tmp: &Path, paths: &[&str]) -> std::io::Result<()> {
        for rel in paths {
            let full = tmp.join(rel);
            fs::create_dir_all(full.parent().unwrap())?;
            fs::write(&full, b"")?;
        }
        Ok(())
    }

    #[test]
    fn detects_usr_sbin_sshd() {
        let dir = tempfile::tempdir().unwrap();
        make_rootfs(dir.path(), &["usr/sbin/sshd"]).unwrap();
        assert!(rootfs_has_ssh_server(dir.path()));
    }

    #[test]
    fn detects_usr_bin_sshd() {
        let dir = tempfile::tempdir().unwrap();
        make_rootfs(dir.path(), &["usr/bin/sshd"]).unwrap();
        assert!(rootfs_has_ssh_server(dir.path()));
    }

    #[test]
    fn detects_usr_sbin_dropbear() {
        let dir = tempfile::tempdir().unwrap();
        make_rootfs(dir.path(), &["usr/sbin/dropbear"]).unwrap();
        assert!(rootfs_has_ssh_server(dir.path()));
    }

    #[test]
    fn detects_usr_bin_dropbear() {
        let dir = tempfile::tempdir().unwrap();
        make_rootfs(dir.path(), &["usr/bin/dropbear"]).unwrap();
        assert!(rootfs_has_ssh_server(dir.path()));
    }

    #[test]
    fn returns_false_when_no_ssh_server() {
        let dir = tempfile::tempdir().unwrap();
        make_rootfs(dir.path(), &["usr/bin/sh", "usr/sbin/crond"]).unwrap();
        assert!(!rootfs_has_ssh_server(dir.path()));
    }

    #[test]
    fn returns_false_for_empty_rootfs() {
        let dir = tempfile::tempdir().unwrap();
        assert!(!rootfs_has_ssh_server(dir.path()));
    }
}

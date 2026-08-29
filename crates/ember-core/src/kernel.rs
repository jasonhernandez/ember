//! Named kernel presets for Firecracker microVMs.
//!
//! Provides [`KernelPreset`] (known kernels with download URLs) and
//! [`KernelSpec`] (either a preset name or an explicit file path).
//! Both the CLI flags and YAML config parse into `KernelSpec`.

pub mod build;

use std::fmt;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::str::FromStr;

use serde::de;

use crate::error::{Error, Result};
use crate::state::store::StateStore;

/// ELF magic. Firecracker (x86_64) boots an uncompressed ELF `vmlinux`.
const ELF_MAGIC: [u8; 4] = [0x7f, b'E', b'L', b'F'];

/// arm64 Linux boot `Image` magic, stored at offset 56 of the header
/// (see the kernel's `Documentation/arm64/booting.rst`). Apple Virtualization
/// boots this format rather than ELF.
const ARM64_IMAGE_MAGIC: [u8; 4] = *b"ARM\x64";
const ARM64_IMAGE_MAGIC_OFFSET: usize = 56;

/// Number of header bytes read when sanity-checking a kernel image.
const KERNEL_HEADER_LEN: usize = ARM64_IMAGE_MAGIC_OFFSET + ARM64_IMAGE_MAGIC.len();

/// Whether two paths name the same file on disk.
///
/// Symlinks defeat naive path comparison: a state directory reached as
/// `/var/lib/ember` may be the very same directory as `~/.thermite/ember`.
/// Canonicalize both sides (which resolves symlinks in every component) and,
/// as a backstop for filesystems where canonicalization can still differ,
/// compare the underlying device/inode pair.
///
/// A path that does not exist is never "the same file" as anything.
fn is_same_file(a: &Path, b: &Path) -> bool {
    if let (Ok(a), Ok(b)) = (std::fs::canonicalize(a), std::fs::canonicalize(b)) {
        if a == b {
            return true;
        }
    }

    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        if let (Ok(am), Ok(bm)) = (std::fs::metadata(a), std::fs::metadata(b)) {
            return am.dev() == bm.dev() && am.ino() == bm.ino();
        }
    }

    false
}

/// Fail loudly unless `path` holds something that could plausibly boot.
///
/// A 0-byte or garbage kernel is otherwise only discovered several steps
/// later, as a firecracker `Unable to read elf header` during `pool start`,
/// where the natural suspicion is images or networking rather than the
/// kernel. Catch it at install time instead.
fn validate_kernel_image(path: &Path) -> anyhow::Result<()> {
    let meta =
        std::fs::metadata(path).map_err(|e| anyhow::anyhow!("kernel '{}': {e}", path.display()))?;

    if meta.len() == 0 {
        anyhow::bail!(
            "kernel '{}' is 0 bytes — refusing to record an unbootable kernel.\n\
             Hint: restore a known-good kernel image, or run `ember kernel build` \
             to compile one.",
            path.display()
        );
    }

    let mut header = [0u8; KERNEL_HEADER_LEN];
    let read = {
        use std::io::Read;
        let mut f = std::fs::File::open(path)
            .map_err(|e| anyhow::anyhow!("kernel '{}': {e}", path.display()))?;
        let mut filled = 0;
        loop {
            match f.read(&mut header[filled..]) {
                Ok(0) => break,
                Ok(n) => filled += n,
                Err(e) if e.kind() == std::io::ErrorKind::Interrupted => {}
                Err(e) => return Err(anyhow::anyhow!("kernel '{}': {e}", path.display())),
            }
            if filled == header.len() {
                break;
            }
        }
        filled
    };

    let is_elf = read >= ELF_MAGIC.len() && header[..ELF_MAGIC.len()] == ELF_MAGIC;
    let is_arm64_image = read >= KERNEL_HEADER_LEN
        && header[ARM64_IMAGE_MAGIC_OFFSET..KERNEL_HEADER_LEN] == ARM64_IMAGE_MAGIC;

    if !is_elf && !is_arm64_image {
        let head: Vec<String> = header[..read.min(4)]
            .iter()
            .map(|b| format!("{b:02x}"))
            .collect();
        anyhow::bail!(
            "kernel '{}' ({} bytes) is not a bootable kernel image: expected ELF magic \
             \\x7fELF or an arm64 boot Image, but the file starts with [{}].\n\
             Hint: point --kernel at an uncompressed vmlinux (not a bzImage, tarball, \
             or partial download), or run `ember kernel build` to compile one.",
            path.display(),
            meta.len(),
            head.join(" ")
        );
    }

    Ok(())
}

/// Download a file using curl.
pub fn download_file(url: &str, dest: &Path) -> Result<()> {
    let output = Command::new("curl")
        .args(["-fSL", "-o"])
        .arg(dest)
        .arg(url)
        .output()
        .map_err(|e| Error::CommandExec {
            command: "curl".to_string(),
            source: e,
        })?;

    Error::check_command("curl", output)?;
    Ok(())
}

/// Download a kernel to `dest`, atomically and only if it is usable.
///
/// The download lands on a `.partial` sibling that is validated and only then
/// renamed into place. An interrupted or truncated transfer therefore never
/// leaves a plausible-looking kernel at `dest` for a later run to trust.
fn download_kernel(url: &str, dest: &Path) -> anyhow::Result<()> {
    let mut partial = dest.as_os_str().to_os_string();
    partial.push(".partial");
    let partial = PathBuf::from(partial);

    let result = download_file(url, &partial)
        .map_err(anyhow::Error::from)
        .and_then(|()| validate_kernel_image(&partial))
        .and_then(|()| {
            std::fs::rename(&partial, dest).map_err(|e| {
                anyhow::anyhow!(
                    "failed to install kernel '{}' → '{}': {e}",
                    partial.display(),
                    dest.display()
                )
            })
        });

    if result.is_err() {
        let _ = std::fs::remove_file(&partial);
    }
    result
}

/// Named kernel presets with known download URLs.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum KernelPreset {
    /// Firecracker CI kernel (vmlinux-6.1.102). Includes overlayfs,
    /// cgroups, namespaces, iptables, bridge, veth, and virtio-rng.
    Stock,

    /// Custom-built kernel based on Amazon Linux 6.1.163 with Docker
    /// networking support (iptables raw, nftables, dummy interface).
    /// Built locally via `ember kernel build`.
    Docker,
}

/// The default kernel preset used when no kernel is specified.
pub const DEFAULT_PRESET: KernelPreset = KernelPreset::Docker;

impl KernelPreset {
    /// Download URL for this preset on the current architecture.
    ///
    /// Returns `None` for presets that must be built locally.
    pub fn url(&self) -> Option<String> {
        let arch = match std::env::consts::ARCH {
            "aarch64" => "aarch64",
            _ => "x86_64",
        };
        match self {
            KernelPreset::Stock => Some(format!(
                "https://s3.amazonaws.com/spec.ccfc.min/firecracker-ci/v1.11/{arch}/vmlinux-6.1.102"
            )),
            KernelPreset::Docker => None,
        }
    }

    /// Filename used when saving this kernel to the kernels/ directory.
    pub fn filename(&self) -> &'static str {
        match self {
            KernelPreset::Stock => "vmlinux-6.1.102",
            KernelPreset::Docker => "vmlinux-docker-6.1.163",
        }
    }
}

impl fmt::Display for KernelPreset {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            KernelPreset::Stock => write!(f, "stock"),
            KernelPreset::Docker => write!(f, "docker"),
        }
    }
}

impl FromStr for KernelPreset {
    type Err = String;

    fn from_str(s: &str) -> std::result::Result<Self, Self::Err> {
        match s.to_ascii_lowercase().as_str() {
            "stock" => {
                #[cfg(target_os = "macos")]
                return Err(
                    "--kernel stock is not supported on macOS.\n\
                     The Firecracker CI kernel lacks virtio-pci/virtio-console drivers required by\n\
                     Apple Virtualization. Build a macOS-compatible kernel with:\n\
                         ember kernel build"
                        .to_string(),
                );
                #[cfg(not(target_os = "macos"))]
                Ok(KernelPreset::Stock)
            }
            "docker" => Ok(KernelPreset::Docker),
            _ => Err(format!("unknown kernel preset '{s}'")),
        }
    }
}

/// A kernel specification: either a named preset or an explicit file path.
///
/// When parsed from a string (CLI flag or YAML config), known preset names
/// (`stock`, `docker`) are recognized; anything else is treated as a filesystem path.
#[derive(Debug, Clone, PartialEq)]
pub enum KernelSpec {
    Preset(KernelPreset),
    Path(PathBuf),
}

impl KernelSpec {
    /// Resolve this kernel spec to a concrete filesystem path.
    ///
    /// For downloadable presets, downloads the kernel to the state store's
    /// `kernels/` directory if not already cached. For locally-built presets,
    /// checks that the kernel exists and returns a helpful error if not.
    /// For paths, copies the kernel into the store — unless it is already
    /// there, in which case the copy is skipped (copying a file onto itself
    /// truncates it to 0 bytes).
    ///
    /// Every branch validates the resulting file before returning it, so a
    /// destroyed, truncated, or partially downloaded kernel fails here rather
    /// than as an opaque firecracker error several steps later.
    pub fn resolve(&self, store: &StateStore) -> anyhow::Result<PathBuf> {
        match self {
            KernelSpec::Path(p) => {
                let src = crate::config::vm::expand_tilde(p);
                let src = std::fs::canonicalize(&src)
                    .map_err(|e| anyhow::anyhow!("kernel path '{}': {e}", src.display()))?;
                let filename = src
                    .file_name()
                    .ok_or_else(|| anyhow::anyhow!("kernel path has no filename"))?;
                let dest = store.kernel_dir().join(filename);

                // Never copy a file onto itself: std::fs::copy opens the
                // destination with O_TRUNC and then reads an empty source,
                // destroying the kernel and reporting success. The desired end
                // state already holds, so this is a no-op success.
                if is_same_file(&src, &dest) {
                    validate_kernel_image(&dest)?;
                    println!(
                        "Kernel already in place at {} — nothing to copy.",
                        dest.display()
                    );
                    return Ok(dest);
                }

                // Validate before copying so a bad source cannot clobber a
                // good kernel already sitting at the destination.
                validate_kernel_image(&src)?;
                std::fs::copy(&src, &dest).map_err(|e| {
                    anyhow::anyhow!(
                        "failed to copy kernel '{}' → '{}': {e}",
                        src.display(),
                        dest.display()
                    )
                })?;
                validate_kernel_image(&dest)?;
                println!("Copied kernel {} → {}", src.display(), dest.display());
                Ok(dest)
            }
            KernelSpec::Preset(preset) => {
                let dest = store.kernel_dir().join(preset.filename());
                if dest.exists() {
                    validate_kernel_image(&dest)?;
                    println!("Using {preset} kernel at {}", dest.display());
                    return Ok(dest);
                }
                match preset.url() {
                    Some(url) => {
                        println!("Downloading {preset} kernel from {url}...");
                        download_kernel(&url, &dest)?;
                        println!("Kernel saved to {}", dest.display());
                        Ok(dest)
                    }
                    None => {
                        #[cfg(target_os = "macos")]
                        anyhow::bail!(
                            "Default kernel has not been built yet.\n\
                             Hint: run `ember kernel build` to compile a macOS-compatible kernel."
                        );
                        #[cfg(not(target_os = "macos"))]
                        anyhow::bail!(
                            "Default kernel has not been built yet.\n\
                             Hint: run `sudo ember kernel build` to compile a kernel \
                             with Docker networking support,\n\
                             or use `--kernel stock` for a pre-built kernel without \
                             Docker support."
                        );
                    }
                }
            }
        }
    }
}

impl FromStr for KernelSpec {
    type Err = String;

    fn from_str(s: &str) -> std::result::Result<Self, Self::Err> {
        // Known preset keywords must never silently fall back to a path so
        // that platform-specific rejections (e.g. "stock" on macOS) surface.
        let is_preset_keyword = matches!(s.to_ascii_lowercase().as_str(), "stock" | "docker");
        if is_preset_keyword {
            s.parse::<KernelPreset>().map(KernelSpec::Preset)
        } else {
            Ok(KernelSpec::Path(PathBuf::from(s)))
        }
    }
}

impl fmt::Display for KernelSpec {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            KernelSpec::Preset(p) => write!(f, "{p}"),
            KernelSpec::Path(p) => write!(f, "{}", p.display()),
        }
    }
}

impl<'de> de::Deserialize<'de> for KernelSpec {
    fn deserialize<D>(deserializer: D) -> std::result::Result<Self, D::Error>
    where
        D: de::Deserializer<'de>,
    {
        let s = String::deserialize(deserializer)?;
        s.parse::<KernelSpec>().map_err(de::Error::custom)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[cfg(not(target_os = "macos"))]
    #[test]
    fn parse_preset_stock() {
        assert_eq!("stock".parse::<KernelPreset>(), Ok(KernelPreset::Stock));
    }

    #[test]
    fn parse_preset_docker() {
        assert_eq!("docker".parse::<KernelPreset>(), Ok(KernelPreset::Docker));
    }

    #[cfg(not(target_os = "macos"))]
    #[test]
    fn parse_preset_case_insensitive() {
        assert_eq!("STOCK".parse::<KernelPreset>(), Ok(KernelPreset::Stock));
        assert_eq!("DOCKER".parse::<KernelPreset>(), Ok(KernelPreset::Docker));
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn parse_preset_case_insensitive() {
        // STOCK is rejected on macOS; DOCKER is always valid.
        assert!("STOCK".parse::<KernelPreset>().is_err());
        assert_eq!("DOCKER".parse::<KernelPreset>(), Ok(KernelPreset::Docker));
    }

    #[test]
    fn parse_preset_unknown() {
        assert!("custom".parse::<KernelPreset>().is_err());
        assert!("/path/to/vmlinux".parse::<KernelPreset>().is_err());
        assert!("containerd".parse::<KernelPreset>().is_err());
    }

    /// On macOS, --kernel stock is rejected at parse time with a clear message.
    #[cfg(target_os = "macos")]
    #[test]
    fn stock_preset_rejected_on_macos() {
        let err = "stock".parse::<KernelPreset>().unwrap_err();
        assert!(
            err.contains("not supported on macOS"),
            "unexpected error: {err}"
        );
        assert!(err.contains("ember kernel build"), "hint missing: {err}");
    }

    /// On macOS, parsing "stock" as a KernelSpec must fail (not silently become a path).
    #[cfg(target_os = "macos")]
    #[test]
    fn spec_stock_rejected_on_macos() {
        let result = "stock".parse::<KernelSpec>();
        assert!(result.is_err(), "expected Err but got Ok");
        let err = result.unwrap_err();
        assert!(
            err.contains("not supported on macOS"),
            "unexpected error: {err}"
        );
    }

    #[cfg(not(target_os = "macos"))]
    #[test]
    fn spec_from_preset_name() {
        assert_eq!(
            "stock".parse::<KernelSpec>().unwrap(),
            KernelSpec::Preset(KernelPreset::Stock)
        );
        assert_eq!(
            "docker".parse::<KernelSpec>().unwrap(),
            KernelSpec::Preset(KernelPreset::Docker)
        );
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn spec_from_preset_name() {
        // On macOS only "docker" is valid; "stock" is rejected.
        assert!("stock".parse::<KernelSpec>().is_err());
        assert_eq!(
            "docker".parse::<KernelSpec>().unwrap(),
            KernelSpec::Preset(KernelPreset::Docker)
        );
    }

    #[test]
    fn spec_from_path() {
        assert_eq!(
            "/path/to/vmlinux".parse::<KernelSpec>().unwrap(),
            KernelSpec::Path(PathBuf::from("/path/to/vmlinux"))
        );
        assert_eq!(
            "~/kernels/vmlinux".parse::<KernelSpec>().unwrap(),
            KernelSpec::Path(PathBuf::from("~/kernels/vmlinux"))
        );
    }

    #[test]
    fn containerd_is_treated_as_path() {
        // "containerd" is no longer a preset — it should parse as a path.
        assert_eq!(
            "containerd".parse::<KernelSpec>().unwrap(),
            KernelSpec::Path(PathBuf::from("containerd"))
        );
    }

    #[test]
    fn preset_urls_contain_arch() {
        let arch = std::env::consts::ARCH;
        let expected_arch = if arch == "aarch64" {
            "aarch64"
        } else {
            "x86_64"
        };
        assert!(KernelPreset::Stock.url().unwrap().contains(expected_arch));
    }

    #[test]
    fn docker_preset_has_no_url() {
        assert!(KernelPreset::Docker.url().is_none());
    }

    #[test]
    fn default_preset_is_docker() {
        assert_eq!(DEFAULT_PRESET, KernelPreset::Docker);
    }

    #[test]
    fn display_round_trip() {
        assert_eq!(KernelPreset::Stock.to_string(), "stock");
        assert_eq!(KernelPreset::Docker.to_string(), "docker");
    }

    #[cfg(not(target_os = "macos"))]
    #[test]
    fn serde_deserialize_preset() {
        let spec: KernelSpec = serde_json::from_str(r#""stock""#).unwrap();
        assert_eq!(spec, KernelSpec::Preset(KernelPreset::Stock));

        let spec: KernelSpec = serde_json::from_str(r#""docker""#).unwrap();
        assert_eq!(spec, KernelSpec::Preset(KernelPreset::Docker));
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn serde_deserialize_preset() {
        // On macOS, "stock" in YAML/JSON config must fail to deserialize.
        assert!(serde_json::from_str::<KernelSpec>(r#""stock""#).is_err());

        let spec: KernelSpec = serde_json::from_str(r#""docker""#).unwrap();
        assert_eq!(spec, KernelSpec::Preset(KernelPreset::Docker));
    }

    #[test]
    fn serde_deserialize_path() {
        let spec: KernelSpec = serde_json::from_str(r#""/path/to/vmlinux""#).unwrap();
        assert_eq!(spec, KernelSpec::Path(PathBuf::from("/path/to/vmlinux")));
    }

    // -----------------------------------------------------------------
    // Same-file guard and image validation (P1: `ember init --kernel`
    // could truncate the very kernel it was pointed at).
    // -----------------------------------------------------------------

    /// A minimal but plausible ELF kernel image.
    fn fake_elf(len: usize) -> Vec<u8> {
        let mut data = vec![0x7f, b'E', b'L', b'F'];
        data.resize(len, 0xab);
        data
    }

    /// A minimal but plausible arm64 boot Image (magic at offset 56).
    fn fake_arm64_image(len: usize) -> Vec<u8> {
        let mut data = vec![0u8; len.max(KERNEL_HEADER_LEN)];
        data[ARM64_IMAGE_MAGIC_OFFSET..KERNEL_HEADER_LEN].copy_from_slice(&ARM64_IMAGE_MAGIC);
        data
    }

    /// Build a store whose root is a symlink to a real directory, mirroring
    /// the host where `/var/lib/ember` → `~/.thermite/ember`.
    fn symlinked_store(tmp: &Path) -> (StateStore, PathBuf) {
        let real = tmp.join("real");
        std::fs::create_dir_all(&real).unwrap();
        let link = tmp.join("link");
        std::os::unix::fs::symlink(&real, &link).unwrap();
        let store = StateStore::new(link);
        store.init().unwrap();
        (store, real)
    }

    /// The incident: `--kernel <real path>` naming the same inode as the
    /// destination reached through a symlinked state dir. Must be a no-op
    /// success, and the kernel must survive intact.
    #[test]
    fn resolve_same_file_via_symlinked_state_dir_is_noop() {
        let tmp = tempfile::tempdir().unwrap();
        let (store, real) = symlinked_store(tmp.path());

        let kernel = real.join("kernels").join("vmlinux-6.1.102");
        std::fs::write(&kernel, fake_elf(4096)).unwrap();

        // Paths differ as strings; only canonicalization reveals the truth.
        let dest = store.kernel_dir().join("vmlinux-6.1.102");
        assert_ne!(kernel, dest);

        let resolved = KernelSpec::Path(kernel.clone()).resolve(&store).unwrap();

        assert_eq!(
            std::fs::metadata(&kernel).unwrap().len(),
            4096,
            "kernel must not be truncated"
        );
        assert!(is_same_file(&resolved, &kernel));
    }

    /// The same-file guard also covers the plain case of passing the exact
    /// destination path back to `--kernel`.
    #[test]
    fn resolve_identical_path_is_noop() {
        let tmp = tempfile::tempdir().unwrap();
        let store = StateStore::new(tmp.path().to_path_buf());
        store.init().unwrap();

        let dest = store.kernel_dir().join("vmlinux-6.1.102");
        std::fs::write(&dest, fake_elf(2048)).unwrap();

        let resolved = KernelSpec::Path(dest.clone()).resolve(&store).unwrap();

        assert_eq!(resolved, dest);
        assert_eq!(std::fs::metadata(&dest).unwrap().len(), 2048);
    }

    /// A genuinely different source is still copied into the store.
    #[test]
    fn resolve_different_file_copies() {
        let tmp = tempfile::tempdir().unwrap();
        let store = StateStore::new(tmp.path().join("state"));
        store.init().unwrap();

        let src_dir = tmp.path().join("elsewhere");
        std::fs::create_dir_all(&src_dir).unwrap();
        let src = src_dir.join("vmlinux-custom");
        std::fs::write(&src, fake_elf(8192)).unwrap();

        let resolved = KernelSpec::Path(src.clone()).resolve(&store).unwrap();

        assert_eq!(resolved, store.kernel_dir().join("vmlinux-custom"));
        assert_eq!(std::fs::metadata(&resolved).unwrap().len(), 8192);
        assert_eq!(std::fs::metadata(&src).unwrap().len(), 8192);
        assert!(!is_same_file(&src, &resolved));
    }

    /// An already-in-place kernel that is 0 bytes (e.g. destroyed by an
    /// earlier run of the buggy code) must fail here, not at `pool start`.
    #[test]
    fn resolve_same_file_empty_fails_loudly() {
        let tmp = tempfile::tempdir().unwrap();
        let (store, real) = symlinked_store(tmp.path());

        let kernel = real.join("kernels").join("vmlinux-6.1.102");
        std::fs::write(&kernel, b"").unwrap();

        let err = KernelSpec::Path(kernel)
            .resolve(&store)
            .unwrap_err()
            .to_string();
        assert!(err.contains("0 bytes"), "unexpected error: {err}");
        assert!(err.contains("ember kernel build"), "hint missing: {err}");
    }

    /// A non-ELF source is rejected before it can overwrite anything.
    #[test]
    fn resolve_non_elf_source_fails_and_preserves_destination() {
        let tmp = tempfile::tempdir().unwrap();
        let store = StateStore::new(tmp.path().join("state"));
        store.init().unwrap();

        // A good kernel already installed under the same filename.
        let dest = store.kernel_dir().join("vmlinux-custom");
        std::fs::write(&dest, fake_elf(4096)).unwrap();

        let src_dir = tmp.path().join("elsewhere");
        std::fs::create_dir_all(&src_dir).unwrap();
        let src = src_dir.join("vmlinux-custom");
        std::fs::write(&src, b"not a kernel at all").unwrap();

        let err = KernelSpec::Path(src)
            .resolve(&store)
            .unwrap_err()
            .to_string();
        assert!(
            err.contains("not a bootable kernel image"),
            "unexpected error: {err}"
        );
        assert_eq!(
            std::fs::metadata(&dest).unwrap().len(),
            4096,
            "a bad source must not clobber the installed kernel"
        );
    }

    /// An empty source file is rejected too.
    #[test]
    fn resolve_empty_source_fails() {
        let tmp = tempfile::tempdir().unwrap();
        let store = StateStore::new(tmp.path().join("state"));
        store.init().unwrap();

        let src = tmp.path().join("vmlinux-empty");
        std::fs::write(&src, b"").unwrap();

        let err = KernelSpec::Path(src)
            .resolve(&store)
            .unwrap_err()
            .to_string();
        assert!(err.contains("0 bytes"), "unexpected error: {err}");
    }

    /// A cached preset kernel that has been truncated must not be reported
    /// as usable (the download path landing in the same state as the copy).
    #[test]
    fn resolve_preset_rejects_truncated_cached_kernel() {
        let tmp = tempfile::tempdir().unwrap();
        let store = StateStore::new(tmp.path().to_path_buf());
        store.init().unwrap();

        let dest = store.kernel_dir().join(KernelPreset::Docker.filename());
        std::fs::write(&dest, b"").unwrap();

        let err = KernelSpec::Preset(KernelPreset::Docker)
            .resolve(&store)
            .unwrap_err()
            .to_string();
        assert!(err.contains("0 bytes"), "unexpected error: {err}");
    }

    /// A cached preset kernel that is intact resolves without complaint.
    #[test]
    fn resolve_preset_accepts_valid_cached_kernel() {
        let tmp = tempfile::tempdir().unwrap();
        let store = StateStore::new(tmp.path().to_path_buf());
        store.init().unwrap();

        let dest = store.kernel_dir().join(KernelPreset::Docker.filename());
        std::fs::write(&dest, fake_elf(4096)).unwrap();

        assert_eq!(
            KernelSpec::Preset(KernelPreset::Docker)
                .resolve(&store)
                .unwrap(),
            dest
        );
    }

    #[test]
    fn validate_accepts_elf_and_arm64_image() {
        let tmp = tempfile::tempdir().unwrap();

        let elf = tmp.path().join("vmlinux");
        std::fs::write(&elf, fake_elf(1024)).unwrap();
        assert!(validate_kernel_image(&elf).is_ok());

        let image = tmp.path().join("Image");
        std::fs::write(&image, fake_arm64_image(1024)).unwrap();
        assert!(validate_kernel_image(&image).is_ok());
    }

    #[test]
    fn validate_rejects_tiny_non_kernel_files() {
        let tmp = tempfile::tempdir().unwrap();

        // Shorter than the arm64 header, so the offset check must not panic.
        let tiny = tmp.path().join("tiny");
        std::fs::write(&tiny, b"hi").unwrap();
        let err = validate_kernel_image(&tiny).unwrap_err().to_string();
        assert!(
            err.contains("not a bootable kernel image"),
            "unexpected error: {err}"
        );

        let missing = tmp.path().join("nope");
        assert!(validate_kernel_image(&missing).is_err());
    }

    #[test]
    fn is_same_file_handles_symlinks_and_distinct_files() {
        let tmp = tempfile::tempdir().unwrap();
        let a = tmp.path().join("a");
        std::fs::write(&a, b"x").unwrap();
        let b = tmp.path().join("b");
        std::fs::write(&b, b"x").unwrap();

        let link = tmp.path().join("link-to-a");
        std::os::unix::fs::symlink(&a, &link).unwrap();
        let hard = tmp.path().join("hard-to-a");
        std::fs::hard_link(&a, &hard).unwrap();

        assert!(is_same_file(&a, &a));
        assert!(is_same_file(&a, &link));
        assert!(is_same_file(&a, &hard));
        assert!(!is_same_file(&a, &b));
        assert!(!is_same_file(&a, &tmp.path().join("missing")));
        assert!(!is_same_file(
            &tmp.path().join("missing"),
            &tmp.path().join("missing")
        ));
    }
}

//! Container-based kernel build for ember microVMs.
//!
//! Builds a custom Linux kernel with Docker networking and AVF (Apple
//! Virtualization Framework) support inside a container. All build assets
//! (Dockerfile, config fragments, URLs) are embedded in the binary — no
//! runtime dependency on the `kernel/` directory.

use std::path::PathBuf;
use std::process::Command;

use anyhow::{bail, Context};

use super::KernelPreset;
use crate::state::store::StateStore;

// ---------------------------------------------------------------------------
// Embedded build constants (from kernel/ directory)
// ---------------------------------------------------------------------------

const KERNEL_TAG: &str = "microvm-kernel-6.1.163-20.299.amzn2023";
const KERNEL_REPO: &str = "https://github.com/amazonlinux/linux.git";
const BASE_CONFIG_URL: &str = "https://raw.githubusercontent.com/firecracker-microvm/firecracker/main/resources/guest_configs/microvm-kernel-ci-x86_64-6.1.config";
const BUILDER_IMAGE: &str = "ember-kernel-builder";

const DOCKERFILE: &str = include_str!("../../../../kernel/Dockerfile");
const DOCKER_FRAGMENT: &str = include_str!("../../../../kernel/docker.fragment");
const AVF_FRAGMENT: &str = include_str!("../../../../kernel/avf.fragment");

// ---------------------------------------------------------------------------
// Container tool detection
// ---------------------------------------------------------------------------

/// Detect whether `docker` or `podman` is available.
pub fn detect_container_tool() -> anyhow::Result<String> {
    for tool in &["docker", "podman"] {
        let ok = Command::new("which")
            .arg(tool)
            .output()
            .map(|o| o.status.success())
            .unwrap_or(false);
        if ok {
            return Ok((*tool).to_string());
        }
    }
    bail!(
        "neither 'docker' nor 'podman' is installed.\n\
         Install one to build kernels."
    );
}

// ---------------------------------------------------------------------------
// Build orchestration
// ---------------------------------------------------------------------------

/// Build a kernel with Docker networking and AVF support inside a container.
///
/// 1. Writes build assets (Dockerfile, docker.fragment, avf.fragment) into `work_dir`
/// 2. Builds the builder container image
/// 3. Runs the full kernel build inside the container
/// 4. Copies the resulting vmlinux to the state store's kernels/ directory
///
/// Returns the path to the installed kernel.
pub fn build(store: &StateStore, jobs: usize, tool: &str) -> anyhow::Result<PathBuf> {
    let work_dir = tempfile::tempdir().context("failed to create temp build directory")?;
    let work = work_dir.path();

    println!("Build directory: {}", work.display());

    // Write embedded assets into the work directory.
    std::fs::write(work.join("Dockerfile"), DOCKERFILE).context("failed to write Dockerfile")?;
    std::fs::write(work.join("docker.fragment"), DOCKER_FRAGMENT)
        .context("failed to write docker.fragment")?;
    std::fs::write(work.join("avf.fragment"), AVF_FRAGMENT)
        .context("failed to write avf.fragment")?;

    // Build the builder image.
    println!("Building container image ({BUILDER_IMAGE})...");
    let output = Command::new(tool)
        .env("DOCKER_BUILDKIT", "1")
        .args(["build", "-t", BUILDER_IMAGE, "."])
        .current_dir(work)
        .output()
        .with_context(|| format!("failed to execute '{tool} build'"))?;
    if !output.status.success() {
        bail!(
            "{tool} build failed:\n{}",
            String::from_utf8_lossy(&output.stderr)
        );
    }

    // Run the kernel build inside the container.
    //
    // The build script mirrors the Makefile targets:
    //   1. Download Firecracker CI base config
    //   2. Shallow-clone the Amazon Linux kernel source
    //   3. Merge base config + docker.fragment + avf.fragment
    //   4. Strip BUILD_SALT for reproducibility
    //   5. Compile vmlinux
    #[cfg(not(target_os = "macos"))]
    let user_flag = {
        let uid = nix::unistd::getuid();
        let gid = nix::unistd::getgid();
        format!("{}:{}", uid, gid)
    };

    let build_script = format!(
        "set -e\n\
         echo '==> Downloading Firecracker CI kernel config...'\n\
         curl -fSL -o base.config '{BASE_CONFIG_URL}'\n\
         echo '==> Cloning kernel source (shallow, tag {KERNEL_TAG})...'\n\
         git clone --depth 1 --branch '{KERNEL_TAG}' '{KERNEL_REPO}' linux\n\
         echo '==> Merging base config + fragments...'\n\
         cd linux\n\
         KCONFIG_CONFIG=.config scripts/kconfig/merge_config.sh -m ../base.config ../docker.fragment ../avf.fragment\n\
         sed -i 's/^CONFIG_BUILD_SALT=.*/CONFIG_BUILD_SALT=\"\"/' .config\n\
         make olddefconfig\n\
         echo '==> Building vmlinux ({jobs} jobs)...'\n\
         make -j{jobs} vmlinux\n\
         echo '==> Done.'"
    );

    println!("Starting kernel build (this may take 10-30 minutes)...");

    let volume = format!("{}:/build", work.display());
    #[cfg(not(target_os = "macos"))]
    let run_args = docker_run_args(&volume, &user_flag, &build_script);
    #[cfg(target_os = "macos")]
    let run_args = docker_run_args(&volume, &build_script);

    let status = Command::new(tool)
        .args(&run_args)
        .status()
        .with_context(|| format!("failed to execute '{tool} run'"))?;
    if !status.success() {
        bail!(
            "kernel build failed (exit code {})",
            status.code().unwrap_or(-1)
        );
    }

    // Copy the built kernel to the state store.
    let built = work.join("linux/vmlinux");
    if !built.exists() {
        bail!(
            "build completed but vmlinux not found at {}",
            built.display()
        );
    }

    let kernel_dir = store.kernel_dir();
    std::fs::create_dir_all(&kernel_dir)
        .with_context(|| format!("failed to create {}", kernel_dir.display()))?;

    let dest = kernel_dir.join(KernelPreset::Docker.filename());
    std::fs::copy(&built, &dest).with_context(|| {
        format!(
            "failed to copy vmlinux from {} to {}",
            built.display(),
            dest.display()
        )
    })?;

    println!("Kernel installed to {}", dest.display());
    Ok(dest)
}

// ---------------------------------------------------------------------------
// Internal helpers
// ---------------------------------------------------------------------------

/// Build the `docker run` argument list.
///
/// On Linux, `--user uid:gid` is included so build artifacts are owned by the
/// calling user. On macOS the flag is omitted because Colima / Docker Desktop
/// manage file ownership through their file-sharing layer and an explicit
/// `--user` causes permission errors (SEC-475 #1).
#[cfg(not(target_os = "macos"))]
fn docker_run_args<'a>(volume: &'a str, user_flag: &'a str, build_script: &'a str) -> Vec<&'a str> {
    vec![
        "run",
        "--rm",
        "--user",
        user_flag,
        "-v",
        volume,
        "-w",
        "/build",
        BUILDER_IMAGE,
        "sh",
        "-c",
        build_script,
    ]
}

#[cfg(target_os = "macos")]
fn docker_run_args<'a>(volume: &'a str, build_script: &'a str) -> Vec<&'a str> {
    vec![
        "run",
        "--rm",
        "-v",
        volume,
        "-w",
        "/build",
        BUILDER_IMAGE,
        "sh",
        "-c",
        build_script,
    ]
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    #[cfg(not(target_os = "macos"))]
    #[test]
    fn docker_run_args_linux_includes_user_flag() {
        let args = docker_run_args("/work:/build", "1000:1000", "echo hi");
        let pos = args.iter().position(|&a| a == "--user");
        assert!(pos.is_some(), "--user must be present on Linux");
        let next = args[pos.unwrap() + 1];
        assert_eq!(next, "1000:1000");
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn docker_run_args_macos_omits_user_flag() {
        let args = docker_run_args("/work:/build", "echo hi");
        assert!(
            !args.contains(&"--user"),
            "--user must NOT be present on macOS"
        );
    }
}

//! Integration tests for `ember image pull`, `image list`, and `image delete`.
//!
//! Cross-platform tests use `TestEnv` to abstract platform setup.
//! Platform-specific storage checks (ZFS zvol on Linux, .img file on macOS)
//! are gated with `#[cfg(target_os)]`.
//!
//! To run:
//!   ./run-integration-tests.sh image

#[allow(dead_code)]
mod common;

// ---------------------------------------------------------------------------
// Cross-platform tests
// ---------------------------------------------------------------------------

/// `ember image pull` downloads an image and reports success.
#[test]
#[ignore]
fn pull_creates_image() {
    let env = common::TestEnv::init("imgpull");

    let output = common::ember(&["--state-dir", env.state(), "image", "pull", "alpine:latest"]);
    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        output.status.success(),
        "image pull failed.\nstdout: {stdout}\nstderr: {stderr}"
    );
    assert!(
        stdout.contains("pulled successfully"),
        "expected success message in stdout: {stdout}"
    );

    // Platform-specific storage verification.
    #[cfg(target_os = "linux")]
    {
        let zvol = format!("{}/ember/images/library-alpine-latest", env.pool);
        common::linux::assert_dataset_exists(&zvol);
        common::linux::assert_snapshot_exists(&format!("{zvol}@base"));
    }

    #[cfg(target_os = "macos")]
    {
        let img_path = env.state_dir.join("images/data/library-alpine-latest.img");
        assert!(
            img_path.exists(),
            "expected image file at {}",
            img_path.display()
        );
        let metadata = std::fs::metadata(&img_path).unwrap();
        assert!(
            metadata.len() >= 10 * 1024 * 1024,
            "image file too small: {} bytes",
            metadata.len()
        );
    }
}

/// `ember image list` shows pulled images in table and JSON formats.
#[test]
#[ignore]
fn list_shows_pulled_image() {
    let env = common::TestEnv::with_image("imglist");

    // Table output.
    let list = common::ember(&["--state-dir", env.state(), "image", "list"]);
    let stdout = String::from_utf8_lossy(&list.stdout);
    assert!(
        list.status.success(),
        "image list failed: {}",
        String::from_utf8_lossy(&list.stderr)
    );
    assert!(
        stdout.contains("library-alpine-latest"),
        "expected local name in table output: {stdout}"
    );
    assert!(
        stdout.contains("docker.io/library/alpine:latest"),
        "expected full reference in table output: {stdout}"
    );

    // JSON output.
    let json_list = common::ember(&[
        "--state-dir",
        env.state(),
        "image",
        "list",
        "--format",
        "json",
    ]);
    let json_stdout = String::from_utf8_lossy(&json_list.stdout);
    assert!(json_list.status.success());

    let parsed: serde_json::Value = serde_json::from_str(&json_stdout)
        .unwrap_or_else(|e| panic!("invalid JSON: {e}\noutput: {json_stdout}"));
    let images = parsed["images"]
        .as_array()
        .expect("expected 'images' array");
    assert_eq!(images.len(), 1, "expected one image");
    assert_eq!(images[0]["local_name"], "library-alpine-latest");
    assert_eq!(images[0]["reference"], "docker.io/library/alpine:latest");
}

/// `ember image delete` removes the image from registry and storage.
#[test]
#[ignore]
fn delete_removes_image() {
    let env = common::TestEnv::with_image("imgdel");

    // Platform-specific: verify storage exists before delete.
    #[cfg(target_os = "linux")]
    {
        let zvol = format!("{}/ember/images/library-alpine-latest", env.pool);
        common::linux::assert_dataset_exists(&zvol);
    }

    #[cfg(target_os = "macos")]
    {
        let img_path = env.state_dir.join("images/data/library-alpine-latest.img");
        assert!(img_path.exists());
    }

    // Delete.
    let del = common::ember(&[
        "--state-dir",
        env.state(),
        "image",
        "delete",
        "alpine:latest",
    ]);
    let stdout = String::from_utf8_lossy(&del.stdout);
    let stderr = String::from_utf8_lossy(&del.stderr);
    assert!(
        del.status.success(),
        "image delete failed.\nstdout: {stdout}\nstderr: {stderr}"
    );

    // Platform-specific: verify storage is gone.
    #[cfg(target_os = "linux")]
    {
        let zvol = format!("{}/ember/images/library-alpine-latest", env.pool);
        common::linux::assert_dataset_absent(&zvol);
    }

    #[cfg(target_os = "macos")]
    {
        let img_path = env.state_dir.join("images/data/library-alpine-latest.img");
        assert!(
            !img_path.exists(),
            "image file should have been deleted: {}",
            img_path.display()
        );
    }

    // Image list should be empty.
    let list = common::ember(&["--state-dir", env.state(), "image", "list"]);
    let list_stdout = String::from_utf8_lossy(&list.stdout);
    assert!(
        list_stdout.contains("No images found"),
        "expected empty list, got: {list_stdout}"
    );
}

/// `ember image delete` succeeds when the backing dataset is already
/// gone, and still drops the registry entry.
///
/// This is the state an `ember init` onto a different pool leaves
/// behind: `registry.json` describes datasets that no longer exist
/// under the configured pool. Before the fix, both supported routes out
/// (`image delete` and `image build --force`) failed on `zfs destroy`
/// and the entry could only be removed by hand-editing the registry.
#[cfg(target_os = "linux")]
#[test]
#[ignore]
fn delete_tolerates_missing_dataset() {
    let env = common::TestEnv::with_image("imgdelmissing");
    let zvol = format!("{}/ember/images/library-alpine-latest", env.pool);
    common::linux::assert_dataset_exists(&zvol);

    // Destroy the dataset behind ember's back, simulating a registry
    // that outlived its pool.
    let destroyed = std::process::Command::new("zfs")
        .args(["destroy", "-r", &zvol])
        .output()
        .expect("failed to run zfs destroy");
    assert!(
        destroyed.status.success(),
        "setup: zfs destroy failed: {}",
        String::from_utf8_lossy(&destroyed.stderr)
    );
    common::linux::assert_dataset_absent(&zvol);

    let del = common::ember(&[
        "--state-dir",
        env.state(),
        "image",
        "delete",
        "alpine:latest",
    ]);
    let stdout = String::from_utf8_lossy(&del.stdout);
    let stderr = String::from_utf8_lossy(&del.stderr);
    assert!(
        del.status.success(),
        "delete of an image with a missing dataset should succeed.\nstdout: {stdout}\nstderr: {stderr}"
    );

    let list = common::ember(&["--state-dir", env.state(), "image", "list"]);
    let list_stdout = String::from_utf8_lossy(&list.stdout);
    assert!(
        list_stdout.contains("No images found"),
        "registry entry should be gone, got: {list_stdout}"
    );
}

// ---------------------------------------------------------------------------
// image build --force tests
// ---------------------------------------------------------------------------

/// `ember image build` without `--force` fails with a non-zero exit when the
/// image already exists locally.  The error message should mention `--force`
/// and `ember image delete` as remedies.
#[test]
#[ignore]
fn build_exists_without_force_is_error() {
    common::require_docker();

    #[cfg(target_os = "linux")]
    {
        let tmp = tempfile::tempdir().unwrap();
        let (_, state_dir, _cleanup) =
            common::linux::setup_pool_init_and_build_ubuntu("imgbld_noforce", &tmp);
        let state = state_dir.to_str().unwrap();

        let dockerfile = format!(
            "{}/images/Dockerfile.ubuntu-slim",
            env!("CARGO_MANIFEST_DIR")
        );

        // Second build without --force should fail.
        let out = common::ember(&[
            "--state-dir",
            state,
            "image",
            "build",
            "ubuntu-slim",
            "-f",
            &dockerfile,
        ]);
        let stderr = String::from_utf8_lossy(&out.stderr);
        assert!(
            !out.status.success(),
            "expected non-zero exit when image exists without --force; stderr: {stderr}"
        );
        assert!(
            stderr.contains("already exists") || stderr.contains("--force"),
            "expected error message mentioning 'already exists' or '--force': {stderr}"
        );
    }
}

/// `ember image build --force` succeeds when the image already exists locally,
/// deleting the old image and rebuilding it.
#[test]
#[ignore]
fn build_exists_with_force_rebuilds() {
    common::require_docker();

    #[cfg(target_os = "linux")]
    {
        let tmp = tempfile::tempdir().unwrap();
        let (_, state_dir, _cleanup) =
            common::linux::setup_pool_init_and_build_ubuntu("imgbld_force", &tmp);
        let state = state_dir.to_str().unwrap();

        let dockerfile = format!(
            "{}/images/Dockerfile.ubuntu-slim",
            env!("CARGO_MANIFEST_DIR")
        );

        // Second build WITH --force should succeed.
        let out = common::ember(&[
            "--state-dir",
            state,
            "image",
            "build",
            "ubuntu-slim",
            "-f",
            &dockerfile,
            "--force",
        ]);
        let stdout = String::from_utf8_lossy(&out.stdout);
        let stderr = String::from_utf8_lossy(&out.stderr);
        assert!(
            out.status.success(),
            "image build --force failed.\nstdout: {stdout}\nstderr: {stderr}"
        );
        assert!(
            stdout.contains("built successfully"),
            "expected 'built successfully' in stdout: {stdout}"
        );
    }
}

/// `ember image build --force` fails with a clear error listing the dependent
/// VMs when a VM is using the image.  No VM should be deleted.
#[test]
#[ignore]
fn build_exists_with_force_vm_locked_is_error() {
    common::require_docker();

    #[cfg(target_os = "linux")]
    {
        common::linux::require_firecracker();
        let tmp = tempfile::tempdir().unwrap();
        let (_, state_dir, _cleanup) =
            common::linux::setup_pool_init_and_build_ubuntu("imgbld_locked", &tmp);
        let state = state_dir.to_str().unwrap();

        // Create a VM that depends on the built image (--no-start so we don't
        // need a real kernel or KVM for this part of the test).
        let kernel = common::linux::create_dummy_kernel(tmp.path());
        let create_out = common::ember(&[
            "--state-dir",
            state,
            "vm",
            "create",
            "locked-vm",
            "--image",
            "ubuntu-slim",
            "--kernel",
            kernel.to_str().unwrap(),
            "--no-start",
        ]);
        let stdout = String::from_utf8_lossy(&create_out.stdout);
        let stderr = String::from_utf8_lossy(&create_out.stderr);
        assert!(
            create_out.status.success(),
            "vm create failed.\nstdout: {stdout}\nstderr: {stderr}"
        );

        let dockerfile = format!(
            "{}/images/Dockerfile.ubuntu-slim",
            env!("CARGO_MANIFEST_DIR")
        );

        // Build with --force should fail because a VM depends on the image.
        let build_out = common::ember(&[
            "--state-dir",
            state,
            "image",
            "build",
            "ubuntu-slim",
            "-f",
            &dockerfile,
            "--force",
        ]);
        let stderr = String::from_utf8_lossy(&build_out.stderr);
        assert!(
            !build_out.status.success(),
            "expected non-zero exit when VMs depend on the image; stderr: {stderr}"
        );
        assert!(
            stderr.contains("in use by VM") || stderr.contains("locked-vm"),
            "expected error mentioning dependent VM 'locked-vm': {stderr}"
        );

        // The VM should still exist (no VM destruction).
        let list_out = common::ember(&["--state-dir", state, "vm", "list"]);
        let list_stdout = String::from_utf8_lossy(&list_out.stdout);
        assert!(
            list_stdout.contains("locked-vm"),
            "VM should not have been deleted: {list_stdout}"
        );

        // Cleanup.
        common::linux::stop_and_delete_vm(&state_dir, "locked-vm");
    }
}

/// `ember image build` when the image does not yet exist builds normally.
///
/// This is the baseline case (no behavior change from before).  It is
/// tested implicitly by every `setup_pool_init_and_build_ubuntu` call;
/// this explicit test makes the AC coverage obvious.
#[test]
#[ignore]
fn build_not_exists_builds_normally() {
    common::require_docker();

    #[cfg(target_os = "linux")]
    {
        let tmp = tempfile::tempdir().unwrap();
        let (_, state_dir, _cleanup) =
            common::linux::setup_pool_init_and_build_ubuntu("imgbld_new", &tmp);
        let state = state_dir.to_str().unwrap();

        // Image should be present in the registry after a successful build.
        let list_out = common::ember(&["--state-dir", state, "image", "list"]);
        let list_stdout = String::from_utf8_lossy(&list_out.stdout);
        assert!(
            list_out.status.success(),
            "image list failed: {}",
            String::from_utf8_lossy(&list_out.stderr)
        );
        assert!(
            list_stdout.contains("ubuntu-slim"),
            "expected 'ubuntu-slim' in image list: {list_stdout}"
        );
    }
}

/// Pulling the same image twice is idempotent.
#[test]
#[ignore]
fn pull_same_image_twice_is_idempotent() {
    let env = common::TestEnv::with_image("imgidempotent");

    // Second pull of the same image.
    let pull2 = common::ember(&["--state-dir", env.state(), "image", "pull", "alpine:latest"]);
    let stdout2 = String::from_utf8_lossy(&pull2.stdout);
    assert!(
        pull2.status.success(),
        "second pull failed: {}",
        String::from_utf8_lossy(&pull2.stderr)
    );
    assert!(
        stdout2.contains("already exists"),
        "expected 'already exists' on re-pull: {stdout2}"
    );
}

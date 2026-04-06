//! Integration tests for pool commands.
//!
//! These tests require Docker (for image pull) and are `#[ignore]` by default.
//! Run them explicitly with: cargo test --test pool -- --ignored

#[allow(dead_code)]
mod common;

use common::ember;

/// Helper: destroy a pool (best-effort cleanup).
fn destroy_pool(state_dir: &str, pool_name: &str) {
    let _ = ember(&["--state-dir", state_dir, "pool", "destroy", pool_name]);
}

/// Test environment with a pulled image and a dummy kernel file.
///
/// Pool create requires a kernel path. Since we use `--no-start`, a dummy
/// file is sufficient (same approach as `TestEnv::with_vm`).
struct PoolTestEnv {
    env: common::TestEnv,
    kernel: tempfile::NamedTempFile,
}

impl PoolTestEnv {
    fn new(test_name: &str) -> Self {
        let env = common::TestEnv::with_image(test_name);
        let mut kernel = tempfile::NamedTempFile::new_in(env.state_dir.parent().unwrap()).unwrap();
        std::io::Write::write_all(&mut kernel, b"not a real kernel").unwrap();
        Self { env, kernel }
    }

    fn state(&self) -> &str {
        self.env.state()
    }

    fn kernel(&self) -> &str {
        self.kernel.path().to_str().unwrap()
    }

    /// Run `ember pool create` with the dummy kernel and --no-start.
    fn pool_create(&self, pool_name: &str, count: u32, extra_args: &[&str]) -> std::process::Output {
        let count_str = count.to_string();
        let mut args = vec![
            "--state-dir",
            self.state(),
            "pool",
            "create",
            pool_name,
            "--count",
            &count_str,
            "--image",
            "alpine:latest",
            "--kernel",
            self.kernel(),
            "--no-start",
        ];
        args.extend_from_slice(extra_args);
        ember(&args)
    }
}

// ── Tests that don't require Docker ────────────────────────────────

#[test]
fn pool_help_returns_zero() {
    let output = ember(&["pool", "--help"]);
    assert!(output.status.success());
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(stdout.contains("create"));
    assert!(stdout.contains("list"));
    assert!(stdout.contains("status"));
    assert!(stdout.contains("destroy"));
}

#[test]
fn pool_create_help_shows_options() {
    let output = ember(&["pool", "create", "--help"]);
    assert!(output.status.success());
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(stdout.contains("--count"));
    assert!(stdout.contains("--image"));
    assert!(stdout.contains("--format"));
}

#[test]
fn pool_list_empty() {
    let env = common::TestEnv::init("pool_list_empty");
    let output = ember(&["--state-dir", env.state(), "pool", "list"]);
    assert!(output.status.success());
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(stdout.contains("No pools found"));
}

#[test]
fn pool_list_json_empty() {
    let env = common::TestEnv::init("pool_list_json_empty");
    let output = ember(&["--state-dir", env.state(), "pool", "list", "--format", "json"]);
    assert!(output.status.success());
    let stdout = String::from_utf8_lossy(&output.stdout);
    let parsed: serde_json::Value = serde_json::from_str(&stdout).unwrap();
    assert!(parsed.as_array().unwrap().is_empty());
}

#[test]
fn pool_status_nonexistent_fails() {
    let env = common::TestEnv::init("pool_status_nonexistent");
    let output = ember(&[
        "--state-dir",
        env.state(),
        "pool",
        "status",
        "no-such-pool",
    ]);
    assert!(!output.status.success());
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("not found"),
        "expected 'not found' in stderr: {stderr}"
    );
}

#[test]
fn pool_destroy_nonexistent_fails() {
    let env = common::TestEnv::init("pool_destroy_nonexistent");
    let output = ember(&[
        "--state-dir",
        env.state(),
        "pool",
        "destroy",
        "no-such-pool",
    ]);
    assert!(!output.status.success());
}

#[test]
fn pool_create_zero_count_fails() {
    let env = common::TestEnv::init("pool_create_zero");
    let output = ember(&[
        "--state-dir",
        env.state(),
        "pool",
        "create",
        "test-pool",
        "--count",
        "0",
        "--image",
        "alpine:latest",
        "--no-start",
    ]);
    assert!(!output.status.success());
}

// ── Tests below require Docker for image pull ─────────────────────

#[test]
#[ignore]
fn pool_create_and_list() {
    let pte = PoolTestEnv::new("pool_create_and_list");

    let output = pte.pool_create("test-pool", 2, &[]);
    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        output.status.success(),
        "pool create failed.\nstdout: {stdout}\nstderr: {stderr}"
    );

    // List should show the pool.
    let output = ember(&["--state-dir", pte.state(), "pool", "list"]);
    assert!(output.status.success());
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(stdout.contains("test-pool"));

    // VMs should be visible via vm list.
    let output = ember(&[
        "--state-dir",
        pte.state(),
        "vm",
        "list",
        "--format",
        "json",
    ]);
    assert!(output.status.success());
    let stdout = String::from_utf8_lossy(&output.stdout);
    let vms: serde_json::Value = serde_json::from_str(&stdout).unwrap();
    let vm_names: Vec<&str> = vms
        .as_array()
        .unwrap()
        .iter()
        .map(|v| v["name"].as_str().unwrap())
        .collect();
    assert!(vm_names.contains(&"test-pool-1"));
    assert!(vm_names.contains(&"test-pool-2"));

    destroy_pool(pte.state(), "test-pool");
}

#[test]
#[ignore]
fn pool_create_json_output() {
    let pte = PoolTestEnv::new("pool_create_json");

    let output = pte.pool_create("json-pool", 2, &["--format", "json"]);
    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        output.status.success(),
        "pool create json failed.\nstdout: {stdout}\nstderr: {stderr}"
    );

    // stdout should be valid JSON.
    let parsed: serde_json::Value = serde_json::from_str(&stdout)
        .unwrap_or_else(|e| panic!("invalid JSON in stdout: {e}\nstdout: {stdout}"));
    assert_eq!(parsed["name"], "json-pool");
    assert_eq!(parsed["count"], 2);
    let vms = parsed["vms"].as_array().unwrap();
    assert_eq!(vms.len(), 2);
    assert_eq!(vms[0], "json-pool-1");
    assert_eq!(vms[1], "json-pool-2");

    destroy_pool(pte.state(), "json-pool");
}

#[test]
#[ignore]
fn pool_status_shows_vms() {
    let pte = PoolTestEnv::new("pool_status");

    let output = pte.pool_create("stat-pool", 2, &[]);
    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        output.status.success(),
        "pool create failed.\nstdout: {stdout}\nstderr: {stderr}"
    );

    // Table format.
    let output = ember(&[
        "--state-dir",
        pte.state(),
        "pool",
        "status",
        "stat-pool",
    ]);
    assert!(output.status.success());
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(stdout.contains("stat-pool-1"));
    assert!(stdout.contains("stat-pool-2"));

    // JSON format.
    let output = ember(&[
        "--state-dir",
        pte.state(),
        "pool",
        "status",
        "stat-pool",
        "--format",
        "json",
    ]);
    assert!(output.status.success());
    let stdout = String::from_utf8_lossy(&output.stdout);
    let parsed: serde_json::Value = serde_json::from_str(&stdout).unwrap();
    assert_eq!(parsed["name"], "stat-pool");
    assert_eq!(parsed["count"], 2);
    let vms = parsed["vms"].as_array().unwrap();
    assert_eq!(vms.len(), 2);
    assert_eq!(vms[0]["vm_name"], "stat-pool-1");
    assert_eq!(vms[0]["status"], "created"); // --no-start => created

    destroy_pool(pte.state(), "stat-pool");
}

#[test]
#[ignore]
fn pool_destroy_removes_pool_and_vms() {
    let pte = PoolTestEnv::new("pool_destroy");

    let output = pte.pool_create("doomed", 2, &[]);
    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        output.status.success(),
        "pool create failed.\nstdout: {stdout}\nstderr: {stderr}"
    );

    // Destroy.
    let output = ember(&[
        "--state-dir",
        pte.state(),
        "pool",
        "destroy",
        "doomed",
    ]);
    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        output.status.success(),
        "pool destroy failed.\nstdout: {stdout}\nstderr: {stderr}"
    );

    // Pool should be gone.
    let output = ember(&["--state-dir", pte.state(), "pool", "list", "--format", "json"]);
    assert!(output.status.success());
    let stdout = String::from_utf8_lossy(&output.stdout);
    let pools: serde_json::Value = serde_json::from_str(&stdout).unwrap();
    assert!(pools.as_array().unwrap().is_empty());

    // VMs should also be gone.
    let output = ember(&[
        "--state-dir",
        pte.state(),
        "vm",
        "list",
        "--format",
        "json",
    ]);
    assert!(output.status.success());
    let stdout = String::from_utf8_lossy(&output.stdout);
    let vms: serde_json::Value = serde_json::from_str(&stdout).unwrap();
    assert!(vms.as_array().unwrap().is_empty());
}

#[test]
#[ignore]
fn pool_create_duplicate_name_fails() {
    let pte = PoolTestEnv::new("pool_dup");

    let output = pte.pool_create("dup-pool", 1, &[]);
    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        output.status.success(),
        "pool create failed.\nstdout: {stdout}\nstderr: {stderr}"
    );

    // Second create with same name should fail.
    let output = pte.pool_create("dup-pool", 1, &[]);
    assert!(!output.status.success());
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(stderr.contains("already exists"));

    destroy_pool(pte.state(), "dup-pool");
}

#[test]
#[ignore]
fn pool_create_bad_image_fails() {
    let env = common::TestEnv::init("pool_bad_image");

    let output = ember(&[
        "--state-dir",
        env.state(),
        "pool",
        "create",
        "bad-pool",
        "--count",
        "1",
        "--image",
        "nonexistent:latest",
        "--no-start",
    ]);
    assert!(!output.status.success());
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(stderr.contains("not found"));
}

#[test]
#[ignore]
fn pool_list_json_after_create() {
    let pte = PoolTestEnv::new("pool_list_json");

    let output = pte.pool_create("listed", 1, &[]);
    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        output.status.success(),
        "pool create failed.\nstdout: {stdout}\nstderr: {stderr}"
    );

    let output = ember(&[
        "--state-dir",
        pte.state(),
        "pool",
        "list",
        "--format",
        "json",
    ]);
    assert!(output.status.success());
    let stdout = String::from_utf8_lossy(&output.stdout);
    let pools: Vec<serde_json::Value> = serde_json::from_str(&stdout).unwrap();
    assert_eq!(pools.len(), 1);
    assert_eq!(pools[0]["name"], "listed");
    assert_eq!(pools[0]["count"], 1);

    destroy_pool(pte.state(), "listed");
}

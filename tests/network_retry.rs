//! Integration test for the VM-start slot-poisoning retry (SEC-419/345).
//!
//! `EMBER_VZ_FAULT_INJECT=N` deterministically simulates N transient VZ boot
//! crashes (see `maybe_inject_start_fault` in the macOS backend). With N
//! faults, `ember vm create` must poison the first N /30 slots, route around
//! them, and land the VM on slot N — proving the start-retry loop recovers a
//! dispatch instead of failing it, and that the poisoned slots are released
//! (not leaked).
//!
//! Requirements (same as `macos_ember_vz`): macOS 13+ with AVF, the `ember-vz`
//! helper built, and a resolvable test kernel. Marked `#[ignore]` because it
//! boots a real VM; run via:
//!   ./run-integration-tests.sh network_retry
#![cfg(target_os = "macos")]

#[allow(dead_code)]
mod common;

use std::process::Command;

/// Run ember with `EMBER_VZ_FAULT_INJECT` set, returning the Output.
fn ember_with_faults(faults: u32, args: &[&str]) -> std::process::Output {
    Command::new(common::ember_bin())
        .args(args)
        .env("EMBER_VZ_FAULT_INJECT", faults.to_string())
        .output()
        .unwrap_or_else(|e| panic!("failed to execute ember: {e}"))
}

/// Two injected transient crashes → VM recovers on the third slot.
#[test]
#[ignore]
fn start_retries_around_two_poisoned_slots() {
    let env = common::TestEnv::with_image("netretry");
    let state = env.state().to_string();

    // Fresh state-dir: `faulttest` would normally take slot 0. With two
    // injected faults it must poison slots 0 and 1 and land on slot 2.
    let output = ember_with_faults(
        2,
        &[
            "--state-dir",
            &state,
            "vm",
            "create",
            "faulttest",
            "--image",
            "alpine:latest",
        ],
    );
    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        output.status.success(),
        "create should recover after retries.\nstdout: {stdout}\nstderr: {stderr}"
    );

    // Both injected failures should have been reported as retries.
    let combined = format!("{stdout}{stderr}");
    assert!(
        combined.contains("retrying on next slot (attempt 2/3)"),
        "expected first retry message; got:\n{combined}"
    );
    assert!(
        combined.contains("retrying on next slot (attempt 3/3)"),
        "expected second retry message; got:\n{combined}"
    );

    // `network status` must show the VM on slot 2 (slots 0 and 1 poisoned),
    // and the poisoned slots must be free again (released on each retry).
    let status = common::ember(&[
        "--state-dir",
        &state,
        "network",
        "status",
        "--format",
        "json",
    ]);
    let status_out = String::from_utf8_lossy(&status.stdout);
    let rows: serde_json::Value = serde_json::from_str(&status_out)
        .unwrap_or_else(|e| panic!("invalid network status JSON: {e}\n{status_out}"));
    let arr = rows
        .as_array()
        .unwrap_or_else(|| panic!("expected JSON array:\n{status_out}"));

    let faulttest = arr
        .iter()
        .find(|r| r["vm_name"] == "faulttest")
        .unwrap_or_else(|| panic!("faulttest not in network status:\n{status_out}"));
    assert_eq!(
        faulttest["block_index"], 2,
        "VM should land on slot 2 after poisoning 0 and 1"
    );

    // Only one allocation (the survivor) — the poisoned slots were released.
    assert_eq!(
        arr.len(),
        1,
        "poisoned slots should be released, not leaked"
    );

    common::stop_and_delete_vm(&state, "faulttest");
}

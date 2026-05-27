//! Integration test for the `task_checkpoint` / `task_restore` RPCs.
//!
//! Drives the real `emberd` binary over a Unix domain socket through the full
//! lifecycle: checkpoint -> mutate the workspace -> restore -> verify the
//! workspace matches the checkpoint exactly.
//!
//!   -> {"op":"task_checkpoint","name":"...","workspace":"...","checkpoint_root":"..."}
//!   <- {"checkpoint_id":"cp-..."}
//!   -> {"op":"task_restore","checkpoint_id":"cp-...","workspace":"...","checkpoint_root":"..."}
//!   <- {"ok":true,"restored_count":N}

use std::io::{BufRead, BufReader, Write};
use std::os::unix::net::UnixStream;
use std::process::{Child, Command};
use std::time::{Duration, Instant};

#[allow(clippy::zombie_processes)]
fn spawn_emberd(sock: &str) -> Child {
    let bin = env!("CARGO_BIN_EXE_emberd");
    let child = Command::new(bin)
        .arg("--uds")
        .arg(sock)
        .spawn()
        .expect("spawn emberd");

    let deadline = Instant::now() + Duration::from_secs(5);
    while Instant::now() < deadline {
        if UnixStream::connect(sock).is_ok() {
            return child;
        }
        std::thread::sleep(Duration::from_millis(20));
    }
    panic!("emberd did not start listening on {sock}");
}

fn roundtrip(sock: &str, request: &serde_json::Value) -> serde_json::Value {
    let stream = UnixStream::connect(sock).expect("connect to emberd");
    let mut writer = &stream;
    let mut reader = BufReader::new(&stream);

    writer.write_all(request.to_string().as_bytes()).unwrap();
    writer.write_all(b"\n").unwrap();
    writer.flush().unwrap();

    let mut line = String::new();
    reader.read_line(&mut line).unwrap();
    serde_json::from_str(&line).expect("valid JSON response")
}

#[test]
fn checkpoint_then_restore_over_uds() {
    let pid = std::process::id();
    let sock = format!("/tmp/emberd-checkpoint-it-{pid}.sock");
    let base = format!("/tmp/emberd-checkpoint-it-{pid}");
    let workspace = format!("{base}/workspace");
    let checkpoint_root = format!("{base}/checkpoints");
    let scratch = format!("{base}/scratch");

    let _ = std::fs::remove_dir_all(&base);
    std::fs::create_dir_all(format!("{workspace}/sub")).unwrap();
    std::fs::create_dir_all(&scratch).unwrap();
    std::fs::write(format!("{workspace}/a.txt"), b"original-a").unwrap();
    std::fs::write(format!("{workspace}/sub/b.txt"), b"original-b").unwrap();

    // A scratch file that should be snapshotted and restored.
    let scratch_file = format!("{scratch}/thermite-state.json");
    std::fs::write(&scratch_file, b"snapshot-state").unwrap();
    let tmp_glob = format!("{scratch}/thermite-*");

    let mut child = spawn_emberd(&sock);

    // 1. Checkpoint.
    let resp = roundtrip(
        &sock,
        &serde_json::json!({
            "op": "task_checkpoint",
            "name": "before-change",
            "workspace": workspace,
            "checkpoint_root": checkpoint_root,
            "tmp_globs": [tmp_glob],
        }),
    );
    let id = resp["checkpoint_id"]
        .as_str()
        .unwrap_or_else(|| panic!("expected checkpoint_id, got: {resp}"))
        .to_string();
    assert!(id.starts_with("cp-"), "id: {id}");

    // 2. Mutate the workspace and scratch file after the checkpoint.
    std::fs::write(format!("{workspace}/a.txt"), b"CHANGED").unwrap();
    std::fs::remove_file(format!("{workspace}/sub/b.txt")).unwrap();
    std::fs::write(format!("{workspace}/new.txt"), b"new").unwrap();
    std::fs::write(&scratch_file, b"mutated-state").unwrap();

    // 3. Restore.
    let resp = roundtrip(
        &sock,
        &serde_json::json!({
            "op": "task_restore",
            "checkpoint_id": id,
            "workspace": workspace,
            "checkpoint_root": checkpoint_root,
            "tmp_globs": [tmp_glob],
        }),
    );
    assert_eq!(resp["ok"], true, "resp: {resp}");
    assert!(
        resp["restored_count"].as_u64().unwrap() >= 1,
        "resp: {resp}"
    );

    // 4. Workspace + scratch must match the checkpoint exactly.
    assert_eq!(
        std::fs::read(format!("{workspace}/a.txt")).unwrap(),
        b"original-a"
    );
    assert_eq!(
        std::fs::read(format!("{workspace}/sub/b.txt")).unwrap(),
        b"original-b"
    );
    assert!(
        !std::path::Path::new(&format!("{workspace}/new.txt")).exists(),
        "post-checkpoint file should be gone after restore"
    );
    assert_eq!(std::fs::read(&scratch_file).unwrap(), b"snapshot-state");

    // Restoring an unknown checkpoint is an error.
    let resp = roundtrip(
        &sock,
        &serde_json::json!({
            "op": "task_restore",
            "checkpoint_id": "cp-1-deadbeef",
            "workspace": workspace,
            "checkpoint_root": checkpoint_root,
        }),
    );
    assert!(resp["error"].as_str().is_some(), "resp: {resp}");

    let _ = child.kill();
    let _ = child.wait();
    let _ = std::fs::remove_dir_all(&base);
    let _ = std::fs::remove_file(&sock);
}

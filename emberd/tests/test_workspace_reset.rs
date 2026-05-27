//! Integration test for the `workspace_reset` RPC.
//!
//! Drives the real `emberd` binary over a Unix domain socket (the cross-platform
//! transport emberd exposes for testing) and exercises the full request/response
//! contract end to end:
//!
//!   -> {"op":"workspace_reset","path":"/tmp/..."}
//!   <- {"ok":true,"removed_count":N,"duration_ms":M}

use std::io::{BufRead, BufReader, Write};
use std::os::unix::net::UnixStream;
use std::path::PathBuf;
use std::process::{Child, Command};
use std::time::{Duration, Instant};

/// Spawn `emberd --uds <sock>` and wait until the socket is connectable.
/// The caller owns the returned child and is responsible for killing/waiting it.
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

/// Send one JSON request line and read one JSON response line.
fn roundtrip(sock: &str, request: &str) -> serde_json::Value {
    let stream = UnixStream::connect(sock).expect("connect to emberd");
    let mut writer = &stream;
    let mut reader = BufReader::new(&stream);

    writer.write_all(request.as_bytes()).unwrap();
    writer.write_all(b"\n").unwrap();
    writer.flush().unwrap();

    let mut line = String::new();
    reader.read_line(&mut line).unwrap();
    serde_json::from_str(&line).expect("valid JSON response")
}

#[test]
fn workspace_reset_over_uds_removes_tree() {
    let pid = std::process::id();
    let sock = format!("/tmp/emberd-it-{pid}.sock");
    let workspace = PathBuf::from(format!("/tmp/emberd-it-ws-{pid}"));

    let _ = std::fs::remove_dir_all(&workspace);
    std::fs::create_dir_all(workspace.join("nested/deep")).unwrap();
    std::fs::write(workspace.join("a.txt"), b"alpha").unwrap();
    std::fs::write(workspace.join("nested/b.txt"), b"beta").unwrap();
    std::fs::write(workspace.join("nested/deep/c.txt"), b"gamma").unwrap();

    let mut child = spawn_emberd(&sock);

    // A process whose CWD lives inside the workspace must be killed by the reset.
    let mut inhabitant = Command::new("sleep")
        .arg("300")
        .current_dir(&workspace)
        .spawn()
        .expect("spawn workspace inhabitant");
    let inhabitant_pid = inhabitant.id();

    let req = format!(
        "{{\"op\":\"workspace_reset\",\"path\":\"{}\"}}",
        workspace.display()
    );
    let resp = roundtrip(&sock, &req);

    assert_eq!(resp["ok"], true, "resp: {resp}");
    // workspace + nested + nested/deep + a.txt + nested/b.txt + nested/deep/c.txt = 6
    assert_eq!(resp["removed_count"].as_u64().unwrap(), 6, "resp: {resp}");
    assert!(resp["duration_ms"].as_u64().is_some(), "resp: {resp}");
    assert!(!workspace.exists(), "workspace should be removed");

    // The inhabitant should have been signalled; reap the zombie either way.
    let _ = inhabitant.wait();
    assert!(
        !process_alive(inhabitant_pid),
        "process with cwd inside the workspace should be killed"
    );

    // Unsafe paths are rejected without touching the filesystem.
    let resp = roundtrip(&sock, "{\"op\":\"workspace_reset\",\"path\":\"/etc\"}");
    assert!(
        resp["error"].as_str().unwrap().contains("workspace_reset"),
        "resp: {resp}"
    );

    let _ = child.kill();
    let _ = child.wait();
    let _ = std::fs::remove_file(&sock);
}

/// True if a process with the given pid currently exists (kill -0 probe).
fn process_alive(pid: u32) -> bool {
    // SIGCONT(0)-style probe via `kill -0`.
    Command::new("kill")
        .arg("-0")
        .arg(pid.to_string())
        .output()
        .map(|o| o.status.success())
        .unwrap_or(false)
}

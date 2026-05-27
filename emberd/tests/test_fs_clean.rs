//! Integration test for the `fs_clean` RPC.
//!
//! Drives the real `emberd` binary over a Unix domain socket and exercises the
//! full request/response contract:
//!
//!   -> {"op":"fs_clean","globs":["/tmp/..."]}
//!   <- {"removed":["/tmp/...", ...]}

use std::io::{BufRead, BufReader, Write};
use std::os::unix::net::UnixStream;
use std::process::{Child, Command};
use std::time::{Duration, Instant};

/// Spawn `emberd --uds <sock>` and wait until the socket is connectable.
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
fn fs_clean_over_uds_removes_matching_tmp_files() {
    let pid = std::process::id();
    let sock = format!("/tmp/emberd-fsclean-it-{pid}.sock");
    let base = format!("/tmp/emberd-fsclean-it-{pid}");

    let _ = std::fs::remove_dir_all(&base);
    std::fs::create_dir_all(&base).unwrap();
    let result = format!("{base}/thermite-result.json");
    let stream = format!("{base}/agent-output.jsonl");
    let keep = format!("{base}/keep.txt");
    std::fs::write(&result, b"{}").unwrap();
    std::fs::write(&stream, b"line\n").unwrap();
    std::fs::write(&keep, b"keep me").unwrap();

    let mut child = spawn_emberd(&sock);

    let req =
        format!("{{\"op\":\"fs_clean\",\"globs\":[\"{base}/thermite-*\",\"{base}/agent-*\"]}}");
    let resp = roundtrip(&sock, &req);

    let removed = resp["removed"].as_array().expect("removed array");
    assert_eq!(removed.len(), 2, "resp: {resp}");
    assert!(!std::path::Path::new(&result).exists(), "result removed");
    assert!(!std::path::Path::new(&stream).exists(), "stream removed");
    assert!(std::path::Path::new(&keep).exists(), "keep.txt preserved");

    // Empty globs is a no-op.
    let resp = roundtrip(&sock, "{\"op\":\"fs_clean\",\"globs\":[]}");
    assert!(
        resp["removed"].as_array().unwrap().is_empty(),
        "resp: {resp}"
    );

    // Fails closed outside /tmp: matching /etc files are never removed.
    let resp = roundtrip(&sock, "{\"op\":\"fs_clean\",\"globs\":[\"/etc/hostnam*\"]}");
    assert!(
        resp["removed"].as_array().unwrap().is_empty(),
        "must not remove anything outside /tmp: {resp}"
    );

    let _ = child.kill();
    let _ = child.wait();
    let _ = std::fs::remove_dir_all(&base);
    let _ = std::fs::remove_file(&sock);
}

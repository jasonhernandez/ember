//! Integration test for the enhanced `agent_status` RPC.
//!
//! Drives the real `emberd` binary over a Unix domain socket and verifies the
//! response carries all five fields with sensible defaults when no agent is
//! running:
//!
//!   -> {"op":"agent_status"}
//!   <- {"alive":false,"pid":null,"rss_kb":0,"stream_offset":N,"result_mtime":M}

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
fn agent_status_over_uds_returns_all_fields() {
    let pid = std::process::id();
    let sock = format!("/tmp/emberd-agentstatus-it-{pid}.sock");

    let mut child = spawn_emberd(&sock);

    let resp = roundtrip(&sock, "{\"op\":\"agent_status\"}");

    // No thermite-entrypoint process runs during the test: not alive, null pid.
    assert_eq!(resp["alive"], false, "resp: {resp}");
    assert!(resp["pid"].is_null(), "resp: {resp}");
    // The telemetry fields are always present with numeric defaults.
    assert!(resp["rss_kb"].is_u64(), "rss_kb missing: {resp}");
    assert!(
        resp["stream_offset"].is_u64(),
        "stream_offset missing: {resp}"
    );
    assert!(
        resp["result_mtime"].is_u64(),
        "result_mtime missing: {resp}"
    );

    let _ = child.kill();
    let _ = child.wait();
    let _ = std::fs::remove_file(&sock);
}

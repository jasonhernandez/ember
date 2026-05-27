# emberd

Lightweight in-VM daemon for [Ember](https://github.com/aljoscha/ember) VMs.
Serves JSON-lines requests over vsock or Unix domain sockets, providing
structured host-guest communication for the
[Thermite](https://github.com/jasonhernandez/thermite) orchestrator.

## Why

Without emberd, Thermite communicates with VMs via SSH — shelling out for
every exec, file read, and status check. This works but adds latency and
fragility at scale. emberd replaces SSH with a direct vsock channel:
structured requests in, structured responses out, no shell parsing.

Thermite's `EmberdClient` speaks this protocol natively. When emberd is
running in a VM, Thermite automatically routes through vsock. When it's
not (legacy images, crash), Thermite falls back to SSH transparently.

## Protocol

JSON lines over vsock port 1024. One request per line, one response per line.

### ping

```
-> {"op":"ping"}
<- {"ok":true,"uptime_seconds":123.45}
```

### exec

```
-> {"op":"exec","command":"echo hello","env":{"FOO":"bar"}}
<- {"exit_code":0,"stdout":"hello\n","stderr":""}
```

### read_file

```
-> {"op":"read_file","path":"/tmp/result.json"}
<- {"data":"eyJzdGF0dXMiOiAiZG9uZSJ9"}
```

Content is base64-encoded.

### write_file

```
-> {"op":"write_file","path":"/tmp/config.json","data":"eyJ0YXNrIjogIlNFQy0yMDAifQ=="}
<- {"ok":true}
```

### agent_status

```
-> {"op":"agent_status"}
<- {"alive":true,"pid":9999,"rss_kb":1234567,"stream_offset":28847,"result_mtime":1716006000}
```

Scans `/proc` for a process matching `thermite-entrypoint` and reports its
liveness plus the telemetry a poller needs to skip no-op polls and detect
"done + result written" early:

- `alive` — whether an agent process is running.
- `pid` — the agent pid, or `null` when not alive.
- `rss_kb` — agent resident set size in kB (`VmRSS` from `/proc/<pid>/status`),
  `0` when not alive.
- `stream_offset` — byte size of `/tmp/agent-output.jsonl`, `0` if absent.
- `result_mtime` — mtime of `/tmp/thermite-result.json` in whole seconds since
  the Unix epoch, `0` if the file is absent.

All fields are always present; missing files and absent agents fall back to the
defaults above.

### agent_reap

```
-> {"op":"agent_reap"}
<- {"killed_pids":[123,456],"process_count":2}
```

Kills all `claude` agent subprocesses (matched by the `claude --model` argv
pattern: argv[0] basename `claude` plus a `--model` flag). Sends `SIGTERM`,
waits up to 5 seconds, then escalates to `SIGKILL` for any stragglers. Returns
the PIDs that were targeted for observability.

No-op safe: when no claude processes are running, replies
`{"killed_pids":[],"process_count":0}` with success.

### workspace_reset

```
-> {"op":"workspace_reset","path":"/home/ubuntu/workspace"}
<- {"ok":true,"removed_count":1234,"duration_ms":87}
```

Atomically resets a workspace directory. In order:

1. Kills every process whose CWD is under `path` (walks `/proc/*/cwd`).
2. Unmounts any bind/overlay mounts at or under `path`, deepest first
   (lazy `umount -l`).
3. `rm -rf path`.
4. Verifies `path` is gone; returns an error if it still exists.

`removed_count` is the number of filesystem entries removed (the directory
itself plus all descendants); `duration_ms` is the wall-clock time of the reset.

Because this performs an unconditional recursive delete, `path` is validated
hard and **fails closed**: it must be an absolute path with no `..` component,
strictly under `/home/ubuntu/` or `/tmp/`. Anything else is rejected with an
`error` and no filesystem changes. A `path` that does not exist resets to
"already clean" (`removed_count: 0`).

Replaces the SSH `pkill + rm -rf workspace + verify` dance previously done from
the host.

### fs_clean

```
-> {"op":"fs_clean","globs":["/tmp/thermite-*","/tmp/agent-*"]}
<- {"removed":["/tmp/thermite-result.json","/tmp/agent-output.jsonl"]}
```

Deletes files matching the given shell globs and returns the list of paths
actually removed. Only regular files are removed — directories are skipped.

Path-validated and **fails closed**: every expanded path must be absolute, have
no `..` component, and sit strictly under `/tmp` (component-based matching, so
look-alikes like `/tmpfoo` are rejected). Anything outside `/tmp` is silently
skipped rather than removed. Invalid glob patterns are skipped without failing
the whole request. An empty `globs` list is a no-op (`{"removed":[]}`).

### task_checkpoint

```
-> {"op":"task_checkpoint","name":"before-rate-limit"}
<- {"checkpoint_id":"cp-1716000000-abcd1234"}
```

Snapshots the agent workspace (`/home/ubuntu/workspace`) plus the
`/tmp/thermite-*` and `/tmp/agent-*` scratch files into
`/var/lib/emberd/checkpoints/<id>/` and returns the generated checkpoint id.
The optional `name` is recorded in the checkpoint manifest as a human label.

The workspace tree is cloned copy-on-write via `cp --reflink=always` (btrfs /
XFS / ZFS reflinks) when the filesystem supports it; otherwise emberd **falls
back to `tar + gzip`**. The snapshot is written under a `.staging-<id>` name
and renamed into place only once complete, so a crash mid-checkpoint never
leaves a partial snapshot under a usable id.

**Disk-quota guard**: a checkpoint is refused with an `error` if the checkpoint
area (existing checkpoints plus this one's source size) would exceed **1 GiB**.

Requests may override the defaults — `workspace`, `checkpoint_root`,
`tmp_globs` (array), and `quota_bytes` — primarily for testing.

### task_restore

```
-> {"op":"task_restore","checkpoint_id":"cp-1716000000-abcd1234"}
<- {"ok":true,"restored_count":123}
```

Replaces the live workspace and scratch files with checkpoint `<id>`.
`restored_count` is the number of workspace entries plus scratch files restored.

The workspace swap is **atomic**: the snapshot is materialised into a sibling
staging directory, the live workspace is moved aside, the staging directory is
renamed into place, and only then is the old tree removed. If the swap fails
the live workspace is rolled back so it is never lost. Scratch files matching
the checkpoint's globs are removed and replaced with the snapshotted copies.

The `checkpoint_id` is validated (no path traversal); an unknown or malformed
id returns an `error` with no filesystem changes.

> The Thermite-side `EmberdClient.task_checkpoint()/.task_restore()` typed
> wrappers are a separate follow-up in the Thermite repo.

## Build

```bash
# From the ember repo root:
cargo build -p emberd            # debug
cargo build -p emberd --release  # release
```

emberd is a workspace member with minimal dependencies (clap, serde_json,
base64). The vsock listener uses nix and only compiles on Linux. On macOS,
emberd builds in UDS-only mode for testing.

## Usage

```bash
# Production (inside a Linux VM, listens on vsock port 1024)
emberd

# Custom port
emberd --port 2048

# Testing (Unix domain socket, works on any platform)
emberd --uds /tmp/emberd.sock
```

## Testing

```bash
cargo test -p emberd
```

Tests cover all operations, error handling, the agent_reap SIGTERM/SIGKILL
escalation path, the workspace_reset and fs_clean path-validation and delete
logic, the enhanced agent_status fields, the checkpoint/restore roundtrip
(CoW or tar fallback, atomic swap, quota guard), and full UDS integration
roundtrips (dedicated `tests/test_workspace_reset.rs`, `tests/test_fs_clean.rs`,
`tests/test_agent_status.rs`, and `tests/test_checkpoint.rs`).

## Image integration

emberd is baked into Ember VM images via Dockerfile and starts on boot:

```ini
# /etc/systemd/system/emberd.service
[Service]
Type=simple
ExecStart=/usr/local/bin/emberd --port 1024
Restart=always
```

Build and stage for image builds:

```bash
make emberd-image   # builds for Linux, copies to images/emberd
```

## Architecture

```
Host (Thermite)                    Guest VM (emberd)
+-----------------+                +------------------+
| EmberdClient    |                |  emberd          |
|  VsockTransport |--> UDS --> Firecracker/AVF --> AF_VSOCK |
|  (or SSH        |    bridge     |  port 1024       |
|   fallback)     |               |  JSON lines      |
+-----------------+                +------------------+
```

- **Host side**: Thermite connects to `<state_dir>/vms/<name>/vsock.sock`
- **Bridge**: Firecracker or ember-vz (AVF) bridges UDS to vsock
- **Guest side**: emberd listens on `AF_VSOCK` port 1024
- **Fallback**: If emberd isn't running, Thermite uses SSH automatically

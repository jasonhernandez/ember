# emberd protocol

`emberd` is the lightweight in-VM daemon that backs Thermite's
`EmberdClient`. It speaks a line-delimited JSON request/response protocol
over a stream socket — `AF_VSOCK` port 1024 in production, a Unix domain
socket (`--uds <path>`) for tests and macOS development.

This document is the authoritative reference for that protocol: transport,
framing, common conventions, and every operation's request/response shape.

## Transport

| Mode       | Where it lives                                                       |
|------------|----------------------------------------------------------------------|
| Production | `AF_VSOCK`, port **1024**, host CID = `VMADDR_CID_ANY` (0xFFFFFFFF). |
| Testing    | `AF_UNIX` stream socket via `emberd --uds <path>`.                   |

Each connection is a long-lived bidirectional stream. emberd accepts many
concurrent connections; each is handled in its own thread. Closing the
stream from the client side cleanly terminates the handler.

## Framing

- One JSON object per request line. Lines are terminated by `\n`.
- Exactly one JSON object response per request, followed by `\n`.
- Empty lines are ignored.
- A request that is not valid JSON is answered with
  `{"error":"invalid JSON: <detail>"}` on the same connection — the
  connection stays open so the client can retry.
- An unknown `op` field is answered with `{"error":"unknown op: <op>"}`.

There is no batching, no streaming, and no per-request id field: responses
are strictly in-order with requests on the same connection.

## Common conventions

- **`op`** (string, required) — operation name; matches one of the
  operations listed below.
- Requests with required fields missing reply `{"error":"missing '<field>' field"}`.
- Successful responses always include a positive-shape field (`ok: true`,
  the requested data, or a typed result object). Failures always include
  `error: <string>`.
- Binary payloads (`read_file`, `write_file`) are base64-encoded using the
  standard alphabet.
- Time fields are whole seconds since the Unix epoch unless otherwise noted.
- Path-handling operations (`fs_clean`, `workspace_reset`) **fail closed**:
  paths must be absolute, must not contain a `..` component, and must sit
  strictly under their allowed root (`/tmp/` for `fs_clean`; `/home/ubuntu/`
  or `/tmp/` for `workspace_reset`). A bare root (`/tmp`, `/home/ubuntu`)
  is rejected — the path must be strictly a descendant.

## Operations

### `ping`

Liveness probe; also reports daemon uptime.

```
-> {"op":"ping"}
<- {"ok":true,"uptime_seconds":123.45}
```

`uptime_seconds` is parsed from `/proc/uptime` when available, otherwise
the daemon's own process uptime.

### `exec`

Run a shell command. The command runs under `sh -c`; environment overrides
in `env` are layered on top of the daemon's environment.

```
-> {"op":"exec","command":"echo hello","env":{"FOO":"bar"}}
<- {"exit_code":0,"stdout":"hello\n","stderr":""}
```

Output is captured in full and returned as UTF-8 strings (invalid bytes
are replaced). If the process cannot be spawned, `exit_code` is `-1` and
`stderr` carries the spawn error. There is no streaming.

### `read_file`

Read a file. Content is base64-encoded.

```
-> {"op":"read_file","path":"/tmp/result.json"}
<- {"data":"eyJzdGF0dXMiOiAiZG9uZSJ9"}
```

Errors (missing file, permission denied, etc.) reply
`{"error":"read_file: <detail>"}`.

### `write_file`

Write a file. Content is base64-encoded.

```
-> {"op":"write_file","path":"/tmp/config.json","data":"eyJ0YXNrIjogIlNFQy0yMDAifQ=="}
<- {"ok":true}
```

Missing `path` or `data`, or invalid base64, replies with `error`.

### `agent_status` (enhanced; additive)

Report the agent process's liveness plus the telemetry a poller needs to
skip no-op polls and detect "done + result written" early.

```
-> {"op":"agent_status"}
<- {"alive":true,"pid":9999,"rss_kb":1234567,"stream_offset":28847,"result_mtime":1716006000}
```

Scans `/proc` for a process whose cmdline contains
`thermite-entrypoint`. Fields:

| Field           | Type           | Meaning                                                                                                |
|-----------------|----------------|--------------------------------------------------------------------------------------------------------|
| `alive`         | bool           | Whether an agent process is running.                                                                   |
| `pid`           | u32 or `null`  | Agent pid, or `null` when not alive.                                                                   |
| `rss_kb`        | u64            | Agent resident set size in kB (`VmRSS` from `/proc/<pid>/status`); `0` when not alive.                 |
| `stream_offset` | u64            | Byte size of `/tmp/agent-output.jsonl`; `0` if the file is absent.                                     |
| `result_mtime`  | u64            | Mtime of `/tmp/thermite-result.json` in whole seconds since the Unix epoch; `0` if the file is absent. |

All fields are always present in the response — missing files and absent
agents fall back to the documented defaults. Backward-compatible:
historical clients that only consumed `alive` continue to work; the new
`pid`, `rss_kb`, `stream_offset`, and `result_mtime` fields are additive.

### `agent_reap`

Kill all `claude` agent subprocesses; replaces the SSH `pkill -f 'claude.*--model'`
dance previously done from the host.

```
-> {"op":"agent_reap"}
<- {"killed_pids":[123,456],"process_count":2}
```

Matches processes whose `/proc/<pid>/cmdline` has `claude` as the argv[0]
basename (component match — `claudette` is not `claude`) **and** a
`--model` flag somewhere in argv. The daemon's own pid is excluded from
the match.

Reaping sends `SIGTERM`, waits up to **5 seconds** for the targets to
exit, then escalates to `SIGKILL` for any stragglers. The response lists
the pids that were targeted, in the order they were discovered.

No-op safe: when no claude processes are running, replies
`{"killed_pids":[],"process_count":0}`. Linux-only behavior; on other
platforms `killed_pids` is always empty.

### `vm_stats`

Coarse VM-level resource metrics.

```
-> {"op":"vm_stats"}
<- {"cpu_pct":4.5,"memory_used_mb":1234,"memory_total_mb":8192,"disk_used_gb":12.3,"net_rx_bytes":98765,"net_tx_bytes":54321}
```

- `cpu_pct` — busy/total over a 100 ms sample of `/proc/stat`'s aggregate
  `cpu` line, clamped to `[0.0, 100.0]`.
- `memory_used_mb` / `memory_total_mb` — derived from `MemTotal` and
  `MemAvailable` in `/proc/meminfo`. `used = total - available`.
- `disk_used_gb` — `statvfs("/")`; `(blocks − bfree) × bsize` rendered as
  GiB (1024³).
- `net_rx_bytes` / `net_tx_bytes` — sum of every non-`lo` interface in
  `/proc/net/dev`.

Any unavailable source falls back to `0`; the call never fails.

### `workspace_reset`

Atomically reset a workspace directory. Replaces the SSH `pkill + rm -rf
+ verify` dance.

```
-> {"op":"workspace_reset","path":"/home/ubuntu/workspace"}
<- {"ok":true,"removed_count":1234,"duration_ms":87}
```

Sequence:

1. Walk `/proc/*/cwd` and SIGTERM/SIGKILL every process whose cwd is
   `path` or a descendant — so nothing races the delete. (Linux only.)
2. Parse `/proc/mounts` and `umount -l` every mountpoint at or under
   `path`, deepest first.
3. `rm -rf path`. An already-absent path is fine and replies
   `removed_count: 0`.
4. Verify `path` is gone; otherwise reply
   `{"error":"workspace_reset: path still exists after delete: <path>"}`.

`removed_count` is the number of filesystem entries removed (including
`path` itself); `duration_ms` is the wall-clock time of the reset.

**Fails closed**: `path` must be absolute, contain no `..` component, and
sit strictly under `/home/ubuntu/` or `/tmp/`. Anything else is rejected
with an `error` and no filesystem changes (`/etc`, `/var/lib`, bare
`/tmp`, `/home/ubuntuevil`, `/tmpfoo`, and `path/with/..` are all rejected).

### `fs_clean`

Delete scratch files matching one or more shell globs and report what was
removed. Replaces scattered `rm -f /tmp/...` from the host.

```
-> {"op":"fs_clean","globs":["/tmp/thermite-*","/tmp/agent-*"]}
<- {"removed":["/tmp/thermite-result.json","/tmp/agent-output.jsonl"]}
```

- Only regular files are removed — directories are skipped.
- Every expanded path must be absolute, contain no `..` component, and
  sit strictly under `/tmp/` (component-based prefix match — `/tmpfoo`
  is not under `/tmp/`). Matches outside `/tmp/` are silently skipped
  rather than removed.
- Invalid glob patterns are skipped without failing the whole request.
- An empty `globs` list is a no-op (`{"removed":[]}`).
- Missing `globs` field replies `{"error":"missing 'globs' field"}`.

### `task_checkpoint`

Snapshot the agent workspace and scratch files into a checkpoint.

```
-> {"op":"task_checkpoint","name":"before-rate-limit"}
<- {"checkpoint_id":"cp-1716000000-abcd1234"}
```

Snapshots `/home/ubuntu/workspace` plus `/tmp/thermite-*` and
`/tmp/agent-*` into `/var/lib/emberd/checkpoints/<id>/`. The optional
`name` is recorded in the checkpoint manifest as a human label.

Workspace storage method, picked automatically:

- **Copy-on-write** — `cp --reflink=always` (btrfs / XFS / ZFS reflinks)
  when the filesystem supports it. Workspace ends up under
  `<id>/workspace/`.
- **`tar + gzip`** fallback when reflinks are not supported. Workspace
  ends up at `<id>/workspace.tar.gz`.

The chosen method is recorded as `workspace_method` in the checkpoint's
`manifest.json` so `task_restore` knows how to reverse it.

**Atomicity**: the snapshot is first written under a `.staging-<id>`
sibling and only renamed into `<id>` once `manifest.json` is on disk. A
crash mid-checkpoint never leaves a partial snapshot under a usable id.

**Disk-quota guard**: a checkpoint is refused with an `error` if the
checkpoint area (existing checkpoints plus this one's uncompressed source
size) would exceed **1 GiB** (`DEFAULT_QUOTA_BYTES`).

Request overrides (primarily for tests):

| Field             | Type       | Default                            |
|-------------------|------------|------------------------------------|
| `workspace`       | string     | `/home/ubuntu/workspace`           |
| `checkpoint_root` | string     | `/var/lib/emberd/checkpoints`      |
| `tmp_globs`       | [string]   | `["/tmp/thermite-*","/tmp/agent-*"]` |
| `quota_bytes`     | u64        | `1073741824` (1 GiB)               |
| `name`            | string     | `null`                             |

The returned `checkpoint_id` has the form `cp-<unix-secs>-<8 hex>`.

### `task_restore`

Replace the live workspace and scratch files with a previously created
checkpoint.

```
-> {"op":"task_restore","checkpoint_id":"cp-1716000000-abcd1234"}
<- {"ok":true,"restored_count":123}
```

`restored_count` is the number of workspace entries plus scratch files
restored.

The workspace swap is **atomic**: the snapshot is materialised into a
sibling staging directory, the live workspace is moved aside, the staging
directory is renamed into place, and only then is the old tree removed.
If the rename-into-place fails, the live workspace is rolled back so it
is never lost.

Scratch files matching the checkpoint's `tmp_globs` are removed from the
live system and replaced with the copies in the manifest's `tmp_files`
list. Each entry's original absolute path is preserved.

`checkpoint_id` is validated (no path traversal — only
`[A-Za-z0-9._-]`, no `..` substring, non-empty); an unknown or malformed
id replies `{"error":"task_restore: ..."}` with no filesystem changes.

Request overrides match `task_checkpoint`'s (`workspace`,
`checkpoint_root`, `tmp_globs`).

## Errors

Every operation can fail with `{"error":"<op>: <detail>"}`. Common
shapes:

- `missing '<field>' field` — required field absent.
- `invalid JSON: <detail>` — request line failed to parse; the connection
  stays open.
- `unknown op: <op>` — `op` field is absent or unrecognised.
- `<op>: <message>` — operation-specific failure (path validation,
  filesystem error, quota guard, etc.).

There are no numeric error codes; clients should branch on the `error`
field's presence and parse the message when more detail is needed.

## Versioning and compatibility

The protocol is currently un-versioned. Backward-compatible changes
(additive response fields, new ops, new optional request fields) are
made in place; existing fields are never repurposed. `agent_status`'s
enhancement (`pid`, `rss_kb`, `stream_offset`, `result_mtime` added in
SEC-349) is an example: historical clients that only read `alive`
continue to work unchanged.

## Related

- `emberd/src/main.rs` — listener, dispatch, and all non-checkpoint ops.
- `emberd/src/checkpoint.rs` — `task_checkpoint` / `task_restore`.
- `emberd/tests/test_workspace_reset.rs`,
  `emberd/tests/test_fs_clean.rs`,
  `emberd/tests/test_agent_status.rs`,
  `emberd/tests/test_checkpoint.rs` — UDS integration tests exercising
  the wire protocol end-to-end against the real `emberd` binary.
- Thermite's `EmberdClient` (in the Thermite repo's `daemon_client.py`)
  is the canonical Python client; typed wrappers for the new lifecycle
  RPCs are tracked as a follow-up there.

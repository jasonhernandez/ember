//! Workspace + scratch-file checkpoint/restore for emberd.
//!
//! Backs the `task_checkpoint` / `task_restore` RPCs. A checkpoint snapshots
//! the agent workspace plus the `/tmp/thermite-*` and `/tmp/agent-*` scratch
//! files into `/var/lib/emberd/checkpoints/<id>/`; a restore atomically swaps
//! the live workspace and scratch files back to a snapshot.
//!
//! Copy-on-write (`cp --reflink=always`, i.e. btrfs/XFS/ZFS reflinks) is used
//! for the workspace tree when the filesystem supports it; otherwise we fall
//! back to `tar + gzip`. Restore is atomic: the new workspace is staged beside
//! the live one, then rename-swapped into place, and the old tree is removed
//! afterwards.

use serde_json::{json, Value};
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

/// Where checkpoints are stored in production.
pub const DEFAULT_CHECKPOINT_ROOT: &str = "/var/lib/emberd/checkpoints";

/// The live agent workspace.
pub const DEFAULT_WORKSPACE: &str = "/home/ubuntu/workspace";

/// Scratch-file globs included in every checkpoint.
pub const DEFAULT_TMP_GLOBS: [&str; 2] = ["/tmp/thermite-*", "/tmp/agent-*"];

/// Refuse a checkpoint if the checkpoint area would exceed this many bytes.
pub const DEFAULT_QUOTA_BYTES: u64 = 1024 * 1024 * 1024; // 1 GiB

/// Inputs for a checkpoint/restore. Defaults point at the production paths;
/// requests may override them (used by tests to redirect to a tempdir).
pub struct Config {
    pub workspace: PathBuf,
    pub checkpoint_root: PathBuf,
    pub tmp_globs: Vec<String>,
    pub quota_bytes: u64,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            workspace: PathBuf::from(DEFAULT_WORKSPACE),
            checkpoint_root: PathBuf::from(DEFAULT_CHECKPOINT_ROOT),
            tmp_globs: DEFAULT_TMP_GLOBS.iter().map(|s| s.to_string()).collect(),
            quota_bytes: DEFAULT_QUOTA_BYTES,
        }
    }
}

impl Config {
    /// Build a `Config` from a request, applying overrides when present.
    fn from_request(req: &Value) -> Self {
        let mut cfg = Config::default();
        if let Some(ws) = req.get("workspace").and_then(Value::as_str) {
            cfg.workspace = PathBuf::from(ws);
        }
        if let Some(root) = req.get("checkpoint_root").and_then(Value::as_str) {
            cfg.checkpoint_root = PathBuf::from(root);
        }
        if let Some(globs) = req.get("tmp_globs").and_then(Value::as_array) {
            cfg.tmp_globs = globs
                .iter()
                .filter_map(|v| v.as_str().map(str::to_string))
                .collect();
        }
        if let Some(q) = req.get("quota_bytes").and_then(Value::as_u64) {
            cfg.quota_bytes = q;
        }
        cfg
    }
}

/// How the workspace tree was stored in a checkpoint.
#[derive(Clone, Copy, PartialEq, Eq)]
enum WorkspaceMethod {
    /// Reflink copy under `<id>/workspace/`.
    Cow,
    /// `tar + gzip` archive at `<id>/workspace.tar.gz`.
    Tar,
}

impl WorkspaceMethod {
    fn as_str(self) -> &'static str {
        match self {
            WorkspaceMethod::Cow => "cow",
            WorkspaceMethod::Tar => "tar",
        }
    }

    fn from_str(s: &str) -> Option<Self> {
        match s {
            "cow" => Some(WorkspaceMethod::Cow),
            "tar" => Some(WorkspaceMethod::Tar),
            _ => None,
        }
    }
}

// ---------------------------------------------------------------------------
// RPC entry points
// ---------------------------------------------------------------------------

/// Handle `{"op":"task_checkpoint","name":"..."}`.
/// Reply: `{"checkpoint_id":"cp-<secs>-<rand>"}` or `{"error":...}`.
pub fn op_task_checkpoint(req: &Value) -> Value {
    let cfg = Config::from_request(req);
    let label = req.get("name").and_then(Value::as_str);
    match checkpoint(&cfg, label) {
        Ok(id) => json!({ "checkpoint_id": id }),
        Err(e) => json!({ "error": format!("task_checkpoint: {e}") }),
    }
}

/// Handle `{"op":"task_restore","checkpoint_id":"..."}`.
/// Reply: `{"ok":true,"restored_count":N}` or `{"error":...}`.
pub fn op_task_restore(req: &Value) -> Value {
    let cfg = Config::from_request(req);
    let Some(id) = req.get("checkpoint_id").and_then(Value::as_str) else {
        return json!({ "error": "task_restore: missing 'checkpoint_id' field" });
    };
    match restore(&cfg, id) {
        Ok(count) => json!({ "ok": true, "restored_count": count }),
        Err(e) => json!({ "error": format!("task_restore: {e}") }),
    }
}

// ---------------------------------------------------------------------------
// Checkpoint
// ---------------------------------------------------------------------------

/// Snapshot the workspace and scratch files into `<root>/<id>/`.
///
/// The directory is built under a `.staging-<id>` name and renamed into place
/// only once fully written, so a crash mid-checkpoint never leaves a partial
/// snapshot under a usable id.
fn checkpoint(cfg: &Config, _label: Option<&str>) -> Result<String, String> {
    std::fs::create_dir_all(&cfg.checkpoint_root)
        .map_err(|e| format!("create checkpoint root: {e}"))?;

    // Disk-quota guard: refuse if existing checkpoints plus this one's
    // (uncompressed) source size would exceed the quota.
    let existing = dir_size(&cfg.checkpoint_root);
    let mut incoming = dir_size(&cfg.workspace);
    let tmp_files = collect_tmp_files(&cfg.tmp_globs);
    for f in &tmp_files {
        incoming += std::fs::metadata(f).map(|m| m.len()).unwrap_or(0);
    }
    if existing + incoming > cfg.quota_bytes {
        return Err(format!(
            "disk-quota guard: checkpoint area would reach {} bytes, exceeding limit {}",
            existing + incoming,
            cfg.quota_bytes
        ));
    }

    let id = generate_id();
    let staging = cfg.checkpoint_root.join(format!(".staging-{id}"));
    let _ = std::fs::remove_dir_all(&staging);
    std::fs::create_dir_all(&staging).map_err(|e| format!("create staging dir: {e}"))?;

    // Snapshot the workspace tree (CoW if possible, else tar+gzip).
    let method = snapshot_workspace(&cfg.workspace, &staging)?;

    // Snapshot scratch files, recording their original absolute paths so a
    // restore can put them back exactly where they came from.
    let tmp_dir = staging.join("tmp");
    std::fs::create_dir_all(&tmp_dir).map_err(|e| format!("create tmp dir: {e}"))?;
    let mut tmp_manifest: Vec<Value> = Vec::new();
    for (i, src) in tmp_files.iter().enumerate() {
        let stored = format!("{i}-{}", file_name_str(src));
        let dst = tmp_dir.join(&stored);
        std::fs::copy(src, &dst).map_err(|e| format!("copy {}: {e}", src.display()))?;
        tmp_manifest.push(json!({
            "original": src.to_string_lossy(),
            "stored": stored,
        }));
    }

    // Write the manifest, then atomically publish the snapshot.
    let manifest = json!({
        "id": id,
        "name": _label,
        "created_secs": now_secs(),
        "workspace_method": method.as_str(),
        "workspace_original": cfg.workspace.to_string_lossy(),
        "tmp_files": tmp_manifest,
    });
    std::fs::write(
        staging.join("manifest.json"),
        serde_json::to_vec_pretty(&manifest).unwrap(),
    )
    .map_err(|e| format!("write manifest: {e}"))?;

    let final_dir = cfg.checkpoint_root.join(&id);
    let _ = std::fs::remove_dir_all(&final_dir);
    std::fs::rename(&staging, &final_dir).map_err(|e| format!("publish checkpoint: {e}"))?;

    Ok(id)
}

/// Snapshot `workspace` into `staging`, preferring a reflink copy and falling
/// back to `tar + gzip`. Returns the method used. A missing workspace is
/// snapshotted as an empty CoW tree so restore recreates an empty workspace.
fn snapshot_workspace(workspace: &Path, staging: &Path) -> Result<WorkspaceMethod, String> {
    let dst = staging.join("workspace");
    if !workspace.exists() {
        std::fs::create_dir_all(&dst).map_err(|e| format!("create empty workspace: {e}"))?;
        return Ok(WorkspaceMethod::Cow);
    }

    if cow_copy(workspace, &dst) {
        return Ok(WorkspaceMethod::Cow);
    }
    // Reflink failed (filesystem unsupported): clean any partial copy and
    // fall back to a portable tar+gzip archive.
    let _ = std::fs::remove_dir_all(&dst);
    tar_create(workspace, &staging.join("workspace.tar.gz"))?;
    Ok(WorkspaceMethod::Tar)
}

// ---------------------------------------------------------------------------
// Restore
// ---------------------------------------------------------------------------

/// Replace the live workspace and scratch files with checkpoint `<id>`.
///
/// Atomic for the workspace: the snapshot is materialised into a sibling
/// staging dir, the live workspace is moved aside, the staging dir is renamed
/// into place, and only then is the old tree removed.
fn restore(cfg: &Config, id: &str) -> Result<u64, String> {
    if !valid_id(id) {
        return Err(format!("invalid checkpoint id: {id}"));
    }
    let dir = cfg.checkpoint_root.join(id);
    if !dir.is_dir() {
        return Err(format!("no such checkpoint: {id}"));
    }

    let manifest: Value = {
        let raw = std::fs::read_to_string(dir.join("manifest.json"))
            .map_err(|e| format!("read manifest: {e}"))?;
        serde_json::from_str(&raw).map_err(|e| format!("parse manifest: {e}"))?
    };
    let method = manifest
        .get("workspace_method")
        .and_then(Value::as_str)
        .and_then(WorkspaceMethod::from_str)
        .ok_or("manifest missing/invalid workspace_method")?;

    let mut restored: u64 = 0;

    // --- Workspace: stage -> swap -> cleanup ---
    let parent = cfg
        .workspace
        .parent()
        .ok_or("workspace has no parent directory")?;
    let file_name = file_name_str(&cfg.workspace);
    let stage = parent.join(format!(".emberd-restore-{id}-{file_name}"));
    let old = parent.join(format!(".emberd-old-{id}-{file_name}"));
    let _ = std::fs::remove_dir_all(&stage);
    let _ = std::fs::remove_dir_all(&old);

    match method {
        WorkspaceMethod::Cow => {
            if !cow_copy(&dir.join("workspace"), &stage) {
                return Err("reflink restore of workspace failed".to_string());
            }
        }
        WorkspaceMethod::Tar => {
            std::fs::create_dir_all(&stage).map_err(|e| format!("create restore stage: {e}"))?;
            tar_extract(&dir.join("workspace.tar.gz"), &stage)?;
        }
    }
    restored += count_entries(&stage);

    // Swap: move live aside (if present), move staged into place, drop old.
    let had_old = cfg.workspace.exists();
    if had_old {
        std::fs::rename(&cfg.workspace, &old).map_err(|e| format!("move live workspace: {e}"))?;
    }
    if let Err(e) = std::fs::rename(&stage, &cfg.workspace) {
        // Roll back: put the live workspace back so we never lose it.
        if had_old {
            let _ = std::fs::rename(&old, &cfg.workspace);
        }
        return Err(format!("swap workspace into place: {e}"));
    }
    if had_old {
        let _ = std::fs::remove_dir_all(&old);
    }

    // --- Scratch files: remove current matches, then restore snapshot ---
    for f in collect_tmp_files(&cfg.tmp_globs) {
        let _ = std::fs::remove_file(&f);
    }
    if let Some(entries) = manifest.get("tmp_files").and_then(Value::as_array) {
        for e in entries {
            let (Some(original), Some(stored)) = (
                e.get("original").and_then(Value::as_str),
                e.get("stored").and_then(Value::as_str),
            ) else {
                continue;
            };
            let src = dir.join("tmp").join(stored);
            if std::fs::copy(&src, original).is_ok() {
                restored += 1;
            }
        }
    }

    Ok(restored)
}

// ---------------------------------------------------------------------------
// CoW + tar helpers
// ---------------------------------------------------------------------------

/// Attempt a copy-on-write clone of `src` to `dst` via `cp --reflink=always`.
/// Returns `true` only when the reflink copy succeeded; any failure (the
/// filesystem doesn't support reflinks, `cp` missing, etc.) returns `false`
/// so the caller can fall back.
fn cow_copy(src: &Path, dst: &Path) -> bool {
    matches!(
        std::process::Command::new("cp")
            .arg("--reflink=always")
            .arg("-a")
            .arg(src)
            .arg(dst)
            .output(),
        Ok(o) if o.status.success()
    )
}

/// `tar -czf archive -C <parent> <name>` — archive the workspace directory.
fn tar_create(src: &Path, archive: &Path) -> Result<(), String> {
    let parent = src.parent().ok_or("workspace has no parent")?;
    let name = file_name_str(src);
    let out = std::process::Command::new("tar")
        .arg("-czf")
        .arg(archive)
        .arg("-C")
        .arg(parent)
        .arg(&name)
        .output()
        .map_err(|e| format!("spawn tar: {e}"))?;
    if !out.status.success() {
        return Err(format!(
            "tar create: {}",
            String::from_utf8_lossy(&out.stderr).trim()
        ));
    }
    Ok(())
}

/// `tar -xzf archive -C <dest>` — extract a workspace archive. The archive
/// contains a single top-level `<name>` directory; we extract it then lift its
/// contents up so `dest` itself becomes the workspace tree.
fn tar_extract(archive: &Path, dest: &Path) -> Result<(), String> {
    let tmp = dest.with_extension("untar");
    let _ = std::fs::remove_dir_all(&tmp);
    std::fs::create_dir_all(&tmp).map_err(|e| format!("create untar dir: {e}"))?;
    let out = std::process::Command::new("tar")
        .arg("-xzf")
        .arg(archive)
        .arg("-C")
        .arg(&tmp)
        .output()
        .map_err(|e| format!("spawn tar: {e}"))?;
    if !out.status.success() {
        let _ = std::fs::remove_dir_all(&tmp);
        return Err(format!(
            "tar extract: {}",
            String::from_utf8_lossy(&out.stderr).trim()
        ));
    }
    // The archive holds exactly one top-level directory; rename it to `dest`.
    let inner = std::fs::read_dir(&tmp)
        .map_err(|e| format!("read untar dir: {e}"))?
        .flatten()
        .next()
        .map(|e| e.path())
        .ok_or("empty archive")?;
    std::fs::rename(&inner, dest).map_err(|e| format!("place extracted workspace: {e}"))?;
    let _ = std::fs::remove_dir_all(&tmp);
    Ok(())
}

// ---------------------------------------------------------------------------
// Small helpers
// ---------------------------------------------------------------------------

/// Expand the scratch-file globs to a sorted, de-duplicated list of regular
/// files. Directories and glob errors are skipped.
fn collect_tmp_files(globs: &[String]) -> Vec<PathBuf> {
    let mut files: Vec<PathBuf> = Vec::new();
    for pattern in globs {
        let Ok(paths) = glob::glob(pattern) else {
            continue;
        };
        for entry in paths.flatten() {
            if entry.is_file() && !files.contains(&entry) {
                files.push(entry);
            }
        }
    }
    files.sort();
    files
}

/// Recursively sum the byte sizes of regular files under `path`.
/// Returns 0 if `path` does not exist.
fn dir_size(path: &Path) -> u64 {
    let Ok(meta) = std::fs::symlink_metadata(path) else {
        return 0;
    };
    if meta.file_type().is_symlink() {
        return 0;
    }
    if meta.is_file() {
        return meta.len();
    }
    let mut total = 0;
    if meta.is_dir() {
        if let Ok(rd) = std::fs::read_dir(path) {
            for entry in rd.flatten() {
                total += dir_size(&entry.path());
            }
        }
    }
    total
}

/// Recursively count filesystem entries under `path`, including `path` itself.
/// Symlinks count as one entry and are not followed. 0 if `path` is absent.
fn count_entries(path: &Path) -> u64 {
    let Ok(meta) = std::fs::symlink_metadata(path) else {
        return 0;
    };
    let mut count = 1;
    if meta.file_type().is_dir() {
        if let Ok(rd) = std::fs::read_dir(path) {
            for entry in rd.flatten() {
                count += count_entries(&entry.path());
            }
        }
    }
    count
}

fn file_name_str(path: &Path) -> String {
    path.file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_else(|| "workspace".to_string())
}

fn now_secs() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

/// Generate a checkpoint id of the form `cp-<secs>-<8 hex>`. The hex suffix is
/// derived from the high-resolution clock and pid — collision-resistant enough
/// for one host's checkpoint store without pulling in an RNG crate.
fn generate_id() -> String {
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.subsec_nanos())
        .unwrap_or(0);
    let mix = (nanos ^ std::process::id().rotate_left(11)) as u32;
    format!("cp-{}-{:08x}", now_secs(), mix)
}

/// True if `id` is a safe checkpoint id: only `[A-Za-z0-9._-]`, non-empty, and
/// no path-traversal. Guards `restore` against directory escape.
fn valid_id(id: &str) -> bool {
    !id.is_empty()
        && id != "."
        && id != ".."
        && id
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-'))
        && !id.contains("..")
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    fn test_config(root: &Path, workspace: &Path, tmp_globs: Vec<String>) -> Config {
        Config {
            workspace: workspace.to_path_buf(),
            checkpoint_root: root.to_path_buf(),
            tmp_globs,
            quota_bytes: DEFAULT_QUOTA_BYTES,
        }
    }

    #[test]
    fn valid_id_accepts_generated_ids() {
        assert!(valid_id(&generate_id()));
        assert!(valid_id("cp-1716000000-abcd1234"));
    }

    #[test]
    fn valid_id_rejects_traversal() {
        assert!(!valid_id(""));
        assert!(!valid_id(".."));
        assert!(!valid_id("../etc"));
        assert!(!valid_id("cp/../../etc"));
        assert!(!valid_id("cp-1716/passwd"));
    }

    #[test]
    fn checkpoint_then_restore_roundtrips_workspace() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().join("checkpoints");
        let ws = tmp.path().join("workspace");
        std::fs::create_dir_all(ws.join("sub")).unwrap();
        std::fs::write(ws.join("a.txt"), b"original-a").unwrap();
        std::fs::write(ws.join("sub/b.txt"), b"original-b").unwrap();

        let cfg = test_config(&root, &ws, vec![]);
        let id = checkpoint(&cfg, Some("before-change")).unwrap();
        assert!(valid_id(&id));
        assert!(root.join(&id).join("manifest.json").exists());

        // Mutate the workspace after the checkpoint.
        std::fs::write(ws.join("a.txt"), b"CHANGED").unwrap();
        std::fs::remove_file(ws.join("sub/b.txt")).unwrap();
        std::fs::write(ws.join("new.txt"), b"new file").unwrap();

        let count = restore(&cfg, &id).unwrap();
        assert!(count >= 1);

        // Workspace must match the checkpoint exactly.
        assert_eq!(std::fs::read(ws.join("a.txt")).unwrap(), b"original-a");
        assert_eq!(std::fs::read(ws.join("sub/b.txt")).unwrap(), b"original-b");
        assert!(!ws.join("new.txt").exists(), "new file should be gone");
    }

    #[test]
    fn checkpoint_restore_handles_tmp_files() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().join("checkpoints");
        let ws = tmp.path().join("workspace");
        std::fs::create_dir_all(&ws).unwrap();

        let scratch = tmp.path().join("scratch");
        std::fs::create_dir_all(&scratch).unwrap();
        let f = scratch.join("thermite-state.json");
        std::fs::write(&f, b"snapshot").unwrap();
        let glob = format!("{}/thermite-*", scratch.display());

        let cfg = test_config(&root, &ws, vec![glob]);
        let id = checkpoint(&cfg, None).unwrap();

        std::fs::write(&f, b"mutated").unwrap();
        restore(&cfg, &id).unwrap();
        assert_eq!(std::fs::read(&f).unwrap(), b"snapshot");
    }

    #[test]
    fn quota_guard_refuses_oversized_checkpoint() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().join("checkpoints");
        let ws = tmp.path().join("workspace");
        std::fs::create_dir_all(&ws).unwrap();
        std::fs::write(ws.join("big.bin"), vec![0u8; 4096]).unwrap();

        let mut cfg = test_config(&root, &ws, vec![]);
        cfg.quota_bytes = 1024; // smaller than the workspace
        let err = checkpoint(&cfg, None).unwrap_err();
        assert!(err.contains("disk-quota guard"), "got: {err}");
    }

    #[test]
    fn restore_unknown_id_errors() {
        let tmp = tempfile::tempdir().unwrap();
        let cfg = test_config(
            &tmp.path().join("checkpoints"),
            &tmp.path().join("workspace"),
            vec![],
        );
        assert!(restore(&cfg, "cp-1-deadbeef").is_err());
    }

    #[test]
    fn op_restore_rejects_missing_id() {
        let resp = op_task_restore(&json!({"op": "task_restore"}));
        assert!(resp["error"].as_str().unwrap().contains("checkpoint_id"));
    }
}

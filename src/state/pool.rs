//! Pool state management — tracks groups of VMs for bulk task assignment.
//!
//! A pool is a named collection of VMs created from the same image.
//! Pool state is stored at `<state_dir>/pools/<name>/pool.json` with
//! flock-based locking for atomic assignment.

use std::fmt;

use serde::{Deserialize, Serialize};

use crate::error::{Error, Result};
use crate::state::store::StateStore;
use crate::state::vm;

/// Status of a VM within a pool.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum PoolVmStatus {
    /// VM is ready to accept a task assignment.
    Available,
    /// VM has been assigned a task and is working.
    Assigned,
    /// VM's task completed successfully.
    Completed,
    /// VM's task failed.
    Failed,
}

impl fmt::Display for PoolVmStatus {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            PoolVmStatus::Available => write!(f, "available"),
            PoolVmStatus::Assigned => write!(f, "assigned"),
            PoolVmStatus::Completed => write!(f, "completed"),
            PoolVmStatus::Failed => write!(f, "failed"),
        }
    }
}

/// A VM within a pool, with its assignment state.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct PoolVm {
    /// VM name (matches the `ember vm` name, e.g., "mypool-1").
    pub vm_name: String,
    /// Current status within the pool.
    pub status: PoolVmStatus,
    /// Task ID assigned to this VM, if any.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub task_id: Option<String>,
    /// ISO 8601 timestamp when a task was assigned.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub assigned_at: Option<String>,
    /// ISO 8601 timestamp when the task completed (success or failure).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub completed_at: Option<String>,
}

/// Persistent state for a pool of VMs.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct PoolState {
    /// Pool name.
    pub name: String,
    /// Image reference used to create the pool VMs.
    pub image: String,
    /// VMs in this pool with their assignment state.
    pub vms: Vec<PoolVm>,
    /// ISO 8601 creation timestamp.
    pub created_at: String,
}

// ---------------------------------------------------------------------------
// Persistence helpers
// ---------------------------------------------------------------------------

/// Path to a pool's state file.
pub fn pool_path(store: &StateStore, name: &str) -> std::path::PathBuf {
    store.root().join("pools").join(name).join("pool.json")
}

/// Load pool state from the store. Returns `Error::PoolNotFound` if missing.
pub fn load(store: &StateStore, name: &str) -> Result<PoolState> {
    let path = pool_path(store, name);
    store
        .read_optional(&path)?
        .ok_or_else(|| Error::PoolNotFound {
            name: name.to_string(),
        })
}

/// Save pool state to the store (atomic write with exclusive lock).
pub fn save(store: &StateStore, pool: &PoolState) -> Result<()> {
    let path = pool_path(store, &pool.name);
    store.write(&path, pool)
}

/// Check whether a pool exists.
pub fn exists(store: &StateStore, name: &str) -> bool {
    pool_path(store, name).exists()
}

/// List all pools by reading state from each subdirectory under `pools/`.
pub fn list(store: &StateStore) -> Result<Vec<PoolState>> {
    let pools_dir = store.root().join("pools");
    let entries = match std::fs::read_dir(&pools_dir) {
        Ok(entries) => entries,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(e) => {
            return Err(Error::Io {
                path: pools_dir,
                source: e,
            })
        }
    };

    let mut pools = Vec::new();
    for entry in entries {
        let entry = entry.map_err(|e| Error::Io {
            path: pools_dir.clone(),
            source: e,
        })?;
        let path = entry.path();
        if !path.is_dir() {
            continue;
        }
        let meta_path = path.join("pool.json");
        if let Ok(Some(pool)) = store.read_optional::<PoolState>(&meta_path) {
            pools.push(pool);
        }
    }

    pools.sort_by(|a, b| a.name.cmp(&b.name));
    Ok(pools)
}

/// Delete a pool's state directory.
pub fn delete(store: &StateStore, name: &str) -> Result<()> {
    let dir = store.root().join("pools").join(name);
    store.remove_dir(&dir)
}

// ---------------------------------------------------------------------------
// Pool operations
// ---------------------------------------------------------------------------

/// Atomically assign a task to the next available VM in the pool.
///
/// Uses the StateStore's exclusive flock to prevent two concurrent callers
/// from assigning the same VM. Returns the assigned VM name and task ID.
pub fn assign(store: &StateStore, pool_name: &str, task_id: &str) -> Result<PoolVm> {
    let mut pool = load(store, pool_name)?;

    let slot = pool
        .vms
        .iter_mut()
        .find(|vm| vm.status == PoolVmStatus::Available);

    match slot {
        Some(vm) => {
            vm.status = PoolVmStatus::Assigned;
            vm.task_id = Some(task_id.to_string());
            vm.assigned_at = Some(vm::now_iso8601());
            let result = vm.clone();
            save(store, &pool)?;
            Ok(result)
        }
        None => Err(Error::PoolFull {
            name: pool_name.to_string(),
        }),
    }
}

/// Mark a VM's task as complete (success or failure).
pub fn complete(store: &StateStore, pool_name: &str, vm_name: &str, failed: bool) -> Result<()> {
    let mut pool = load(store, pool_name)?;

    let vm = pool
        .vms
        .iter_mut()
        .find(|v| v.vm_name == vm_name)
        .ok_or_else(|| Error::VmNotInPool {
            vm_name: vm_name.to_string(),
            pool_name: pool_name.to_string(),
        })?;

    vm.status = if failed {
        PoolVmStatus::Failed
    } else {
        PoolVmStatus::Completed
    };
    vm.completed_at = Some(vm::now_iso8601());

    save(store, &pool)
}

/// Release a VM back to available state.
pub fn release(store: &StateStore, pool_name: &str, vm_name: &str) -> Result<()> {
    let mut pool = load(store, pool_name)?;

    let vm = pool
        .vms
        .iter_mut()
        .find(|v| v.vm_name == vm_name)
        .ok_or_else(|| Error::VmNotInPool {
            vm_name: vm_name.to_string(),
            pool_name: pool_name.to_string(),
        })?;

    vm.status = PoolVmStatus::Available;
    vm.task_id = None;
    vm.assigned_at = None;
    vm.completed_at = None;

    save(store, &pool)
}

/// Generate VM names for a pool: `<pool>-1`, `<pool>-2`, etc.
pub fn vm_names(pool_name: &str, count: u32) -> Vec<String> {
    (1..=count).map(|i| format!("{pool_name}-{i}")).collect()
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    fn test_store() -> (tempfile::TempDir, StateStore) {
        let dir = tempfile::tempdir().unwrap();
        let store = StateStore::new(dir.path().to_path_buf());
        store.init().unwrap();
        (dir, store)
    }

    fn sample_pool(name: &str, count: u32) -> PoolState {
        PoolState {
            name: name.to_string(),
            image: "alpine:latest".to_string(),
            vms: vm_names(name, count)
                .into_iter()
                .map(|vm_name| PoolVm {
                    vm_name,
                    status: PoolVmStatus::Available,
                    task_id: None,
                    assigned_at: None,
                    completed_at: None,
                })
                .collect(),
            created_at: "2026-01-01T00:00:00Z".to_string(),
        }
    }

    #[test]
    fn save_and_load() {
        let (_dir, store) = test_store();
        let pool = sample_pool("mypool", 3);

        save(&store, &pool).unwrap();
        let loaded = load(&store, "mypool").unwrap();
        assert_eq!(loaded, pool);
        assert_eq!(loaded.vms.len(), 3);
        assert_eq!(loaded.vms[0].vm_name, "mypool-1");
    }

    #[test]
    fn load_nonexistent_returns_not_found() {
        let (_dir, store) = test_store();
        let err = load(&store, "nope").unwrap_err();
        assert!(matches!(err, Error::PoolNotFound { name } if name == "nope"));
    }

    #[test]
    fn exists_check() {
        let (_dir, store) = test_store();
        assert!(!exists(&store, "mypool"));

        save(&store, &sample_pool("mypool", 2)).unwrap();
        assert!(exists(&store, "mypool"));
    }

    #[test]
    fn assign_picks_first_available() {
        let (_dir, store) = test_store();
        save(&store, &sample_pool("pool", 3)).unwrap();

        let assigned = assign(&store, "pool", "SEC-1").unwrap();
        assert_eq!(assigned.vm_name, "pool-1");
        assert_eq!(assigned.task_id, Some("SEC-1".to_string()));
        assert_eq!(assigned.status, PoolVmStatus::Assigned);

        let assigned2 = assign(&store, "pool", "SEC-2").unwrap();
        assert_eq!(assigned2.vm_name, "pool-2");
    }

    #[test]
    fn assign_pool_full() {
        let (_dir, store) = test_store();
        save(&store, &sample_pool("pool", 1)).unwrap();

        assign(&store, "pool", "SEC-1").unwrap();
        let err = assign(&store, "pool", "SEC-2").unwrap_err();
        assert!(matches!(err, Error::PoolFull { name } if name == "pool"));
    }

    #[test]
    fn complete_marks_success() {
        let (_dir, store) = test_store();
        save(&store, &sample_pool("pool", 2)).unwrap();
        assign(&store, "pool", "SEC-1").unwrap();

        complete(&store, "pool", "pool-1", false).unwrap();
        let pool = load(&store, "pool").unwrap();
        assert_eq!(pool.vms[0].status, PoolVmStatus::Completed);
        assert!(pool.vms[0].completed_at.is_some());
    }

    #[test]
    fn complete_marks_failure() {
        let (_dir, store) = test_store();
        save(&store, &sample_pool("pool", 2)).unwrap();
        assign(&store, "pool", "SEC-1").unwrap();

        complete(&store, "pool", "pool-1", true).unwrap();
        let pool = load(&store, "pool").unwrap();
        assert_eq!(pool.vms[0].status, PoolVmStatus::Failed);
    }

    #[test]
    fn complete_unknown_vm_errors() {
        let (_dir, store) = test_store();
        save(&store, &sample_pool("pool", 1)).unwrap();

        let err = complete(&store, "pool", "pool-99", false).unwrap_err();
        assert!(matches!(err, Error::VmNotInPool { .. }));
    }

    #[test]
    fn release_resets_vm() {
        let (_dir, store) = test_store();
        save(&store, &sample_pool("pool", 2)).unwrap();
        assign(&store, "pool", "SEC-1").unwrap();
        complete(&store, "pool", "pool-1", false).unwrap();

        release(&store, "pool", "pool-1").unwrap();
        let pool = load(&store, "pool").unwrap();
        assert_eq!(pool.vms[0].status, PoolVmStatus::Available);
        assert!(pool.vms[0].task_id.is_none());
        assert!(pool.vms[0].assigned_at.is_none());
        assert!(pool.vms[0].completed_at.is_none());
    }

    #[test]
    fn release_makes_vm_reassignable() {
        let (_dir, store) = test_store();
        save(&store, &sample_pool("pool", 1)).unwrap();

        assign(&store, "pool", "SEC-1").unwrap();
        complete(&store, "pool", "pool-1", false).unwrap();
        release(&store, "pool", "pool-1").unwrap();

        // Should be assignable again.
        let assigned = assign(&store, "pool", "SEC-2").unwrap();
        assert_eq!(assigned.vm_name, "pool-1");
        assert_eq!(assigned.task_id, Some("SEC-2".to_string()));
    }

    #[test]
    fn vm_names_generates_correct_names() {
        let names = vm_names("mypool", 3);
        assert_eq!(names, vec!["mypool-1", "mypool-2", "mypool-3"]);
    }

    #[test]
    fn list_empty() {
        let (_dir, store) = test_store();
        let pools = list(&store).unwrap();
        assert!(pools.is_empty());
    }

    #[test]
    fn list_multiple_pools() {
        let (_dir, store) = test_store();
        save(&store, &sample_pool("beta", 2)).unwrap();
        save(&store, &sample_pool("alpha", 3)).unwrap();

        let pools = list(&store).unwrap();
        assert_eq!(pools.len(), 2);
        assert_eq!(pools[0].name, "alpha");
        assert_eq!(pools[1].name, "beta");
    }

    #[test]
    fn delete_pool() {
        let (_dir, store) = test_store();
        save(&store, &sample_pool("pool", 2)).unwrap();
        assert!(exists(&store, "pool"));

        delete(&store, "pool").unwrap();
        assert!(!exists(&store, "pool"));
    }

    #[test]
    fn delete_idempotent() {
        let (_dir, store) = test_store();
        delete(&store, "nope").unwrap();
    }

    #[test]
    fn json_format() {
        let pool = sample_pool("mypool", 2);
        let json: serde_json::Value = serde_json::to_value(&pool).unwrap();

        assert_eq!(json["name"], "mypool");
        assert_eq!(json["image"], "alpine:latest");
        assert_eq!(json["vms"].as_array().unwrap().len(), 2);
        assert_eq!(json["vms"][0]["vm_name"], "mypool-1");
        assert_eq!(json["vms"][0]["status"], "available");
        // task_id/assigned_at/completed_at should be absent (skip_serializing_if)
        assert!(json["vms"][0].get("task_id").is_none());
    }

    #[test]
    fn status_display() {
        assert_eq!(PoolVmStatus::Available.to_string(), "available");
        assert_eq!(PoolVmStatus::Assigned.to_string(), "assigned");
        assert_eq!(PoolVmStatus::Completed.to_string(), "completed");
        assert_eq!(PoolVmStatus::Failed.to_string(), "failed");
    }
}

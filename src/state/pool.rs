//! Pool metadata types and state tracking.
//!
//! A pool is a named group of VMs created from the same image.
//! Pool metadata is stored at `<state-dir>/pools/<name>/pool.json`.

use std::path::PathBuf;

use serde::{Deserialize, Serialize};

use crate::error::{Error, Result};
use crate::state::store::StateStore;

/// Metadata for a named pool of VMs.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PoolMetadata {
    /// Pool name (unique identifier).
    pub name: String,
    /// Image reference used to create all VMs in this pool.
    pub image: String,
    /// Number of VMs requested at creation time.
    pub count: u32,
    /// Names of VMs belonging to this pool.
    pub vms: Vec<String>,
    /// ISO 8601 creation timestamp.
    pub created_at: String,
}

/// Summary of a pool VM for status output.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PoolVmStatus {
    pub vm_name: String,
    pub status: String,
    pub pid: Option<u32>,
    pub guest_ip: Option<String>,
}

/// Load pool metadata from the state store.
///
/// Returns an error if the pool does not exist.
pub fn load(store: &StateStore, name: &str) -> Result<PoolMetadata> {
    let path = pool_metadata_path(store, name);
    store
        .read_optional(&path)?
        .ok_or_else(|| Error::State(format!("pool '{name}' not found")))
}

/// Save pool metadata to the state store.
pub fn save(store: &StateStore, pool: &PoolMetadata) -> Result<()> {
    let path = pool_metadata_path(store, &pool.name);
    store.write(&path, pool)
}

/// List all pools by reading metadata from each subdirectory under `pools/`.
pub fn list(store: &StateStore) -> Result<Vec<PoolMetadata>> {
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
        if let Ok(Some(pool)) = store.read_optional::<PoolMetadata>(&meta_path) {
            pools.push(pool);
        }
    }

    pools.sort_by(|a, b| a.name.cmp(&b.name));
    Ok(pools)
}

/// Check whether a pool exists in the state store.
pub fn exists(store: &StateStore, name: &str) -> bool {
    pool_metadata_path(store, name).exists()
}

/// Delete a pool's state directory and all files within it.
///
/// Idempotent — succeeds even if the directory is already gone.
pub fn delete(store: &StateStore, name: &str) -> Result<()> {
    let dir = pool_dir(store, name);
    store.remove_dir(&dir)
}

/// Directory for a specific pool's state files.
pub fn pool_dir(store: &StateStore, name: &str) -> PathBuf {
    store.root().join("pools").join(name)
}

/// Path to a pool's metadata file.
fn pool_metadata_path(store: &StateStore, name: &str) -> PathBuf {
    pool_dir(store, name).join("pool.json")
}

/// Generate VM names for a pool: `<pool>-1`, `<pool>-2`, etc.
pub fn vm_names(pool_name: &str, count: u32) -> Vec<String> {
    (1..=count).map(|i| format!("{pool_name}-{i}")).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn test_store() -> (tempfile::TempDir, StateStore) {
        let dir = tempfile::tempdir().unwrap();
        let store = StateStore::new(dir.path().to_path_buf());
        store.init().unwrap();
        (dir, store)
    }

    fn sample_pool(name: &str) -> PoolMetadata {
        PoolMetadata {
            name: name.to_string(),
            image: "agent-base:latest".to_string(),
            count: 3,
            vms: vm_names(name, 3),
            created_at: "2026-04-06T00:00:00Z".to_string(),
        }
    }

    #[test]
    fn save_and_load() {
        let (_dir, store) = test_store();
        let pool = sample_pool("mypool");
        save(&store, &pool).unwrap();
        let loaded = load(&store, "mypool").unwrap();
        assert_eq!(loaded.name, "mypool");
        assert_eq!(loaded.count, 3);
        assert_eq!(loaded.vms.len(), 3);
    }

    #[test]
    fn load_nonexistent() {
        let (_dir, store) = test_store();
        let err = load(&store, "nope").unwrap_err();
        assert!(matches!(err, Error::State(_)));
    }

    #[test]
    fn list_empty() {
        let (_dir, store) = test_store();
        let pools = list(&store).unwrap();
        assert!(pools.is_empty());
    }

    #[test]
    fn list_multiple() {
        let (_dir, store) = test_store();
        save(&store, &sample_pool("beta")).unwrap();
        save(&store, &sample_pool("alpha")).unwrap();
        let pools = list(&store).unwrap();
        assert_eq!(pools.len(), 2);
        assert_eq!(pools[0].name, "alpha");
        assert_eq!(pools[1].name, "beta");
    }

    #[test]
    fn exists_check() {
        let (_dir, store) = test_store();
        assert!(!exists(&store, "mypool"));
        save(&store, &sample_pool("mypool")).unwrap();
        assert!(exists(&store, "mypool"));
    }

    #[test]
    fn delete_removes_pool() {
        let (_dir, store) = test_store();
        save(&store, &sample_pool("mypool")).unwrap();
        assert!(exists(&store, "mypool"));
        delete(&store, "mypool").unwrap();
        assert!(!exists(&store, "mypool"));
    }

    #[test]
    fn delete_idempotent() {
        let (_dir, store) = test_store();
        delete(&store, "nope").unwrap();
    }

    #[test]
    fn vm_names_format() {
        let names = vm_names("test-pool", 3);
        assert_eq!(names, vec!["test-pool-1", "test-pool-2", "test-pool-3"]);
    }
}

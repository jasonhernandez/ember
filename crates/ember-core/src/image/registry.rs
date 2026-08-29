//! Local image registry tracking.
//!
//! Tracks metadata about images (pulled from OCI registries or built
//! locally from Dockerfiles) that have been imported into storage.
//! Persisted as `registry.json` in the state directory via [`StateStore`].

use serde::{Deserialize, Serialize};

use crate::config::{GlobalConfig, StorageKind};
use crate::error::{Error, Result};
use crate::image::pull::ImageReference;
use crate::state::store::StateStore;

/// A single image entry in the local registry.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ImageEntry {
    /// Full image reference (e.g. `docker.io/library/alpine:latest`).
    pub reference: String,
    /// Filesystem-safe local name (e.g. `library-alpine-latest`).
    pub local_name: String,
    /// Where the backing storage lived **when the image was created**.
    ///
    /// Linux/ZFS: zvol path (e.g. `tank/ember/images/library-alpine-latest`).
    /// macOS/APFS: `.img` file path (e.g. `~/Library/.../images/data/library-alpine-latest.img`).
    ///
    /// # Display and diagnostics only — never authoritative
    ///
    /// Nothing that creates, opens, or destroys storage may read this
    /// field. Backends derive paths from the *current* `GlobalConfig`
    /// (`LinuxStorage::image_zvol`, `DmThinStorage`'s dm names,
    /// `MacosStorage::image_path`); this string is only rendered by
    /// `ember image inspect` and compared against the configured
    /// location by [`ImageRegistry::stale_entries`].
    ///
    /// The reason is that the two can disagree, and the recorded value
    /// is the stale one. `ember init` can be re-run against a different
    /// pool, dataset, or state directory while `registry.json` is kept;
    /// every entry then still names the *old* location. A destroy path
    /// that honoured `disk_path` would reach outside the configured
    /// pool and wipe datasets the operator may be holding as their only
    /// rollback — during one such migration, three `ember image delete`
    /// attempts would have destroyed 25 GB in the pool being kept.
    /// Deriving from config makes the blast radius of any storage
    /// operation exactly the pool ember is configured to own.
    ///
    /// If you need the real location, ask the backend. If you need to
    /// know whether an entry is stale, use
    /// [`ImageRegistry::stale_entries`] — that is a read, and a warning,
    /// not a target.
    #[serde(alias = "zvol")]
    pub disk_path: String,
    /// Disk size in MiB.
    pub size_mib: u64,
    /// ISO 8601 timestamp when the image was pulled.
    pub pulled_at: String,
    /// dm-thin base snapshot id. `None` for other backends.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub thin_id: Option<u64>,
}

/// A registry entry whose recorded storage location disagrees with the
/// active config, paired with the location the config implies.
///
/// Returned by [`ImageRegistry::stale_entries`] so callers can report
/// both halves of the mismatch.
#[derive(Debug, Clone, PartialEq)]
pub struct StaleEntry<'a> {
    /// The registry entry, with its recorded (old) `disk_path`.
    pub entry: &'a ImageEntry,
    /// Where the current config would place this image.
    pub expected: String,
}

/// The local image registry: a list of pulled images.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct ImageRegistry {
    pub images: Vec<ImageEntry>,
}

impl ImageRegistry {
    /// Load the registry from the state store, returning an empty registry
    /// if the file doesn't exist yet.
    pub fn load(store: &StateStore) -> Result<Self> {
        let path = store.image_registry_path();
        store
            .read_optional(&path)
            .map(|opt| opt.unwrap_or_default())
    }

    /// Save the registry to the state store.
    pub fn save(&self, store: &StateStore) -> Result<()> {
        let path = store.image_registry_path();
        store.write(&path, self)
    }

    /// Add an image entry. Replaces any existing entry with the same local name.
    pub fn add(&mut self, entry: ImageEntry) {
        self.remove(&entry.local_name);
        self.images.push(entry);
    }

    /// Remove an image by its local name. Returns the removed entry, if any.
    pub fn remove(&mut self, local_name: &str) -> Option<ImageEntry> {
        let pos = self
            .images
            .iter()
            .position(|e| e.local_name == local_name)?;
        Some(self.images.remove(pos))
    }

    /// Look up an image by local name.
    pub fn get(&self, local_name: &str) -> Option<&ImageEntry> {
        self.images.iter().find(|e| e.local_name == local_name)
    }

    /// Check whether an image with this local name is registered.
    pub fn exists(&self, local_name: &str) -> bool {
        self.get(local_name).is_some()
    }

    /// Find an image by a user-provided reference string.
    ///
    /// First tries parsing as an OCI reference and looking up by its
    /// local name (so `alpine` finds `library-alpine-latest`).  If that
    /// doesn't match, falls back to a direct local-name lookup so that
    /// locally built images (e.g. `ubuntu-vm`) can be found too.
    pub fn find_by_reference(&self, reference: &str) -> Result<Option<&ImageEntry>> {
        let parsed = ImageReference::parse(reference)?;
        if let Some(entry) = self.get(&parsed.local_name()) {
            return Ok(Some(entry));
        }
        // Fall back to direct local_name match (for locally built images).
        Ok(self.get(reference))
    }

    /// Entries whose recorded `disk_path` does not match where `config`
    /// would put them today.
    ///
    /// This is the drift left behind by re-running `ember init` against
    /// a different pool or dataset while keeping `registry.json`: the
    /// entries survive, `ember image list` keeps advertising them, but
    /// their data is in the pool that was left behind. Purely
    /// diagnostic — callers warn, they do not delete (and in
    /// particular must not delete *the path named here*; see
    /// [`ImageEntry::disk_path`]).
    ///
    /// Only meaningful for ZFS, where `disk_path` is a dataset name
    /// derived from pool + dataset and therefore comparable to what
    /// config yields. dm-thin and macOS encode instance ids and state
    /// directories into their paths, so a mismatch there is not
    /// reliably a pool change; those backends return an empty list
    /// rather than a guess.
    ///
    /// Skipped entries: an empty `disk_path` (cleanup stubs), and any
    /// absolute path. A ZFS dataset name never starts with `/`, while
    /// macOS `.img` files and dm-thin `/dev/mapper` nodes always do —
    /// and macOS configs carry the default `StorageKind::Zfs`, so the
    /// backend field alone would not keep them out.
    pub fn stale_entries<'a>(&'a self, config: &GlobalConfig) -> Vec<StaleEntry<'a>> {
        if config.storage_backend != StorageKind::Zfs {
            return Vec::new();
        }
        self.images
            .iter()
            .filter(|e| !e.disk_path.is_empty() && !e.disk_path.starts_with('/'))
            .filter_map(|entry| {
                let expected = format!("{}/{}", config.images_dataset(), entry.local_name);
                (entry.disk_path != expected).then_some(StaleEntry { entry, expected })
            })
            .collect()
    }

    /// Number of tracked images.
    pub fn len(&self) -> usize {
        self.images.len()
    }

    /// Whether the registry is empty.
    pub fn is_empty(&self) -> bool {
        self.images.is_empty()
    }
}

/// Build an [`ImageEntry`] from a pull result.
pub fn new_entry(
    reference: &ImageReference,
    disk_path: &str,
    size_mib: u64,
    thin_id: Option<u64>,
) -> ImageEntry {
    ImageEntry {
        reference: reference.to_string(),
        local_name: reference.local_name(),
        disk_path: disk_path.to_string(),
        size_mib,
        pulled_at: now_iso8601(),
        thin_id,
    }
}

/// Build an [`ImageEntry`] for a locally built image.
///
/// The reference is stored as `local:<name>` to distinguish built
/// images from pulled ones in `ember image list` output.
pub fn new_build_entry(
    name: &str,
    local_name: &str,
    disk_path: &str,
    size_mib: u64,
    thin_id: Option<u64>,
) -> ImageEntry {
    ImageEntry {
        reference: format!("local:{name}"),
        local_name: local_name.to_string(),
        disk_path: disk_path.to_string(),
        size_mib,
        pulled_at: now_iso8601(),
        thin_id,
    }
}

/// Current UTC time as an ISO 8601 string (second precision).
fn now_iso8601() -> String {
    crate::state::vm::now_iso8601()
}

/// Load the registry, remove an entry by local name, save, and return
/// the removed entry. Returns [`Error::ImageNotFound`] if not present.
pub fn remove_image(store: &StateStore, local_name: &str) -> Result<ImageEntry> {
    let mut registry = ImageRegistry::load(store)?;
    let entry = registry
        .remove(local_name)
        .ok_or_else(|| Error::ImageNotFound {
            name: local_name.to_string(),
        })?;
    registry.save(store)?;
    Ok(entry)
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

    fn sample_entry(name: &str) -> ImageEntry {
        ImageEntry {
            reference: format!("docker.io/library/{name}:latest"),
            local_name: format!("library-{name}-latest"),
            disk_path: format!("tank/ember/images/library-{name}-latest"),
            size_mib: 64,
            pulled_at: "2026-01-01T00:00:00Z".to_string(),
            thin_id: None,
        }
    }

    #[test]
    fn empty_registry_loads_when_no_file() {
        let (_dir, store) = test_store();
        let reg = ImageRegistry::load(&store).unwrap();
        assert!(reg.is_empty());
        assert_eq!(reg.len(), 0);
    }

    #[test]
    fn add_and_get() {
        let mut reg = ImageRegistry::default();
        let entry = sample_entry("alpine");
        reg.add(entry.clone());

        assert_eq!(reg.len(), 1);
        assert!(reg.exists("library-alpine-latest"));
        assert_eq!(reg.get("library-alpine-latest"), Some(&entry));
    }

    #[test]
    fn add_replaces_existing() {
        let mut reg = ImageRegistry::default();
        let entry1 = ImageEntry {
            size_mib: 64,
            ..sample_entry("alpine")
        };
        let entry2 = ImageEntry {
            size_mib: 128,
            ..sample_entry("alpine")
        };

        reg.add(entry1);
        reg.add(entry2.clone());

        assert_eq!(reg.len(), 1);
        assert_eq!(reg.get("library-alpine-latest").unwrap().size_mib, 128);
    }

    #[test]
    fn remove_returns_entry() {
        let mut reg = ImageRegistry::default();
        reg.add(sample_entry("alpine"));

        let removed = reg.remove("library-alpine-latest");
        assert!(removed.is_some());
        assert!(reg.is_empty());
    }

    #[test]
    fn remove_nonexistent_returns_none() {
        let mut reg = ImageRegistry::default();
        assert!(reg.remove("nope").is_none());
    }

    #[test]
    fn find_by_reference_parses_shorthand() {
        let mut reg = ImageRegistry::default();
        reg.add(sample_entry("alpine"));

        // "alpine" should resolve to docker.io/library/alpine:latest
        let found = reg.find_by_reference("alpine").unwrap();
        assert!(found.is_some());
        assert_eq!(found.unwrap().local_name, "library-alpine-latest");
    }

    #[test]
    fn find_by_reference_missing() {
        let reg = ImageRegistry::default();
        let found = reg.find_by_reference("ubuntu").unwrap();
        assert!(found.is_none());
    }

    #[test]
    fn round_trip_through_state_store() {
        let (_dir, store) = test_store();

        let mut reg = ImageRegistry::default();
        reg.add(sample_entry("alpine"));
        reg.add(sample_entry("ubuntu"));
        reg.save(&store).unwrap();

        let loaded = ImageRegistry::load(&store).unwrap();
        assert_eq!(loaded, reg);
        assert_eq!(loaded.len(), 2);
    }

    #[test]
    fn save_and_reload_preserves_data() {
        let (_dir, store) = test_store();

        // Save, load, modify, save, load — verify consistency.
        let mut reg = ImageRegistry::default();
        reg.add(sample_entry("alpine"));
        reg.save(&store).unwrap();

        let mut reg2 = ImageRegistry::load(&store).unwrap();
        reg2.add(sample_entry("ubuntu"));
        reg2.save(&store).unwrap();

        let final_reg = ImageRegistry::load(&store).unwrap();
        assert_eq!(final_reg.len(), 2);
        assert!(final_reg.exists("library-alpine-latest"));
        assert!(final_reg.exists("library-ubuntu-latest"));
    }

    #[test]
    fn new_entry_builds_correctly() {
        let reference = ImageReference::parse("alpine:3.19").unwrap();
        let entry = new_entry(
            &reference,
            "tank/ember/images/library-alpine-3.19",
            96,
            None,
        );

        assert_eq!(entry.reference, "docker.io/library/alpine:3.19");
        assert_eq!(entry.local_name, "library-alpine-3.19");
        assert_eq!(entry.disk_path, "tank/ember/images/library-alpine-3.19");
        assert_eq!(entry.size_mib, 96);
        assert!(!entry.pulled_at.is_empty());
        assert_eq!(entry.thin_id, None);
    }

    #[test]
    fn remove_image_from_store() {
        let (_dir, store) = test_store();

        let mut reg = ImageRegistry::default();
        reg.add(sample_entry("alpine"));
        reg.save(&store).unwrap();

        let removed = remove_image(&store, "library-alpine-latest").unwrap();
        assert_eq!(removed.local_name, "library-alpine-latest");

        let loaded = ImageRegistry::load(&store).unwrap();
        assert!(loaded.is_empty());
    }

    #[test]
    fn remove_image_not_found() {
        let (_dir, store) = test_store();
        let result = remove_image(&store, "nope");
        assert!(result.is_err());
    }

    #[test]
    fn json_format() {
        let entry = sample_entry("alpine");
        let json: serde_json::Value = serde_json::to_value(&entry).unwrap();

        assert_eq!(json["reference"], "docker.io/library/alpine:latest");
        assert_eq!(json["local_name"], "library-alpine-latest");
        assert_eq!(json["disk_path"], "tank/ember/images/library-alpine-latest");
        assert_eq!(json["size_mib"], 64);
        assert_eq!(json["pulled_at"], "2026-01-01T00:00:00Z");
    }

    fn zfs_config(pool: &str, dataset: &str) -> GlobalConfig {
        GlobalConfig {
            storage_backend: StorageKind::Zfs,
            pool: pool.to_string(),
            dataset: dataset.to_string(),
            kernel_path: None,
            wan_iface: None,
            state_dir: std::path::PathBuf::default(),
            instance_id: "abcd".to_string(),
            ip_subnet: "10.100.0.0/16".to_string(),
            storage_path: None,
            dm_thin_block_size: None,
            dm_thin_mode: None,
        }
    }

    /// An entry recorded under the pool the operator just migrated away
    /// from is reported, with the location the new config implies.
    #[test]
    fn stale_entries_flags_foreign_pool() {
        let mut reg = ImageRegistry::default();
        reg.add(ImageEntry {
            disk_path: "manypool/ember/images/library-alpine-latest".to_string(),
            ..sample_entry("alpine")
        });

        let stale = reg.stale_entries(&zfs_config("ember", "ember"));
        assert_eq!(stale.len(), 1);
        assert_eq!(stale[0].entry.local_name, "library-alpine-latest");
        assert_eq!(
            stale[0].expected,
            "ember/ember/images/library-alpine-latest"
        );
    }

    /// A different *dataset* in the same pool is drift too.
    #[test]
    fn stale_entries_flags_foreign_dataset() {
        let mut reg = ImageRegistry::default();
        reg.add(ImageEntry {
            disk_path: "tank/old/images/library-alpine-latest".to_string(),
            ..sample_entry("alpine")
        });

        let stale = reg.stale_entries(&zfs_config("tank", "ember"));
        assert_eq!(stale.len(), 1);
    }

    #[test]
    fn stale_entries_silent_when_paths_match() {
        let mut reg = ImageRegistry::default();
        // sample_entry records tank/ember/images/<local_name>.
        reg.add(sample_entry("alpine"));
        reg.add(sample_entry("ubuntu"));

        assert!(reg.stale_entries(&zfs_config("tank", "ember")).is_empty());
    }

    #[test]
    fn stale_entries_skips_empty_disk_path() {
        let mut reg = ImageRegistry::default();
        reg.add(ImageEntry {
            disk_path: String::new(),
            ..sample_entry("alpine")
        });

        assert!(reg.stale_entries(&zfs_config("ember", "ember")).is_empty());
    }

    /// macOS records an absolute `.img` path while its config still
    /// carries the default `StorageKind::Zfs` — that must not read as
    /// a pool mismatch.
    #[test]
    fn stale_entries_skips_absolute_paths() {
        let mut reg = ImageRegistry::default();
        reg.add(ImageEntry {
            disk_path: "/Users/me/Library/ember/images/data/library-alpine-latest.img".to_string(),
            ..sample_entry("alpine")
        });

        assert!(reg.stale_entries(&zfs_config("ember", "ember")).is_empty());
    }

    /// dm-thin paths encode instance ids, not pool names — a mismatch
    /// there is not evidence of a pool change, so we stay quiet.
    #[test]
    fn stale_entries_empty_for_non_zfs_backends() {
        let mut reg = ImageRegistry::default();
        reg.add(ImageEntry {
            disk_path: "/dev/mapper/ember-abcd-image-library-alpine-latest".to_string(),
            ..sample_entry("alpine")
        });

        let config = GlobalConfig {
            storage_backend: StorageKind::DmThin,
            ..zfs_config("ember", "ember")
        };
        assert!(reg.stale_entries(&config).is_empty());
    }

    #[test]
    fn registry_json_format() {
        let mut reg = ImageRegistry::default();
        reg.add(sample_entry("alpine"));

        let json: serde_json::Value = serde_json::to_value(&reg).unwrap();
        assert!(json["images"].is_array());
        assert_eq!(json["images"].as_array().unwrap().len(), 1);
    }
}

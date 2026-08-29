pub mod dataset;
pub mod pool;
pub mod snapshot;
pub mod volume;

use std::process::Command;

use ember_core::error::{Error, Result};

/// The reserved snapshot name used for image cloning.
///
/// Every image zvol has a `@base` snapshot that serves as the clone source
/// for per-VM zvols. This name is checked in snapshot create/delete commands
/// and filtered from user-facing snapshot listings.
pub const BASE_SNAPSHOT_NAME: &str = "base";

/// Result of a destroy that tolerates an already-absent target.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum DestroyOutcome {
    /// The dataset existed and `zfs destroy` removed it.
    Destroyed,
    /// The dataset was already gone — nothing to do.
    AlreadyAbsent,
}

/// Run `zfs destroy` on a dataset, volume, or snapshot.
///
/// With `recursive: true`, passes `-r` to also destroy children and snapshots.
/// With `force_dependents: true`, passes `-R` to also destroy dependent clones
/// (e.g., VM zvols cloned from an image snapshot). `-R` implies `-r`.
pub(crate) fn destroy(name: &str, recursive: bool) -> Result<()> {
    destroy_impl(name, recursive, false)
}

/// Like [`destroy`], but an already-absent dataset is success.
///
/// See [`tolerate_missing`] for how "already gone" is told apart from a
/// real failure.
pub(crate) fn destroy_if_present(name: &str, recursive: bool) -> Result<DestroyOutcome> {
    tolerate_missing(name, destroy_impl(name, recursive, false))
}

/// Like [`destroy_if_present`], but with `-R` to also destroy dependent clones.
pub(crate) fn destroy_with_dependents_if_present(name: &str) -> Result<DestroyOutcome> {
    tolerate_missing(name, destroy_impl(name, false, true))
}

/// Map a `zfs destroy` failure that means "the target is already gone"
/// onto success, and let every other failure through unchanged.
///
/// Deletion is idempotent by intent: a missing dataset is the *desired*
/// end state, so the caller should be free to move on and clean up its
/// own bookkeeping. Blanket-swallowing errors would be wrong, though —
/// a busy dataset, a permission error, or dependent clones without
/// `-R` all mean the data is still there and the caller must abort.
///
/// Two independent signals must agree before we call it absent:
///
/// 1. libzfs printed its exact "not there" line for the *name we
///    passed* — see [`reports_missing_dataset`].
/// 2. A follow-up `zfs list` confirms nothing by that name exists.
///
/// The probe runs *after* the destroy attempt rather than instead of
/// it, so we never skip the destroy on the strength of a racy
/// existence check; it only ever downgrades an error we have already
/// classified. It fails closed: if the probe itself cannot answer, we
/// assume the dataset is present and return the original error.
fn tolerate_missing(name: &str, result: Result<()>) -> Result<DestroyOutcome> {
    match result {
        Ok(()) => Ok(DestroyOutcome::Destroyed),
        Err(Error::Command {
            command,
            exit_code,
            stderr,
        }) => {
            if reports_missing_dataset(&stderr, name) && !dataset_present(name) {
                Ok(DestroyOutcome::AlreadyAbsent)
            } else {
                Err(Error::Command {
                    command,
                    exit_code,
                    stderr,
                })
            }
        }
        Err(e) => Err(e),
    }
}

/// True when `zfs` reported that `name` itself does not exist.
///
/// libzfs has one phrasing for this: `cannot open '<name>': dataset does
/// not exist`. Matching the whole line — verb, quoted name, and reason —
/// keeps three classes of real failure from being mistaken for it:
///
/// * different reason, same shape: `cannot destroy '<name>': dataset is
///   busy`, `... permission denied`, `... volume has children`;
/// * dependent clones: `cannot destroy '<name>@base': snapshot has
///   dependent clones` (plus the `use '-R' to destroy ...` list);
/// * a message about *some other* dataset — e.g. an `-R` run that lists
///   datasets it could not open — since the quoted name must be the one
///   we asked to destroy.
fn reports_missing_dataset(stderr: &str, name: &str) -> bool {
    let expected = format!("cannot open '{name}': dataset does not exist");
    stderr.lines().any(|line| line.trim() == expected)
}

/// Second opinion on whether `name` exists, failing closed.
///
/// Any inability to answer (probe could not run) is reported as
/// "present" so [`tolerate_missing`] propagates the original error
/// instead of silently dropping it.
fn dataset_present(name: &str) -> bool {
    dataset::exists(name).unwrap_or(true)
}

fn destroy_impl(name: &str, recursive: bool, force_dependents: bool) -> Result<()> {
    let mut args = vec!["destroy"];
    if force_dependents {
        args.push("-R");
    } else if recursive {
        args.push("-r");
    }
    args.push(name);

    let output = Command::new("zfs")
        .args(&args)
        .output()
        .map_err(|e| Error::CommandExec {
            command: "zfs destroy".to_string(),
            source: e,
        })?;

    Error::check_command("zfs destroy", output)?;
    Ok(())
}

/// Parse a numeric string from ZFS tab-separated output into a `u64`.
///
/// Used across all ZFS modules to parse byte counts, timestamps, and
/// other numeric fields from `zfs list` / `zpool list` output.
pub(crate) fn parse_u64(s: &str, field: &str) -> Result<u64> {
    s.trim()
        .parse::<u64>()
        .map_err(|_| Error::Zfs(format!("cannot parse {field} value: {s}")))
}

#[cfg(test)]
mod tests {
    use super::*;

    const ZVOL: &str = "manypool/ember/images/ubuntu-dev";

    fn command_error(stderr: &str) -> Error {
        Error::Command {
            command: "zfs destroy".to_string(),
            exit_code: 1,
            stderr: stderr.to_string(),
        }
    }

    #[test]
    fn missing_dataset_message_is_recognized() {
        // Verbatim from `zfs destroy` on a pool that no longer holds
        // the image (the ember image delete bug).
        let stderr = format!("cannot open '{ZVOL}': dataset does not exist");
        assert!(reports_missing_dataset(&stderr, ZVOL));
    }

    #[test]
    fn missing_dataset_message_recognized_among_other_lines() {
        let stderr =
            format!("cannot open '{ZVOL}': dataset does not exist\ncould not find any snapshots to destroy; check snapshot names.");
        assert!(reports_missing_dataset(&stderr, ZVOL));
    }

    #[test]
    fn busy_dataset_is_not_missing() {
        let stderr = format!("cannot destroy '{ZVOL}': dataset is busy");
        assert!(!reports_missing_dataset(&stderr, ZVOL));
    }

    #[test]
    fn permission_denied_is_not_missing() {
        let stderr = format!("cannot destroy '{ZVOL}': permission denied");
        assert!(!reports_missing_dataset(&stderr, ZVOL));
    }

    #[test]
    fn dependent_clones_are_not_missing() {
        let stderr = format!(
            "cannot destroy '{ZVOL}@base': snapshot has dependent clones\n\
             use '-R' to destroy the following datasets:\n\
             manypool/ember/vms/agent-1"
        );
        assert!(!reports_missing_dataset(&stderr, ZVOL));
    }

    #[test]
    fn missing_message_about_another_dataset_is_not_missing() {
        // An `-R` run can name datasets other than the one we asked to
        // destroy; only our own target counts.
        let stderr = "cannot open 'manypool/ember/vms/agent-1': dataset does not exist";
        assert!(!reports_missing_dataset(stderr, ZVOL));
    }

    #[test]
    fn no_such_pool_is_not_missing() {
        // An exported / renamed pool is not the same as a destroyed
        // dataset — the data may still be on disk.
        let stderr = "cannot open 'manypool': no such pool";
        assert!(!reports_missing_dataset(stderr, ZVOL));
    }

    #[test]
    fn successful_destroy_reports_destroyed() {
        assert_eq!(
            tolerate_missing(ZVOL, Ok(())).unwrap(),
            DestroyOutcome::Destroyed
        );
    }

    #[test]
    fn real_failure_propagates() {
        let err = tolerate_missing(ZVOL, Err(command_error("cannot destroy: dataset is busy")))
            .unwrap_err();
        assert!(err.to_string().contains("dataset is busy"));
    }

    #[test]
    fn spawn_failure_propagates() {
        let err = tolerate_missing(
            ZVOL,
            Err(Error::CommandExec {
                command: "zfs destroy".to_string(),
                source: std::io::Error::from(std::io::ErrorKind::NotFound),
            }),
        )
        .unwrap_err();
        assert!(matches!(err, Error::CommandExec { .. }));
    }
}

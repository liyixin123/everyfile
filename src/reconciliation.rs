use std::path::{Path, PathBuf};
use std::time::Duration;

use crate::index::{IndexStore, VolumeCheckpoint};
use crate::model::{Coverage, RootCoverage};
use crate::projection::SearchProjection;
use crate::scanner::{ScanReport, scan_root};

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub enum CoalescingPreset {
    Responsive,
    #[default]
    Balanced,
    LowEnergy,
}

impl CoalescingPreset {
    pub const fn window(self) -> Duration {
        match self {
            Self::Responsive => Duration::from_secs(1),
            Self::Balanced => Duration::from_secs(5),
            Self::LowEnergy => Duration::from_secs(15),
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct EventBatch {
    pub stream_identity: String,
    pub highest_event_id: u64,
    pub paths: Vec<PathBuf>,
    pub history_lost: bool,
    pub ids_wrapped: bool,
    pub root_changed: bool,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum RecoveryReason {
    NormalReplay,
    MissingOrInvalidCursor,
    StreamIdentityMismatch,
    DroppedHistory,
    EventIdsWrapped,
    RootChanged,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum RecoveryPlan {
    Replay {
        since_event_id: u64,
    },
    RepairSubtrees {
        scopes: Vec<PathBuf>,
        reason: RecoveryReason,
    },
    RebuildVolume {
        reason: RecoveryReason,
    },
}

pub fn plan_stream_start(
    checkpoint: Option<&VolumeCheckpoint>,
    stream_identity: &str,
    published_generation: u64,
) -> RecoveryPlan {
    let Some(checkpoint) = checkpoint else {
        return RecoveryPlan::RebuildVolume {
            reason: RecoveryReason::MissingOrInvalidCursor,
        };
    };
    if checkpoint.stream_identity != stream_identity {
        return RecoveryPlan::RebuildVolume {
            reason: RecoveryReason::StreamIdentityMismatch,
        };
    }
    if checkpoint.event_id == 0
        || checkpoint.event_id == u64::MAX
        || checkpoint.generation != published_generation
    {
        return RecoveryPlan::RebuildVolume {
            reason: RecoveryReason::MissingOrInvalidCursor,
        };
    }
    RecoveryPlan::Replay {
        since_event_id: checkpoint.event_id,
    }
}

pub fn plan_batch(root: &Path, batch: &EventBatch) -> RecoveryPlan {
    if batch.ids_wrapped {
        return RecoveryPlan::RebuildVolume {
            reason: RecoveryReason::EventIdsWrapped,
        };
    }
    let scopes = minimal_safe_scopes(root, &batch.paths);
    if batch.history_lost || batch.root_changed {
        if scopes.is_empty() || scopes.iter().any(|scope| scope == root) {
            return RecoveryPlan::RebuildVolume {
                reason: if batch.root_changed {
                    RecoveryReason::RootChanged
                } else {
                    RecoveryReason::DroppedHistory
                },
            };
        }
        return RecoveryPlan::RepairSubtrees {
            scopes,
            reason: if batch.root_changed {
                RecoveryReason::RootChanged
            } else {
                RecoveryReason::DroppedHistory
            },
        };
    }
    RecoveryPlan::RepairSubtrees {
        scopes,
        reason: RecoveryReason::NormalReplay,
    }
}

#[derive(Default)]
pub struct HintCoalescer {
    paths: Vec<PathBuf>,
    highest_event_id: u64,
    history_lost: bool,
    ids_wrapped: bool,
    root_changed: bool,
    stream_identity: String,
}

impl HintCoalescer {
    pub fn push(&mut self, batch: EventBatch) {
        self.highest_event_id = self.highest_event_id.max(batch.highest_event_id);
        self.history_lost |= batch.history_lost;
        self.ids_wrapped |= batch.ids_wrapped;
        self.root_changed |= batch.root_changed;
        self.stream_identity = batch.stream_identity;
        for path in batch.paths {
            if self.paths.iter().any(|existing| path.starts_with(existing)) {
                continue;
            }
            self.paths.retain(|existing| !existing.starts_with(&path));
            self.paths.push(path);
        }
        self.paths.sort_unstable();
    }

    pub fn has_pending(&self) -> bool {
        self.history_lost || self.ids_wrapped || self.root_changed || !self.paths.is_empty()
    }

    pub fn take(&mut self) -> Option<EventBatch> {
        if !self.has_pending() {
            return None;
        }
        Some(EventBatch {
            stream_identity: std::mem::take(&mut self.stream_identity),
            highest_event_id: std::mem::take(&mut self.highest_event_id),
            paths: std::mem::take(&mut self.paths),
            history_lost: std::mem::take(&mut self.history_lost),
            ids_wrapped: std::mem::take(&mut self.ids_wrapped),
            root_changed: std::mem::take(&mut self.root_changed),
        })
    }
}

pub struct ReconciledIndex {
    pub projection: SearchProjection,
    pub coverage: Coverage,
    pub generation: u64,
    pub coverage_report: RootCoverage,
}

pub fn reconcile_committed_root(
    root: &Path,
    data_directory: &Path,
    batch: &EventBatch,
) -> Result<ReconciledIndex, String> {
    if batch.stream_identity.is_empty() {
        return Err("FSEvents stream identity is missing".into());
    }
    let report = scan_root(root).map_err(|error| error.to_string())?;
    let mut store = IndexStore::open(&data_directory.join("index.sqlite3"))
        .map_err(|error| error.to_string())?;
    let generation = store
        .commit_reconciliation(&report, &batch.stream_identity, batch.highest_event_id)
        .map_err(|error| error.to_string())?;
    let committed = store
        .latest_committed()
        .map_err(|error| error.to_string())?
        .ok_or_else(|| "reconciliation committed without a published generation".to_owned())?;
    let coverage = committed.coverage;
    let enabled = store
        .enabled_committed()
        .map_err(|error| error.to_string())?;
    let projection =
        SearchProjection::build_combined(&data_directory.join("search.projection"), &enabled)
            .map_err(|error| error.to_string())?;
    Ok(ReconciledIndex {
        projection,
        coverage,
        generation,
        coverage_report: RootCoverage {
            volume_id: committed.volume_id,
            root: committed.root,
            coverage,
            skipped: committed.skipped,
        },
    })
}

pub fn reconcile_recovery_plan(
    root: &Path,
    data_directory: &Path,
    batch: &EventBatch,
    plan: &RecoveryPlan,
) -> Result<ReconciledIndex, String> {
    match plan {
        RecoveryPlan::RebuildVolume { .. } => reconcile_committed_root(root, data_directory, batch),
        RecoveryPlan::RepairSubtrees { scopes, .. } => {
            reconcile_committed_scopes(root, data_directory, batch, scopes)
        }
        RecoveryPlan::Replay { .. } => Err("replay plan does not contain observed changes".into()),
    }
}

fn reconcile_committed_scopes(
    root: &Path,
    data_directory: &Path,
    batch: &EventBatch,
    scopes: &[PathBuf],
) -> Result<ReconciledIndex, String> {
    if scopes.is_empty() || scopes.iter().any(|scope| scope == root) {
        return reconcile_committed_root(root, data_directory, batch);
    }
    let database = data_directory.join("index.sqlite3");
    let mut store = IndexStore::open(&database).map_err(|error| error.to_string())?;
    let committed = store
        .latest_committed()
        .map_err(|error| error.to_string())?
        .ok_or_else(|| "subtree repair requires a committed File Index".to_owned())?;
    let prior_coverage = committed.coverage;
    let mut entries: Vec<_> = committed
        .entries
        .into_iter()
        .filter(|entry| !scopes.iter().any(|scope| entry.path.starts_with(scope)))
        .collect();
    let mut skipped: Vec<_> = committed
        .skipped
        .into_iter()
        .filter(|location| !scopes.iter().any(|scope| location.path.starts_with(scope)))
        .collect();
    for scope in scopes {
        if !scope.exists() {
            continue;
        }
        let observed = scan_root(scope).map_err(|error| error.to_string())?;
        entries.extend(observed.entries);
        skipped.extend(observed.skipped);
    }
    entries.sort_unstable_by(|left, right| left.path.cmp(&right.path));
    entries.dedup_by(|left, right| left.path == right.path);
    let report = ScanReport {
        root: root.to_path_buf(),
        volume_id: committed.volume_id,
        entries,
        skipped,
    };
    let coverage = if prior_coverage == Coverage::Partial || report.coverage() == Coverage::Partial
    {
        Coverage::Partial
    } else {
        Coverage::Complete
    };
    let generation = store
        .commit_reconciliation_with_coverage(
            &report,
            coverage,
            &batch.stream_identity,
            batch.highest_event_id,
        )
        .map_err(|error| error.to_string())?;
    let committed = store
        .latest_committed()
        .map_err(|error| error.to_string())?
        .ok_or_else(|| "repair committed without a published generation".to_owned())?;
    let coverage = committed.coverage;
    let enabled = store
        .enabled_committed()
        .map_err(|error| error.to_string())?;
    let projection =
        SearchProjection::build_combined(&data_directory.join("search.projection"), &enabled)
            .map_err(|error| error.to_string())?;
    Ok(ReconciledIndex {
        projection,
        coverage,
        generation,
        coverage_report: RootCoverage {
            volume_id: committed.volume_id,
            root: committed.root,
            coverage,
            skipped: committed.skipped,
        },
    })
}

fn minimal_safe_scopes(root: &Path, paths: &[PathBuf]) -> Vec<PathBuf> {
    let mut scopes = Vec::new();
    for path in paths.iter().filter(|path| path.starts_with(root)) {
        let scope = if path == root {
            root.to_path_buf()
        } else {
            path.parent()
                .filter(|parent| parent.starts_with(root))
                .unwrap_or(root)
                .to_path_buf()
        };
        if scopes
            .iter()
            .any(|existing: &PathBuf| scope.starts_with(existing))
        {
            continue;
        }
        scopes.retain(|existing| !existing.starts_with(&scope));
        scopes.push(scope);
    }
    scopes.sort_unstable();
    scopes
}

#[cfg(test)]
mod tests {
    use std::fs;
    use std::os::unix::fs::MetadataExt;

    use tempfile::tempdir;

    use super::*;
    use crate::coordinator::build_first_index;

    #[test]
    fn presets_are_coalescing_windows() {
        assert_eq!(
            CoalescingPreset::Responsive.window(),
            Duration::from_secs(1)
        );
        assert_eq!(CoalescingPreset::default().window(), Duration::from_secs(5));
        assert_eq!(
            CoalescingPreset::LowEnergy.window(),
            Duration::from_secs(15)
        );
        assert!(!HintCoalescer::default().has_pending());
    }

    #[test]
    fn coalescing_collapses_descendants_and_keeps_highest_event() {
        let mut coalescer = HintCoalescer::default();
        coalescer.push(EventBatch {
            stream_identity: "disk".into(),
            highest_event_id: 3,
            paths: vec!["/root/a/b".into(), "/root/a".into()],
            history_lost: false,
            ids_wrapped: false,
            root_changed: false,
        });
        coalescer.push(EventBatch {
            stream_identity: "disk".into(),
            highest_event_id: 9,
            paths: vec!["/root/c".into()],
            history_lost: true,
            ids_wrapped: false,
            root_changed: false,
        });
        let batch = coalescer.take().unwrap();
        assert_eq!(
            batch.paths,
            vec![PathBuf::from("/root/a"), PathBuf::from("/root/c")]
        );
        assert_eq!(batch.highest_event_id, 9);
        assert!(batch.history_lost);
        assert!(!coalescer.has_pending());
    }

    #[test]
    fn stream_start_validates_identity_cursor_and_generation() {
        let checkpoint = VolumeCheckpoint {
            volume_id: 1,
            root: "/root".into(),
            stream_identity: "uuid-a".into(),
            event_id: 42,
            generation: 7,
        };
        assert_eq!(
            plan_stream_start(Some(&checkpoint), "uuid-a", 7),
            RecoveryPlan::Replay { since_event_id: 42 }
        );
        assert_eq!(
            plan_stream_start(Some(&checkpoint), "uuid-b", 7),
            RecoveryPlan::RebuildVolume {
                reason: RecoveryReason::StreamIdentityMismatch
            }
        );
        let mut invalid = checkpoint.clone();
        invalid.event_id = 0;
        assert_eq!(
            plan_stream_start(Some(&invalid), "uuid-a", 7),
            RecoveryPlan::RebuildVolume {
                reason: RecoveryReason::MissingOrInvalidCursor
            }
        );
        assert!(matches!(
            plan_stream_start(None, "uuid-a", 7),
            RecoveryPlan::RebuildVolume { .. }
        ));
    }

    #[test]
    fn history_loss_uses_known_subtrees_but_wrapped_ids_rebuild() {
        let known = EventBatch {
            stream_identity: "uuid".into(),
            highest_event_id: 10,
            paths: vec!["/root/a/file".into(), "/root/a/other".into()],
            history_lost: true,
            ids_wrapped: false,
            root_changed: false,
        };
        assert_eq!(
            plan_batch(Path::new("/root"), &known),
            RecoveryPlan::RepairSubtrees {
                scopes: vec![PathBuf::from("/root/a")],
                reason: RecoveryReason::DroppedHistory,
            }
        );
        let wrapped = EventBatch {
            ids_wrapped: true,
            ..known
        };
        assert_eq!(
            plan_batch(Path::new("/root"), &wrapped),
            RecoveryPlan::RebuildVolume {
                reason: RecoveryReason::EventIdsWrapped
            }
        );
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn create_rename_move_delete_and_metadata_reconcile_from_observed_state() {
        let root = tempdir().unwrap();
        let data = tempdir().unwrap();
        build_first_index(root.path(), data.path()).unwrap();

        fs::create_dir(root.path().join("folder")).unwrap();
        fs::write(root.path().join("created.txt"), "one").unwrap();
        fs::rename(
            root.path().join("created.txt"),
            root.path().join("folder/moved.txt"),
        )
        .unwrap();
        fs::write(root.path().join("folder/moved.txt"), "metadata changed").unwrap();
        fs::write(root.path().join("deleted.txt"), "gone").unwrap();
        fs::remove_file(root.path().join("deleted.txt")).unwrap();

        let reconciled = reconcile_committed_root(
            root.path(),
            data.path(),
            &EventBatch {
                stream_identity: "test-volume".into(),
                highest_event_id: 42,
                paths: vec![root.path().to_path_buf()],
                history_lost: false,
                ids_wrapped: false,
                root_changed: false,
            },
        )
        .unwrap();
        assert_eq!(reconciled.projection.search("moved", 100).unwrap().len(), 1);
        assert!(
            reconciled
                .projection
                .search("deleted", 100)
                .unwrap()
                .is_empty()
        );
        let checkpoint = IndexStore::open(&data.path().join("index.sqlite3"))
            .unwrap()
            .checkpoint(std::fs::metadata(root.path()).unwrap().dev(), root.path())
            .unwrap()
            .unwrap();
        assert_eq!(checkpoint.event_id, 42);
        assert_eq!(checkpoint.generation, reconciled.generation);
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn subtree_repair_does_not_observe_unrelated_scope() {
        let root = tempdir().unwrap();
        let data = tempdir().unwrap();
        fs::create_dir(root.path().join("affected")).unwrap();
        fs::create_dir(root.path().join("unrelated")).unwrap();
        fs::write(root.path().join("affected/old.txt"), "old").unwrap();
        fs::write(root.path().join("unrelated/stale.txt"), "stale").unwrap();
        build_first_index(root.path(), data.path()).unwrap();
        let canonical_root = root.path().canonicalize().unwrap();
        fs::remove_file(root.path().join("affected/old.txt")).unwrap();
        fs::write(root.path().join("affected/new.txt"), "new").unwrap();
        fs::remove_file(root.path().join("unrelated/stale.txt")).unwrap();

        let batch = EventBatch {
            stream_identity: "test-volume".into(),
            highest_event_id: 55,
            paths: vec![canonical_root.join("affected/new.txt")],
            history_lost: true,
            ids_wrapped: false,
            root_changed: false,
        };
        let plan = plan_batch(&canonical_root, &batch);
        let repaired =
            reconcile_recovery_plan(&canonical_root, data.path(), &batch, &plan).unwrap();
        assert_eq!(repaired.projection.search("new.txt", 100).unwrap().len(), 1);
        let old = repaired.projection.search("old.txt", 100).unwrap();
        assert!(old.is_empty(), "unexpected stale results: {old:?}");
        assert_eq!(
            repaired.projection.search("stale.txt", 100).unwrap().len(),
            1
        );
    }
}

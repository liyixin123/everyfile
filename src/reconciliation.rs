use std::path::{Path, PathBuf};
use std::time::Duration;

use crate::index::IndexStore;
use crate::model::Coverage;
use crate::projection::SearchProjection;
use crate::scanner::scan_root;

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
}

#[derive(Default)]
pub struct HintCoalescer {
    paths: Vec<PathBuf>,
    highest_event_id: u64,
    history_lost: bool,
    stream_identity: String,
}

impl HintCoalescer {
    pub fn push(&mut self, batch: EventBatch) {
        self.highest_event_id = self.highest_event_id.max(batch.highest_event_id);
        self.history_lost |= batch.history_lost;
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
        self.history_lost || !self.paths.is_empty()
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
        })
    }
}

pub struct ReconciledIndex {
    pub projection: SearchProjection,
    pub coverage: Coverage,
    pub generation: u64,
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
    let projection = SearchProjection::build(&data_directory.join("search.projection"), &committed)
        .map_err(|error| error.to_string())?;
    Ok(ReconciledIndex {
        projection,
        coverage,
        generation,
    })
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
        });
        coalescer.push(EventBatch {
            stream_identity: "disk".into(),
            highest_event_id: 9,
            paths: vec!["/root/c".into()],
            history_lost: true,
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
}

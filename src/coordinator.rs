use std::path::{Path, PathBuf};

use crate::index::IndexStore;
use crate::model::{Coverage, FileIndexState, RootCoverage};
use crate::projection::SearchProjection;
use crate::scanner::scan_root_with_progress;
use crate::volume::{discover_mounted_volumes, volume_containing};

pub struct BuiltIndex {
    pub state: FileIndexState,
    pub projection: SearchProjection,
    pub coverage_report: RootCoverage,
    pub recovery_archive: Option<PathBuf>,
}

pub fn build_first_index(root: &Path, data_directory: &Path) -> Result<BuiltIndex, String> {
    build_first_index_with_progress(root, data_directory, |_| {})
}

pub fn build_first_index_with_progress(
    root: &Path,
    data_directory: &Path,
    progress: impl FnMut(u64),
) -> Result<BuiltIndex, String> {
    let canonical_root = root
        .canonicalize()
        .map_err(|error| format!("cannot resolve configured root: {error}"))?;
    let volumes = discover_mounted_volumes().map_err(|error| error.to_string())?;
    let volume = volume_containing(&volumes, &canonical_root)
        .ok_or_else(|| "configured root is not on a discovered volume".to_owned())?;
    let (mut store, recovery_archive) =
        IndexStore::open_or_recover(&data_directory.join("index.sqlite3"))?;
    for discovered in &volumes {
        store
            .observe_volume(discovered)
            .map_err(|error| error.to_string())?;
    }
    let enabled = store
        .volume_configurations()
        .map_err(|error| error.to_string())?
        .into_iter()
        .find(|configuration| configuration.identity == volume.identity)
        .is_some_and(|configuration| configuration.enabled);
    if !volume.local || !enabled {
        return Err("configured external volume requires explicit opt-in".to_owned());
    }

    if let Some(summary) = store
        .committed_root_summary(&canonical_root)
        .map_err(|error| error.to_string())?
    {
        let projection_path = data_directory.join("search.projection");
        let expected_generation = store
            .enabled_projection_generation()
            .map_err(|error| error.to_string())?;
        let projection = SearchProjection::open(&projection_path, expected_generation)
            .or_else(|_| {
                let enabled = store.enabled_committed()?;
                SearchProjection::build_combined(&projection_path, &enabled)
                    .map_err(|error| rusqlite::Error::ToSqlConversionFailure(Box::new(error)))
            })
            .map_err(|error| error.to_string())?;
        return Ok(BuiltIndex {
            state: FileIndexState::Current {
                coverage: summary.coverage,
            },
            projection,
            coverage_report: summary,
            recovery_archive,
        });
    }

    let report =
        scan_root_with_progress(&canonical_root, progress).map_err(|error| error.to_string())?;
    #[cfg(target_os = "macos")]
    let commit = crate::fsevents::stream_identity(&canonical_root).and_then(|identity| {
        store
            .commit_reconciliation(&report, &identity, crate::fsevents::current_event_id())
            .map_err(|error| error.to_string())
    });
    #[cfg(not(target_os = "macos"))]
    let commit = store.commit_scan(&report);
    commit.map_err(|error| error.to_string())?;
    let committed = store
        .all_committed()
        .map_err(|error| error.to_string())?
        .into_iter()
        .find(|committed| committed.root == canonical_root)
        .ok_or_else(|| "scan committed without a published generation".to_owned())?;
    let enabled = store
        .enabled_committed()
        .map_err(|error| error.to_string())?;
    let projection =
        SearchProjection::build_combined(&data_directory.join("search.projection"), &enabled)
            .map_err(|error| error.to_string())?;
    Ok(BuiltIndex {
        state: FileIndexState::Current {
            coverage: committed.coverage,
        },
        projection,
        coverage_report: RootCoverage {
            volume_id: committed.volume_id,
            root: committed.root,
            coverage: committed.coverage,
            skipped: committed.skipped,
        },
        recovery_archive,
    })
}

pub fn default_data_directory() -> PathBuf {
    if let Some(path) = std::env::var_os("EVERYFILE_DATA_DIR") {
        return PathBuf::from(path);
    }
    std::env::var_os("HOME")
        .map(PathBuf::from)
        .unwrap_or_else(std::env::temp_dir)
        .join("Library/Application Support/Everyfile")
}

pub fn configured_root() -> Option<PathBuf> {
    std::env::var_os("EVERYFILE_INDEX_ROOT").map(PathBuf::from)
}

pub fn coverage_for_skips(skip_count: usize) -> Coverage {
    if skip_count == 0 {
        Coverage::Complete
    } else {
        Coverage::Partial
    }
}

#[cfg(test)]
mod tests {
    use std::fs;

    use tempfile::tempdir;

    use super::*;

    #[cfg(target_os = "macos")]
    #[test]
    fn real_files_flow_from_scanner_through_projection() {
        let root = tempdir().unwrap();
        fs::create_dir(root.path().join("Projects")).unwrap();
        fs::write(root.path().join("Projects/Everyfile Notes.md"), "notes").unwrap();
        let data = tempdir().unwrap();

        let built = build_first_index(root.path(), data.path()).unwrap();
        assert_eq!(
            built.projection.search("everyfile", 100).unwrap()[0].name,
            "Everyfile Notes.md"
        );
        assert_eq!(
            built.state,
            FileIndexState::Current {
                coverage: Coverage::Complete
            }
        );

        fs::remove_file(root.path().join("Projects/Everyfile Notes.md")).unwrap();
        fs::remove_file(data.path().join("search.projection")).unwrap();
        let restarted = build_first_index(root.path(), data.path()).unwrap();
        assert_eq!(
            restarted.projection.search("everyfile", 100).unwrap().len(),
            1
        );
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn damaged_index_is_archived_before_verified_rebuild_publication() {
        let root = tempdir().unwrap();
        fs::write(root.path().join("verified.txt"), "verified").unwrap();
        let data = tempdir().unwrap();
        fs::write(data.path().join("index.sqlite3"), b"damaged sqlite").unwrap();

        let built = build_first_index(root.path(), data.path()).unwrap();
        assert!(built.recovery_archive.as_ref().unwrap().exists());
        assert_eq!(built.projection.search("verified", 100).unwrap().len(), 1);
        assert_eq!(
            built.state,
            FileIndexState::Current {
                coverage: Coverage::Complete
            }
        );
    }
}

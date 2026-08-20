use std::collections::VecDeque;
use std::fs;
use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};

use crate::model::{Coverage, EntryKind, IndexedEntry, SkippedLocation};

#[derive(Debug)]
pub struct ScanReport {
    pub root: PathBuf,
    pub volume_id: u64,
    pub entries: Vec<IndexedEntry>,
    pub skipped: Vec<SkippedLocation>,
}

impl ScanReport {
    pub fn coverage(&self) -> Coverage {
        if self.skipped.is_empty() {
            Coverage::Complete
        } else {
            Coverage::Partial
        }
    }
}

pub fn scan_root(root: &Path) -> std::io::Result<ScanReport> {
    scan_root_with_progress(root, |_| {})
}

pub fn scan_root_with_progress(
    root: &Path,
    mut progress: impl FnMut(u64),
) -> std::io::Result<ScanReport> {
    let root_metadata = fs::symlink_metadata(root)?;
    let volume_id = root_metadata.dev();
    let mut entries = Vec::new();
    let mut skipped = Vec::new();
    let mut pending = VecDeque::from([root.to_path_buf()]);

    while let Some(directory) = pending.pop_front() {
        let children = match fs::read_dir(&directory) {
            Ok(children) => children,
            Err(error) => {
                skipped.push(SkippedLocation {
                    path: directory,
                    reason: error.to_string(),
                });
                continue;
            }
        };

        for child in children {
            let child = match child {
                Ok(child) => child,
                Err(error) => {
                    skipped.push(SkippedLocation {
                        path: directory.clone(),
                        reason: error.to_string(),
                    });
                    continue;
                }
            };
            let path = child.path();
            let metadata = match fs::symlink_metadata(&path) {
                Ok(metadata) => metadata,
                Err(error) => {
                    skipped.push(SkippedLocation {
                        path,
                        reason: error.to_string(),
                    });
                    continue;
                }
            };
            if metadata.dev() != volume_id {
                skipped.push(SkippedLocation {
                    path,
                    reason: "mount boundary".into(),
                });
                continue;
            }

            let file_type = metadata.file_type();
            let kind = if file_type.is_symlink() {
                EntryKind::Symlink
            } else if file_type.is_dir() {
                EntryKind::Directory
            } else if file_type.is_file() {
                EntryKind::File
            } else {
                EntryKind::Other
            };
            let name = child.file_name().to_string_lossy().into_owned();
            let hidden = name.starts_with('.');
            let entry_id = stable_entry_id(volume_id, metadata.ino());
            entries.push(IndexedEntry {
                entry_id,
                volume_id,
                name,
                path: path.clone(),
                kind,
                size: metadata.size(),
                created_ns: system_time_ns(metadata.created().ok()),
                modified_ns: system_time_ns(metadata.modified().ok()),
                hidden,
            });
            progress(entries.len() as u64);

            if file_type.is_dir() && !file_type.is_symlink() {
                pending.push_back(path);
            }
        }
    }

    entries.sort_unstable_by(|left, right| left.path.cmp(&right.path));
    Ok(ScanReport {
        root: root.to_path_buf(),
        volume_id,
        entries,
        skipped,
    })
}

fn stable_entry_id(volume_id: u64, inode: u64) -> u64 {
    volume_id.rotate_left(17) ^ inode
}

fn system_time_ns(time: Option<std::time::SystemTime>) -> Option<i64> {
    let duration = time?.duration_since(std::time::UNIX_EPOCH).ok()?;
    i64::try_from(duration.as_nanos()).ok()
}

#[cfg(test)]
mod tests {
    use std::os::unix::fs::symlink;

    use tempfile::tempdir;

    use super::*;

    #[test]
    fn scans_real_entries_without_following_directory_symlinks() {
        let root = tempdir().unwrap();
        fs::create_dir(root.path().join("folder")).unwrap();
        fs::write(root.path().join("folder/report.txt"), "report").unwrap();
        symlink(root.path().join("folder"), root.path().join("folder-link")).unwrap();

        let report = scan_root(root.path()).unwrap();
        let paths: Vec<_> = report
            .entries
            .iter()
            .map(|entry| entry.path.as_path())
            .collect();
        assert!(paths.contains(&root.path().join("folder").as_path()));
        assert!(paths.contains(&root.path().join("folder/report.txt").as_path()));
        assert!(paths.contains(&root.path().join("folder-link").as_path()));
        assert_eq!(paths.len(), 3);
        assert_eq!(report.coverage(), Coverage::Complete);
    }
}

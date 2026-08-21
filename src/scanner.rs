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
    progress: impl FnMut(u64),
) -> std::io::Result<ScanReport> {
    scan_root_with_policy(root, progress, |_, _| None)
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum ScanOperation {
    Enumerate,
    Metadata,
}

fn scan_root_with_policy(
    root: &Path,
    mut progress: impl FnMut(u64),
    mut denied: impl FnMut(&Path, ScanOperation) -> Option<std::io::Error>,
) -> std::io::Result<ScanReport> {
    let root_metadata = fs::symlink_metadata(root)?;
    let volume_id = root_metadata.dev();
    let mut entries = Vec::new();
    let mut skipped = Vec::new();
    let mut pending = VecDeque::from([root.to_path_buf()]);

    while let Some(directory) = pending.pop_front() {
        let children = match denied(&directory, ScanOperation::Enumerate)
            .map_or_else(|| fs::read_dir(&directory), Err)
        {
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
            let metadata = match denied(&path, ScanOperation::Metadata)
                .map_or_else(|| fs::symlink_metadata(&path), Err)
            {
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
    use std::ffi::CString;
    use std::os::unix::ffi::OsStrExt;
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

    #[test]
    fn denied_paths_are_reported_while_accessible_siblings_are_indexed() {
        let root = tempdir().unwrap();
        fs::create_dir(root.path().join("denied")).unwrap();
        fs::write(root.path().join("denied/private.txt"), "private").unwrap();
        fs::write(root.path().join("accessible.txt"), "accessible").unwrap();

        let report = scan_root_with_policy(
            root.path(),
            |_| {},
            |path, operation| {
                (path.ends_with("denied") && operation == ScanOperation::Enumerate).then(|| {
                    std::io::Error::new(std::io::ErrorKind::PermissionDenied, "permission denied")
                })
            },
        )
        .unwrap();
        assert_eq!(report.coverage(), Coverage::Partial);
        assert!(
            report
                .entries
                .iter()
                .any(|entry| entry.name == "accessible.txt")
        );
        assert!(report.entries.iter().any(|entry| entry.name == "denied"));
        assert!(
            !report
                .entries
                .iter()
                .any(|entry| entry.name == "private.txt")
        );
        assert_eq!(report.skipped.len(), 1);
        assert!(report.skipped[0].reason.contains("permission denied"));
    }

    #[test]
    fn metadata_denial_does_not_abort_other_entries() {
        let root = tempdir().unwrap();
        fs::write(root.path().join("denied.txt"), "denied").unwrap();
        fs::write(root.path().join("visible.txt"), "visible").unwrap();
        let report = scan_root_with_policy(
            root.path(),
            |_| {},
            |path, operation| {
                (path.ends_with("denied.txt") && operation == ScanOperation::Metadata).then(|| {
                    std::io::Error::new(std::io::ErrorKind::PermissionDenied, "metadata denied")
                })
            },
        )
        .unwrap();
        assert_eq!(report.coverage(), Coverage::Partial);
        assert_eq!(report.entries.len(), 1);
        assert_eq!(report.entries[0].name, "visible.txt");
    }

    #[test]
    fn scanning_never_opens_entry_contents() {
        let root = tempdir().unwrap();
        let fifo = root.path().join("cloud-placeholder");
        let fifo_path = CString::new(fifo.as_os_str().as_bytes()).unwrap();
        assert_eq!(unsafe { libc::mkfifo(fifo_path.as_ptr(), 0o600) }, 0);
        let report = scan_root(root.path()).unwrap();
        assert_eq!(report.entries.len(), 1);
        assert_eq!(report.entries[0].name, "cloud-placeholder");
        assert_eq!(report.entries[0].kind, EntryKind::Other);
    }
}

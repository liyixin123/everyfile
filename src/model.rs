use std::path::PathBuf;

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum FileIndexState {
    NotAvailable,
    Rebuilding { scanned_entries: u64 },
    Current { coverage: Coverage },
}

impl FileIndexState {
    pub const fn title(&self) -> &'static str {
        match self {
            Self::NotAvailable => "No File Index",
            Self::Rebuilding { .. } => "File Index: Rebuilding",
            Self::Current { .. } => "File Index: Current",
        }
    }

    pub fn detail(&self) -> String {
        match self {
            Self::NotAvailable => "Everyfile has not built a File Index yet.".into(),
            Self::Rebuilding { scanned_entries } => {
                format!("Scanning configured root — {scanned_entries} entries observed")
            }
            Self::Current {
                coverage: Coverage::Complete,
            } => "Coverage: Complete".into(),
            Self::Current {
                coverage: Coverage::Partial,
            } => "Coverage: Partial — some locations were skipped.".into(),
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Coverage {
    Complete,
    Partial,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Freshness {
    Current,
    CatchingUp,
    Rebuilding,
    Offline,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum EntryKind {
    File,
    Directory,
    Symlink,
    Other,
}

impl EntryKind {
    pub const fn as_i64(self) -> i64 {
        match self {
            Self::File => 1,
            Self::Directory => 2,
            Self::Symlink => 3,
            Self::Other => 4,
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct IndexedEntry {
    pub entry_id: u64,
    pub volume_id: u64,
    pub name: String,
    pub path: PathBuf,
    pub kind: EntryKind,
    pub size: u64,
    pub created_ns: Option<i64>,
    pub modified_ns: Option<i64>,
    pub hidden: bool,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct SkippedLocation {
    pub path: PathBuf,
    pub reason: String,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct RootCoverage {
    pub volume_id: u64,
    pub root: PathBuf,
    pub coverage: Coverage,
    pub skipped: Vec<SkippedLocation>,
}

pub fn overall_coverage(reports: &[RootCoverage]) -> Option<Coverage> {
    (!reports.is_empty()).then(|| {
        if reports
            .iter()
            .all(|report| report.coverage == Coverage::Complete)
        {
            Coverage::Complete
        } else {
            Coverage::Partial
        }
    })
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct SearchResult {
    pub entry_id: u64,
    pub name: String,
    pub path: PathBuf,
    pub size: u64,
    pub created_ns: Option<i64>,
    pub modified_ns: Option<i64>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct AppSnapshot {
    pub file_index: FileIndexState,
}

impl Default for AppSnapshot {
    fn default() -> Self {
        Self {
            file_index: FileIndexState::NotAvailable,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn empty_state_never_claims_freshness_or_coverage() {
        let snapshot = AppSnapshot::default();
        assert_eq!(snapshot.file_index.title(), "No File Index");
        assert!(snapshot.file_index.detail().contains("not built"));
    }

    #[test]
    fn overall_coverage_requires_reports_and_every_root_complete() {
        assert_eq!(overall_coverage(&[]), None);
        let report = |coverage| RootCoverage {
            volume_id: 1,
            root: "/".into(),
            coverage,
            skipped: Vec::new(),
        };
        assert_eq!(
            overall_coverage(&[report(Coverage::Complete)]),
            Some(Coverage::Complete)
        );
        assert_eq!(
            overall_coverage(&[report(Coverage::Complete), report(Coverage::Partial)]),
            Some(Coverage::Partial)
        );
    }
}

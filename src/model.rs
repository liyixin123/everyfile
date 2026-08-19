#[derive(Clone, Debug, Eq, PartialEq)]
pub enum FileIndexState {
    NotAvailable,
}

impl FileIndexState {
    pub const fn title(&self) -> &'static str {
        match self {
            Self::NotAvailable => "No File Index",
        }
    }

    pub const fn detail(&self) -> &'static str {
        match self {
            Self::NotAvailable => "Everyfile has not built a File Index yet.",
        }
    }
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
}

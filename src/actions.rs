use crate::model::SearchResult;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ResultAction {
    Open,
    Reveal,
    CopyPath,
}

pub trait ResultActionDispatcher {
    fn dispatch(&mut self, action: ResultAction, result: &SearchResult) -> bool;
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    use super::*;

    #[derive(Default)]
    struct RecordingDispatcher(Vec<(ResultAction, PathBuf)>);

    impl ResultActionDispatcher for RecordingDispatcher {
        fn dispatch(&mut self, action: ResultAction, result: &SearchResult) -> bool {
            self.0.push((action, result.path.clone()));
            true
        }
    }

    #[test]
    fn dispatcher_seam_records_observable_commands() {
        let result = SearchResult {
            entry_id: 1,
            name: "report.txt".into(),
            path: PathBuf::from("/tmp/report.txt"),
            size: 0,
            modified_ns: None,
        };
        let mut dispatcher = RecordingDispatcher::default();
        assert!(dispatcher.dispatch(ResultAction::Reveal, &result));
        assert_eq!(dispatcher.0, vec![(ResultAction::Reveal, result.path)]);
    }
}

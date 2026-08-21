use std::cmp::Ordering;
use std::collections::BinaryHeap;
use std::collections::HashMap;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering as AtomicOrdering};

use unicode_casefold::UnicodeCaseFold;
use unicode_normalization::{UnicodeNormalization, char::is_combining_mark};

use crate::model::SearchResult;

#[derive(Clone, Copy, Debug, Eq, Ord, PartialEq, PartialOrd)]
pub enum RelevanceClass {
    ExactFileName,
    FileNamePrefix,
    FileNameSegmentPrefix,
    OtherFileNameSubstring,
    PathOnly,
}

#[derive(Clone, Debug)]
pub struct QueryCandidate {
    pub result: SearchResult,
    pub normalized_name: String,
    pub normalized_path: String,
}

#[derive(Clone, Debug, Eq, PartialEq)]
struct RankKey {
    selected: SelectedValue,
    direction: SortDirection,
    class: RelevanceClass,
    name_term_count: usize,
    name_len: usize,
    path_len: usize,
    recent_open: u64,
    normalized_name: String,
    canonical_path: String,
}

#[derive(Clone, Debug, Eq, PartialEq)]
enum SelectedValue {
    Relevance,
    OptionalNumber(Option<i64>),
    Number(u64),
    Text(String),
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum SortField {
    Relevance,
    ModificationTime,
    CreationTime,
    FileName,
    FullPath,
    FileSize,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum SortDirection {
    Ascending,
    Descending,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct SortOrder {
    pub field: SortField,
    pub direction: SortDirection,
}

impl Default for SortOrder {
    fn default() -> Self {
        Self {
            field: SortField::Relevance,
            direction: SortDirection::Ascending,
        }
    }
}

#[derive(Clone, Default)]
pub struct CancellationToken(Arc<AtomicBool>);

impl CancellationToken {
    pub fn cancel(&self) {
        self.0.store(true, AtomicOrdering::Release);
    }
    pub fn is_cancelled(&self) -> bool {
        self.0.load(AtomicOrdering::Acquire)
    }
}

#[derive(Debug, Eq, PartialEq)]
pub struct RankedResults {
    pub rows: Vec<SearchResult>,
    pub exact_total: usize,
    pub max_retained: usize,
    pub cancelled: bool,
}

#[derive(Debug)]
struct HeapEntry {
    key: RankKey,
    result: SearchResult,
}

impl PartialEq for HeapEntry {
    fn eq(&self, other: &Self) -> bool {
        self.key == other.key
    }
}
impl Eq for HeapEntry {}
impl PartialOrd for HeapEntry {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}
impl Ord for HeapEntry {
    fn cmp(&self, other: &Self) -> Ordering {
        compare_keys(&self.key, &other.key)
    }
}

pub fn normalize_search_text(value: &str) -> String {
    let mut normalized = String::new();
    let mut latin_base = false;
    for character in value.case_fold().nfd() {
        if is_combining_mark(character) {
            if !latin_base {
                normalized.push(character);
            }
            continue;
        }
        latin_base = is_latin(character);
        normalized.push(character);
    }
    normalized
}

pub fn rank_candidates(
    query: &str,
    candidates: impl IntoIterator<Item = QueryCandidate>,
    recent_opens: &HashMap<u64, u64>,
    limit: usize,
) -> Vec<SearchResult> {
    rank_candidates_with_options(
        query,
        candidates,
        recent_opens,
        limit,
        SortOrder::default(),
        &CancellationToken::default(),
    )
    .rows
}

pub fn rank_candidates_with_options(
    query: &str,
    candidates: impl IntoIterator<Item = QueryCandidate>,
    recent_opens: &HashMap<u64, u64>,
    limit: usize,
    sort: SortOrder,
    cancellation: &CancellationToken,
) -> RankedResults {
    let terms: Vec<_> = normalize_search_text(query)
        .split_whitespace()
        .map(str::to_owned)
        .collect();
    let mut heap = BinaryHeap::with_capacity(limit.saturating_add(1));
    let mut exact_total = 0;
    let mut max_retained = 0;
    for (index, candidate) in candidates.into_iter().enumerate() {
        if index % 256 == 0 && cancellation.is_cancelled() {
            return RankedResults {
                rows: Vec::new(),
                exact_total,
                max_retained,
                cancelled: true,
            };
        }
        if let Some((mut key, result)) = rank_candidate(candidate, &terms, recent_opens) {
            exact_total += 1;
            apply_sort(&mut key, &result, sort);
            if limit > 0 {
                heap.push(HeapEntry { key, result });
                if heap.len() > limit {
                    heap.pop();
                }
                max_retained = max_retained.max(heap.len());
            }
        }
    }
    if cancellation.is_cancelled() {
        return RankedResults {
            rows: Vec::new(),
            exact_total,
            max_retained,
            cancelled: true,
        };
    }
    let mut ranked = heap.into_vec();
    ranked.sort_unstable_by(|left, right| compare_keys(&left.key, &right.key));
    RankedResults {
        rows: ranked.into_iter().map(|entry| entry.result).collect(),
        exact_total,
        max_retained,
        cancelled: false,
    }
}

fn apply_sort(key: &mut RankKey, result: &SearchResult, sort: SortOrder) {
    // RankKey remains the stable Relevance/tie-break contract. The selected field is
    // encoded ahead of it by the comparator below through thread-local-free fields.
    key.selected = match sort.field {
        SortField::Relevance => SelectedValue::Relevance,
        SortField::ModificationTime => SelectedValue::OptionalNumber(result.modified_ns),
        SortField::CreationTime => SelectedValue::OptionalNumber(result.created_ns),
        SortField::FileName => SelectedValue::Text(key.normalized_name.clone()),
        SortField::FullPath => SelectedValue::Text(key.canonical_path.clone()),
        SortField::FileSize => SelectedValue::Number(result.size),
    };
    key.direction = sort.direction;
}

fn rank_candidate(
    candidate: QueryCandidate,
    terms: &[String],
    recent_opens: &HashMap<u64, u64>,
) -> Option<(RankKey, SearchResult)> {
    if !terms.iter().all(|term| {
        candidate.normalized_name.contains(term) || candidate.normalized_path.contains(term)
    }) {
        return None;
    }
    let name_term_count = terms
        .iter()
        .filter(|term| candidate.normalized_name.contains(term.as_str()))
        .count();
    let class = classify(&candidate.normalized_name, terms, name_term_count);
    let key = RankKey {
        selected: SelectedValue::Relevance,
        direction: SortDirection::Ascending,
        class,
        name_term_count,
        name_len: candidate.normalized_name.chars().count(),
        path_len: candidate.normalized_path.chars().count(),
        recent_open: recent_opens
            .get(&candidate.result.entry_id)
            .copied()
            .unwrap_or_default(),
        normalized_name: candidate.normalized_name,
        canonical_path: candidate.normalized_path,
    };
    Some((key, candidate.result))
}

fn classify(name: &str, terms: &[String], name_term_count: usize) -> RelevanceClass {
    if terms.len() == 1 && name == terms[0] {
        RelevanceClass::ExactFileName
    } else if terms.iter().all(|term| name.starts_with(term)) {
        RelevanceClass::FileNamePrefix
    } else if terms.iter().all(|term| has_segment_prefix(name, term)) {
        RelevanceClass::FileNameSegmentPrefix
    } else if name_term_count > 0 {
        RelevanceClass::OtherFileNameSubstring
    } else {
        RelevanceClass::PathOnly
    }
}

fn has_segment_prefix(name: &str, term: &str) -> bool {
    name.match_indices(term).any(|(index, _)| {
        index == 0
            || name[..index]
                .chars()
                .next_back()
                .is_some_and(is_segment_separator)
    })
}

fn is_segment_separator(character: char) -> bool {
    character.is_whitespace() || matches!(character, '.' | '-' | '_' | '/' | '(' | ')')
}

fn compare_keys(left: &RankKey, right: &RankKey) -> Ordering {
    if !matches!(left.selected, SelectedValue::Relevance) {
        return compare_selected(&left.selected, &right.selected, left.direction)
            .then_with(|| compare_relevance(left, right))
            .then_with(|| left.normalized_name.cmp(&right.normalized_name))
            .then_with(|| left.canonical_path.cmp(&right.canonical_path));
    }
    let relevance = compare_relevance(left, right);
    match left.direction {
        SortDirection::Ascending => relevance,
        SortDirection::Descending => relevance.reverse(),
    }
}

fn compare_relevance(left: &RankKey, right: &RankKey) -> Ordering {
    left.class
        .cmp(&right.class)
        .then_with(|| right.name_term_count.cmp(&left.name_term_count))
        .then_with(|| left.name_len.cmp(&right.name_len))
        .then_with(|| left.path_len.cmp(&right.path_len))
        .then_with(|| right.recent_open.cmp(&left.recent_open))
        .then_with(|| left.canonical_path.cmp(&right.canonical_path))
}

fn compare_selected(
    left: &SelectedValue,
    right: &SelectedValue,
    direction: SortDirection,
) -> Ordering {
    let order = match (left, right) {
        (SelectedValue::OptionalNumber(None), SelectedValue::OptionalNumber(None)) => {
            Ordering::Equal
        }
        (SelectedValue::OptionalNumber(None), _) => return Ordering::Greater,
        (_, SelectedValue::OptionalNumber(None)) => return Ordering::Less,
        (SelectedValue::OptionalNumber(Some(left)), SelectedValue::OptionalNumber(Some(right))) => {
            left.cmp(right)
        }
        (SelectedValue::Number(left), SelectedValue::Number(right)) => left.cmp(right),
        (SelectedValue::Text(left), SelectedValue::Text(right)) => left.cmp(right),
        _ => Ordering::Equal,
    };
    match direction {
        SortDirection::Ascending => order,
        SortDirection::Descending => order.reverse(),
    }
}

fn is_latin(character: char) -> bool {
    matches!(
        character as u32,
        0x0041..=0x007A | 0x00C0..=0x024F | 0x1E00..=0x1EFF
    )
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    use super::*;

    fn candidate(id: u64, name: &str, path: &str) -> QueryCandidate {
        QueryCandidate {
            result: SearchResult {
                entry_id: id,
                name: name.into(),
                path: PathBuf::from(path),
                size: 0,
                created_ns: None,
                modified_ns: None,
            },
            normalized_name: normalize_search_text(name),
            normalized_path: normalize_search_text(path),
        }
    }

    fn ranked(
        candidates: impl IntoIterator<Item = QueryCandidate>,
        field: SortField,
        direction: SortDirection,
        limit: usize,
    ) -> RankedResults {
        rank_candidates_with_options(
            "",
            candidates,
            &HashMap::new(),
            limit,
            SortOrder { field, direction },
            &CancellationToken::default(),
        )
    }

    #[test]
    fn canonical_casefold_and_latin_diacritics_share_one_form() {
        assert_eq!(normalize_search_text("CAFÉ"), "cafe");
        assert_eq!(normalize_search_text("Cafe\u{301}"), "cafe");
        assert_eq!(normalize_search_text("Straße"), "strasse");
    }

    #[test]
    fn and_terms_may_split_between_name_and_path() {
        let results = rank_candidates(
            "budget quarterly",
            [candidate(1, "Budget.txt", "/Reports/Quarterly/Budget.txt")],
            &HashMap::new(),
            100,
        );
        assert_eq!(results.len(), 1);
    }

    #[test]
    fn match_classes_precede_recent_history() {
        let candidates = [
            candidate(1, "report", "/long/report"),
            candidate(2, "my-report", "/short/my-report"),
        ];
        let recent = HashMap::from([(2, 999)]);
        let results = rank_candidates("report", candidates, &recent, 100);
        assert_eq!(results[0].entry_id, 1);
    }

    #[test]
    fn recent_history_breaks_only_late_same_class_ties() {
        let candidates = [
            candidate(1, "a-report", "/a/a-report"),
            candidate(2, "b-report", "/b/b-report"),
        ];
        let recent = HashMap::from([(2, 99)]);
        let results = rank_candidates("report", candidates, &recent, 100);
        assert_eq!(results[0].entry_id, 2);
    }

    #[test]
    fn every_relevance_class_has_the_locked_order() {
        let candidates = [
            candidate(5, "other", "/report/other"),
            candidate(4, "myreport", "/myreport"),
            candidate(3, "my-report", "/my-report"),
            candidate(2, "report-final", "/report-final"),
            candidate(1, "report", "/report"),
        ];
        let ids: Vec<_> = rank_candidates("report", candidates, &HashMap::new(), 100)
            .into_iter()
            .map(|result| result.entry_id)
            .collect();
        assert_eq!(ids, vec![1, 2, 3, 4, 5]);
    }

    #[test]
    fn name_term_count_then_lengths_then_path_are_stable_ties() {
        let candidates = [
            candidate(4, "alpha-long", "/z/quarterly/alpha-long"),
            candidate(3, "alpha", "/long/quarterly/alpha"),
            candidate(2, "alpha", "/b/quarterly/alpha"),
            candidate(1, "alpha-quarterly", "/a/alpha-quarterly"),
        ];
        let ids: Vec<_> = rank_candidates("alpha quarterly", candidates, &HashMap::new(), 100)
            .into_iter()
            .map(|result| result.entry_id)
            .collect();
        assert_eq!(ids, vec![1, 2, 3, 4]);
    }

    #[test]
    fn non_latin_diacritics_are_not_discarded() {
        assert_ne!(normalize_search_text("ά"), normalize_search_text("α"));
    }

    #[test]
    fn every_selected_field_supports_both_directions() {
        let mut first = candidate(1, "alpha", "/z/alpha");
        first.result.size = 10;
        first.result.created_ns = Some(10);
        first.result.modified_ns = Some(30);
        let mut second = candidate(2, "beta", "/a/beta");
        second.result.size = 20;
        second.result.created_ns = Some(20);
        second.result.modified_ns = Some(20);
        for field in [
            SortField::ModificationTime,
            SortField::CreationTime,
            SortField::FileName,
            SortField::FullPath,
            SortField::FileSize,
        ] {
            let ascending = ranked(
                [first.clone(), second.clone()],
                field,
                SortDirection::Ascending,
                100,
            );
            let descending = ranked(
                [first.clone(), second.clone()],
                field,
                SortDirection::Descending,
                100,
            );
            let ascending_ids: Vec<_> = ascending.rows.iter().map(|row| row.entry_id).collect();
            let descending_ids: Vec<_> = descending.rows.iter().map(|row| row.entry_id).collect();
            assert_eq!(
                descending_ids,
                ascending_ids.into_iter().rev().collect::<Vec<_>>()
            );
        }
    }

    #[test]
    fn missing_metadata_is_last_in_both_directions() {
        let mut missing = candidate(1, "missing", "/missing");
        missing.result.modified_ns = None;
        let mut present = candidate(2, "present", "/present");
        present.result.modified_ns = Some(5);
        for direction in [SortDirection::Ascending, SortDirection::Descending] {
            let result = ranked(
                [missing.clone(), present.clone()],
                SortField::ModificationTime,
                direction,
                100,
            );
            assert_eq!(
                result
                    .rows
                    .iter()
                    .map(|row| row.entry_id)
                    .collect::<Vec<_>>(),
                vec![2, 1]
            );
        }
    }

    #[test]
    fn high_match_queries_retain_only_the_requested_frontier() {
        let candidates =
            (0..10_000).map(|id| candidate(id, &format!("file-{id}"), &format!("/{id}")));
        let result = ranked(
            candidates,
            SortField::FileName,
            SortDirection::Ascending,
            100,
        );
        assert_eq!(result.exact_total, 10_000);
        assert_eq!(result.rows.len(), 100);
        assert_eq!(result.max_retained, 100);
    }

    #[test]
    fn cancelled_query_publishes_no_rows_for_every_sort() {
        for field in [
            SortField::Relevance,
            SortField::ModificationTime,
            SortField::CreationTime,
            SortField::FileName,
            SortField::FullPath,
            SortField::FileSize,
        ] {
            let cancellation = CancellationToken::default();
            cancellation.cancel();
            let result = rank_candidates_with_options(
                "",
                [candidate(1, "old", "/old")],
                &HashMap::new(),
                100,
                SortOrder {
                    field,
                    direction: SortDirection::Ascending,
                },
                &cancellation,
            );
            assert!(result.cancelled);
            assert!(result.rows.is_empty());
        }
    }

    #[test]
    fn later_frontier_preserves_the_first_page_prefix() {
        let corpus: Vec<_> = (0..250)
            .rev()
            .map(|id| candidate(id, &format!("file-{id:03}"), &format!("/{id:03}")))
            .collect();
        let first = ranked(
            corpus.clone(),
            SortField::FileName,
            SortDirection::Ascending,
            100,
        );
        let later = ranked(corpus, SortField::FileName, SortDirection::Ascending, 200);
        assert_eq!(first.rows, later.rows[..100]);
        assert_eq!(later.exact_total, 250);
    }
}

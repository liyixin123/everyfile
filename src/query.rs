use std::cmp::Ordering;
use std::collections::HashMap;

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

#[derive(Debug, Eq, PartialEq)]
struct RankKey {
    class: RelevanceClass,
    name_term_count: usize,
    name_len: usize,
    path_len: usize,
    recent_open: u64,
    canonical_path: String,
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
    let terms: Vec<_> = normalize_search_text(query)
        .split_whitespace()
        .map(str::to_owned)
        .collect();
    let mut ranked: Vec<_> = candidates
        .into_iter()
        .filter_map(|candidate| rank_candidate(candidate, &terms, recent_opens))
        .collect();
    ranked.sort_unstable_by(|(left_key, _), (right_key, _)| compare_keys(left_key, right_key));
    ranked
        .into_iter()
        .take(limit)
        .map(|(_, result)| result)
        .collect()
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
        class,
        name_term_count,
        name_len: candidate.normalized_name.chars().count(),
        path_len: candidate.normalized_path.chars().count(),
        recent_open: recent_opens
            .get(&candidate.result.entry_id)
            .copied()
            .unwrap_or_default(),
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
    left.class
        .cmp(&right.class)
        .then_with(|| right.name_term_count.cmp(&left.name_term_count))
        .then_with(|| left.name_len.cmp(&right.name_len))
        .then_with(|| left.path_len.cmp(&right.path_len))
        .then_with(|| right.recent_open.cmp(&left.recent_open))
        .then_with(|| left.canonical_path.cmp(&right.canonical_path))
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
                modified_ns: None,
            },
            normalized_name: normalize_search_text(name),
            normalized_path: normalize_search_text(path),
        }
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
}

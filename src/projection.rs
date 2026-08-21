use std::collections::HashMap;
use std::fs::{self, File};
use std::io::{self, Write};
use std::path::{Path, PathBuf};

use memmap2::{Mmap, MmapOptions};

use crate::index::CommittedIndex;
use crate::model::SearchResult;
use crate::query::{
    CancellationToken, QueryCandidate, RankedResults, SortOrder, normalize_search_text,
    rank_candidates_with_options,
};

const MAGIC: &[u8; 8] = b"EVFLIDX\0";
const VERSION: u32 = 4;
const HEADER_LEN: usize = 24;

pub struct SearchProjection {
    map: Mmap,
    generation: u64,
    record_count: u32,
}

impl SearchProjection {
    pub fn build_combined(path: &Path, committed: &[CommittedIndex]) -> io::Result<Self> {
        let combined = CommittedIndex {
            generation: committed
                .iter()
                .map(|index| index.generation)
                .max()
                .unwrap_or(0),
            volume_id: 0,
            root: PathBuf::new(),
            coverage: if committed
                .iter()
                .all(|index| index.coverage == crate::model::Coverage::Complete)
            {
                crate::model::Coverage::Complete
            } else {
                crate::model::Coverage::Partial
            },
            entries: committed
                .iter()
                .flat_map(|index| index.entries.iter().cloned())
                .collect(),
            skipped: committed
                .iter()
                .flat_map(|index| index.skipped.iter().cloned())
                .collect(),
        };
        Self::build(path, &combined)
    }

    pub fn build(path: &Path, committed: &CommittedIndex) -> io::Result<Self> {
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent)?;
        }
        let temporary = temporary_sibling(path);
        let mut file = File::create(&temporary)?;
        file.write_all(MAGIC)?;
        file.write_all(&VERSION.to_le_bytes())?;
        file.write_all(&committed.generation.to_le_bytes())?;
        file.write_all(&(committed.entries.len() as u32).to_le_bytes())?;
        for entry in &committed.entries {
            let name = entry.name.as_bytes();
            let path = entry.path.to_string_lossy();
            let path = path.as_bytes();
            let normalized_name = normalize_search_text(&entry.name);
            let normalized_path = normalize_search_text(&entry.path.to_string_lossy());
            file.write_all(&entry.entry_id.to_le_bytes())?;
            file.write_all(&entry.size.to_le_bytes())?;
            file.write_all(&entry.created_ns.unwrap_or(i64::MIN).to_le_bytes())?;
            file.write_all(&entry.modified_ns.unwrap_or(i64::MIN).to_le_bytes())?;
            file.write_all(&(name.len() as u32).to_le_bytes())?;
            file.write_all(&(path.len() as u32).to_le_bytes())?;
            file.write_all(&(normalized_name.len() as u32).to_le_bytes())?;
            file.write_all(&(normalized_path.len() as u32).to_le_bytes())?;
            file.write_all(&[u8::from(entry.hidden)])?;
            file.write_all(name)?;
            file.write_all(path)?;
            file.write_all(normalized_name.as_bytes())?;
            file.write_all(normalized_path.as_bytes())?;
        }
        file.sync_all()?;
        drop(file);
        fs::rename(&temporary, path)?;
        Self::open(path, Some(committed.generation))
    }

    pub fn open(path: &Path, expected_generation: Option<u64>) -> io::Result<Self> {
        let file = File::open(path)?;
        let map = unsafe { MmapOptions::new().map(&file)? };
        if map.len() < HEADER_LEN || &map[..8] != MAGIC {
            return Err(invalid("invalid projection header"));
        }
        let version = read_u32(&map, 8)?;
        if version != VERSION {
            return Err(invalid("unsupported projection version"));
        }
        let generation = read_u64(&map, 12)?;
        if expected_generation.is_some_and(|expected| generation != expected) {
            return Err(invalid("stale projection generation"));
        }
        let record_count = read_u32(&map, 20)?;
        validate_records(&map, record_count)?;
        Ok(Self {
            map,
            generation,
            record_count,
        })
    }

    pub const fn generation(&self) -> u64 {
        self.generation
    }

    pub fn search(&self, query: &str, limit: usize) -> io::Result<Vec<SearchResult>> {
        self.search_with_history(query, &HashMap::new(), limit)
    }

    pub fn search_with_history(
        &self,
        query: &str,
        recent_opens: &HashMap<u64, u64>,
        limit: usize,
    ) -> io::Result<Vec<SearchResult>> {
        Ok(self
            .search_ranked(
                query,
                recent_opens,
                limit,
                SortOrder::default(),
                &CancellationToken::default(),
            )?
            .rows)
    }

    pub fn search_ranked(
        &self,
        query: &str,
        recent_opens: &HashMap<u64, u64>,
        limit: usize,
        sort: SortOrder,
        cancellation: &CancellationToken,
    ) -> io::Result<RankedResults> {
        self.search_ranked_with_visibility(query, recent_opens, limit, sort, cancellation, true)
    }

    pub fn search_ranked_with_visibility(
        &self,
        query: &str,
        recent_opens: &HashMap<u64, u64>,
        limit: usize,
        sort: SortOrder,
        cancellation: &CancellationToken,
        show_hidden: bool,
    ) -> io::Result<RankedResults> {
        let candidates = ProjectionCandidates {
            map: &self.map,
            remaining: self.record_count,
            offset: HEADER_LEN,
        };
        Ok(rank_candidates_with_options(
            query,
            candidates.filter(|candidate| show_hidden || !candidate.hidden),
            recent_opens,
            limit,
            sort,
            cancellation,
        ))
    }
}

struct ProjectionCandidates<'a> {
    map: &'a [u8],
    remaining: u32,
    offset: usize,
}

impl Iterator for ProjectionCandidates<'_> {
    type Item = QueryCandidate;

    fn next(&mut self) -> Option<Self::Item> {
        if self.remaining == 0 {
            return None;
        }
        let offset = self.offset;
        let entry_id = read_u64(self.map, offset).expect("projection was validated");
        let size = read_u64(self.map, offset + 8).expect("projection was validated");
        let raw_created = read_i64(self.map, offset + 16).expect("projection was validated");
        let raw_modified = read_i64(self.map, offset + 24).expect("projection was validated");
        let name_len = read_u32(self.map, offset + 32).expect("projection was validated") as usize;
        let path_len = read_u32(self.map, offset + 36).expect("projection was validated") as usize;
        let normalized_name_len =
            read_u32(self.map, offset + 40).expect("projection was validated") as usize;
        let normalized_path_len =
            read_u32(self.map, offset + 44).expect("projection was validated") as usize;
        let hidden = self.map[offset + 48] != 0;
        let mut cursor = offset + 49;
        let name = read_str(self.map, cursor, name_len).expect("projection was validated");
        cursor += name_len;
        let path = read_str(self.map, cursor, path_len).expect("projection was validated");
        cursor += path_len;
        let normalized_name =
            read_str(self.map, cursor, normalized_name_len).expect("projection was validated");
        cursor += normalized_name_len;
        let normalized_path =
            read_str(self.map, cursor, normalized_path_len).expect("projection was validated");
        cursor += normalized_path_len;
        self.offset = cursor;
        self.remaining -= 1;
        Some(QueryCandidate {
            result: SearchResult {
                entry_id,
                name: name.to_owned(),
                path: PathBuf::from(path),
                size,
                created_ns: (raw_created != i64::MIN).then_some(raw_created),
                modified_ns: (raw_modified != i64::MIN).then_some(raw_modified),
            },
            normalized_name: normalized_name.to_owned(),
            normalized_path: normalized_path.to_owned(),
            hidden,
        })
    }
}

fn temporary_sibling(path: &Path) -> PathBuf {
    path.with_extension(format!("tmp-{}", std::process::id()))
}

fn validate_records(map: &[u8], count: u32) -> io::Result<()> {
    let mut offset = HEADER_LEN;
    for _ in 0..count {
        let name_len = read_u32(map, offset + 32)? as usize;
        let path_len = read_u32(map, offset + 36)? as usize;
        let normalized_name_len = read_u32(map, offset + 40)? as usize;
        let normalized_path_len = read_u32(map, offset + 44)? as usize;
        offset = offset
            .checked_add(49)
            .and_then(|value| value.checked_add(name_len))
            .and_then(|value| value.checked_add(path_len))
            .and_then(|value| value.checked_add(normalized_name_len))
            .and_then(|value| value.checked_add(normalized_path_len))
            .ok_or_else(|| invalid("projection offsets overflow"))?;
        if offset > map.len() {
            return Err(invalid("projection record exceeds file"));
        }
        let name_start = offset - normalized_path_len - normalized_name_len - path_len - name_len;
        read_str(map, name_start, name_len)?;
        read_str(map, name_start + name_len, path_len)?;
        read_str(map, name_start + name_len + path_len, normalized_name_len)?;
        read_str(
            map,
            name_start + name_len + path_len + normalized_name_len,
            normalized_path_len,
        )?;
    }
    if offset != map.len() {
        return Err(invalid("projection has trailing bytes"));
    }
    Ok(())
}

fn read_u32(bytes: &[u8], offset: usize) -> io::Result<u32> {
    Ok(u32::from_le_bytes(read_array(bytes, offset)?))
}

fn read_u64(bytes: &[u8], offset: usize) -> io::Result<u64> {
    Ok(u64::from_le_bytes(read_array(bytes, offset)?))
}

fn read_i64(bytes: &[u8], offset: usize) -> io::Result<i64> {
    Ok(i64::from_le_bytes(read_array(bytes, offset)?))
}

fn read_array<const N: usize>(bytes: &[u8], offset: usize) -> io::Result<[u8; N]> {
    bytes
        .get(offset..offset + N)
        .ok_or_else(|| invalid("projection is truncated"))?
        .try_into()
        .map_err(|_| invalid("projection field has wrong size"))
}

fn read_str(bytes: &[u8], offset: usize, len: usize) -> io::Result<&str> {
    std::str::from_utf8(
        bytes
            .get(offset..offset + len)
            .ok_or_else(|| invalid("projection string exceeds file"))?,
    )
    .map_err(|_| invalid("projection string is not UTF-8"))
}

fn invalid(message: &'static str) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, message)
}

#[cfg(test)]
mod tests {
    use std::fs;

    use tempfile::tempdir;

    use super::*;
    use crate::index::IndexStore;
    use crate::scanner::scan_root;

    #[test]
    fn projection_rebuilds_from_sqlite_and_searches_name_or_path() {
        let fixture = tempdir().unwrap();
        fs::create_dir(fixture.path().join("Quarterly Reports")).unwrap();
        fs::write(
            fixture.path().join("Quarterly Reports/Budget.txt"),
            "budget",
        )
        .unwrap();
        let data = tempdir().unwrap();
        let mut store = IndexStore::open(&data.path().join("index.sqlite3")).unwrap();
        store
            .commit_scan(&scan_root(fixture.path()).unwrap())
            .unwrap();
        let committed = store.latest_committed().unwrap().unwrap();

        let path = data.path().join("search.projection");
        let projection = SearchProjection::build(&path, &committed).unwrap();
        assert_eq!(projection.generation(), committed.generation);
        assert_eq!(projection.search("budget", 100).unwrap().len(), 1);
        assert_eq!(projection.search("quarterly", 100).unwrap().len(), 2);

        fs::remove_file(&path).unwrap();
        let rebuilt = SearchProjection::build(&path, &committed).unwrap();
        assert_eq!(rebuilt.search("budget", 100).unwrap()[0].name, "Budget.txt");
    }

    #[test]
    fn combined_projection_keeps_results_from_multiple_roots() {
        let first = tempdir().unwrap();
        let second = tempdir().unwrap();
        fs::write(first.path().join("internal.txt"), "internal").unwrap();
        fs::write(second.path().join("external.txt"), "external").unwrap();
        let data = tempdir().unwrap();
        let database = data.path().join("index.sqlite3");
        let mut store = IndexStore::open(&database).unwrap();
        store
            .commit_scan(&scan_root(first.path()).unwrap())
            .unwrap();
        store
            .commit_scan(&scan_root(second.path()).unwrap())
            .unwrap();
        let committed = store.all_committed().unwrap();
        assert_eq!(committed.len(), 2);
        let projection =
            SearchProjection::build_combined(&data.path().join("combined.projection"), &committed)
                .unwrap();
        assert_eq!(projection.search("internal.txt", 100).unwrap().len(), 1);
        assert_eq!(projection.search("external.txt", 100).unwrap().len(), 1);
    }

    #[test]
    fn hidden_visibility_and_unicode_normalization_are_query_options() {
        let fixture = tempdir().unwrap();
        fs::write(fixture.path().join(".secret-café.txt"), "hidden").unwrap();
        fs::write(fixture.path().join("cafe\u{301}.txt"), "decomposed").unwrap();
        let data = tempdir().unwrap();
        let mut store = IndexStore::open(&data.path().join("index.sqlite3")).unwrap();
        store
            .commit_scan(&scan_root(fixture.path()).unwrap())
            .unwrap();
        let committed = store.latest_committed().unwrap().unwrap();
        let projection =
            SearchProjection::build(&data.path().join("search.projection"), &committed).unwrap();

        let shown = projection
            .search_ranked_with_visibility(
                "café",
                &HashMap::new(),
                100,
                SortOrder::default(),
                &CancellationToken::default(),
                true,
            )
            .unwrap();
        let hidden = projection
            .search_ranked_with_visibility(
                "café",
                &HashMap::new(),
                100,
                SortOrder::default(),
                &CancellationToken::default(),
                false,
            )
            .unwrap();
        assert_eq!(shown.rows.len(), 2);
        assert_eq!(hidden.rows.len(), 1);
        assert_eq!(hidden.rows[0].name, "cafe\u{301}.txt");
    }
}

use std::cmp::Ordering;
use std::collections::HashMap;
use std::fs::{self, File};
use std::io::{self, Seek, Write};
use std::path::{Path, PathBuf};

use memmap2::{Mmap, MmapOptions};

use crate::index::{CommittedIndex, IndexStore};
use crate::model::{EntryKind, IndexedEntry, SearchResult};
use crate::query::{
    BorrowedQueryCandidate, CancellationToken, RankedResults, SortOrder, normalize_search_text,
    rank_borrowed_candidates_with_options,
};

const MAGIC: &[u8; 8] = b"EVFLIDX\0";
const VERSION: u32 = 7;
const HEADER_LEN: usize = 28;
const SORT_INDEX_COUNT: usize = 5;

pub struct SearchProjection {
    map: Mmap,
    generation: u64,
    record_count: u32,
    directories: Vec<(usize, usize)>,
    normalized_directory_bytes: Vec<u8>,
    normalized_directories: Vec<(usize, usize)>,
    records_offset: usize,
    sort_indexes_offset: usize,
}

struct SortStub {
    offset: u32,
    size: u64,
    created_ns: Option<i64>,
    modified_ns: Option<i64>,
    normalized_name: String,
    name_len: usize,
    path_len: usize,
    recent_open: u64,
}

impl SearchProjection {
    pub fn build_from_store(path: &Path, store: &IndexStore) -> io::Result<Self> {
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent)?;
        }
        let connection = store.connection();
        let (generation, count): (u64, u32) = connection
            .query_row(
                "SELECT COALESCE(MAX(published.generation), 0), COUNT(entries.full_path)
                 FROM published_roots published
                 JOIN entries ON entries.generation = published.generation
                 LEFT JOIN volume_checkpoints checkpoint
                   ON checkpoint.volume_id = published.volume_id
                  AND checkpoint.root_path = published.root_path
                 LEFT JOIN volume_configurations configuration
                   ON configuration.identity = checkpoint.stream_identity
                 WHERE COALESCE(configuration.enabled, 1) = 1",
                [],
                |row| Ok((row.get::<_, i64>(0)? as u64, row.get::<_, i64>(1)? as u32)),
            )
            .map_err(sqlite_io)?;
        let mut directory_statement = connection
            .prepare(
                "SELECT entries.full_path
                 FROM published_roots published
                 JOIN entries ON entries.generation = published.generation
                 LEFT JOIN volume_checkpoints checkpoint
                   ON checkpoint.volume_id = published.volume_id
                  AND checkpoint.root_path = published.root_path
                 LEFT JOIN volume_configurations configuration
                   ON configuration.identity = checkpoint.stream_identity
                 WHERE COALESCE(configuration.enabled, 1) = 1
                 ORDER BY lower(entries.full_path), entries.full_path",
            )
            .map_err(sqlite_io)?;
        let mut directory_rows = directory_statement.query([]).map_err(sqlite_io)?;
        let mut directory_ids = HashMap::new();
        let mut directories = Vec::new();
        while let Some(row) = directory_rows.next().map_err(sqlite_io)? {
            let full_path: String = row.get(0).map_err(sqlite_io)?;
            let parent = parent_string(&full_path);
            if !directory_ids.contains_key(&parent) {
                let id = directories.len() as u32;
                directory_ids.insert(parent.clone(), id);
                directories.push(parent);
            }
        }
        let temporary = temporary_sibling(path);
        let mut file = File::create(&temporary)?;
        write_header(&mut file, generation, count, directories.len() as u32)?;
        write_directories(&mut file, &directories)?;
        let recent_opens = store.recent_opens().map_err(sqlite_io)?;
        let mut sort_stubs = Vec::with_capacity(count as usize);
        let mut statement = connection
            .prepare(
                "SELECT entries.entry_id, entries.volume_id, entries.full_path,
                        entries.kind, entries.size, entries.created_ns, entries.modified_ns,
                        entries.hidden
                 FROM published_roots published
                 JOIN entries ON entries.generation = published.generation
                 LEFT JOIN volume_checkpoints checkpoint
                   ON checkpoint.volume_id = published.volume_id
                  AND checkpoint.root_path = published.root_path
                 LEFT JOIN volume_configurations configuration
                   ON configuration.identity = checkpoint.stream_identity
                 WHERE COALESCE(configuration.enabled, 1) = 1
                 ORDER BY lower(entries.full_path), entries.full_path",
            )
            .map_err(sqlite_io)?;
        let mut rows = statement.query([]).map_err(sqlite_io)?;
        while let Some(row) = rows.next().map_err(sqlite_io)? {
            let full_path: String = row.get(2).map_err(sqlite_io)?;
            let entry = IndexedEntry {
                entry_id: row.get::<_, i64>(0).map_err(sqlite_io)? as u64,
                volume_id: row.get::<_, i64>(1).map_err(sqlite_io)? as u64,
                path: PathBuf::from(&full_path),
                name: Path::new(&full_path)
                    .file_name()
                    .map(|name| name.to_string_lossy().into_owned())
                    .unwrap_or_default(),
                kind: match row.get::<_, i64>(3).map_err(sqlite_io)? {
                    1 => EntryKind::File,
                    2 => EntryKind::Directory,
                    3 => EntryKind::Symlink,
                    _ => EntryKind::Other,
                },
                size: row.get::<_, i64>(4).map_err(sqlite_io)? as u64,
                created_ns: row.get(5).map_err(sqlite_io)?,
                modified_ns: row.get(6).map_err(sqlite_io)?,
                hidden: row.get(7).map_err(sqlite_io)?,
            };
            let offset = u32::try_from(file.stream_position()?)
                .map_err(|_| invalid("projection exceeds 4 GiB"))?;
            write_entry(&mut file, &entry, directory_ids[&parent_string(&full_path)])?;
            sort_stubs.push(sort_stub(&entry, offset, &recent_opens));
        }
        write_sort_indexes(&mut file, &mut sort_stubs)?;
        file.sync_all()?;
        drop(file);
        fs::rename(&temporary, path)?;
        Self::open(path, Some(generation))
    }

    pub fn build_combined(path: &Path, committed: &[CommittedIndex]) -> io::Result<Self> {
        let mut combined = CommittedIndex {
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
        combined
            .entries
            .sort_unstable_by_key(|entry| normalize_search_text(&entry.path.to_string_lossy()));
        Self::build(path, &combined)
    }

    pub fn build(path: &Path, committed: &CommittedIndex) -> io::Result<Self> {
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent)?;
        }
        let temporary = temporary_sibling(path);
        let mut directory_ids = HashMap::new();
        let mut directories = Vec::new();
        for entry in &committed.entries {
            let parent = parent_string(&entry.path.to_string_lossy());
            if !directory_ids.contains_key(&parent) {
                let id = directories.len() as u32;
                directory_ids.insert(parent.clone(), id);
                directories.push(parent);
            }
        }
        let mut file = File::create(&temporary)?;
        write_header(
            &mut file,
            committed.generation,
            committed.entries.len() as u32,
            directories.len() as u32,
        )?;
        write_directories(&mut file, &directories)?;
        let mut sort_stubs = Vec::with_capacity(committed.entries.len());
        let no_recent_opens = HashMap::new();
        for entry in &committed.entries {
            let parent = parent_string(&entry.path.to_string_lossy());
            let offset = u32::try_from(file.stream_position()?)
                .map_err(|_| invalid("projection exceeds 4 GiB"))?;
            write_entry(&mut file, entry, directory_ids[&parent])?;
            sort_stubs.push(sort_stub(entry, offset, &no_recent_opens));
        }
        write_sort_indexes(&mut file, &mut sort_stubs)?;
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
        let directory_count = read_u32(&map, 24)?;
        let (directories, records_offset) = read_directories(&map, directory_count)?;
        let sort_indexes_offset =
            validate_records(&map, records_offset, record_count, directory_count)?;
        let mut normalized_directory_bytes = Vec::new();
        let mut normalized_directories = Vec::with_capacity(directories.len());
        for &(offset, len) in &directories {
            let normalized = normalize_search_text(read_str(&map, offset, len)?);
            let start = normalized_directory_bytes.len();
            normalized_directory_bytes.extend_from_slice(normalized.as_bytes());
            normalized_directories.push((start, normalized.len()));
        }
        Ok(Self {
            map,
            generation,
            record_count,
            directories,
            normalized_directory_bytes,
            normalized_directories,
            records_offset,
            sort_indexes_offset,
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
        if query.trim().is_empty()
            && recent_opens.is_empty()
            && sort.direction == crate::query::SortDirection::Ascending
            && show_hidden
        {
            return Ok(self.empty_ranked_ascending(limit, sort, show_hidden));
        }
        let candidates = ProjectionCandidates {
            map: &self.map,
            directories: &self.directories,
            normalized_directory_bytes: &self.normalized_directory_bytes,
            normalized_directories: &self.normalized_directories,
            remaining: self.record_count,
            offset: self.records_offset,
        };
        Ok(rank_borrowed_candidates_with_options(
            query,
            candidates.filter(|candidate| show_hidden || !candidate.hidden),
            recent_opens,
            limit,
            sort,
            cancellation,
        ))
    }

    fn empty_ranked_ascending(
        &self,
        limit: usize,
        sort: SortOrder,
        show_hidden: bool,
    ) -> RankedResults {
        let index = match sort.field {
            crate::query::SortField::Relevance => Some(0),
            crate::query::SortField::ModificationTime => Some(1),
            crate::query::SortField::CreationTime => Some(2),
            crate::query::SortField::FileName => Some(3),
            crate::query::SortField::FileSize => Some(4),
            crate::query::SortField::FullPath => None,
        };
        let mut rows = Vec::with_capacity(limit.min(self.record_count as usize));
        if let Some(index) = index {
            let start = self.sort_indexes_offset + index * self.record_count as usize * 4;
            for position in 0..self.record_count as usize {
                let offset = read_u32(&self.map, start + position * 4)
                    .expect("sort index was validated") as usize;
                let candidate = self.candidate_at(offset);
                if show_hidden || !candidate.hidden {
                    rows.push(candidate_result(candidate));
                    if rows.len() == limit {
                        break;
                    }
                }
            }
        } else {
            let mut candidates = ProjectionCandidates {
                map: &self.map,
                directories: &self.directories,
                normalized_directory_bytes: &self.normalized_directory_bytes,
                normalized_directories: &self.normalized_directories,
                remaining: self.record_count,
                offset: self.records_offset,
            };
            for candidate in &mut candidates {
                if show_hidden || !candidate.hidden {
                    rows.push(candidate_result(candidate));
                    if rows.len() == limit {
                        break;
                    }
                }
            }
        }
        RankedResults {
            exact_total: self.record_count as usize,
            max_retained: rows.len(),
            rows,
            cancelled: false,
        }
    }

    fn candidate_at(&self, offset: usize) -> BorrowedQueryCandidate<'_> {
        candidate_at(
            &self.map,
            &self.directories,
            &self.normalized_directory_bytes,
            &self.normalized_directories,
            offset,
        )
        .0
    }
}

fn candidate_result(candidate: BorrowedQueryCandidate<'_>) -> SearchResult {
    SearchResult {
        entry_id: candidate.entry_id,
        name: candidate.name.to_owned(),
        path: Path::new(candidate.parent_path).join(candidate.name),
        size: candidate.size,
        created_ns: candidate.created_ns,
        modified_ns: candidate.modified_ns,
    }
}

fn write_header(
    file: &mut File,
    generation: u64,
    count: u32,
    directory_count: u32,
) -> io::Result<()> {
    file.write_all(MAGIC)?;
    file.write_all(&VERSION.to_le_bytes())?;
    file.write_all(&generation.to_le_bytes())?;
    file.write_all(&count.to_le_bytes())?;
    file.write_all(&directory_count.to_le_bytes())
}

fn write_directories(file: &mut File, directories: &[String]) -> io::Result<()> {
    for directory in directories {
        let bytes = directory.as_bytes();
        file.write_all(&(bytes.len() as u32).to_le_bytes())?;
        file.write_all(bytes)?;
    }
    Ok(())
}

fn write_entry(file: &mut File, entry: &IndexedEntry, directory_id: u32) -> io::Result<()> {
    let name = entry.name.as_bytes();
    let name_len = u16::try_from(name.len()).map_err(|_| invalid("file name is too long"))?;
    let normalized_name = normalize_search_text(&entry.name);
    let normalized_name_len = u16::try_from(normalized_name.len())
        .map_err(|_| invalid("normalized file name is too long"))?;
    file.write_all(&entry.entry_id.to_le_bytes())?;
    file.write_all(&entry.size.to_le_bytes())?;
    file.write_all(&entry.created_ns.unwrap_or(i64::MIN).to_le_bytes())?;
    file.write_all(&entry.modified_ns.unwrap_or(i64::MIN).to_le_bytes())?;
    file.write_all(&directory_id.to_le_bytes())?;
    file.write_all(&name_len.to_le_bytes())?;
    file.write_all(&normalized_name_len.to_le_bytes())?;
    file.write_all(&[u8::from(entry.hidden)])?;
    file.write_all(name)?;
    file.write_all(normalized_name.as_bytes())
}

fn parent_string(full_path: &str) -> String {
    Path::new(full_path)
        .parent()
        .unwrap_or_else(|| Path::new(""))
        .to_string_lossy()
        .into_owned()
}

fn sort_stub(entry: &IndexedEntry, offset: u32, recent_opens: &HashMap<u64, u64>) -> SortStub {
    let normalized_name = normalize_search_text(&entry.name);
    SortStub {
        offset,
        size: entry.size,
        created_ns: entry.created_ns,
        modified_ns: entry.modified_ns,
        name_len: normalized_name.chars().count(),
        path_len: entry.path.to_string_lossy().chars().count(),
        recent_open: recent_opens
            .get(&entry.entry_id)
            .copied()
            .unwrap_or_default(),
        normalized_name,
    }
}

fn write_sort_indexes(file: &mut File, stubs: &mut [SortStub]) -> io::Result<()> {
    stubs.sort_unstable_by(|left, right| {
        left.name_len
            .cmp(&right.name_len)
            .then_with(|| left.path_len.cmp(&right.path_len))
            .then_with(|| right.recent_open.cmp(&left.recent_open))
            .then_with(|| left.offset.cmp(&right.offset))
    });
    write_offsets(file, stubs)?;
    stubs.sort_unstable_by(|left, right| {
        compare_optional(left.modified_ns, right.modified_ns)
            .then_with(|| left.offset.cmp(&right.offset))
    });
    write_offsets(file, stubs)?;
    stubs.sort_unstable_by(|left, right| {
        compare_optional(left.created_ns, right.created_ns)
            .then_with(|| left.offset.cmp(&right.offset))
    });
    write_offsets(file, stubs)?;
    stubs.sort_unstable_by(|left, right| {
        left.normalized_name
            .cmp(&right.normalized_name)
            .then_with(|| left.offset.cmp(&right.offset))
    });
    write_offsets(file, stubs)?;
    stubs.sort_unstable_by(|left, right| {
        left.size
            .cmp(&right.size)
            .then_with(|| left.offset.cmp(&right.offset))
    });
    write_offsets(file, stubs)
}

fn write_offsets(file: &mut File, stubs: &[SortStub]) -> io::Result<()> {
    for stub in stubs {
        file.write_all(&stub.offset.to_le_bytes())?;
    }
    Ok(())
}

fn compare_optional(left: Option<i64>, right: Option<i64>) -> Ordering {
    match (left, right) {
        (None, None) => Ordering::Equal,
        (None, _) => Ordering::Greater,
        (_, None) => Ordering::Less,
        (Some(left), Some(right)) => left.cmp(&right),
    }
}

fn sqlite_io(error: rusqlite::Error) -> io::Error {
    io::Error::other(error)
}

struct ProjectionCandidates<'a> {
    map: &'a [u8],
    directories: &'a [(usize, usize)],
    normalized_directory_bytes: &'a [u8],
    normalized_directories: &'a [(usize, usize)],
    remaining: u32,
    offset: usize,
}

impl<'a> Iterator for ProjectionCandidates<'a> {
    type Item = BorrowedQueryCandidate<'a>;

    #[inline]
    fn next(&mut self) -> Option<Self::Item> {
        if self.remaining == 0 {
            return None;
        }
        let offset = self.offset;
        let (candidate, next_offset) = candidate_at(
            self.map,
            self.directories,
            self.normalized_directory_bytes,
            self.normalized_directories,
            offset,
        );
        self.offset = next_offset;
        self.remaining -= 1;
        Some(candidate)
    }
}

fn candidate_at<'a>(
    map: &'a [u8],
    directories: &'a [(usize, usize)],
    normalized_directory_bytes: &'a [u8],
    normalized_directories: &'a [(usize, usize)],
    offset: usize,
) -> (BorrowedQueryCandidate<'a>, usize) {
    let entry_id = read_u64(map, offset).expect("projection was validated");
    let size = read_u64(map, offset + 8).expect("projection was validated");
    let raw_created = read_i64(map, offset + 16).expect("projection was validated");
    let raw_modified = read_i64(map, offset + 24).expect("projection was validated");
    let directory_id = read_u32(map, offset + 32).expect("projection was validated") as usize;
    let name_len = read_u16(map, offset + 36).expect("projection was validated") as usize;
    let normalized_name_len =
        read_u16(map, offset + 38).expect("projection was validated") as usize;
    let hidden = map[offset + 40] != 0;
    let name = read_str(map, offset + 41, name_len).expect("projection was validated");
    let normalized_name = read_str(map, offset + 41 + name_len, normalized_name_len)
        .expect("projection was validated");
    let (directory_offset, directory_len) = directories[directory_id];
    let parent_path =
        read_str(map, directory_offset, directory_len).expect("projection was validated");
    let (normalized_offset, normalized_len) = normalized_directories[directory_id];
    let normalized_parent_path = read_str(
        normalized_directory_bytes,
        normalized_offset,
        normalized_len,
    )
    .expect("normalized directory is valid UTF-8");
    (
        BorrowedQueryCandidate {
            entry_id,
            name,
            normalized_name,
            parent_path,
            normalized_parent_path,
            size,
            created_ns: (raw_created != i64::MIN).then_some(raw_created),
            modified_ns: (raw_modified != i64::MIN).then_some(raw_modified),
            hidden,
        },
        offset + 41 + name_len + normalized_name_len,
    )
}

fn temporary_sibling(path: &Path) -> PathBuf {
    path.with_extension(format!("tmp-{}", std::process::id()))
}

fn read_directories(map: &[u8], count: u32) -> io::Result<(Vec<(usize, usize)>, usize)> {
    let mut directories = Vec::with_capacity(count as usize);
    let mut offset = HEADER_LEN;
    for _ in 0..count {
        let len = read_u32(map, offset)? as usize;
        let start = offset
            .checked_add(4)
            .ok_or_else(|| invalid("directory offset overflow"))?;
        read_str(map, start, len)?;
        directories.push((start, len));
        offset = start
            .checked_add(len)
            .ok_or_else(|| invalid("directory offset overflow"))?;
    }
    Ok((directories, offset))
}

fn validate_records(
    map: &[u8],
    mut offset: usize,
    count: u32,
    directory_count: u32,
) -> io::Result<usize> {
    for _ in 0..count {
        let directory_id = read_u32(map, offset + 32)?;
        if directory_id >= directory_count {
            return Err(invalid("projection directory id is invalid"));
        }
        let name_len = read_u16(map, offset + 36)? as usize;
        let normalized_name_len = read_u16(map, offset + 38)? as usize;
        offset = offset
            .checked_add(41)
            .and_then(|value| value.checked_add(name_len))
            .and_then(|value| value.checked_add(normalized_name_len))
            .ok_or_else(|| invalid("projection offsets overflow"))?;
        if offset > map.len() {
            return Err(invalid("projection record exceeds file"));
        }
        let name_start = offset - normalized_name_len - name_len;
        read_str(map, name_start, name_len)?;
        read_str(map, name_start + name_len, normalized_name_len)?;
    }
    let expected_len = offset
        .checked_add(count as usize * std::mem::size_of::<u32>() * SORT_INDEX_COUNT)
        .ok_or_else(|| invalid("projection sort index overflows"))?;
    if expected_len != map.len() {
        return Err(invalid("projection sort indexes have the wrong size"));
    }
    Ok(offset)
}

#[inline]
fn read_u32(bytes: &[u8], offset: usize) -> io::Result<u32> {
    Ok(u32::from_le_bytes(read_array(bytes, offset)?))
}

#[inline]
fn read_u16(bytes: &[u8], offset: usize) -> io::Result<u16> {
    Ok(u16::from_le_bytes(read_array(bytes, offset)?))
}

#[inline]
fn read_u64(bytes: &[u8], offset: usize) -> io::Result<u64> {
    Ok(u64::from_le_bytes(read_array(bytes, offset)?))
}

#[inline]
fn read_i64(bytes: &[u8], offset: usize) -> io::Result<i64> {
    Ok(i64::from_le_bytes(read_array(bytes, offset)?))
}

#[inline]
fn read_array<const N: usize>(bytes: &[u8], offset: usize) -> io::Result<[u8; N]> {
    bytes
        .get(offset..offset + N)
        .ok_or_else(|| invalid("projection is truncated"))?
        .try_into()
        .map_err(|_| invalid("projection field has wrong size"))
}

#[inline]
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

    #[test]
    fn empty_query_sort_indexes_match_the_reference_ranker() {
        let fixture = tempdir().unwrap();
        for name in ["z.txt", "Alpha.txt", "medium-name.txt", "beta.txt"] {
            fs::write(fixture.path().join(name), name).unwrap();
        }
        let data = tempdir().unwrap();
        let mut store = IndexStore::open(&data.path().join("index.sqlite3")).unwrap();
        store
            .commit_scan(&scan_root(fixture.path()).unwrap())
            .unwrap();
        let mut committed = store.latest_committed().unwrap().unwrap();
        for (index, entry) in committed.entries.iter_mut().enumerate() {
            entry.size = index as u64;
            entry.created_ns = (index != 1).then_some(index as i64);
            entry.modified_ns = (index != 2).then_some((10 - index) as i64);
        }
        let projection =
            SearchProjection::build(&data.path().join("sort.projection"), &committed).unwrap();
        for field in [
            crate::query::SortField::Relevance,
            crate::query::SortField::ModificationTime,
            crate::query::SortField::CreationTime,
            crate::query::SortField::FileName,
            crate::query::SortField::FullPath,
            crate::query::SortField::FileSize,
        ] {
            let sort = SortOrder {
                field,
                direction: crate::query::SortDirection::Ascending,
            };
            let expected = crate::query::rank_candidates_with_options(
                "",
                committed
                    .entries
                    .iter()
                    .map(|entry| crate::query::QueryCandidate {
                        result: SearchResult {
                            entry_id: entry.entry_id,
                            name: entry.name.clone(),
                            path: entry.path.clone(),
                            size: entry.size,
                            created_ns: entry.created_ns,
                            modified_ns: entry.modified_ns,
                        },
                        normalized_name: normalize_search_text(&entry.name),
                        normalized_path: normalize_search_text(&entry.path.to_string_lossy()),
                        hidden: entry.hidden,
                    }),
                &HashMap::new(),
                100,
                sort,
                &CancellationToken::default(),
            );
            let actual = projection
                .search_ranked(
                    "",
                    &HashMap::new(),
                    100,
                    sort,
                    &CancellationToken::default(),
                )
                .unwrap();
            assert_eq!(actual.rows, expected.rows, "sort {field:?}");
        }
    }
}

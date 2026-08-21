use std::collections::HashMap;
use std::fs::{self, File};
use std::io::{self, Write};
use std::path::{Path, PathBuf};

use memmap2::{Mmap, MmapOptions};

use crate::index::CommittedIndex;
use crate::model::SearchResult;
use crate::query::{QueryCandidate, normalize_search_text, rank_candidates};

const MAGIC: &[u8; 8] = b"EVFLIDX\0";
const VERSION: u32 = 2;
const HEADER_LEN: usize = 24;

pub struct SearchProjection {
    map: Mmap,
    generation: u64,
    record_count: u32,
}

impl SearchProjection {
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
            file.write_all(&entry.modified_ns.unwrap_or(i64::MIN).to_le_bytes())?;
            file.write_all(&(name.len() as u32).to_le_bytes())?;
            file.write_all(&(path.len() as u32).to_le_bytes())?;
            file.write_all(&(normalized_name.len() as u32).to_le_bytes())?;
            file.write_all(&(normalized_path.len() as u32).to_le_bytes())?;
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
        let mut candidates = Vec::with_capacity(self.record_count as usize);
        let mut offset = HEADER_LEN;
        for _ in 0..self.record_count {
            let entry_id = read_u64(&self.map, offset)?;
            let size = read_u64(&self.map, offset + 8)?;
            let raw_modified = read_i64(&self.map, offset + 16)?;
            let name_len = read_u32(&self.map, offset + 24)? as usize;
            let path_len = read_u32(&self.map, offset + 28)? as usize;
            let normalized_name_len = read_u32(&self.map, offset + 32)? as usize;
            let normalized_path_len = read_u32(&self.map, offset + 36)? as usize;
            offset += 40;
            let name = read_str(&self.map, offset, name_len)?;
            offset += name_len;
            let path = read_str(&self.map, offset, path_len)?;
            offset += path_len;
            let normalized_name = read_str(&self.map, offset, normalized_name_len)?;
            offset += normalized_name_len;
            let normalized_path = read_str(&self.map, offset, normalized_path_len)?;
            offset += normalized_path_len;
            candidates.push(QueryCandidate {
                result: SearchResult {
                    entry_id,
                    name: name.to_owned(),
                    path: PathBuf::from(path),
                    size,
                    modified_ns: (raw_modified != i64::MIN).then_some(raw_modified),
                },
                normalized_name: normalized_name.to_owned(),
                normalized_path: normalized_path.to_owned(),
            });
        }
        Ok(rank_candidates(query, candidates, recent_opens, limit))
    }
}

fn temporary_sibling(path: &Path) -> PathBuf {
    path.with_extension(format!("tmp-{}", std::process::id()))
}

fn validate_records(map: &[u8], count: u32) -> io::Result<()> {
    let mut offset = HEADER_LEN;
    for _ in 0..count {
        let name_len = read_u32(map, offset + 24)? as usize;
        let path_len = read_u32(map, offset + 28)? as usize;
        let normalized_name_len = read_u32(map, offset + 32)? as usize;
        let normalized_path_len = read_u32(map, offset + 36)? as usize;
        offset = offset
            .checked_add(40)
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
}

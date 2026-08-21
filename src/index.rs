use std::path::{Path, PathBuf};
use std::{collections::HashMap, time::SystemTime};

use rusqlite::{Connection, OptionalExtension, params};

use crate::model::{Coverage, IndexedEntry, SkippedLocation};
use crate::scanner::ScanReport;

pub struct IndexStore {
    connection: Connection,
}

#[derive(Debug)]
pub struct CommittedIndex {
    pub generation: u64,
    pub volume_id: u64,
    pub root: PathBuf,
    pub coverage: Coverage,
    pub entries: Vec<IndexedEntry>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct VolumeCheckpoint {
    pub volume_id: u64,
    pub root: PathBuf,
    pub stream_identity: String,
    pub event_id: u64,
    pub generation: u64,
}

impl IndexStore {
    pub fn open(path: &Path) -> rusqlite::Result<Self> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)
                .map_err(|error| rusqlite::Error::ToSqlConversionFailure(Box::new(error)))?;
        }
        let connection = Connection::open(path)?;
        connection.pragma_update(None, "journal_mode", "WAL")?;
        connection.pragma_update(None, "foreign_keys", "ON")?;
        connection.execute_batch(SCHEMA)?;
        Ok(Self { connection })
    }

    pub fn commit_scan(&mut self, report: &ScanReport) -> rusqlite::Result<u64> {
        self.commit_scan_with_checkpoint(report, None)
    }

    pub fn commit_reconciliation(
        &mut self,
        report: &ScanReport,
        stream_identity: &str,
        event_id: u64,
    ) -> rusqlite::Result<u64> {
        self.commit_scan_with_checkpoint(report, Some((stream_identity, event_id)))
    }

    fn commit_scan_with_checkpoint(
        &mut self,
        report: &ScanReport,
        checkpoint: Option<(&str, u64)>,
    ) -> rusqlite::Result<u64> {
        let transaction = self.connection.transaction()?;
        let next_generation = transaction.query_row(
            "SELECT COALESCE(MAX(generation), 0) + 1 FROM scan_generations",
            [],
            |row| row.get::<_, i64>(0),
        )? as u64;
        transaction.execute(
            "INSERT INTO scan_generations (generation, volume_id, root_path, coverage, committed)
             VALUES (?1, ?2, ?3, ?4, 0)",
            params![
                next_generation as i64,
                report.volume_id as i64,
                report.root.to_string_lossy(),
                coverage_text(report.coverage())
            ],
        )?;
        {
            let mut insert = transaction.prepare_cached(
                "INSERT INTO entries
                 (generation, entry_id, volume_id, name, full_path, kind, size, created_ns, modified_ns, hidden)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10)",
            )?;
            for entry in &report.entries {
                insert.execute(params![
                    next_generation as i64,
                    entry.entry_id as i64,
                    entry.volume_id as i64,
                    entry.name,
                    entry.path.to_string_lossy(),
                    entry.kind.as_i64(),
                    entry.size as i64,
                    entry.created_ns,
                    entry.modified_ns,
                    entry.hidden
                ])?;
            }
        }
        insert_skips(&transaction, next_generation, &report.skipped)?;
        transaction.execute(
            "UPDATE scan_generations SET committed = 1 WHERE generation = ?1",
            [next_generation as i64],
        )?;
        transaction.execute(
            "INSERT INTO published_roots (volume_id, root_path, generation, coverage)
             VALUES (?1, ?2, ?3, ?4)
             ON CONFLICT(volume_id, root_path) DO UPDATE SET
                 generation = excluded.generation,
                 coverage = excluded.coverage",
            params![
                report.volume_id as i64,
                report.root.to_string_lossy(),
                next_generation as i64,
                coverage_text(report.coverage())
            ],
        )?;
        if let Some((stream_identity, event_id)) = checkpoint {
            transaction.execute(
                "INSERT INTO volume_checkpoints
                 (volume_id, root_path, stream_identity, event_id, generation)
                 VALUES (?1, ?2, ?3, ?4, ?5)
                 ON CONFLICT(volume_id, root_path) DO UPDATE SET
                     stream_identity = excluded.stream_identity,
                     event_id = excluded.event_id,
                     generation = excluded.generation",
                params![
                    report.volume_id as i64,
                    report.root.to_string_lossy(),
                    stream_identity,
                    event_id as i64,
                    next_generation as i64,
                ],
            )?;
        }
        transaction.commit()?;
        Ok(next_generation)
    }

    pub fn checkpoint(
        &self,
        volume_id: u64,
        root: &Path,
    ) -> rusqlite::Result<Option<VolumeCheckpoint>> {
        self.connection
            .query_row(
                "SELECT stream_identity, event_id, generation FROM volume_checkpoints
                 WHERE volume_id = ?1 AND root_path = ?2",
                params![volume_id as i64, root.to_string_lossy()],
                |row| {
                    Ok(VolumeCheckpoint {
                        volume_id,
                        root: root.to_path_buf(),
                        stream_identity: row.get(0)?,
                        event_id: row.get::<_, i64>(1)? as u64,
                        generation: row.get::<_, i64>(2)? as u64,
                    })
                },
            )
            .optional()
    }

    pub fn latest_committed(&self) -> rusqlite::Result<Option<CommittedIndex>> {
        let row: Option<(i64, i64, String, String)> = self
            .connection
            .query_row(
                "SELECT generation, volume_id, root_path, coverage
                 FROM published_roots ORDER BY generation DESC LIMIT 1",
                [],
                |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?)),
            )
            .optional()?;
        let Some((generation, volume_id, root, coverage)) = row else {
            return Ok(None);
        };
        let generation = generation as u64;
        let volume_id = volume_id as u64;
        let mut statement = self.connection.prepare(
            "SELECT entry_id, volume_id, name, full_path, kind, size, created_ns, modified_ns, hidden
             FROM entries WHERE generation = ?1 ORDER BY full_path",
        )?;
        let entries = statement
            .query_map([generation as i64], |row| {
                Ok(IndexedEntry {
                    entry_id: row.get::<_, i64>(0)? as u64,
                    volume_id: row.get::<_, i64>(1)? as u64,
                    name: row.get(2)?,
                    path: PathBuf::from(row.get::<_, String>(3)?),
                    kind: match row.get::<_, i64>(4)? {
                        1 => crate::model::EntryKind::File,
                        2 => crate::model::EntryKind::Directory,
                        3 => crate::model::EntryKind::Symlink,
                        _ => crate::model::EntryKind::Other,
                    },
                    size: row.get::<_, i64>(5)? as u64,
                    created_ns: row.get(6)?,
                    modified_ns: row.get(7)?,
                    hidden: row.get(8)?,
                })
            })?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        Ok(Some(CommittedIndex {
            generation,
            volume_id,
            root: PathBuf::from(root),
            coverage: if coverage == "complete" {
                Coverage::Complete
            } else {
                Coverage::Partial
            },
            entries,
        }))
    }

    pub fn record_successful_open(&self, entry_id: u64) -> rusqlite::Result<()> {
        let opened_ns = SystemTime::now()
            .duration_since(SystemTime::UNIX_EPOCH)
            .unwrap_or_default()
            .as_nanos()
            .min(i64::MAX as u128) as i64;
        self.connection.execute(
            "INSERT INTO recent_opens (entry_id, opened_ns) VALUES (?1, ?2)
             ON CONFLICT(entry_id) DO UPDATE SET opened_ns = excluded.opened_ns",
            params![entry_id as i64, opened_ns],
        )?;
        Ok(())
    }

    pub fn recent_opens(&self) -> rusqlite::Result<HashMap<u64, u64>> {
        let mut statement = self
            .connection
            .prepare("SELECT entry_id, opened_ns FROM recent_opens")?;
        statement
            .query_map([], |row| {
                Ok((row.get::<_, i64>(0)? as u64, row.get::<_, i64>(1)? as u64))
            })?
            .collect()
    }

    pub fn clear_open_history(&self) -> rusqlite::Result<()> {
        self.connection.execute("DELETE FROM recent_opens", [])?;
        Ok(())
    }
}

fn coverage_text(coverage: Coverage) -> &'static str {
    match coverage {
        Coverage::Complete => "complete",
        Coverage::Partial => "partial",
    }
}

fn insert_skips(
    transaction: &rusqlite::Transaction<'_>,
    generation: u64,
    skipped: &[SkippedLocation],
) -> rusqlite::Result<()> {
    let mut insert = transaction.prepare_cached(
        "INSERT INTO skipped_locations (generation, path, reason) VALUES (?1, ?2, ?3)",
    )?;
    for location in skipped {
        insert.execute(params![
            generation as i64,
            location.path.to_string_lossy(),
            location.reason
        ])?;
    }
    Ok(())
}

const SCHEMA: &str = "
CREATE TABLE IF NOT EXISTS scan_generations (
    generation INTEGER PRIMARY KEY,
    volume_id INTEGER NOT NULL,
    root_path TEXT NOT NULL,
    coverage TEXT NOT NULL CHECK (coverage IN ('complete', 'partial')),
    committed INTEGER NOT NULL CHECK (committed IN (0, 1))
);
CREATE TABLE IF NOT EXISTS entries (
    generation INTEGER NOT NULL REFERENCES scan_generations(generation) ON DELETE CASCADE,
    entry_id INTEGER NOT NULL,
    volume_id INTEGER NOT NULL,
    name TEXT NOT NULL,
    full_path TEXT NOT NULL,
    kind INTEGER NOT NULL,
    size INTEGER NOT NULL,
    created_ns INTEGER,
    modified_ns INTEGER,
    hidden INTEGER NOT NULL,
    PRIMARY KEY (generation, entry_id, full_path)
);
CREATE INDEX IF NOT EXISTS entries_generation_path ON entries(generation, full_path);
CREATE TABLE IF NOT EXISTS skipped_locations (
    generation INTEGER NOT NULL REFERENCES scan_generations(generation) ON DELETE CASCADE,
    path TEXT NOT NULL,
    reason TEXT NOT NULL
);
CREATE TABLE IF NOT EXISTS published_roots (
    volume_id INTEGER NOT NULL,
    root_path TEXT NOT NULL,
    generation INTEGER NOT NULL REFERENCES scan_generations(generation),
    coverage TEXT NOT NULL CHECK (coverage IN ('complete', 'partial')),
    PRIMARY KEY (volume_id, root_path)
);
CREATE TABLE IF NOT EXISTS recent_opens (
    entry_id INTEGER PRIMARY KEY,
    opened_ns INTEGER NOT NULL
);
CREATE TABLE IF NOT EXISTS volume_checkpoints (
    volume_id INTEGER NOT NULL,
    root_path TEXT NOT NULL,
    stream_identity TEXT NOT NULL,
    event_id INTEGER NOT NULL,
    generation INTEGER NOT NULL REFERENCES scan_generations(generation),
    PRIMARY KEY (volume_id, root_path)
);
";

#[cfg(test)]
mod tests {
    use std::fs;

    use tempfile::tempdir;

    use super::*;
    use crate::scanner::scan_root;

    #[test]
    fn committed_scan_survives_reopen() {
        let fixture = tempdir().unwrap();
        fs::write(fixture.path().join("report.txt"), "report").unwrap();
        let data = tempdir().unwrap();
        let database = data.path().join("index.sqlite3");
        let report = scan_root(fixture.path()).unwrap();

        let generation = IndexStore::open(&database)
            .unwrap()
            .commit_scan(&report)
            .unwrap();
        let committed = IndexStore::open(&database)
            .unwrap()
            .latest_committed()
            .unwrap()
            .unwrap();

        assert_eq!(committed.generation, generation);
        assert_eq!(committed.entries.len(), 1);
        assert_eq!(committed.entries[0].name, "report.txt");
        assert_eq!(committed.coverage, Coverage::Complete);
    }

    #[test]
    fn failed_replacement_keeps_the_previous_generation_published() {
        let fixture = tempdir().unwrap();
        fs::write(fixture.path().join("first.txt"), "first").unwrap();
        let data = tempdir().unwrap();
        let database = data.path().join("index.sqlite3");
        let mut store = IndexStore::open(&database).unwrap();
        let first = scan_root(fixture.path()).unwrap();
        let first_generation = store.commit_scan(&first).unwrap();

        fs::write(fixture.path().join("second.txt"), "second").unwrap();
        let mut invalid_replacement = scan_root(fixture.path()).unwrap();
        invalid_replacement
            .entries
            .push(invalid_replacement.entries[0].clone());
        assert!(store.commit_scan(&invalid_replacement).is_err());

        let published = store.latest_committed().unwrap().unwrap();
        assert_eq!(published.generation, first_generation);
        assert_eq!(published.entries.len(), 1);
        assert_eq!(published.entries[0].name, "first.txt");
    }

    #[test]
    fn successful_open_history_persists_and_clears() {
        let data = tempdir().unwrap();
        let database = data.path().join("index.sqlite3");
        IndexStore::open(&database)
            .unwrap()
            .record_successful_open(42)
            .unwrap();
        let reopened = IndexStore::open(&database).unwrap();
        assert!(reopened.recent_opens().unwrap().contains_key(&42));
        reopened.clear_open_history().unwrap();
        assert!(reopened.recent_opens().unwrap().is_empty());
    }

    #[test]
    fn reconciliation_commits_generation_and_checkpoint_together() {
        let fixture = tempdir().unwrap();
        fs::write(fixture.path().join("first.txt"), "first").unwrap();
        let data = tempdir().unwrap();
        let database = data.path().join("index.sqlite3");
        let mut store = IndexStore::open(&database).unwrap();
        let report = scan_root(fixture.path()).unwrap();
        let generation = store
            .commit_reconciliation(&report, "volume-identity", 42)
            .unwrap();

        let checkpoint = store
            .checkpoint(report.volume_id, fixture.path())
            .unwrap()
            .unwrap();
        assert_eq!(checkpoint.event_id, 42);
        assert_eq!(checkpoint.generation, generation);
        assert_eq!(checkpoint.stream_identity, "volume-identity");
    }

    #[test]
    fn failed_reconciliation_does_not_advance_checkpoint() {
        let fixture = tempdir().unwrap();
        fs::write(fixture.path().join("first.txt"), "first").unwrap();
        let data = tempdir().unwrap();
        let database = data.path().join("index.sqlite3");
        let mut store = IndexStore::open(&database).unwrap();
        let first = scan_root(fixture.path()).unwrap();
        store
            .commit_reconciliation(&first, "volume-identity", 7)
            .unwrap();

        let mut invalid = scan_root(fixture.path()).unwrap();
        invalid.entries.push(invalid.entries[0].clone());
        assert!(
            store
                .commit_reconciliation(&invalid, "volume-identity", 99)
                .is_err()
        );
        assert_eq!(
            store
                .checkpoint(first.volume_id, fixture.path())
                .unwrap()
                .unwrap()
                .event_id,
            7
        );
    }
}

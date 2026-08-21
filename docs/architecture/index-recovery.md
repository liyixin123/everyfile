# File Index recovery

Startup verifies SQLite with `PRAGMA quick_check(1)` and rejects schema versions
newer than the running application before any Search Projection is published. A
failed check moves the database and its WAL/SHM sidecars to a unique
`index.damaged-<timestamp>.sqlite3` diagnostic archive, then creates a new File
Index.

Recovery begins with no projection, reports `Rebuilding` with observed-entry
progress, and publishes only the generation produced by the successful replacement
scan. If preservation or replacement creation fails, the error remains visible and
no old results are treated as verified. Normal reconciliation continues to update
entries, the published generation, Coverage, and its FSEvents checkpoint in one
SQLite transaction, so interruption cannot expose partial mutations or advance an
unapplied cursor.

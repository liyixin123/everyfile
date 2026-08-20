# First searchable File Index slice

## End-to-end flow

The first searchable slice follows the component boundaries selected in issue #5:

1. Volume Catalog reads native macOS mount information and identifies the deepest mounted volume containing the configured root.
2. Scanner enumerates that real root on a bounded worker without following directory symlinks or crossing its filesystem device boundary.
3. Index Store commits a new scan generation, entries, skipped locations, Coverage, and publication pointer in SQLite WAL mode.
4. Query Engine writes a versioned search projection to a sibling temporary file, syncs it, atomically renames it, and opens it read-only through `mmap`.
5. Quick Search reads the immutable projection and publishes at most 100 rows to the native table.

SQLite is the only durable File Index. On restart, a matching committed root is searchable without rescanning. A missing, invalid, or stale projection is rebuilt from SQLite.

## Truthful state

Before a configured root exists, state remains `No File Index`. During enumeration it is `File Index: Rebuilding` with the observed entry count. Only a committed generation with a validated projection becomes `File Index: Current`. Coverage is Complete only when enumeration recorded no skipped/error location; otherwise it is Partial.

The same state appears in the Quick Search Window and Menu Bar Control. Typing never starts a scan. In this slice, Return submits the Search Query; query-as-you-type and cancellation belong to the later query tickets.

## Controlled-root acceptance seam

Set `EVERYFILE_INDEX_ROOT` to a permitted directory on an internal volume and optionally set `EVERYFILE_DATA_DIR` to an isolated data directory before launching the executable. These variables are a development and integration seam, not an end-user volume-selection surface.

The highest-level repeatable test creates real files beneath the controlled root and verifies Scanner → SQLite → projection → Search Query behavior. Lower tests cover mount selection, symlink non-traversal, transaction rollback, projection validation/rebuild, and process-independent SQLite reopen.

## Deferred behavior

FSEvents, complete Unicode/diacritic normalization, AND-term Relevance, query cancellation, external volumes, Full Disk Access transitions, package rules, hidden-item toggles, resource pressure, and final performance qualification remain assigned to their dependent tickets.

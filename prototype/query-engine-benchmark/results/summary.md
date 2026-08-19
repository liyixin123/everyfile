# Captured result: 1000000 entries

## Recommendation for human review

Use the compact in-memory projection behind the replaceable Query Engine interface for the MVP. It streams relevance results immediately and is the closest candidate on deliberately high-match scans, while FTS5 trigram exceeded 100 ms on common 3+ character queries and used much more disk. The prototype narrowly misses 100 ms for some complete scans and selected sorts, so the implementation decision must include a release-build gate for parallel scan/top-K optimization. SQLite remains the only persistent File Index; the projection is a rebuildable cache, not a second source of truth.

This is not the ticket resolution. The choice remains HITL. In this run, median FTS5 complete latency for supported 3+ character queries ranged up to 384.46 ms, and compact-memory first result for 1–2 character queries ranged up to 0.00 ms.

## Build, startup, size, memory

| metric | value | unit | note |
|---|---|---|---|
| build_time | 5793.046 | ms | SQLite plus FTS5 plus projection |
| sqlite_startup | 23.601 | ms | warm open and count |
| projection_load | 12.475 | ms | read two contiguous arrays |
| sqlite_disk | 352501760 | bytes | checkpointed database |
| projection_disk | 101099194 | bytes | fixed records plus string arena |
| process_current_rss | 115008 | KiB | resident set after loading projection |
| process_peak_rss | 124192 | KiB | conservative upper bound including build |

## Query medians (five measured runs after one warm-up)

| engine | case | query | sort | matches | first_ms | full_ms |
|---|---|---|---|---|---|---|
| memory | one-char | r | relevance | 937500 | 0.001 | 105.925 |
| memory | two-char | re | relevance | 375000 | 0.001 | 96.190 |
| memory | substring | port | relevance | 250000 | 0.001 | 101.296 |
| memory | multi-term | report pdf | modified | 20834 | 108.424 | 109.535 |
| memory | unicode | résumé | relevance | 62500 | 0.001 | 115.889 |
| memory | case-normalized | readme | relevance | 62500 | 0.002 | 117.347 |
| fts5 | substring | port | relevance | 250000 | 374.882 | 384.465 |
| fts5 | multi-term | report pdf | modified | 20834 | 53.972 | 57.446 |
| fts5 | unicode | résumé | relevance | 62500 | 119.052 | 116.530 |
| fts5 | case-normalized | readme | relevance | 62500 | 115.157 | 115.203 |
| hybrid | one-char | r | relevance | 937500 | 0.001 | 99.167 |
| hybrid | two-char | re | relevance | 375000 | 0.001 | 98.134 |
| hybrid | substring | port | relevance | 250000 | 402.774 | 475.251 |
| hybrid | multi-term | report pdf | modified | 20834 | 58.417 | 56.187 |
| hybrid | unicode | résumé | relevance | 62500 | 115.900 | 118.603 |
| hybrid | case-normalized | readme | relevance | 62500 | 116.847 | 122.529 |
| memory | selectable-sort | report | name | 62500 | 109.315 | 138.038 |
| hybrid | selectable-sort | report | name | 62500 | 145.982 | 152.007 |
| memory | selectable-sort | report | path | 62500 | 108.720 | 122.423 |
| hybrid | selectable-sort | report | path | 62500 | 140.883 | 140.882 |
| memory | selectable-sort | report | modified | 62500 | 102.968 | 106.498 |
| hybrid | selectable-sort | report | modified | 62500 | 118.454 | 116.747 |
| memory | selectable-sort | report | created | 62500 | 100.837 | 105.401 |
| hybrid | selectable-sort | report | created | 62500 | 116.287 | 119.613 |
| memory | selectable-sort | report | size | 62500 | 105.934 | 114.977 |
| hybrid | selectable-sort | report | size | 62500 | 123.508 | 120.785 |

## Incremental updates

| engine | case | matches | full_ms | note |
|---|---|---|---|---|
| sqlite+fts5 | batch-1 | 1 | 3.341 | transactional searchable updates |
| memory | batch-1 | 1 | 0.000 | in-place cache updates after transaction |
| sqlite+fts5 | batch-100 | 100 | 1.049 | transactional searchable updates |
| memory | batch-100 | 100 | 0.025 | in-place cache updates after transaction |
| sqlite+fts5 | batch-10000 | 10000 | 84.964 | transactional searchable updates |
| memory | batch-10000 | 10000 | 0.184 | in-place cache updates after transaction |

## Interpretation

- FTS5 trigram cannot directly answer one- or two-character MATCH queries. Falling back to SQLite `LIKE '%x%'` violates the design intent because it scans persistent rows and was slower than the compact projection.
- The compact projection has low startup cost when memory-mapped/read as two contiguous arrays, and it handles short queries predictably. Its full scan scales linearly; at one million entries this run remains near the provisional line, so larger File Index sizes require another benchmark.
- FTS5 is effective for selective 3+ character queries, but its rank ordering crossed 100 ms for common substrings in this corpus. The measured hybrid inherits that slow first-result path and is therefore rejected for the MVP.
- The compact projection is the only candidate that handles every query length through one rule and stays close to the target for complete high-match scans. A selected metadata sort must scan all matches before its first correct row; those measurements appear in the selectable-sort rows.
- Unicode normalization is done once at ingestion and once at the query boundary with NFC plus Unicode case-fold. `verify_normalization.py` covers composed/decomposed accents and Straße/STRASSE equivalence.
- All selectable sorts end with Entry ID as a stable tie-break. Modified time, created time, size, name, and path do not require separate FTS indexes when applied to the bounded candidate set.
- `fd` is not a runtime dependency or Query Engine candidate. The synthetic corpus does not create one million filesystem nodes, so no misleading `fd` timing is included. A later real-corpus run can record no-index parallel traversal only as an informational baseline.

## Follow-up threshold

Keep this design only if a release-build Rust implementation on a real corpus stays under 100 ms for first result, under the agreed memory budget, and does not make short-query full-result scans visible in the Quick Search Window. Use bounded parallel scan only while a query is active and top-K selection instead of fully sorting large hit sets. Memory-map the rebuildable projection so the OS can reclaim pages. If memory is too high, disable 1-character live results or add an mmap gram table rather than adopting the measured FTS layout unchanged.

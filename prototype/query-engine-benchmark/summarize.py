#!/usr/bin/env python3
import csv, pathlib, sys

rows_n, source, target = sys.argv[1:]
rows = list(csv.DictReader(open(source, newline="")))
meta = {r["metric"]: r["value"] for r in rows if r["engine"] == "meta"}
queries = [r for r in rows if r["metric"] == "query"]
updates = [r for r in rows if r["metric"] == "update"]

def table(items, cols):
    out = ["| " + " | ".join(cols) + " |", "|" + "|".join(["---"] * len(cols)) + "|"]
    out += ["| " + " | ".join(r.get(c, "") or "" for c in cols) + " |" for r in items]
    return "\n".join(out)

fts_good = [float(r["full_ms"]) for r in queries if r["engine"] == "fts5" and int(r["query_len"]) >= 3]
mem_short = [float(r["first_ms"]) for r in queries if r["engine"] == "memory" and int(r["query_len"]) <= 2]
recommendation = (
    "Use the compact in-memory projection behind the replaceable Query Engine interface for the MVP. "
    "It streams relevance results immediately and is the closest candidate on deliberately high-match scans, "
    "while FTS5 trigram exceeded 100 ms on common 3+ character queries and used much more disk. "
    "The prototype narrowly misses 100 ms for some complete scans and selected sorts, so the implementation decision must include a release-build gate for parallel scan/top-K optimization. "
    "SQLite remains the only persistent File Index; the projection is a rebuildable cache, not a second source of truth."
)
text = f"""# Captured result: {rows_n} entries

## Recommendation for human review

{recommendation}

This is not the ticket resolution. The choice remains HITL. In this run, median FTS5 complete latency for supported 3+ character queries ranged up to {max(fts_good):.2f} ms, and compact-memory first result for 1–2 character queries ranged up to {max(mem_short):.2f} ms.

## Build, startup, size, memory

{table([r for r in rows if r['engine'] == 'meta'], ['metric','value','unit','note'])}

## Query medians (five measured runs after one warm-up)

{table(queries, ['engine','case','query','sort','matches','first_ms','full_ms'])}

## Incremental updates

{table(updates, ['engine','case','matches','full_ms','note'])}

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
"""
pathlib.Path(target).write_text(text)

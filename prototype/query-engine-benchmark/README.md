# Query Engine benchmark (throwaway prototype)

Question: which replaceable Query Engine should Everyfile use for one million File Index entries while keeping first results below 100 ms and resource use low?

This prototype compares:

1. SQLite FTS5 `trigram`, with normalized name and path columns.
2. A compact in-memory projection: one contiguous string arena plus fixed-size entry records.
3. A hybrid: the in-memory projection for 1–2 character queries, FTS5 candidates for longer queries, then stable sorting against compact metadata.

The benchmark is deterministic and intentionally throwaway. It uses synthetic macOS-style paths with a controlled mix of documents, source trees, app-like names, spaces, dotfiles, and pre-normalized Unicode names. Normalization is an ingestion/query-boundary concern, so `verify_normalization.py` separately verifies NFC plus Unicode case-fold behavior.

## Run

Requires the macOS system SQLite library with FTS5 and a C compiler.

```bash
cd prototype/query-engine-benchmark
./run.sh
```

The full run creates one million entries. Set `EF_BENCH_ROWS` for a smoke run:

```bash
EF_BENCH_ROWS=100000 ./run.sh
```

Raw CSV, environment data, SQLite files, and the binary projection are written under `results/`. Generated data is ignored by Git; the committed `results/summary.md` and `results/raw.csv` are the captured primary result.

## Measurements

- build time and warm startup/load time
- first-result and complete-result latency
- one-character, two-character, substring, multi-term, Unicode, and case-normalized queries
- stable selectable sorting by relevance/name/path/modified time/created time/size
- incremental batches of 1, 100, and 10,000 updates
- process resident memory and SQLite/projection disk size

`first_ms` stops at the first matching ID. `full_ms` materializes every match and applies the selected stable sort. Each timed query is warmed once and measured five times; CSV reports the median.

## Limits

Synthetic data cannot predict a user's exact path distribution or APFS cache state. This prototype chooses an MVP engine boundary; production work must retain the benchmark and repeat it on a real exported name/path corpus. The compact scan is deliberately simple (`strstr`) and sets a useful lower-complexity baseline, not a final fuzzy ranker.

`fd` is intentionally not a candidate and is not invoked by Everyfile. This benchmark does not materialize one million real filesystem nodes, so timing `fd` against the synthetic corpus would be false precision. A later real-corpus benchmark may record `fd` or an equivalent no-index parallel traversal as an informational baseline only. Its parallel-walk implementation ideas or Rust crates may be studied separately; the shipped Query Engine must not depend on the `fd` CLI.

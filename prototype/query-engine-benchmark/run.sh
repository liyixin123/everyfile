#!/bin/sh
set -eu
cd "$(dirname "$0")"
mkdir -p results
cc -O3 -DNDEBUG -Wall -Wextra benchmark.c -lsqlite3 -o bench
python3 verify_normalization.py
ROWS="${EF_BENCH_ROWS:-1000000}"
./bench "$ROWS" results/file-index.db results/projection.bin results/raw.csv
{
  uname -a
  sysctl -n machdep.cpu.brand_string 2>/dev/null || true
  sqlite3 --version
  cc --version | head -1
} > results/environment.txt
./summarize.py "$ROWS" results/raw.csv results/summary.md
cat results/summary.md

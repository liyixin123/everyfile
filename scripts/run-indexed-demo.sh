#!/bin/sh
set -eu

if [ "$#" -ne 1 ]; then
    echo "usage: $0 <permitted-root-on-an-internal-volume>" >&2
    exit 2
fi

repo_dir=$(CDPATH= cd -- "$(dirname -- "$0")/.." && pwd)
root=$(CDPATH= cd -- "$1" && pwd)
data_dir=${EVERYFILE_DEMO_DATA_DIR:-"${TMPDIR:-/tmp}/everyfile-indexed-demo"}
bundle=$($repo_dir/scripts/build-app.sh release | tail -n 1)

mkdir -p "$data_dir"
echo "index root: $root"
echo "data directory: $data_dir"
EVERYFILE_INDEX_ROOT="$root" EVERYFILE_DATA_DIR="$data_dir" \
    "$bundle/Contents/MacOS/Everyfile"

#!/bin/sh
set -eu

repo_dir=$(CDPATH= cd -- "$(dirname -- "$0")/.." && pwd)
bundle=$($repo_dir/scripts/build-app.sh release | tail -n 1)
sample_count=${EVERYFILE_SMOKE_SAMPLES:-5}

echo "hardware=$(uname -m)"
echo "macos=$(sw_vers -productVersion)"
echo "profile=release"
echo "samples=$sample_count"

index=1
while [ "$index" -le "$sample_count" ]; do
    log_file=$(mktemp "${TMPDIR:-/tmp}/everyfile-smoke.XXXXXX")
    "$bundle/Contents/MacOS/Everyfile" 2>"$log_file" &
    app_pid=$!

    attempts=0
    while ! { rg -q 'event=application_ready' "$log_file" && rg -q 'event=quick_search_interactive' "$log_file"; }; do
        attempts=$((attempts + 1))
        if [ "$attempts" -ge 100 ]; then
            echo "sample=$index status=timeout"
            kill "$app_pid" 2>/dev/null || true
            wait "$app_pid" 2>/dev/null || true
            rm -f "$log_file"
            exit 1
        fi
        sleep 0.01
    done

    ready_ms=$(sed -n 's/.*event=application_ready elapsed_ms=\([0-9][0-9]*\).*/\1/p' "$log_file" | tail -n 1)
    interactive_us=$(sed -n 's/.*event=quick_search_interactive elapsed_us=\([0-9][0-9]*\).*/\1/p' "$log_file" | tail -n 1)
    sleep 1
    rss_kb=$(ps -o rss= -p "$app_pid" | tr -d ' ')
    cpu_percent=$(ps -o %cpu= -p "$app_pid" | tr -d ' ')
    echo "sample=$index ready_ms=$ready_ms interactive_us=$interactive_us rss_kb=$rss_kb cpu_percent=$cpu_percent"

    kill "$app_pid" 2>/dev/null || true
    wait "$app_pid" 2>/dev/null || true
    rm -f "$log_file"
    index=$((index + 1))
done

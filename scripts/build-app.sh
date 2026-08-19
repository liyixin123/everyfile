#!/bin/sh
set -eu

repo_dir=$(CDPATH= cd -- "$(dirname -- "$0")/.." && pwd)
profile=${1:-release}

case "$profile" in
    release)
        cargo_args="--release"
        output_dir="release"
        ;;
    debug)
        cargo_args=""
        output_dir="debug"
        ;;
    *)
        echo "usage: $0 [release|debug]" >&2
        exit 2
        ;;
esac

cd "$repo_dir"
# shellcheck disable=SC2086
cargo build $cargo_args

bundle="$repo_dir/target/$output_dir/Everyfile.app"
mkdir -p "$bundle/Contents/MacOS" "$bundle/Contents/Resources"
cp "$repo_dir/target/$output_dir/everyfile" "$bundle/Contents/MacOS/Everyfile"
cp "$repo_dir/resources/Info.plist" "$bundle/Contents/Info.plist"
touch "$bundle"
codesign --force --deep --sign - "$bundle"

echo "$bundle"

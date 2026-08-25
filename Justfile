set shell := ["zsh", "-cu"]

app := "target/release/Everyfile.app"
archive := "dist/Everyfile.zip"

# List available recipes.
default:
    @just --list

# Check formatting and run the complete test suite.
test:
    cargo fmt --check
    cargo test -- --test-threads=1

# Run fast compiler checks.
check:
    cargo check

# Format Rust sources.
fmt:
    cargo fmt

# Build and ad-hoc sign the release application bundle.
app:
    ./scripts/build-app.sh release

# Build and open the release application.
run: app
    open "{{app}}"

# Stop any currently running Everyfile application.
stop:
    @pkill -x Everyfile 2>/dev/null || true

# Test, build, and package the signed application as a ZIP archive.
package: test app
    mkdir -p dist
    ditto -c -k --sequesterRsrc --keepParent "{{app}}" "{{archive}}"
    @echo "Packaged {{archive}}"

# Build a DMG containing the signed release application.
dmg: app
    mkdir -p dist
    hdiutil create -volname Everyfile -srcfolder "{{app}}" -ov -format UDZO "dist/Everyfile.dmg"
    @echo "Packaged dist/Everyfile.dmg"

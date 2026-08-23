# Everyfile application shell baseline

## Runtime boundary

Everyfile is one Rust-owned macOS application process. AppKit and its objects stay on the main thread. The UI reads immutable application snapshots and submits future blocking work through a bounded background scheduler; background jobs never own AppKit objects.

The initial snapshot is deliberately `No File Index`. It does not claim Freshness or Coverage before #11 creates and verifies a durable File Index.

## Selected platform components

- AppKit through the maintained `objc2` bindings for the application, floating window, status item, menu, settings prompt, and dense table shell. On macOS 26, the Quick Search Window uses the native `NSGlassEffectView` Liquid Glass surface; macOS 15–25 use the equivalent dynamic `NSVisualEffectView` fallback without a hard class-link dependency.
- The macOS registered-hot-key facility for global shortcuts without an Accessibility permission dependency.
- Native user defaults for the selected shortcut.
- `rusqlite` with bundled SQLite as the later Index Store adapter. #10 does not create a schema or database.
- Dedicated Rust worker threads and bounded channels for the cross-thread scheduling contract. An async runtime is not an application-wide requirement.

## UI behavior

The centered Quick Search Window uses a layered Liquid Glass composition: a rounded outer surface, a clear glass search capsule, and a separate rounded results surface. Search receives focus immediately. Result cells and headers use platform label colors with truncation for long paths. File Index freshness and coverage are rendered as a compact two-line status hint below the results surface, so empty/indexing states never overlap search results.

The menu bar reports `File Index: Not Available`, opens Quick Search and settings, and quits. Settings let the user choose between two supported shortcut presets; failed registration retains the last working shortcut.

## Measurement boundary

Stable stderr events use monotonic elapsed measurements for application readiness and Quick Search interactivity. The smoke harness records architecture, macOS version, build profile, raw samples, resident memory, and CPU. Final percentile and million-entry qualification remains #21.

Full black-box accessibility automation requires a complete Xcode installation and test-host permission. The application remains Cargo-buildable with Command Line Tools alone.

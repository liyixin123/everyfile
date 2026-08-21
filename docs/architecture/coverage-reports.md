# Coverage Reports

Coverage is committed observation, not a permission setting. Every configured volume/root has a `RootCoverage` report containing its volume ID, canonical root, Complete or Partial state, and skipped locations with reasons. Overall Coverage is Complete only when at least one report exists and every configured report is Complete; no reports means Not Available.

Directory enumeration and metadata failures append a skipped location and scanning continues with accessible siblings. A denied root or child therefore produces Partial rather than preventing an otherwise usable File Index. Granting Full Disk Access has no immediate semantic effect by itself: Coverage changes only after normal enumeration observes the formerly skipped paths and a replacement generation commits.

Skipped locations are stored with the same SQLite generation as entries. Restart loads both from the published generation. Subtree reconciliation replaces skips inside repaired scopes and preserves committed skips outside them; a full reconciliation replaces the complete set. Entry mutations, skips, Coverage, and the FSEvents checkpoint publish atomically.

Menu Bar Control renders Freshness and Coverage separately, reports overall and configured-root Coverage, shows the skipped count, and exposes each skipped path and reason. Search remains available for accessible committed entries under Partial Coverage.

The scanner enumerates directory entries and reads metadata only. It never opens file contents, so locally enumerated cloud placeholders may appear without Everyfile requesting hydration.

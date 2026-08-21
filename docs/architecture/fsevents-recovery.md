# FSEvents Recovery

Everyfile binds a persistent event ID to the volume UUID returned by `FSEventsCopyUUIDForDevice`. A cursor is replayable only when its UUID matches the current volume, its event ID is neither zero nor the since-now sentinel, and its generation is the currently published SQLite generation. Launch, wake, and restart all recreate the stream from this committed pair.

Recovery planning has three outcomes:

- Replay resumes a trustworthy stream cursor.
- Repair Subtrees reduces known event paths to minimal safe parents, removes their stale committed entries, observes only those subtrees, and merges them into a complete replacement generation.
- Rebuild Volume observes the configured root when the cursor is missing or invalid, the UUID changed, event IDs wrapped, or lost history has no trustworthy narrower scope.

MustScanSubDirs and dropped-history flags retain known paths when available. Wrapped IDs always rebuild. Root changes rebuild when the safe scope is the configured root. FSEvents flags never become entry mutations; observed filesystem state remains authoritative.

Replay and subtree repair report Catching Up. Full recovery reports Rebuilding. Search continues using the preceding memory-mapped projection until entries, Coverage, volume UUID, event ID, and the replacement generation commit together. An interrupted repair therefore leaves both the published generation and cursor unchanged and can be repeated safely.

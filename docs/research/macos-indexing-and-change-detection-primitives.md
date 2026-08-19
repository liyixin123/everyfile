# macOS indexing and change-detection primitives

## Question

Which supported macOS APIs and storage primitives should Everyfile use to build the initial global File Index, observe file creation, rename, move, and deletion, catch up after sleep or app downtime, and perform a low-cost refresh when a Quick Search Window opens?

## Recommendation

Use a **snapshot plus journal-reconciliation design**:

1. Discover mounted volumes with Foundation's `FileManager.mountedVolumeURLs` and classify them with volume resource keys. Use Disk Arbitration notifications when mount and unmount events matter.
2. Build the initial File Index by recursively enumerating each enabled local volume with `FileManager.DirectoryEnumerator`. Read only the resource values needed by the index. Do not follow directory symlinks; decide separately whether package descendants belong in the File Index.
3. Maintain one persistent FSEvents cursor per enabled volume. Store the FSEvents database UUID with the cursor, because a replaced or purged event database changes that UUID. Use a **per-disk FSEvent stream** and directory-level events as the default. Reconcile each reported directory against the stored snapshot. This fits a global index better than file-level events, which Apple warns produce significantly more events.
4. Start monitoring before the initial enumeration, then reconcile every subtree reported changed during the scan before declaring the snapshot current. Persist a cursor only after the corresponding reconciliation transaction commits. On macOS 10.15+, evaluate `FullHistory` to overlap the replay boundary after an unclean restart, deduplicating the overlap.
5. On launch, wake, or Quick Search Window opening, replay FSEvents from the committed per-volume cursor. Search the existing File Index immediately; catch-up must run independently and publish updates when ready.
6. If FSEvents reports dropped/coalesced history, wrapped IDs, an invalid/missing cursor, or an untracked volume, recursively rescan the smallest safe affected subtree. Fall back to a whole-volume rebuild when the affected scope is unknown.
7. Treat coverage as explicit state. Full Disk Access can only be granted by the user in System Settings and still does not override all POSIX, ACL, or system restrictions. Show inaccessible roots and skipped paths rather than presenting the File Index as complete.

This design avoids continuous polling. FSEvents is specifically designed for passive monitoring of large trees and persists changes across reboots, while its configurable latency lets Everyfile trade a few seconds of staleness for fewer callbacks and lower resource use.

## Why these primitives

### Initial enumeration: Foundation `FileManager`

`FileManager.enumerator(at:includingPropertiesForKeys:options:errorHandler:)` performs deep directory enumeration. `DirectoryEnumerator` recursively includes descendants, crosses device boundaries, and does not traverse directory symbolic links. Everyfile should therefore enumerate **per enabled volume** and prevent accidental crossing into other mounts during a volume scan. Its error handler can record coverage gaps caused by access failures.

Foundation supports prefetching selected URL resource keys during enumeration. The first File Index needs only fields required for search and agreed sorting, such as path/name, item type, size, creation date, and modification date. Omitting `.skipsHiddenFiles` includes hidden items, matching the product decision that hidden items appear by default. `.skipsPackageDescendants` is an independent product choice: packages are directories presented to users as files, so descending into them increases coverage and index size but can expose implementation detail. If profiling later shows enumeration syscall overhead is a bottleneck, Darwin's `getattrlistbulk(2)` (macOS 10.10+) is a candidate implementation optimization behind the same abstraction, not an MVP architecture dependency.

`FileManager.mountedVolumeURLs` supplies mounted volume roots. URL volume resource keys distinguish internal, local, removable, read-only, and root volumes. Disk Arbitration supplies appearance, disappearance, mount, unmount, and volume-name notifications; it is useful for reacting to external media without polling. Apple notes that sandboxed utilities have limited practical access to newly inserted media unless they hold an appropriate security-scoped bookmark.

### Incremental change detection: FSEvents

Apple describes FSEvents as a lightweight API for monitoring large directory hierarchies. Its daemon coalesces events and stores persistent change history. A stream accepts a `sinceWhen` event ID, so Everyfile can replay changes that happened while it was asleep or not running. Apple recommends per-disk streams for software that persists cursors because per-host historical IDs can conflict when disks previously used on other Macs are attached.

The default stream reports directory-level changes. That is sufficient when combined with a stored directory snapshot: enumerate the reported directory, compare it with the indexed children, and apply creates, deletes, renames/moves, and metadata changes. `kFSEventStreamCreateFlagFileEvents` can request per-item notifications, but Apple explicitly warns that it generates significantly more events. It is therefore a later optimization to benchmark, not the default low-resource architecture. FSEvents is advisory change detection, not a transactional stream of exact file operations; rename pairing and correctness must come from reconciliation with current filesystem state.

The stream latency is a direct resource/staleness control: Apple says larger latency enables more temporal coalescing, fewer callbacks, and greater efficiency. Everyfile can expose a small set of modes backed by latency/batching settings. Opening the Quick Search Window should enqueue or accelerate pending reconciliation, but must not trigger a recursive disk scan for each query or keystroke.

### Correctness and event loss

FSEvents flags define the recovery boundary:

- `MustScanSubDirs` requires recursive rescanning below the reported path.
- `UserDropped` or `KernelDropped` explains why events were coalesced/lost and accompanies the need to rescan.
- `EventIdsWrapped` invalidates assumptions about cursor ordering and requires rebuilding from a trustworthy snapshot boundary.
- `RootChanged`, used with `WatchRoot`, reports when a watched root or its ancestors move or disappear.
- `HistoryDone` marks the transition from replayed history to live events.
- mount/unmount flags and Disk Arbitration signals require volume lifecycle handling.

FSEvents history is not guaranteed to be available forever. Volumes can disable event logging, and events are filtered by filesystem permissions. The File Index must therefore store a persistent volume UUID where the filesystem supplies one, the FSEvents database UUID, cursor, last successful reconciliation time, and coverage state. Foundation documents resource/file identifiers as not necessarily persistent across restarts, so inode-like identifiers can optimize reconciliation but must not be the sole durable identity. Any ambiguous journal state must degrade to bounded enumeration rather than silently accepting stale entries.

For an initial snapshot without a race window, begin receiving events, enumerate, then consume/reconcile all events covering the scan before marking the File Index current. The persisted cursor and index mutation should be committed together (or made recoverable through an application journal); otherwise a crash can advance the cursor past unapplied changes.

## Permissions and distribution constraints

Global search and App Sandbox are in tension. A sandboxed app normally gains recursive access to a folder selected through `NSOpenPanel`, and can persist that access with a security-scoped bookmark. This works well for explicitly enabled roots and removable volumes, but it is not equivalent to automatic global coverage.

Apple states that an app cannot grant itself Full Disk Access through an entitlement or code; the user must grant it in System Settings. Even with sandbox permission, POSIX permissions, ACLs, System Integrity Protection, and data protection may still deny access. FSEvents itself withholds events when the current user cannot traverse the changed directory; only root is guaranteed to receive all events. Therefore:

- do not require Full Disk Access on first launch;
- explain that granting it improves coverage;
- report inaccessible roots and coverage status;
- never promise literally every file on the machine;
- decide distribution/sandbox posture before implementation, because it materially changes the attainable meaning of “global.”

## Minimum macOS implications

The core primitives do not force a recent minimum OS: FSEvents dates to macOS 10.5, file-level events to 10.7, `getattrlistbulk` to 10.10, and `FullHistory` to 10.15. The product has already chosen Apple Silicon (macOS 11 or later) and a recent macOS baseline, so the minimum version should be chosen from the UI/runtime/toolchain needs, not indexing availability. Before implementation, confirm the exact deployment target against the current SDK headers for every selected flag and API.

## Rejected as the primary mechanism

- **Periodic full scans:** simple but violates the low-background-resource requirement and still has stale intervals. Retain only as explicit repair/rebuild.
- **One watcher per directory with kqueue:** Apple positions kqueues for watching particular files, not large persistent trees; they do not supply FSEvents-style offline history and scale poorly for a global File Index.
- **File-level FSEvents by default:** more direct events but substantially greater event volume; test only if directory reconciliation proves too expensive.
- **Reading `.fseventsd` directly:** Apple explicitly says not to rely on its private on-disk format.
- **Spotlight metadata queries as the source of truth:** they would delegate coverage and update semantics to Spotlight and do not provide the explicit index ownership required by Everyfile's design. They may be useful only as an optional bootstrap experiment, not the correctness layer.

## Decisions still required

1. Whether the first release is sandboxed and how it is distributed; this defines attainable global coverage.
2. Whether application package descendants are indexed by default.
3. The exact enabled-volume policy and stable volume identity scheme for APFS volumes, removable media, network mounts, and cloud-provider roots.
4. The reconciliation granularity and batching/latency modes, validated with measurements on large trees and event bursts.
5. Whether file-level FSEvents improves rename/move handling enough to justify its event cost after profiling.

## Primary sources

- Apple, [File System Events Programming Guide: Introduction](https://developer.apple.com/library/archive/documentation/Darwin/Conceptual/FSEvents_ProgGuide/)
- Apple, [File System Events Programming Guide: Technology Overview](https://developer.apple.com/library/archive/documentation/Darwin/Conceptual/FSEvents_ProgGuide/TechnologyOverview/TechnologyOverview.html)
- Apple, [Using the File System Events API](https://developer.apple.com/library/archive/documentation/Darwin/Conceptual/FSEvents_ProgGuide/UsingtheFSEventsFramework/UsingtheFSEventsFramework.html)
- Apple, [File System Event Security](https://developer.apple.com/library/archive/documentation/Darwin/Conceptual/FSEvents_ProgGuide/FileSystemEventSecurity/FileSystemEventSecurity.html)
- Apple, [`FSEventStreamCreate`](https://developer.apple.com/documentation/coreservices/1443980-fseventstreamcreate)
- Apple, [`kFSEventStreamCreateFlagFileEvents`](https://developer.apple.com/documentation/coreservices/kfseventstreamcreateflagfileevents)
- Apple, [`kFSEventStreamCreateFlagFullHistory`](https://developer.apple.com/documentation/coreservices/kfseventstreamcreateflagfullhistory)
- Apple, [`kFSEventStreamEventFlagMustScanSubDirs`](https://developer.apple.com/documentation/coreservices/1455361-fseventstreameventflags/kfseventstreameventflagmustscansubdirs/)
- Apple, [`kFSEventStreamEventFlagHistoryDone`](https://developer.apple.com/documentation/coreservices/kfseventstreameventflaghistorydone)
- Apple, [`kFSEventStreamEventFlagRootChanged`](https://developer.apple.com/documentation/coreservices/kfseventstreameventflagrootchanged)
- Apple, [`FileManager`](https://developer.apple.com/documentation/foundation/filemanager)
- Apple, [`FileManager.DirectoryEnumerator`](https://developer.apple.com/documentation/foundation/filemanager/directoryenumerator)
- Apple, [`FileManager.DirectoryEnumerationOptions`](https://developer.apple.com/documentation/foundation/filemanager/directoryenumerationoptions)
- Apple, [`URLResourceKey`](https://developer.apple.com/documentation/foundation/urlresourcekey)
- Apple, [File System Programming Guide: Working with Files and Directories](https://developer.apple.com/library/archive/documentation/FileManagement/Conceptual/FileSystemProgrammingGuide/AccessingFilesandDirectories/AccessingFilesandDirectories.html)
- Apple, [About Disk Arbitration](https://developer.apple.com/library/archive/documentation/DriversKernelHardware/Conceptual/DiskArbitrationProgGuide/Introduction/Introduction.html)
- Apple, [Accessing files from the macOS App Sandbox](https://developer.apple.com/documentation/security/accessing-files-from-the-macos-app-sandbox)

# macOS permissions and volume coverage

Research for [Determine permissions and volume coverage constraints](https://github.com/liyixin123/everyfile/issues/3).

## Decision summary

Everyfile should ship its first self-use/open-source build as a directly distributed, notarized macOS application **without App Sandbox**. App Sandbox is mandatory for Mac App Store distribution and normally confines file access to the app container plus explicitly entitled or user-selected locations; that does not fit an application whose primary job is to build a global File Index. This choice does not bypass macOS privacy controls or ordinary file permissions.

Full Disk Access (FDA) should be **optional but recommended for maximum internal-disk coverage**. Everyfile must work before FDA is granted, index every path that the current process can actually enumerate, and clearly label the File Index as incomplete when protected or otherwise unreadable subtrees are observed. Apple states that an app cannot obtain FDA through an entitlement or code; the user must grant it in System Settings. Apple also documents that POSIX permissions, ACLs, System Integrity Protection, and data protection can still deny a file independently of App Sandbox or FDA.[^sandbox-access]

Coverage must be represented as observed facts per configured root/volume, not as one guessed `hasFullDiskAccess` Boolean. Apple documents no supported API that reports FDA as a single application status. A probe of one protected location would only be a heuristic and would not prove that every other location is accessible.

## Recommended coverage policy

| Storage class | MVP default | Eligibility and behavior |
| --- | --- | --- |
| Mounted internal volumes | On | Discover mounted volumes and include volumes for which Foundation reports `volumeIsInternal == true`. Traverse only with the current user's effective access. Keep separate coverage and event checkpoints per volume. |
| External/removable local volumes | Off, opt in per volume | Identify with `volumeIsLocal`, `volumeIsInternal`, `volumeIsRemovable`, and stable volume identifiers where available. Treat mount, unmount, identity change, and missing persistent IDs as coverage-state changes. |
| Network volumes | Off | Identify non-local mounted volumes with volume resource values. Only index after explicit opt in in a later scope; privacy consent applies to network volumes, connectivity is intermittent, and local persistent-event guarantees must not be assumed.[^file-privacy][^url-resource] |
| Cloud-provider trees | Only locally enumerable names/paths | Do not download content to build a name/path File Index. Include metadata placeholders that are already visible during normal directory enumeration, but do not claim complete provider coverage. A dataless folder may not have had its children enumerated, so unseen remote descendants cannot be inferred.[^file-provider] |
| Unmounted volumes and remote-only cloud descendants | Out of coverage | Retain their last checkpoint only as stale state if useful; do not present stale entries as current searchable coverage. |

The product phrase “global search” should therefore mean: **all currently mounted internal volumes and all paths on them that the running user and macOS privacy controls permit Everyfile to enumerate**, plus explicitly enabled external volumes. It must not mean every byte on the machine.

## Permission model

### Without Full Disk Access

Everyfile can traverse ordinary locations allowed by the current user's POSIX/ACL permissions and by macOS privacy consent. Since macOS 10.15, Apple requires user consent for protected locations including Desktop, Documents, Downloads, iCloud Drive, and network volumes; removable volumes are also part of Files & Folders privacy control.[^file-privacy]

The first scan should not force an FDA request. It should continue around denied directories and record each denied subtree. Do not repeatedly retry denied roots in a tight loop.

### With Full Disk Access

FDA expands access for workflows that need files throughout storage, but the user alone grants it in System Settings. Everyfile should offer a settings action that opens or explains the correct Privacy & Security pane and then requests a rescan/reconciliation after the user returns.[^sandbox-access]

FDA is not equivalent to root. Files can remain inaccessible because of POSIX mode bits, ACLs, System Integrity Protection, data protection, an unavailable volume, or provider behavior. FSEvents similarly filters events by whether the user can reach the changed directory; Apple's archived guide says only a process running as root can be guaranteed all events.[^fsevents-security]

### App Sandbox

A sandboxed application starts with unrestricted access to its own container, not the user's whole home directory. It can extend access to a folder selected in an open panel and persist that access with a security-scoped bookmark. Selecting a folder recursively extends the sandbox to descendants, subject to other access controls.[^sandbox-access] That is a viable future “selected roots only” product mode, but not the agreed default of global internal-volume indexing.

Mac App Store distribution requires App Sandbox.[^app-sandbox] Therefore the MVP's global coverage implies direct distribution. A future App Store variant would need a different coverage contract (selected roots and security-scoped bookmarks) and should be treated as a separate product/distribution decision.

## Detect and explain incomplete coverage

Maintain a coverage record for every configured root:

- volume identity and classification (`internal`, `local`, `removable`, filesystem type, mounted state);
- last full traversal start/completion time and last FSEvents checkpoint;
- `complete`, `partial`, `stale`, `offline`, or `never scanned` state;
- count of unreadable subtrees and representative paths/errors;
- event-stream health, including dropped events, event-ID wrap, root replacement, mount/unmount, and history gaps;
- cloud caveat when remote descendants may not be locally enumerated.

During traversal, classify `EACCES` and `EPERM` as inaccessible and continue with sibling paths. Do not tell the user that every such error is caused by missing FDA; Apple's diagnostics show several independent causes.[^sandbox-access] Suggested UI copy:

> File Index coverage is partial. Everyfile could not read 14 folders. Full Disk Access may increase coverage, but file ownership and system protections can still limit access. Review skipped locations.

Everyfile can offer an **FDA likely missing** hint after a documented protected-location probe fails, but it must label this as guidance, not authoritative permission detection. The authoritative product signal is actual scan/event coverage.

## Change tracking and resource use

Use FSEvents to avoid continuous disk scans. Apple describes it as a passive mechanism for large directory trees, with a persistent event database that can expose changes made while the application was not running. The stream latency is configurable and events may be coalesced.[^fsevents-overview]

For persistent correctness, store a separate event checkpoint for each physical/local volume. Apple recommends per-disk streams for persistent software because event IDs are scoped to a disk and may conflict when disks move between hosts. Apple also says the event list is advisory and recommends periodic full sweeps because a disk can be changed where compatible history is unavailable.[^fsevents-usage]

This supports the agreed low-resource behavior:

1. Build the initial File Index once per enabled volume.
2. Consume/coalesce FSEvents at an energy-conscious latency while active.
3. When the Quick Search Window opens, return results from the existing File Index immediately and concurrently drain/reconcile the saved event backlog.
4. Rescan only affected directories when the events are trustworthy.
5. Schedule a bounded full verification infrequently or when health flags require it; never continuously walk every disk.

Trigger a scoped or full reconciliation when FSEvents reports `MustScanSubDirs`, user/kernel dropped events, event IDs wrapped, a watched root changed, or a mount/unmount event. These conditions are exposed by `FSEventStreamEventFlags`.[^fsevents-flags]

Some volumes can disable persistent FSEvents logging with `.fseventsd/no_log`; Apple also warns not to read the on-disk database directly.[^fsevents-security] Treat missing history as a stale coverage checkpoint that requires traversal, not as proof that nothing changed.

## Cloud-file boundary

Apple's File Provider model distinguishes dataless and materialized items. A dataless document still has metadata such as name and size, while a dataless folder means the system knows the folder exists but has not enumerated its contents. A materialized folder has been enumerated and can expose its children.[^file-provider]

Consequences for Everyfile:

- Index a cloud item's name/path when normal filesystem enumeration exposes it, even if its content is not downloaded; content is outside the MVP.
- Never deliberately materialize/download files merely to index names.
- Do not promise complete cloud-provider coverage: remote descendants under an unenumerated dataless folder may be unknown.
- Report such roots as “local view indexed” rather than “complete.”

## Remaining validation work

Before implementation is considered complete, test the coverage model on the minimum supported macOS version and current macOS using: FDA off/on, a protected home folder, another user's home, an encrypted removable APFS volume, a non-APFS removable volume, an SMB share, iCloud Drive with optimized storage, and at least one third-party File Provider. These are behavioral validation cases, not reasons to delay the architecture decision above.

[^app-sandbox]: Apple Developer Documentation, [App Sandbox](https://developer.apple.com/documentation/security/app-sandbox).
[^sandbox-access]: Apple Developer Documentation, [Accessing files from the macOS App Sandbox](https://developer.apple.com/documentation/security/accessing-files-from-the-macos-app-sandbox).
[^file-privacy]: Apple Platform Security, [Controlling app access to files in macOS](https://support.apple.com/guide/security/controlling-app-access-to-files-secddd1d86a6/web).
[^url-resource]: Apple Developer Documentation, [URLResourceKey](https://developer.apple.com/documentation/foundation/urlresourcekey).
[^file-provider]: Apple Developer Documentation, [Synchronizing the File Provider Extension](https://developer.apple.com/documentation/fileprovider/synchronizing-the-file-provider-extension).
[^fsevents-overview]: Apple Developer Documentation Archive, [File System Events Programming Guide: Technology Overview](https://developer.apple.com/library/archive/documentation/Darwin/Conceptual/FSEvents_ProgGuide/TechnologyOverview/TechnologyOverview.html).
[^fsevents-usage]: Apple Developer Documentation Archive, [Using the File System Events API](https://developer.apple.com/library/archive/documentation/Darwin/Conceptual/FSEvents_ProgGuide/UsingtheFSEventsFramework/UsingtheFSEventsFramework.html).
[^fsevents-security]: Apple Developer Documentation Archive, [File System Event Security](https://developer.apple.com/library/archive/documentation/Darwin/Conceptual/FSEvents_ProgGuide/FileSystemEventSecurity/FileSystemEventSecurity.html).
[^fsevents-flags]: Apple Developer Documentation, [FSEventStreamEventFlags](https://developer.apple.com/documentation/coreservices/fseventstreameventflags).

# Search result actions, filters, and sort state

Issue: #39

The Quick Search Window treats a search result as an indexed filesystem entry, not merely display text. Each result carries its `Indexed Entry Type` from the File Index into the UI. The Result Filter has three user-facing values: All, Files, and Folders. All includes File, Directory, Symlink, and Other; Folders includes Directory; Files includes File, Symlink, and Other. The filter is applied before the result limit so a filtered query can still fill the visible page. It is persisted with the other user-facing search preferences and applies even when the Search Query is empty.

The result list renders one cached, generic system icon per entry type. Icon acquisition is kept outside the query worker and cached by type, so scrolling does not perform a filesystem metadata or Finder lookup for every row. The icon is the only type affordance in the row; no redundant type label is shown.

Selected results expose the same Result Action boundary to keyboard and pointer interactions. Double-click invokes the default Open action. A contextual menu offers Open, Open With, Reveal in Finder, Copy Item, and Copy Path. Open With uses the native macOS application chooser without changing the system default application. Copy Item writes a file URL so Finder can paste the indexed item; Copy Path retains the existing text-copy behavior. Failed actions report a transient non-blocking message in the search window and leave the result list intact.

Sorting has one canonical `SortOrder` state. Clicking the active table column toggles its direction; clicking a different column selects ascending order. The active table header shows an ascending or descending indicator, while the toolbar field selector and direction control are synchronized from the same state. Relevance keeps its existing meaning: ascending places the best matches first.

## Consequences

- Search results need to preserve entry type through projection, query ranking, and UI publication.
- Filter and action behavior can be tested without AppKit by keeping the domain boundary independent from native dispatch.
- The UI must update sort indicators and the toolbar together rather than maintaining separate control state.

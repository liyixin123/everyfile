# Query Sessions and Sort Order

Every Search Query runs as a generation-scoped Query Session against one committed, memory-mapped search projection. Starting a new query, changing Sort Order, or requesting a larger frontier cancels the preceding session. A completed publication is applied only when its generation is still current, so obsolete work cannot replace newer results.

Query work runs on the bounded background scheduler. Projection records stream into a bounded heap that retains at most the requested K candidates; the Query Engine never builds and fully sorts the complete hit set. The first frontier is 100 rows. Selecting near the end requests the next 100, while AppKit continues to create only visible table row views. A completed scan also supplies the exact total.

The persisted Sort Order consists of a field and direction. Fields are Relevance, modification time, creation time, file name, full path, and file size. Clicking a metadata column selects it; clicking it again toggles direction. Relevance is available from the Menu Bar Control. File name and path comparisons use the same locale-independent normalized forms as matching.

Non-Relevance sorts compare the selected field, then locked Relevance, normalized file name, and canonical normalized path. Missing creation or modification times remain last in both directions. SQLite remains the durable File Index; creation time was added only to the rebuildable projection and public Search Result contract.

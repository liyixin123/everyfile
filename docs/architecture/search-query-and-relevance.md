# Search Query, Relevance, and result actions

## Normalized matching

Everyfile canonical-decomposes and Unicode-case-folds both the corpus and Search Query. Combining marks attached to Latin bases are removed, making composed/decomposed and Latin-diacritic spellings equivalent while preserving non-Latin distinctions. The rebuildable projection stores normalized name and path strings beside the display strings so each query does not normalize the full corpus.

Unicode whitespace separates terms. Empty terms disappear and every remaining term must occur in either the normalized file name or normalized full path. Query punctuation remains literal; there is no advanced language, spelling correction, stemming, synonym expansion, or content search.

## Relevance

Candidates sort by the locked sequence:

1. exact file name;
2. file-name prefix;
3. file-name segment prefix;
4. other file-name substring;
5. path-only match;
6. more terms occurring in the name;
7. shorter normalized name;
8. shorter normalized path;
9. more recent successful Everyfile Open;
10. canonical normalized full path.

Recent-open history therefore cannot move a result across a match class or the stronger structural tie-breakers. SQLite persists only successful Open actions. Reveal and Copy Path do not affect history, and the menu exposes Clear Open History.

## Application interaction

Editing the search field immediately queries the committed memory-mapped projection and reloads at most 100 table rows. Return opens the selected row (or the first row), Command-Return reveals it in Finder, and Command-C copies its full path. Native macOS services are behind the result-action boundary; contract tests use a recording dispatcher.

Selectable sorting, top-K optimization, progressive publication, exact totals, and cancellation remain #13.

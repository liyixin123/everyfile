# Everyfile

Everyfile is a macOS application for finding local files quickly by name and path.

## Language

**Quick Search Window**:
The primary search interface, shown by a global keyboard shortcut and hidden when it is not in use.
_Avoid_: Menu Bar App, Spotlight Window

**Menu Bar Control**:
The secondary interface for index status, settings, and application lifecycle controls.
_Avoid_: Main Window

**File Index**:
The local catalog of searchable file names and paths.
_Avoid_: Search Database

**Freshness**:
How far the File Index has caught up with known filesystem changes. Its user-visible states are Current, Catching Up, Rebuilding, and Offline.
_Avoid_: Coverage, Accuracy

**Coverage**:
Which configured files and directories Everyfile can observe and include in the File Index. Coverage can be Complete or Partial independently of Freshness.
_Avoid_: Freshness, Full Disk Access Status

**Search Query**:
The text entered in the Quick Search Window, interpreted as space-separated terms that must each match a file name or path.
_Avoid_: Command, Advanced Query

**Relevance**:
The default result order based primarily on match quality in the file name and secondarily on path match quality and Everyfile open history.
_Avoid_: Sort Score, Spotlight Rank

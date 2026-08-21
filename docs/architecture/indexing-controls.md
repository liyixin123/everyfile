# Indexing controls and resource policy

Everyfile keeps committed search data readable while indexing is paused, throttled,
catching up, or rebuilding. FSEvents hints continue to accumulate while work is
paused; a checkpoint advances only in the same SQLite transaction that publishes
the corresponding replacement generation.

The Menu Bar Control exposes Settings, Pause Indexing, Resume Indexing, Process
Pending Changes, Rebuild Configured Volume, state and Coverage, skipped locations,
external volumes, and Quit. Opening Quick Search requests an accelerated drain of
the already-bounded pending hint batch. Queries only read the memory-mapped Search
Projection and never enumerate the filesystem.

Large initial scans inspect system conditions between entries. User pause,
unavailable volumes, severe thermal state, and memory pressure suspend work. Low
Power Mode and low battery select reduced work. Reconciliation applies the same
gate before taking a pending batch and resumes from the committed checkpoint when
conditions recover. `Current` performs no timer-driven traversal; only FSEvents or
an explicit rebuild schedules filesystem reconciliation.

# External Volume Lifecycle

Mounted volumes are classified as internal local, external local, or network. Internal local volumes are enabled by policy. External local volumes are persisted as disabled until the user explicitly enables them from External Volumes. Network volumes remain disabled and cannot be enabled through this surface.

Volume configuration uses the persistent FSEvents UUID as identity; mount paths are mutable observations. Reusing a mount path with a different UUID never inherits opt-in or a checkpoint. Opt-in survives application restart and volume absence.

Each enabled volume retains its latest committed generation, Coverage, and checkpoint. Search Projection construction combines the latest enabled generations. Removing a volume changes its lifecycle Freshness to Offline, stops uncommitted reconciliation, and keeps its committed entries searchable. Other volumes continue through the same immutable combined projection. Reconnection with the same UUID enters Catching Up and reconciles from the checkpoint; identity mismatch is treated as another, disabled volume.

Mount/unmount notifications refresh observed state. Enabling or reconnecting an external volume schedules its scan off the main thread; disabling it rebuilds the combined projection without that volume but does not destructively delete durable index data.

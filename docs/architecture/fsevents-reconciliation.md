# FSEvents Reconciliation

FSEvents is a persistent hint source, not the File Index mutation log. Everyfile requests file-level events for the configured root and resumes from the event ID stored for that volume/root when the stream identity still matches. Stream callbacks perform no filesystem or SQLite work; they enqueue path batches for the bounded background scheduler.

The default Balanced stream latency is five seconds. Responsive and Low Energy use one and fifteen seconds. These are coalescing windows: without a pending FSEvents hint, Everyfile performs no reconciliation scan. Descendant paths collapse beneath an already pending ancestor. History-loss and root-change flags are preserved for the recovery policy in #15.

On a batch, Freshness becomes Catching Up while Search continues using the last committed projection. Reconciliation observes current filesystem state rather than interpreting event flags as create, rename, move, or delete commands. A replacement entry generation, observed Coverage, stream identity, and highest applied event ID commit in one SQLite transaction. Failure retains the preceding generation and checkpoint. After commit, Everyfile rebuilds the memory-mapped projection, atomically publishes it to Query Sessions, refreshes the active Search Query, and reports Current.

The current slice reconciles the configured root as the safe observation scope. Smaller subtree recovery and history-loss escalation are owned by #15; multi-volume stream lifecycle is owned by #17.

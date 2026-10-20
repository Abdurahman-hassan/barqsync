# Part 3A: protocol coordination outside the worker pool

Date: 20 October 2026.

Status: implemented; see [checkpoint verification](verification.md).

Finding: NET-02.

Both remote roles previously ran their blocking receive loop inside
`pool.install(|| rayon::scope(...))`. With one worker that loop consumed the only
thread needed to scan directories, answer requests or write queued packs.

The sender and receiver now use `ThreadPool::in_place_scope`. Coordination stays
on the calling thread, while spawned filesystem work runs on the configured
pool. The user's worker count is preserved.

`tests/remote_lifecycle.rs` exercises push and pull with one and two workers,
threaded and Tokio I/O, postcard and rkyv, small-file packs, a large whole file,
a no-op and a subsequent delta. A separate case checks one-worker hard links,
symlinks and source removal after deferred xattr updates. Each CLI invocation has a 20-second deadline;
the test harness kills and reaps its owned process group on timeout.

This part does not establish bounded cancellation, slow-peer behavior, metadata
chunking or stream admission. Those remain parts 3B/3C.

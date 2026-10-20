# barqsync: audit findings and implementation plan

Reviewed **20 October 2026** against **v0.3.0**, commit
`f9f8d8d87855c3ac8d24c8d852ca6c37a80ecc69`.
The existing checkout matched the public repository's HEAD, verified with
`git ls-remote`. At the time of this review, this document proposed fixes and production code
had not been changed. Current implementation status and verification are tracked
in [docs/implementation](docs/implementation/README.md); the checkout has since
been reconciled with v0.4.0. The findings and evidence below retain their original
v0.3.0 context.

The first release should address the confirmed security and data-loss bugs below.
The macOS streaming fix should provide parallel data channels through SSH so a
working SSH connection is sufficient; disabling the firewall must never be required.

## Evidence and limits

- Examined local synchronization, remote sender/receiver, TCP/TLS and QUIC setup,
  protocol framing, scanning, delta application, CLI parsing, and CI/release configuration.
- Ran the existing suite on macOS 26.6.2 arm64 with Rust 1.92.0 and rsync 3.5.1:
  **184 tests reported passing**. The three `real_remote` tests returned early because
  `BARQSYNC_TEST_REMOTE` was unset; Linux-specific paths were not validated on this host.
- `cargo clippy --locked --offline --all-targets -- -D warnings` passed.
- An initial sandboxed run failed eight socket tests because socket creation was
  prohibited. Rerunning with local networking permitted passed. Those initial failures
  are environment restrictions, not product bugs.
- Additional isolated probes reproduced failures outside the existing test coverage.
  Their source and observed output are in
  [docs/review/2026-10-20](docs/review/2026-10-20/).
- Checked all **203 registry package versions** in `Cargo.lock` against OSV.
  The only returned advisory was the informational, unmaintained
  `atomic-polyfill` advisory discussed under MAINT-01.
- The local firewall was **already disabled** when inspected. No firewall settings
  were changed, and no remote Mac was accessed. The firewall diagnosis combines
  the user's report, the existing `ISSUES.md`, source inspection, and Apple's
  documented firewall behavior. A two-machine, firewall-enabled acceptance test
  remains necessary.
- The pre-existing untracked `ISSUES.md` was preserved. Its historical observations
  are not treated as fresh measurements.

“Reproduced” below means a local CLI or public-API probe demonstrated the behavior.
“Code-confirmed” means the relevant path is identifiable in the source, but the
complete failure scenario has not been reproduced. This is a targeted audit, not a
claim that every possible defect or vulnerability has been found.

## Prioritized findings

P1 means fix before the next normal release; P2 means scheduled reliability,
performance, or compatibility work; P3 means maintenance or usability improvement.

| ID | Priority | Finding | Evidence |
| --- | --- | --- | --- |
| SEC-01 | P1 | Remote filenames can escape the destination directory | Reproduced |
| SEC-02 | P1 | A remote receiver can delete sender files without permission, even during dry run | Reproduced |
| DATA-01 | P1 | Same-file copies can truncate or delete their own source | Reproduced |
| DATA-02 | P1 | Multiple source roots with `--delete` remove each other's output | Reproduced |
| DATA-03 | P1 | Scan errors do not suppress destructive deletion | Reproduced |
| DATA-04 | P1 | Backup failure does not stop replacement of the original | Reproduced |
| DATA-05 | P1 | Missing or malformed file data can be committed successfully | Reproduced |
| DATA-06 | P1 | Source-removal guard misses same-second changes and identity changes | Same-second case reproduced; identity gap code-confirmed |
| NET-01 | P1 | Parallel streams require an additional inbound service that macOS can block | Code-confirmed; user-reported firewall behavior |
| NET-02 | P1 | Remote synchronization deadlocks with `--threads=1` | Reproduced |
| NET-03 | P1 | Cancellation, control disconnects, and writer shutdown can wait indefinitely | Code-confirmed risk; blocked-peer reproduction pending |
| PROTO-01 | P1 | Oversized metadata messages can be silently dropped and leave peers waiting | Code-confirmed |
| NET-04 | P2 | Stream admission lacks bounded, independently timed authentication | Code-confirmed |
| PERF-01 | P2 | Queue limits count frames, allowing memory to grow heavily with stream count | Code-confirmed; peak RSS benchmark pending |
| PERF-02 | P2 | A single large file uses one data stream; large-file receiving is serialized | Code-confirmed, documented limitation |
| PLAT-01 | P2 | CDC mmap can crash if a mapped file shrinks during use | Code-confirmed, acknowledged in source |
| CLI-01 | P2 | Remote `~` paths and bracketed IPv6 addresses are not handled correctly | Code-confirmed; `~` also recorded in existing issue notes |
| MAINT-01 | P3 | Dependency advisory, release signing, and CI coverage need follow-up | Dependency lookup and configuration inspection |

## Security and data correctness

### SEC-01 — Confine remote entries to the requested destination

**Evidence:** `src/remote/receiver.rs:355–415` accepts peer-supplied listings.
`src/local/exec.rs:825–828` joins each entry's name directly onto the source and
destination paths. No entry-name validation rejects `..`, embedded separators,
or absolute paths first.

The protocol probe sent a normal root listing containing the filename
`../escaped`, followed by four bytes of data. The receiver wrote `escaped`
**outside** its destination directory and returned **0**.

**Impact:** A malicious or compromised remote sender can write outside the
destination selected by a pull operation, with the receiving user's permissions.
SSH authentication identifies the peer; it does not make all filesystem
instructions from that peer safe. This is not a demonstrated unauthenticated
network exploit.

**Implementation:**

- Validate every wire entry before planning or modifying files: a child name must
  be one nonempty filename component, not `.` or `..`, with no `/` or NUL.
  Preserve valid non-UTF-8 Unix filenames.
- Validate root IDs, directory IDs, relative paths, duplicate entries, and listing
  order. Require each non-root directory to correspond to an accepted parent entry.
- Use directory descriptors and relative filesystem operations for writes,
  replacement, metadata updates, and deletion. On macOS, walk components with
  `openat` and appropriate no-follow checks; on Linux, use equivalent descriptor
  confinement, optionally `openat2` where available.
- Do not rely on `canonicalize()` followed by a path-based write: that leaves a
  symlink-swap race. Explicit `-K` directory-link behavior needs a documented,
  narrowly scoped exception; a peer must not enable it itself.

**Acceptance:** Traversal, absolute names, malformed relative paths, and
symlink-swap tests cannot modify a sentinel outside the authorized tree. Run
against both wire encodings and each transport.

### SEC-02 — Authenticate and authorize source-deletion acknowledgements

**Evidence:** `src/remote/sender.rs:639` accepts `Msg::Synced` unconditionally.
`remove_synced()` at lines 201–212 joins a peer-supplied path and calls
`remove_source()`. Neither this function nor `src/local/flags.rs:58–66` checks
`remove_source_files` or `dry_run`.

The probe sent `Synced` for a temporary file outside the source root:

```text
remove_source_files=false, dry_run=false: outside file deleted; exit 0
remove_source_files=false, dry_run=true:  outside file deleted; exit 0
```

**Impact:** A malicious receiver can instruct a push sender to unlink files the
user never authorized it to remove. The probe supplied the victim's correct size
and seconds-resolution mtime. Non-regular files also bypass that size/time
comparison in the current removal helper.

**Implementation:**

- Reject `Synced` when source removal is disabled or the operation is a dry run.
  Make the removal helper enforce these conditions too.
- Replace path-based acknowledgements with transfer IDs referring to the sender's
  own ledger of eligible, successfully transferred entries.
- Store trusted path, root, type, identity, size, and timestamps in that ledger.
  Do not accept these facts from the peer as deletion authority.
- Reject unknown, duplicate, premature, and out-of-scope acknowledgements.
  Use the confinement work from SEC-01 for source-side operations as well.
- Never send a removal acknowledgement before file data and requested metadata
  have been successfully committed.

**Acceptance:** The two probes above preserve their sentinels and fail cleanly.
Add positive tests proving authorized removal still works for files, symlinks,
hard links, and deferred xattrs.

### DATA-01 — Detect source/destination identity before destructive operations

**Evidence:** `src/fsops/mod.rs:457–461` opens the source and then opens the
destination with `truncate(true)`. `src/local/flags.rs:399–406` removes a source
after an apparently successful synchronization without checking whether it is
also the destination.

Both commands were reproduced in disposable directories:

```text
barqsync -a --inplace -I f f       -> f becomes empty; exit 0
barqsync -a --remove-source-files f f -> f disappears; exit 0
```

Reference rsync preserved the file in both cases and reported an error for the
removal attempt.

**Implementation:** Compare opened file identities (`dev`, `ino`) before
truncation, in-place delta work, backup, or removal. Handle hard-link aliases and
equivalent paths, not just equal path strings. Keep the destination identity in
the source-removal guard. Detect identity again at the point of mutation.

**Acceptance:** Same-path, `./` aliases, hard-link aliases, and directory-source
variants preserve the source and destination; normal `--inplace` behavior remains
supported.

### DATA-02 — Plan deletion across the union of overlapping roots

**Evidence:** `src/local/mod.rs:185–193` processes roots separately.
`src/local/exec.rs:543–545,565–580` deletes destination entries absent from the
current root's listing.

For `a/alpha`, `b/beta`, and an existing destination:

```text
barqsync -a --delete a/ b/ dst/
```

returned **0** but left only `dst/beta`. Reference rsync left both `alpha`
and `beta`. Remote roots also share destination paths and need equivalent
coordination; remote manifestations were not separately reproduced here.

**Implementation:** Build a merged destination namespace for roots that overlap.
Resolve duplicate-name precedence consistently with rsync. Perform deletion only
after the relevant complete source set is known, with one owner for each
destination path. Avoid racing multiple root tasks against the same destination.

**Acceptance:** Test local, push, and pull; existing and newly created
destinations; both scanners; duplicate names; filters; and nested directories.
Only entries absent from the complete selected source set may be deleted.

### DATA-03 — Suppress deletion when the source view is incomplete

**Evidence:** `src/local/flags.rs:29–53` reports and drops dangling symlinks under
`-L`, but `src/local/exec.rs:543` still deletes against the reduced listing.
`src/fsops/mod.rs:170–171` discards individual `read_dir` errors, and
`src/scan.rs:63–89` drops child/stat errors while emitting the remaining listing.

With `src/keep -> missing-target` and valuable `dst/keep`:

```text
barqsync -aL --delete src/ dst/
```

reported an error but deleted `dst/keep`. Reference rsync reported the error and
skipped deletion.

**Implementation:**

- Carry explicit scan completeness/error state with directory listings.
  Propagate individual enumeration, metadata, ACL, and xattr errors instead of
  turning an incomplete tree into an authoritative absence list.
- For a safe initial implementation, defer deletion until scanning has completed
  successfully. A later streaming design may use a precisely defined, tested
  completeness barrier.
- Add a remote scan-error/completion message before deletion can be authorized.
  The final `SenderDone.errors` value arrives too late to undo earlier deletions.
- Avoid treating intentional filtering or a supported vanished-file condition as
  the same thing as an unreported I/O error.

**Acceptance:** The dangling-link reproduction preserves `dst/keep`. Fault
injection for permission errors, unreadable directories, failed stats, and
interrupted listings also preserves destination-only data.

### DATA-04 — Make successful backup a prerequisite for replacement

**Evidence:** `src/local/flags.rs:460–483` reports backup errors but returns no
failure to its callers. `src/local/exec.rs:1377–1378` and remote receiving paths
continue to overwrite the destination.

With `dst/blocked` being a regular file, running
`-a -b --backup-dir=blocked/sub src/ dst/` replaced `dst/f` with new data despite
failing to save its old contents. Exit status was 23. Reference rsync preserved
the original and failed before transfer.

**Implementation:** Return `Result` from backup preparation and require callers
to stop that file's replacement on failure. Audit regular files, symlinks,
hard-link followers, special files, and io_uring paths. Write a replacement
backup atomically before discarding any prior backup; reject backup paths that
alias the original or an active output.

**Acceptance:** Permission denied, ENOSPC, invalid backup directory, copy failure,
and backup collision leave the original destination and any prior valid backup
intact. A later successful retry preserves the intended previous version.

### DATA-05 — Validate file completeness before committing

**Evidence:** `src/remote/receiver.rs:621–705` accepts data offsets without
checking them against the declared size. `FileEnd { ok: true }` can proceed to
`set_len()` and rename even if no bytes arrived.
At lines 715–718 and 753–755, invalid `SmallPack` slices become empty slices via
`unwrap_or(&[])`.

Reproductions:

- `FileBegin(size=4)` followed immediately by successful `FileEnd`: a four-byte
  zero-filled file was committed, **exit 0**.
- `SmallPack(files=[(id,4)], blob=[one byte])`: an empty file was committed,
  **exit 0**.

**Implementation:**

- Validate the complete pack before creating files: checked length arithmetic,
  exact blob coverage, unique requested IDs, and valid xattr associations.
- Maintain a file state machine and validated received/copy ranges. Reject
  out-of-bounds writes, arithmetic overflow, missing ranges, unexpected IDs,
  duplicate begin/end messages, and inconsistent sizes.
- Validate all delta copy ranges against both source/basis size and output size.
  Receiving a declared length must never substitute for receiving its contents.
- Mark a file failed after any data-write error; never rename or acknowledge
  that file later merely because `FileEnd.ok` is true.
- Add a final content-integrity check bound to the transfer. Strong hashes used
  to choose reusable blocks do not validate the final assembled file or detect
  a basis changing after its signatures were calculated. Benchmark the additional
  I/O and use a negotiated digest or verified extent scheme with explicit rules.
- Finish verification before success metadata, atomic replacement, and
  source-removal acknowledgement. Detect source changes rather than silently
  declaring a mixed-version copy successful.

**Acceptance:** Both probes fail without replacing an existing destination.
Include truncated packs, overlapping/out-of-range ranges, mutated basis files,
failed writes, sparse files, deltas, and both encodings.

### DATA-06 — Strengthen source-removal change detection

**Evidence:** `src/local/flags.rs:58–66` checks size and mtime seconds only.
`SyncedFiles` at `src/proto/mod.rs:110–111` also omits nanoseconds and identity.

A public-helper probe changed a four-byte file from `OLD!` to `NEW!`, keeping its
size and mtime second while moving nanoseconds from 100 to 900,000,000.
`remove_source()` returned success and deleted the changed file.

**Implementation:** Record and compare full-resolution mtime, appropriate ctime,
file type, device/inode, and size from the file actually transferred. Revalidate
after transfer and immediately before removal. Use a sender-owned ledger, as in
SEC-02, and document the remaining limits of concurrently modified sources.
Do not equate metadata checks with a filesystem snapshot.

**Acceptance:** Same-second edits, same-size replacements, inode replacement,
and symlink swaps prevent removal. The already verified, unchanged source is
removed only when explicitly requested.

## NET-01 — A real macOS streaming solution

### Diagnosis

`src/cli.rs:501–508` turns bare `--streams N` into TCP mode.
`src/remote/streams.rs:52–56,81` obtains the **server-side** SSH address from
`SSH_CONNECTION` and binds a new TCP listener to a random port.
`src/remote/quic.rs:94–100` does the same with UDP. This happens on the SSH server
for both push and pull, regardless of which side is receiving files.

Therefore SSH and the data streams are different inbound services. Permission
for Remote Login/sshd does not imply permission for a separately running
`barqsync` executable. Apple's firewall supports per-application permission and
an option for automatically allowing qualifying signed downloaded software.
[Apple: firewall settings](https://support.apple.com/en-gb/guide/mac-help/-mh11783/mac)

The inspected **local debug binary** was ad hoc, linker-signed with no
TeamIdentifier. Release configuration has no explicit Developer ID signing or
notarization setup; downloaded release artifacts were not inspected.
An ad hoc signature is not a stable Developer ID distribution identity.

Two related limitations share this design: the server's advertised private address
may be unreachable through NAT or ProxyJump, and SSH port forwarding does not
automatically expose the additional random data port. Increasing timeouts,
changing TLS certificates, or switching TCP to QUIC does not resolve those
reachability conditions.

### Recommended product design: parallel SSH data sessions

Add **`--transport ssh-streams`** as a transport with multiple independently
authenticated SSH data sessions. These sessions carry file frames over stdin/stdout;
only the already permitted SSH service needs to accept network connections.

Proposed flow:

```text
client control connection -- SSH --> sshd --> main barqsync --server
client data connection 1  -- SSH --> sshd --> barqsync stream helper --+
client data connection 2  -- SSH --> sshd --> barqsync stream helper --+--> private
...                                                                  Unix socket
client data connection N  -- SSH --> sshd --> barqsync stream helper --+--> main server
```

This is a proposed architecture, not functionality currently available in v0.3.0.

1. **Negotiate capabilities and a session.** The main server creates a random
   session ID, secret, and short-lived Unix-domain rendezvous socket inside a
   newly created owner-only directory. It sends the session offer over the
   authenticated control connection.
2. **Attach helpers.** The client starts bounded `ssh -T` child processes using
   the same destination, account, key, port, jump-host, and remote executable
   configuration. Each helper attaches to the existing session; it must not
   start a separate synchronization engine.
3. **Protect local IPC.** Use directory mode 0700 and socket permissions 0600;
   verify directory ownership and Unix peer credentials where supported.
   Validate session, role, and unique stream index. Send the secret over the
   SSH stream, never in argv, logs, or a world-readable file. Reject stale or
   duplicate joins.
4. **Reuse framing and routing.** Feed the authenticated channels into the
   existing data-stream abstraction. SSH supplies transport authentication and
   encryption; retain the existing pinned-certificate and token authentication
   for optional direct TLS/QUIC modes.
5. **Make actual connection behavior explicit.** Independent SSH connections
   are needed for multiple TCP flows. OpenSSH ControlMaster can share multiple
   sessions over one network connection, so counting channels alone does not
   establish that independent flows exist.
   [OpenSSH: connection sharing](https://man.openbsd.com/ssh_config.5)
   Preserve host-key verification. Use a bounded authentication strategy;
   agent/key authentication supports independent sessions, while password-only
   setups may need a shared connection or a clear fallback without N simultaneous
   password prompts.
6. **Own lifecycle centrally.** Give all channels a startup deadline and shared
   cancellation. On exit, close channels, terminate/reap owned helper children,
   and remove the session socket/directory. Control-session loss must end the
   attached data session. Implement NET-03 before relying on this transport.
7. **Choose predictable CLI behavior.** Keep the ordinary no-stream SSH default.
   In the next compatible release, make bare `--streams N` choose SSH streams;
   explicitly document that change. Keep `--transport tcp` and `quic` as direct
   transport choices. Define a negotiated, acknowledged fallback to the single
   SSH connection if extra sessions cannot be opened. With strict transport
   selection, fail clearly instead of silently changing modes.
8. **Version the protocol.** Adding session offers, capabilities, acknowledgements,
   and transport variants requires a new protocol version and tests against old
   peers. Fall back before transfer begins or report an actionable version
   mismatch. Do not let the two peers make independent fallback decisions.

A smaller alternative is forwarding a **loopback-only** TLS listener through
`ssh -W host:port`, which OpenSSH supports. However, that depends on server
forwarding policy and needs its own loopback/firewall testing. The Unix-socket
helper design avoids requiring `AllowTcpForwarding` and avoids a TCP rendezvous
listener. [OpenSSH: `-W`](https://man.openbsd.org/ssh),
[OpenSSH: forwarding policy](https://man.openbsd.org/sshd_config)

**Performance constraint:** Multiple SSH connections add process and encryption
cost. Benchmark them; do not promise direct-TLS throughput. Putting all data
channels in one ControlMaster connection also retains that connection's TCP
loss coupling. Neither arrangement fixes the single-large-file scheduling
limitation in PERF-02 by itself.

### Direct TCP/QUIC support with the firewall enabled

- Sign release binaries using a consistent Developer ID identity and integrate
  Apple's applicable notarization/distribution workflow. Verify the downloaded
  arm64 and x86_64 artifacts, not only the build intermediates.
  [Apple: notarizing macOS software](https://developer.apple.com/documentation/security/notarizing-macos-software-before-distribution)
- Document explicit permission for the exact installed executable through
  System Settings → Network → Firewall → Options. A permitted app and a signed
  release still remain subject to administrator policy, block-all rules, and
  upstream network restrictions.
- For a user who chooses direct mode, an administrator can register and allow
  the resolved executable with `socketfilterfw --add` and `--unblockapp`.
  Resolve the actual path on the machine running the server first. Do not
  assume every installation is at `~/.cargo/bin/barqsync`.
- Do not automatically alter firewall rules, re-sign an installed binary, or
  turn off block-all or the firewall. The historical `codesign --force -s -`
  workaround in `ISSUES.md` is not the distribution solution.
- Report attempted transport, endpoint, negotiated stream count, fallback, and
  error category. A reset/timeout alone does not prove a firewall cause; also
  consider routing, address selection, NAT, and server failure.

**Acceptance:** On two real Macs with firewall enabled and SSH already allowed,
push and pull with four SSH data sessions must finish without granting
`barqsync` public inbound access. Verify hashes, negotiated channels, no leaked
helpers, and unchanged firewall configuration. Repeat after upgrading the binary.
For direct mode, separately test a valid per-app allowance and a blocked app.
If policy blocks SSH itself, report that prerequisite; the transport cannot
legitimately bypass it.

## Runtime, protocol, and performance

### NET-02 — Keep blocking protocol coordination outside the worker pool

**Evidence:** `src/remote/sender.rs:623–663` and
`src/remote/receiver.rs:1040–1129` run a blocking receive loop inside a Rayon
scope on the configured pool. With one worker, queued scan/listing work cannot
run while that worker waits for a message that depends on the queued work.

A small remote push through the local fake-SSH fixture timed out after seven
seconds with `--threads=1`; the otherwise identical two-worker run succeeded.

**Implementation:** Run protocol coordination on a separate thread/task. Use
the worker pool for finite filesystem/hash work, with cancellation-aware queues.
Audit blocking sends, pack admission, and refinement waits for pool starvation.
Silently forcing a minimum of two threads is not a complete fix for deeper
backpressure cycles.

**Acceptance:** Push/pull, no-op, whole-file, delta, packs, and xattrs finish with
one and two workers, both I/O backends, and slow peers. Add test deadlines.

### NET-03 — Make disconnects and cancellation terminate every participant

**Evidence:**

- `src/remote/streams.rs:139–140,364–365` clears socket timeouts after authentication.
- `src/proto/mod.rs:306–310` performs blocking queue sends; `Conn::finish()` at
  lines 621–629 sends close sentinels and joins writers without a deadline.
- At lines 455–465, clean control EOF returns from the reader. Other attached
  data readers can keep the shared channel alive, so control EOF need not wake
  the coordinator as a disconnected peer.
- `src/remote/client.rs:165–166` finishes the connection and waits for the SSH
  child without a bounded cancellation/reaping path.

**Implementation:** Introduce session cancellation and explicit channel-failure
events. Distinguish normal completion from unexpected control EOF. Cancel queue
waits, refinement waits, pack admission, active sockets, and tasks on terminal
failure. Use bounded shutdown and reap all owned child processes. Propagate
writer errors instead of dropping them. A deliberately idle, healthy transfer
must remain distinguishable from a dead or unresponsive peer.

**Acceptance:** Drop only control, drop one data stream, stop a peer reading,
and interrupt during setup/hash/write/finish. Each exits within a documented
bound, preserves correct partial-file behavior, and leaves no server/helper.
The older orphan-server report is a useful test target, not proof of one exact
root cause.

### PROTO-01 — Chunk large metadata and propagate send failures

**Evidence:** `src/remote/sender.rs:79–84` sends an entire directory as one
`DirBatch`; lines 74–75 discard send errors. `src/proto/mod.rs:53,200–236`
caps encoded frames at 64 MiB. A listing/signature exceeding that limit can be
registered but never transmitted; the sender then waits for a `Need` that
cannot arrive. Compression also needs consistent limits on both encoded and
decoded lengths.

**Implementation:** Return and propagate `Result` for control sends. Chunk
directory listings, signatures, refinement responses, and large metadata under
explicit byte limits. Add sequence/completion markers, bound reassembly, and
make completion accounting depend on successful transmission. Recheck small
file size at open time: `send_small()` uses `fs::read()` after an earlier size
classification and can read a file that has since grown far beyond pack limits.

**Acceptance:** Synthetic metadata at and above the limit, highly compressible
oversized messages, large directories, and small-to-large source growth cannot
cause silent loss or indefinite waiting. No huge real directory is required
for the protocol-level boundary tests.

### NET-04 — Bound stream admission and validate negotiation

**Evidence:** The blocking TLS accept loop at `src/remote/streams.rs:104–115`
authenticates connections serially. An unauthenticated slow connection can
delay legitimate arrivals. The async path at lines 280–303 creates per-connection
tasks without a concurrency semaphore. `src/remote/quic.rs:112–120` waits for
the first connection's control stream/token before trying the next connection.

**Implementation:** Use bounded concurrent authentication, absolute handshake
deadlines, cancellation, and admission limits. Socket read timeouts alone are
not an absolute handshake deadline against trickled bytes. Validate requested
counts against negotiated maxima and require unique, in-range stream indexes.
Enforce option/resource limits after deserializing `Hello`, not only in CLI parsing.

**Acceptance:** A stalled or wrong-token connection cannot monopolize setup;
valid clients still connect. Task, descriptor, and memory use remain bounded.
Wrong certificates and tokens continue to be rejected.

### PERF-01 — Establish a byte-based memory budget

**Evidence:** `src/proto/mod.rs:441–445,537–545` bounds queues by message count.
With 128 streams, four queued approximately 4 MiB packs per stream alone can
account for about **2 GiB** of payload capacity, before active buffers, decoded
frames, and metadata. This is a capacity estimate, not measured peak RSS.
Frames can be as large as 64 MiB. Sender listings and receiver pending state
also accumulate as scanning runs ahead of transfer.

`src/remote/mod.rs:64` allocates a 32 MiB stack reservation per worker; 64 workers
can reserve another 2 GiB of virtual address space. Reservation is not the same
as committed physical memory.

**Implementation:** Introduce a session-wide byte budget covering queued and
in-progress frames, compression copies, file buffers, and metadata. Bound
outstanding listings and active files; reserve capacity for control messages so
backpressure cannot prevent progress. Reduce frame cloning, tune worker counts
from measurements, and eliminate nesting that requires very large stacks.

**Acceptance:** Measure RSS, virtual memory, queue bytes, descriptors, and CPU
for 1/4/16/128 streams against a deliberately slow receiver. Test wide and deep
trees. Publish the chosen default memory budget and measured throughput tradeoff.

### PERF-02 — Schedule large-file work across streams and report live progress

**Evidence:** `src/proto/mod.rs:343–367` assigns all frames for one file to one
stream. `src/remote/receiver.rs:1067` writes large-file data from one receive
loop, with the shared incoming-file lock held during writes at lines 621–638.
Local whole-file progress is credited after `write_file()` returns at
`src/local/exec.rs:1378–1379`.

**Implementation:** After DATA-05, add bounded, ordered per-file write work
queues so slow disk writes do not block all control traffic. Consider opt-in
striping of large-file extents across streams with explicit offsets, checksums,
completion barriers, and backpressure. Update local progress incrementally in
buffered copy paths; account carefully for instantaneous clone operations.

**Acceptance:** Benchmark one large file separately from many small files on
LAN, high-latency, and lossy links. Compare 1/4/8 streams and measure disk
contention. Reordered extents and interrupted striped transfers remain correct.
Do not make striping the default until the measurements justify it.

### PLAT-01 — Avoid mmap crashes on mutable files

**Evidence:** `src/delta/cdc.rs:15–17,214–224` explicitly acknowledges that a
file shrinking after mapping can cause SIGBUS. Checking size once after mapping
does not eliminate later truncation.

**Implementation:** Use checked positional reads for live files, or operate on
a private stable snapshot where the filesystem supports it. Apply the policy
to source and basis files. Treat snapshot acquisition or source-change failure
as retryable or a partial transfer; do not attempt to recover arbitrary Rust
execution by catching SIGBUS.

**Acceptance:** A subprocess test truncates the file after mapping/read setup.
The synchronization exits cleanly or retries without committing invalid output.
Measure the buffered CDC alternative before retaining mmap as an explicit
immutable-input optimization.

### CLI-01 — Parse remote locations and home-relative paths deliberately

**Evidence:** `src/paths.rs:19–29` splits at the first colon, so
`user@[::1]:/data` is split inside the IPv6 address.
`src/remote/client.rs:46` only normalizes an empty path; the server receives
`~/data` literally. `spawn_server()` quotes the entire `--remote-path`, which
also suppresses shell tilde expansion.

**Implementation:** Parse bracketed IPv6 syntax explicitly, retaining raw
filename bytes. Validate hosts and reject leading-option/empty-user forms before
invoking the remote shell. Expand supported leading `~/` paths in a controlled
remote-side path resolver. Keep shell quoting for all other characters; never
fix tilde handling by evaluating arbitrary shell expressions. Document whether
`~otheruser` is supported.

**Acceptance:** IPv4, DNS names, SSH aliases, bracketed IPv6, spaces, quotes,
non-UTF-8 paths, `~/`, absolute paths, and malformed hosts have deterministic
behavior without command execution from path content.

## MAINT-01 and further enhancements

The dependency lookup returned
[RUSTSEC-2023-0089](https://rustsec.org/advisories/RUSTSEC-2023-0089.html) for
`atomic-polyfill 1.0.3`. RustSec classifies this as **unmaintained**, not a
demonstrated exploitable vulnerability. `cargo tree --target all` traces it
through `heapless 0.7.17 → postcard 1.1.3`; it was absent from the current
macOS target's active tree.

- Review postcard's enabled features and supported target trees; remove the
  unused dependency path or upgrade through a supported upstream change.
  Add scheduled and PR dependency auditing, with narrowly documented exceptions
  for informational or target-inapplicable advisories.
- Add the malicious-peer and data-loss regressions above. Existing happy-path
  equivalence tests are useful but do not cover hostile message sequences or
  several destructive flag combinations.
- Extend CI with the declared minimum Rust version, explicit tokio-I/O tests,
  TCP and QUIC coverage on macOS and Linux, and test-level hang deadlines.
  Make skipped real-host checks visible instead of counting them as evidence
  that a real network or firewall passed.
- Add controlled filesystem fault injection for ENOSPC, short writes, stat
  failures, interrupted scans, failed rename, and failed backup. Use fuzzing for
  protocol state transitions, delta ranges, and path validation.
- Pin third-party CI actions to reviewed revisions and use automated update PRs.
  Keep release signing credentials limited to the release job and verify packaged
  signatures/checksums. Implement the signing work specified in NET-01.
- Add a read-only diagnostic command that reports local version/protocol,
  selected executable, transport, endpoint reachability, stream count, and
  relevant firewall/signature state when available. Keep secrets and tokens out
  of diagnostics. Do not execute privileged repair commands automatically.
- Strengthen `--fsync` documentation and offer/test crash durability semantics:
  file contents and metadata, atomic rename, and parent-directory persistence
  are separate concerns. Establish the intended platform guarantees before
  promising durable backups.
- Improve progress/error reporting: distinguish queued, transferred, verified,
  and failed files; report transport fallback even when performance expectations
  would otherwise be misleading. Give actionable missing-remote-binary and
  protocol-version messages.

## Implementation sequence

These are unimplemented work items. Completion means the acceptance criteria
have passed, not merely that code has been written.

| Stage | Work | Dependencies | Release gate |
| --- | --- | --- | --- |
| 1 | Add regression fixtures; fix SEC-01/02 and DATA-01/04 | None; share path/identity primitives | No unauthorized or dry-run mutation; same-file and backup probes preserve originals |
| 2 | Fix DATA-02/03/05/06; validate protocol states and acknowledgements | Stage 1 ledger/confinement design | Exact content, complete source namespace, and safe deletion on failure |
| 3 | Fix NET-02/03 and PROTO-01; bound stream admission | May proceed alongside correctness work with coordinated protocol changes | One-worker transfers, oversized metadata, dropped channels, and Ctrl-C terminate correctly |
| 4 | Implement NET-01 SSH streams, direct-mode diagnostics, signed releases | Stages 1–3; new protocol negotiation | Two real Macs transfer with firewall enabled and no new public barqsync listener |
| 5 | Apply PERF-01 and PLAT-01; benchmark PERF-02 | Correctness and cancellation foundation | Memory budget holds; mutable files do not crash; no correctness regressions |
| 6 | Finish CLI-01 and maintenance/usability improvements | Relevant shared parser/protocol work | Compatibility matrix and release artifact checks pass |

Keep remediation commits small enough to review. Changes that can be safely
backported without changing the wire format should not wait for all transport
or performance enhancements. Changes to `Msg`, `Options` serialization, source
acknowledgements, and session negotiation must be versioned together.

## Validation matrix and definition of done

| Dimension | Required cases |
| --- | --- |
| Platforms | macOS arm64/x86_64; Linux x86_64/aarch64 for supported releases |
| Filesystems | APFS; Linux filesystem with/without reflinks; representative external/cross-volume storage |
| Direction | Local, push, pull |
| Transport | Single SSH, new SSH streams, direct TLS/TCP, QUIC, auto/fallback |
| Execution | One/two/default workers; thread and tokio I/O; postcard and rkyv |
| Content | Empty/small/large/sparse files; whole-file and delta; growing/truncated files; non-UTF-8 names |
| Metadata | Symlinks, hard links, permissions, full-resolution times, xattrs, ACLs |
| Destructive flags | `--delete`, multiple roots, `--inplace`, `--backup-dir`, `--remove-source-files`, dry run, partial files |
| Failures | Scan errors, disk errors, malformed peers, bad auth, interrupted setup, blocked readers, control/data EOF |
| Networks | LAN, latency/loss, NAT, ProxyJump, blocked UDP, blocked direct TCP |
| macOS firewall | Enabled with barqsync blocked; direct app allowed; signed-release upgrade; managed/block-all policy with SSH prerequisite explicitly checked |

Every P1 finding needs a failing regression on the reviewed version and a passing
regression on the fix. Compare contents and metadata, not only exit status or
file size. Every failure test must also verify that protected source/destination
sentinels remain intact and owned processes/resources are cleaned up.

Record performance on reproducible datasets using repeated runs, medians and
variation, CPU, RSS, and negotiated transport. Existing benchmark results remain
historical context; they do not establish the speed of the proposed changes.

## Reproducing this review

The diagnostic programs print the observed behavior. They are deliberately not
passing regression tests and must be converted into assertions when fixes land.
All mutations use newly created temporary trees. No real remote host is used.
The CLI probe kills only its own process group when a test exceeds its deadline.

These historical probes target the audited v0.3.0 commit (`f9f8d8d`), including
its public Rust APIs. Run them in a separate checkout of that revision. Current
regression tests and commands are listed in [the work records](docs/implementation/README.md).

From the audit checkout root:

```bash
cargo build --locked

rustc --edition=2024 docs/review/2026-10-20/protocol_probes.rs \
  -L dependency=target/debug/deps \
  --extern barqsync=target/debug/libbarqsync.rlib \
  -o /tmp/barqsync-review-protocol-probes
/tmp/barqsync-review-protocol-probes

# Set RSYNC if the first rsync on PATH is not rsync 3.x.
python3 docs/review/2026-10-20/cli_probes.py

env -u BARQSYNC_TEST_REMOTE cargo test --locked --offline
cargo clippy --locked --offline --all-targets -- -D warnings
```

See [recorded results](docs/review/2026-10-20/results.txt) for the original
outputs, including the dependency query and environment limits.

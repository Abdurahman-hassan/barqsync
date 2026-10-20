# Peer input: listings, file data and large messages

Date: 21 October 2026.

Status: implemented as described below; it covers part of 1B (SEC-01), part of 2B
(DATA-05) and part of 3C (PROTO-01). Verification is recorded at the end.

Findings: SEC-01, DATA-05, PROTO-01. The protocol stays at version 10, which no release
speaks yet; its framing gains one frame kind.

## Changes

- **SEC-01, names and listings.** `src/remote/check.rs` checks what the sending side
  announces before the receiver plans or touches anything. A name must be one
  non-empty path component: not `.` or `..`, no `/`, no NUL. Other bytes are allowed,
  including non-UTF-8 names. Each name may appear once per listing. Roots must be
  well formed (a `src/` root has a directory's metadata and no items; other roots
  have valid items). A listing's root must exist, its id must be new, its relative
  path must be valid, and it must belong to a directory the receiver accepted: the
  root, an item of a root, or a directory entry of an accepted listing. Each such
  listing is accepted once. The top listing of a root of named items must repeat
  those items. Anything else ends the transfer with a protocol error (exit 12),
  sends `Fatal` to the peer, and changes nothing for the refused message.
- **SEC-01, several roots.** With more than one root, all roots write into one
  destination, and one root's entry can sit where another root has a directory.
  The receiver writes into directories by path. A symlink substituted for such a
  directory would redirect that directory's remaining entries, and its attributes,
  to the symlink's target. The receiver therefore takes each directory it writes into
  (`Overlap` in `src/remote/receiver.rs`): it is taken only while it is still a
  directory, and afterwards no symlink may replace it and no other root may remove it
  for a non-directory. With `-K`, a symlink this transfer made never stands in for a
  directory. With a single root, a listing's names are unique, so the guard is off.
- **DATA-05.** Literal data must arrive in order, exactly at the next missing offset,
  before the next copied range and within the announced size. A file is put in place
  only when its data reaches that size. A small-file pack must have lengths that add
  up to its blob exactly, each file once, and attribute values only for its own files.
  Delta copies must be copies, in order, not overlapping, inside the new file and
  inside the basis as it is now. A write error, a range out of place, a short file,
  `FileEnd` without data, or an invalid pack or delta discards the temporary file,
  reports the path and makes the transfer exit 23. Later data for that file is
  dropped. `FileEnd { ok: false }` never commits.
- **PROTO-01, framing.** A message larger than a frame (64 MiB) is cut into several
  frames. Every piece but the last has kind `KIND_PART`; the last carries the message's
  own kind. Each piece is compressed separately, so compressed and decompressed
  frame sizes have the same limit. Both readers (threads and Tokio) collect the
  pieces, up to 1 GiB per message, and treat an end of stream inside a message as an
  error. This applies to every message, including the handshake, on the SSH pipe,
  TCP streams and QUIC.
- **PROTO-01, send failures.** A message that cannot be encoded (over 1 GiB) stops
  the transfer: the side that failed tells its peer with `Fatal`, and its receive loop
  stops and reports the message it could not send. A closed connection is left to the
  receive loop as before. On the sender, every failed send still counts as an error.
  A refinement request that is not sent no longer waits for an answer.
- A file listed as small and found larger than the small-file size when it is read
  is sent as a large file. The whole file is no longer read into one pack.

## Verification coverage

`tests/peer_input.rs` plays the sending side by hand over a socket pair against the
real receiver, with postcard and rkyv and with threaded and Tokio connection layers:

- the audit's traversal probe and seven more invalid names (`..`, `.`, empty, `a/b`,
  absolute, embedded `..`, NUL): exit 12, nothing written in or next to the destination;
- malformed roots, unknown roots, unannounced, repeated or `..` listings, reused listing
  ids, duplicate names and named items that disagree with their roots;
- valid unusual names, including non-UTF-8 ones on Linux (APFS refuses those);
- two roots swapping a directory for a symlink between a directory's creation and its
  listing, with and without `-K`: nothing is written outside the destination;
- the audit's missing-data and short-pack probes plus long packs, stray pack
  attributes, partial, oversized and out-of-order data, and `FileEnd` without data:
  the old file stays, no temporary file is left, exit 23;
- delta copies past the basis or the file, overlapping or overflowing copies, literal
  ranges in the copy list, missing literal data and literal data over a copy, plus a
  valid delta that still applies;
- a 450,000-entry listing larger than one frame, answered normally;
- a listed small file that grows before it is sent, delivered as a whole file.

`messages_cut_into_frames_cross_every_transport` runs the real client and server
through fake SSH: push and pull over SSH, TCP streams and QUIC, threaded and Tokio
I/O, both wire formats, with and without `-z`. `BARQSYNC_TEST_FRAME_PIECE=4096` cuts
messages at 4 KiB so they cross as many frames. Without it, a message would need more
than 64 MiB. Unit tests cover name rules, listing bookkeeping, pack and copy checks,
and a real two-frame message.

The eight regression tests fail on the unfixed tree (`eeb6955`): the traversal name is
written outside the destination, zero-filled and empty files are committed, a bad
delta exits 0, a listing over 64 MiB cannot be sent, a grown file is packed, and with
two roots a file is written outside the destination.

## Remaining acceptance work

- **Descriptor confinement is not implemented.** Writes, renames, metadata updates and
  deletions still use paths. The checks above stop a peer from naming a path outside
  the destination, and the multi-root guard stops this transfer's own entries from
  redirecting each other. A local process that swaps a destination directory for a
  symlink during a transfer is not covered. Closing that needs `openat`-style walks
  with no-follow checks (part 1B).
- No end-to-end content hash: a file whose ranges are all present and in place is
  committed, but its assembled bytes are not checked against the source's digest,
  and a basis that changed after its signature was sent is detected only when it
  shrank (part 2B).
- Messages up to 1 GiB are accepted and held whole. Memory across many data
  streams is not budgeted (PERF-01), and stream admission is not bounded (NET-04).

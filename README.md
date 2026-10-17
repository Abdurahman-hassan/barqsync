# barqsync

A parallel, rsync-compatible file synchronizer written in Rust.

rsync is single-threaded on both the sending and receiving side, and its receiver makes one
blocking open/write/rename/utimes sequence per file. barqsync keeps rsync's command line and
semantics but runs one task per directory across all cores, copies files in parallel, and
uses copy-on-write clones (APFS `fclonefileat`) or in-kernel copies (Linux
`copy_file_range`) so file data never passes through userspace when it doesn't need to.

## Status

| Milestone | Scope | State |
| --- | --- | --- |
| M0 | toolchain, crate, release profile | done |
| M1 | CLI, filters, parallel scan, quick-check diff, `-n -v` | done |
| M2 | local sync engine: dirs, files, symlinks, specials, perms/times/owner, `--delete`, temp+rename, Ctrl-C cleanup | done |
| M3 | delta engine: aligned blocks, FastCDC fallback, clone+patch, BLAKE3-verified matches | done |
| M4 | remote over SSH (`host:path`): push and pull, small-file packing, delta over the wire, `-z`, `-e`, `--remote-path` | done |
| M5 | parallel TLS data streams (`--streams N`), pinned ephemeral certificate, token auth, ssh fallback | done |
| M6 | `-H`, `--progress`/`--info=progress2`, `-i`/`%i`, `getattrlistbulk`/`statx` metadata reads | done |
| M7 | common rsync flags: `-c -S -R -d -L -k -K -b -O`, `--partial(-dir)`, `--files-from`, `--existing`, `--max/min-size`, `--link/compare-dest`, `--numeric-ids`, `--chmod`, `--remove-source-files`; page-cache hints | done |
| M8 | verification harness: Linux container with sshd and `tc netem` link profiles, `strace -c`, flamegraphs, `cp` baselines | done |
| M9 | the original plan's delta design as `--delta=cdc` (FastCDC throughout, xxh3 + BLAKE3, mmap); measured slower, so `blocks` stays the default | done |
| M10 | the original plan's scanner as `--scanner=jwalk` (jwalk + DashMap index + queues A–D); measured slower, so `dirtask` stays the default | done |
| M11 | QUIC transport (`--transport quic`, quinn), rkyv wire format (`--wire rkyv`), tokio connection layer (`--net-io=tokio`) | done |
| M12 | adaptive strategy (`--strategy adaptive`): link probe, whole-vs-delta routing by link speed, pack compression with link-based zstd levels, `--compress-level`; up to 7.6× on compressible data, slower on incompressible, so `fixed` stays the default | done |
| M13 | io_uring batches for received small files (`--io-uring`, Linux): 24% fewer syscalls, no faster, so opt-in; found on the way: skipping a redundant truncate made small-file pushes onto XFS 3.2× faster | done |
| M14 | full verification: local A/B/C/D vs rsync and `cp`, Linux syscall counts and flamegraphs, every transport and strategy over lan/wan/lossy links; README and as-built plan updated | done |

The test suite passes on macOS (APFS) and Linux (aarch64 container, compared against rsync 3.2.7).

## Build

```bash
brew install rustup && rustup default stable   # macOS; or https://rustup.rs
cargo build --release
./target/release/barqsync -av src/ dst/
```

With Homebrew's keg-only rustup, add `/opt/homebrew/opt/rustup/bin` to `PATH`.

## Supported options

`-a` (= `-rlptgoD`), `-r -l -p -t -g -o -D --devices --specials`, `-H`, `-v -q -n`,
`-i`/`-ii` (`%i` in `--out-format`), `--progress`/`-P`/`--info=progress2`,
`--out-format` (`%n`, `%L`), `--delete`, `--delete-excluded`,
`--exclude/--include/--exclude-from/--include-from` (rsync wildcard rules, first match wins,
`/` anchoring, trailing `/` for directories, `**`, `dir/***`), `-u`, `--size-only`, `-I`,
`--ignore-existing`, `-x`, `--modify-window` (`-1` compares nanoseconds), `-W/--whole-file`,
`--no-whole-file` (delta transfer; locally off by default, like rsync), `--inplace` (also
writes new files directly, like rsync),
`--fsync`, `--stats`, `-h`, `--threads N`, `--drop-cache`/`--no-drop-cache` (page-cache hints
for files of 8 MiB+, on by default; see [Page-cache hints](#page-cache-hints)),
`--delta=blocks|cdc` (delta design, see [Delta transfer](#delta-transfer); for a remote
transfer the receiver's choice applies), `--scanner=dirtask|jwalk`, `--transport
ssh|tcp|quic` with `--streams N`, `--wire postcard|rkyv` (see [Remote transfers](#remote-transfers)),
`--strategy adaptive|fixed` and `--compress-level N` (see [Adaptive strategy](#adaptive-strategy---strategy-adaptive)),
`--io-uring` (Linux; see [io_uring](#io_uring---io-uring-linux)).

Also, with rsync's semantics locally and over ssh (each has a differential test in
`tests/rsync_flags.rs`):

| Option | Notes |
| --- | --- |
| `-c/--checksum` | Same-size files are compared by whole-file BLAKE3 (128-bit). Files are hashed in parallel, by the sender for its side. A rewritten file shows `>fc`. |
| `-S/--sparse` | Runs of zeros (4 KiB blocks) become holes. Clones and kernel copies are skipped, since they would keep the zeros as data. |
| `--partial`, `--partial-dir=DIR`, `-P` | On Ctrl-C or a dropped connection, the received prefix is kept as the destination file, or in `DIR` (created 0700). A file left in `DIR` is the delta basis next time and is removed once the file is complete. A relative `DIR` is excluded from the transfer and protected from `--delete`. |
| `-R/--relative` | Full relative paths, `/./` to trim the prefix. Implied directories get their source attributes but are never pruned by `--delete`. |
| `--files-from=FILE`, `-0/--from0` | Implies `-R` and `-d`; `-a` no longer implies `-r`. `#`/`;` comments, a trailing `/` lists a directory's contents, `.` or a leading `./` includes the root itself. `-` reads stdin. The client reads the list and sends it to the server. |
| `-d/--dirs` | Directories are created without recursing. |
| `--existing`, `--max-size`, `--min-size` | rsync's size syntax (`1.5M`, `4KB` = 4000, `1M-1`). |
| `-b/--backup`, `--suffix`, `--backup-dir` | Replaced and deleted files are kept (hard link, or a copy with `--inplace`). Without `--backup-dir`, `*~` backups are protected from `--delete` and `-O` is forced on, as in rsync. |
| `--link-dest`, `--compare-dest` | Checked even when the destination file exists but differs. Identical data and attributes → hard link / skip. Identical data, other attributes → local copy (`cf`). Otherwise the alt file is the delta basis and what the new file is itemized against. |
| `-L`, `-k`, `-K` | `-L` reports dangling links (`symlink has no referent`, exit 23). With `-K`, like rsync on macOS, the kept symlink gets the directory's time. |
| `--numeric-ids` | Without it, remote transfers map users and groups by name (id 0 never), as rsync does. |
| `--chmod=CHMOD` | rsync's syntax (`D`/`F`, `ugoa`, `+-=`, `rwxXst`, octal), applied to the source modes as sent. |
| `--remove-source-files` | Non-directories the receiver has (transferred or already up to date) are removed from the source, unless their size or mtime changed meanwhile. |
| `-O/--omit-dir-times` | |

Trailing-slash semantics match rsync: `src/` syncs the contents, `src` syncs the directory itself.
Exit codes follow rsync (0 ok, 1 syntax, 11 file I/O, 20 interrupted, 23 partial transfer).

Still missing: `-X/--xattrs`, `-A/--acls` and daemon mode (`rsync://`, `host::module`), as
well as `--delete-before/-after/-delay`, `--no-OPTION`, `--copy-dest`, `--fuzzy`, `-T`,
`--bwlimit`, `--timeout`, `--log-file`, `--usermap/--groupmap/--chown`, `--no-implied-dirs`,
`--no-R`, and a remote (`host:`) `--files-from` list.

Known differences:

- On APFS, clones also carry extended attributes (rsync needs `-X` for those).
- With `-H`, the same names end up sharing inodes as with rsync, but the name shown after
  `=>` can differ. rsync links each group to the last name in its file list; barqsync's
  parallel walk links them to the first name it reaches.
- `--progress` shows one overall line, like `--info=progress2`. Files are copied in
  parallel, so per-file progress lines would interleave meaninglessly.
- `-R` with an absolute path: rsync shows the names with a leading `/` (`/private/...`),
  barqsync without it. The files land in the same place.
- rsync doesn't strip comment lines from a `--files-from` list it forwards to a remote
  sender, and it rejects names like `a/../b` there. barqsync handles both the same way
  in every direction.
- `--partial` keeps the contiguous received prefix of the files in flight. rsync handles one
  file at a time, so it keeps at most one partial file; barqsync can keep several.
- After a missing `--files-from` entry, rsync leaves out the "created directory" line;
  barqsync prints it.
- With `-K`, rsync on macOS also tries to chmod the symlink and fails ("Too many levels of
  symbolic links"); barqsync leaves the mode alone.

## Remote transfers

```bash
barqsync -avz src/ user@host:/backup/src/        # push
barqsync -av user@host:/data/ ./data/            # pull
barqsync -av -e "ssh -p 2222" --remote-path=/opt/bin/barqsync src/ host:dst/
```

Like rsync, the client starts `barqsync --server` on the other machine through the remote
shell (`ssh` by default, `-e` or `BARQSYNC_RSH` to change it, `--remote-path` when the binary
isn't on the remote `PATH`). barqsync has to be installed on both ends. Either side can be the
sender, and both sides run the same code:

- **Pipelined listings.** The sender scans in parallel and streams one listing per directory
  as soon as it is read. The receiver diffs each listing against its own directory as it
  arrives, so the transfer starts before the scan ends.
- **One round trip per directory, not per file.** For each listing the receiver answers with
  one request naming the files it needs. For files it already has an older copy of, the
  request carries a basis signature: aligned blocks when the size is unchanged, FastCDC
  chunks otherwise.
- **Small files are packed.** Files up to 128 KiB travel many-per-frame in ~4 MiB packs and
  are written by the receiver's thread pool.
- **Deltas over the wire.** Changed files come back as copy ranges plus only the missing
  bytes. The receiver clones its old file and patches it where it can. Delta is the default
  remotely, as in rsync; `-W` sends whole files.
- **Compression.** `-z` zstd-compresses each frame on the thread that produced it, so
  compression runs in parallel.
- **No pipe deadlocks.** Each side has a dedicated reader and writer thread.
- **Output.** Errors and `-v` lines produced on the server are forwarded to the client and
  escaped there for the client's terminal.

### Parallel data streams (`--streams N`)

A single ssh channel often can't fill a fast or long-distance link: one TCP flow, one
encryption thread, ssh's own window. With `--streams N`, file data moves to N extra TLS 1.3
connections and control messages stay on ssh:

1. **Offer.** Over the already authenticated ssh connection, the server listens on the address
   the client reached it at (`SSH_CONNECTION`). It sends that address, the BLAKE3 fingerprint
   of a freshly generated self-signed certificate, and a random 32-byte token.
2. **Connect.** The client opens N TLS connections that accept only that exact certificate
   and presents the token on each, inside TLS. Nothing on these connections is trusted
   without both checks.
3. **Use.** Every frame of a file goes over the same stream, so it stays in order. Small-file
   packs spread across streams. Each stream ends with an end-of-stream marker, and the
   receiver finishes only after all of them and the sender's final ssh message.
4. **Fall back.** If no stream can be opened within 5 s (a firewall, NAT, a non-routable
   server address), both sides carry on over the ssh pipe; `-v` says so.

`-vv` reports how many streams are in use. Encryption and `-z` compression then run on one
thread per stream instead of in a single ssh process. `--streams N` on its own means
`--transport tcp`.

### QUIC (`--transport quic`)

The original plan's transport. Setup works like `--streams`: over the authenticated ssh
connection the server opens a UDP endpoint on the address the client reached it at, with a
fresh self-signed certificate (TLS 1.3, ALPN `barqsync`), and sends the address, the
certificate's BLAKE3 fingerprint and a random token. The client connects within 3 s,
accepting only that certificate, and presents the token on the first stream; the server
acknowledges it. Then:

- **Everything moves to QUIC.** Control messages and directory listings use that first
  (bidirectional) stream; file data uses `--streams` unidirectional streams (default 16, up
  to 128), routed by file id exactly as over TCP. The ssh connection stays open, idle, until
  the end.
- **Fallback.** If UDP doesn't get through in 3 s, both sides carry on over the ssh pipe
  (`-v` says so). A peer that disappears is noticed within 15 s (QUIC idle timeout with
  keep-alives), where ssh would see the pipe close at once.
- **Clean shutdown.** Each stream is finished and waited for (bounded) until the peer has
  read it, before the connection closes, so nothing in flight is lost when a process exits.

quinn drives a connection from one task, which limits QUIC to about 350 MB/s per transfer on
this Mac over loopback; TLS-over-TCP streams scale with the number of streams.

### Wire format (`--wire postcard|rkyv`)

Control messages (listings, requests, signatures) are postcard-encoded by default.
`--wire rkyv` uses the original plan's rkyv instead: fixed-layout archives, checked with
bytecheck before use, with non-UTF-8 names carried as raw bytes. Each frame says which
encoding it holds, so the session agrees on one in `Hello` (always postcard, so any version
can read it). File data frames are raw either way. rkyv measured no faster (see
[Transports](#transports-and-wire-formats-measured)): the messages are decoded into owned
structures anyway, so its zero-copy reads don't come into play.

### Connection layer

The plan had tokio own all network I/O. QUIC runs on a small tokio runtime (2 worker
threads); the protocol's reader and writer threads reach it through blocking bridges. For the
ssh pipe and TCP streams a tokio layer is built as well (reader and writer tasks, the ssh
pipe through non-blocking descriptors, TLS through tokio-rustls), behind the hidden
`--net-io=tokio`. The thread-based layer stays the default because it measured at least as
fast.

### Transports and wire formats, measured

Through the local fake-ssh shim (no link: CPU and protocol overhead only; macOS, 3 runs,
`bench/transport_modes.sh` and `bench/interleave.py`):

| Mode | A: fresh push, 200k files (interleaved) | C: 3 changed of 500k files |
| --- | ---: | ---: |
| ssh pipe, postcard | 34.8 s | 4.33 s |
| ssh pipe, rkyv | 34.9 s | 4.34 s |
| `--transport tcp --streams 4` | 35.0 s | 4.30 s |
| `--transport quic` (16 streams) | 34.5 s | 4.53 s |
| `--transport quic --wire rkyv` | 34.8 s | 4.66 s |

On one machine the fresh push is disk-bound whatever carries it, and the metadata-bound no-op
is a few percent slower over QUIC (userspace UDP and crypto on top of the same work). What a
transport is for shows on a link. Over real sshd shaped with `tc netem` in the Linux
container (`bench/net/net_bench.sh`, datasets at `SCALE=0.5`, 3 runs; lan = 1 Gbit/0.2 ms,
wan = 100 Mbit/20 ms, lossy = 20 Mbit/80 ms with ~1% loss):

| Profile, workload | rsync | ssh pipe | ssh, tokio I/O | `--transport tcp` (4) | `--transport quic` | quic, `--wire rkyv` |
| --- | ---: | ---: | ---: | ---: | ---: | ---: |
| lan, 4,000 small files | 0.35 s | **0.31 s** | 0.31 s | 0.32 s | 0.33 s | 0.32 s |
| lan, 3 changed of 20,000 | 0.26 s | **0.18 s** | 0.18 s | 0.19 s | 0.19 s | 0.20 s |
| wan, 4,000 small files | 2.24 s | **2.07 s** | 2.07 s | 2.39 s | 2.08 s | 2.12 s |
| wan, 128 MiB delta | 1.65 s | 1.56 s | 1.59 s | 1.75 s | **1.55 s** | 1.54 s |
| wan, 3 changed of 20,000 | 1.21 s | 0.67 s | **0.64 s** | 0.78 s | 0.71 s | 0.84 s |
| lossy, 4,000 small files | 38.6 s | 38.2 s | 39.5 s | 13.0 s | **9.1 s** | 11.8 s |
| lossy, 128 MiB delta | 18.6 s | 21.1 s | 20.3 s | 15.4 s | **9.3 s** | 13.1 s |
| lossy, 3 changed of 20,000 | 4.0 s | **2.1 s** | 2.2 s | 2.8 s | 2.3 s | 5.3 s (±3.5) |

QUIC matches the ssh pipe on a clean WAN and is a few percent slower on a LAN, but on a lossy,
long link it is 2–4× faster than the ssh pipe and faster than TCP streams too: one connection
recovers from loss per stream instead of stalling every byte behind a lost packet. It costs a
UDP port on the server, and up to 3 s per run where UDP is blocked. So the ssh pipe stays the
default, and `--transport quic` is the choice for lossy or long-distance links. rkyv and the
tokio I/O layer are within noise of postcard and threads, which stay the defaults.

(The first network run had QUIC 1.6–2× slower than the ssh pipe on LAN and WAN: the control
stream queued behind 256 MiB of buffered file data, and closing waited out QUIC's closing
period. Prioritizing the control stream, right-sizing the windows and a FIN-for-FIN close
fixed both; `bench/results/net-20261017-095932.md` is that first run.)

### Adaptive strategy (`--strategy adaptive`)

The original plan's §4: measure the link once, then choose per file.

1. **Probe.** Before the first listing the sender pings the receiver for the round-trip
   time, then sends 256 KiB of incompressible data over the path file data will take (spread
   over every data stream), timed until the receiver acknowledges it. Only if that looks like
   500 Mbit/s or more does a 4 MiB burst follow for an accurate figure, so a slow link pays
   for a quarter megabyte and two round trips. Probe frames say which burst they belong to,
   so they may arrive in any order. `-vv` prints the result.
2. **Routing** (the receiver, per file): up to 128 KiB packed, never as a delta; 128 KiB to
   32 MiB sent whole on links of 1 Gbit/s or more (hashing both copies costs more than
   sending the file) and as a delta below that; larger files always as deltas. `-W` and
   `--no-whole-file` override it and skip the probe.
3. **Compression** (the sender): small-file packs are zstd-compressed even without `-z`, at
   level 1 from 1 Gbit/s, 3 from 100 Mbit/s and 6 below, stepped down whenever compressing
   (on all cores) would fall behind the link. Incompressible packs go raw, as before. `-z`
   compresses all data at that level; `--compress-level N` fixes the level.

Measured in the same network run (half-scale datasets; `text` = 2,000 compressible text
files of 8–32 KiB, `mid` = 20 files of 4 MiB with 1 KiB changed):

| Profile, workload | `--strategy fixed` (ssh) | `--strategy adaptive` (ssh) | fixed, quic | adaptive, quic |
| --- | ---: | ---: | ---: | ---: |
| lan, small files | **0.31 s** | 0.36 s | 0.33 s | 0.37 s |
| lan, text | 0.52 s | 0.37 s | 0.54 s | **0.36 s** |
| lan, mid | **0.26 s** | 0.29 s | 0.26 s | 0.30 s |
| wan, small files | **2.07 s** | 2.16 s | 2.08 s | 2.22 s |
| wan, text | 4.03 s | **1.99 s** | 4.12 s | 1.99 s |
| wan, 3 changed of 20,000 | **0.67 s** | 0.74 s | 0.71 s | 0.79 s |
| lossy, text | 99.2 s | 28.7 s | 48.6 s | **13.1 s** |
| lossy, 128 MiB delta | 21.1 s | 13.2 s | 9.3 s | **7.8 s** |

Compressing packs by default pays off on compressible data: 1.4× on the LAN, 2× on the WAN,
7.6× on the lossy link together with QUIC. On incompressible data the strategy costs 4–22%:
the probe (two round trips plus 4 MiB on a fast link, ~40 ms on the LAN) and compressing packs
that then go out raw. Mid-size files gain nothing on these links (they stay deltas below
1 Gbit/s). Since neither strategy is faster across the board, `--strategy fixed` stays the
default; `--strategy adaptive` (with `--transport quic` on lossy links) is the choice for
compressible data over slower links.

### io_uring (`--io-uring`, Linux)

The original plan's Phase 4.1. A receiver writes each small-file pack in three io_uring
submissions per pack (up to 256 operations each) instead of three syscalls per file: create
every temp file (`openat`), write every file, close every file. Owner, mode, times and the
rename stay ordinary syscalls. Each worker thread keeps its own ring; if the kernel or a
seccomp profile refuses io_uring, the normal path is used. The plan named tokio-uring; this
uses the `io-uring` crate it is built on, since batching is the point and tokio-uring
submits per future.

Remote push (fake ssh) of 20,000 files of 4 KiB in the Linux container (kernel 6.19, 5 runs,
`bench/uring.sh`):

| Receiver writes onto | normal | `--io-uring` | syscalls normal / io_uring |
| --- | ---: | ---: | ---: |
| tmpfs | 148 ms (±15) | 133 ms (±16) | 249,705 / 190,295 |
| XFS (the VM's disk) | **218 ms** (±28) | 265 ms (±41) | 249,529 / 190,377 |

A quarter fewer syscalls, but no measurable time saved: the filesystem work per file is the
same, and a syscall costs well under a microsecond. So `--io-uring` stays opt-in.

The first measurement showed `--io-uring` 2.8× faster on XFS, and not because of io_uring:
the normal path truncated every received file to its size even when it had just written
exactly that much, and XFS writes back a truncated file when it is closed (its protection
against the truncate-then-write pattern). Skipping the redundant truncate made the normal
path 3.2× faster on XFS (689 → 218 ms), for every transfer, with or without io_uring.

## Delta transfer

With `--no-whole-file` (and, from M4, by default for remote syncs), a changed file is rebuilt
from the old destination file plus only the ranges that differ:

1. **Aligned blocks first.** Both files are hashed in parallel in 64 KiB blocks at the same
   offsets. This is ideal for in-place changes and appends (VM images, databases, logs).
2. **FastCDC fallback.** If under half the bytes matched, both files are re-chunked by
   content (16/32/64 KiB chunks), so inserted or deleted bytes only cost the chunks around
   them. Chunking runs in parallel over 4 MiB segments, joined so the cuts are exactly those of
   one sequential pass (see [`--delta=cdc`](#--deltacdc-the-original-plans-design)).
3. **Clone + patch.** When matched ranges keep their offsets, the old file is cloned (APFS
   `fclonefileat`, Linux `FICLONE`) and only the changed ranges are written. Otherwise the
   new file is rebuilt in parallel. With `--inplace`, changes go straight into the destination.

Blocks and chunks are matched by BLAKE3 (128-bit), so every reused range is cryptographically
verified. That replaces the originally planned xxh3 + whole-file check, because a whole-file
check would have to re-read the entire output and cancel out clone+patch. BLAKE3 per block runs
at ~12.6 GB/s on all cores of an M-series Mac, faster than the SSD.

| Change to a 6 MiB file | bytes written |
| --- | ---: |
| 4 KiB overwritten in place | 64 KiB |
| 300 KB appended | 300 KB |
| 777 bytes inserted at the start | 42 KB |
| 12 KB removed from the middle | 274 KB |
| truncated to 4 MiB | 17 B |

After an insert or delete, the chunk boundaries of the two files fall back into step within a
few chunks; in the middle-cut case above it took eight. Files of 128 KiB or less are always
sent whole.

### `--delta=cdc`: the original plan's design

`--delta=cdc` builds the delta engine the way the original plan describes it, for comparison:

- **FastCDC for every delta** (16/32/64 KiB chunks), no aligned-block pass. Files are chunked
  in parallel in 16 MiB regions, then stitched: the sequential chain continues from the last
  cut before each region until it lands on one of the region's own cuts. FastCDC restarts its
  hash at every cut, so from there both chains agree, and the result is exactly one sequential
  FastCDC pass over the whole file, whatever the thread count. A property test checks this at
  1, 2 and 8 threads.
- **`ChunkSignature { offset, length, xxh3_hash, blake3_hash }`.** An xxh3 hash table finds
  candidates; a match only counts when BLAKE3 (256-bit) agrees too. The sender computes BLAKE3
  only for chunks whose xxh3 is in the receiver's signature.
- **Files read through `mmap`** (memmap2) for hashing and for copy/literal ranges. The size is
  checked after mapping, and a file that changed since it was listed is sent whole. A file
  truncated *while* mapped can still raise `SIGBUS`.

Clone + patch, the in-order rebuild and `--inplace` work the same way for both designs.

Measured on the same Mac (warm cache, 3 interleaved runs, `bench/delta_modes.sh`):

| Dataset | Where | `--delta=blocks` | `--delta=cdc` |
| --- | --- | ---: | ---: |
| B: 10 GiB, 50 × 1 MiB overwritten in place | local, clone + patch | **5.9–6.3 s** | 42.5–47.3 s |
| B | local, `--no-clone` (rebuild) | **16.0–16.8 s** | 77.4–84.2 s |
| B | remote (fake ssh) | **6.0–6.1 s** | 37.9–43.3 s |
| E: 2 GiB, 40 inserts/deletes of 1–64 KiB | local, clone + patch | **2.8–3.5 s** | 3.2–3.5 s |
| E | local, `--no-clone` (rebuild) | 3.1–3.5 s | **2.2–2.5 s** |
| E | remote (fake ssh) | **2.6–2.8 s** | 3.6–4.6 s |

One idea from the plan did measure better and is now part of the default: exact stitching of
the parallel FastCDC regions. `blocks` used to force a cut at every 4 MiB segment edge, so
once content shifted, the chunks around every edge stopped matching: on E it sent 86.5 MB of
literal data. Its fallback now joins segments like `cdc` does (one read pass, `cdc_max`-byte
windows at the joins) and sends the same 4.1 MB as `cdc`, while getting a little faster
(remote E 2.8 → 2.7 s). On a 100 Mbit link those 82 MB are about 6.5 s.

Hashing itself is not what loses: on a cached 2 GiB file the cdc signature takes 230 ms against
300 ms for the blocks-mode chunk signature. The difference is how the data is read. Every first
touch of a mapped 16 KiB page is a page fault (about 1 s of CPU per 2 GiB on macOS), and mapped
pages go through the page cache. Two 10 GiB files are more than this Mac's 16 GB of RAM, so the
mapped reads evict and fault their way through, while `pread` with `F_NOCACHE` streams.
On Linux (the aarch64 container, VM disk, files scaled to 2.5 GiB for B and 512 MiB for E, so
B's two files still exceed the VM's 3.7 GiB of RAM) the picture is the same for big files and
better for `cdc` on small shifted ones, because mapped pages fault in cheaply there:

| Dataset | Where | `--delta=blocks` | `--delta=cdc` |
| --- | --- | ---: | ---: |
| B (2.5 GiB) | local, copy + patch | **2.0–2.5 s** | 4.8–6.2 s |
| B | local, `--no-clone` | **4.4–5.3 s** | 6.5–6.8 s |
| B | remote (fake ssh) | **2.1–2.8 s** | 2.7–3.0 s |
| E (512 MiB) | local, copy + patch | 0.7–1.3 s | **0.5–0.6 s** |
| E | local, `--no-clone` | 0.7–0.9 s | **0.37–0.43 s** |
| E | remote (fake ssh) | 0.7–0.8 s | 0.7–0.9 s |

`cdc` wins where a shifted file fits in RAM, most clearly when it is rebuilt without clones. There it hashes
each file once, while `blocks` first tries aligned blocks and only then falls back to chunks.
Those wins are under a second; the losses on big files are tens of seconds.
Following the defaults rule (the faster design stays the default), `--delta=blocks` remains the
default and `--delta=cdc` stays selectable.

## Scanner: `--scanner=dirtask|jwalk`

`--scanner=jwalk` runs the original plan's four-stage pipeline instead of one task per
directory:

1. **Scan.** `jwalk` walks the source tree on its own rayon pool. A `process_read_dir` hook
   stats each directory's children through a directory handle (`statx` on Linux, `fstatat` on
   macOS; directories of 512+ entries on 8 threads), prunes excluded subdirectories and `-x`
   mount points, and hands the listing on as soon as the directory is read. The destination
   tree is scanned at the same time into a `DashMap` index of directory → entries.
2. **Diff.** A listing is processed once its directory exists on the destination side (its
   parent creates it first), against the destination entries from the index, so there is no
   per-file `lstat`. Directories, symlinks and specials are handled inline in scan order;
   `--delete` runs per directory. Files are classified into queues: A metadata-only, B small
   (≤ 128 KiB), C large and new, D large and changed.
3. **Transfer.** A pool of worker threads per queue (2 / threads / threads÷2 / threads÷4).
4. **Commit.** Temp file + rename per file, as before; hard links and directory attributes
   once everything is written.

A directory the scan never lists (reached through a symlink with `-L`/`-k`, kept by `-K`, or
unreadable) is read directly once the scan is over. As a remote receiver, the destination is
indexed the same way while the sender's listings arrive. `-R`/`--files-from` and non-recursive
transfers always use `dirtask`. The macOS `getattrlistbulk` path has no hook in jwalk, so the
jwalk scanner doesn't use it.

Measured with `bench/scanner_modes.sh` (5 runs; macOS: APFS, full datasets; Linux: the
aarch64 container on tmpfs, datasets at `--scale 0.25`):

| Workload | macOS dirtask | macOS jwalk | Linux dirtask | Linux jwalk |
| --- | ---: | ---: | ---: | ---: |
| A: fresh copy (200k / 50k files), local | **18.1 s** | 19.7 s | **113 ms** | 184 ms |
| C: 3 changed of 500k / 125k files, local | **2.93 s** | 3.26 s | **42 ms** | 87 ms |
| C with `--delete`, local | **2.97 s** | 3.29 s | **42 ms** | 75 ms |
| A, remote (fake ssh) | 19.4 s | 18.5 s (±1.8) | 305 ms | 301 ms |
| C, remote (fake ssh, 10 runs on macOS) | 3.10 s | **2.88 s** | **80 ms** | 91 ms |

`dirtask` wins locally on both systems: it reads each directory's entries and attributes in
one pass (`getattrlistbulk` on macOS), stats in parallel within big directories, and starts
copying a directory's files as soon as it is listed. jwalk's pool, the index and the queues
add hand-offs without saving any syscalls. jwalk only won the macOS remote no-op (7%), where
the receiver indexes the destination while the sender's listings are still arriving. So
`dirtask` stays the default and `--scanner=jwalk` stays selectable. jwalk did use half the
memory on the fresh copy (92 vs 186 MiB), because its queues are bounded.


## Layout

```
src/cli.rs          flags → Options (filter order preserved across --include/--exclude)
src/chmod.rs        --chmod parser and mode tweaks
src/select.rs       -R / --files-from path selection (implied directories)
src/checksum.rs     -c whole-file BLAKE3 on a dedicated pool
src/ids.rs          user/group name mapping for remote transfers (--numeric-ids off)
src/paths.rs        host:path detection, trailing-slash rules
src/filter.rs       rsync wildmatch + include/exclude rules
src/diff.rs         quick check (size + mtime, --modify-window, -u, --size-only, ...)
src/fsops/          readdir + fstatat, temp names, clone/copy, utimensat, mknod, page-cache hints
src/local/          planner (SRC... DEST → roots) and the parallel per-directory engine;
                    flags.rs: link/compare-dest, backups, partial, sparse, -L/-k, removals
                    pipeline.rs: --scanner=jwalk feed, transfer queues A-D
src/scan.rs         --scanner=jwalk tree scan (jwalk hook) and the destination index
src/delta/          block/CDC signatures, diff, parallel apply (clone+patch or rebuild);
                    cdc.rs: --delta=cdc (stitched parallel FastCDC, xxh3 + BLAKE3, mmap)
src/proto/          wire frames (postcard or rkyv + raw data, optional zstd), reader/writer
                    threads; async_io.rs: the tokio runtime and the tokio connection layer
src/remote/         client (spawns the server over ssh), server, sender and receiver roles;
                    streams.rs: --transport tcp, quic.rs: --transport quic
src/output.rs       rsync-style item lines and stats
tests/local_sync.rs end-to-end tests, most of them differential against rsync 3.x
tests/delta_sync.rs --no-whole-file end to end (clone+patch, --inplace, rebuild) vs rsync
tests/remote_sync.rs push/pull through a fake ssh, differential against rsync over the same shim
tests/rsync_flags.rs the options above, locally and over the fake ssh (and one that cuts it)
bench/              dataset generator + hyperfine runner
```

## Testing

```bash
cargo test
```

The delta engine also has property tests (random insert/delete/overwrite/append/truncate
sequences; every delta must rebuild the new file exactly via both rebuild and clone+patch).
`PROPTEST_CASES=5000 cargo test --release delta` runs a longer sweep.
`BARQSYNC_DELTA=cdc cargo test` runs the whole suite with `--delta=cdc` as the default;
`BARQSYNC_SCANNER=jwalk`, `BARQSYNC_TRANSPORT=quic`, `BARQSYNC_WIRE=rkyv` and
`BARQSYNC_NET_IO=tokio` do the same for the other alternatives.

The integration tests run each scenario through the reference rsync 3.x as well (found at
`/opt/homebrew/bin/rsync`, `/usr/local/bin/rsync` or `/usr/bin/rsync`, or set
`RSYNC=/path`) and assert that both the printed items and the resulting trees are identical:
content, mode, mtime and symlink targets. When no rsync 3.x is installed they are skipped.
macOS's `/usr/bin/rsync` is openrsync and is ignored.

## Benchmarks

```bash
cargo build --release
python3 bench/gen.py /tmp/fr-bench A C     # A: 200k x 4 KiB, C: 500k x 1 KiB
bench/run.sh /tmp/fr-bench all             # warm cache
bench/run.sh /tmp/fr-bench all --cold      # drops the page cache between runs (sudo)
```

`--no-clone` (hidden flag) forces buffered copies, which shows the engine's speed without APFS
clones, e.g. for copies to another volume.

### Verification harness (original plan §8)

```bash
bench/net/container.sh build              # Linux image: sshd, tc, strace, perf, inferno
bench/net/container.sh net lan wan lossy  # rsync vs barqsync over real ssh, tc-shaped links
bench/net/container.sh syscalls           # strace -c syscall counts
bench/net/container.sh flamegraph copy    # perf flamegraph (copy | noop | delta)
bench/net/container.sh test               # cargo test + clippy on Linux
bench/flamegraph.sh copy                  # macOS flamegraph via `sample` (cargo install inferno)
bench/remote_setup.sh user@host           # install barqsync on a real second machine
BARQSYNC_TEST_REMOTE=user@host cargo test --test real_remote   # tests over real ssh
```

- **What the container does:** it shapes its own loopback interface with `tc netem`, so ssh,
  the `--streams` TCP connections (and later QUIC) all cross the same emulated link. The
  container runs privileged (needed for `tc`, `perf` and `drop_caches`) and touches nothing
  outside itself.
- **Link profiles:**
  - `lan`: 1 Gbit, ~0.2 ms RTT
  - `wan`: 100 Mbit, ~20 ms RTT
  - `lossy`: 20 Mbit, ~80 ms RTT, ~1% loss
- **Benchmark A** also times `cp -a` and the platform's fastest stock copy (`cp -ac` clones
  on macOS, `--reflink=auto` on Linux).
- **Not covered:** NFS mounts (they need an NFS server).

### Results (M-series Mac, 10 cores, APFS, warm cache, rsync 3.5.1)

Final measurement (M14, default options, 3 runs each, `bench/run.sh DIR all`; the M6 numbers
for comparison):

| Workload | rsync | barqsync | speedup | at M6 |
| --- | ---: | ---: | ---: | ---: |
| A: fresh copy, 200k × 4 KiB files, `-a` | 42.4 s | 20.5 s | 2.1× | 2.4× |
| A with `--inplace` (both tools) | 30.0 s | 11.7 s | 2.6× | 2.6× |
| A, `cp -a` / `cp -ac` (APFS clones) | 76.0 s / 25.9 s | 20.5 s | 3.7× / 1.3× | — |
| C: 500k files, 3 changed, `-a` | 18.2 s (±6.1) | 3.49 s | 5.2× | 3.6× |
| B: 10 GiB, 50 × 1 MiB changed, `--no-whole-file` (clone + patch) | 24.3 s (±6.6) | 5.9 s | 4.1× | ~3× |
| B without clones (full rebuild, e.g. ext4/exFAT) | 24.3 s (±6.6) | 25.7 s | 0.9× | 1.0–1.2× |
| B, `-a` (whole file), same APFS volume | 16.1 s | 0.014 s (clone) | — | — |

rsync's no-op and delta times varied more in this run (C 12.2 s at M6, ±6 s here), so the
speedups are best read as ranges. A fresh copy is 0.2–2 s slower than at M6 run to run (the
SSD's state matters more than any code change in between); barqsync still beats a clone
copy (`cp -ac`) of the same tree.

Benchmark D: remote push through a local fake-ssh shim (`bench/run.sh DIR D`). That measures
the protocol and parallelism over a pipe, not a network; link profiles are in
[Transports and wire formats](#transports-and-wire-formats-measured).

| D: remote push (pipe) | rsync | barqsync | speedup | at M6 |
| --- | ---: | ---: | ---: | ---: |
| fresh copy, 200k × 4 KiB files | 42.4 s | 19.3 s | 2.2× | 2.7× |
| fresh copy, `--streams 4` (TLS over loopback) | 42.4 s | 16.4 s | 2.6× | 2.6× |
| 500k files, 3 changed | 12.9 s | 3.26 s | 4.0× | 4.9× |

Benchmark B resets the destination to the old version before every run. On the same volume a
changed big file is simply cloned (last B row); when the destination can't be cloned, writing a
whole new 10 GiB file is disk-bound for both tools, so the delta engine's win comes from
clone + patch (or `--inplace`), not from hashing.

### Page-cache hints

Files of 8 MiB or more are read and written with page-cache hints, so a big sync doesn't
push other cached files and the memory of running applications out of RAM:

- **macOS:** `F_NOCACHE` on both files. Pages that are already cached are still used, but no
  new ones are cached.
- **Linux:** `POSIX_FADV_SEQUENTIAL` while reading, and `POSIX_FADV_DONTNEED` on each range
  once it's read. Writes start writeback (`sync_file_range`) every 8 MiB, wait for the window
  before that one, then drop it, so only about two windows per file stay in the cache.

The hints are on by default; `--no-drop-cache` turns them off. Smaller files are never hinted.

Measured on benchmark B. Before every run, the B files were evicted from the page cache
(`msync(MS_INVALIDATE)` on a mapping, no root needed) and a 1 GiB "canary" file was read in,
standing in for an application's hot data. `mincore` then showed what was still cached.
Runs alternated hints on and off, swapping the order every round. The last two rounds left
30 s between runs for the SSD's write cache to drain. The machine was under memory pressure
(16 GB, ~4.4 GB of swap in use, a 7.5 GB Podman VM), as a busy laptop is.

| B, 10 GiB, 50 × 1 MiB changed | hints on | hints off | canary still cached (on / off) |
| --- | ---: | ---: | --- |
| `--no-whole-file` (hash both, clone + patch) | 6.29–6.54 s, median 6.3 | 6.69–7.10 s, median 6.8 | 100% in 4 of 4 / 0% |
| `--no-whole-file --no-clone` (hash both, rebuild) | 14.7–16.3 s, median 15.8 (one 26.7 s outlier) | 17.1–20.7 s, median 18.8 | 0–100%, median 73% / 0% |
| `--no-clone` (whole-file buffered copy) | 8.6–11.1 s, median 10.0 (one 18.8 s outlier) | 8.8–12.0 s, median 10.7 | 6–100%, median 32% / 0% |

- **The canary is the clearest signal.** Without hints it was evicted completely in all 16
  runs, and up to 2.8 GiB of the source stayed cached afterwards. With hints, the B files
  were never cached. The canary still lost some pages in some runs, because other programs
  were also competing for memory: another session's benchmark was running in the Podman VM
  during the last rounds. File-backed pages (`vm_stat`) grew by up to 1.5 GiB per run without
  hints, and by at most 0.25 GiB with them.
- **No slowdown; delta runs got faster.** Hashing reads lose nothing without read-ahead,
  because 10 threads read 4 MiB segments in parallel. The rebuild is probably faster because
  10 GiB of written data never turns into cached pages the system then has to reclaim. The
  two outliers with hints both ran straight after a long burst of big writes; with a 30 s
  pause between runs, the same cases took 9.7 s and 14.7 s.
- **Uncached copies must overlap reads and writes.** The first version kept the 1 MiB
  read-then-write loop and took 14.9 s instead of 10.5 s, because uncached I/O on macOS is
  synchronous (no read-ahead, no write-behind). Big files are now copied through two 8 MiB
  buffers, reading the next one while the current one is written.
- **The remote receiver must write in big pieces too.** Pushing B through the fake-ssh
  pipe (`-W`, whole file) took 15.0 / 14.4 s with hints and 11.3 / 11.1 s without. The
  receiver writes `Data` frames on its single receive loop, and each uncached 1 MiB write
  stalled that loop. On macOS it now collects a big file's contiguous frames into 8 MiB
  writes: 12.3 / 10.8 s with hints, 11.8 / 10.3 s without. A remote delta push of B
  was faster with hints from the start: 6.34 / 6.36 s, against 6.70 / 7.09 s without.
- **Costs:** a big file synced twice in a row is re-read from disk the second time. On
  Linux, `DONTNEED` also drops source pages that were cached before the sync. macOS
  `F_NOCACHE` keeps those.

On Linux this was checked for effect only. The timings of a disk inside a VM say little,
so none were taken. The check ran in the Podman VM (kernel 6.19, aarch64) with a 512 MiB
file whose cache was dropped before each run; `fincore` then measured each file:

| Linux, XFS volume, 512 MiB | cached afterwards, hints on (src / dst) | hints off |
| --- | ---: | ---: |
| `--no-clone` (buffered copy) | 0 / 8 MiB | 474 / 482 MiB |
| `--no-whole-file --no-clone` (hash both, rebuild) | 0 / 16 MiB | 490 / 500 MiB |
| default (`copy_file_range`, reflinked on XFS) | 0 / 0 | 0 / 0 |

On overlayfs (a container's root filesystem), only reads are dropped. `sync_file_range` on an
overlay file doesn't reach the underlying file, so written pages stay dirty and can't be
dropped. The delta hashing reads use 4 MiB segments on many threads, and one thread's
read-ahead could pull in pages another thread had already dropped (96 MiB of 512). The file
is therefore dropped once more as a whole when hashing and rebuilding are done.

Peak memory is higher than rsync's (~130 MiB vs 9 MiB on A locally, ~230 MiB for a remote push) because whole directory listings
and per-thread 1 MiB copy buffers are held at once; bounding that is on the list.

### Over a network (Linux container, real sshd, `tc netem` on loopback, data on tmpfs, 3 runs)

| Profile | Workload | rsync | barqsync (ssh) | barqsync `--streams 4` |
|---|---|---:|---:|---:|
| LAN 1 Gbit | 8k × 4 KiB push | 0.49 s | **0.46 s** | 0.47 s |
| LAN | 256 MiB, 8 × 1 MiB changed | 0.49 s | **0.34 s** | 0.35 s |
| LAN | 40k files, 3 changed | 0.32 s | **0.19 s** | 0.20 s |
| WAN 100 Mbit, 20 ms | small push | 3.59 s | **3.40 s** | 3.88 s |
| WAN | delta | 1.64 s | **1.57 s** | 1.78 s |
| WAN | no-op | 1.82 s | **0.69 s** | 0.84 s |
| Lossy 20 Mbit, 80 ms, 1% | small push | 79.2 s | 79.5 s | **24.4 s** |
| Lossy | delta | 18.2 s | **17.0 s** | 22.2 s (±6.3) |
| Lossy | no-op | 5.80 s | 3.73 s | **3.05 s** |

What this shows:

- **When a transfer is limited by the link or by ssh, the tools tie.** Both push data through
  ssh's single encryption thread (~130 MB/s in this VM).
- **barqsync wins on no-ops** (1.7–2.6×), where rsync's single-threaded scan dominates.
- **`--streams` wins big on lossy links** (3.3×). One TCP flow can't fill a lossy,
  high-latency link; several can. On clean links its extra connection setup costs a little.
- **One big file gains nothing from `--streams`:** all frames of a file use one stream.

Syscalls (`strace -c -f`, Linux, 20k files; the original plan's bar is ≥ 50% fewer than rsync):

| Workload | rsync | barqsync |
|---|---:|---:|
| Copy 20k × 4 KiB | 404,488 | **182,155** (−55%) |
| No-op over 20k files | 42,684 | 44,410 (+4%) |

- **No-op:** both tools need two stats per file (source and destination), and Linux has no
  bulk stat, so the no-op count can't drop much. barqsync's no-op speed comes from
  parallelism.
- **Copy:** getting under the bar took two fixes found in these traces. An extra
  `copy_file_range` call per file only to see EOF is now skipped. A `fchmod` after every
  `chown` is now done only when setuid/setgid bits are involved.

Flamegraphs (`bench/flamegraph.sh`; Linux via perf, macOS via `sample`) show the time going into
`openat`/`copy_file_range` (copy), `pwrite` (delta) and `statx`/`getdents` (no-op). No lock
contention or allocation shows up in the hot frames.

### What the measurements showed

- **Small-file copies scale to ~4 threads, then APFS metadata locking caps them.** Fresh copy of
  A: 1 thread 40 s (≈ rsync), 2 threads 23 s, 4 threads 16 s, 20 threads 17 s.
- **Clones lose below ~512 KiB.** `fclonefileat` is ~20% slower than a plain copy for 16–256 KiB
  files and 2–15× faster from 1 MiB up, so files under 512 KiB are copied, larger ones cloned.
- **The temp-file rename is ~1/3 of a small file's cost.** `--inplace` writes new files at their
  final name (rsync does the same with `--inplace`). The default stays rsync's atomic
  temp+rename. Either way the mtime is set last, so an interrupted copy never passes the
  quick check on the next run.
- **Rebuilding big files must write in order.** Scattered parallel 4 MiB writes into a sparse
  10 GiB file fell to 45–85 s once the SSD's write cache filled. Reading in parallel and writing
  sequentially keeps it at rsync's speed or better.
- **Remote transfers need backpressure in both directions.** Without a cap on queued
  small-file packs, the receiver buffered 834 MiB. Blocking sends inside nested parallel loops
  also overflowed a 2 MiB worker stack on 5000 directories. Now packs are bounded, the
  sender's serve path has no nested parallel loops, and workers get 32 MiB stacks.
- **No-op syncs are bound by APFS metadata reads, not syscalls.**
  - With one thread, a 500k-file no-op takes 12.2 s but only 4.4 s of CPU: it waits on the
    filesystem. Parallelism hides that latency up to ~4–6 threads; beyond that, wall time
    stays near 3.5 s while kernel CPU climbs to 21 s at 20 threads.
  - `getattrlistbulk` (macOS) returns a whole directory's names and attributes in a few
    syscalls. It saves 8–12% wall time at 6–8 threads and 15–30% kernel CPU, but it can't
    lower that floor.
  - On Linux, `statx` asks only for the fields sync decisions need and never forces a
    network filesystem to resync attributes. There is no bulk equivalent there.
  - The default is 2 threads per core because that is fastest for no-op syncs. Fresh
    small-file copies are as fast at one per core and fastest at 4–6 threads, and
    `--threads 6` uses about a third of the kernel CPU.

# fastrsync

A parallel, rsync-compatible file synchronizer written in Rust.

rsync is single-threaded on both the sending and receiving side, and its receiver makes one
blocking open/write/rename/utimes sequence per file. fastrsync keeps rsync's command line and
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

The test suite passes on macOS (APFS) and Linux (aarch64 container, compared against rsync 3.2.7).

## Build

```bash
brew install rustup && rustup default stable   # macOS; or https://rustup.rs
cargo build --release
./target/release/fastrsync -av src/ dst/
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
for files of 8 MiB+, on by default; see [Page-cache hints](#page-cache-hints)).

Trailing-slash semantics match rsync: `src/` syncs the contents, `src` syncs the directory itself.
Exit codes follow rsync (0 ok, 1 syntax, 11 file I/O, 20 interrupted, 23 partial transfer).

Known differences:

- On APFS, clones also carry extended attributes (rsync needs `-X` for those).
- With `-H`, the same names end up sharing inodes as with rsync, but the name shown after
  `=>` can differ. rsync links each group to the last name in its file list; fastrsync's
  parallel walk links them to the first name it reaches.
- `--progress` shows one overall line, like `--info=progress2`. Files are copied in
  parallel, so per-file progress lines would interleave meaninglessly.

## Remote transfers

```bash
fastrsync -avz src/ user@host:/backup/src/        # push
fastrsync -av user@host:/data/ ./data/            # pull
fastrsync -av -e "ssh -p 2222" --remote-path=/opt/bin/fastrsync src/ host:dst/
```

Like rsync, the client starts `fastrsync --server` on the other machine through the remote
shell (`ssh` by default, `-e` or `FASTRSYNC_RSH` to change it, `--remote-path` when the binary
isn't on the remote `PATH`). fastrsync has to be installed on both ends. Either side can be the
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
thread per stream instead of in a single ssh process.

## Delta transfer

With `--no-whole-file` (and, from M4, by default for remote syncs), a changed file is rebuilt
from the old destination file plus only the ranges that differ:

1. **Aligned blocks first.** Both files are hashed in parallel in 64 KiB blocks at the same
   offsets. This is ideal for in-place changes and appends (VM images, databases, logs).
2. **FastCDC fallback.** If under half the bytes matched, both files are re-chunked by
   content (16/32/64 KiB chunks inside 4 MiB segments, in parallel), so inserted or deleted
   bytes only cost the chunks around them.
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
| 777 bytes inserted at the start | 83 KB |
| 12 KB removed from the middle | 109 KB |
| truncated to 4 MiB | 17 B |

## Layout

```
src/cli.rs          flags → Options (filter order preserved across --include/--exclude)
src/paths.rs        host:path detection, trailing-slash rules
src/filter.rs       rsync wildmatch + include/exclude rules
src/diff.rs         quick check (size + mtime, --modify-window, -u, --size-only, ...)
src/fsops/          readdir + fstatat, temp names, clone/copy, utimensat, mknod, page-cache hints
src/local/          planner (SRC... DEST → roots) and the parallel per-directory engine
src/delta/          block/CDC signatures, diff, parallel apply (clone+patch or rebuild)
src/proto/          wire frames (postcard + raw data, optional zstd), reader/writer threads
src/remote/         client (spawns the server over ssh), server, sender and receiver roles
src/output.rs       rsync-style item lines and stats
tests/local_sync.rs end-to-end tests, most of them differential against rsync 3.x
tests/delta_sync.rs --no-whole-file end to end (clone+patch, --inplace, rebuild) vs rsync
tests/remote_sync.rs push/pull through a fake ssh, differential against rsync over the same shim
bench/              dataset generator + hyperfine runner
```

## Testing

```bash
cargo test
```

The delta engine also has property tests (random insert/delete/overwrite/append/truncate
sequences; every delta must rebuild the new file exactly via both rebuild and clone+patch).
`PROPTEST_CASES=5000 cargo test --release delta` runs a longer sweep.

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

### Results (M-series Mac, 10 cores, APFS, warm cache, rsync 3.5.1)

| Workload | rsync | fastrsync | speedup |
| --- | ---: | ---: | ---: |
| A: fresh copy, 200k × 4 KiB files, `-a` | 43.5 s | 18.3 s | 2.4× |
| A with `--inplace` (both tools) | 29.6 s | 11.6 s | 2.6× |
| C: 500k files, 3 changed, `-a` | 12.2 s | 3.4 s | 3.6× |

Benchmark D: remote push through a local fake-ssh shim (`bench/run.sh DIR D`). That measures
the protocol and parallelism over a pipe, not a network; set `REMOTE=user@host` to run the
same comparison over real ssh.

| D: remote push (pipe) | rsync | fastrsync | speedup |
| --- | ---: | ---: | ---: |
| fresh copy, 200k × 4 KiB files | 45.3 s | 16.9 s | 2.7× |
| 500k files, 3 changed | 15.2 s | 3.1 s | 4.9× |
| fresh copy, `--streams 4` (TLS over loopback) | 43.9 s | 17.2 s | 2.6× |

On one machine `--streams` neither helps nor hurts: the pipe isn't the bottleneck, APFS
metadata is. Its benefit is meant for real links where a single ssh channel is the limit
(high latency, or more bandwidth than one ssh encryption thread delivers). That hasn't been
measured yet; `REMOTE=user@host bench/run.sh DIR D` does it.

Benchmark B: a 10 GiB image with 50 scattered 1 MiB overwrites, destination reset to the old
version before every run. rsync and fastrsync runs were interleaved because big sequential
writes on this machine slow down as the SSD's write cache fills, which biases back-to-back
runs.

| B: 10 GiB, 50 × 1 MiB changed | rsync | fastrsync | speedup |
| --- | ---: | ---: | ---: |
| `-a --no-whole-file`, APFS (clone + patch 50 MiB) | 20.2 / 19.5 / 22.7 s | 6.9 / 6.8 / 6.5 s | ~3× |
| `-a --no-whole-file`, no clones (full rebuild, e.g. ext4/exFAT) | 20.2 / 19.5 / 22.7 s | 20.0 / 17.3 / 18.5 s | 1.0–1.2× |
| `-a` (whole file), same APFS volume | 15.9 s | 0.013 s (clone of the source) | — |

The last row says more about APFS than about fastrsync: on the same volume, a changed big file
is simply cloned. When the destination can't be cloned, writing a whole new 10 GiB file is disk
bound for both tools, so the delta engine's win comes from clone + patch (or `--inplace`), not
from hashing.

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

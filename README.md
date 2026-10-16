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
| M5 | parallel TCP data streams (`--streams N`) | next |
| M6 | `-H`, `--progress`, `-i`, `getattrlistbulk`/`statx` fast paths | planned |

The test suite passes on macOS (APFS) and Linux (aarch64 container, compared against rsync 3.2.7).

## Build

```bash
brew install rustup && rustup default stable   # macOS; or https://rustup.rs
cargo build --release
./target/release/fastrsync -av src/ dst/
```

With Homebrew's keg-only rustup, add `/opt/homebrew/opt/rustup/bin` to `PATH`.

## Supported options

`-a` (= `-rlptgoD`), `-r -l -p -t -g -o -D --devices --specials`, `-v -q -n`,
`--out-format` (`%n`, `%L`), `--delete`, `--delete-excluded`,
`--exclude/--include/--exclude-from/--include-from` (rsync wildcard rules, first match wins,
`/` anchoring, trailing `/` for directories, `**`, `dir/***`), `-u`, `--size-only`, `-I`,
`--ignore-existing`, `-x`, `--modify-window` (`-1` compares nanoseconds), `-W/--whole-file`,
`--no-whole-file` (delta transfer; locally off by default, like rsync), `--inplace` (also
writes new files directly, like rsync),
`--fsync`, `--stats`, `-h`, `--threads N`.

Trailing-slash semantics match rsync: `src/` syncs the contents, `src` syncs the directory itself.
Exit codes follow rsync (0 ok, 1 syntax, 11 file I/O, 20 interrupted, 23 partial transfer).

Known differences: on APFS, clones also carry extended attributes (rsync needs `-X` for those).
Hard links (`-H`) are not preserved yet.

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
src/fsops/          readdir + fstatat, temp names, clone/copy, utimensat, mknod
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
- **No-op syncs are bound by `lstat`.** They scale to ~20 threads; the next step is
  `getattrlistbulk` (macOS) and `statx` (Linux) bulk metadata reads.

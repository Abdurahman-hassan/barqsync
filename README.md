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
| M7 | common rsync flags: `-c -S -R -d -L -k -K -b -O`, `--partial(-dir)`, `--files-from`, `--existing`, `--max/min-size`, `--link/compare-dest`, `--numeric-ids`, `--chmod`, `--remove-source-files` | done |

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
`--fsync`, `--stats`, `-h`, `--threads N`.

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
  `=>` can differ. rsync links each group to the last name in its file list; fastrsync's
  parallel walk links them to the first name it reaches.
- `--progress` shows one overall line, like `--info=progress2`. Files are copied in
  parallel, so per-file progress lines would interleave meaninglessly.
- `-R` with an absolute path: rsync shows the names with a leading `/` (`/private/...`),
  fastrsync without it. The files land in the same place.
- rsync doesn't strip comment lines from a `--files-from` list it forwards to a remote
  sender, and it rejects names like `a/../b` there. fastrsync handles both the same way
  in every direction.
- `--partial` keeps the contiguous received prefix of the files in flight. rsync handles one
  file at a time, so it keeps at most one partial file; fastrsync can keep several.
- After a missing `--files-from` entry, rsync leaves out the "created directory" line;
  fastrsync prints it.
- With `-K`, rsync on macOS also tries to chmod the symlink and fails ("Too many levels of
  symbolic links"); fastrsync leaves the mode alone.

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
src/chmod.rs        --chmod parser and mode tweaks
src/select.rs       -R / --files-from path selection (implied directories)
src/checksum.rs     -c whole-file BLAKE3 on a dedicated pool
src/ids.rs          user/group name mapping for remote transfers (--numeric-ids off)
src/paths.rs        host:path detection, trailing-slash rules
src/filter.rs       rsync wildmatch + include/exclude rules
src/diff.rs         quick check (size + mtime, --modify-window, -u, --size-only, ...)
src/fsops/          readdir + fstatat, temp names, clone/copy, utimensat, mknod
src/local/          planner (SRC... DEST → roots) and the parallel per-directory engine;
                    flags.rs: link/compare-dest, backups, partial, sparse, -L/-k, removals
src/delta/          block/CDC signatures, diff, parallel apply (clone+patch or rebuild)
src/proto/          wire frames (postcard + raw data, optional zstd), reader/writer threads
src/remote/         client (spawns the server over ssh), server, sender and receiver roles
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

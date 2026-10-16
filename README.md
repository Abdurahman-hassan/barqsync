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
| M3 | delta engine: aligned blocks, FastCDC fallback, clone+patch, BLAKE3 verify | next |
| M4 | remote over SSH (`host:path`) | planned |
| M5 | parallel TCP data streams (`--streams N`) | planned |
| M6 | `-H`, `--progress`, `-i`, `getattrlistbulk`/`statx` fast paths | planned |

Remote paths (`host:path`) are rejected for now.

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
`--ignore-existing`, `-x`, `--modify-window` (`-1` compares nanoseconds), `--inplace` (also
writes new files directly, like rsync),
`--fsync`, `--stats`, `-h`, `--threads N`.

Trailing-slash semantics match rsync: `src/` syncs the contents, `src` syncs the directory itself.
Exit codes follow rsync (0 ok, 1 syntax, 11 file I/O, 20 interrupted, 23 partial transfer).

Known differences: on APFS, clones also carry extended attributes (rsync needs `-X` for those).
Hard links (`-H`) are not preserved yet.

## Layout

```
src/cli.rs          flags → Options (filter order preserved across --include/--exclude)
src/paths.rs        host:path detection, trailing-slash rules
src/filter.rs       rsync wildmatch + include/exclude rules
src/diff.rs         quick check (size + mtime, --modify-window, -u, --size-only, ...)
src/fsops/          readdir + fstatat, temp names, clone/copy, utimensat, mknod
src/local/          planner (SRC... DEST → roots) and the parallel per-directory engine
src/output.rs       rsync-style item lines and stats
tests/local_sync.rs end-to-end tests, most of them differential against rsync 3.x
bench/              dataset generator + hyperfine runner
```

## Testing

```bash
cargo test
```

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

Peak memory is higher than rsync's (~130 MiB vs 9 MiB on A) because whole directory listings
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
- **No-op syncs are bound by `lstat`.** They scale to ~20 threads; the next step is
  `getattrlistbulk` (macOS) and `statx` (Linux) bulk metadata reads.

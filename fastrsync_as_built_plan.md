# fastrsync: The Plan as Built

This document is a companion to `rust_rsync_replacement_master_plan.md`, in the same order. It
describes what was built, what changed from the original plan and why, how it performs, and
what is still open. The code is about 6,800 lines of Rust plus about 1,400 lines of tests,
built in milestones M0–M6.

---

## 1. Summary and measured results

The main lesson from building it: **rsync 3.x is not slow because of its algorithms.** It
already uses xxh128 checksums and zstd, and it pipelines its protocol. It is slow because it
is single-threaded at both ends and does one blocking create/write/rename/utime sequence per
file. fastrsync keeps rsync's command line and behavior, and gets its speed from:

- parallelism: one task per directory, files copied in parallel;
- fewer filesystem metadata operations;
- copy-on-write clones.

Every result below is against Homebrew rsync 3.5.1 on an M-series Mac (10 cores, 16 GB, APFS
SSD).

| Original target | Target | Measured | Notes |
|---|---|---|---|
| Scan / no-op sync | 5–8× | **3.3×** (500k files: 11.4 s → 3.5 s) | APFS metadata latency is the floor (see §8) |
| Small files over the network | 10–20× | **2.7×** push, **4.9×** remote no-op | Through a local pipe; real network not measured |
| 10 GB delta sync | 4–6× | **~3×** vs rsync's delta (19.5–22.7 s → 6.5–6.9 s) | APFS clone + patch; 1.0–1.2× without clones |
| Local copy at full hardware speed | line rate | big files: instant APFS clone; small files **2.4–3.8×** | Linux `copy_file_range` implemented, not benchmarked |

---

## 2. Where rsync is slow, and what fastrsync does about it

| rsync's real limit | fastrsync |
|---|---|
| One process walks the tree | One rayon task per directory; a directory's subtasks start as soon as its listing is read |
| Receiver does every file sequentially | Files written in parallel (local pool, or the remote receiver's pool) |
| Delta always rewrites the whole file | Clone the old file (APFS `fclonefileat` / Linux `FICLONE`) and write only the changed ranges |
| Hashing on one core | Block/chunk signatures computed in parallel on both files at once |
| One ssh channel | Optional `--streams N` extra TLS connections for file data |

Two premises in the original plan turned out wrong:

- **"Legacy MD5/zlib":** rsync 3.2+ negotiates xxh128 and zstd already.
- **"The rolling checksum is a flaw":** it is what makes rsync robust to inserted bytes.

---

## 3. Crates actually used

```toml
[dependencies]
clap = { version = "4.6", features = ["derive"] }   # rsync-compatible CLI
anyhow = "1.0"
ctrlc = "3.5"                                       # cooperative Ctrl-C (no half-written files)
rayon = "1.12"                                      # all parallelism
crossbeam-channel = "0.5"                           # connection reader/writer threads
libc = "0.2"                                        # clonefile, copy_file_range, FICLONE,
                                                    # getattrlistbulk, statx, utimensat
blake3 = "1.8"                                      # block/chunk matching (128-bit)
fastcdc = "5.0"                                     # content-defined chunking fallback
zstd = "0.14"                                       # -z, per-frame compression
serde = { version = "1.0", features = ["derive"] }
postcard = { version = "1.1", features = ["use-std"] }  # wire encoding
shlex = "2.0"                                       # -e "ssh -p 2222" parsing
rustls = { version = "0.23", default-features = false, features = ["ring", "std", "tls12"] }
rcgen = { version = "0.14", default-features = false, features = ["crypto", "ring"] }

[dev-dependencies]
proptest = "1.12"
tempfile = "3.27"

[profile.release]
opt-level = 3
lto = "fat"
codegen-units = 1
panic = "abort"
```

**Not used, and why:**

- **tokio / tokio-uring:** file I/O blocks anyway; threads + rayon are simpler and fast enough.
- **jwalk + DashMap:** replaced by the per-directory pipeline. Memory grows with directories,
  not files, and transfers start before the scan ends.
- **xxhash-rust:** BLAKE3 alone hashes at 12.6 GB/s on all cores, faster than the SSD.
- **memmap2:** `pread` into per-thread buffers cannot crash if a file shrinks mid-read.
- **rkyv:** serialization never showed up as a cost.
- **quinn:** replaced by ssh plus TLS-over-TCP streams.
- **indicatif:** a small built-in progress line.
- **filetime:** it opens files to set times, which blocks forever on a FIFO; `utimensat` is
  used instead.

---

## 4. Architecture and data pipeline

### Local sync

```
[root task]
    │  stat SRC, plan DEST (rsync's trailing-slash and single-file rules)
    ▼
[directory task]  (one per directory, run in parallel by rayon)
    1. read the source listing  (macOS: getattrlistbulk; Linux: readdir + statx)
    2. apply --exclude/--include (rsync wildmatch, first match wins)
    3. read the destination directory; --delete extras (except protected ones)
    4. per entry:
         directory  → mkdir if new, spawn its task
         file       → quick check (size + mtime) → skip / fix attributes / transfer
         symlink, device, FIFO → create or update in place
         -H link    → first name is the leader; the others are linked at the end
    5. transfer changed files in parallel:
         new / changed → copy (clone ≥ 512 KiB, else buffered) to .name.XXXXXX,
                         set mode/owner/mtime on the open file, rename
         --inplace     → write the final name directly (no temp, no rename)
         --no-whole-file → delta (see below)
[end]  link -H followers, then fix directory modes and mtimes deepest-first
```

### Delta (`--no-whole-file` locally, the default remotely)

1. **Aligned 64 KiB blocks first.** Both files are hashed in parallel. This is the best case
   for in-place edits and appends: VM images, databases, logs.
2. **FastCDC fallback.** Used when under half the bytes matched: 16/32/64 KiB chunks inside
   4 MiB segments, which keeps chunking deterministic and parallel.
3. **Apply:**
   - **Clone + patch** when ranges kept their offsets: only changed ranges are written.
   - **Rebuild** otherwise: parallel reads, sequential writes. Scattered writes into a sparse
     file degraded 2–4× under SSD write pressure.
   - **`--inplace`:** patch the destination directly.

### Remote (ssh)

```
client ──ssh──► fastrsync --server   (role, paths and options travel in the Hello frame)

sender                                   receiver
  Roots ──────────────────────────────►  plan destination
  DirBatch (per directory, as scanned) ►  same engine as local, on each listing
                                     ◄──  Need (one per DirBatch: files + basis signature)
  SmallPack (~4 MiB of small files) ───►  parallel writer pool (bounded, so it pushes back)
  FileBegin/DeltaBegin + Data + FileEnd ►  temp file / clone + patch → rename
  ScanDone, SenderDone ────────────────►
                                     ◄──  ReceiverDone (stats)
```

With `--streams N`, file data moves to N TLS connections; control stays on ssh. All frames of
one file use the same stream, so they stay in order.

---

## 5. Wire protocol

- **Frame format:** `u32 length | u8 kind | body`.
  - **Kind 0:** a control message, postcard-encoded. Messages: `Hello`, `HelloAck`, `Roots`,
    `DirBatch`, `Need`, `FileBegin`, `DeltaBegin`, `FileEnd`, `SmallPack`, `ScanDone`,
    `SenderDone`, `ReceiverDone`, `Log`, `Fatal`, `StreamOffer`, `StreamUnavailable`,
    `StreamsReady`, `StreamEnd`.
  - **Kind 1:** raw file `Data` (`id`, `offset`, bytes).
  - **Flag `0x80`:** zstd-compressed body (`-z`), compressed on the producing thread, so
    compression runs in parallel.
- **File ids:** `directory id << 32 | index in its listing`.
- **Basis signatures:**
  - aligned-block BLAKE3-128 hashes when the file size is unchanged;
  - FastCDC chunk hashes when it changed.
- **Threads:** each side has a dedicated reader thread and writer thread, so neither side can
  deadlock waiting for the other to drain the pipe.

---

## 6. Milestones (each committed on its own branch, stacked)

### M0 – M2: local engine (branch `local-engine`, 57dca15)

- rsync-compatible CLI subset, filters, parallel scan, quick check, dry run.
- Copying via clone, `copy_file_range` or buffered; temp file + rename.
- Symlinks, FIFOs and devices; `-p -t -o -g`; `--delete`; `-x`; `--inplace`.
- Cooperative Ctrl-C: in-flight files are discarded, so nothing is left half-written.

### M3: delta engine + name escaping (branch `delta-engine`, 03909f2)

- Aligned blocks with a FastCDC fallback; BLAKE3-verified matches.
- Clone + patch, parallel rebuild, `--inplace` patching.
- rsync-style `\#ooo` escaping of control characters and invalid UTF-8 in printed names
  (built in a side session).

### M4: remote over ssh (branch `remote-ssh`, 33ac9e4)

- Push and pull, server mode, small-file packing, deltas over the wire, `-z`, `-e`,
  `--remote-path`.
- Server output forwarded to the client.
- Backpressure: bounded frame queues and in-flight packs. Without it, memory reached 834 MiB
  and a worker stack overflowed on 5000 directories.

### M5: parallel streams (branch `parallel-tcp`, 585a65c)

- **Offer, over ssh:** the server listens on the `SSH_CONNECTION` address with an ephemeral
  rcgen certificate. It sends the address, the certificate's BLAKE3 fingerprint and a 32-byte
  token.
- **Connect:** the client opens TLS 1.3 connections that accept only that certificate and
  presents the token on each.
- **Fall back:** if no stream connects within 5 s, both sides stay on the ssh pipe.

### M6: polish (branch `polish-m6`, a39cc6f)

- `-H` hard links: same inode groups as rsync.
- `-i`/`-ii`/`%i`: rsync's `YXcstpoguax` codes, matching rsync in tests.
- `--progress`/`-P`/`--info=progress2`: one overall progress line.
- macOS `getattrlistbulk` and Linux `statx` metadata reads.

### M7: rsync drop-in flags + page-cache hints (branch `m7-integrated`, 3e58983)

- `-c -S -R -d -L -k -K -b -O`, `--partial(-dir)`, `--files-from`, `--existing`,
  `--max/min-size`, `--backup-dir`, `--link/compare-dest`, `--numeric-ids`, `--chmod`,
  `--remove-source-files`, each with a differential test (`tests/rsync_flags.rs`).
- Page-cache hints for files of 8 MiB+: `F_NOCACHE` (macOS), `fadvise` + bounded write-behind
  (Linux); `--no-drop-cache` turns them off.

### M8: verification harness (part of `m7-integrated`)

- Linux container with sshd and `tc netem` profiles (lan, wan, lossy), `strace -c` counts,
  perf flamegraphs, `cp` baselines; see the README's benchmark section.

### M9: the plan's delta design as `--delta=cdc` (branch `m9-delta-cdc`)

- FastCDC for every delta, chunked in parallel 16 MiB regions and stitched so the result
  equals one sequential pass; `ChunkSignature { offset, length, xxh3_hash, blake3_hash }`;
  files read through memmap2; files of 128 KiB or less never sent as deltas.
- Measured 4–7× slower than `--delta=blocks` on a 10 GiB file (mmap page faults and page
  cache pressure; hashing itself is not slower), so `blocks` stays the default.

### M10: the plan's scanner as `--scanner=jwalk` (branch `m10-jwalk-scanner`)

- Stage 1 `jwalk` scan with a `process_read_dir` hook (stat via a directory handle, prune
  excluded dirs and `-x` mounts), the destination scanned concurrently into a `DashMap`
  index, listings matched to directory jobs as they arrive, queues A–D with a worker pool
  each. The remote receiver indexes its destination the same way.
- Measured slower locally on macOS (9–11%) and Linux (1.6–2.1×); faster only for a macOS
  remote no-op (7%). `dirtask` stays the default.
- Found on the way: a fresh local copy of 5000 sibling directories overflowed the 2 MiB
  rayon worker stacks once the per-entry code grew (a task waiting in `par_iter` runs other
  directories' tasks on its stack). Local workers now get 32 MiB stacks, like the remote ones.
- jwalk's workers block inside rayon's `par_bridge`, so the hook must not use rayon itself
  (a nested `par_iter` there deadlocked); big directories are stat'ed on plain threads.

### M11: QUIC, rkyv, tokio layer (branch `m11-quic-rkyv`)

- `--transport ssh|tcp|quic`: QUIC (quinn) set up over ssh like `--streams`, control and
  listings on one stream, file data on `--streams` more; 3 s fallback to ssh.
- `--wire postcard|rkyv` (rkyv validated with bytecheck; frames carry their encoding).
- The tokio connection layer for ssh/TCP behind the hidden `--net-io=tokio`.
- Measured: QUIC ties the ssh pipe on a clean WAN, is a few percent slower on a LAN and 2–4×
  faster on a lossy link (after fixing control-stream starvation and the close). rkyv and
  tokio I/O are within noise. Defaults stay ssh / postcard / threads.

### M12: adaptive strategy (branch `m12-adaptive`)

- Link probe (ping + 256 KiB, 4 MiB more on fast links), whole-vs-delta routing at 1 Gbit/s
  and 32 MiB, pack compression with link-based zstd levels, `--compress-level`.
- Up to 7.6× faster on compressible data (with QUIC on a lossy link), 4–22% slower on
  incompressible data; `fixed` stays the default.

### M13: io_uring (branch `m13-io-uring`)

- Received small-file packs as three ring batches (openat, write, close) per pack.
- 24% fewer syscalls, no faster; opt-in. Fixed on the way: a redundant truncate of every
  received file, which cost XFS a writeback per file (3.2× faster small-file pushes).

### Testing

- **Coverage:** 79 tests on macOS and 81 on Linux (aarch64 container, glibc rsync 3.2.7).
- **Differential tests:** most tests run the same command through rsync and fastrsync and
  require identical printed items and identical trees (content, mode, mtime, symlink targets,
  hard-link groups).
- **Remote tests:** run through a fake ssh that starts the server locally, against rsync over
  the same shim.
- **Delta property test:** random insert/delete/overwrite/append/truncate sequences, checked
  with up to 5000 cases. Every delta must rebuild the file exactly.

---

## 7. Code map

```
src/cli.rs            flags → Options (also sent to the remote server)
src/paths.rs          host:path detection, trailing-slash rules
src/filter.rs         rsync wildmatch, include/exclude rules
src/diff.rs           quick check (size + mtime, -u, --size-only, -I, --modify-window)
src/entry.rs          Meta / DirEnt (serializable)
src/fsops/            platform I/O: getattrlistbulk, statx, clone, copy_file_range,
                      temp files, utimensat, Ctrl-C cleanup
src/local/            planner + the engine (shared by local sync and the remote receiver)
src/delta/            signatures, diff, apply (clone + patch, ordered rebuild)
src/proto/            frames, messages, connection threads, data-stream routing
src/remote/           client (spawns ssh), server, sender, receiver, TLS streams
src/output.rs         item lines, %i, escaping, forwarding, stats
src/progress.rs       overall progress line
tests/                local, delta, remote, M6 features; fake ssh in tests/common
bench/gen.py, run.sh  datasets A/B/C, hyperfine runner incl. remote (D) and --cold
```

---

## 8. Benchmarks and what they showed

All runs use hyperfine on a warm cache. Big-file runs were interleaved, because on this
machine large sequential writes slow down as the SSD's write cache fills.

| Workload | rsync | fastrsync |
|---|---:|---:|
| A: copy 200k × 4 KiB files | 43.5 s | 18.3 s (2.4×) |
| A with `--inplace` (both tools) | 29.6 s | 11.6 s (2.6×) |
| C: 500k files, 3 changed | 11.4 s | 3.5 s (3.3×) |
| B: 10 GiB, 50 × 1 MiB changed, delta mode | 19.5–22.7 s | 6.5–6.9 s (~3×) |
| B without clones (full rebuild) | 19.5–22.7 s | 17.3–20.0 s (1.0–1.2×) |
| D: remote push of A (local pipe) | 45.3 s | 16.9 s (2.7×) |
| D: remote no-op of C (local pipe) | 15.2 s | 3.1 s (4.9×) |

Measurements that shaped the design:

- **APFS clones lose below ~512 KiB.** They are 20% slower than a plain copy for small files,
  and 2–15× faster from 1 MiB up. Hence the 512 KiB threshold.
- **The temp-file rename costs ~⅓ of a small file's time.** That is why `--inplace` exists.
- **Small-file copies scale to only ~4 threads.** APFS metadata locking caps them.
- **No-op syncs wait on filesystem metadata, not CPU.** One thread takes 12.2 s wall time
  for 4.4 s of CPU.
  - `getattrlistbulk` saves 15–30% kernel CPU but cannot lower that floor.
  - The default stays at 2 threads per core, which is fastest; `--threads 6` uses about a
    third of the CPU.
- **`--streams` doesn't help over a local pipe.** Its benefit is on real networks, which
  haven't been measured.

---

## 9. Changes from the original plan

| Original plan | As built | Reason |
|---|---|---|
| QUIC over UDP (quinn), up to 128 streams | ssh pipe + `--streams N` TLS-over-TCP | Your choice at planning time. UDP is often blocked, and quinn tends to be CPU-bound. |
| jwalk + DashMap of both trees | Per-directory rayon pipeline | Streaming; memory grows with directories |
| xxh3 filter + BLAKE3 per chunk | BLAKE3-128 per block/chunk | One hash, faster than the disk, cryptographically verified. The plan's version is `--delta=cdc` (M9), measured slower. |
| FastCDC for every large file | Aligned blocks first, FastCDC fallback | In-place edits are the common case for big files. The plan's version is `--delta=cdc` (M9). |
| Merge chunks across 16 MB regions | Fixed cut points at 4 MiB segment edges | Deterministic; at most one extra chunk per segment. `--delta=cdc` stitches regions exactly (M9). |
| rkyv zero-copy | serde + postcard | Not a bottleneck |
| tokio / tokio-uring | Threads, crossbeam, rayon | File I/O blocks anyway |
| mmap | pread into per-thread buffers | No crash if a file shrinks mid-read; `--delta=cdc` uses mmap and measured 4–7× slower on 10 GiB files (M9) |
| `std::fs::copy` | Own clone / `copy_file_range` / buffered paths | Measured clone threshold, fd-based attributes |
| Starter code's temp names | `.name.XXXXXX` built from raw bytes | The original collapsed every non-UTF-8 name onto one temp name |
| Starter code "crash-safe" without fsync | rsync semantics; `--fsync` optional | rsync doesn't fsync either |

---

## 10. Still open

Being built now, in this order (approved plan "M7–M14"); each one is benchmarked against the
current design and the faster one stays the default:

- **M10** jwalk + DashMap scanner with queues A–D (`--scanner`).
- **M11** tokio + rkyv (`--wire`) + QUIC (`--transport quic|tcp|ssh`).
- **M12** adaptive strategy: link probe, size routing, pack compression, adaptive zstd level.
- **M13** io_uring on Linux (`--io-uring`).
- **M14** full verification (every transport and link profile, cold cache, the real Intel Mac)
  and these documents.

The real second machine (Intel MacBook Pro at 192.168.1.13) needs key-based ssh first:
`ssh-copy-id -i ~/.ssh/id_ed25519.pub a.hamdy@192.168.1.13`.

Also still missing from rsync's feature set: xattrs and ACLs (`-X`, `-A`), daemon mode
(`rsync://`), and the single-argument listing mode.

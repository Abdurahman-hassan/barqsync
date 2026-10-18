# barqsync vs fast_rsync

An investigation of how barqsync 0.1.1 (this repository at `c18f18d`) compares with
[fast_rsync](https://crates.io/crates/fast_rsync) 0.2.0, first by reading both crates, then by
measuring real transfers of the same data over a shaped link, with rsync 3.2.7 as a reference.
Done on 2026-10-18. The harness is in [`bench/fast_rsync/`](bench/fast_rsync/), the raw results in
`bench/results/fast_rsync-*-20261018.*`.

## Summary

- **They are different kinds of crate.** fast_rsync is a small librsync-compatible delta library
  (signature, diff and apply on in-memory buffers). barqsync is an rsync-compatible file
  synchronizer with its own, parallel delta engine. barqsync does not use fast_rsync.
- **Where all three send about the same bytes, barqsync is fastest.** For 256 MiB files changed
  in place or appended to: 3.3–3.6× faster than fast_rsync and 1.4–1.6× faster than rsync on
  1 Gbit, 1.9× and 1.1× on 100 Mbit with a 20 ms round trip. Its delta engine finishes a 256 MiB
  file 2–6× sooner (on 4 cores; fast_rsync's diff uses one).
- **Where changes are small or scattered, fast_rsync and rsync send far fewer bytes.** barqsync
  uploaded 3.6× as much for inserts and deletes, 19× as much for 1 KiB edits in 4 MiB files, and
  1.85× as much for a SQLite database. On 100 Mbit the database took barqsync 16.7 s against
  9.4 s for rsync and 11.3 s for fast_rsync.
- **The gap comes from barqsync's block and chunk sizes, not its design.** With 4 KiB aligned
  blocks, barqsync's own engine would send 30 MB for the database instead of 192 MB, less than
  rsync's 104 MB. With 8 KiB content-defined chunks its upload for inserts and deletes drops
  from 4.6 MB to about 1.4 MB, close to rsync's 1.3 MB (for a 0.7 MB signature instead of 0.2 MB).
- All 90 measured transfers produced byte-identical destinations.

## 1. What each crate is

| | barqsync 0.1.1 | fast_rsync 0.2.0 |
| --- | --- | --- |
| Kind | CLI (`rsync` replacement) plus a library | Library only |
| Scope | Directory trees: parallel scan, filters, metadata, `--delete`, ssh / TLS streams / QUIC transports | One buffer: `Signature::calculate`, `diff`, `apply` |
| Size | ~9.9k lines of Rust, 24 direct dependencies | ~1.7k lines of Rust, 1 dependency (`arrayref`) |
| Platforms | macOS and Linux (Unix APIs, APFS clones, `copy_file_range`, io_uring) | Portable pure Rust; SIMD MD4 on x86, x86-64 and aarch64 |
| Concurrency | rayon throughout: per-directory tasks, parallel hashing and rebuild | Single-threaded; multi-buffer SIMD MD4 for signatures |
| Compatibility | rsync's command line; own wire protocol, so barqsync runs on both ends | librsync / `rdiff` signature and delta format (MD4 only, no BLAKE2) |
| License, maintainer | MIT OR Apache-2.0; this project; first released 2026-10-17 | Apache-2.0; Dropbox; last release 2023-03-31, ~775k downloads |

barqsync's modules are all `pub` (`src/lib.rs`), but that is its internals, not a designed
library API; fast_rsync has a small documented API (`#![deny(missing_docs)]`).

## 2. How the delta algorithms differ

**fast_rsync** implements the classic rsync / librsync scheme:

- The receiver's signature holds, for every fixed block of the old file, a weak rolling
  checksum and a truncated MD4 hash.
- The sender rolls the weak checksum over its data one byte at a time; on a hit it confirms
  with MD4 and emits a copy, otherwise one more literal byte. Moved data matches at any byte
  offset.
- MD4 is broken, so the README requires checking the rebuilt data with another hash.
- Everything works on `&[u8]` and writes into `impl Write`, so both files live in memory.

**barqsync** never searches byte by byte. Both sides hash their own copy and compare
signatures (`src/delta/mod.rs`, `src/remote/receiver.rs` `basis_signature`):

- **Same size → aligned blocks**: 64 KiB blocks at the same offsets in both files. Ideal for
  in-place edits; the receiver clones the old file and writes only the changed blocks.
- **Size changed → content-defined chunks** (FastCDC, 16/32/64 KiB min/avg/max): inserts and
  deletes only cost the chunks around them.
- Every block and chunk is matched by BLAKE3-128, so each reused range is verified and no
  whole-file check is needed.
- Hashing and rebuilding run in parallel over 4 MiB segments, reading with `pread`.

The trade-off: fast_rsync finds matches at a finer grain (blocks of about √size, at any
offset) but its diff is sequential per file; barqsync matches coarser units but hashes on every
core and can patch a clone instead of rewriting the file.

## 3. Method

### Harness: `frsync`

fast_rsync has no transport, so [`frsync`](bench/fast_rsync/src/bin/frsync.rs) wraps it in the
smallest fair push tool, using the same ssh pipe as barqsync and rsync and one round trip for
the whole transfer:

1. The client sends the list of files.
2. The server answers with the MD4 signature of each existing destination file, using rsync's
   block size rule (√size rounded to a multiple of 8, between 700 B and 128 KiB) and an 8-byte
   strong hash.
3. The client sends a librsync delta per file (or the whole file when the delta would not be
   smaller) plus the BLAKE3 hash of the new data.
4. The server applies the delta, checks the hash, writes a temp file, sets the mtime and
   renames it.

Signatures, diffs and applies run in parallel across files with rayon; each file's diff is
single-threaded, as fast_rsync is.

[`engine`](bench/fast_rsync/src/bin/engine.rs) compares the two delta engines in-process on one
file pair, without the network: for barqsync it makes the same signature and diff calls as a
remote push (receiver's basis signature, sender's signature and diff), then rebuilds the new
file from the delta.

### Link

- The client talks to an sshd in a separate network namespace over a veth pair; both share
  the filesystem, which is a tmpfs, so the link and CPU are what is measured.
- **lan**: 1 Gbit, no added delay.
- **wan**: 100 Mbit, 20 ms round trip.
- The VM's kernel has no `netem`. Bandwidth is limited with `tbf` on both veth ends; the wan
  delay comes from [`relay.py`](bench/fast_rsync/relay.py), a TCP relay in front of sshd that
  holds every chunk for 10 ms in each direction. That models round-trip latency but not TCP slow
  start or loss on a long path; it affects the three tools alike.
- Checked with `ssh … cat`: 50 MB took 4.76 s at 100 Mbit (4.2 s of data plus ssh setup), 0.73 s
  at 1 Gbit and 0.44 s unshaped; the relay adds ~160 ms to an ssh login.

### Datasets ([`gen.py`](bench/fast_rsync/gen.py))

| Name | Old copy on the receiver | New copy on the sender |
| --- | --- | --- |
| inplace | 256 MiB random file | 8 × 1 MiB overwritten in place |
| shift | 256 MiB random file | 40 inserts / deletes of 1–64 KiB at random offsets |
| append | 256 MiB random file | 8 MiB appended |
| mid | 40 random files of 4 MiB | 1 KiB changed in each |
| sqlite | 252 MB SQLite database (400,000 rows, 4 KiB pages, one index) | after 2,000 `UPDATE`s, 5,000 `INSERT`s and 1,000 `DELETE`s (255.6 MB) |

### Runs

- Commands: `frsync -e ssh SRC HOST:DST`, `barqsync -a -e ssh SRC/ HOST:DST/` (defaults: ssh
  pipe, delta, `--strategy fixed`), `rsync -a -e ssh SRC/ HOST:DST/` (delta, as rsync does
  remotely).
- 2 profiles × 5 datasets × 3 tools × 3 runs, the tools interleaved within each run.
- Before every run the destination is reset to the old copy (old mtime, so the quick check
  sees every file as changed). Afterwards the destination is compared with `diff -r`.
- Bytes are read from the veth counters, so they include ssh and TCP overhead.

### Environment

4 vCPU Intel Xeon @ 2.10 GHz (Firecracker VM), 15 GB RAM, Linux 6.18, Ubuntu 24.04, rustc
1.97.0, OpenSSH 9.6p1 (chacha20-poly1305), rsync 3.2.7.

### Problems found on the way

- The first attempt used `tc netem`, which this kernel lacks; `tc` failed and the runs were
  unshaped, so they were discarded.
- `ip netns exec` gives sshd a private copy of the mount table, so the tmpfs mounted after sshd
  had started was invisible to the "remote" side, and every transfer wrote to the directory
  underneath. Caught by the verification step; `setup.sh` now mounts first.
- In this VM, the first touch of fresh guest memory is expensive, which made fast_rsync's
  in-memory apply vary between 0.16 and 1.3 s for the same work. Memory was pre-touched before
  the engine runs; the network runs came after that. `setup.sh` now does this; without it, a
  freshly booted VM measured `frsync` at 4.3 s instead of 1.5 s on `shift`.

## 4. Results

### 4.1 Real transfers: wall time

Median of 3 runs (min–max), lowest in bold.

**lan (1 Gbit)**

| Dataset | fast_rsync | barqsync | rsync |
| --- | ---: | ---: | ---: |
| inplace | 1.70 s (1.62–1.70) | **0.47 s** (0.47–0.51) | 0.67 s (0.67–0.77) |
| shift | 1.52 s (1.32–1.62) | **0.54 s** (0.52–0.57) | 0.59 s (0.58–0.60) |
| append | 1.74 s (1.60–1.77) | **0.53 s** (0.51–0.57) | 0.83 s (0.75–0.87) |
| mid | **0.37 s** (0.37–0.41) | 0.45 s (0.43–0.45) | 0.40 s (0.38–0.41) |
| sqlite | 3.58 s (3.53–3.67) | **2.07 s** (2.04–2.11) | 2.12 s (1.91–2.32) |

**wan (100 Mbit, 20 ms RTT)**

| Dataset | fast_rsync | barqsync | rsync |
| --- | ---: | ---: | ---: |
| inplace | 2.36 s (2.36–2.36) | **1.21 s** (1.19–1.21) | 1.35 s (1.34–1.39) |
| shift | 1.76 s (1.68–1.80) | 0.97 s (0.97–0.97) | **0.81 s** (0.81–0.87) |
| append | 2.43 s (2.37–2.46) | **1.28 s** (1.27–1.31) | 1.45 s (1.42–1.52) |
| mid | **0.56 s** (0.56–0.57) | 0.71 s (0.71–0.72) | 0.66 s (0.66–0.67) |
| sqlite | 11.34 s (11.17–11.47) | 16.68 s (16.67–16.70) | **9.38 s** (9.36–9.40) |

### 4.2 Bytes on the link

Median over all runs, up (client to server) / down.

| Dataset | fast_rsync | barqsync | rsync |
| --- | ---: | ---: | ---: |
| inplace | 8.02 / 0.22 MB | 8.35 / 0.09 MB | 8.10 / 0.15 MB |
| shift | **1.27** / 0.21 MB | 4.63 / 0.16 MB | 1.33 / 0.13 MB |
| append | 8.42 / 0.22 MB | 8.45 / 0.17 MB | 8.50 / 0.14 MB |
| mid | **0.14** / 1.00 MB | 2.64 / 0.05 MB | 0.46 / 0.50 MB |
| sqlite | **104.22** / 0.40 MB | 192.83 / 0.55 MB | 104.34 / 0.36 MB |

The down column is mostly signatures. fast_rsync's 2 KiB blocks on 4 MiB files cost 1 MB of
signatures for `mid`, a large share of the 1.14 MB it moved in total (barqsync 2.69 MB,
rsync 0.96 MB).

### 4.3 Delta engines alone

One file pair, in-process, median of 3 runs, 4 cores. fast_rsync: signature, rolling diff,
apply in memory, BLAKE3 check. barqsync: basis signature, new-file signature and diff, rebuild
into a file. barqsync's signature sizes are raw estimates (16 B per block, 28 B per chunk;
postcard encodes them smaller).

| Dataset | Engine | Unit | Signature | Delta | Signature ms | Diff ms | Apply ms | Verify ms | Total ms |
| --- | --- | --- | ---: | ---: | ---: | ---: | ---: | ---: | ---: |
| inplace | fast_rsync | 16 KiB blocks | 197 KB | 8.00 MB | 67 | 433 | 162 | 18 | 772 |
| inplace | barqsync | 64 KiB blocks | 66 KB | 8.32 MB | 30 | 26 | 106 | – | **181** |
| shift | fast_rsync | 16 KiB blocks | 197 KB | **1.26 MB** | 64 | 356 | 160 | 18 | 595 |
| shift | barqsync | 32 KiB chunks | 180 KB | 4.61 MB | 68 | 72 | 126 | – | **297** |
| append | fast_rsync | 16 KiB blocks | 197 KB | 8.39 MB | 66 | 433 | 174 | 16 | 869 |
| append | barqsync | 32 KiB chunks | 180 KB | 8.42 MB | 61 | 60 | 108 | – | **227** |
| sqlite | fast_rsync | 15.5 KiB blocks | 191 KB | **103.96 MB** | 58 | 1356 | 147 | 18 | 1580 |
| sqlite | barqsync | 32 KiB chunks | 163 KB | 192.35 MB | 55 | 56 | 139 | – | **263** |
| mid (one 4 MiB file) | fast_rsync | 2 KiB blocks | 25 KB | **0.002 MB** | 1 | 6 | 3 | 1 | 11 |
| mid (one 4 MiB file) | barqsync | 64 KiB blocks | 1 KB | 0.07 MB | 4 | 2 | 4 | – | **9** |

fast_rsync's diff takes 356–1356 ms per 256 MiB on one core, slowest when much of the file
does not match and the checksum rolls byte by byte. barqsync's signature diff takes 26–72 ms.

## 5. Why barqsync sends more: granularity

[`granularity`](bench/fast_rsync/src/bin/granularity.rs) runs barqsync's own delta functions
with smaller units on the same files (literal bytes the sender would send; signature size as a
raw estimate):

| Dataset | Unit | Literal data | Signature |
| --- | --- | ---: | ---: |
| sqlite | 64 KiB aligned blocks | 213.3 MB | 62 KB |
| sqlite | 16 KiB aligned blocks | 94.8 MB | 246 KB |
| sqlite | **4 KiB aligned blocks** | **29.8 MB** | 986 KB |
| sqlite | 32 KiB chunks (current) | 192.3 MB | 163 KB |
| sqlite | 8 KiB chunks | 92.2 MB | 656 KB |
| sqlite | 4 KiB chunks | 62.2 MB | 1,305 KB |
| shift | 32 KiB chunks (current) | 4.61 MB | 180 KB |
| shift | **8 KiB chunks** | **1.36 MB** | 717 KB |
| shift | 4 KiB chunks | 1.04 MB | 1,434 KB |
| mid (one file) | 64 KiB aligned blocks (current) | 66 KB | 1 KB |
| mid (one file) | 16 KiB aligned blocks | 20 KB | 4 KB |
| mid (one file) | 4 KiB aligned blocks | 4 KB | 16 KB |

All of these took 50–150 ms for a 256 MiB file, so smaller units keep barqsync's speed.

The SQLite case also shows a second issue. A database changes 4 KiB pages in place and grows
at the end. Because its size changed, barqsync switched to content-defined chunks, which do
worse than aligned blocks of similar size here: 62 MB with 4 KiB average chunks against 30 MB
with 4 KiB aligned blocks. Those 30 MB are about a sixth of what barqsync sends now and under
a third of what rsync and fast_rsync send.

## 6. Conclusions

1. **Scale the block and chunk size with the file size**, like rsync's √size rule, instead of
   fixed 64 KiB blocks and 32 KiB average chunks. The signature grows (16 B per block), but for
   the files here it stays under 1.5 MB per 256 MiB, while deltas shrink 3–6×.
2. **Keep aligned blocks when a file only grew or shrank a little.** Databases, VM images and
   logs keep their existing data in place. Options: aligned blocks over the common length plus
   the tail sent whole, or send both signatures for large files and let the sender take the
   better diff.
3. **A rolling search is not needed to close the gap.** fast_rsync's byte-by-byte search is
   what makes its diff sequential, and it took 5–24× as long as barqsync's diff here; 8 KiB
   content-defined chunks bring the shift delta to 1.36 MB, against 1.26 MB for fast_rsync,
   in about 130 ms.
4. **Nothing else to adopt from fast_rsync** for barqsync: MD4 needs a separate verification
   pass, and an in-memory API does not fit files larger than RAM or clone-and-patch.
5. **For other users:** fast_rsync remains the right choice for a portable, minimal delta
   library or librsync-format compatibility; barqsync is the choice for syncing trees on
   macOS and Linux, and would be the faster one on scattered edits too once (1) and (2) are
   done.

## 7. Limitations

- `frsync` is a minimal harness: it reads whole files into memory and does not overlap
  reading, diffing and writing within a file. A production tool built on fast_rsync could
  stream or mmap; the single-threaded diff, which dominates its engine time, would stay.
- The link emulation has no loss and no long-path TCP dynamics (see [Link](#link)).
- One 4-vCPU VM with data on tmpfs: disk speed and larger core counts are not measured. On
  more cores barqsync's engine would get faster; fast_rsync's per-file diff would not.
- All datasets but `sqlite` are random data; real files with repeated content can match
  differently.

## 8. Reproducing

As root on Linux:

```bash
apt-get install -y openssh-server openssh-client iproute2 rsync python3
bench/fast_rsync/setup.sh                  # builds, datasets on a /bench tmpfs, pre-touches memory, netns + sshd + relay
bench/fast_rsync/engine.sh > engine.txt    # in-process engine comparison
bench/fast_rsync/netbench.sh net.tsv       # real transfers (PROFILES, WORKLOADS, TOOLS, RUNS)
python3 bench/fast_rsync/summarize.py net.tsv engine.txt
bench/fast_rsync/target/release/granularity /bench/data/sqlite/old/app.db /bench/data/sqlite/new/app.db
```

After a reboot, `setup.sh` from scratch regenerated identical datasets (same granularity
numbers to the byte). Spot checks with `netbench.sh` reproduced the byte counts and came close
to the times above: on lan `shift`, frsync took 1.51–1.97 s and barqsync 0.52–0.57 s, against
medians of 1.52 s and 0.54 s.

Raw results of this run: `bench/results/fast_rsync-net-20261018.tsv`,
`bench/results/fast_rsync-engine-20261018.txt` and
`bench/results/fast_rsync-granularity-20261018.txt`.

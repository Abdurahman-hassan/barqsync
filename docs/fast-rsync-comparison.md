# barqsync vs fast_rsync

An investigation of how barqsync 0.1.1 (this repository at `c18f18d`) compares with
[fast_rsync](https://crates.io/crates/fast_rsync) 0.2.0, first by reading both crates, then by
measuring real transfers of the same data over a shaped link, with rsync 3.2.7 as a reference.
Done on 2026-10-18. The harness is in [`bench/fast_rsync/`](../bench/fast_rsync/), the raw results in
`bench/results/fast_rsync-*-20261018.*`. Sections 1–8 describe barqsync 0.1.1. The fixes they
led to came in two steps: M15 ([section 9](#9-after-the-fixes-m15)) fixed scattered edits but
slowed appends, and M16 ([section 10](#10-two-level-delta-m16)), the current design, is as fast
as 0.1.1 on appends and in-place runs and faster on everything else.

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
- **Update, M16:** with a two-level delta (coarse signatures as in 0.1.1, refined on demand
  with one round trip), barqsync over 100 Mbit is as fast as 0.1.1 on appends, in-place runs
  and small files, 1.3–7.6× faster on everything else (the database in 3.2 s against 16.7 s,
  and rsync's 9.4 s), and faster than rsync on 7 of 8 datasets. See
  [section 10](#10-two-level-delta-m16); M15's intermediate step is in section 9.

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

fast_rsync has no transport, so [`frsync`](../bench/fast_rsync/src/bin/frsync.rs) wraps it in the
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

[`engine`](../bench/fast_rsync/src/bin/engine.rs) compares the two delta engines in-process on one
file pair, without the network: for barqsync it makes the same signature and diff calls as a
remote push (receiver's basis signature, sender's signature and diff), then rebuilds the new
file from the delta.

### Link

- The client talks to an sshd in a separate network namespace over a veth pair; both share
  the filesystem, which is a tmpfs, so the link and CPU are what is measured.
- **lan**: 1 Gbit, no added delay.
- **wan**: 100 Mbit, 20 ms round trip.
- The VM's kernel has no `netem`. Bandwidth is limited with `tbf` on both veth ends; the wan
  delay comes from [`relay.py`](../bench/fast_rsync/relay.py), a TCP relay in front of sshd that
  holds every chunk for 10 ms in each direction. That models round-trip latency but not TCP slow
  start or loss on a long path; it affects the three tools alike.
- Checked with `ssh … cat`: 50 MB took 4.76 s at 100 Mbit (4.2 s of data plus ssh setup), 0.73 s
  at 1 Gbit and 0.44 s unshaped; the relay adds ~160 ms to an ssh login.

### Datasets ([`gen.py`](../bench/fast_rsync/gen.py))

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

[`granularity`](../bench/fast_rsync/src/bin/granularity.rs) runs barqsync's own delta functions
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
   the files here it stays under 1.5 MB per 256 MiB, while deltas shrink 3–6×. *Done in
   M15 (section 9).*
2. **Keep aligned blocks when a file only grew or shrank a little.** Databases, VM images and
   logs keep their existing data in place. Options: aligned blocks over the common length plus
   the tail sent whole, or send both signatures for large files and let the sender take the
   better diff. *Done in M15: both signatures, merged rather than picked (section 9).*
3. **A rolling search is not needed to close the gap.** fast_rsync's byte-by-byte search is
   what makes its diff sequential, and it took 5–24× as long as barqsync's diff here; 8 KiB
   content-defined chunks bring the shift delta to 1.36 MB, against 1.26 MB for fast_rsync,
   in about 130 ms.
4. **Nothing else to adopt from fast_rsync** for barqsync: MD4 needs a separate verification
   pass, and an in-memory API does not fit files larger than RAM or clone-and-patch.
5. **For other users:** fast_rsync remains the right choice for a portable, minimal delta
   library or librsync-format compatibility; barqsync is the choice for syncing trees on
   macOS and Linux, and would be the faster one on scattered edits too once (1) and (2) are
   done. *It now is (section 9).*

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

Sections 9 and 10 used all eight datasets, and older barqsync builds installed as
`barqsync-NAME` on both ends, here from `git worktree`s of earlier commits: 0.1.1's code
is `c18f18d`, the last commit on `main`, and M15's is the commit "Document and measure M15:
scaled block and chunk sizes, merged deltas" on this branch:

```bash
git worktree add ../bq-011 <0.1.1 commit> && (cd ../bq-011 && cargo build --release)
ln -sf "$PWD/../bq-011/target/release/barqsync" /usr/local/bin/barqsync-0.1.1
W8="inplace scatter shift append mid sqlite tar csv"
WORKLOADS="$W8" TOOLS="frsync barqsync-0.1.1 barqsync rsync" bench/fast_rsync/netbench.sh net.tsv
ENGINE_WORKLOADS="inplace scatter shift append sqlite tar csv" bench/fast_rsync/engine.sh > engine.txt
bench/fast_rsync/target/release/rules OLD NEW [OLD NEW ...] > rules.tsv   # sizing rules
```

## 9. After the fixes (M15)

*M15's signature scheme below (both signatures for every resized file) was replaced in M16,
because it made appends and in-place runs slower; see [section 10](#10-two-level-delta-m16).*

Conclusions (1) and (2) are implemented:

- **Sizes scale with the file:** `Params::for_size` uses a quarter of √size as a power of two,
  4 KiB to 64 KiB (4 KiB up to 256 MiB). Chunks average the block size (half to twice it). The
  receiver picks the sizes from its basis file; the signature carries them.
- **A remote file that changed size gets both signatures,** aligned blocks and chunks from one
  read of the basis, and the sender merges the two deltas: every literal range of the chunk
  delta that the block delta found unchanged in place becomes a copy, so the result never has
  more literal data than either. The local CDC fallback is merged the same way.
- Chunk entries drop their offset (implied by the lengths). Protocol version 6: a version 5
  peer is refused with "protocol version mismatch".
- `--delta=cdc` is unchanged.

### Choosing the sizes

To avoid tuning on the very datasets being reported, three more were added to `gen.py`:
`tar` (a 54 MB archive of 4,000 text files, 1–10 lines inserted into 50 of them), `csv` (78 MB,
300 rows rewritten with fields of another length) and `scatter` (256 MiB, 3,000 writes of 64
bytes in place). [`rules`](../bench/fast_rsync/src/bin/rules.rs) then ran barqsync's own signature,
diff and merge functions with five sizing rules and measured what would cross the link: the
postcard-encoded signature plus literal data and ops (`bench/results/fast_rsync-rules-20261018.tsv`).
Three of the rules, for what barqsync sends (blocks for a same-size file; for a resized one,
chunks before M15, merged blocks and chunks after):

| Dataset | Fixed 64 KiB / 32 KiB (before) | √size | ¼ √size (chosen) |
| --- | ---: | ---: | ---: |
| inplace (same size) | 8.39 MB | **8.26 MB** | 8.95 MB |
| scatter (same size) | 141.45 MB | 45.61 MB | **13.24 MB** |
| mid, one 4 MiB file (same size) | 0.07 MB | **0.02 MB** | **0.02 MB** |
| shift | 4.76 MB | 3.20 MB | **3.22 MB** |
| append | **8.57 MB** | 8.94 MB | 10.56 MB |
| sqlite | 192.46 MB | 85.74 MB | **29.97 MB** |
| tar | 3.85 MB | 1.61 MB | **1.29 MB** |
| csv | 14.18 MB | 7.78 MB | **2.73 MB** |
| **Total** | 373.7 MB | 161.2 MB | **70.0 MB** |

¼ √size moves 5.3× less than the fixed sizes in total and stays within 1.7× of the best
combination of rule and policy (any of the five rules, chunks alone or merged) on every
dataset. Its losses are the signature: about 1 MB per 256 MiB for
blocks and 1 MB more for chunks, which `inplace` and `append` pay without gaining from it.
Sending chunks alone for resized files would save that second megabyte (`shift` 2.17 MB,
`append` 9.52 MB) but double the database's delta (63.3 MB), so both signatures it is.

### Real transfers

Same link, method and verification as section 3, now with all eight datasets and four tools:
fast_rsync (`frsync`), barqsync before M15 (installed as `barqsync-old`), barqsync after, and
rsync. 192 transfers, all byte-identical; raw data in
`bench/results/fast_rsync-net-m15-20261018.tsv`. Median of 3 runs (min–max), lowest in bold.

**lan (1 Gbit)**

| Dataset | fast_rsync | barqsync before | barqsync M15 | rsync |
| --- | ---: | ---: | ---: | ---: |
| inplace | 2.93 s (1.80–3.08) | **0.51 s** (0.48–0.54) | 0.53 s (0.53–0.53) | 0.71 s (0.71–0.72) |
| scatter | 3.15 s (2.45–3.47) | 1.61 s (1.57–1.62) | **0.64 s** (0.59–0.69) | 1.04 s (1.03–1.05) |
| shift | 2.25 s (1.50–2.57) | **0.55 s** (0.54–0.58) | 0.66 s (0.63–0.68) | 0.57 s (0.56–0.77) |
| append | 1.78 s (1.70–2.90) | **0.55 s** (0.54–0.63) | 0.67 s (0.64–0.68) | 0.83 s (0.80–0.91) |
| mid | **0.40 s** (0.40–0.43) | 0.46 s (0.44–0.46) | 0.48 s (0.47–0.50) | 0.40 s (0.39–0.44) |
| sqlite | 4.06 s (3.83–4.43) | 2.09 s (2.06–2.12) | **0.91 s** (0.90–0.98) | 1.78 s (1.68–1.94) |
| tar | 0.52 s (0.50–0.56) | 0.35 s (0.35–0.40) | **0.35 s** (0.34–0.36) | 0.36 s (0.34–0.36) |
| csv | 0.69 s (0.67–0.71) | 0.50 s (0.48–0.53) | 0.38 s (0.36–0.40) | **0.37 s** (0.37–0.39) |

**wan (100 Mbit, 20 ms RTT)**

| Dataset | fast_rsync | barqsync before | barqsync M15 | rsync |
| --- | ---: | ---: | ---: | ---: |
| inplace | 2.71 s (2.42–3.44) | **1.21 s** (1.20–1.24) | 1.28 s (1.26–1.28) | 1.37 s (1.36–1.38) |
| scatter | 5.95 s (5.95–5.96) | 12.36 s (12.36–12.40) | **1.64 s** (1.64–1.66) | 4.46 s (4.44–4.51) |
| shift | 1.78 s (1.77–2.17) | 0.98 s (0.97–0.98) | 0.98 s (0.96–0.98) | **0.88 s** (0.88–0.89) |
| append | 2.50 s (2.47–2.73) | **1.32 s** (1.31–1.32) | 1.55 s (1.52–1.56) | 1.49 s (1.46–1.50) |
| mid | **0.59 s** (0.57–0.59) | 0.73 s (0.73–0.74) | 0.74 s (0.73–0.78) | 0.70 s (0.70–0.75) |
| sqlite | 11.65 s (11.64–11.73) | 16.70 s (16.69–16.71) | **3.18 s** (3.17–3.21) | 9.38 s (9.36–9.40) |
| tar | 0.78 s (0.76–0.79) | 0.80 s (0.79–0.81) | **0.58 s** (0.57–0.58) | 0.63 s (0.63–0.64) |
| csv | 1.06 s (1.05–1.08) | 1.67 s (1.66–1.69) | **0.72 s** (0.72–0.73) | 0.80 s (0.80–0.81) |

**Bytes on the link**, median over all runs, up (client to server) / down:

| Dataset | fast_rsync | barqsync before | barqsync M15 | rsync |
| --- | ---: | ---: | ---: | ---: |
| inplace | 8.02 / 0.22 MB | 8.35 / 0.09 MB | **7.94** / 1.07 MB | 8.10 / 0.14 MB |
| scatter | 45.44 / 0.29 MB | 141.79 / 0.38 MB | **12.27** / 1.08 MB | 45.53 / 0.23 MB |
| shift | 1.27 / 0.21 MB | 4.63 / 0.16 MB | **1.06** / 1.98 MB | 1.33 / 0.13 MB |
| append | 8.42 / 0.22 MB | 8.45 / 0.17 MB | 8.42 / 2.00 MB | 8.50 / 0.14 MB |
| mid | **0.14** / 1.00 MB | 2.64 / 0.05 MB | 0.22 / 0.66 MB | 0.46 / 0.50 MB |
| sqlite | 104.21 / 0.40 MB | 192.84 / 0.55 MB | **28.12** / 1.90 MB | 104.32 / 0.36 MB |
| tar | 0.87 / 0.10 MB | 3.84 / 0.04 MB | **0.86** / 0.40 MB | 0.90 / 0.06 MB |
| csv | 2.62 / 0.12 MB | 14.18 / 0.08 MB | **2.11** / 0.58 MB | 2.65 / 0.07 MB |

What changed:

- **Against barqsync before:** over 100 Mbit, 7.5× faster on `scatter`, 5.3× on `sqlite`,
  2.3× on `csv` and 1.4× on `tar`; `shift` and `mid` unchanged; `inplace` 6% and `append` 17%
  slower. On 1 Gbit the same pattern, with `shift` (+20%) and `append` (+22%) slower: there
  the extra hashing on both sides shows more than the bytes saved.
- **Against rsync:** fastest on 5 of 8 datasets over 100 Mbit (the database 2.9× faster,
  `scatter` 2.7×). rsync stays ahead on `shift` (0.88 s against 0.98 s), `append` (1.49 against
  1.55 s) and `mid` (0.70 against 0.74 s).
- **Against fast_rsync:** faster everywhere except `mid`, where fast_rsync's small signatures
  for 4 MiB files win (0.59 s).
- **Bytes:** barqsync now uploads the least of the four tools on 6 of 8 datasets and ties on
  `append`; only fast_rsync's 2 KiB blocks beat it on `mid`. The other direction grew: its
  signatures are now 0.4–2.0 MB, against 0.06–0.5 MB for rsync, the largest of the four tools
  everywhere but `mid` (where fast_rsync's are).
- fast_rsync's times varied more in this run than in the first (`inplace` on lan 1.80–3.08 s
  against 1.62–1.70 s), most likely the VM's memory behavior again; its byte counts match the
  first run exactly.

### Delta engines, after

In-process, one file pair, median of 3 runs, 4 cores
(`bench/results/fast_rsync-engine-m15-20261018.txt`; barqsync signature and ops sizes are now
the real postcard encoding):

| Dataset | Engine | Unit | Signature | Delta | Total ms |
| --- | --- | --- | ---: | ---: | ---: |
| inplace | fast_rsync | 16 KiB blocks | 197 KB | 8.00 MB | 716 |
| inplace | barqsync | 4 KiB blocks | 1,049 KB | 7.91 MB | **194** |
| scatter | fast_rsync | 16 KiB blocks | 197 KB | 45.33 MB | 1,144 |
| scatter | barqsync | 4 KiB blocks | 1,049 KB | **12.19 MB** | **220** |
| shift | fast_rsync | 16 KiB blocks | 197 KB | 1.26 MB | 608 |
| shift | barqsync | 4 KiB both | 1,970 KB | **1.04 MB** | **331** |
| append | fast_rsync | 16 KiB blocks | 197 KB | 8.39 MB | 1,217 |
| append | barqsync | 4 KiB both | 1,971 KB | 8.39 MB | **345** |
| sqlite | fast_rsync | 15.5 KiB blocks | 191 KB | 103.96 MB | 2,356 |
| sqlite | barqsync | 4 KiB both | 1,825 KB | **27.95 MB** | **407** |
| tar | fast_rsync | 7.2 KiB blocks | 88 KB | 0.86 MB | 145 |
| tar | barqsync | 4 KiB both | 395 KB | **0.85 MB** | **83** |
| csv | fast_rsync | 8.6 KiB blocks | 106 KB | 2.60 MB | 220 |
| csv | barqsync | 4 KiB both | 571 KB | **2.10 MB** | **110** |

barqsync's engine stays 1.7–5.8× faster per file, now with smaller deltas than fast_rsync on
every dataset but `append`, where they are equal.

### Costs and checks

- **Signature size** is the price: 16 bytes per block plus about 20 per chunk, so 1 MB per
  256 MiB for a same-size file and 2 MB for a resized one, computed on both sides. A coarse
  signature first and a second round trip for the blocks that differ would avoid most of it;
  that is listed as open in the as-built plan.
- **Local deltas:** a 1 GiB file with 50 × 1 MiB overwritten, synced locally with
  `--no-whole-file`, took the same time before and after (medians 1.82 and 1.85 s over 6
  alternating runs, VM noise 0.7–4.2 s) with 5% less literal data.
- **The 6 MiB examples** ([implementation.md](implementation.md#delta-transfer), Delta transfer), re-measured on the same files with both builds: 4 KiB
  overwritten 64 KiB → 8 KiB, 777 bytes inserted 57.6 → 8.0 KB, 12 KB removed 344.7 → 4.6 KB,
  appends and truncation unchanged.
- **Tests:** 152 pass (4 new: the sizing rule, chunk-size validation, a grown file with
  scattered edits in-process and over ssh in both directions; the ssh one sends 3.2 MB on the
  old code and fails there), the delta property test also checks that merging never adds
  literal data (4,000 cases run), and `Sig::Both` round-trips through both wire formats. The
  whole suite also passes with rkyv, QUIC, the tokio I/O layer, `--delta=cdc` and the
  adaptive strategy as defaults. With TCP streams as the default, one test failed in 2 of 25
  full runs, identified once as `keep_dirlinks_matches_rsync`; on its own that test failed
  once in 110 runs with this code and never in 30 with the old. It syncs only tiny files,
  which never take the delta path.
- **Compatibility:** 0.1.1 and M15 builds refuse each other with "protocol version mismatch";
  the next release needs a new version number.

## 10. Two-level delta (M16)

M15 fixed scattered edits by making every delta fine, and so made every changed file pay for
it: a 1–2 MB signature per 256 MiB back to the sender and a second hashing pass, which
appends and in-place runs gain nothing from (4–22% slower). M16 makes the cost follow the
change instead of the file:

1. **Coarse first, as in 0.1.1.** The receiver sends 64 KiB block hashes for a same-size file
   and 32 KiB chunk hashes for a resized one; the sender diffs.
2. **Refine what is still literal** (`delta::refine`). Each literal run of the coarse delta is
   a gap; the sender knows where the same stretch sits in the basis, between the same
   matched neighbours. If the gaps hold 64 KiB of basis data or more, it sends `Refine` with
   those stretches and the fine sizes (`Params::for_size`, 4 KiB up to 256 MiB) and hashes its
   own side while the receiver answers `Refined` with:
   - block hashes on the basis's grid, compared at the gap's offset shift (pages that stayed
     in place, or a stretch that moved as a whole);
   - chunk hashes where the two stretches differ in length (an insert or delete inside).
3. **Skipped when it can't pay:** an append's only mismatch is the basis's last chunk (under
   64 KiB), and a file that matched nothing (rewritten, compressed) is sent whole as before.
   Locally there is no round trip, so every gap with a fine block's worth of data is refined.

Protocol version 7; `--delta=cdc` is unchanged.

### Real transfers

Same link, method and verification as section 3; five tools: fast_rsync (`frsync`), barqsync
0.1.1, M15 and M16 (the two older builds installed as `barqsync-0.1.1` and `barqsync-m15` on
both ends), and rsync. 240 transfers, all byte-identical; raw data in
`bench/results/fast_rsync-net-m16-20261018.tsv`. Median of 3 runs (min–max), lowest in bold.

**lan (1 Gbit)**

| Dataset | fast_rsync | 0.1.1 | M15 | M16 | rsync |
| --- | ---: | ---: | ---: | ---: | ---: |
| inplace | 1.78 s (1.73–2.90) | **0.52 s** (0.50–0.54) | 0.55 s (0.52–0.55) | **0.52 s** (0.47–0.56) | 1.08 s (0.77–1.43) |
| scatter | 2.51 s (2.48–3.58) | 1.67 s (1.66–1.79) | 0.68 s (0.64–0.70) | **0.66 s** (0.63–0.70) | 1.04 s (0.99–1.11) |
| shift | 2.35 s (1.50–2.61) | 0.59 s (0.53–0.59) | 0.66 s (0.64–0.69) | **0.53 s** (0.50–0.54) | 0.59 s (0.56–0.61) |
| append | 1.75 s (1.73–2.56) | **0.58 s** (0.57–0.62) | 0.70 s (0.67–0.75) | **0.58 s** (0.56–0.59) | 0.84 s (0.80–0.90) |
| mid | **0.41 s** (0.39–0.41) | 0.46 s (0.44–0.49) | 0.47 s (0.45–0.48) | 0.45 s (0.43–0.46) | **0.41 s** (0.39–0.41) |
| sqlite | 3.72 s (3.68–3.76) | 2.07 s (2.02–2.08) | 0.89 s (0.89–0.91) | **0.88 s** (0.87–0.88) | 2.02 s (1.80–2.51) |
| tar | 0.52 s (0.51–0.54) | 0.40 s (0.39–0.56) | 0.36 s (0.34–0.36) | **0.32 s** (0.32–0.35) | 0.36 s (0.34–0.36) |
| csv | 0.70 s (0.69–0.72) | 0.50 s (0.45–0.51) | **0.38 s** (0.35–0.40) | **0.38 s** (0.35–0.38) | 0.39 s (0.36–0.42) |

**wan (100 Mbit, 20 ms RTT)**

| Dataset | fast_rsync | 0.1.1 | M15 | M16 | rsync |
| --- | ---: | ---: | ---: | ---: | ---: |
| inplace | 2.36 s (2.35–2.40) | **1.24 s** (1.23–1.25) | 1.30 s (1.28–1.31) | **1.24 s** (1.21–1.24) | 1.37 s (1.36–1.41) |
| scatter | 6.13 s (5.89–7.16) | 12.38 s (12.36–12.40) | 1.64 s (1.63–1.66) | **1.63 s** (1.62–1.68) | 4.46 s (4.43–4.47) |
| shift | 1.99 s (1.85–2.44) | 1.00 s (0.97–1.01) | 0.99 s (0.97–1.02) | **0.75 s** (0.74–0.78) | 0.84 s (0.82–0.87) |
| append | 2.49 s (2.44–3.16) | 1.30 s (1.30–1.32) | 1.56 s (1.55–1.60) | **1.29 s** (1.29–1.32) | 1.50 s (1.46–1.51) |
| mid | **0.58 s** (0.58–0.59) | 0.72 s (0.70–0.73) | 0.72 s (0.71–0.73) | 0.73 s (0.72–0.75) | 0.69 s (0.69–0.72) |
| sqlite | 12.14 s (11.77–12.18) | 16.71 s (16.69–16.72) | **3.17 s** (3.14–3.21) | 3.23 s (3.22–3.24) | 9.38 s (9.36–9.46) |
| tar | 0.75 s (0.74–0.78) | 0.79 s (0.78–0.83) | 0.58 s (0.57–0.60) | **0.56 s** (0.54–0.56) | 0.63 s (0.62–0.65) |
| csv | 1.03 s (1.03–1.03) | 1.67 s (1.66–1.67) | 0.73 s (0.71–0.74) | **0.66 s** (0.66–0.68) | 0.80 s (0.77–0.80) |

**Bytes on the link**, median over all runs, up (client to server) / down:

| Dataset | fast_rsync | 0.1.1 | M15 | M16 | rsync |
| --- | ---: | ---: | ---: | ---: | ---: |
| inplace | 8.02 / 0.22 MB | 8.35 / 0.09 MB | 7.94 / 1.07 MB | **7.93** / 0.12 MB | 8.10 / 0.15 MB |
| scatter | 45.44 / 0.30 MB | 141.78 / 0.38 MB | 12.27 / 1.08 MB | 12.28 / 0.66 MB | 45.54 / 0.23 MB |
| shift | 1.27 / 0.21 MB | 4.63 / 0.16 MB | 1.06 / 1.98 MB | **0.96** / 0.17 MB | 1.33 / 0.13 MB |
| append | 8.42 / 0.22 MB | 8.45 / 0.17 MB | 8.42 / 2.00 MB | 8.45 / 0.15 MB | 8.50 / 0.14 MB |
| mid | **0.14** / 1.00 MB | 2.64 / 0.05 MB | 0.22 / 0.66 MB | 0.23 / 0.06 MB | 0.46 / 0.50 MB |
| sqlite | 104.22 / 0.40 MB | 192.82 / 0.55 MB | **28.12** / 1.89 MB | 30.05 / 0.94 MB | 104.34 / 0.36 MB |
| tar | 0.87 / 0.10 MB | 3.84 / 0.04 MB | 0.86 / 0.40 MB | **0.73** / 0.06 MB | 0.90 / 0.06 MB |
| csv | 2.62 / 0.12 MB | 14.18 / 0.08 MB | 2.11 / 0.58 MB | **1.77** / 0.15 MB | 2.65 / 0.07 MB |

What this shows:

- **Against 0.1.1:** never slower beyond run-to-run noise (`inplace` 1.24 = 1.24 s, `append`
  1.30 → 1.29 s, `mid` 0.72 → 0.73 s over 100 Mbit), and 1.3–7.6× faster on the rest: `shift`
  1.00 → 0.75 s, `tar` 0.79 → 0.56 s, `csv` 1.67 → 0.66 s, `sqlite` 16.71 → 3.23 s, `scatter`
  12.38 → 1.63 s.
- **Against M15:** the append and in-place costs are gone (`append` 1.56 → 1.29 s), shifted
  data got faster (0.99 → 0.75 s: fine chunks for the gaps only, not 2 MB of signatures), and
  the database is within 2% (3.17 → 3.23 s; equal-length gaps get blocks only, 30.1 MB
  against M15's merged 28.1 MB).
- **Against rsync:** faster on 7 of 8 datasets over 100 Mbit and on 7 of 8 over 1 Gbit. The
  exception is `mid`, 40 files of 4 MiB with 1 KiB changed each: 0.73 against 0.69 s (and
  fast_rsync's 0.58 s). Each such file differs in one coarse block, at the 64 KiB threshold,
  so it pays a round trip to save 60 KiB; per-file overheads dominate there.
- **Signatures** are back near 0.1.1's size: 0.06–0.94 MB down (0.1.1: 0.04–0.55, M15:
  0.4–2.0, rsync: 0.06–0.5).
- Every run-to-run outlier in these runs (about 1 in 10 on the LAN, up to 2× the median) hit
  all tools alike, 0.1.1 and rsync included: VM noise.

### Delta engines, M16

In-process, one file pair, median of 3 runs, 4 cores
(`bench/results/fast_rsync-engine-m16-20261018.txt`). barqsync's signature column includes the
refinement request and answer:

| Dataset | Engine | Signature | Delta | Total ms |
| --- | --- | ---: | ---: | ---: |
| inplace | fast_rsync | 197 KB | 8.00 MB | 705 |
| inplace | barqsync (blocks + refine) | 98 KB | 7.91 MB | **173** |
| scatter | fast_rsync | 197 KB | 45.33 MB | 1,075 |
| scatter | barqsync (blocks + refine) | 628 KB | **12.19 MB** | **252** |
| shift | fast_rsync | 197 KB | 1.26 MB | 654 |
| shift | barqsync (chunks + refine) | 158 KB | **0.95 MB** | **246** |
| append | fast_rsync | 197 KB | 8.39 MB | 745 |
| append | barqsync (chunks) | 122 KB | 8.42 MB | **256** |
| sqlite | fast_rsync | 191 KB | 103.96 MB | 2,062 |
| sqlite | barqsync (chunks + refine) | 879 KB | **29.88 MB** | **380** |
| tar | fast_rsync | 88 KB | 0.86 MB | 136 |
| tar | barqsync (chunks + refine) | 49 KB | **0.72 MB** | **68** |
| csv | fast_rsync | 106 KB | 2.60 MB | 204 |
| csv | barqsync (chunks + refine) | 145 KB | **1.75 MB** | **106** |

barqsync's engine is 1.9–5.4× faster per file, with smaller deltas than fast_rsync everywhere
but `append` (equal), and signatures of the same order as fast_rsync's.

### Checks

- **6 MiB examples** (local `--no-whole-file`, same files for both builds): 4 KiB overwritten
  64 KiB → 8 KiB, 777 bytes inserted 57.6 → 8.0 KB, 12 KB removed 344.7 → 4.6 KB; appends and
  truncation unchanged.
- **Tests:** 157 pass, also with rkyv, TCP streams, QUIC, the tokio I/O layer, `--delta=cdc`
  and the adaptive strategy as defaults. New: a refinement property test (every refined delta
  rebuilds the file and has no more literal data than the coarse one; 3,000 cases), gap
  pairing, request validation and malformed answers, an append and a rewritten file not
  refined, a grown file with scattered edits, `Refine`/`Refined` in both wire formats.
- **The rare test failure** of section 9 was a race in the test fixture, not in barqsync.
  `keep_dirlinks_matches_rsync` (and three tests built the same way) created
  destination-only files once per tool and compared their mtimes, which differed whenever a
  second boundary fell between creating the two trees. With a 1.1 s pause between them it
  failed every time; with `Tree::same_mtimes` giving those files one time it passes, and 400
  parallel runs went from 1 failure to 0.
- **macOS:** the test suite passes there too (154 tests on APFS; three tests are
  Linux-only). The transfer benchmarks are from the Linux VM only: the Mac is on a Tailscale
  network the VM can't reach.

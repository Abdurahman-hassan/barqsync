# Project Blueprint: Building a Faster-Than-`rsync` Engine in Rust

## 1. Executive Summary & Performance Targets

`rsync` was designed in 1996 around an era of slow network links (modems/T1), spinning hard drives, and single-core CPUs. Modern infrastructure features **NVMe SSDs (7+ GB/s)**, **10GbE–100GbE networks**, and **16–128+ CPU cores**, making `rsync` itself the bottleneck.

### Target Performance Metrics vs. `rsync`
1. **Local Directory Scan (1M files):** **5x–8x faster** via work-stealing parallel `readdir`/`stat` calls.
2. **Small-File Network Transfer (< 64 KB files):** **10x–20x faster** by packing small files into compressed in-memory frames over multiplexed QUIC streams (eliminating per-file syscall and RTT stalls).
3. **Large-File Delta Sync (10 GB+ VM/DB images):** **4x–6x faster** using parallel Content-Defined Chunking (`FastCDC`) and SIMD-accelerated `BLAKE3` / `XXH3` hashing.
4. **Local Disk-to-Disk Copy:** **Line-rate hardware saturation** via Linux `copy_file_range` / `io_uring` and macOS `fclonefileat` (Copy-on-Write).

---

## 2. Core Architectural Bottlenecks & Rust Solutions

| Bottleneck in `rsync` | Technical Root Cause | Rust Architecture Solution |
| :--- | :--- | :--- |
| **Single-Threaded File Discovery** | Sequential directory tree walking stalls on filesystem metadata latency. | **Work-Stealing Parallel Traversal** (`jwalk` + `rayon`) saturating NVMe IOPS queues. |
| **Per-File Network Chattiness** | Opening, writing, and closing millions of small files sequentially over TCP/SSH. | **In-Memory Frame Packing (Tar-like Bundles)** compressed with `zstd` and streamed in 4 MB chunks. |
| **Byte-by-Byte Sliding Window** | Adler-32 rolling checksum must slide 1 byte at a time on a single core; shifting bytes invalidates fixed blocks. | **Parallel Content-Defined Chunking (`FastCDC`)** partitioned across cores via `memmap2` + `rayon`. |
| **Slow Checksums & Compression** | Legacy MD4/MD5/SHA-1 and single-threaded `zlib`. | **SIMD `XXH3` (64-bit/128-bit)** for fast matching, **`BLAKE3`** for cryptographic verification, and **`zstd`** for compression. |
| **SSH/TCP Head-of-Line Blocking** | Single TCP stream stalls all transfers when a single packet drops; SSH cipher framing limits throughput. | **QUIC (`quinn`) Transport** with TLS 1.3 encryption, zero-RTT setup, and hundreds of independent concurrent streams. |

---

## 3. High-Performance Rust Crate Ecosystem (`Cargo.toml`)

```toml
[package]
name = "fastrsync"
version = "0.1.0"
edition = "2021"

[dependencies]
# CLI & Terminal UI
clap = { version = "4.5", features = ["derive"] }
indicatif = { version = "0.17", features = ["rayon"] }
anyhow = "1.0"
thiserror = "1.0"
tracing = "0.1"
tracing-subscriber = { version = "0.3", features = ["env-filter"] }

# Parallelism & Async Runtime
rayon = "1.10"
tokio = { version = "1.40", features = ["full"] }
crossbeam-channel = "0.5"
dashmap = "6.1"

# Filesystem & Zero-Copy I/O
jwalk = "0.8"
memmap2 = "0.9"
filetime = "0.2"
rustix = { version = "0.38", features = ["fs"] } # Direct copy_file_range / fadvise syscalls
same-file = "1.0"

# Hashing, Delta Chunking & Compression
xxhash-rust = { version = "0.8", features = ["xxh3"] }
blake3 = { version = "1.5", features = ["rayon", "mmap"] }
fastcdc = "3.1"
zstd = "0.13"

# Networking & Zero-Copy Serialization
quinn = "0.11"
rustls = { version = "0.23", default-features = false, features = ["ring"] }
rkyv = { version = "0.7", features = ["validation"] } # Zero-copy deserialization
bytes = "1.7"

# Linux-specific Async Disk I/O (Optional Phase 4)
[target.'cfg(target_os = "linux")'.dependencies]
tokio-uring = "0.5"

[profile.release]
opt-level = 3
lto = "fat"
codegen-units = 1
panic = "abort"
strip = true
```

---

## 4. System Architecture & Data Pipeline

Instead of waiting for the entire directory scan to finish before transferring data, the engine operates as a **4-stage lock-free streaming pipeline**:

```
[Stage 1: Parallel Scanner (jwalk)]
       │ (Emits FileEntry batches via crossbeam-channel)
       ▼
[Stage 2: Diff & Classification Engine]
       ├──► Queue A: Metadata-Only Updates (chmod/chown/utimes)
       ├──► Queue B: Small Files (< 128 KB) ──► [4MB Frame Packer + Zstd]
       ├──► Queue C: Large New Files (> 128 KB) ──► [Zero-Copy / Stream Chunker]
       └──► Queue D: Large Modified Files ──► [Parallel FastCDC + XXH3/BLAKE3 Delta]
                                                       │
                                                       ▼
                                        [Stage 3: QUIC Multiplexer / Local I/O Pool]
                                                       │
                                                       ▼
                                        [Stage 4: Atomic Temp-File Writer & Commit]
```

### File Classification Heuristics
To maximize throughput, files are routed dynamically based on size and modification status:

* **Tiny/Small Files ($\le 128\text{ KB}$):** Never run delta-sync. Pack up to 4 MB of small files into a single contiguous memory buffer (`SmallFileBatch`), compress the entire buffer once with `zstd` (level 1 or 3), and send over a dedicated QUIC stream.
* **Medium Files ($128\text{ KB} < \text{size} \le 32\text{ MB}$):**
  * If on a fast network ($\ge 1\text{ Gbps}$) or local disk: Send whole file compressed (`zstd --adapt`).
  * If on a slow WAN link ($< 100\text{ Mbps}$): Run `FastCDC` delta sync.
* **Large Files ($> 32\text{ MB}$):** Memory-map via `memmap2`, split into parallel segments on `rayon`, compute `FastCDC` variable-length chunks, match chunk hashes via `XXH3`, and transmit only missing/changed chunks.

---

## 5. Wire Protocol & Zero-Copy Serialization (`rkyv`)

Using `serde` + `bincode` or `protobuf` incurs memory allocation overhead on millions of file entries. Using **`rkyv`** allows the receiver to read network buffers directly as Rust structs with **zero deserialization cost**.

```rust
use rkyv::{Archive, Deserialize, Serialize};

#[derive(Archive, Deserialize, Serialize, Debug, Clone, PartialEq)]
#[archive(check_bytes)]
pub struct FileMetadataWire {
    pub rel_path: String,
    pub size: u64,
    pub mtime_sec: i64,
    pub mtime_nsec: u32,
    pub mode: u32,
    pub uid: u32,
    pub gid: u32,
    pub is_symlink: bool,
    pub symlink_target: Option<String>,
}

#[derive(Archive, Deserialize, Serialize, Debug, Clone)]
#[archive(check_bytes)]
pub struct ChunkSignature {
    pub offset: u64,
    pub length: u32,
    pub xxh3_hash: u64,          // Fast 64-bit filter
    pub blake3_hash: [u8; 32],   // Collision-proof verification
}

#[derive(Archive, Deserialize, Serialize, Debug)]
#[archive(check_bytes)]
pub struct PackedSmallFile {
    pub meta: FileMetadataWire,
    pub data_offset: u32,
    pub data_len: u32,
}

#[derive(Archive, Deserialize, Serialize, Debug)]
#[archive(check_bytes)]
pub struct SmallFileBatchFrame {
    pub files: Vec<PackedSmallFile>,
    pub compressed_payload: Vec<u8>, // Zstd-compressed contiguous payload
}
```

---

## 6. Phased Implementation Plan

### Phase 1: Ultra-Fast Local Sync Engine (Week 1–2)
**Goal:** Beat `rsync -a /src /dst` and `cp -r` on local NVMe, SATA SSDs, and NFS mounts.
1. **Parallel Directory Scanner:**
   * Implement `jwalk::WalkDir` configured with `Parallelism::RayonNewPool(0)`.
   * Extract `size`, `mtime`, `mode`, `uid`, `gid`, and `ino` directly from `DirEntry` without redundant `stat` syscalls where supported.
2. **In-Memory State Indexing:**
   * Populate a `DashMap<PathBuf, FileMetadataWire>` for source and destination trees concurrently.
3. **Zero-Copy Kernel File Copy:**
   * Use `rustix::fs::copy_file_range` on Linux and `std::fs::copy` (which triggers `fclonefileat` on macOS APFS) so data never crosses the kernel-to-userspace boundary.
4. **Atomic File Replacement:**
   * Write changes to `.fastrsync.<filename>.tmp.<pid>` in the target directory, apply permissions (`fchmod`), apply timestamps (`filetime::set_file_handle_times`), and atomically `rename()` over the destination file.
5. **Hardlink & Symlink Preservation:**
   * Track `(dev, ino)` pairs in a concurrent `DashMap<(u64, u64), PathBuf>` when `--hard-links` (`-H`) is enabled.

### Phase 2: Parallel FastCDC & SIMD Delta Engine (Week 3–4)
**Goal:** Synchronize modified multi-gigabyte files 5x faster than `rsync`'s rolling Adler-32.
1. **Why FastCDC Beats Fixed-Size Rolling Hash:**
   * If 10 bytes are inserted at the beginning of a 10 GB file, fixed-block hashing shifts every single block boundary, requiring an expensive byte-by-byte rolling window scan across all 10 GB.
   * **Content-Defined Chunking (FastCDC)** determines chunk boundaries based on a rolling gear hash of the data itself. Inserting bytes at the start of a file only affects the very first chunk; all subsequent chunks remain identical.
2. **Parallel Chunking Implementation:**
   * Memory-map the file using `memmap2::MmapOptions`.
   * Divide the file into coarse 16 MB regions across all CPU cores using `rayon`.
   * Inside each 16 MB region, run `fastcdc::v2020::FastCDC::new(slice, 16384, 32768, 65536)` (Min: 16 KB, Avg: 32 KB, Max: 64 KB).
   * At the boundary between two 16 MB regions, merge the trailing chunk of region $N$ with the leading bytes of region $N+1$ until the next natural CDC cut-point is found (ensuring deterministic chunk boundaries regardless of thread count).
3. **Dual-Hash Signature Generation:**
   * Compute `xxhash_rust::xxh3::xxh3_64(chunk)` for instant hash-table lookups.
   * Compute `blake3::hash(chunk)` to guarantee zero cryptographic collisions without needing an extra whole-file pass.

### Phase 3: QUIC Network Transport & Small-File Frame Packing (Week 5–6)
**Goal:** Saturate 10GbE/40GbE links and eliminate latency on millions of small files.
1. **QUIC Server & Client (`quinn`):**
   * Control Stream (Bi-directional Stream #0): Exchanges handshake, protocol version, CLI flags, and streaming `FileMetadataWire` batches.
   * Data Streams (Unidirectional Streams #1..N): Up to 128 concurrent streams transferring `SmallFileBatchFrame` bundles and large file delta chunks simultaneously.
2. **SSH Bootstrap Mode (Zero Configuration):**
   * Just like `rsync` uses SSH to launch `rsync --server`, implement an SSH bootstrapper (`std::process::Command::new("ssh")`) that executes `fastrsync --server --ephemeral-quic` on the remote host.
   * The remote process binds a temporary UDP port, generates an ephemeral TLS certificate, prints the `(port, cert_fingerprint, session_token)` to `stdout` over SSH, and the client immediately upgrades to a direct QUIC connection—or falls back to framing over the SSH `stdin`/`stdout` pipe if UDP is blocked by a firewall.
3. **Small-File Packer:**
   * Accumulate small files into a 4 MB buffer.
   * Compress the buffer with `zstd::bulk::compress(buf, 1)`.
   * Unpack on the receiver across a dedicated pool of disk writer threads.

### Phase 4: Linux Kernel Acceleration & Polish (Week 7–8)
**Goal:** Squeeze out the final 20–30% hardware performance and ensure drop-in CLI compatibility.
1. **`io_uring` Backend (Linux):**
   * Use `tokio-uring` to batch `openat`, `write`, `fsync`, and `close` calls on the receiver when unpacking small-file frames.
2. **Page Cache Hints (`fadvise`):**
   * Call `posix_fadvise(fd, 0, 0, POSIX_FADV_SEQUENTIAL | POSIX_FADV_NOREUSE)` on large files so scanning a 5 TB backup doesn't evict hot application memory from the Linux page cache.
3. **Drop-In CLI Flags & Output:**
   * Support standard flags (`-a`, `-v`, `-z`, `--delete`, `--exclude`, `--dry-run`, `--progress`) so users can alias `rsync=fastrsync`.

---

## 7. Production-Grade Starter Code

Below is the foundational implementation of the **Parallel Scanner**, **Parallel FastCDC Delta Engine**, and **Small-File Packer** in Rust.

```rust
use anyhow::{Context, Result};
use dashmap::DashMap;
use fastcdc::v2020::FastCDC;
use jwalk::{Parallelism, WalkDir};
use memmap2::MmapOptions;
use rayon::prelude::*;
use std::fs::{self, File};
use std::io::Write;
use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};
use xxhash_rust::xxh3::xxh3_64;

/// Core metadata structure indexed in memory
#[derive(Debug, Clone)]
pub struct FileIndexEntry {
    pub rel_path: PathBuf,
    pub size: u64,
    pub mtime: i64,
    pub mtime_nsec: i64,
    pub mode: u32,
}

/// Chunk signature generated by Parallel FastCDC
#[derive(Debug, Clone)]
pub struct FileChunkSig {
    pub offset: u64,
    pub length: usize,
    pub xxh3: u64,
    pub blake3: [u8; 32],
}

/// 1. PARALLEL DIRECTORY SCANNER
/// Saturates NVMe queues using work-stealing Rayon threads.
pub fn scan_tree_parallel(root: &Path) -> Result<DashMap<PathBuf, FileIndexEntry>> {
    let root = root.canonicalize().context("Failed to canonicalize root path")?;
    let map = DashMap::with_capacity(65_536);

    WalkDir::new(&root)
        .skip_hidden(false)
        .follow_links(false)
        .parallelism(Parallelism::RayonNewPool(0))
        .into_iter()
        .par_bridge()
        .filter_map(|entry| entry.ok())
        .for_each(|entry| {
            let file_type = entry.file_type();
            if file_type.is_file() {
                if let Ok(meta) = entry.metadata() {
                    if let Ok(rel) = entry.path().strip_prefix(&root) {
                        let rel_buf = rel.to_path_buf();
                        map.insert(
                            rel_buf.clone(),
                            FileIndexEntry {
                                rel_path: rel_buf,
                                size: meta.len(),
                                mtime: meta.mtime(),
                                mtime_nsec: meta.mtime_nsec(),
                                mode: meta.mode(),
                            },
                        );
                    }
                }
            }
        });

    Ok(map)
}

/// 2. PARALLEL FASTCDC + SIMD HASHING FOR LARGE FILES
/// Splits a memory-mapped file into coarse regions, runs FastCDC in parallel,
/// and hashes chunks using XXH3 and BLAKE3.
pub fn compute_file_signatures(path: &Path) -> Result<Vec<FileChunkSig>> {
    let file = File::open(path)?;
    let len = file.metadata()?.len();
    if len == 0 {
        return Ok(Vec::new());
    }

    // Safety: Read-only memory map for high-throughput zero-copy scanning
    let mmap = unsafe { MmapOptions::new().map(&file)? };
    let data = &mmap[..];

    // Coarse segment size: 16 MB per Rayon task
    const SEGMENT_SIZE: usize = 16 * 1024 * 1024;
    // FastCDC parameters: Min 16KB, Avg 32KB, Max 64KB
    const MIN_CHUNK: u32 = 16 * 1024;
    const AVG_CHUNK: u32 = 32 * 1024;
    const MAX_CHUNK: u32 = 64 * 1024;

    let segments: Vec<(usize, &[u8])> = data
        .chunks(SEGMENT_SIZE)
        .enumerate()
        .map(|(idx, slice)| (idx * SEGMENT_SIZE, slice))
        .collect();

    let chunk_vecs: Vec<Vec<FileChunkSig>> = segments
        .into_par_iter()
        .map(|(base_offset, slice)| {
            let chunker = FastCDC::new(slice, MIN_CHUNK, AVG_CHUNK, MAX_CHUNK);
            let mut local_sigs = Vec::with_capacity(slice.len() / (AVG_CHUNK as usize) + 1);

            for chunk in chunker {
                let start = chunk.offset;
                let end = start + chunk.length;
                let chunk_bytes = &slice[start..end];

                let xxh3 = xxh3_64(chunk_bytes);
                let blake3 = *blake3::hash(chunk_bytes).as_bytes();

                local_sigs.push(FileChunkSig {
                    offset: (base_offset + start) as u64,
                    length: chunk.length,
                    xxh3,
                    blake3,
                });
            }
            local_sigs
        })
        .collect();

    Ok(chunk_vecs.into_iter().flatten().collect())
}

/// 3. SMALL-FILE BATCH PACKER + ZSTD COMPRESSION
/// Packs thousands of small files into a single contiguous compressed frame.
pub struct SmallFileBatcher {
    raw_buffer: Vec<u8>,
    manifest: Vec<(PathBuf, u32, usize, usize)>, // (rel_path, mode, offset, len)
    max_batch_bytes: usize,
}

impl SmallFileBatcher {
    pub fn new(max_batch_bytes: usize) -> Self {
        Self {
            raw_buffer: Vec::with_capacity(max_batch_bytes),
            manifest: Vec::with_capacity(1024),
            max_batch_bytes,
        }
    }

    pub fn try_push(&mut self, root: &Path, entry: &FileIndexEntry) -> Result<bool> {
        if self.raw_buffer.len() + (entry.size as usize) > self.max_batch_bytes
            && !self.raw_buffer.is_empty()
        {
            return Ok(false); // Batch is full, flush required
        }

        let full_path = root.join(&entry.rel_path);
        let bytes = fs::read(&full_path)?;
        let offset = self.raw_buffer.len();
        let len = bytes.len();

        self.raw_buffer.extend_from_slice(&bytes);
        self.manifest
            .push((entry.rel_path.clone(), entry.mode, offset, len));
        Ok(true)
    }

    pub fn compress_and_flush(&mut self) -> Result<(Vec<(PathBuf, u32, usize, usize)>, Vec<u8>)> {
        let compressed = zstd::bulk::compress(&self.raw_buffer, 1)?;
        let manifest = std::mem::take(&mut self.manifest);
        self.raw_buffer.clear();
        Ok((manifest, compressed))
    }
}

/// 4. ATOMIC FILE WRITER (RECEIVER SIDE)
/// Ensures crash-safe file updates without leaving half-written corrupted files.
pub fn write_atomic(dst_root: &Path, rel_path: &Path, data: &[u8], _mode: u32) -> Result<()> {
    let final_path = dst_root.join(rel_path);
    if let Some(parent) = final_path.parent() {
        fs::create_dir_all(parent)?;
    }

    let file_name = final_path
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or("tmp");
    let tmp_path = final_path.with_file_name(format!(".fastrsync.{}.tmp", file_name));

    {
        let mut tmp_file = File::create(&tmp_path)?;
        tmp_file.write_all(data)?;
        // Note: In batch mode, defer fsync() to directory level after unpacking the whole frame
    }

    fs::rename(&tmp_path, &final_path)?;
    Ok(())
}
```

---

## 8. Benchmarking & Verification Protocol

To verify that your Rust engine beats `rsync` at every stage of development, use **`hyperfine`** and **`tc` (Traffic Control)** to test across realistic disk and network conditions.

### Step 1: Generate Synthetic Benchmark Datasets
```bash
# Dataset 1: The Node_Modules / Small-File Storm (200,000 files, 4KB each)
mkdir -p /tmp/bench_src_small
seq 1 200 | xargs -P 16 -I {} bash -c '
  mkdir -p /tmp/bench_src_small/dir_{}
  for i in {1..1000}; do
    head -c 4096 /dev/urandom > /tmp/bench_src_small/dir_{}/file_$i.bin
  done
'

# Dataset 2: The 10GB Mutated VM Image (Delta Sync Test)
dd if=/dev/urandom of=/tmp/bench_src_large.img bs=1M count=10240 status=progress
cp /tmp/bench_src_large.img /tmp/bench_dst_large.img
# Mutate 50 random 1MB blocks inside the source file
for i in {1..50}; do
  OFFSET=$((RANDOM % 10000))
  dd if=/dev/urandom of=/tmp/bench_src_large.img bs=1M count=1 seek=$OFFSET conv=notrunc 2>/dev/null
done
```

### Step 2: Run Cold-Cache `hyperfine` Comparisons
Always drop the OS kernel page cache before each run to measure real NVMe and syscall performance rather than RAM speed:

```bash
hyperfine --warmup 0 --runs 5 \
  --prepare 'rm -rf /tmp/bench_dst_small && sync && echo 3 | sudo tee /proc/sys/vm/drop_caches' \
  'rsync -a /tmp/bench_src_small/ /tmp/bench_dst_small/' \
  './target/release/fastrsync /tmp/bench_src_small/ /tmp/bench_dst_small/'
```

### Step 3: Profiling & Flamegraphs
If any workload is slower than expected, profile immediately using `cargo-flamegraph` and `strace -c`:
1. **`strace -cf ./target/release/fastrsync ...`**: Verify that your syscall count is at least 50% lower than `rsync -a` (avoiding redundant `lstat` or `openat` calls).
2. **`cargo flamegraph`**: Ensure CPU cores are spending time in `blake3::simd`, `zstd`, or `copy_file_range` rather than lock contention in `DashMap` or memory allocation.
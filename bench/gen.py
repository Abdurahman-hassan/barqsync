#!/usr/bin/env python3
"""Generate benchmark datasets (macOS + Linux).

  A  small-file storm:  200 dirs x 1000 files x 4 KiB          (~800 MiB)
  B  big blob:          one 10 GiB file + a copy with 50 x 1 MiB overwrites
  C  many files:        5000 dirs x 100 files x 1 KiB          (~500 MiB, 500k files)
  E  shifted blob:      one 2 GiB file + a copy with 40 random inserts/deletes
                        of 1-64 KiB (content moves, so aligned blocks stop matching)

Usage: bench/gen.py BENCH_DIR [A] [B] [C] [E] [--scale 0.1]
File contents are random so compression and dedup can't cheat.
"""

import argparse
import os
import random
import shutil
import sys
from concurrent.futures import ThreadPoolExecutor


def make_tree(root, dirs, files_per_dir, size):
    if os.path.isdir(root):
        print(f"  {root} exists, skipping")
        return
    tmp = root + ".partial"
    shutil.rmtree(tmp, ignore_errors=True)
    os.makedirs(tmp)

    def one_dir(d):
        dpath = os.path.join(tmp, f"dir_{d:05d}")
        os.makedirs(dpath)
        blob = os.urandom(size * files_per_dir)
        for i in range(files_per_dir):
            with open(os.path.join(dpath, f"file_{i:05d}.bin"), "wb") as f:
                f.write(blob[i * size:(i + 1) * size])

    with ThreadPoolExecutor(max_workers=os.cpu_count()) as ex:
        list(ex.map(one_dir, range(dirs)))
    os.rename(tmp, root)


def make_blob(base, size, mutations):
    src, dst = base + "_src.img", base + "_dst.img"
    if os.path.exists(src) and os.path.exists(dst):
        print(f"  {src} exists, skipping")
        return
    chunk = 64 << 20
    with open(dst, "wb") as f:
        left = size
        while left > 0:
            n = min(chunk, left)
            f.write(os.urandom(n))
            left -= n
    # Source = destination with scattered 1 MiB overwrites (same size, in place).
    shutil.copyfile(dst, src)
    rng = random.Random(42)
    with open(src, "r+b") as f:
        for _ in range(mutations):
            f.seek(rng.randrange(0, size - (1 << 20)))
            f.write(os.urandom(1 << 20))


def make_shifted(base, size, edits):
    src, dst = base + "_src.img", base + "_dst.img"
    if os.path.exists(src) and os.path.exists(dst):
        print(f"  {src} exists, skipping")
        return
    chunk = 64 << 20
    with open(dst, "wb") as f:
        left = size
        while left > 0:
            n = min(chunk, left)
            f.write(os.urandom(n))
            left -= n
    # Source = destination with random inserts and deletes, streamed in order.
    rng = random.Random(43)
    points = sorted((rng.randrange(0, size - (64 << 10)), rng.random() < 0.5, rng.randrange(1 << 10, 64 << 10))
                    for _ in range(edits))
    with open(dst, "rb") as fin, open(src, "wb") as fout:
        pos = 0
        for at, insert, n in points:
            if at < pos:
                continue
            while pos < at:
                b = fin.read(min(chunk, at - pos))
                fout.write(b)
                pos += len(b)
            if insert:
                fout.write(os.urandom(n))
            else:
                fin.seek(n, os.SEEK_CUR)
                pos += n
        shutil.copyfileobj(fin, fout, chunk)


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("dir")
    ap.add_argument("sets", nargs="*", default=["A", "C"])
    ap.add_argument("--scale", type=float, default=1.0, help="shrink datasets for quick runs")
    a = ap.parse_args()
    os.makedirs(a.dir, exist_ok=True)
    s = a.scale
    if "A" in a.sets:
        print("A: small-file storm")
        make_tree(os.path.join(a.dir, "A_src"), max(1, int(200 * s)), 1000, 4096)
    if "B" in a.sets:
        print("B: big blob")
        make_blob(os.path.join(a.dir, "B"), int((10 << 30) * s), 50)
    if "C" in a.sets:
        print("C: many files")
        make_tree(os.path.join(a.dir, "C_src"), max(1, int(5000 * s)), 100, 1024)
    if "E" in a.sets:
        print("E: shifted blob")
        make_shifted(os.path.join(a.dir, "E"), int((2 << 30) * s), 40)
    return 0


if __name__ == "__main__":
    sys.exit(main())

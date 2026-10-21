# Datasets for the fast_rsync comparison (docs/fast-rsync-comparison.md), each an old/ and a new/
# directory: inplace, shift, append, mid, sqlite (the first comparison), and tar, csv,
# scatter (added to choose barqsync's block and chunk sizes). Seeded, so reruns produce
# identical files.
#   python3 -I gen.py OUT_DIR
import os, random, sqlite3, shutil, sys
W = sys.argv[1]
MiB = 1 << 20

def pair(name):
    o, n = f"{W}/{name}/old", f"{W}/{name}/new"
    os.makedirs(o); os.makedirs(n)
    return o, n

def rnd(r, size):
    return bytearray(b"".join(r.randbytes(MiB) for _ in range(size // MiB)) + r.randbytes(size % MiB))

# 1. inplace: 256 MiB, 8 x 1 MiB overwritten in place (VM image / database style)
r = random.Random(1); o, n = pair("inplace")
d = rnd(r, 256 * MiB); open(f"{o}/disk.img", "wb").write(d)
for _ in range(8):
    off = r.randrange(0, len(d) - MiB); d[off:off + MiB] = r.randbytes(MiB)
open(f"{n}/disk.img", "wb").write(d)

# 2. shift: 256 MiB, 40 inserts/deletes of 1-64 KiB at random offsets (shifts later data)
r = random.Random(2); o, n = pair("shift")
d = rnd(r, 256 * MiB); open(f"{o}/blob.bin", "wb").write(d)
for i in range(40):
    off = r.randrange(0, len(d)); k = r.randint(1024, 64 * 1024)
    if i % 2: d[off:off] = r.randbytes(k)
    else: del d[off:off + k]
open(f"{n}/blob.bin", "wb").write(d)

# 3. append: 256 MiB log, 8 MiB appended
r = random.Random(3); o, n = pair("append")
d = rnd(r, 256 * MiB); open(f"{o}/app.log", "wb").write(d)
d += r.randbytes(8 * MiB); open(f"{n}/app.log", "wb").write(d)

# 4. mid: 40 files of 4 MiB, 1 KiB changed in each
r = random.Random(4); o, n = pair("mid")
for i in range(40):
    d = bytearray(r.randbytes(4 * MiB)); open(f"{o}/m{i:03d}.bin", "wb").write(d)
    off = r.randrange(0, 4 * MiB - 1024); d[off:off + 1024] = r.randbytes(1024)
    open(f"{n}/m{i:03d}.bin", "wb").write(d)

# 5. sqlite: ~200 MiB database, then 2,000 updates, 5,000 inserts, 1,000 deletes
r = random.Random(5); o, n = pair("sqlite")
db = f"{o}/app.db"
c = sqlite3.connect(db)
c.execute("PRAGMA journal_mode=DELETE"); c.execute("PRAGMA page_size=4096")
c.execute("CREATE TABLE users(id INTEGER PRIMARY KEY, name TEXT, email TEXT, bio TEXT, avatar BLOB)")
c.execute("CREATE INDEX users_email ON users(email)")
words = [''.join(r.choice('abcdefghijklmnopqrstuvwxyz') for _ in range(r.randint(3, 10))) for _ in range(5000)]
def row(i):
    name = f"{r.choice(words)} {r.choice(words)}"
    return (i, name, f"{name.replace(' ', '.')}{i}@example.com", ' '.join(r.choices(words, k=40)), r.randbytes(160))
c.executemany("INSERT INTO users VALUES (?,?,?,?,?)", (row(i) for i in range(400_000)))
c.commit(); c.close()
shutil.copy2(db, f"{n}/app.db")
c = sqlite3.connect(f"{n}/app.db")
for _ in range(2000):
    c.execute("UPDATE users SET bio=?, avatar=? WHERE id=?", (' '.join(r.choices(words, k=40)), r.randbytes(160), r.randrange(400_000)))
c.executemany("INSERT INTO users VALUES (?,?,?,?,?)", (row(400_000 + i) for i in range(5000)))
for _ in range(1000):
    c.execute("DELETE FROM users WHERE id=?", (r.randrange(400_000),))
c.commit(); c.close()

# Extra datasets, used to choose the block and chunk sizes (not part of the first comparison).
vocab_rng = random.Random(60)
vocab = [''.join(vocab_rng.choice('abcdefghijklmnopqrstuvwxyz_') for _ in range(vocab_rng.randint(2, 12))) for _ in range(3000)]

# 6. tar: an archive of a 4,000-file text tree; 50 files get 1-10 lines inserted
import io, tarfile
r = random.Random(6); o, n = pair("tar")
tree = {f"src/m{i // 100:02d}/f{i:04d}.rs": [" ".join(r.choices(vocab, k=r.randint(3, 12))) for _ in range(r.randint(20, 400))]
        for i in range(4000)}
def write_tar(path):
    with tarfile.open(path, "w", format=tarfile.USTAR_FORMAT) as t:
        for name in sorted(tree):
            data = ("\n".join(tree[name]) + "\n").encode()
            ti = tarfile.TarInfo(name); ti.size = len(data); ti.mtime = 1_000_000_000; ti.mode = 0o644
            t.addfile(ti, io.BytesIO(data))
write_tar(f"{o}/src.tar")
for name in r.sample(sorted(tree), 50):
    lines = tree[name]; pos = r.randrange(len(lines) + 1)
    lines[pos:pos] = [" ".join(r.choices(vocab, k=8)) for _ in range(r.randint(1, 10))]
write_tar(f"{n}/src.tar")

# 7. csv: ~64 MB of rows; 300 rows rewritten with fields of a different length
r = random.Random(7); o, n = pair("csv")
def csv_row(i):
    return f"{i},{r.choice(vocab)} {r.choice(vocab)},{r.choice(vocab)}@example.com,{r.randint(0, 10**9)},{r.random():.6f}\n"
rows = [csv_row(i) for i in range(1_250_000)]
open(f"{o}/data.csv", "w").write("".join(rows))
for i in r.sample(range(len(rows)), 300):
    rows[i] = csv_row(i)
open(f"{n}/data.csv", "w").write("".join(rows))

# 8. scatter: 256 MiB, 3,000 writes of 64 bytes in place (a fixed-size database or image)
r = random.Random(8); o, n = pair("scatter")
d = rnd(r, 256 * MiB); open(f"{o}/scatter.img", "wb").write(d)
for _ in range(3000):
    off = r.randrange(0, len(d) - 64); d[off:off + 64] = r.randbytes(64)
open(f"{n}/scatter.img", "wb").write(d)

# Datasets for the fast_rsync comparison (fast_rsync_comparison.md), each an old/ and a new/
# directory: inplace, shift, append, mid, sqlite. Seeded, so reruns produce identical files.
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

# Markdown tables from netbench.sh and engine.sh output (medians, min-max, bytes).
#   python3 -I summarize.py net.tsv engine.txt
import csv, re, statistics as st, sys
from collections import defaultdict
net, engine = sys.argv[1], sys.argv[2]
MB = 1e6
rows = list(csv.DictReader(open(net), delimiter="\t"))
cell = defaultdict(list)
for r in rows:
    cell[(r["profile"], r["workload"], r["tool"])].append(r)
# Tools and datasets in the order they first appear.
tools = list(dict.fromkeys(r["tool"] for r in rows))
wls = list(dict.fromkeys(r["workload"] for r in rows))
NAMES = {"frsync": "fast_rsync (frsync)", "barqsync": "barqsync", "rsync": "rsync"}
fails = [r for r in rows if r["ok"] != "OK"]
print(f"runs: {len(rows)}, failed verification: {len(fails)}\n")
for prof in ["lan", "wan"]:
    print(f"### {prof}: wall time, median (min–max) of runs\n")
    print("| workload | " + " | ".join(NAMES.get(t, t) for t in tools) + " |")
    print("|---|" + "---:|" * len(tools))
    for wl in wls:
        out = []
        meds = {}
        for t in tools:
            xs = [float(r["secs"]) for r in cell[(prof, wl, t)]]
            if not xs:
                out.append("–"); continue
            meds[t] = st.median(xs)
            out.append(f"{st.median(xs):.2f} s ({min(xs):.2f}–{max(xs):.2f})")
        best = min(meds, key=meds.get) if meds else None
        out = [f"**{o}**" if t == best else o for t, o in zip(tools, out)]
        print(f"| {wl} | " + " | ".join(out) + " |")
    print()
print("### Bytes on the link (veth counters, incl. ssh/TCP overhead), median of all runs\n")
print("| workload | " + " | ".join(NAMES.get(t, t) + " up / down" for t in tools) + " |")
print("|---|" + "---:|" * len(tools))
for wl in wls:
    out = []
    for t in tools:
        rs = [r for p in ("lan", "wan") for r in cell[(p, wl, t)]]
        if not rs:
            out.append("–"); continue
        up = st.median(int(r["up_bytes"]) for r in rs) / MB
        dn = st.median(int(r["down_bytes"]) for r in rs) / MB
        out.append(f"{up:.2f} / {dn:.2f} MB")
    print(f"| {wl} | " + " | ".join(out) + " |")
print()
# engine
eng = defaultdict(lambda: defaultdict(list))
wl = None
for line in open(engine):
    m = re.match(r"== run \d+ (\S+)", line)
    if m:
        wl = m.group(1) if m.group(1) != "mid" else "mid"; continue
    parts = line.split("\t")
    if len(parts) > 2:
        kv = dict(p.split("=") for p in parts[1:])
        for k, v in kv.items():
            eng[(wl, parts[0])][k].append(v)
print("### In-process engine, one file pair, median of 3 runs (4 cores)\n")
print("| workload | engine | block | signature | delta | sig ms | diff ms | apply ms | verify ms | total ms |")
print("|---|---|---|---:|---:|---:|---:|---:|---:|---:|")
for wl in dict.fromkeys(w for w, _ in eng):
    for e in dict.fromkeys(e for _, e in eng):
        d = eng[(wl, e)]
        if not d: continue
        med = lambda k: st.median(float(x) for x in d[k])
        blk = d.get("block", d.get("mode"))[0]
        print(f"| {wl} | {e} | {blk} | {int(d['sig_bytes'][0])/1e3:.0f} KB | {int(d['delta_bytes'][0])/MB:.2f} MB | {med('sig_ms'):.0f} | {med('diff_ms'):.0f} | {med('apply_ms'):.0f} | {med('verify_ms'):.0f} | {med('total_ms'):.0f} |")

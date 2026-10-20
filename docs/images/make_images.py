#!/usr/bin/env python3
"""Draw the README's charts and diagrams as SVG (no dependencies).

    python3 docs/images/make_images.py

Writes bench-everyday.svg, bench-delta-wan.svg, how-it-works.svg and two-level-delta.svg
next to this script. Each SVG carries light and dark colors (prefers-color-scheme) on its
own background, so it reads on GitHub and crates.io in either theme. The numbers are the
measurements in IMPLEMENTATION.md (M14, macOS) and fast_rsync_comparison.md (M16, Linux).
"""
import os

HERE = os.path.dirname(os.path.abspath(__file__))
W = 760
FONT = 'system-ui,-apple-system,"Segoe UI",Roboto,sans-serif'

# Color roles: (light, dark). Series 1/2 and the surfaces are the validated reference
# palette (both modes pass the CVD and contrast checks).
ROLES = {
    "bg": ("#fcfcfb", "#1a1a19"),
    "ring": ("rgba(11,11,11,0.10)", "rgba(255,255,255,0.10)"),
    "ink": ("#0b0b0b", "#f0efec"),
    "ink2": ("#52514e", "#c3c2b7"),
    "muted": ("#898781", "#898781"),
    "grid": ("#e1e0d9", "#2c2c2a"),
    "base": ("#c3c2b7", "#383835"),
    "s1": ("#2a78d6", "#3987e5"),
    "s2": ("#eb6834", "#d95926"),
    "s1soft": ("#b7d3f6", "#184f95"),
    "onfill": ("#ffffff", "#ffffff"),
}


def style():
    light = "".join(f".f-{k}{{fill:{v[0]}}}.k-{k}{{stroke:{v[0]}}}" for k, v in ROLES.items())
    dark = "".join(f".f-{k}{{fill:{v[1]}}}.k-{k}{{stroke:{v[1]}}}" for k, v in ROLES.items())
    return (f"<style>text{{font-family:{FONT}}}{light}"
            f"@media (prefers-color-scheme:dark){{{dark}}}</style>")


def esc(s):
    return s.replace("&", "&amp;").replace("<", "&lt;").replace(">", "&gt;")


def text(x, y, s, size=13, role="ink", weight=400, anchor="start"):
    return (f'<text x="{x:.1f}" y="{y:.1f}" font-size="{size}" font-weight="{weight}" '
            f'text-anchor="{anchor}" class="f-{role}">{esc(s)}</text>')


def text_width(s, size=13, weight=400):
    """Rough width of system-ui text, for layout only."""
    return len(s) * size * (0.60 if weight >= 600 else 0.55)


def bar(x, y, w, h, role, r=4):
    """A bar growing right from x: square at the baseline, rounded data end."""
    if w <= 0:
        return ""
    r = min(r, w, h / 2)
    return (f'<path class="f-{role}" d="M{x:.1f},{y:.1f}H{x + w - r:.1f}'
            f'A{r},{r} 0 0 1 {x + w:.1f},{y + r:.1f}V{y + h - r:.1f}'
            f'A{r},{r} 0 0 1 {x + w - r:.1f},{y + h:.1f}H{x:.1f}Z"/>')


def box(x, y, w, h, role, r=4):
    return f'<rect x="{x:.1f}" y="{y:.1f}" width="{w:.1f}" height="{h:.1f}" rx="{r}" class="f-{role}"/>'


def frame(h, title, desc, body):
    return (f'<svg xmlns="http://www.w3.org/2000/svg" width="{W}" height="{h}" viewBox="0 0 {W} {h}" '
            f'role="img" aria-labelledby="t d"><title id="t">{esc(title)}</title>'
            f'<desc id="d">{esc(desc)}</desc>{style()}'
            f'<rect x="0.5" y="0.5" width="{W - 1}" height="{h - 1}" rx="12" class="f-bg k-ring"/>'
            f'{"".join(body)}</svg>\n')


def legend(x, y, items):
    out = []
    for label, role in items:
        out.append(box(x, y - 9, 12, 12, role, 3))
        out.append(text(x + 18, y + 1, label, 13, "ink2"))
        x += 18 + text_width(label) + 22
    return out


def verdict(fast, slow):
    ratio = slow / fast
    return f"{ratio:.1f}× faster" if ratio >= 1.05 else ("about the same" if ratio > 0.95 else f"{(fast / slow - 1) * 100:.0f}% slower")


def grouped_bars(name, title, subtitle, rows, domain, ticks, unit="s"):
    """Horizontal grouped bars: barqsync (series 1) above rsync (series 2) per row."""
    left, right = 290, 140  # label column, room for value labels past the plot
    x0, x1 = left, W - right
    scale = (x1 - x0) / domain
    top = 96
    bh, gap, row_h = 13, 2, 46
    h = top + len(rows) * row_h + 56
    body = [text(24, 36, title, 17, "ink", 600), text(24, 58, subtitle, 13, "ink2")]
    body += legend(24, 82, [("barqsync", "s1"), ("rsync", "s2")])
    plot_bottom = top + len(rows) * row_h
    for t in ticks:
        gx = x0 + t * scale
        body.append(f'<line x1="{gx:.1f}" y1="{top - 6}" x2="{gx:.1f}" y2="{plot_bottom}" class="k-grid" stroke-width="1"/>')
        body.append(text(gx, plot_bottom + 18, f"{t:g} {unit}", 11, "muted", anchor="middle"))
    # Values are strings, printed with the precision they were measured with.
    for i, (label, fast_s, slow_s) in enumerate(rows):
        fast, slow = float(fast_s), float(slow_s)
        y = top + i * row_h + 6
        body.append(text(left - 14, y + bh + 5, label, 13, "ink", anchor="end"))
        body.append(bar(x0, y, fast * scale, bh, "s1"))
        body.append(bar(x0, y + bh + gap, slow * scale, bh, "s2"))
        body.append(text(x0 + fast * scale + 6, y + bh - 2, f"{fast_s} {unit}  ·  {verdict(fast, slow)}", 12, "ink"))
        body.append(text(x0 + slow * scale + 6, y + 2 * bh + gap - 2, f"{slow_s} {unit}", 12, "ink2"))
    body.append(f'<line x1="{x0}" y1="{top - 6}" x2="{x0}" y2="{plot_bottom}" class="k-base" stroke-width="1"/>')
    desc = "; ".join(f"{l}: barqsync {f} {unit}, rsync {s} {unit}" for l, f, s in rows)
    write(name, frame(h, title, desc, body))


def how_it_works():
    title = "How barqsync gets the same work done sooner"
    body = [text(24, 36, title, 17, "ink", 600),
            text(24, 58, "Copying a directory tree: rsync works through it one file at a time; barqsync uses every core.", 13, "ink2")]
    lane_x, unit = 150, 52  # time per task, in px
    # rsync: one lane, everything in sequence.
    y = 96
    body.append(text(24, y + 17, "rsync", 14, "ink", 600))
    body.append(text(24, y + 34, "1 thread", 12, "muted"))
    tasks = ["scan", "compare"] + [f"file {i}" for i in range(1, 9)]
    for i, t in enumerate(tasks):
        x = lane_x + i * (unit + 2)
        body.append(box(x, y, unit, 28, "base"))
        body.append(text(x + unit / 2, y + 18, t, 11, "ink", anchor="middle"))
    rsync_end = lane_x + len(tasks) * (unit + 2) - 2
    # barqsync: four lanes in parallel; scanning, comparing and copying overlap.
    y = 156
    body.append(text(24, y + 17, "barqsync", 14, "ink", 600))
    body.append(text(24, y + 34, "all cores", 12, "muted"))
    lanes = [["scan A", "file 1", "file 5"], ["scan B", "file 2", "file 6"],
             ["compare", "file 3", "file 7"], ["file 4", "clone", "file 8"]]
    for li, lane in enumerate(lanes):
        ly = y + li * 32
        for i, t in enumerate(lane):
            x = lane_x + i * (unit + 2)
            body.append(box(x, ly, unit, 28, "s1"))
            body.append(text(x + unit / 2, ly + 18, t, 11, "onfill", anchor="middle"))
    barq_end = lane_x + 3 * (unit + 2) - 2
    # Time axis and the two finish lines.
    ay = y + 4 * 32 + 14
    body.append(f'<line x1="{lane_x}" y1="{ay}" x2="{rsync_end + 10}" y2="{ay}" class="k-base" stroke-width="1"/>')
    body.append(text(rsync_end + 14, ay + 4, "time", 11, "muted"))
    for x, label in [(barq_end, "barqsync done"), (rsync_end, "rsync done")]:
        body.append(f'<line x1="{x}" y1="{ay - 5}" x2="{x}" y2="{ay + 5}" class="k-ink" stroke-width="1.5"/>')
        body.append(text(x, ay + 20, label, 12, "ink2", anchor="middle"))
    notes = ["Scan: one task per directory, in parallel; files are compared as listings arrive.",
             "Copy: many files at once; big files are cloned (APFS, XFS, Btrfs) or copied in the kernel.",
             "Network: listings and requests are pipelined, so data flows before the scan ends."]
    ny = ay + 52
    for i, n in enumerate(notes):
        body.append(box(24, ny + i * 22 - 9, 8, 8, "s1", 2))
        body.append(text(40, ny + i * 22, n, 12.5, "ink2"))
    h = ny + len(notes) * 22 + 10
    desc = "rsync scans, compares and copies one file after another in one thread; barqsync runs the same tasks on four lanes at once and finishes first."
    write("how-it-works.svg", frame(h, title, desc, body))


def two_level_delta():
    title = "Only the changed parts travel"
    body = [text(24, 36, title, 17, "ink", 600),
            text(24, 58, "Updating a 256 MiB file the other side has an older copy of. Bigger files use bigger pieces.", 13, "ink2")]
    x0, n, bw, g = 184, 8, 50, 2
    changed = {2, 6}

    def row(y, label, sub, roles, labels=None):
        out = [text(24, y + 15, label, 13, "ink", 600), text(24, y + 31, sub, 12, "muted")]
        for i in range(n):
            out.append(box(x0 + i * (bw + g), y, bw, 26, roles[i]))
            if labels and labels[i]:
                out.append(text(x0 + i * (bw + g) + bw / 2, y + 17, labels[i], 11, "ink", anchor="middle"))
        return out

    y = 84
    body += row(y, "1  Coarse check", "one hash per 64 KiB", ["base"] * n)
    body.append(text(x0 + n * (bw + g) + 10, y + 17, "small: ~0.1 MB", 12, "ink2"))
    body.append(text(x0 + n * (bw + g) + 10, y + 32, "per 256 MiB", 12, "ink2"))
    y += 52
    roles = ["s1soft" if i in changed else "base" for i in range(n)]
    labels = ["differs" if i in changed else "" for i in range(n)]
    body += row(y, "2  Compare", "6 blocks match, 2 differ", roles, labels)
    # 3: zoom into each differing block: sixteen 4 KiB pieces, one of them changed.
    y += 52
    body.append(text(24, y + 15, "3  Look closer", 13, "ink", 600))
    body.append(text(24, y + 31, "only where it differs: 4 KiB", 12, "muted"))
    pieces, pw = 16, (2 * bw + g - 15 * 1) / 16  # one 64 KiB block = sixteen 4 KiB pieces
    zx = [x0, x0 + 4 * (bw + g)]
    for zi, (z, c) in enumerate(zip(zx, sorted(changed))):
        # guide lines from the coarse block above to its zoomed pieces
        cx = x0 + c * (bw + g)
        body.append(f'<line x1="{cx:.1f}" y1="{y - 26}" x2="{z:.1f}" y2="{y}" class="k-grid" stroke-width="1"/>')
        body.append(f'<line x1="{cx + bw:.1f}" y1="{y - 26}" x2="{z + 2 * bw + g:.1f}" y2="{y}" class="k-grid" stroke-width="1"/>')
        hot = 5 if zi == 0 else 11
        for p in range(pieces):
            body.append(box(z + p * (pw + 1), y, pw, 26, "s1" if p == hot else "base", 2))
    y += 52
    body += row(y, "4  Send", "just the changed pieces", ["base"] * n)
    for zi, (z, c) in enumerate(zip(zx, sorted(changed))):
        hot = 5 if zi == 0 else 11
        # the changed piece, at its place in the block (drawn at least 5 px wide)
        body.append(box(x0 + c * (bw + g) + hot * bw / 16 + bw / 32 - 2.5, y, 5, 26, "s1", 1.5))
    body.append(text(x0 + n * (bw + g) + 10, y + 11, "8 KiB sent,", 12, "ink", 600))
    body.append(text(x0 + n * (bw + g) + 10, y + 26, "not 128 KiB", 12, "ink", 600))
    y += 60
    body += legend(24, y, [("already on the receiver, not sent", "base"), ("needs a closer look", "s1soft"), ("sent", "s1")])
    notes = ["Over a network, an appended log or a rewritten file skips step 3: no extra cost.",
             "Inserted or deleted bytes are found too: chunk boundaries follow the content."]
    ny = y + 30
    for i, s in enumerate(notes):
        body.append(text(24, ny + i * 20, s, 12.5, "ink2"))
    h = ny + len(notes) * 20 + 8
    desc = ("A 256 MiB file. Step 1 hashes 64 KiB blocks; step 2 finds the two that differ; step 3 hashes those two "
            "in 4 KiB pieces; step 4 sends only the two changed pieces: 8 KiB instead of 128 KiB.")
    write("two-level-delta.svg", frame(h, title, desc, body))


def write(name, svg):
    with open(os.path.join(HERE, name), "w") as f:
        f.write(svg)
    print("wrote", name)


grouped_bars(
    "bench-everyday.svg",
    "Everyday syncs: barqsync vs rsync",
    "Seconds, lower is better · M-series Mac, APFS, rsync 3.5.1 · ssh rows run over a local pipe",
    [
        ("Copy 200,000 small files", "20.5", "42.4"),
        ("Re-sync 500,000 files, 3 changed", "3.49", "18.2"),
        ("Update a 10 GiB file, 50 MiB changed", "5.9", "24.3"),
        ("Push 200,000 small files over ssh", "19.3", "42.4"),
        ("Re-sync 500,000 files over ssh", "3.26", "12.9"),
    ],
    domain=45, ticks=[0, 10, 20, 30, 40],
)
grouped_bars(
    "bench-delta-wan.svg",
    "Updating changed files over a 100 Mbit link",
    "Seconds, lower is better · push over ssh, 20 ms round trip · Linux, rsync 3.2.7",
    [
        ("SQLite database, 8,000 changes", "3.23", "9.38"),
        ("256 MiB file, 3,000 small writes", "1.63", "4.46"),
        ("78 MB CSV, 300 rows rewritten", "0.66", "0.80"),
        ("54 MB tar, 50 files edited", "0.56", "0.63"),
        ("256 MiB file, 40 inserts/deletes", "0.75", "0.84"),
        ("256 MiB file, 8 MiB appended", "1.29", "1.50"),
        ("256 MiB file, 8 MiB rewritten", "1.24", "1.37"),
        ("40 × 4 MiB files, 1 KiB each", "0.73", "0.69"),
    ],
    domain=10, ticks=[0, 2, 4, 6, 8, 10],
)
how_it_works()
two_level_delta()

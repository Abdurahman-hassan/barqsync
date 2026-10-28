# barqsync: implementation notes

The detailed notes behind barqsync: milestone status, every supported option, how each part
works, and the measurements that shaped it. Through 0.2.0 this was the README;
[README.md](../README.md) is now a short overview.

A parallel, rsync-compatible file synchronizer written in Rust.

rsync is single-threaded on both the sending and receiving side, and its receiver makes one
blocking open/write/rename/utimes sequence per file. barqsync keeps rsync's command line and
semantics but runs one task per directory across all cores, copies files in parallel, and
uses copy-on-write clones (APFS `fclonefileat`) or in-kernel copies (Linux
`copy_file_range`) so file data never passes through userspace when it doesn't need to.

## Status

| Milestone | Scope | State |
| --- | --- | --- |
| M0 | toolchain, crate, release profile | done |
| M1 | CLI, filters, parallel scan, quick-check diff, `-n -v` | done |
| M2 | local sync engine: dirs, files, symlinks, specials, perms/times/owner, `--delete`, temp+rename, Ctrl-C cleanup | done |
| M3 | delta engine: aligned blocks, FastCDC fallback, clone+patch, BLAKE3-verified matches | done |
| M4 | remote over SSH (`host:path`): push and pull, small-file packing, delta over the wire, `-z`, `-e`, `--remote-path` | done |
| M5 | parallel TLS data streams (`--streams N`), pinned ephemeral certificate, token auth, ssh fallback | done |
| M6 | `-H`, `--progress`/`--info=progress2`, `-i`/`%i`, `getattrlistbulk`/`statx` metadata reads | done |
| M7 | common rsync flags: `-c -S -R -d -L -k -K -b -O`, `--partial(-dir)`, `--files-from`, `--existing`, `--max/min-size`, `--link/compare-dest`, `--numeric-ids`, `--chmod`, `--remove-source-files`; page-cache hints | done |
| M8 | verification harness: Linux container with sshd and `tc netem` link profiles, `strace -c`, flamegraphs, `cp` baselines | done |
| M9 | the original plan's delta design as `--delta=cdc` (FastCDC throughout, xxh3 + BLAKE3, mmap); measured slower, so `blocks` stays the default | done |
| M10 | the original plan's scanner as `--scanner=jwalk` (jwalk + DashMap index + queues A–D); measured slower, so `dirtask` stays the default | done |
| M11 | QUIC transport (`--transport quic`, quinn), rkyv wire format (`--wire rkyv`), tokio connection layer (`--net-io=tokio`) | done |
| M12 | adaptive strategy (`--strategy adaptive`): link probe, whole-vs-delta routing by link speed, pack compression with link-based zstd levels, `--compress-level`; up to 7.6× on compressible data, slower on incompressible, so `fixed` stays the default | done |
| M13 | io_uring batches for received small files (`--io-uring`, Linux): 24% fewer syscalls, no faster, so opt-in; found on the way: skipping a redundant truncate made small-file pushes onto XFS 3.2× faster | done |
| M14 | full verification: local A/B/C/D vs rsync and `cp`, Linux syscall counts and flamegraphs, every transport and strategy over lan/wan/lossy links; README and as-built plan updated | done |
| M15 | delta block and chunk sizes scale with the file (4–64 KiB); a remote file that changed size got both signatures and a merged delta (protocol 6): 1.4–7.5× faster on small or scattered edits, but 4–22% slower on large appends and in-place runs, so M16 replaced the signature scheme | done |
| M16 | two-level delta: the coarse signatures of 0.1.1, refined on demand with one round trip where the coarse delta left enough literal data (protocol 7). Over 100 Mbit: as fast as 0.1.1 on appends, in-place runs and small files, 1.3–7.6× faster on everything else (a growing SQLite database 16.7 → 3.2 s, rsync 9.4 s), faster than rsync on 7 of 8 datasets; see [Two-level delta](#two-level-delta-m16). Also fixed a racy test fixture | done |
| M17 | `--transport auto`: an ssh ping picks QUIC on round trips of 30 ms and more, a short wait for it, hosts where it failed skipped for an hour (protocol 8). As fast as QUIC on the lossy link (2–3× the ssh pipe), 0.5 s lost at most where UDP is blocked, but a round trip more on short links and QUIC's ~3-round-trip setup and close on clean long ones (11–22% slower there on a delta or a no-op), so the ssh pipe stays the default; see [`--transport auto`, measured](#--transport-auto-measured) | done |
| M18 | `-X/--xattrs` (`-XX`) and `-A/--acls` with rsync's semantics, locally and remote (protocol 9): rsync's namespace rules on each system, long values sent as checksums and fetched only when needed, POSIX ACLs on Linux and extended ACLs on macOS, `x`/`a` item columns, attribute-only updates; APFS clones no longer keep attributes without `-X`; see [Extended attributes and ACLs](#extended-attributes-and-acls--x--a) | done |
| M19 | QUIC's fixed cost: `BARQSYNC_TIMING` milestones on both sides; the sender closes once it has `ReceiverDone`, nothing waits for acknowledgements, the client doesn't wait for the token's acknowledgement, auto connects alongside its ping, and hosts where QUIC found a clean link stay on the ssh pipe for an hour. On a clean 80 ms link QUIC's cost over the pipe fell from 3–5 round trips per transfer to 2 (auto: delta +9% → +3.5%, no-op +23% → +9%; once the host is known clean, the pipe's own times); the lossy win is unchanged. The ssh pipe stays the default; see [QUIC's fixed cost](#quics-fixed-cost-m19). Also: a pull with `--remove-source-files` prints the sender's removals and exits 23 when one fails, as rsync does (protocol 10) | done |
| M20 | parallel streams through ssh when a firewall blocks them: `barqsync --stream-relay` on the server carries each stream over its own ssh connection to the stream listener, which also listens on loopback. TLS with the pinned certificate and token still runs end to end. Verified on a real Mac with its firewall on: push 18.0 → 12.7 s and pull 65.5 → 38.6 s on a slow link. Also `~` in remote paths, clearer errors, local copy progress and literal-data stats. See [audit/04-ssh-relay.md](audit/04-ssh-relay.md) | done |
| M21 | the October 2026 audit's 12 P1 findings, fixed for 0.4.0 (PRs 15–19): what the sending side announces is checked before anything is written; source removal is limited to entries the sender listed; same-file and backup guards; `--delete` across several sources, and none after a read error; `--threads=1` no longer hangs; Ctrl-C and lost peers end both sides in bounded time; large messages are chunked. See [the audit](audit/README.md) | done |
| M22 | rsync command lines that failed with "unexpected argument" now run (protocol 11): `--no-OPTION`, repeated options, `--delete-before/-during/-delay/-after`, `--max-delete` (exit 25), rsync's filter rules (`--filter`/`-f` with merge and per-directory files, `-F`, `-C`), `-m`, `--mkpath`, `-s`, `--timeout` (exit 30), exit 24 for vanished files, and rsync's byte counts in `--stats`. Also: what the rules protect inside a directory `--delete` removes now stays, as with rsync | done |

The test suite passes on macOS (APFS; 249 tests at 0.4.0, 198 at M19, 164 at M17, 154 at
M16, 145 at M14, when it was also built as x86_64 and run under Rosetta) and Linux (253 tests
at 0.4.0 and 201 at M19 in an aarch64 container, 167 at M17, 157 at M16 on an x86_64 VM, 148
at M14), compared against rsync 3.5.1 on macOS and 3.2.7 on Linux. A few tests are
Linux-only.

## Install

```bash
cargo install barqsync      # needs Rust: https://rustup.rs
barqsync -av src/ dst/
```

If `barqsync` is not found after installing, ensure Cargo's binary directory (`~/.cargo/bin`) is in your `PATH`:

- **Zsh (`~/.zshrc`) — default on macOS:**
  ```bash
  echo 'export PATH="$HOME/.cargo/bin:$PATH"' >> ~/.zshrc
  source ~/.zshrc
  ```

- **Bash (`~/.bashrc` on Linux or `~/.bash_profile` on macOS):**
  ```bash
  echo 'export PATH="$HOME/.cargo/bin:$PATH"' >> ~/.bashrc        # Linux
  echo 'export PATH="$HOME/.cargo/bin:$PATH"' >> ~/.bash_profile  # macOS
  ```

macOS and Linux only. For remote transfers, barqsync has to be installed on both machines
(like rsync), or pointed to with `--remote-path`, in the same release line. Version 0.4
uses wire protocol 10 (0.3 moved to 9 for extended attributes and ACLs; 0.4 adds the
pull's `RemovalsDone`), so 0.4.x cannot communicate with 0.3.x or earlier releases
("protocol version mismatch"). The unreleased code moves to protocol 11 for the rsync
options of M22 and those added since (`--bwlimit`, the id maps, `--append`, `-T`,
`--delay-updates`, the deletion and symlink options), so it cannot communicate with 0.4.x.
Upgrade both ends together.

## Build from source

```bash
brew install rustup && rustup default stable   # macOS; or https://rustup.rs
cargo build --release
./target/release/barqsync -av src/ dst/
```

With Homebrew's keg-only rustup, add `/opt/homebrew/opt/rustup/bin` to `PATH`.

## Supported options

`-a` (= `-rlptgoD`), `-r -l -p -t -g -o -D --devices --specials`, `-H`, `-X`/`-XX`, `-A`,
`-v -q -n` (`-q`: nothing on stdout, as in rsync),
`-i`/`-ii`, `--progress`/`-P`, `--out-format`, `--log-file` and `--info` (see the table),
`--delete`, `--delete-excluded`,
`--exclude/--include/--exclude-from/--include-from` (rsync wildcard rules, first match wins,
`/` anchoring, trailing `/` for directories, `**`, `dir/***`, `+ `/`- ` prefixes, `!` to clear),
`--filter`/`-f`, `-F`, `-C` (see the table), `-u`, `--size-only`, `-I`,
`--ignore-existing`, `-x`, `--modify-window` (`-1` compares nanoseconds), `-W/--whole-file`,
`--no-whole-file` (delta transfer; locally off by default, like rsync), `--inplace` (also
writes new files directly, like rsync),
`--fsync`, `--stats`, `-h`, `--threads N`, `--drop-cache`/`--no-drop-cache` (page-cache hints
for files of 8 MiB+, on by default; see [Page-cache hints](#page-cache-hints)),
`--delta=blocks|cdc` (delta design, see [Delta transfer](#delta-transfer); for a remote
transfer the receiver's choice applies), `--scanner=dirtask|jwalk`, `--transport
ssh|tcp|quic|auto` with `--streams N`, `--wire postcard|rkyv` (see [Remote transfers](#remote-transfers)),
`--strategy adaptive|fixed` and `--compress-level N` (see [Adaptive strategy](#adaptive-strategy---strategy-adaptive)),
`--io-uring` (Linux; see [io_uring](#io_uring---io-uring-linux)).

Also, with rsync's semantics locally and over ssh (each has a differential test in
`tests/rsync_flags.rs`, or for output options in `tests/output_options.rs`):

| Option | Notes |
| --- | --- |
| `-c/--checksum` | Same-size files are compared by whole-file BLAKE3 (128-bit). Files are hashed in parallel, by the sender for its side. A rewritten file shows `>fc`. |
| `-S/--sparse` | Runs of zeros (4 KiB blocks) become holes. Clones and kernel copies are skipped, since they would keep the zeros as data. |
| `--partial`, `--partial-dir=DIR`, `-P` | On Ctrl-C or a dropped connection, the received prefix is kept as the destination file, or in `DIR` (created 0700). A file left in `DIR` is the delta basis next time and is removed once the file is complete. A relative `DIR` is excluded from the transfer and protected from `--delete`. |
| `-R/--relative` | Full relative paths, `/./` to trim the prefix. Implied directories get their source attributes but are never pruned by `--delete`. |
| `--files-from=FILE`, `-0/--from0` | Implies `-R` and `-d`; `-a` no longer implies `-r`. `#`/`;` comments, a trailing `/` lists a directory's contents, `.` or a leading `./` includes the root itself. `-` reads stdin. The client reads the list and sends it to the server. |
| `-d/--dirs` | Directories are created without recursing. |
| `--existing`, `--max-size`, `--min-size` | rsync's size syntax (`1.5M`, `4KB` = 4000, `1M-1`). |
| `-b/--backup`, `--suffix`, `--backup-dir` | Replaced and deleted files are kept (hard link, or a copy with `--inplace`). Without `--backup-dir`, `*~` backups are protected from `--delete` and `-O` is forced on, as in rsync. |
| `--link-dest`, `--compare-dest` | Checked even when the destination file exists but differs. Identical data and attributes → hard link / skip. Identical data, other attributes → local copy (`cf`). Otherwise the alt file is the delta basis and what the new file is itemized against. |
| `-L`, `-k`, `-K` | `-L` reports dangling links (`symlink has no referent`, exit 23). With `-K`, like rsync on macOS, the kept symlink gets the directory's time. |
| `--numeric-ids` | Without it, remote transfers map users and groups by name (id 0 never), as rsync does. |
| `--chmod=CHMOD` | rsync's syntax (`D`/`F`, `ugoa`, `+-=`, `rwxXst`, octal), applied to the source modes as sent. |
| `--remove-source-files` | Non-directories the receiver has (transferred or already up to date) are removed from the source, unless their size or mtime changed meanwhile. |
| `-O/--omit-dir-times` | |
| `--no-OPTION` | `--no-r`, `--no-l`, `--no-p`, `--no-t`, `--no-g`, `--no-o`, `--no-D`, `--no-devices`, `--no-specials`, `--no-H`, `--no-A`, `--no-X`, `--no-O`, `--no-d`, `--no-x`, `--no-S`, `--no-c`, `--no-z`, `--no-W`, `--no-v`, `--no-i`, `--no-h`, `--no-8`, `--no-m`, `--no-s`, `--no-inplace`, `--no-append`, `--no-partial`, `--no-delay-updates`, `--no-force`, `--no-ignore-errors`, `--no-munge-links`, `--no-progress`, `--no-numeric-ids`, `--no-backup`, `--no-mkpath`, `--no-timeout`, `--no-from0`, and their long forms (`--no-owner`, ...). Options apply in order, as in rsync: `-a --no-o` drops `-o`, `--no-o -a` keeps it, and `--no-v` resets the `-v` count. Any option may be given more than once. |
| `--delete-before`, `--delete-during`/`--del`, `--delete-delay`, `--delete-after` | Each implies `--delete`. Two different times are refused, and so is a `--delete` without `-r` or `-d` (also after `--no-r`). barqsync always deletes once the transfer is done, the whole source was read and everything changed was written without errors, which is rsync's `--delete-delay`/`--delete-after`; see the differences below. |
| `--max-delete=N` | At most N deletions (a negative N: none), in rsync's order (directory by directory as rsync's file list visits them; in each, subdirectories first, then files, by descending name). What the limit stops is counted: "Deletions stopped due to --max-delete limit (K skipped)", exit 25 unless an error (23) or a vanished file (24) says otherwise. A directory that keeps an entry stays ("cannot delete non-empty directory"). |
| `-x` with `--delete` | A mount point in the source (a directory on another filesystem than its transfer root) is made in the destination without its contents, and nothing in the destination's copy of it is deleted. The sender decides, and marks these directories in its listings (protocol 11). When several sources merge into one destination, later ones that have that directory on their own filesystem fill it instead (`a/ b/ dst/`, with `a/m` a mount point and `b/m` not: `dst/m` gets `b/m`'s contents and `a/m`'s attributes), as rsync's receiver merges the duplicates' content flag. A `-R` or `--files-from` path doesn't stop at a mount point: its implied directories are never taken for mount points, and each named path is checked from its own filesystem, as rsync sets `-x`'s device from each argument (`-R s/mnt/file` sends the file). A mount point in the destination (a directory on another filesystem than the one it is in) is left alone. If the source lacks it, it isn't deleted, and `-vv` says "cannot delete mount point: X" (rsync's `--info=mount`). If the source has it, its extra entries stay, unless it is one of rsync's top directories: the destination itself, where a named source directory goes, or the end of a `-R` path. A directory being deleted that holds one stays ("cannot delete non-empty directory"). |
| `--filter`/`-f`, `-F`, `-C` | rsync's filter rules: `-`/`+` (`exclude`/`include`), `H`/`S` (sender only), `P`/`R` (receiver only), `!` (`clear`), the modifiers `!`, `/`, `p`, `s`, `r` and `C`, a space or `_` before the pattern. `. FILE` merges FILE's rules (read on the client, `-` for stdin); `: FILE` reads FILE in each directory, its rules applying there and below, deeper ones first (`n`: not inherited; `-`/`+`: bare patterns; `w`: words; `e`: exclude FILE too; `C`: `.cvsignore` style). A name with a leading `/` or other path (`-F` is `: /.rsync-filter`) is also read in the directories above the first one transferred. A leading `/` in a per-directory file anchors at its directory. `-FF` adds `- .rsync-filter`; `-C` appends `:C` and `-C` (rsync's default CVS list, perishable, then `~/.cvsignore` and `$CVSIGNORE`). With `-0`/`--from0`, rules files, merge and per-directory ones included, have NUL-separated rules. With `--delete`, receiver-side rules protect destination entries, read from the destination's own per-directory files, also inside a directory being deleted (which then stays), perishable rules aside; `--delete-excluded` makes plain rules sender-only. A remote receiver re-checks what the sender lists unless the rules need the sender's files (per-directory or `/` rules), as rsync trusts the sender then. |
| `-m/--prune-empty-dirs` | Directories with nothing but directories below them, once the rules apply, are left out (with `--delete`, the destination's copies are then extraneous). A directory a receiver-side rule (`P`) protects stays, with the ones leading to it, and a `--files-from` entry missing from a pruned directory is still reported (23). The sender decides, so a pull needs the server to know `-m` (protocol 11). |
| `--mkpath` | The destination's missing directories are created ("created N directories for DIR" with `-v`/`-i`, also counted in a dry run); for a single file, its parent's. |
| `-s/--secluded-args`, `--protect-args` | Accepted: barqsync's remote paths and options always travel in the protocol, never on the remote command line. |
| `--timeout=SECONDS` | A remote transfer whose connection delivers nothing for that long ends with 30 ("io timeout after N seconds -- exiting"), the server too. Each side sends a keepalive when it has nothing else to send, so a side that is busy (scanning, hashing, deleting) doesn't trip it; only a connection that stopped moving data does. |
| `--out-format=FORMAT` (`--log-format`) | rsync's escapes: `%o` (`send`, `recv`, `del.`), `%n`, `%f` (the source path for a sending client, a root directory as `src/.`; the name in the destination for a receiving one), `%l`, `%b` (see the differences below), `%M`, `%t`, `%B`, `%U` and `%G` (0 and `DEFAULT` without `-o`/`-g`, or for a group the receiving client can't give), `%p`, `%i`, `%L`, `%%`, with the modifiers `-`, a width (a leading `0` pads numbers with zeros, and strings too on macOS, as its libc does) and `'` (`%'l`: digit grouping, `%''l`: units of 1000, `%'''l`: units of 1024). As in rsync, `%a`, `%h`, `%P`, `%u`, unknown escapes and a trailing `%` stay as they are outside a daemon, and `%m` is empty. A format decides which items are listed: every transfer, entries created or changed in place, hard links, changed directories; with `%i` any change. Deletions use the format when it has `%o` or `%i`, otherwise they show as `deleting NAME`. `%c` and `%C` (rsync's checksum data) are refused, exit 1. |
| `--log-file=FILE`, `--log-file-format=FORMAT` | The client appends to FILE (created with mode 0644) what rsync's client logs, each line after the local time and its process id (`2026/10/27 09:41:07 [4321] `): "building file list" as the run starts ("receiving file list" in a pull); in FORMAT (`--out-format`'s escapes; `%i %n%L` by default, empty: no items), the items `--out-format`'s rule picks for stdout's own format (`-i`, `-v` or none, also with `-q`) when they changed, or all of them when FORMAT has `%i`; deletions in FORMAT when it has `%o` or `%i`, otherwise as `deleting NAME`; every message the client shows, also with `-q` (warnings, errors, `-v`'s lines and summary), but not a server's errors and warnings, which rsync's server writes to its own stderr ("X is uptodate" is logged for a local copy and a push, where another process says it, not for a pull); and at the end "sent N bytes  received M bytes  total size T", unless `-v` or `--stats` logged the summary (a pull logs it only after a run without errors). A dry run logs no transfers or deletions. A FILE that can't be opened is reported ("failed to open log-file FILE", then "Ignoring \"log file\" setting.") and the run goes on. The server writes no log: what it reports reaches the client's. |
| `--info=FLAGS` | rsync's words, levels and priorities: what `--info` sets wins over what `-v`, `--stats` and `--progress` imply, in any order; `ALL[N]`, `NONE` and `help` work; an unknown word is refused (exit 1). `name` (`-v`; 2: unchanged entries too, "X is uptodate"), `del` (deletions without an item format), `skip` (`-u`, `--existing`, `--ignore-existing`, `--max-size`/`--min-size`; 2: what an existing file differs in), `flist` (rsync's file-list messages, "sending incremental file list" or, without incremental recursion, "building file list ... done"; 2: the number of files), `stats` (1: the closing lines of `-v`, 2: `--stats`), `nonreg` ("skipping non-regular file", on by default as in rsync), `remove`, `backup` (also "backup_dir is DIR"), `progress`. `misc` and `symsafe` are accepted: their rsync messages concern daemons, busy executables and options barqsync doesn't have. `copy` and `mount` are refused when named: barqsync doesn't produce rsync's messages for failed `--link-dest` copies or mount points. |
| `--stats` | Also rsync's "Total bytes sent/received" lines, and with `-v` or `--stats` its "sent N bytes  received M bytes  R bytes/sec" and "total size is T  speedup is S" lines (thousands separators; `-h`: units of 1000). They count the bytes on the connection, all streams included; a local copy, which has none, counts the file data written as sent. |
| `--bwlimit=RATE` | rsync's size syntax, in KiB per second unless a unit is given (`1.5m`, `100KB`), rounded to whole KiB as rsync does; `0` is no limit and anything else under 512 bytes is refused with rsync's message (exit 1). Each side paces what it writes to its connections, as rsync's `sleep_for_bwlimit` does: pieces of an eighth of a second's worth, a writer waiting once it is more than a tenth of a second ahead. All streams of a transfer (`--streams`, QUIC) share the one limit. A local copy paces the file data it writes, as rsync's local copies go through a pipe the limit applies to; it then makes no clones or in-kernel copies, which can't be paced. Backup and `--link-dest`/`--compare-dest` copies aren't paced (rsync makes those without the connection). |
| `--chown=USER:GROUP`, `--usermap=STRING`, `--groupmap=STRING` | rsync's comma-separated `FROM:TO` pairs, applied by the receiver (locally too), the first pair that matches deciding. `FROM` is an id, a `LOW-HIGH` range, a name or a wildcard (`*`, `?`, `[...]`) matched against the sender's name; an id without a name has an empty one, which `*` and an empty `FROM` match. `TO` is a name or id on the receiver; an unknown name is reported ("Unknown --groupmap name on receiver: NAME") and its pair left out. `--chown=U:G` is `--usermap=*:U --groupmap=*:G`, and either part may be empty. `--usermap` implies `-o` and `--groupmap` `-g` where they are given, so a later `--no-o`/`--no-g` still wins. As with rsync, only root changes owners, and groups change only to ones the user is in. The sender's name for id 0 travels too, as with a current rsync (`root:...` or `wheel:...` match it), though id 0 is never mapped by name alone. With `--numeric-ids` no names travel, so only ids, ranges, `*` and an empty `FROM` match. Giving a map twice, or `--chown` with the map it sets, is refused with rsync's messages (exit 1). |
| `-T DIR`/`--temp-dir=DIR` | Temp files are made in DIR (relative to the destination, as with rsync), named without the leading `.` there; DIR must exist and be a directory, or the run stops with rsync's message (exit 1). When DIR is on another filesystem the rename can't work, so, as rsync does, the data is copied over the destination file instead, which then gets its attributes again. |
| `--delay-updates` | Each updated file waits, complete with its attributes, in a `.~tmp~` directory next to it (or `--partial-dir`), and all are renamed into place once the transfer is done, before hard links, deletions and directory times; the holding directories go once empty. Implies `--partial` with that directory, which is hidden from the transfer and protected from `--delete`; an interrupted run leaves its files there as the basis for the next one. `--inplace` and `--append` are refused with it, as in rsync. |
| `--force` | A directory in the way of a file or symlink is deleted with what it holds even without `--delete` (what the rules protect inside it stays, and so does it then); without either, only an empty one goes ("cannot delete non-empty directory", exit 23), as in rsync. |
| `--ignore-errors` | `--delete` deletes even after a source read error or a failed write (rsync's "IO error encountered -- skipping file deletion" no longer applies). |
| `--ignore-missing-args`, `--delete-missing-args` | A named source (an argument without a trailing `/`, or a `-R`/`--files-from` path) that doesn't exist is no error. With `--delete-missing-args` its destination is deleted (`*deleting   NAME`), a single file's new name included, unless a rule protects it; a directory with something in it only with `--delete` or `--force` (otherwise "cannot delete non-empty directory", no error). A path listed below a directory missing at the source is deleted too, with rsync's warnings (the directory "vanished", exit 24, and "parent dir is absent in the file list"). Missing entries travel like rsync's mode-0 file-list entries; a receiver refuses them without the option. |
| `--safe-links` | The receiver ignores symlinks that point outside the transfer (absolute or empty targets, `..`s that climb above the transfer's top, and, as from rsync 3.4, a `/../` past the leading `../`s or a final `/..`): "ignoring unsafe symlink" with `-v`. Their names still keep `--delete` off what the destination has there. |
| `--copy-unsafe-links` | The sender copies what an unsafe symlink points to (a file, or a directory and its contents) in its place, "copying unsafe symlink" with `-v`; one that leads nowhere is "symlink has no referent" (exit 23). Safe ones stay symlinks. As with `-L`, the file read must still be the one listed. |
| `--munge-links` | As in rsync, each side has its own: the client unmunges the symlinks it sends (a `/rsyncd-munged/` prefix is taken off) and munges those it receives (the prefix is put on), and the option is never passed to the server. A local copy, whose client sends, only unmunges. barqsync refuses to start while a `/rsyncd-munged` directory exists (exit 4). |
| `--append`, `--append-verify` | Implies `--inplace`. A destination file shorter than the source gets the rest of the source's data written onto it; one that isn't shorter is left alone, attributes and all, when its data would otherwise be rewritten, as rsync does. Over the wire the receiver asks for what is past its end, and the sender sends that as a delta keeping the rest. `--append-verify` also checks that what is already there is the source's start (a BLAKE3 hash of it, on both sides); when it isn't, it warns with `-v` or `-i` ("WARNING: NAME failed verification -- update retained (will try again).") and rewrites the whole file, itemizing the second try as rsync does. `-W` and `--partial-dir` are refused with it, as in rsync. |

Trailing-slash semantics match rsync: `src/` syncs the contents, `src` syncs the directory itself;
without `-r` or `-d` neither is transferred ("skipping directory").
Exit codes follow rsync (0 ok, 1 syntax, 11 file I/O, 12 protocol, 20 interrupted, 23 partial
transfer, 24 source files vanished, 25 `--max-delete` limit, 30 `--timeout`).

Still missing: daemon mode (`rsync://`, `host::module`), as well as `--copy-dest`, `--fuzzy`,
`--contimeout`, `-N`, `-E`, `-U`, `--no-implied-dirs`, `--no-R`, filter rules for xattr names
(the `x` modifier), merge and `-C` rules inside per-directory filter files, and a remote
(`host:`) `--files-from` list.

Known differences:

- With `-H`, the same names end up sharing inodes as with rsync, but the name shown after
  `=>` can differ. rsync links each group to the last name in its file list; barqsync's
  parallel walk links them to the first name it reaches.
- `--progress` and `--info=progress1` show one overall line, like `--info=progress2`.
  Files are copied in parallel, so per-file progress lines would interleave
  meaninglessly. Item lines (`-v`, `-i`, `--out-format`) come in the order the parallel
  transfer produces them, not in rsync's file-list order.
- `%b` in `--out-format` and `--log-file-format` is the file data sent for the item (its
  literal data, before `-z`): 3 for a new 3-byte file, where rsync, which counts the
  protocol bytes its transfer of the file took, shows 46. Both show 0 for an item that
  wasn't transferred.
- `--log-file` gets the messages barqsync shows, so with `-v` or `--stats` also its own
  "transferred N files, ..." line; and a failed run's log, like its stderr, has no "rsync
  error: ... (code N)" line. With `-q`, a local copy's log also gets the receiving side's
  notices ("cannot delete non-empty directory", "skipping non-regular file"), which
  rsync's receiving process, quiet as well, drops before they reach the log.
- In a push, `%U` and `%G` show the owner and group as the receiving side mapped them by
  name, where rsync's sender shows its own numbers. They differ only when the two
  machines number the user or group differently.
- `--info=flist2` without incremental recursion (`-P` with `--delete-before`,
  `--delete-after` or `-m`, or `-d`, `--files-from`): rsync shows the number of files
  before the transfer, barqsync once it is done. In a pull with `--delete-before` or
  `--delete-after`, rsync's receiving side also counts each destination directory it
  scans for deletions; barqsync doesn't. With `--existing`, the entries inside a
  directory it doesn't create aren't counted (here or in `--stats`' "Number of files"),
  since barqsync doesn't read that directory; rsync's file list has them.
  `--info=stats3` shows what `stats2` does (rsync adds memory statistics where its
  platform has them).
- `-R` with an absolute path: rsync shows the names with a leading `/` (`/private/...`),
  barqsync without it. The files land in the same place.
- rsync doesn't strip comment lines from a `--files-from` list it forwards to a remote
  sender, and it rejects names like `a/../b` there. barqsync handles both the same way
  in every direction.
- `--partial` keeps the contiguous received prefix of the files in flight. rsync handles one
  file at a time, so it keeps at most one partial file; barqsync can keep several.
- After a missing `--files-from` entry, rsync leaves out the "created directory" line;
  barqsync prints it.
- With `-K`, rsync on macOS also tries to chmod the symlink and fails ("Too many levels of
  symbolic links"); barqsync leaves the mode alone.
- `--delete-before` and `--delete-during` (`--del`, rsync's default) are accepted, but
  barqsync deletes after the transfer, as with `--delete-after`: the source is read in
  parallel, and nothing is deleted until all of it was read without errors. So
  `--delete-before` doesn't make room before new files arrive on a nearly full destination.
  Which entries go is the same, and with `--max-delete` also which ones are left.
- A file that can't be written (a full disk, a quota, an I/O error) is reported and the
  other files are still transferred, but nothing is deleted ("IO error encountered --
  skipping file deletion", exit 23): a file renamed in the source keeps its old name on the
  destination until the new one is there. rsync stops at the first write error (exit 11),
  before its `--delete-after` deletions; with `--delete-during` it may already have deleted.
- The destination's per-directory filter files (`:` rules, `-F`) are read when their
  directory is compared, before its files are updated, as rsync does with its default
  `--delete-during`. With `--delete-after` rsync reads the updated ones; barqsync still
  reads the old ones.
- A per-directory filter file that can't be read or has an invalid rule is reported, and
  what its directory holds is left out (exit 23, and nothing is deleted); rsync stops (exit 1).
- With `-x` and `--delete`, a mount point that a file must replace stays whole ("could not
  make way for new regular file", exit 23). rsync gives up too, but first deletes the
  files at the top of the mount point.
- With `-x`, a push or a pull of several sources into one destination (`a/ b dst/`) leaves
  the extra entries of a mount point where a named directory goes (`dst/b`). rsync deletes
  them, as barqsync does in a local copy, or for `b dst/` alone: the sources reach the
  receiver as one merged listing, which doesn't say which directories were named.
- With `-x`, a `-R` or `--files-from` path inside another named directory that is sent
  whole stops at a mount point on its way, as the rest of that directory does
  (`-R s s/mnt/file`, with `s/mnt` a mount point: barqsync sends nothing in `s/mnt`).
  rsync still sends the path, and with `-R s s/mnt` all of `s/mnt`.
- With `-x` and `-m`, a mount point that a later source fills (`a/ b/ dst/`, with `a/m` a
  mount point and `b/m` not) gets `b/m`'s attributes; rsync gives `dst/m` `a/m`'s, as
  barqsync does without `-m`.
- `--from0` applies to per-directory filter files on both sides. rsync tells its server
  about it only along with `--files-from`, so in a pull its sender reads them line by line.
- `--timeout` applies to remote transfers, where both sides send keepalives while they are
  busy; rsync's peers do so in fewer places, so it can also time out a busy peer. A local
  copy has no connection, and the option has no effect there.
- A source file that disappears after its directory was read gives rsync's "file has
  vanished" warning and exit 24. One that disappears between reading its directory and
  reading its metadata is skipped without a word (rsync warns about it too).
- `--delete-missing-args` leaves an excluded missing source's destination alone, locally and
  over ssh. Over ssh rsync's sender sends the excluded name anyway: its server then deletes
  it in a push, and its client refuses it in a pull ("rejecting excluded file-list name",
  exit 4).
- In a push without `--delete` or `-m`, rsync's server gets none of the filter rules, so
  what `--force` and `--delete-missing-args` delete there ignores receiver-side rules
  (`P`, protecting per-directory files). barqsync's server applies them, as a local copy
  or a pull does.
- For a path listed below a directory missing at the source, rsync says the directory
  vanished once for each such path; barqsync says it once.
- `--chown` or `--groupmap` followed by `--no-g` leaves groups alone, locally and remote.
  rsync's server turns `-g` back on when it is given the map, while its client sends no
  groups, so over ssh its chgrp calls fail (exit 23).

### Extended attributes and ACLs (`-X`, `-A`)

Both follow rsync's xattrs.c and acls.c, locally and over the wire, and each has
differential tests against rsync (`tests/xattrs_acls.rs`: locally and pushed and pulled over
the ssh pipe, TCP streams and QUIC; updates of attributes or ACLs alone, removed and
destination-only ones, symlinks, a read-only file, `-XX`, `--delete` and `-H`).

- **Which attributes.** Linux never copies `system.*`; a non-root receiver stores and
  compares only `user.*` (non-root senders can't list `trusted.*` anyway), root all the
  others. macOS has a single namespace: names travel as `user.NAME`, `rsync.NS` names (other
  systems' namespaces kept by a root receiver) as `NS`, and a non-root receiver drops names
  outside `user.`. rsync's own `rsync.%...` attributes (`user.rsync.%...` on Linux) need
  `-XX`. Extra attributes on the destination are removed. macOS keeps none on devices and
  FIFOs; on Linux symlinks only root's namespaces (`trusted.*`, ...) are written, since the
  kernel refuses `user.*` there, as rsync.
- **ACLs** (`-A` implies `-p`): POSIX access ACLs, and default ACLs on directories, on Linux
  (read and written as the kernel's `system.posix_acl_*` attributes); extended ACLs on macOS
  (`acl_get_link_np`/`acl_set_link_np`, ids through UUIDs, entries packed as rsync packs them,
  so `synchronize` is dropped as rsync drops it). What the mode says is not sent, and only
  the extended part is compared, as rsync's `rsync_acl_equal_enough`. Not for symlinks.
  ACLs from the other system aren't translated (they are left alone). Remote transfers map
  the users and groups named in ACL entries by name, like owners.
- **Comparing.** Attributes or ACLs that differ alone update the entry in place (`.f........x`,
  `.f.......a.`), never a transfer; a rewritten file shows them too (`>f.st......x`). As with
  rsync, a hard-linked name already linked to its leader shows `hf........x` when the
  leader's attributes changed (not for modes and ACLs). Plain `-v` shows none of this.
- **Writing.** After the owner and before the mode, as rsync's `set_file_attrs` (a later
  `chown` would drop a file capability); files get theirs on the temp file before the
  rename. An entry the owner can't write gets `u+w` for the moment. Directories get theirs
  when created, their mode last.
- **Over the wire.** Listings carry the attributes, with values over 32 bytes as a length
  and BLAKE3-128 checksum (rsync's limit); the full values come with the file's data when it
  is sent anyway, or in an `Xattrs` reply to the listing's `Need` for an in-place update.
  Protocol version 9: older peers are refused with "protocol version mismatch".
- **Clones.** An APFS clone carries its source's attributes, so without `-X` they are removed
  from it, leaving the file as rsync's freshly written one (the kernel-managed
  `com.apple.provenance` stays, as on any new file). Clones made without `CLONE_ACL` already
  get the destination directory's inherited ACL, like a new file.
- **One difference:** rsync rewrites every attribute value of an entry it updates, barqsync
  only those that differ. Where a value can't be written but is already identical (as root
  in a container, the SELinux labels: `lsetxattr(..., "security.selinux") failed: Operation not
  supported`), rsync ends with exit code 23 and barqsync succeeds.

Measured (release build, warm cache; rsync 3.5.1 on macOS, 3.2.7 on Linux). macOS, APFS, M-series
Mac, 10 cores; every file there also carries the system's `com.apple.provenance`, so `-X`
reads one attribute per file even in the plain tree; "tagged" adds a short and a 100-byte
`user.` attribute to every file:

| macOS | barqsync `-a` | barqsync `-aX` | barqsync `-aA` | rsync `-a` | rsync `-aX` | rsync `-aA` |
| --- | ---: | ---: | ---: | ---: | ---: | ---: |
| 5,000 files, fresh copy | 426 ms | 444 ms | | 1151 ms | 1285 ms | |
| 5,000 tagged, fresh copy | 424 ms | 752 ms | | 1151 ms | 1585 ms | |
| 5,000 files, no-op | 11 ms | 97 ms | 33 ms | 57 ms | 295 ms | 144 ms |
| 5,000 tagged, no-op | 12 ms | 210 ms | 31 ms | 49 ms | 657 ms | 153 ms |
| 500,000 files (C), no-op | 2.98 s | 13.2 s | 4.60 s | 9.78 s | 31.9 s | |

APFS makes extended-attribute calls expensive: ~10 µs of kernel time each, and they contend
(the first build took 21.3 s on the 500k no-op, with 157 s of system CPU). Two changes got it
to 13.2 s: a value is read in one call into a reusable buffer instead of asking for its size
first, and at most 3 threads make attribute calls at once (2: 14.6 s, 4: 13.3 s, 8: 16.7 s,
unlimited: 20.5 s; `BARQSYNC_XATTR_LANES` overrides). What remains is four calls per file
(list and read, on both sides), against rsync's six.

On Linux (container on the same Mac, tmpfs, non-root, 5 cores) attribute calls are cheap:

| Linux | barqsync `-a` | barqsync `-aX` | barqsync `-aA` | rsync `-a` | rsync `-aX` | rsync `-aA` |
| --- | ---: | ---: | ---: | ---: | ---: | ---: |
| 5,000 files, fresh copy | 17.8 ms | 18.8 ms | 18.8 ms | 109 ms | 115 ms | 113 ms |
| 5,000 tagged, fresh copy | 15.6 ms | 25.3 ms | 15.1 ms | 116 ms | 125 ms | 114 ms |
| 5,000 files, no-op | 3.3 ms | 6.5 ms | 4.7 ms | 55 ms | 64 ms | 62 ms |
| 5,000 tagged, no-op | 2.7 ms | 9.0 ms | 4.7 ms | 59 ms | 87 ms | 62 ms |

## Remote transfers

```bash
barqsync -avz src/ user@host:/backup/src/        # push
barqsync -av user@host:/data/ ./data/            # pull
barqsync -av -e "ssh -p 2222" --remote-path=/opt/bin/barqsync src/ host:dst/
```

Like rsync, the client starts `barqsync --server` on the other machine through the remote
shell (`ssh` by default, `-e` or `BARQSYNC_RSH` to change it, `--remote-path` when the binary
isn't on the remote `PATH`). barqsync has to be installed on both ends. Either side can be the
sender, and both sides run the same code:

- **Pipelined listings.** The sender scans in parallel and streams one listing per directory
  as soon as it is read. The receiver diffs each listing against its own directory as it
  arrives, so the transfer starts before the scan ends.
- **One round trip per directory, not per file.** For each listing the receiver answers with
  one request naming the files it needs. For files it already has an older copy of, the
  request carries a coarse basis signature: aligned blocks when the size is unchanged,
  FastCDC chunks otherwise. Where the coarse delta leaves enough data unmatched, the sender
  asks for fine signatures of just those parts (see [Delta transfer](#delta-transfer)).
- **Small files are packed.** Files up to 128 KiB travel many-per-frame in ~4 MiB packs and
  are written by the receiver's thread pool.
- **Deltas over the wire.** Changed files come back as copy ranges plus only the missing
  bytes. The receiver clones its old file and patches it where it can. Delta is the default
  remotely, as in rsync; `-W` sends whole files.
- **Compression.** `-z` zstd-compresses each frame on the thread that produced it, so
  compression runs in parallel.
- **No pipe deadlocks.** Each side has a dedicated reader and writer thread.
- **Output.** Errors and `-v` lines produced on the server are forwarded to the client and
  escaped there for the client's terminal.

### Parallel data streams (`--streams N`)

A single ssh channel often can't fill a fast or long-distance link: one TCP flow, one
encryption thread, ssh's own window. With `--streams N`, file data moves to N extra TLS 1.3
connections and control messages stay on ssh:

1. **Offer.** Over the already authenticated ssh connection, the server listens on the address
   the client reached it at (`SSH_CONNECTION`). It sends that address, the BLAKE3 fingerprint
   of a freshly generated self-signed certificate, and a random 32-byte token.
2. **Connect.** The client opens N TLS connections that accept only that exact certificate
   and presents the token on each, inside TLS. Nothing on these connections is trusted
   without both checks.
3. **Use.** Every frame of a file goes over the same stream, so it stays in order. Small-file
   packs spread across streams. Each stream ends with an end-of-stream marker, and the
   receiver finishes only after all of them and the sender's final ssh message. A stream
   that ends without its marker, or a write that fails, ends the transfer, but first the
   control connection gets up to 2 s to say why. A side that is interrupted closes its
   streams too, and its notice travels on ssh, so it can arrive after their end. The
   interrupt then decides the exit code (20, not 12). A side that stops sends its notice
   before it closes its streams.
4. **Through ssh when blocked.** If no direct connection opens within 5 s (a firewall on the
   server, NAT, a VPN that only lets ssh in), the client opens the same N streams over N
   extra ssh connections instead. Each runs `barqsync --stream-relay` on the server, which
   passes bytes between its stdin/stdout and the stream listener on that machine (the
   listener also listens on loopback, on the same port, for this). TLS with the pinned
   certificate and the token still runs end to end, so the relay only sees ciphertext. No
   firewall rule is needed, and several TCP connections still carry the data. With ssh as
   the remote shell, relays never prompt (`BatchMode`) and never share a multiplexed master
   (`ControlPath=none`), so they need key or agent authentication; `-v` says when they are
   used. The relay copies with plain reads and writes: with `io::copy`, which splices between
   pipe and socket on Linux, the TLS handshake stalled after the server's first flight.
   `bench/net/container.sh firewall` checks all this with real OpenSSH behind an iptables rule
   that resets everything but ssh.
5. **Fall back.** If the relays fail too, both sides carry on over the ssh pipe, and the
   message says why each way failed.

`-vv` reports how many streams are in use. Encryption and `-z` compression then run on one
thread per stream instead of in a single ssh process. `--streams N` on its own means
`--transport tcp`.

### QUIC (`--transport quic`)

The original plan's transport. Setup works like `--streams`: over the authenticated ssh
connection the server opens a UDP endpoint on the address the client reached it at, with a
fresh self-signed certificate (TLS 1.3, ALPN `barqsync`), and sends the address, the
certificate's BLAKE3 fingerprint and a random token. The client connects within 3 s,
accepting only that certificate, and presents the token on the first stream; the server
takes the first connection that presents it (vetting connections side by side, so one that
never presents a token can't hold up the client's) and acknowledges it with one byte. The
client doesn't wait for that byte before moving to QUIC: the pinned certificate already
proves it reached this server, so waiting would cost a round trip for nothing. The byte is
checked with the first read. Then:

- **Everything moves to QUIC.** Control messages and directory listings use that first
  (bidirectional) stream; file data uses `--streams` unidirectional streams (default 16, up
  to 128), routed by file id exactly as over TCP. The ssh connection stays open, idle, until
  the end.
- **Fallback.** If UDP doesn't get through in 3 s, both sides carry on over the ssh pipe
  (`-v` says so).
- **Lost peers.** The idle ssh pipe is still watched. A peer that dies ends it at once, and
  the transfer ends with exit 12 after the control stream has had 2 s to say why, as when a
  data stream ends early. The same happens when only the ssh connection drops while QUIC
  stays up: rsync dies with its ssh, the server's stderr reaches the user only through it,
  and the user stops a remote side through it. A peer that stays connected but no longer
  answers is noticed by QUIC's idle timeout (15 s, with keep-alives).
- **Clean shutdown.** QUIC resends lost packets only while the process lives, so nothing may
  close before the peer's application has what it needs. The protocol says when that is:
  the sender, once it has the receiver's final `ReceiverDone`, knows the receiver has read
  everything, and closes the session and its ssh pipe at once. The receiver ends its
  streams and waits (up to 5 s) for the sender to end too: a QUIC close or stream end, or
  the idle ssh pipe reaching its end, which covers a QUIC close lost on the way (TCP keeps
  resending after a process exits). Neither side waits for acknowledgements. A side that
  stopped (interrupted, or failed) waits the same way, so that its notice gets resent until
  the peer has it, but it also stops waiting once nothing at all has arrived from the peer
  for 1 s (or three round trips). A running peer acknowledges what is outstanding within a
  round trip, so such a peer either has everything or is stopped or gone. Ctrl-C with a
  stopped server then ends within about 4 s over QUIC, as over ssh. See
  [QUIC's fixed cost](#quics-fixed-cost-m19). One exception: in a pull with
  `--remove-source-files` the server (the sender) removes the last files after
  `ReceiverDone`, so it ends with `RemovalsDone`, after their `-vv` lines and errors, which
  the client reads before finishing; then the client closes at once and the server waits.
  Over QUIC that costs such a pull a round trip; the ssh pipe doesn't wait (protocol 10).

quinn drives a connection from one task, which limits QUIC to about 350 MB/s per transfer on
this Mac over loopback; TLS-over-TCP streams scale with the number of streams.

### Automatic choice (`--transport auto`)

QUIC is 2–4× faster than the ssh pipe on a long, lossy link, ties it on a clean WAN, is a few
percent slower on a LAN, and costs `--transport quic` 3 s wherever UDP is blocked.
`--transport auto` decides per transfer, before the first listing:

1. **Offer.** The server opens its QUIC endpoint and sends the offer over ssh, as for
   `--transport quic`. (The server is told `quic`: an older server can still read the
   handshake and say it speaks another protocol version.)
2. **Ping, and connect.** The client pings the server over ssh and starts connecting to
   QUIC at the same time, so on a long link the handshake is done about when the pong
   arrives. A round trip under 30 ms is a short link: the attempt is dropped and the
   transfer stays on the ssh pipe. The ping is all it costs (the handshake's few UDP
   packets go out in parallel).
3. **Wait for QUIC, briefly.** On a longer round trip the client waits for QUIC until 300 ms
   plus two round trips after the ping went out (the handshake takes one). If QUIC
   connected, and its own handshake also measured 30 ms or more (a ping that ssh had to
   resend looks long), the transfer moves to QUIC, with `--streams` data streams (16 by
   default). Otherwise it stays on ssh.
4. **Remember.** A host where QUIC didn't connect is not tried again for an hour. Nor is a
   host where QUIC's last transfer found a clean link (below): those stay on the ssh pipe
   for an hour, without a ping. The markers are empty files per host (named by a hash of
   the host, without `user@`) in `$XDG_RUNTIME_DIR/barqsync` (gone at logout), else
   `~/Library/Caches/barqsync` (macOS) or `~/.cache/barqsync`; `BARQSYNC_CACHE_DIR` moves
   them.

`-vv` says what auto chose and why, e.g. `auto: 81.2 ms round trip, QUIC connected in 85
ms, using QUIC to 10.0.0.5:50612 (16 data streams)` or `auto: 0.4 ms round trip, using the
ssh connection`, and after a QUIC transfer what it learned (`auto: QUIC lost 0 of 11677
packets, a clean link: the ssh connection next time`); `-v` output stays rsync's. Both QUIC
ends assume a 50 ms round trip until they have measured one (quinn's default of 333 ms
re-sends a lost first flight only after ~1 s), so one lost handshake packet on a lossy link
doesn't miss auto's short wait. The setup step is new on the wire: the protocol version is
8, and older servers refuse the connection with "protocol version mismatch".

Loss can't be measured up front: the handshake is a handful of packets, too few to see a 1%
loss rate, and a probe burst large enough to see it costs more on a clean WAN than QUIC
saves there. A long round trip is the first signal. A QUIC transfer then counts its own
losses, and the client remembers the verdict:

- **Lossy:** any packet lost. QUIC recovers from loss far better than the ssh pipe's single
  TCP connection, which shrinks its window at every loss and, 80 ms away, takes long to grow
  it back; so it stays QUIC.
- **Clean:** no loss in at least 1,000 packets of data this side sent (at the lossy
  profile's 0.5% a link that size hides all its losses about once in 150), at a rate the
  ssh pipe can carry: the next hour's transfers to that host stay on the pipe, which then
  costs nothing over `--transport ssh`.
- **Fast:** no loss, but more data per round trip than three quarters of the ssh pipe's
  2 MiB channel window (OpenSSH's default) allows: the pipe would hold it back, so it
  stays QUIC.
- **Too few to tell** (a no-op, or a pull, where the client mostly acknowledges): nothing
  changes.

A clean verdict judged wrongly, or a link that turns lossy within the hour, costs what
`--transport ssh` would. So does a clean link fast enough to outrun the pipe's window when
the transfers QUIC saw were too small to show it. **The ssh pipe stays the default** (see
[QUIC's fixed cost](#quics-fixed-cost-m19) for what keeping auto opt-in costs); use
`--transport auto` for links that are long and may be lossy (Wi-Fi, mobile, across
continents), especially where UDP may be blocked. See
[`--transport auto`, measured](#--transport-auto-measured).

### Wire format (`--wire postcard|rkyv`)

Control messages (listings, requests, signatures) are postcard-encoded by default.
`--wire rkyv` uses the original plan's rkyv instead: fixed-layout archives, checked with
bytecheck before use, with non-UTF-8 names carried as raw bytes. Each frame says which
encoding it holds, so the session agrees on one in `Hello` (always postcard, so any version
can read it). File data frames are raw either way. rkyv measured no faster (see
[Transports](#transports-and-wire-formats-measured)): the messages are decoded into owned
structures anyway, so its zero-copy reads don't come into play.

### Connection layer

The plan had tokio own all network I/O. QUIC runs on a small tokio runtime (2 worker
threads); the protocol's reader and writer threads reach it through blocking bridges. For the
ssh pipe and TCP streams a tokio layer is built as well (reader and writer tasks, the ssh
pipe through non-blocking descriptors, TLS through tokio-rustls), behind the hidden
`--net-io=tokio`. The thread-based layer stays the default because it measured at least as
fast.

### Transports and wire formats, measured

Through the local fake-ssh shim (no link: CPU and protocol overhead only; macOS, 3 runs,
`bench/transport_modes.sh` and `bench/interleave.py`):

| Mode | A: fresh push, 200k files (interleaved) | C: 3 changed of 500k files |
| --- | ---: | ---: |
| ssh pipe, postcard | 34.8 s | 4.33 s |
| ssh pipe, rkyv | 34.9 s | 4.34 s |
| `--transport tcp --streams 4` | 35.0 s | 4.30 s |
| `--transport quic` (16 streams) | 34.5 s | 4.53 s |
| `--transport quic --wire rkyv` | 34.8 s | 4.66 s |

On one machine the fresh push is disk-bound whatever carries it, and the metadata-bound no-op
is a few percent slower over QUIC (userspace UDP and crypto on top of the same work). What a
transport is for shows on a link. Over real sshd shaped with `tc netem` in the Linux
container (`bench/net/net_bench.sh`, datasets at `SCALE=0.5`, 3 runs; lan = 1 Gbit/0.2 ms,
wan = 100 Mbit/20 ms, lossy = 20 Mbit/80 ms with ~1% loss):

| Profile, workload | rsync | ssh pipe | ssh, tokio I/O | `--transport tcp` (4) | `--transport quic` | quic, `--wire rkyv` |
| --- | ---: | ---: | ---: | ---: | ---: | ---: |
| lan, 4,000 small files | 0.35 s | **0.31 s** | 0.31 s | 0.32 s | 0.33 s | 0.32 s |
| lan, 3 changed of 20,000 | 0.26 s | **0.18 s** | 0.18 s | 0.19 s | 0.19 s | 0.20 s |
| wan, 4,000 small files | 2.24 s | **2.07 s** | 2.07 s | 2.39 s | 2.08 s | 2.12 s |
| wan, 128 MiB delta | 1.65 s | 1.56 s | 1.59 s | 1.75 s | **1.55 s** | 1.54 s |
| wan, 3 changed of 20,000 | 1.21 s | 0.67 s | **0.64 s** | 0.78 s | 0.71 s | 0.84 s |
| lossy, 4,000 small files | 38.6 s | 38.2 s | 39.5 s | 13.0 s | **9.1 s** | 11.8 s |
| lossy, 128 MiB delta | 18.6 s | 21.1 s | 20.3 s | 15.4 s | **9.3 s** | 13.1 s |
| lossy, 3 changed of 20,000 | 4.0 s | **2.1 s** | 2.2 s | 2.8 s | 2.3 s | 5.3 s (±3.5) |

QUIC matches the ssh pipe on a clean WAN and is a few percent slower on a LAN, but on a lossy,
long link it is 2–4× faster than the ssh pipe and faster than TCP streams too: one connection
recovers from loss per stream instead of stalling every byte behind a lost packet. It costs a
UDP port on the server, and up to 3 s per run where UDP is blocked. So the ssh pipe stays the
default, and `--transport quic` is the choice for lossy or long-distance links. rkyv and the
tokio I/O layer are within noise of postcard and threads, which stay the defaults.

(The first network run had QUIC 1.6–2× slower than the ssh pipe on LAN and WAN: the control
stream queued behind 256 MiB of buffered file data, and closing waited out QUIC's closing
period. Prioritizing the control stream, right-sizing the windows and a FIN-for-FIN close
fixed both; `bench/results/net-20261017-095932.md` is that first run.)

### `--transport auto`, measured

Same container and datasets (`bench/net/net_bench.sh`, `SCALE=0.5`), plus `far` = 100 Mbit
with 80 ms round trips and no loss, and `-noudp` profiles that drop every UDP packet (a
firewall that blocks QUIC). 5 runs on lan and wan, 6 on lossy, 3 elsewhere; lossy is noisy
for every transport (one run of the same command can take twice as long as another).

| Profile, workload | ssh pipe | `--transport quic` | `--transport auto` | auto chose |
| --- | ---: | ---: | ---: | --- |
| lan (0.7 ms), 4,000 small files | 0.32 s | 0.34 s | **0.32 s** | ssh |
| lan, 128 MiB delta | 0.41 s¹ | 0.34 s | **0.34 s** | ssh |
| lan, 3 changed of 20,000 | 0.24 s¹ | 0.19 s | **0.19 s** | ssh |
| wan (22 ms), 4,000 small files | 2.11 s | **2.04 s** | 2.05 s | ssh |
| wan, 128 MiB delta | **1.50 s** | 1.54 s | 1.52 s | ssh |
| wan, 3 changed of 20,000 | **0.63 s** | 0.70 s | 0.65 s | ssh |
| lossy (82 ms, ~1% loss), 4,000 small files | 38.8 s | 15.4 s | **13.0 s** | QUIC |
| lossy, 128 MiB delta | 15.6 s | 7.4 s | **7.1 s** | QUIC |
| lossy, 3 changed of 20,000 | 2.64 s | 3.08 s | **2.43 s** | QUIC |
| far (82 ms, no loss), 4,000 small files | 3.55 s | **3.54 s** | 3.60 s | QUIC |
| far, 128 MiB delta | **3.06 s** | 3.41 s | 3.39 s | QUIC |
| far, 3 changed of 20,000 | **1.85 s** | 2.17 s | 2.25 s | QUIC |

¹ The ssh pipe's first run was slow; its fastest runs (0.32 s and 0.18 s) match auto's, which
ran the same pipe.

Where UDP is blocked (3 changed of 20,000):

| Profile | ssh pipe | `--transport quic` | auto, first contact | auto, host remembered |
| --- | ---: | ---: | ---: | ---: |
| wan-noudp | 0.62 s | 3.63 s | 0.65 s | 0.65 s |
| far-noudp | 1.92 s | 4.87 s | 2.41 s | **1.86 s** |
| lossy-noudp | 3.06 s | 5.33 s | 2.95 s | 3.08 s |

What this shows:

- **auto does what it was built for.** On a LAN and a WAN it stays on the ssh pipe; on the
  lossy link it is as fast as QUIC, 2–3× faster than the pipe. In 40 setups on the lossy link
  QUIC connected every time, in 166–185 ms (two round trips), well inside the 464 ms wait.
  Where UDP is blocked it loses 0.5 s on first contact (`--transport quic`: 3 s) and nothing
  once the host is remembered; on a WAN it doesn't even try.
- **It costs a round trip when it stays on ssh:** the ping. That shows only on the WAN no-op,
  0.65 s against 0.63 s (+4%, every time).
- **On a clean long link it picks QUIC, and QUIC is slower there.** A one-file transfer over
  `far` (10 runs each) takes 1.44 s over the pipe, 1.70 s over QUIC and 1.77 s with auto:
  QUIC's setup and close cost about three round trips (the handshake and token two, the
  close one more: data streams wait for the peer's acknowledgement and both control streams
  end before the idle ssh pipe closes), auto's ping a fourth. That makes the delta and the
  no-op 11–22% slower, and at 100 Mbit QUIC gains nothing on the small files to make up for
  it. The round trip can't tell a clean long link from a lossy one, and loss can't be seen
  cheaply beforehand.
- **QUIC's lead on the lossy link is not a quirk of the emulation.** Loopback hands netem
  TCP segments of up to 44 packets (GSO), and a loss drops all of them. With `GSO=off`, TCP's
  losses are single packets, and the pipe gets slower, not faster: 4,000 small files 56.3 s
  (QUIC 12.9 s), the delta 23.0 s (QUIC 7.1 s).

So **the ssh pipe stays the default** and `--transport auto` is opt-in, like
`--strategy adaptive`: a large win where links are long and lossy, a small fixed cost
elsewhere. Reports: `bench/results/net-20261020-070426.md` (lan, wan),
`-070637.md` (lossy, far), `-071421.md` (UDP blocked), `-071612.md` (GSO off),
`-073038.md` (lossy, 6 runs).

### QUIC's fixed cost (M19)

M17 measured QUIC and auto costing about three round trips more than the ssh pipe per
transfer on a clean 80 ms link. To see where, `BARQSYNC_TIMING=1` prints timestamped
milestones on both sides (the client passes it to the server's command line; the server's
lines come back through ssh's stderr): the handshake, the transport's setup, `SenderDone`
and `ReceiverDone`, each step of closing, the remote shell's exit. `bench/net/container.sh
fixed [profiles]` (`bench/net/fixed_cost.sh`) pushes and pulls one 1 KiB file per transport
10 times and breaks each run down from both sides' milestones (one clock: both run in the
container). On `far` (82 ms, no loss), medians in ms, before (`dc4522d`) → after:

| Direction, transport | total | transport setup | transfer | close | over the ssh pipe |
| --- | ---: | ---: | ---: | ---: | ---: |
| push, ssh pipe | 1443 → 1438 | 0 | 170 → 169 | 3 → 2 | |
| push, `--transport quic` | 1693 → **1617** | 175 → **89** | 168 → 170 | 87 → 85 | +250 → **+179** |
| push, `--transport auto` | 1786 → **1609** | 255 → **89** | 170 → 169 | 88 → 85 | +343 → **+171** |
| pull, ssh pipe | 1441 → 1435 | 0 | 86 → 85 | 86 → 85 | |
| pull, `--transport quic` | 1774 → **1620** | 173 → **89** | 169 → 170 | 169 → **88** | +333 → **+185** |
| pull, `--transport auto` | 1855 → **1616** | 255 → **89** | 169 → 171 | 168 → **87** | +414 → **+181** |

(Connecting ssh itself takes 1.27 s of every run, 15 round trips, whatever the transport.)
Where the time went, and what changed:

- **Setup, 2 round trips → 1.** The QUIC handshake is one; the client then waited another
  for the server's one-byte acknowledgement of the token. It doesn't any more (see
  [QUIC](#quic---transport-quic)): the pinned certificate already proves which server it
  reached. auto's ping came first, a third round trip; now QUIC connects alongside it.
- **Closing a pull, 2 round trips → 1, the ssh pipe's own.** The client's stream end waited
  for an acknowledgement, and the server, once it had `ReceiverDone`, waited for its own
  end's acknowledgement and for the client's end before exiting. Now the sender closes the
  moment it has `ReceiverDone` and exits. (The data streams, suspected first, never cost
  anything: the receiver stops each one as soon as it has read its end marker, which settles
  the sender's wait at once.)
- **Closing a push stays at 1 round trip.** The receiver is the server here: it must hear
  that the client has `ReceiverDone` before it exits, since QUIC resends nothing once the
  process is gone, and the client sees the remote shell exit half a round trip later. Over
  the ssh pipe the server exits at once because TCP keeps resending for it. In one lossy run
  in ten the client's QUIC close was lost on the way, and the ssh pipe's end ended the
  server's wait just as soon.
- **Left: about two round trips** per transfer. One is the handshake, which can only start
  when the offer arrives over ssh. The other is the push's close, or in a pull the server's
  late start (it learns of the switch over ssh, then its first listing crosses QUIC: the
  transfer phase above is one round trip longer than over the pipe).

Then **auto remembers clean links** (see [Automatic choice](#automatic-choice---transport-auto)),
so a host that QUIC found clean costs nothing over the pipe for the next hour. Over the
network benchmark (`bench/net/net_bench.sh`, `SCALE=0.5`, 3 runs; before = `dc4522d`, run
right before this branch on the same machine; lossy is noisy for every transport):

| Profile, workload | ssh pipe | `--transport quic` | `--transport auto` | auto, host remembered |
| --- | ---: | ---: | ---: | ---: |
| far, 4,000 small files | 3.81 / 3.69 s | 3.65 → 3.49 s | 3.63 → **3.50 s** | 3.64 s¹ |
| far, 128 MiB delta | 3.11 / 3.15 s | 3.32 → 3.29 s | 3.40 → **3.26 s** | **3.12 s** |
| far, 3 changed of 20,000 | 1.86 / 1.95 s | 2.20 → 2.12 s | 2.28 → **2.13 s** | **1.88 s** |
| lossy, 4,000 small files | 36.0 / 45.6 s | 16.3 → 12.5 s | 9.0 → 14.1 s | 14.2 s |
| lossy, 128 MiB delta | 14.9 / 17.5 s | 6.4 → 10.2 s | 11.4 → 5.9 s | 6.4 s |
| lossy, 3 changed of 20,000 | 2.23 / 2.05 s | 2.29 → 2.22 s | 2.45 → 2.21 s | 2.22 s |
| wan, 4,000 small files | 2.04 / 2.06 s | 2.05 → 2.04 s | 2.06 → 2.09 s | 2.06 s |
| wan, 128 MiB delta | 1.52 / 1.51 s | 1.56 → 1.53 s | 1.55 → 1.58 s | 1.51 s |
| wan, 3 changed of 20,000 | 0.655 / 0.648 s | 0.737 → 0.701 s | 0.682 → 0.663 s | 0.690 s |
| lan, 4,000 small files | 0.307 / 0.308 s | 0.319 → 0.311 s | 0.312 → 0.307 s | 0.305 s |
| lan, 128 MiB delta | 0.303 / 0.296 s | 0.314 → 0.323 s | 0.300 → 0.307 s | 0.306 s |
| lan, 3 changed of 20,000 | 0.177 / 0.178 s | 0.185 → 0.181 s | 0.175 → 0.178 s | 0.180 s |

The ssh pipe column gives both runs (before / after), since each comparison is with its own
run. ¹ The first run, before anything was remembered, went over QUIC; it found the link clean
(`lost 0 of 11677 packets`), so the later runs and the other workloads used the pipe.

- **On the clean long link** auto's cost over the pipe fell from 9% to 3.5% on the delta and
  from 23% to 9% on the no-op; on the small files it is 5% faster than the pipe. Once the
  host is remembered clean, it runs at the pipe's speed.
- **On the lossy link** QUIC and auto stay well ahead of the pipe on small files and the
  delta (1.3–4× in these runs; one run of a command there can take twice as long as
  another, so the before/after differences are noise). Every transfer there lost packets
  (`lost 59 of 11726`), so auto keeps QUIC.
- **On lan and wan** auto stays on the pipe as before: within noise, plus the ping on the
  WAN no-op (+2%).
- **Where UDP is blocked** (the `-noudp` profiles, no-op), first contact now costs 0.42 s on
  the 80 ms links (it was 0.49 s: the wait starts with the ping now), nothing beyond the ping
  on the WAN, and nothing once the host is remembered (`net-20261020-090255.md`).

**Is auto the default now?** By these numbers it is close: it never costs more than 9% (a
no-op on a clean long link, at first contact or once an hour), 2% where it stays on the pipe,
and is 1.3–4× faster on long lossy links. The ssh pipe stays the default for now, for what the
numbers don't show: a QUIC endpoint (a UDP port) on every server for every transfer, which
the macOS application firewall asks about for an unsigned binary; half a second lost on
first contact where UDP is blocked; and no fast long real link measured yet (1 Gbit at
80 ms, where the remembered-clean rule is least certain). Reports:
`bench/results/fixed-cost-20261020-081323.md` (before), `-082035.md` (the close),
`-082904.md` (the setup); `net-20261020-085043.md` (before), `net-20261020-083939.md` (after),
`net-20261020-090255.md` (UDP blocked, after).

### Adaptive strategy (`--strategy adaptive`)

The original plan's §4: measure the link once, then choose per file.

1. **Probe.** Before the first listing the sender pings the receiver for the round-trip
   time, then sends 256 KiB of incompressible data over the path file data will take (spread
   over every data stream), timed until the receiver acknowledges it. Only if that looks like
   500 Mbit/s or more does a 4 MiB burst follow for an accurate figure, so a slow link pays
   for a quarter megabyte and two round trips. Probe frames say which burst they belong to,
   so they may arrive in any order. `-vv` prints the result.
2. **Routing** (the receiver, per file): up to 128 KiB packed, never as a delta; 128 KiB to
   32 MiB sent whole on links of 1 Gbit/s or more (hashing both copies costs more than
   sending the file) and as a delta below that; larger files always as deltas. `-W` and
   `--no-whole-file` override it and skip the probe.
3. **Compression** (the sender): small-file packs are zstd-compressed even without `-z`, at
   level 1 from 1 Gbit/s, 3 from 100 Mbit/s and 6 below, stepped down whenever compressing
   (on all cores) would fall behind the link. Incompressible packs go raw, as before. `-z`
   compresses all data at that level; `--compress-level N` fixes the level.

Measured in the same network run (half-scale datasets; `text` = 2,000 compressible text
files of 8–32 KiB, `mid` = 20 files of 4 MiB with 1 KiB changed):

| Profile, workload | `--strategy fixed` (ssh) | `--strategy adaptive` (ssh) | fixed, quic | adaptive, quic |
| --- | ---: | ---: | ---: | ---: |
| lan, small files | **0.31 s** | 0.36 s | 0.33 s | 0.37 s |
| lan, text | 0.52 s | 0.37 s | 0.54 s | **0.36 s** |
| lan, mid | **0.26 s** | 0.29 s | 0.26 s | 0.30 s |
| wan, small files | **2.07 s** | 2.16 s | 2.08 s | 2.22 s |
| wan, text | 4.03 s | **1.99 s** | 4.12 s | 1.99 s |
| wan, 3 changed of 20,000 | **0.67 s** | 0.74 s | 0.71 s | 0.79 s |
| lossy, text | 99.2 s | 28.7 s | 48.6 s | **13.1 s** |
| lossy, 128 MiB delta | 21.1 s | 13.2 s | 9.3 s | **7.8 s** |

Compressing packs by default pays off on compressible data: 1.4× on the LAN, 2× on the WAN,
7.6× on the lossy link together with QUIC. On incompressible data the strategy costs 4–22%:
the probe (two round trips plus 4 MiB on a fast link, ~40 ms on the LAN) and compressing packs
that then go out raw. Mid-size files gain nothing on these links (they stay deltas below
1 Gbit/s). Since neither strategy is faster across the board, `--strategy fixed` stays the
default; `--strategy adaptive` (with `--transport quic` on lossy links) is the choice for
compressible data over slower links.

### io_uring (`--io-uring`, Linux)

The original plan's Phase 4.1. A receiver writes each small-file pack in three io_uring
submissions per pack (up to 256 operations each) instead of three syscalls per file: create
every temp file (`openat`), write every file, close every file. Owner, mode, times and the
rename stay ordinary syscalls. Each worker thread keeps its own ring; if the kernel or a
seccomp profile refuses io_uring, the normal path is used. The plan named tokio-uring; this
uses the `io-uring` crate it is built on, since batching is the point and tokio-uring
submits per future.

Remote push (fake ssh) of 20,000 files of 4 KiB in the Linux container (kernel 6.19, 5 runs,
`bench/uring.sh`):

| Receiver writes onto | normal | `--io-uring` | syscalls normal / io_uring |
| --- | ---: | ---: | ---: |
| tmpfs | 148 ms (±15) | 133 ms (±16) | 249,705 / 190,295 |
| XFS (the VM's disk) | **218 ms** (±28) | 265 ms (±41) | 249,529 / 190,377 |

A quarter fewer syscalls, but no measurable time saved: the filesystem work per file is the
same, and a syscall costs well under a microsecond. So `--io-uring` stays opt-in.

The first measurement showed `--io-uring` 2.8× faster on XFS, and not because of io_uring:
the normal path truncated every received file to its size even when it had just written
exactly that much, and XFS writes back a truncated file when it is closed (its protection
against the truncate-then-write pattern). Skipping the redundant truncate made the normal
path 3.2× faster on XFS (689 → 218 ms), for every transfer, with or without io_uring.

## Delta transfer

With `--no-whole-file` (and, from M4, by default for remote syncs), a changed file is rebuilt
from the old destination file plus only the ranges that differ:

1. **Aligned blocks first.** Both files are hashed in parallel in blocks at the same
   offsets. This is ideal for in-place changes and appends (VM images, databases, logs).
2. **FastCDC for moved data.** If under half the bytes matched (locally), or the file changed
   size (remotely, where neither side has both files), both files are chunked by content
   (16/32/64 KiB chunks), so inserted or deleted bytes only cost the chunks around them.
   Chunking runs in parallel over 4 MiB segments, joined so the cuts are exactly those of one
   sequential pass (see [`--delta=cdc`](#--deltacdc-the-original-plans-design)).
3. **Refinement.** These first signatures are coarse (64 KiB blocks, 32 KiB chunks), so they
   cost little whatever the file. What the coarse delta leaves literal is refined with fine
   sizes, a quarter of √size (4 KiB up to 256 MiB, 64 KiB beyond 16 GiB): for each literal
   run, the basis stretch between the same matched neighbours is signed again, with blocks on
   the basis's grid compared at the run's offset shift, and with chunks where the two
   stretches differ in length. Remotely that is one more round trip, taken only when the runs
   hold 64 KiB of basis data or more, so an append, whose only mismatch is the basis's last
   chunk, never pays for it. See [Two-level delta](#two-level-delta-m16).
4. **Clone + patch.** When matched ranges keep their offsets, the old file is cloned (APFS
   `fclonefileat`, Linux `FICLONE`) and only the changed ranges are written. Otherwise the
   new file is rebuilt in parallel. With `--inplace`, changes go straight into the destination.

Blocks and chunks are matched by BLAKE3 (128-bit), so every reused range is cryptographically
verified. That replaces the originally planned xxh3 + whole-file check, because a whole-file
check would have to re-read the entire output and cancel out clone+patch. BLAKE3 per block runs
at ~12.6 GB/s on all cores of an M-series Mac, faster than the SSD.

| Change to a 6 MiB file | bytes written, 0.1.1 | bytes written now |
| --- | ---: | ---: |
| 4 KiB overwritten in place (not block-aligned) | 64 KiB | 8 KiB |
| 300 KB appended | 300 KB | 300 KB |
| 777 bytes inserted at the start | 57.6 KB | 8.0 KB |
| 12 KB removed from the middle | 344.7 KB | 4.6 KB |
| truncated to 4 MiB | 0 B | 0 B |

(Literal data with `--no-whole-file --stats`, the same files for both builds.) After an
insert or delete, the chunk boundaries of the two files fall back into step within a few
chunks. Files of 128 KiB or less are always sent whole.

### Two-level delta (M16)

Comparing barqsync with [fast_rsync](https://crates.io/crates/fast_rsync) and rsync on real
transfers ([`fast-rsync-comparison.md`](fast-rsync-comparison.md)) showed that fixed 64 KiB
blocks and 32 KiB chunks made small or scattered edits expensive: a SQLite database that grew
by 3 MB after 8,000 SQL changes sent 193 MB, where rsync sent 104 MB.

- **M15** made the sizes fine (4 KiB) for every delta and sent a resized file both signatures.
  That fixed the database, but every changed file now paid for a fine signature (1–2 MB per
  256 MiB, back to the sender) and a second hashing pass, needed or not: appends and in-place
  runs got 4–22% slower.
- **M16** keeps 0.1.1's coarse first signature and refines only what it leaves literal. The
  sender sends `Refine` with the basis stretches between matched ranges and the fine sizes;
  the receiver answers `Refined` with their fine signatures; meanwhile the sender hashes its
  own side. Signatures now cost what the change needs: 0.06–0.9 MB here, against 0.04–0.6 MB
  in 0.1.1 and 0.4–2.0 MB in M15. Protocol version 7; older peers are refused with
  "protocol version mismatch".

Pushes over ssh to a 100 Mbit link with a 20 ms round trip (median of 3 runs, all 240 results
verified; bytes are data sent):

| Dataset | 0.1.1 | M15 | M16 | rsync 3.2.7 |
| --- | ---: | ---: | ---: | ---: |
| scatter: 256 MiB, 3,000 × 64 B overwritten | 12.38 s, 141.8 MB | 1.64 s, 12.3 MB | **1.63 s**, 12.3 MB | 4.46 s, 45.5 MB |
| sqlite: 250 MB database, 8,000 SQL changes | 16.71 s, 192.8 MB | **3.17 s**, 28.1 MB | 3.23 s, 30.1 MB | 9.38 s, 104.3 MB |
| csv: 78 MB, 300 rows rewritten | 1.67 s, 14.2 MB | 0.73 s, 2.1 MB | **0.66 s**, 1.8 MB | 0.80 s, 2.7 MB |
| tar: 54 MB archive, 50 files edited | 0.79 s, 3.8 MB | 0.58 s, 0.9 MB | **0.56 s**, 0.7 MB | 0.63 s, 0.9 MB |
| shift: 256 MiB, 40 inserts/deletes | 1.00 s, 4.6 MB | 0.99 s, 1.1 MB | **0.75 s**, 1.0 MB | 0.84 s, 1.3 MB |
| append: 256 MiB + 8 MiB | 1.30 s, 8.5 MB | 1.56 s, 8.4 MB | **1.29 s**, 8.5 MB | 1.50 s, 8.5 MB |
| inplace: 256 MiB, 8 × 1 MiB overwritten | **1.24 s**, 8.4 MB | 1.30 s, 7.9 MB | **1.24 s**, 7.9 MB | 1.37 s, 8.1 MB |
| mid: 40 × 4 MiB, 1 KiB changed each | 0.72 s, 2.6 MB | 0.72 s, 0.2 MB | 0.73 s, 0.2 MB | **0.69 s**, 0.5 MB |

On 1 Gbit the picture is the same: M16 is level with 0.1.1 on `inplace` (0.52 s) and `append`
(0.58 s), faster on the rest (`sqlite` 2.07 → 0.88 s, `scatter` 1.67 → 0.66 s, `shift` 0.59 →
0.53 s), and ahead of rsync except on `mid` (0.45 against 0.41 s). The fine sizes were chosen
by evaluating five rules on eight datasets (`bench/fast_rsync/src/bin/rules.rs`,
`bench/results/fast_rsync-rules-20261018.tsv`); `--delta=cdc` keeps its fixed 16/32/64 KiB
chunks.

### `--delta=cdc`: the original plan's design

`--delta=cdc` builds the delta engine the way the original plan describes it, for comparison:

- **FastCDC for every delta** (16/32/64 KiB chunks), no aligned-block pass. Files are chunked
  in parallel in 16 MiB regions, then stitched: the sequential chain continues from the last
  cut before each region until it lands on one of the region's own cuts. FastCDC restarts its
  hash at every cut, so from there both chains agree, and the result is exactly one sequential
  FastCDC pass over the whole file, whatever the thread count. A property test checks this at
  1, 2 and 8 threads.
- **`ChunkSignature { offset, length, xxh3_hash, blake3_hash }`.** An xxh3 hash table finds
  candidates; a match only counts when BLAKE3 (256-bit) agrees too. The sender computes BLAKE3
  only for chunks whose xxh3 is in the receiver's signature.
- **Files read through `mmap`** (memmap2) for hashing and for copy/literal ranges. The size is
  checked after mapping, and a file that changed since it was listed is sent whole. A file
  truncated *while* mapped can still raise `SIGBUS`.

Clone + patch, the in-order rebuild and `--inplace` work the same way for both designs.

Measured on the same Mac (warm cache, 3 interleaved runs, `bench/delta_modes.sh`):

| Dataset | Where | `--delta=blocks` | `--delta=cdc` |
| --- | --- | ---: | ---: |
| B: 10 GiB, 50 × 1 MiB overwritten in place | local, clone + patch | **5.9–6.3 s** | 42.5–47.3 s |
| B | local, `--no-clone` (rebuild) | **16.0–16.8 s** | 77.4–84.2 s |
| B | remote (fake ssh) | **6.0–6.1 s** | 37.9–43.3 s |
| E: 2 GiB, 40 inserts/deletes of 1–64 KiB | local, clone + patch | **2.8–3.5 s** | 3.2–3.5 s |
| E | local, `--no-clone` (rebuild) | 3.1–3.5 s | **2.2–2.5 s** |
| E | remote (fake ssh) | **2.6–2.8 s** | 3.6–4.6 s |

One idea from the plan did measure better and is now part of the default: exact stitching of
the parallel FastCDC regions. `blocks` used to force a cut at every 4 MiB segment edge, so
once content shifted, the chunks around every edge stopped matching: on E it sent 86.5 MB of
literal data. Its fallback now joins segments like `cdc` does (one read pass, `cdc_max`-byte
windows at the joins) and sends the same 4.1 MB as `cdc`, while getting a little faster
(remote E 2.8 → 2.7 s). On a 100 Mbit link those 82 MB are about 6.5 s.

Hashing itself is not what loses: on a cached 2 GiB file the cdc signature takes 230 ms against
300 ms for the blocks-mode chunk signature. The difference is how the data is read. Every first
touch of a mapped 16 KiB page is a page fault (about 1 s of CPU per 2 GiB on macOS), and mapped
pages go through the page cache. Two 10 GiB files are more than this Mac's 16 GB of RAM, so the
mapped reads evict and fault their way through, while `pread` with `F_NOCACHE` streams.
On Linux (the aarch64 container, VM disk, files scaled to 2.5 GiB for B and 512 MiB for E, so
B's two files still exceed the VM's 3.7 GiB of RAM) the picture is the same for big files and
better for `cdc` on small shifted ones, because mapped pages fault in cheaply there:

| Dataset | Where | `--delta=blocks` | `--delta=cdc` |
| --- | --- | ---: | ---: |
| B (2.5 GiB) | local, copy + patch | **2.0–2.5 s** | 4.8–6.2 s |
| B | local, `--no-clone` | **4.4–5.3 s** | 6.5–6.8 s |
| B | remote (fake ssh) | **2.1–2.8 s** | 2.7–3.0 s |
| E (512 MiB) | local, copy + patch | 0.7–1.3 s | **0.5–0.6 s** |
| E | local, `--no-clone` | 0.7–0.9 s | **0.37–0.43 s** |
| E | remote (fake ssh) | 0.7–0.8 s | 0.7–0.9 s |

`cdc` wins where a shifted file fits in RAM, most clearly when it is rebuilt without clones. There it hashes
each file once, while `blocks` first tries aligned blocks and only then falls back to chunks.
Those wins are under a second; the losses on big files are tens of seconds.
Following the defaults rule (the faster design stays the default), `--delta=blocks` remains the
default and `--delta=cdc` stays selectable.

## Scanner: `--scanner=dirtask|jwalk`

`--scanner=jwalk` runs the original plan's four-stage pipeline instead of one task per
directory:

1. **Scan.** `jwalk` walks the source tree on its own rayon pool. A `process_read_dir` hook
   stats each directory's children through a directory handle (`statx` on Linux, `fstatat` on
   macOS; directories of 512+ entries on 8 threads), prunes excluded subdirectories and `-x`
   mount points, and hands the listing on as soon as the directory is read. The destination
   tree is scanned at the same time into a `DashMap` index of directory → entries.
2. **Diff.** A listing is processed once its directory exists on the destination side (its
   parent creates it first), against the destination entries from the index, so there is no
   per-file `lstat`. Directories, symlinks and specials are handled inline in scan order;
   `--delete` candidates are collected per directory and removed only after scanning
   succeeds (since 0.4.1). Files are classified into queues: A metadata-only, B small
   (≤ 128 KiB), C large and new, D large and changed.
3. **Transfer.** A pool of worker threads per queue (2 / threads / threads÷2 / threads÷4).
4. **Commit.** Temp file + rename per file, as before; hard links and directory attributes
   once everything is written.

A directory the scan never lists (reached through a symlink with `-L`/`-k`, kept by `-K`, or
unreadable) is read directly once the scan is over. As a remote receiver, the destination is
indexed the same way while the sender's listings arrive. `-R`/`--files-from` and non-recursive
transfers always use `dirtask`. The macOS `getattrlistbulk` path has no hook in jwalk, so the
jwalk scanner doesn't use it.

Measured with `bench/scanner_modes.sh` (5 runs; macOS: APFS, full datasets; Linux: the
aarch64 container on tmpfs, datasets at `--scale 0.25`):

| Workload | macOS dirtask | macOS jwalk | Linux dirtask | Linux jwalk |
| --- | ---: | ---: | ---: | ---: |
| A: fresh copy (200k / 50k files), local | **18.1 s** | 19.7 s | **113 ms** | 184 ms |
| C: 3 changed of 500k / 125k files, local | **2.93 s** | 3.26 s | **42 ms** | 87 ms |
| C with `--delete`, local | **2.97 s** | 3.29 s | **42 ms** | 75 ms |
| A, remote (fake ssh) | 19.4 s | 18.5 s (±1.8) | 305 ms | 301 ms |
| C, remote (fake ssh, 10 runs on macOS) | 3.10 s | **2.88 s** | **80 ms** | 91 ms |

`dirtask` wins locally on both systems: it reads each directory's entries and attributes in
one pass (`getattrlistbulk` on macOS), stats in parallel within big directories, and starts
copying a directory's files as soon as it is listed. jwalk's pool, the index and the queues
add hand-offs without saving any syscalls. jwalk only won the macOS remote no-op (7%), where
the receiver indexes the destination while the sender's listings are still arriving. So
`dirtask` stays the default and `--scanner=jwalk` stays selectable. jwalk did use half the
memory on the fresh copy (92 vs 186 MiB), because its queues are bounded.


## Layout

```
src/cli.rs          flags → Options (filter order preserved across --include/--exclude)
src/chmod.rs        --chmod parser and mode tweaks
src/select.rs       -R / --files-from path selection (implied directories)
src/checksum.rs     -c whole-file BLAKE3 on a dedicated pool
src/ids.rs          user/group name mapping for remote transfers (--numeric-ids off)
src/xattrs.rs       -X: rsync's name rules, summed long values, compare and apply
src/acls.rs         -A: rsync's ACL model; POSIX ACL encoding (Linux), acl(3) (macOS)
src/paths.rs        host:path detection, trailing-slash rules
src/filter.rs       rsync wildmatch + include/exclude rules
src/diff.rs         quick check (size + mtime, --modify-window, -u, --size-only, ...)
src/fsops/          readdir + fstatat, temp names, clone/copy, utimensat, mknod, page-cache hints,
                    xattr syscalls
src/local/          planner (SRC... DEST → roots) and the parallel per-directory engine;
                    flags.rs: link/compare-dest, backups, partial, sparse, -L/-k, removals
                    extra.rs: -X/-A in the engine (read, compare, write)
                    pipeline.rs: --scanner=jwalk feed, transfer queues A-D
                    merge.rs: several sources into one destination as one tree
src/scan.rs         --scanner=jwalk tree scan (jwalk hook) and the destination index
src/delta/          block/CDC signatures, diff, parallel apply (clone+patch or rebuild);
                    cdc.rs: --delta=cdc (stitched parallel FastCDC, xxh3 + BLAKE3, mmap)
src/proto/          wire frames (postcard or rkyv + raw data, optional zstd), reader/writer
                    threads; async_io.rs: the tokio runtime and the tokio connection layer
src/remote/         client (spawns the server over ssh), server, sender and receiver roles;
                    streams.rs: --transport tcp, quic.rs: --transport quic,
                    relay.rs: streams through ssh (--stream-relay), auto.rs: --transport auto,
                    check.rs: checks on what the sending side announces
src/output.rs       what is shown at rsync's --info levels: item lines, deletions, stats
src/logformat.rs    --out-format, --log-file-format: rsync's %-escapes, parsed and expanded
tests/local_sync.rs end-to-end tests, most of them differential against rsync 3.x
tests/delta_sync.rs --no-whole-file end to end (clone+patch, --inplace, rebuild) vs rsync
tests/remote_sync.rs push/pull through a fake ssh, differential against rsync over the same shim
tests/rsync_flags.rs the options above, locally and over the fake ssh (and one that cuts it)
tests/output_options.rs --out-format, --log-file and --info vs rsync, locally, pushed and pulled
tests/xattrs_acls.rs -X / -A vs rsync, locally and pushed/pulled on each transport
tests/mutation_safety.rs, source_ack_safety.rs, peer_input.rs, deletion.rs,
tests/remote_lifecycle.rs, remote_shutdown.rs, remote_paths.rs
                    the audit's findings (docs/audit): same file, backups, source removal,
                    a hostile sending side, --delete, worker counts, Ctrl-C, locations
bench/              dataset generator + hyperfine runner; net/: the Linux container harness
                    (link profiles, syscalls, flamegraphs, firewall.sh, fixed-cost timing)
docs/               these notes, the plans, the delta comparison and the audit
.github/workflows/  ci.yml: build, test, clippy; release.yml: release builds (generated by dist)
dist-workspace.toml release targets, archive format and installers for dist
```

## Testing

```bash
cargo test
```

The delta engine also has property tests (random insert/delete/overwrite/append/truncate
sequences; every delta must rebuild the new file exactly via both rebuild and clone+patch).
`PROPTEST_CASES=5000 cargo test --release delta` runs a longer sweep.
`BARQSYNC_DELTA=cdc cargo test` runs the whole suite with `--delta=cdc` as the default;
`BARQSYNC_SCANNER=jwalk`, `BARQSYNC_TRANSPORT=quic`, `BARQSYNC_TRANSPORT=auto`,
`BARQSYNC_WIRE=rkyv` and `BARQSYNC_NET_IO=tokio` do the same for the other alternatives.
The fake ssh is a short link, so auto stays on the pipe there; `BARQSYNC_AUTO_RTT_MS=0`
lowers its threshold so that it moves to QUIC. Every barqsync run in the tests gets its own
empty `BARQSYNC_CACHE_DIR` unless the test sets one, so what auto remembers about the fake
host (unreachable, or clean) never carries from one run to the next or into the user's
cache. `BARQSYNC_TIMING=1` prints when each step of a remote transfer happens, on both sides
(see [QUIC's fixed cost](#quics-fixed-cost-m19)).

The integration tests run each scenario through the reference rsync 3.x as well (found at
`/opt/homebrew/bin/rsync`, `/usr/local/bin/rsync` or `/usr/bin/rsync`, or set
`RSYNC=/path`) and assert that both the printed items and the resulting trees are identical:
content, mode, mtime and symlink targets. When no rsync 3.x is installed they are skipped.
macOS's `/usr/bin/rsync` is openrsync and is ignored. On macOS, a few tests also attach
small disk images (`hdiutil`, no root needed) for a full disk or a mount point; they don't
run on Linux.

CI (`.github/workflows/ci.yml`) runs `cargo build`, `cargo test` and
`cargo clippy --all-targets -- -D warnings` on macOS (arm64) and Linux (x86_64) on every push
to `main` and every pull request, with Homebrew's or apt's rsync 3.x as the reference; on
Linux the suite runs a second time with `BARQSYNC_TRANSPORT=quic`.

## Benchmarks

```bash
cargo build --release
python3 bench/gen.py /tmp/fr-bench A C     # A: 200k x 4 KiB, C: 500k x 1 KiB
bench/run.sh /tmp/fr-bench all             # warm cache
bench/run.sh /tmp/fr-bench all --cold      # drops the page cache between runs (sudo)
```

`--no-clone` (hidden flag) forces buffered copies, which shows the engine's speed without APFS
clones, e.g. for copies to another volume.

### Verification harness (original plan §8)

```bash
bench/net/container.sh build              # Linux image: sshd, tc, strace, perf, inferno
bench/net/container.sh net lan wan lossy  # rsync vs barqsync over real ssh, tc-shaped links
bench/net/container.sh fixed far          # each transport's fixed cost, step by step (BARQSYNC_TIMING)
bench/net/container.sh syscalls           # strace -c syscall counts
bench/net/container.sh flamegraph copy    # perf flamegraph (copy | noop | delta)
bench/net/container.sh test               # cargo test + clippy on Linux
bench/flamegraph.sh copy                  # macOS flamegraph via `sample` (cargo install inferno)
bench/remote_setup.sh user@host           # install barqsync on a real second machine
BARQSYNC_TEST_REMOTE=user@host cargo test --test real_remote   # tests over real ssh
```

- **What the container does:** it shapes its own loopback interface with `tc netem`, so ssh,
  the `--streams` TCP connections (and later QUIC) all cross the same emulated link. The
  container runs privileged (needed for `tc`, `perf` and `drop_caches`) and touches nothing
  outside itself.
- **Link profiles:**
  - `lan`: 1 Gbit, ~0.2 ms RTT
  - `wan`: 100 Mbit, ~20 ms RTT
  - `lossy`: 20 Mbit, ~80 ms RTT, ~1% loss
  - `far`: 100 Mbit, ~80 ms RTT, no loss
  - `PROFILE-noudp`: the same with every UDP packet dropped (a firewall that blocks QUIC)
- **Benchmark A** also times `cp -a` and the platform's fastest stock copy (`cp -ac` clones
  on macOS, `--reflink=auto` on Linux).
- **Not covered:** NFS mounts (they need an NFS server).

### Results (M-series Mac, 10 cores, APFS, warm cache, rsync 3.5.1)

Final measurement (M14, default options, 3 runs each, `bench/run.sh DIR all`; the M6 numbers
for comparison):

| Workload | rsync | barqsync | speedup | at M6 |
| --- | ---: | ---: | ---: | ---: |
| A: fresh copy, 200k × 4 KiB files, `-a` | 42.4 s | 20.5 s | 2.1× | 2.4× |
| A with `--inplace` (both tools) | 30.0 s | 11.7 s | 2.6× | 2.6× |
| A, `cp -a` / `cp -ac` (APFS clones) | 76.0 s / 25.9 s | 20.5 s | 3.7× / 1.3× | — |
| C: 500k files, 3 changed, `-a` | 18.2 s (±6.1) | 3.49 s | 5.2× | 3.6× |
| B: 10 GiB, 50 × 1 MiB changed, `--no-whole-file` (clone + patch) | 24.3 s (±6.6) | 5.9 s | 4.1× | ~3× |
| B without clones (full rebuild, e.g. ext4/exFAT) | 24.3 s (±6.6) | 25.7 s | 0.9× | 1.0–1.2× |
| B, `-a` (whole file), same APFS volume | 16.1 s | 0.014 s (clone) | — | — |

rsync's no-op and delta times varied more in this run (C 12.2 s at M6, ±6 s here), so the
speedups are best read as ranges. A fresh copy is 0.2–2 s slower than at M6 run to run (the
SSD's state matters more than any code change in between); barqsync still beats a clone
copy (`cp -ac`) of the same tree.

Benchmark D: remote push through a local fake-ssh shim (`bench/run.sh DIR D`). That measures
the protocol and parallelism over a pipe, not a network; link profiles are in
[Transports and wire formats](#transports-and-wire-formats-measured).

| D: remote push (pipe) | rsync | barqsync | speedup | at M6 |
| --- | ---: | ---: | ---: | ---: |
| fresh copy, 200k × 4 KiB files | 42.4 s | 19.3 s | 2.2× | 2.7× |
| fresh copy, `--streams 4` (TLS over loopback) | 42.4 s | 16.4 s | 2.6× | 2.6× |
| 500k files, 3 changed | 12.9 s | 3.26 s | 4.0× | 4.9× |

Benchmark B resets the destination to the old version before every run. On the same volume a
changed big file is simply cloned (last B row); when the destination can't be cloned, writing a
whole new 10 GiB file is disk-bound for both tools, so the delta engine's win comes from
clone + patch (or `--inplace`), not from hashing.

### A real second machine (over Tailscale)

An Intel MacBook Pro (i9-9980HK, macOS 15.8, rsync 3.5.1) reached from the M-series Mac over
Tailscale, across the internet: about 4 Mbit/s towards this Mac, 0.3–0.6 Mbit/s back, 60–400
ms round trips with some loss (`bench/remote_setup.sh`, `bench/real_host.sh HOST 0.5`, 3 runs).

- **Correctness:** `tests/real_remote.rs` passes against it: round trips over the ssh pipe, TCP
  streams and QUIC; a delta over the real link; and the same item lines and resulting tree as
  rsync between the same two machines.

| Workload (pulls from the MacBook unless noted) | rsync | barqsync | `--strategy adaptive` |
| --- | ---: | ---: | ---: |
| 200 small files, fresh | 5.8 s | 5.2 s | 4.9 s |
| 200 compressible text files, fresh | 7.0 s | 5.5 s | **4.8 s** |
| 8 MiB file, 4 × 64 KiB changed (delta) | 3.7 s | **3.0 s** | 8.3 s |
| 1,000 files, 3 changed | 3.2 s | **2.1 s** | 3.4 s |
| 100 small files, fresh push (0.3–0.6 Mbit/s uplink) | **7.5 s** | 8.0 s (±2.8) | 10.9 s |

On a link this slow the network decides: barqsync is 1.1–1.5× faster than rsync on pulls and
ties on the small push. The adaptive strategy's probe cost more than it saved here (a fixed
256 KiB burst at well under 1 Mbit/s), which led to two fixes: the probe now starts at 16 KiB
and grows only while the link looks fast, and its estimate never subtracts more than half the
measured time for the round trip, so a jittery link reads as slow instead of absurdly fast.
With both, the probe measured this link at 0.1 Mbit/s with one 16 KiB burst. `--transport
tcp` and `quic` fell back to ssh here: the MacBook's firewall blocks incoming connections to
an unsigned binary (`socketfilterfw --unblockapp ~/bin/barqsync` on the remote allows it).
Since then, `--transport tcp` carries its streams through extra ssh connections when that
happens (see "Parallel data streams"); QUIC still needs the firewall to allow it.

### Page-cache hints

Files of 8 MiB or more are read and written with page-cache hints, so a big sync doesn't
push other cached files and the memory of running applications out of RAM:

- **macOS:** `F_NOCACHE` on both files. Pages that are already cached are still used, but no
  new ones are cached.
- **Linux:** `POSIX_FADV_SEQUENTIAL` while reading, and `POSIX_FADV_DONTNEED` on each range
  once it's read. Writes start writeback (`sync_file_range`) every 8 MiB, wait for the window
  before that one, then drop it, so only about two windows per file stay in the cache.

The hints are on by default; `--no-drop-cache` turns them off. Smaller files are never hinted.

Measured on benchmark B. Before every run, the B files were evicted from the page cache
(`msync(MS_INVALIDATE)` on a mapping, no root needed) and a 1 GiB "canary" file was read in,
standing in for an application's hot data. `mincore` then showed what was still cached.
Runs alternated hints on and off, swapping the order every round. The last two rounds left
30 s between runs for the SSD's write cache to drain. The machine was under memory pressure
(16 GB, ~4.4 GB of swap in use, a 7.5 GB Podman VM), as a busy laptop is.

| B, 10 GiB, 50 × 1 MiB changed | hints on | hints off | canary still cached (on / off) |
| --- | ---: | ---: | --- |
| `--no-whole-file` (hash both, clone + patch) | 6.29–6.54 s, median 6.3 | 6.69–7.10 s, median 6.8 | 100% in 4 of 4 / 0% |
| `--no-whole-file --no-clone` (hash both, rebuild) | 14.7–16.3 s, median 15.8 (one 26.7 s outlier) | 17.1–20.7 s, median 18.8 | 0–100%, median 73% / 0% |
| `--no-clone` (whole-file buffered copy) | 8.6–11.1 s, median 10.0 (one 18.8 s outlier) | 8.8–12.0 s, median 10.7 | 6–100%, median 32% / 0% |

- **The canary is the clearest signal.** Without hints it was evicted completely in all 16
  runs, and up to 2.8 GiB of the source stayed cached afterwards. With hints, the B files
  were never cached. The canary still lost some pages in some runs, because other programs
  were also competing for memory: another session's benchmark was running in the Podman VM
  during the last rounds. File-backed pages (`vm_stat`) grew by up to 1.5 GiB per run without
  hints, and by at most 0.25 GiB with them.
- **No slowdown; delta runs got faster.** Hashing reads lose nothing without read-ahead,
  because 10 threads read 4 MiB segments in parallel. The rebuild is probably faster because
  10 GiB of written data never turns into cached pages the system then has to reclaim. The
  two outliers with hints both ran straight after a long burst of big writes; with a 30 s
  pause between runs, the same cases took 9.7 s and 14.7 s.
- **Uncached copies must overlap reads and writes.** The first version kept the 1 MiB
  read-then-write loop and took 14.9 s instead of 10.5 s, because uncached I/O on macOS is
  synchronous (no read-ahead, no write-behind). Big files are now copied through two 8 MiB
  buffers, reading the next one while the current one is written.
- **The remote receiver must write in big pieces too.** Pushing B through the fake-ssh
  pipe (`-W`, whole file) took 15.0 / 14.4 s with hints and 11.3 / 11.1 s without. The
  receiver writes `Data` frames on its single receive loop, and each uncached 1 MiB write
  stalled that loop. On macOS it now collects a big file's contiguous frames into 8 MiB
  writes: 12.3 / 10.8 s with hints, 11.8 / 10.3 s without. A remote delta push of B
  was faster with hints from the start: 6.34 / 6.36 s, against 6.70 / 7.09 s without.
- **Costs:** a big file synced twice in a row is re-read from disk the second time. On
  Linux, `DONTNEED` also drops source pages that were cached before the sync. macOS
  `F_NOCACHE` keeps those.

On Linux this was checked for effect only. The timings of a disk inside a VM say little,
so none were taken. The check ran in the Podman VM (kernel 6.19, aarch64) with a 512 MiB
file whose cache was dropped before each run; `fincore` then measured each file:

| Linux, XFS volume, 512 MiB | cached afterwards, hints on (src / dst) | hints off |
| --- | ---: | ---: |
| `--no-clone` (buffered copy) | 0 / 8 MiB | 474 / 482 MiB |
| `--no-whole-file --no-clone` (hash both, rebuild) | 0 / 16 MiB | 490 / 500 MiB |
| default (`copy_file_range`, reflinked on XFS) | 0 / 0 | 0 / 0 |

On overlayfs (a container's root filesystem), only reads are dropped. `sync_file_range` on an
overlay file doesn't reach the underlying file, so written pages stay dirty and can't be
dropped. The delta hashing reads use 4 MiB segments on many threads, and one thread's
read-ahead could pull in pages another thread had already dropped (96 MiB of 512). The file
is therefore dropped once more as a whole when hashing and rebuilding are done.

Peak memory is higher than rsync's (~130 MiB vs 9 MiB on A locally, ~230 MiB for a remote push) because whole directory listings
and per-thread 1 MiB copy buffers are held at once; bounding that is on the list.

### Over a network (Linux container, real sshd, `tc netem` on loopback, data on tmpfs, 3 runs)

| Profile | Workload | rsync | barqsync (ssh) | barqsync `--streams 4` |
|---|---|---:|---:|---:|
| LAN 1 Gbit | 8k × 4 KiB push | 0.49 s | **0.46 s** | 0.47 s |
| LAN | 256 MiB, 8 × 1 MiB changed | 0.49 s | **0.34 s** | 0.35 s |
| LAN | 40k files, 3 changed | 0.32 s | **0.19 s** | 0.20 s |
| WAN 100 Mbit, 20 ms | small push | 3.59 s | **3.40 s** | 3.88 s |
| WAN | delta | 1.64 s | **1.57 s** | 1.78 s |
| WAN | no-op | 1.82 s | **0.69 s** | 0.84 s |
| Lossy 20 Mbit, 80 ms, 1% | small push | 79.2 s | 79.5 s | **24.4 s** |
| Lossy | delta | 18.2 s | **17.0 s** | 22.2 s (±6.3) |
| Lossy | no-op | 5.80 s | 3.73 s | **3.05 s** |

What this shows:

- **When a transfer is limited by the link or by ssh, the tools tie.** Both push data through
  ssh's single encryption thread (~130 MB/s in this VM).
- **barqsync wins on no-ops** (1.7–2.6×), where rsync's single-threaded scan dominates.
- **`--streams` wins big on lossy links** (3.3×). One TCP flow can't fill a lossy,
  high-latency link; several can. On clean links its extra connection setup costs a little.
- **One big file gains nothing from `--streams`:** all frames of a file use one stream.

Syscalls (`strace -c -f`, Linux, 20k files; the original plan's bar is ≥ 50% fewer than rsync):

| Workload | rsync | barqsync |
|---|---:|---:|
| Copy 20k × 4 KiB | 404,488 | **182,155** (−55%) |
| No-op over 20k files | 42,684 | 44,410 (+4%) |

- **No-op:** both tools need two stats per file (source and destination), and Linux has no
  bulk stat, so the no-op count can't drop much. barqsync's no-op speed comes from
  parallelism.
- **Copy:** getting under the bar took two fixes found in these traces. An extra
  `copy_file_range` call per file only to see EOF is now skipped. A `fchmod` after every
  `chown` is now done only when setuid/setgid bits are involved.

Flamegraphs (`bench/flamegraph.sh`; Linux via perf, macOS via `sample`) show the time going into
`openat`/`copy_file_range` (copy), `pwrite` (delta) and `statx`/`getdents` (no-op). No lock
contention or allocation shows up in the hot frames.

### What the measurements showed

- **Small-file copies scale to ~4 threads, then APFS metadata locking caps them.** Fresh copy of
  A: 1 thread 40 s (≈ rsync), 2 threads 23 s, 4 threads 16 s, 20 threads 17 s.
- **Clones lose below ~512 KiB.** `fclonefileat` is ~20% slower than a plain copy for 16–256 KiB
  files and 2–15× faster from 1 MiB up, so files under 512 KiB are copied, larger ones cloned.
- **The temp-file rename is ~1/3 of a small file's cost.** `--inplace` writes new files at their
  final name (rsync does the same with `--inplace`). The default stays rsync's atomic
  temp+rename. Either way the mtime is set last, so an interrupted copy never passes the
  quick check on the next run.
- **Rebuilding big files must write in order.** Scattered parallel 4 MiB writes into a sparse
  10 GiB file fell to 45–85 s once the SSD's write cache filled. Reading in parallel and writing
  sequentially keeps it at rsync's speed or better.
- **Remote transfers need backpressure in both directions.** Without a cap on queued
  small-file packs, the receiver buffered 834 MiB. Blocking sends inside nested parallel loops
  also overflowed a 2 MiB worker stack on 5000 directories. Now packs are bounded, the
  sender's serve path has no nested parallel loops, and workers get 32 MiB stacks.
- **No-op syncs are bound by APFS metadata reads, not syscalls.**
  - With one thread, a 500k-file no-op takes 12.2 s but only 4.4 s of CPU: it waits on the
    filesystem. Parallelism hides that latency up to ~4–6 threads; beyond that, wall time
    stays near 3.5 s while kernel CPU climbs to 21 s at 20 threads.
  - `getattrlistbulk` (macOS) returns a whole directory's names and attributes in a few
    syscalls. It saves 8–12% wall time at 6–8 threads and 15–30% kernel CPU, but it can't
    lower that floor.
  - On Linux, `statx` asks only for the fields sync decisions need and never forces a
    network filesystem to resync attributes. There is no bulk equivalent there.
  - The default is 2 threads per core because that is fastest for no-op syncs. Fresh
    small-file copies are as fast at one per core and fastest at 4–6 threads, and
    `--threads 6` uses about a third of the kernel CPU.

## Releasing

Releases are built by [dist](https://github.com/axodotdev/cargo-dist) (cargo-dist). Pushing a
version tag starts `.github/workflows/release.yml`. It builds the four targets on native
runners, packages them with checksums, a shell installer and a Homebrew formula
(`barqsync.rb`), and creates the GitHub release. Publishing to crates.io stays a manual
`cargo publish`. After changing `dist-workspace.toml`, run `dist generate` to update the
workflow (`dist plan` shows what a release would contain).

The steps, so the tag and the crate both contain exactly the commit that was checked:

1. Put everything the release changes in one pull request and merge it: the version in
   `Cargo.toml` (and `Cargo.lock`), the CHANGELOG date, and README notes.
2. Fetch and note the exact commit (`git fetch origin && git rev-parse origin/main`), and
   check that CI passed on it.
3. In a clean worktree of that commit, run the checks and `cargo publish --dry-run`:
   `git worktree add /tmp/barqsync-release <commit>`.
4. Tag that commit by its hash and push the tag:
   `git tag -a vX.Y.Z -m "barqsync X.Y.Z" <commit> && git push origin vX.Y.Z`.
5. Run `cargo publish` from the same worktree.

0.4.0's release checks, and what went differently when it was published, are in
[audit/p1-coordination.md](audit/p1-coordination.md).

## License

Licensed under either of [Apache License, Version 2.0](../LICENSE-APACHE) or
[MIT license](../LICENSE-MIT) at your option. Unless you explicitly state otherwise, any
contribution intentionally submitted for inclusion in barqsync, as defined in the Apache-2.0
license, shall be dual licensed as above, without any additional terms or conditions.

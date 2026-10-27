//! `--out-format` and `--info`, run through the reference rsync and barqsync locally and
//! over the fake ssh (a push and a pull): the lines they print must match, except where
//! docs/implementation.md says otherwise (`%b` counts different bytes; rsync's summary
//! counters aren't barqsync's).

mod common;

use common::*;
use std::path::Path;

/// How a run reaches its destination.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Mode {
    Local,
    Push,
    Pull,
}

const MODES: [Mode; 3] = [Mode::Local, Mode::Push, Mode::Pull];

/// `args` then the source and the destination for `mode`, through both tools; `dsts` are
/// the two destinations (rsync's, barqsync's).
fn run_both(cwd: &Path, mode: Mode, args: &[&str], dsts: (&str, &str)) -> (RunOut, RunOut) {
    let mut a = args.to_vec();
    match mode {
        Mode::Local | Mode::Push => a.push("src/"),
        Mode::Pull => a.push("{host}src/"),
    }
    a.push(if mode == Mode::Push { "{host}{dst}" } else { "{dst}" });
    let r = if mode == Mode::Local { both(cwd, &a, dsts.0, dsts.1) } else { both_remote(cwd, &a, dsts.0, dsts.1) };
    r.unwrap()
}

/// The lines both tools must agree on, sorted: summary lines reduced to which ones there
/// are (their numbers differ, and so do the counters rsync and barqsync list), barqsync's
/// own "transferred ..." line left out, and the destination named `<dst>`.
fn lines(out: &RunOut, dst: &str) -> Vec<String> {
    const MARKED: [(&str, &str); 4] =
        [("sent ", "<sent>"), ("total size is ", "<total size>"), ("Number of files:", "<files>"), ("Total bytes sent:", "<bytes sent>")];
    const DROPPED: [&str; 7] = ["Number of ", "Total ", "Literal data", "Matched data", "File list ", "Elapsed", "transferred "];
    let mut v: Vec<String> = out
        .stdout
        .lines()
        .filter(|l| !l.is_empty())
        .filter_map(|l| match MARKED.iter().find(|(p, _)| l.starts_with(p)) {
            Some((_, m)) => Some(m.to_string()),
            None if DROPPED.iter().any(|p| l.starts_with(p)) => None,
            None => Some(l.replace(dst, "<dst>")),
        })
        .collect();
    v.sort();
    v
}

fn same(r: &RunOut, f: &RunOut, dsts: (&str, &str), what: &str) {
    assert_eq!(r.code, f.code, "{what}: exit codes differ\nrsync: {}\nbarqsync: {}", r.stderr, f.stderr);
    assert_eq!(lines(r, dsts.0), lines(f, dsts.1), "{what}\n--- rsync:\n{}--- barqsync:\n{}{}", r.stdout, f.stdout, f.stderr);
}

/// A source with files, a directory tree, a symlink and a name with a space, and `dsts`
/// made from it, then changed so that each run has items of every kind: a new file, a
/// file whose content and one whose mode changed, a newer file, extraneous entries.
fn fixture(root: &Path, dsts: &[String]) {
    let t = Tree::new(root.join("src"));
    t.file("a.txt", "alpha\n")
        .file("with space.txt", "spaces")
        .file("sub/b", "bravo")
        .file("sub/deep/c", "charlie")
        .file("big", vec![9u8; 150_000])
        .dir("emptydir")
        .symlink("lnk", "a.txt")
        .chmod("a.txt", 0o640)
        .settle_mtimes();
    for d in dsts {
        let r = barqsync(root, &["-a", "src/", d]);
        assert_eq!(r.code, 0, "{}", r.stderr);
        Tree::new(root.join(d))
            .file("sub/b", "BRAVO!")
            .mtime("sub/b", BASE_TIME + 5)
            .mtime("sub/deep/c", BASE_TIME + 900_000)
            .file("old/z", "z")
            .file("y", "y");
    }
    t.file("new.txt", "new").mtime("new.txt", BASE_TIME + 3).chmod("a.txt", 0o600);
    // Set apart, whatever second each was changed in: `./` is always an item.
    set_mtime(&root.join("src"), BASE_TIME - 1);
    for d in dsts {
        set_mtime(&root.join(d), BASE_TIME + 1);
    }
}

/// Destination names for `n` runs of each mode, both tools.
fn dst_names(n: usize) -> Vec<String> {
    (0..n).flat_map(|i| MODES.iter().flat_map(move |m| [format!("r{i}{m:?}"), format!("f{i}{m:?}")])).collect()
}

#[test]
fn out_format_escapes_match_rsync() {
    if skip_without_rsync() {
        return;
    }
    let cases: &[&[&str]] = &[
        &["-a", "--delete", "--out-format=%o|%f|%n|%l|%M|%B|%U|%G|%L|%i"],
        &["-a", "--out-format=[%-12n][%12l][%'l][%''l][%'''l][%-5L][%5B][%08l][%-08l][%05U]"],
        // Escapes rsync leaves as they are outside a daemon, an unknown one, %m, a lone %.
        // (Not %%: rsync 3.5 prints it as %, 3.2 as it is.)
        &["-a", "--out-format=%a %h %P %u [%m] %x %n %"],
        // Without -o/-g: %U is 0 and %G "DEFAULT"; deletions use the format (%o).
        &["-rlpt", "--delete", "--out-format=%o %U %G %n%L"],
        // No %o or %i: deletions are "deleting NAME".
        &["-a", "--delete", "--out-format=<%n>"],
        &["-a", "--delete", "-ii", "--out-format=%i|%n|%f"],
    ];
    let tmp = tempfile::tempdir().unwrap();
    let names = dst_names(cases.len());
    fixture(tmp.path(), &names);
    for (i, args) in cases.iter().enumerate() {
        for mode in MODES {
            let dsts = (format!("r{i}{mode:?}"), format!("f{i}{mode:?}"));
            let (r, f) = run_both(tmp.path(), mode, args, (&dsts.0, &dsts.1));
            same(&r, &f, (&dsts.0, &dsts.1), &format!("{mode:?} {args:?}"));
            assert!(!f.stdout.is_empty(), "{mode:?} {args:?}: no output");
        }
    }
}

/// `%f` is the source path for the sender (local copies and pushes, a root directory
/// ending in `/.`, no leading `/`) and the name in the destination for a receiver.
#[test]
fn long_names_match_rsync() {
    if skip_without_rsync() {
        return;
    }
    let tmp = tempfile::tempdir().unwrap();
    Tree::new(tmp.path().join("src")).file("a", "a").file("sub/b", "b").settle_mtimes();
    Tree::new(tmp.path().join("other")).file("o", "o").settle_mtimes();
    let abs = format!("{}/src/sub", tmp.path().display());
    let fmt = "--out-format=%o|%f|%n";
    let cases: &[&[&str]] = &[
        &["-a", fmt, "src", "{dst}"],
        &["-a", fmt, "./src/", "{dst}"],
        &["-a", fmt, "src/a", "other/o", "src/sub", "{dst}/"],
        &["-a", fmt, "src/", "other/", "{dst}"],
        &["-a", fmt, "src/sub/b", "{dst}"],
        &["-a", fmt, &abs, "{dst}"],
        &["-aR", fmt, "src/sub/b", "{dst}"],
    ];
    for (i, args) in cases.iter().enumerate() {
        let dsts = (format!("lr{i}"), format!("lf{i}"));
        let (r, f) = both(tmp.path(), args, &dsts.0, &dsts.1).unwrap();
        same(&r, &f, (&dsts.0, &dsts.1), &format!("{args:?}"));
        // The same over ssh: a push names the source paths too, a pull the received names.
        let (opts, rest) = args.split_at(2);
        let (srcs, dst) = rest.split_at(rest.len() - 1);
        let push: Vec<String> = opts.iter().chain(srcs).map(|a| a.to_string()).chain([format!("{{host}}{}", dst[0])]).collect();
        let pull: Vec<String> =
            opts.iter().map(|a| a.to_string()).chain(srcs.iter().map(|s| format!("{{host}}{s}"))).chain([dst[0].to_string()]).collect();
        for (tag, a) in [("push", push), ("pull", pull)] {
            let a: Vec<&str> = a.iter().map(String::as_str).collect();
            let dsts = (format!("{tag}r{i}"), format!("{tag}f{i}"));
            let (r, f) = both_remote(tmp.path(), &a, &dsts.0, &dsts.1).unwrap();
            same(&r, &f, (&dsts.0, &dsts.1), &format!("{tag} {a:?}"));
        }
    }
}

#[test]
fn info_levels_match_rsync() {
    if skip_without_rsync() {
        return;
    }
    let cases: &[&[&str]] = &[
        &["-av", "--delete", "--info=name0"],
        &["-a", "--delete", "--info=name2"],
        &["-a", "--delete", "--info=del"],
        &["-av", "--delete", "--info=del0"],
        &["-a", "--delete", "--info=NAME2,del", "--out-format=[%n%L]"],
        &["-ai", "--info=name0"],
        &["-a", "--info=name2", "--out-format=%i:%n"],
        &["-a", "-u", "--info=skip"],
        &["-a", "--existing", "--info=skip,name"],
        &["-a", "--ignore-existing", "--info=skip2"],
        &["-a", "--max-size=1K", "--min-size=4", "--info=skip"],
        &["-a", "--info=flist"],
        &["-av", "--info=flist0"],
        // rsync's receiving side also counts the destination directories it scans for
        // --delete-after; that is left out of a pull here.
        &["-a", "--delete-after", "--info=flist2"],
        &["-d", "--info=flist2"],
        // What --existing leaves out is still counted.
        &["-d", "--existing", "--info=flist2"],
        &["-a", "--existing", "--delete-after", "--info=flist2"],
        &["-rv", "-m"],
        &["-a", "--info=stats1"],
        &["-a", "--info=stats2"],
        &["-av", "--stats", "--info=stats0"],
        &["-rv", "--no-l"],
        &["-rv", "--no-l", "--info=nonreg0"],
        &["-av", "--backup-dir=bk", "--info=backup"],
        &["-av", "--delete", "--info=NONE"],
        &["-aq", "--delete", "--stats", "--out-format=%n", "--info=name2,skip,flist2"],
    ];
    let tmp = tempfile::tempdir().unwrap();
    let names = dst_names(cases.len());
    fixture(tmp.path(), &names);
    for (i, args) in cases.iter().enumerate() {
        for mode in MODES {
            if mode == Mode::Pull && args.contains(&"--delete-after") {
                continue;
            }
            let dsts = (format!("r{i}{mode:?}"), format!("f{i}{mode:?}"));
            let (r, f) = run_both(tmp.path(), mode, args, (&dsts.0, &dsts.1));
            same(&r, &f, (&dsts.0, &dsts.1), &format!("{mode:?} {args:?}"));
            if args.contains(&"-aq") {
                assert_eq!(f.stdout, "", "-q prints nothing on stdout");
            }
        }
    }
}

/// `%b` counts the file data sent (rsync also counts its protocol bytes): both show it
/// for the files transferred and 0 for everything else, barqsync the literal data.
#[test]
fn bytes_sent_per_file() {
    if skip_without_rsync() {
        return;
    }
    let tmp = tempfile::tempdir().unwrap();
    let names = dst_names(1);
    fixture(tmp.path(), &names);
    for mode in MODES {
        let dsts = (format!("r0{mode:?}"), format!("f0{mode:?}"));
        let (r, f) = run_both(tmp.path(), mode, &["-a", "--out-format=%n %b %'b"], (&dsts.0, &dsts.1));
        let parse = |out: &RunOut| -> Vec<(String, bool)> {
            let mut v: Vec<(String, bool)> = out
                .stdout
                .lines()
                .map(|l| {
                    let mut w = l.rsplitn(3, ' ');
                    let (_, b, name) = (w.next(), w.next().unwrap(), w.next().unwrap());
                    (name.to_string(), b != "0")
                })
                .collect();
            v.sort();
            v
        };
        assert_eq!(parse(&r), parse(&f), "{mode:?}\n--- rsync:\n{}--- barqsync:\n{}", r.stdout, f.stdout);
        // barqsync's: the file data, here each transferred file's size.
        for (name, size) in [("new.txt", "3"), ("sub/b", "5")] {
            assert!(f.stdout.lines().any(|l| l == format!("{name} {size} {size}")), "{mode:?}: {name}\n{}", f.stdout);
        }
    }
    // Grouped digits, and a whole file sent whole.
    let (_, f) = run_both(tmp.path(), Mode::Local, &["-a", "--out-format=%n %'b"], ("rb", "fb"));
    assert!(f.stdout.lines().any(|l| l == "big 150,000"), "{}", f.stdout);
}

/// `%t` is the time now and `%p` the process id, as in rsync.
#[test]
fn time_and_pid_escapes() {
    let tmp = tempfile::tempdir().unwrap();
    Tree::new(tmp.path().join("src")).file("a", "a").file("b", "b").settle_mtimes();
    let f = barqsync(tmp.path(), &["-a", "--out-format=%t|%p|%M|%n", "src/", "d"]);
    assert_eq!(f.code, 0, "{}", f.stderr);
    let pids: std::collections::BTreeSet<&str> = f.stdout.lines().map(|l| l.split('|').nth(1).unwrap()).collect();
    assert_eq!(pids.len(), 1, "{}", f.stdout);
    assert!(pids.iter().all(|p| p.parse::<u32>().is_ok()), "{}", f.stdout);
    for l in f.stdout.lines() {
        let t: Vec<&str> = l.split('|').collect();
        // `YYYY/MM/DD hh:mm:ss`, and %M with a `-` for the space.
        assert!(t[0].len() == 19 && t[0].as_bytes()[10] == b' ' && t[0].as_bytes()[4] == b'/', "{l}");
        assert!(t[2].len() == 19 && t[2].as_bytes()[10] == b'-', "{l}");
    }
}

#[test]
fn remove_source_files_reports_like_rsync() {
    if skip_without_rsync() {
        return;
    }
    let tmp = tempfile::tempdir().unwrap();
    for (i, mode) in MODES.iter().enumerate() {
        let dsts = (format!("r{i}"), format!("f{i}"));
        for d in [&dsts.0, &dsts.1] {
            Tree::new(tmp.path().join(format!("{d}-src"))).file("one", "1").file("sub/two", "2").settle_mtimes();
        }
        let (src, dst) = match mode {
            Mode::Local => ("{dst}-src/", "{dst}"),
            Mode::Push => ("{dst}-src/", "{host}{dst}"),
            Mode::Pull => ("{host}{dst}-src/", "{dst}"),
        };
        let args = ["-a", "--remove-source-files", "--info=remove", src, dst];
        let (r, f) = if *mode == Mode::Local { both(tmp.path(), &args, &dsts.0, &dsts.1) } else { both_remote(tmp.path(), &args, &dsts.0, &dsts.1) }.unwrap();
        same(&r, &f, (&dsts.0, &dsts.1), &format!("{mode:?}"));
        assert!(f.stdout.contains("sender removed sub/two"), "{mode:?}: {}", f.stdout);
    }
}

#[test]
fn unsupported_requests_are_refused() {
    if skip_without_rsync() {
        return;
    }
    let tmp = tempfile::tempdir().unwrap();
    Tree::new(tmp.path().join("src")).file("a", "a");
    // rsync refuses unknown words too, the same way.
    for bad in ["--info=bogus", "--info=name,NAMES2"] {
        let (r, f) = both(tmp.path(), &["-a", bad, "src/", "{dst}"], "r", "f").unwrap();
        assert_eq!((r.code, f.code), (1, 1), "{bad}");
        let msg = |e: &str| e.lines().find(|l| l.contains("Unknown --info item")).map(|l| l.trim_start_matches("barqsync: ").to_string());
        assert_eq!(msg(&r.stderr), msg(&f.stderr), "{bad}\nrsync: {}\nbarqsync: {}", r.stderr, f.stderr);
    }
    // What barqsync can't show is refused rather than ignored.
    for bad in ["--info=copy", "--info=mount2", "--out-format=%n %C", "--out-format=%-4c"] {
        let f = barqsync(tmp.path(), &["-a", bad, "src/", "f"]);
        assert_eq!(f.code, 1, "{bad}: {}", f.stderr);
        assert!(f.stderr.contains("not supported"), "{bad}: {}", f.stderr);
        assert!(!tmp.path().join("f").exists(), "{bad}: nothing is transferred");
    }
    // --info=help prints rsync's list.
    let r = rsync(tmp.path(), &["--info=help"]).unwrap();
    let f = barqsync(tmp.path(), &["--info=help"]);
    assert_eq!((r.code, f.code), (0, 0));
    assert_eq!(r.stdout, f.stdout);
}

/// `--progress` also names the files, as rsync's does (its per-file progress lines aside).
#[test]
fn progress_names_items_like_rsync() {
    if skip_without_rsync() {
        return;
    }
    let tmp = tempfile::tempdir().unwrap();
    let names = dst_names(1);
    fixture(tmp.path(), &names);
    let (r, f) = run_both(tmp.path(), Mode::Local, &["-a", "--progress"], ("r0Local", "f0Local"));
    let named = |out: &RunOut| -> Vec<String> {
        let mut v: Vec<String> =
            out.stdout.lines().filter(|l| !l.is_empty() && !l.contains("to-chk=")).map(str::to_string).collect();
        v.sort();
        v
    };
    assert_eq!((r.code, f.code), (0, 0));
    assert_eq!(named(&r), named(&f), "--- rsync:\n{}--- barqsync:\n{}", r.stdout, f.stdout);
    assert!(named(&f).contains(&"new.txt".to_string()), "{}", f.stdout);
}

/// A `--log-file`'s lines, sorted, each checked for and stripped of rsync's stamp (`YYYY/MM/DD
/// HH:MM:SS [PID] `), with the byte counts that differ reduced to which lines they are
/// and barqsync's own "transferred ..." line left out.
fn log_lines(path: &Path, dst: &str) -> Vec<String> {
    let text = std::fs::read_to_string(path).unwrap_or_else(|e| panic!("{}: {e}", path.display()));
    let mut v: Vec<String> = text
        .lines()
        .map(|l| {
            let b = l.as_bytes();
            let digits = |r: std::ops::Range<usize>| b.get(r).is_some_and(|d| d.iter().all(u8::is_ascii_digit));
            assert!(
                l.len() > 22 && digits(0..4) && b[4] == b'/' && digits(5..7) && b[7] == b'/' && digits(8..10) && b[10] == b' ' && digits(11..13) && b[13] == b':' && b[19] == b' ' && b[20] == b'[',
                "{}: no time stamp: {l:?}",
                path.display()
            );
            let end = l[21..].find("] ").unwrap_or_else(|| panic!("no pid: {l:?}")) + 21;
            assert!(digits(21..end), "{l:?}");
            l[end + 2..].to_string()
        })
        .filter(|l| !l.starts_with("transferred "))
        .map(|l| match ["sent ", "total size is "].iter().find(|p| l.starts_with(*p)) {
            Some(p) => format!("<{}>", p.trim()),
            None => l.replace(dst, "<dst>"),
        })
        .collect();
    v.sort();
    v
}

#[test]
fn log_file_matches_rsync() {
    if skip_without_rsync() {
        return;
    }
    let cases: &[&[&str]] = &[
        &["-a", "--delete", "--log-file={dst}.log"],
        &["-a", "--delete", "--log-file={dst}.log", "--log-file-format=%o %n%L"],
        // No items, but deletions as "deleting NAME".
        &["-a", "--delete", "--log-file={dst}.log", "--log-file-format="],
        // A dry run logs no transfers or deletions, the rest it would do.
        &["-an", "--delete", "--log-file={dst}.log"],
        // -q keeps stdout empty, not the log.
        &["-aq", "--delete", "--log-file={dst}.log"],
        &["-a", "-ii", "--log-file={dst}.log"],
        &["-av", "--log-file={dst}.log", "--log-file-format=%i %f"],
        // %l, %B and %M of what changed (not %b: its bytes are barqsync's, not rsync's).
        &["-a", "--delete", "--log-file={dst}.log", "--log-file-format=%o %n %l %B %M"],
    ];
    let tmp = tempfile::tempdir().unwrap();
    let names = dst_names(cases.len());
    fixture(tmp.path(), &names);
    for (i, args) in cases.iter().enumerate() {
        for mode in MODES {
            let dsts = (format!("r{i}{mode:?}"), format!("f{i}{mode:?}"));
            let (r, f) = run_both(tmp.path(), mode, args, (&dsts.0, &dsts.1));
            let what = format!("{mode:?} {args:?}");
            same(&r, &f, (&dsts.0, &dsts.1), &what);
            let (rl, fl) = (log_lines(&tmp.path().join(format!("{}.log", dsts.0)), &dsts.0), log_lines(&tmp.path().join(format!("{}.log", dsts.1)), &dsts.1));
            assert_eq!(rl, fl, "{what}");
            assert!(fl.iter().any(|l| l.ends_with("file list")), "{what}: {fl:?}");
        }
    }
    // A log that can't be opened is reported, and the run goes on (rsync: exit 0).
    let (r, f) = run_both(tmp.path(), Mode::Local, &["-a", "--log-file=nowhere/{dst}.log"], ("r0Local", "f0Local"));
    assert_eq!((r.code, f.code), (0, 0), "{}", f.stderr);
    assert!(f.stderr.contains("failed to open log-file nowhere/f0Local.log"), "{}", f.stderr);
    assert!(f.stdout.contains("Ignoring \"log file\" setting."), "{}", f.stdout);
}

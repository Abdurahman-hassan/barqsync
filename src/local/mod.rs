//! Local (disk-to-disk) sync: argument planning and the run loop.

pub mod exec;
pub mod extra;
pub mod flags;
pub(crate) mod merge;
pub mod pipeline;
pub(crate) mod prune;

use crate::cli::Options;
use crate::entry::Kind;
use crate::exit;
use crate::filter::Filter;
use crate::fsops;
use crate::output::{Output, Side, Stats, path_msg};
use crate::paths::{split_source, wants_contents};
use crate::select::{self, Sel};
use exec::Engine;
use std::ffi::OsString;
use std::fs;
use std::os::unix::ffi::OsStrExt;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Instant;

/// One unit of transfer derived from a SRC argument.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Root {
    /// `src/`: the directory's contents are synced into `dst` (shown as `./`).
    Contents { src: PathBuf, dst: PathBuf },
    /// `src` (no trailing slash) or single files: named entries of `src_dir` are
    /// synced into `dst_dir`, each possibly under a different destination name.
    Items { src_dir: PathBuf, dst_dir: PathBuf, items: Vec<(OsString, OsString)> },
    /// `-R`/`--files-from`: the selected paths below `base`, under their relative names.
    /// `with_root`: `base` itself is part of the transfer (`./`).
    Select { base: PathBuf, dst: PathBuf, sel: &'static Sel, with_root: bool },
}

#[derive(Debug)]
pub struct Plan {
    pub roots: Vec<Root>,
    /// Whether the directory the roots write into exists (false only in dry runs).
    pub dest_exists: bool,
    /// The destination directory was created by this run.
    pub dest_created: bool,
    /// The directory roots write into (for a single file copied to a new name, its parent).
    pub dest_root: PathBuf,
}

/// Turn SRC... DEST into transfer roots following rsync's rules, creating DEST
/// (one level, like rsync) when it must be a directory and does not exist yet.
pub fn plan(opts: &Options, sources: &[OsString], dest: &OsString, out: &Output) -> Result<Plan, i32> {
    let dest_path = PathBuf::from(dest);
    // Without -r or -d, rsync transfers no directory, not even what a `src/` holds.
    let sources: Vec<OsString> = if !opts.recursive && !opts.dirs && !opts.relative {
        let skipped = |s: &&OsString| wants_contents(s) && fs::metadata(s).is_ok_and(|m| m.is_dir());
        sources.iter().filter(|s| skipped(s)).for_each(|_| out.note("skipping directory ."));
        sources.iter().filter(|s| !skipped(s)).cloned().collect()
    } else {
        sources.to_vec()
    };
    if sources.is_empty() {
        return Ok(Plan { roots: Vec::new(), dest_exists: false, dest_created: false, dest_root: dest_path });
    }
    let sources = &sources[..];
    let dest_meta = fs::metadata(&dest_path).ok();
    let dest_is_dir = dest_meta.as_ref().is_some_and(|m| m.is_dir());

    // Single non-directory source to a non-directory destination: copy under DEST's name.
    if sources.len() == 1 && !dest_is_dir && !wants_contents(dest) && !opts.relative {
        let src = Path::new(&sources[0]);
        let src_meta = fs::symlink_metadata(src).ok();
        let src_is_dir = src_meta.as_ref().is_some_and(|m| m.is_dir())
            || (wants_contents(&sources[0]) && fs::metadata(src).is_ok_and(|m| m.is_dir()));
        // Without -r a lone directory (no trailing slash) is one item, so it is copied
        // under DEST's name like a file.
        let lone_dir = src_is_dir && !opts.recursive && !wants_contents(&sources[0]) && dest_meta.is_none();
        if (!src_is_dir || lone_dir) && let (parent, Some(name)) = split_source(src) {
            let (dst_dir, dst_name) = split_source(&dest_path);
            let Some(dst_name) = dst_name else {
                out.error(path_msg("invalid destination \"", &dest_path, "\""));
                return Err(exit::SYNTAX);
            };
            if opts.mkpath && dest.as_bytes().contains(&b'/') {
                make_dest_path(&dst_dir, opts, out)?;
            }
            return Ok(Plan {
                roots: vec![Root::Items { src_dir: parent, dst_dir: dst_dir.clone(), items: vec![(name, dst_name)] }],
                dest_exists: true,
                dest_created: false,
                dest_root: dst_dir,
            });
        }
    }

    let mut dest_exists = dest_is_dir;
    let mut dest_created = false;
    if !dest_is_dir && dest_meta.is_some() {
        out.error(path_msg("destination \"", &dest_path, "\" exists and is not a directory"));
        return Err(exit::FILE_IO);
    }
    // --mkpath: like rsync, the destination is then compared as an existing directory.
    if !dest_is_dir && opts.mkpath {
        dest_exists = make_dest_path(&dest_path, opts, out)? > 0 && !opts.dry_run;
    }
    if !dest_exists {
        let raw = dest.as_bytes();
        let shown = if raw.len() > 1 { &raw[..raw.iter().rposition(|&c| c != b'/').map_or(0, |p| p + 1)] } else { raw };
        let created = [b"created directory ", shown].concat();
        if opts.dry_run {
            out.created(&created);
        } else {
            match fs::create_dir(&dest_path) {
                Ok(()) => {
                    out.created(&created);
                    dest_exists = true;
                    dest_created = true;
                }
                Err(e) => {
                    out.error(path_msg("mkdir \"", &dest_path, format!("\" failed: {e}")));
                    return Err(exit::FILE_IO);
                }
            }
        }
    }

    let mut roots = Vec::new();
    if opts.relative {
        let sel_roots = match &opts.files_from {
            Some(list) => select::from_list(&sources[0], list),
            None => select::from_relative_args(sources),
        };
        for r in sel_roots {
            let sel = select::leak(r.sel);
            roots.push(Root::Select { base: r.base, dst: dest_path.clone(), sel, with_root: r.with_root });
        }
        return Ok(Plan { roots, dest_exists, dest_created, dest_root: dest_path });
    }
    for s in sources {
        let sp = PathBuf::from(s);
        match split_source(&sp) {
            (parent, Some(name)) if !wants_contents(s) => {
                roots.push(Root::Items { src_dir: parent, dst_dir: dest_path.clone(), items: vec![(name.clone(), name)] })
            }
            _ => roots.push(Root::Contents { src: sp, dst: dest_path.clone() }),
        }
    }
    Ok(Plan { roots, dest_exists, dest_created, dest_root: dest_path })
}

/// `--mkpath`: create the missing directories leading to and including `dir`, telling
/// how many with `-v`/`-i` as rsync does (a dry run counts them and creates none).
pub(crate) fn make_dest_path(dir: &Path, opts: &Options, out: &Output) -> Result<usize, i32> {
    match fsops::make_path(dir, opts.dry_run) {
        Ok(0) => Ok(0),
        Ok(n) => {
            let raw = dir.as_os_str().as_bytes();
            let shown = &raw[..raw.iter().rposition(|&c| c != b'/').map_or(raw.len(), |p| p + 1)];
            let what = if n == 1 { "directory" } else { "directories" };
            out.created([format!("created {n} {what} for ").as_bytes(), shown].concat());
            Ok(n)
        }
        Err(e) => {
            out.error(path_msg("mkdir \"", dir, format!("\" failed: {e}")));
            Err(exit::FILE_IO)
        }
    }
}

pub fn run(opts: &Options, sources: &[OsString], dest: &OsString) -> i32 {
    // Process-lifetime output, shared with the progress thread.
    let out: &'static Output = Box::leak(Box::new(Output::new(opts, Side::Local)));
    crate::bwlimit::limit_copies(opts.bwlimit);
    let filter = match Filter::new(&opts.filter_rules, opts.delete_excluded, opts.from0) {
        Ok(f) => f,
        Err(e) => {
            out.error(format!("{e:#}"));
            return exit::SYNTAX;
        }
    };
    let started = Instant::now();
    // As rsync's sending side does, before the receiving side creates the destination.
    out.flist_start(opts, false);
    let plan = match plan(opts, sources, dest, out) {
        Ok(p) => p,
        Err(code) => return code,
    };

    // A `src/` whose target is not a directory is an error (rsync: "Not a directory").
    for root in &plan.roots {
        if let Root::Contents { src, .. } = root
            && fsops::stat(src).is_ok_and(|m| m.kind != Kind::Dir)
        {
            out.error(path_msg("change_dir \"", src, "\" failed: Not a directory"));
            return exit::PARTIAL;
        }
    }

    let mut engine = Engine::new(opts, filter, out);
    engine.set_dest(&plan.dest_root);
    if let Err(code) = engine.check_temp_dir() {
        return code;
    }
    if opts.usermap.is_some() || opts.groupmap.is_some() {
        match crate::ids::IdMap::for_receiver(opts, &mut |m| out.warning(m)) {
            Ok(ids) => engine.local_ids = Some(std::sync::Mutex::new(ids)),
            Err(e) => {
                out.error(e);
                return exit::SYNTAX;
            }
        }
    }
    // A directory task waiting in `par_iter` runs other directories' tasks on its stack,
    // so nesting gets deep with many directories (5000 overflowed the 2 MiB default).
    let pool = match rayon::ThreadPoolBuilder::new()
        .num_threads(opts.threads)
        .thread_name(|i| format!("barqsync-{i}"))
        .stack_size(crate::remote::WORKER_STACK)
        .build()
    {
        Ok(p) => p,
        Err(e) => {
            out.error(format!("failed to start worker threads: {e}"));
            return exit::IPC;
        }
    };
    // --scanner=jwalk needs whole trees to walk: not with -R/--files-from or without -r.
    let queues = (opts.scanner == crate::scan::Scanner::Jwalk && opts.recursive && !opts.relative).then(|| {
        let (p, rx) = pipeline::Pipeline::new(opts.threads);
        engine.dst_index = Some(Arc::clone(&p.index));
        engine.pipeline = Some(p);
        rx
    });
    let reporter = opts.progress.then(|| crate::progress::Reporter::start(out, opts.human));
    let engine = &engine;
    std::thread::scope(|ts| {
        if let (Some(p), Some(rx)) = (&engine.pipeline, queues) {
            p.run_workers(ts, engine, rx);
        }
        pool.install(|| {
            // Several roots land in one destination: they are synced as one merged tree.
            match &engine.pipeline {
                Some(p) => pipeline::run_roots(engine, p, &plan.roots, plan.dest_exists, plan.dest_created),
                None => rayon::scope(|s| engine.run_roots(s, &plan.roots, plan.dest_exists, plan.dest_created)),
            }
        });
        if let Some(p) = &engine.pipeline {
            p.close();
        }
    });
    if !fsops::interrupted() {
        pool.install(|| {
            engine.finish_delayed();
            engine.link_followers();
            engine.finish_deletions();
            engine.apply_fixups();
        });
    }
    if let Some(r) = reporter {
        r.finish(out);
    }
    out.flush();
    if fsops::interrupted() {
        fsops::cleanup_temps();
        return exit::SIGNAL;
    }

    out.flist_end(opts, false, engine.stats.entries());
    // No connection: what is "sent" is the file data written.
    let io = crate::output::Io { sent: Stats::get(&engine.stats.literal_bytes), received: 0, dry_run: opts.dry_run };
    engine.stats.print(out, started.elapsed(), opts.human, opts.info.stats, io);
    out.flush();

    if fsops::interrupted() { exit::SIGNAL } else { engine.stats.exit_code() }
}

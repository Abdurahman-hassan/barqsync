//! Local (disk-to-disk) sync: argument planning and the run loop.

pub mod exec;
pub mod extra;
pub mod flags;
pub mod pipeline;

use crate::cli::Options;
use crate::entry::Kind;
use crate::exit;
use crate::filter::Filter;
use crate::fsops;
use crate::output::{Output, Stats, path_msg};
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
    if !dest_is_dir {
        if dest_meta.is_some() {
            out.error(path_msg("destination \"", &dest_path, "\" exists and is not a directory"));
            return Err(exit::FILE_IO);
        }
        let raw = dest.as_bytes();
        let shown = if raw.len() > 1 { &raw[..raw.iter().rposition(|&c| c != b'/').map_or(0, |p| p + 1)] } else { raw };
        let created = [b"created directory ", shown].concat();
        if opts.dry_run {
            out.info(&created);
        } else {
            match fs::create_dir(&dest_path) {
                Ok(()) => {
                    out.info(&created);
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

pub fn run(opts: &Options, sources: &[OsString], dest: &OsString) -> i32 {
    // Process-lifetime output, shared with the progress thread.
    let out: &'static Output = Box::leak(Box::new(Output::new(opts)));
    let filter = match Filter::new(&opts.filter_rules) {
        Ok(f) => f,
        Err(e) => {
            out.error(format!("{e:#}"));
            return exit::SYNTAX;
        }
    };
    let started = Instant::now();
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
            for (i, root) in plan.roots.iter().enumerate() {
                // Only the first root may assume a freshly created destination is empty.
                let fresh = plan.dest_created && i == 0;
                match &engine.pipeline {
                    Some(p) => pipeline::run_root(engine, p, root, plan.dest_exists, fresh),
                    None => rayon::scope(|s| engine.run_root(s, root, plan.dest_exists, fresh)),
                }
            }
        });
        if let Some(p) = &engine.pipeline {
            p.close();
        }
    });
    if !fsops::interrupted() {
        pool.install(|| {
            engine.link_followers();
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

    if opts.stats || opts.verbose > 0 {
        engine.stats.print(out, started.elapsed(), opts.human, opts.stats);
    }
    out.flush();

    if fsops::interrupted() {
        exit::SIGNAL
    } else if Stats::get(&engine.stats.errors) > 0 {
        exit::PARTIAL
    } else {
        exit::OK
    }
}

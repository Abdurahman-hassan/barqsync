//! Local (disk-to-disk) sync: argument planning and the run loop.

pub mod exec;

use crate::cli::Options;
use crate::entry::Kind;
use crate::exit;
use crate::filter::Filter;
use crate::fsops;
use crate::output::{Output, Stats};
use crate::paths::{split_source, wants_contents};
use exec::Engine;
use std::ffi::OsString;
use std::fs;
use std::path::{Path, PathBuf};
use std::time::Instant;

/// One unit of transfer derived from a SRC argument.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Root {
    /// `src/`: the directory's contents are synced into `dst` (shown as `./`).
    Contents { src: PathBuf, dst: PathBuf },
    /// `src` (no trailing slash) or single files: named entries of `src_dir` are
    /// synced into `dst_dir`, each possibly under a different destination name.
    Items { src_dir: PathBuf, dst_dir: PathBuf, items: Vec<(OsString, OsString)> },
}

#[derive(Debug)]
pub struct Plan {
    pub roots: Vec<Root>,
    /// Whether the directory the roots write into exists (false only in dry runs).
    pub dest_exists: bool,
    /// The destination directory was created by this run.
    pub dest_created: bool,
}

/// Turn SRC... DEST into transfer roots following rsync's rules, creating DEST
/// (one level, like rsync) when it must be a directory and does not exist yet.
pub fn plan(opts: &Options, sources: &[OsString], dest: &OsString, out: &Output) -> Result<Plan, i32> {
    let dest_path = PathBuf::from(dest);
    let dest_meta = fs::metadata(&dest_path).ok();
    let dest_is_dir = dest_meta.as_ref().is_some_and(|m| m.is_dir());

    // Single non-directory source to a non-directory destination: copy under DEST's name.
    if sources.len() == 1 && !dest_is_dir && !wants_contents(dest) {
        let src = Path::new(&sources[0]);
        let src_meta = fs::symlink_metadata(src).ok();
        let src_is_dir = src_meta.as_ref().is_some_and(|m| m.is_dir())
            || (wants_contents(&sources[0]) && fs::metadata(src).is_ok_and(|m| m.is_dir()));
        if !src_is_dir && let (parent, Some(name)) = split_source(src) {
            let (dst_dir, dst_name) = split_source(&dest_path);
            let Some(dst_name) = dst_name else {
                out.error(format!("invalid destination \"{}\"", dest_path.display()));
                return Err(exit::SYNTAX);
            };
            return Ok(Plan {
                roots: vec![Root::Items { src_dir: parent, dst_dir, items: vec![(name, dst_name)] }],
                dest_exists: true,
                dest_created: false,
            });
        }
    }

    let mut dest_exists = dest_is_dir;
    let mut dest_created = false;
    if !dest_is_dir {
        if dest_meta.is_some() {
            out.error(format!("destination \"{}\" exists and is not a directory", dest_path.display()));
            return Err(exit::FILE_IO);
        }
        let shown = dest.to_string_lossy();
        let shown = if shown.len() > 1 { shown.trim_end_matches('/') } else { &shown };
        if opts.dry_run {
            out.info(format!("created directory {shown}"));
        } else {
            match fs::create_dir(&dest_path) {
                Ok(()) => {
                    out.info(format!("created directory {shown}"));
                    dest_exists = true;
                    dest_created = true;
                }
                Err(e) => {
                    out.error(format!("mkdir \"{}\" failed: {e}", dest_path.display()));
                    return Err(exit::FILE_IO);
                }
            }
        }
    }

    let mut roots = Vec::new();
    for s in sources {
        let sp = PathBuf::from(s);
        match split_source(&sp) {
            (parent, Some(name)) if !wants_contents(s) => {
                roots.push(Root::Items { src_dir: parent, dst_dir: dest_path.clone(), items: vec![(name.clone(), name)] })
            }
            _ => roots.push(Root::Contents { src: sp, dst: dest_path.clone() }),
        }
    }
    Ok(Plan { roots, dest_exists, dest_created })
}

pub fn run(opts: &Options, sources: &[OsString], dest: &OsString) -> i32 {
    let out = Output::new(opts.verbose, opts.out_format.clone());
    let filter = match Filter::new(&opts.filter_rules) {
        Ok(f) => f,
        Err(e) => {
            out.error(format!("{e:#}"));
            return exit::SYNTAX;
        }
    };
    let started = Instant::now();
    let plan = match plan(opts, sources, dest, &out) {
        Ok(p) => p,
        Err(code) => return code,
    };

    // A `src/` whose target is not a directory is an error (rsync: "Not a directory").
    for root in &plan.roots {
        if let Root::Contents { src, .. } = root
            && fsops::stat(src).is_ok_and(|m| m.kind != Kind::Dir)
        {
            out.error(format!("change_dir \"{}\" failed: Not a directory", src.display()));
            return exit::PARTIAL;
        }
    }

    let engine = Engine::new(opts, filter, &out);
    let pool = match rayon::ThreadPoolBuilder::new()
        .num_threads(opts.threads)
        .thread_name(|i| format!("fastrsync-{i}"))
        .build()
    {
        Ok(p) => p,
        Err(e) => {
            out.error(format!("failed to start worker threads: {e}"));
            return exit::IPC;
        }
    };
    pool.install(|| {
        for (i, root) in plan.roots.iter().enumerate() {
            // Only the first root may assume a freshly created destination is empty.
            let fresh = plan.dest_created && i == 0;
            rayon::scope(|s| engine.run_root(s, root, plan.dest_exists, fresh));
        }
        if !fsops::interrupted() {
            engine.apply_fixups();
        }
    });
    out.flush();
    if fsops::interrupted() {
        fsops::cleanup_temps();
        return exit::SIGNAL;
    }

    if opts.stats || opts.verbose > 0 {
        engine.stats.print(&out, started.elapsed(), opts.human, opts.stats);
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

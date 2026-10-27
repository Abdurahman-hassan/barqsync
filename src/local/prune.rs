//! `-m`/`--prune-empty-dirs`: a directory with nothing but directories below it, once the
//! filter rules apply, is left out of the transfer, as rsync leaves it out of its file
//! list. With `--delete`, the destination's copy of it is then extraneous.

use super::flags::{adjust_listing, missing_selected};
use crate::cli::Options;
use crate::entry::{DirEnt, Kind};
use crate::filter::{DirRules, Filter, Side};
use crate::fsops;
use crate::paths::join_rel;
use crate::select::{self, Descend, Sel};
use std::collections::HashMap;
use std::os::unix::ffi::OsStrExt;
use std::path::{Path, PathBuf};
use std::sync::Mutex;

/// Whether entry `e` of source directory `dir` (at `rel` in the transfer, with its own
/// filter state `rules`, `-R` selection `sel` and `-x` device `root_dev`) is sent: no rule
/// excludes it, and with `-m` a directory has something to send below it. `err` reports
/// what a pruned directory was asked for and lacks (see [`Prune::keeps`]).
#[allow(clippy::too_many_arguments)]
pub(crate) fn sends(
    opts: &Options,
    filter: &Filter,
    prune: &Prune,
    dir: &Path,
    rel: &[u8],
    rules: &DirRules,
    sel: Option<&'static Sel>,
    root_dev: Option<u64>,
    e: &DirEnt,
    err: &dyn Fn(Vec<u8>),
) -> bool {
    if filter.is_empty() && !opts.prune_empty_dirs {
        return true;
    }
    let is_dir = e.meta.kind == Kind::Dir;
    let erel = join_rel(rel, &e.name);
    if filter.excluded(rules, &erel, is_dir, Side::Sender) {
        return false;
    }
    if !is_dir || !opts.prune_empty_dirs {
        return true;
    }
    // A directory a receiver-side rule protects stays, as rsync keeps the ones "through" it.
    if filter.excluded(rules, &erel, true, Side::Receiver) {
        return true;
    }
    let sub = match select::descend(sel, e.name.as_bytes(), opts.recursive) {
        // Not transferred at all (the engine says so), or with -d an empty directory.
        Descend::No => return !opts.dirs,
        Descend::All => None,
        Descend::Some(s) => Some(s),
    };
    if opts.one_file_system && root_dev.is_some_and(|d| d != e.meta.dev) {
        return false;
    }
    let child = dir.join(&e.name);
    match filter.enter(rules, &child, &erel) {
        Ok(crules) => prune.keeps(opts, filter, &child, &erel, &crules, sub, root_dev.or(Some(e.meta.dev)), err),
        // Reported when the directory itself is read.
        Err(_) => true,
    }
}

/// Which source directories have something to transfer below them.
#[derive(Default)]
pub(crate) struct Prune(Mutex<HashMap<PathBuf, bool>>);

impl Prune {
    /// Whether source directory `dir` (at `rel` in the transfer, with its own filter state
    /// `rules`) has an entry other than a directory somewhere below it that the rules let
    /// through: one rsync would list. `sel`: the `-R`/`--files-from` selection inside it;
    /// `root_dev`: `-x`'s device. A directory that a receiver-side rule protects counts
    /// too, as rsync keeps the directories leading to it. A directory found to have
    /// nothing isn't read again, so `err` reports what the selection asked for in it and
    /// isn't there, as rsync's sender does whether or not the directory is pruned.
    #[allow(clippy::too_many_arguments)]
    pub(crate) fn keeps(
        &self,
        opts: &Options,
        filter: &Filter,
        dir: &Path,
        rel: &[u8],
        rules: &DirRules,
        sel: Option<&'static Sel>,
        root_dev: Option<u64>,
        err: &dyn Fn(Vec<u8>),
    ) -> bool {
        if let Some(&keep) = self.0.lock().unwrap().get(dir) {
            return keep;
        }
        let keep = self.look(opts, filter, dir, rel, rules, sel, root_dev, err);
        self.0.lock().unwrap().insert(dir.to_path_buf(), keep);
        keep
    }

    #[allow(clippy::too_many_arguments)]
    fn look(
        &self,
        opts: &Options,
        filter: &Filter,
        dir: &Path,
        rel: &[u8],
        rules: &DirRules,
        sel: Option<&'static Sel>,
        root_dev: Option<u64>,
        err: &dyn Fn(Vec<u8>),
    ) -> bool {
        let (ents, missing) = match sel {
            Some(s) if !s.contents => select::read_selected(dir, s),
            _ => match fsops::read_dir_meta(dir) {
                Ok(v) => (v, Vec::new()),
                // Left to the transfer, which reports it.
                Err(_) => return true,
            },
        };
        // Errors are reported when the transfer reads the directory.
        let mut ents = adjust_listing(opts, dir, rel, ents, &mut |_| {}, &mut |_| {});
        // What the selection names and isn't there counts as the transfer takes it: an
        // error (reported below if this directory has nothing), nothing (with
        // --ignore-missing-args), or an entry asking for a deletion, which is something.
        let mut errs = Vec::new();
        if let Some(s) = sel {
            missing_selected(opts, s, missing, &mut ents, &mut |m| errs.push(m), &mut |_| {});
        }
        let passes = |e: &DirEnt| !filter.excluded(rules, &join_rel(rel, &e.name), e.meta.kind == Kind::Dir, Side::Sender);
        // Anything but a directory settles it without looking further down.
        if ents.iter().any(|e| e.meta.kind != Kind::Dir && passes(e)) {
            return true;
        }
        for e in ents.iter().filter(|e| e.meta.kind == Kind::Dir && passes(e)) {
            let erel = join_rel(rel, &e.name);
            if filter.excluded(rules, &erel, true, Side::Receiver) {
                return true;
            }
            let sub = match select::descend(sel, e.name.as_bytes(), opts.recursive) {
                Descend::No => continue,
                Descend::All => None,
                Descend::Some(s) => Some(s),
            };
            if opts.one_file_system && root_dev.is_some_and(|d| d != e.meta.dev) {
                continue;
            }
            let child = dir.join(&e.name);
            let Ok(crules) = filter.enter(rules, &child, &erel) else { return true };
            if self.keeps(opts, filter, &child, &erel, &crules, sub, root_dev.or(Some(e.meta.dev)), err) {
                return true;
            }
        }
        errs.into_iter().for_each(err);
        false
    }
}

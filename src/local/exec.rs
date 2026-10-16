//! The local sync engine: one rayon task per directory, files copied in parallel.
//!
//! Each directory task reads its source directory, compares it with the matching
//! destination directory, deletes extraneous entries (`--delete`), creates
//! subdirectories and spawns tasks for them, then copies its own files in parallel.
//! Directory attributes are applied after everything else, deepest first, so a
//! read-only source directory stays writable while it is being filled.

use super::Root;
use crate::cli::Options;
use crate::delta::{self, apply::apply};
use crate::diff::{AttrDiff, DiffRules, FileAction, file_action};
use crate::entry::{DirEnt, Kind, Meta};
use crate::filter::Filter;
use crate::fsops::{self, Copied};
use crate::output::{Output, Stats, path_msg};
use crate::paths::join_rel;
use rayon::Scope;
use rayon::prelude::*;
use std::collections::{HashMap, HashSet};
use std::ffi::{OsStr, OsString};
use std::fs;
use std::io;
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::DirBuilderExt;
use std::path::{Path, PathBuf};
use std::sync::Mutex;

pub struct Engine<'a> {
    pub opts: &'a Options,
    pub rules: DiffRules,
    pub filter: Filter,
    pub out: &'a Output,
    pub stats: Stats,
    fixups: Mutex<Vec<DirFixup>>,
    umask: u32,
}

struct DirJob {
    src: PathBuf,
    dst: PathBuf,
    rel: Vec<u8>,
    depth: usize,
    /// The destination directory was just created (or would be, in a dry run): it is empty.
    dst_is_new: bool,
    root_dev: Option<u64>,
    src_meta: Meta,
    /// Destination directory metadata before we touched it (`None` when new).
    pre: Option<Meta>,
}

struct Ctx<'p> {
    src: &'p Path,
    dst: &'p Path,
    /// Open handle on `dst` for `fstatat` (cheaper than resolving full paths).
    dst_handle: Option<&'p fsops::DirHandle>,
    rel: &'p [u8],
    depth: usize,
    dst_is_new: bool,
    root_dev: Option<u64>,
}

struct FileJob {
    src_path: PathBuf,
    dst_path: PathBuf,
    rel: Vec<u8>,
    meta: Meta,
    dst_meta: Option<Meta>,
}

struct DirFixup {
    depth: usize,
    path: PathBuf,
    mode: Option<u32>,
    mtime: Option<(i64, u32)>,
    uid: Option<u32>,
    gid: Option<u32>,
}

/// A name for a message: the root shows as `.`.
fn quoted(rel: &[u8]) -> &[u8] {
    if rel.is_empty() { b"." } else { rel }
}

impl<'a> Engine<'a> {
    pub fn new(opts: &'a Options, filter: Filter, out: &'a Output) -> Engine<'a> {
        let am_root = fsops::am_root();
        let rules = DiffRules {
            perms: opts.perms,
            times: opts.times,
            owner: opts.owner,
            group: opts.group,
            am_root,
            my_groups: if am_root { vec![] } else { fsops::my_groups() },
            update: opts.update,
            size_only: opts.size_only,
            ignore_times: opts.ignore_times,
            ignore_existing: opts.ignore_existing,
            modify_window: opts.modify_window,
        };
        Engine {
            opts,
            rules,
            filter,
            out,
            stats: Stats::default(),
            fixups: Mutex::new(Vec::new()),
            umask: fsops::current_umask(),
        }
    }

    fn err(&self, msg: impl AsRef<[u8]>) {
        Stats::add(&self.stats.errors, 1);
        self.out.error(msg);
    }

    fn dry(&self) -> bool {
        self.opts.dry_run
    }

    /// Permissions for a newly written file or directory.
    fn new_mode(&self, src: &Meta) -> u32 {
        if self.opts.perms { src.mode } else { src.mode & 0o777 & !self.umask }
    }

    // -----------------------------------------------------------------------
    // Roots
    // -----------------------------------------------------------------------

    /// `dest_exists`: the directory roots write into exists. `dest_fresh`: it was created
    /// by this run and nothing has been written into it yet, so it is known to be empty.
    pub fn run_root<'s>(&'s self, s: &Scope<'s>, root: &Root, dest_exists: bool, dest_fresh: bool) {
        match root {
            Root::Contents { src, dst } => {
                let src_meta = match fsops::stat(src) {
                    Ok(m) => m,
                    Err(e) => return self.err(path_msg("link_stat \"", src, format!("\" failed: {e}"))),
                };
                Stats::add(&self.stats.dirs, 1);
                let pre = if dest_exists && !dest_fresh {
                    fsops::lstat(dst).ok().filter(|m| m.kind == Kind::Dir)
                } else {
                    None
                };
                if pre.is_none() || pre.is_some_and(|p| self.opts.times && !self.rules.same_mtime(&src_meta, &p)) {
                    self.out.item(b"", Kind::Dir, None);
                }
                let job = DirJob {
                    src: src.clone(),
                    dst: dst.clone(),
                    rel: Vec::new(),
                    depth: 0,
                    dst_is_new: pre.is_none(),
                    root_dev: Some(src_meta.dev),
                    src_meta,
                    pre,
                };
                s.spawn(move |s| self.process_dir(s, job));
            }
            Root::Items { src_dir, dst_dir, items } => {
                let mut entries = Vec::with_capacity(items.len());
                let mut dst_map = HashMap::new();
                for (sname, dname) in items {
                    let sp = src_dir.join(sname);
                    let meta = match fsops::lstat(&sp) {
                        Ok(m) => m,
                        Err(e) => {
                            self.err(path_msg("link_stat \"", &sp, format!("\" failed: {e}")));
                            continue;
                        }
                    };
                    if self.filter.is_excluded(sname.as_bytes(), meta.kind == Kind::Dir) {
                        continue;
                    }
                    if dest_exists && !dest_fresh && let Ok(dm) = fsops::lstat(&dst_dir.join(dname)) {
                        dst_map.insert(dname.clone(), dm.kind);
                    }
                    entries.push((DirEnt { name: sname.clone(), meta }, Some(dname.clone())));
                }
                let ctx = Ctx {
                    src: src_dir,
                    dst: dst_dir,
                    dst_handle: None,
                    rel: b"",
                    depth: 0,
                    dst_is_new: !dest_exists || dest_fresh,
                    root_dev: None,
                };
                self.process_entries(s, &ctx, entries, &dst_map);
            }
        }
    }

    // -----------------------------------------------------------------------
    // Directories
    // -----------------------------------------------------------------------

    fn process_dir<'s>(&'s self, s: &Scope<'s>, job: DirJob) {
        if fsops::interrupted() {
            return;
        }
        let src_ents = match fsops::read_dir_meta(&job.src) {
            Ok(v) => v,
            Err(e) => {
                self.err(path_msg("opendir \"", &job.src, format!("\" failed: {e}")));
                return self.push_fixup(&job, false);
            }
        };
        let src_ents: Vec<DirEnt> = if self.filter.is_empty() {
            src_ents
        } else {
            src_ents
                .into_iter()
                .filter(|e| !self.filter.is_excluded(&join_rel(&job.rel, &e.name), e.meta.kind == Kind::Dir))
                .collect()
        };
        let dst_map = if job.dst_is_new {
            HashMap::new()
        } else {
            match fsops::read_dir_kinds(&job.dst) {
                Ok(m) => m,
                Err(e) => {
                    self.err(path_msg("opendir \"", &job.dst, format!("\" failed: {e}")));
                    return;
                }
            }
        };

        let mut changed = false;
        if self.opts.delete && !job.dst_is_new {
            changed |= self.delete_extraneous(&job, &src_ents, &dst_map);
        }
        let dst_handle = if job.dst_is_new { None } else { fsops::DirHandle::open(&job.dst).ok() };
        let ctx = Ctx {
            src: &job.src,
            dst: &job.dst,
            dst_handle: dst_handle.as_ref(),
            rel: &job.rel,
            depth: job.depth,
            dst_is_new: job.dst_is_new,
            root_dev: job.root_dev,
        };
        let entries = src_ents.into_iter().map(|e| (e, None)).collect();
        changed |= self.process_entries(s, &ctx, entries, &dst_map);
        self.push_fixup(&job, changed);
    }

    fn delete_extraneous(&self, job: &DirJob, src_ents: &[DirEnt], dst_map: &HashMap<OsString, Kind>) -> bool {
        let src_names: HashSet<&OsStr> = src_ents.iter().map(|e| e.name.as_os_str()).collect();
        let mut extra: Vec<(&OsString, Kind)> = dst_map
            .iter()
            .filter(|(n, _)| !src_names.contains(n.as_os_str()))
            .filter(|(n, k)| {
                self.opts.delete_excluded
                    || !self.filter.is_excluded(&join_rel(&job.rel, n), **k == Kind::Dir)
            })
            .map(|(n, k)| (n, *k))
            .collect();
        extra.sort_unstable_by(|a, b| b.0.as_bytes().cmp(a.0.as_bytes()));
        for (name, kind) in &extra {
            self.delete_tree(&job.dst.join(name), &join_rel(&job.rel, name), *kind, true);
        }
        !extra.is_empty()
    }

    /// Remove `path` (recursively for directories). `report` prints rsync's
    /// "deleting" lines; replacements of one file type by another are silent.
    fn delete_tree(&self, path: &Path, rel: &[u8], kind: Kind, report: bool) {
        if kind == Kind::Dir {
            match fsops::read_dir_kinds(path) {
                Ok(m) => {
                    let mut kids: Vec<_> = m.into_iter().collect();
                    kids.sort_unstable_by(|a, b| b.0.as_bytes().cmp(a.0.as_bytes()));
                    for (n, k) in kids {
                        self.delete_tree(&path.join(&n), &join_rel(rel, &n), k, report);
                    }
                }
                Err(e) => return self.err(path_msg("opendir \"", path, format!("\" failed: {e}"))),
            }
            if report {
                self.out.deleting(rel, true);
            }
            if !self.dry()
                && let Err(e) = fs::remove_dir(path)
            {
                return self.err(path_msg("delete_file: rmdir \"", path, format!("\" failed: {e}")));
            }
        } else {
            if report {
                self.out.deleting(rel, false);
            }
            if !self.dry()
                && let Err(e) = fs::remove_file(path)
            {
                return self.err(path_msg("delete_file: unlink \"", path, format!("\" failed: {e}")));
            }
        }
        Stats::add(&self.stats.deleted, 1);
    }

    /// A directory sits where a non-directory must go. rsync only removes it when
    /// deletions are enabled or it is empty.
    fn remove_conflicting_dir(&self, path: &Path, rel: &[u8]) -> bool {
        if self.opts.delete {
            self.delete_tree(path, rel, Kind::Dir, false);
            return true;
        }
        if self.dry() {
            return true;
        }
        match fs::remove_dir(path) {
            Ok(()) => true,
            Err(_) => {
                self.err([b"cannot delete non-empty directory: ", quoted(rel)].concat());
                false
            }
        }
    }

    fn push_fixup(&self, job: &DirJob, changed: bool) {
        if self.dry() {
            return;
        }
        let src = &job.src_meta;
        let is_new = job.pre.is_none();
        let mode = match &job.pre {
            None => Some(self.new_mode(src)),
            Some(p) if self.opts.perms && p.mode != src.mode => Some(src.mode),
            _ => None,
        };
        let mtime = (self.opts.times
            && (is_new || changed || job.pre.is_some_and(|p| !self.rules.same_mtime(src, &p))))
        .then_some((src.mtime, src.mtime_ns));
        let uid = self.rules.wanted_uid(src).filter(|&u| job.pre.is_none_or(|p| p.uid != u));
        let gid = self.rules.wanted_gid(src).filter(|&g| job.pre.is_none_or(|p| p.gid != g));
        if mode.is_some() || mtime.is_some() || uid.is_some() || gid.is_some() {
            self.fixups.lock().unwrap().push(DirFixup {
                depth: job.depth,
                path: job.dst.clone(),
                mode,
                mtime,
                uid,
                gid,
            });
        }
    }

    /// Apply directory attributes, deepest level first; directories on one level are
    /// independent so each level is applied in parallel.
    pub fn apply_fixups(&self) {
        let mut fixups = std::mem::take(&mut *self.fixups.lock().unwrap());
        fixups.sort_unstable_by(|a, b| b.depth.cmp(&a.depth));
        for level in fixups.chunk_by(|a, b| a.depth == b.depth) {
            level.par_iter().for_each(|f| {
                if let Err(e) = fsops::lchown(&f.path, f.uid, f.gid) {
                    self.err(path_msg("chown \"", &f.path, format!("\" failed: {e}")));
                }
                if let Some(m) = f.mode
                    && let Err(e) = fsops::chmod(&f.path, m)
                {
                    self.err(path_msg("chmod \"", &f.path, format!("\" failed: {e}")));
                }
                if let Some((s, ns)) = f.mtime
                    && let Err(e) = fsops::set_mtime(&f.path, s, ns)
                {
                    self.err(path_msg("failed to set times on \"", &f.path, format!("\": {e}")));
                }
            });
        }
    }

    // -----------------------------------------------------------------------
    // Entries
    // -----------------------------------------------------------------------

    /// Handle the children of one directory. `entries` pairs each source entry with an
    /// optional destination-name override (used when a single file is copied to a new name).
    /// Returns whether the destination directory itself was modified.
    fn process_entries<'s>(
        &'s self,
        s: &Scope<'s>,
        ctx: &Ctx,
        entries: Vec<(DirEnt, Option<OsString>)>,
        dst_map: &HashMap<OsString, Kind>,
    ) -> bool {
        let mut changed = false;
        let mut files: Vec<FileJob> = Vec::new();
        for (e, dname_override) in entries {
            if fsops::interrupted() {
                break;
            }
            let dname: &OsStr = dname_override.as_deref().unwrap_or(&e.name);
            let rel = join_rel(ctx.rel, &e.name);
            let src_path = ctx.src.join(&e.name);
            let dst_path = ctx.dst.join(dname);
            let dst_kind = if ctx.dst_is_new { None } else { dst_map.get(dname).copied() };
            match e.meta.kind {
                Kind::Dir => changed |= self.handle_dir(s, ctx, e.meta, rel, src_path, dst_path, dst_kind),
                Kind::File => {
                    Stats::add(&self.stats.files, 1);
                    Stats::add(&self.stats.total_size, e.meta.size);
                    let mut dst_meta = None;
                    match dst_kind {
                        Some(Kind::Dir) => {
                            if !self.remove_conflicting_dir(&dst_path, &rel) {
                                continue;
                            }
                            changed = true;
                        }
                        Some(_) => match ctx.lstat_dst(dname, &dst_path) {
                            Ok(m) => dst_meta = Some(m),
                            Err(e) if e.kind() == io::ErrorKind::NotFound => {}
                            Err(e) => {
                                self.err(path_msg("stat \"", &dst_path, format!("\" failed: {e}")));
                                continue;
                            }
                        },
                        None => {}
                    }
                    match file_action(&e.meta, dst_meta.as_ref(), &self.rules) {
                        FileAction::Skip => {}
                        FileAction::Attrs(a) => {
                            if !self.dry() {
                                self.fix_attrs(&dst_path, &e.meta, a);
                            }
                        }
                        FileAction::Transfer => {
                            changed = true;
                            files.push(FileJob { src_path, dst_path, rel, meta: e.meta, dst_meta });
                        }
                    }
                }
                Kind::Symlink => {
                    Stats::add(&self.stats.links, 1);
                    if !self.opts.links {
                        self.out.info([b"skipping non-regular file \"", quoted(&rel), b"\""].concat());
                        continue;
                    }
                    changed |= self.handle_symlink(&e.meta, &rel, &src_path, &dst_path, dst_kind);
                }
                kind => {
                    Stats::add(&self.stats.specials, 1);
                    let allowed = match kind {
                        Kind::CharDev | Kind::BlockDev => self.opts.devices,
                        Kind::Fifo => self.opts.specials,
                        _ => false,
                    };
                    if !allowed {
                        self.out.info([b"skipping non-regular file \"", quoted(&rel), b"\""].concat());
                        continue;
                    }
                    changed |= self.handle_special(&e.meta, &rel, &dst_path, dst_kind);
                }
            }
        }
        match files.len() {
            0 => {}
            1 => self.transfer_file(&files[0]),
            _ => files.par_iter().for_each(|j| self.transfer_file(j)),
        }
        changed && !self.dry()
    }

    #[allow(clippy::too_many_arguments)]
    fn handle_dir<'s>(
        &'s self,
        s: &Scope<'s>,
        ctx: &Ctx,
        meta: Meta,
        rel: Vec<u8>,
        src_path: PathBuf,
        dst_path: PathBuf,
        dst_kind: Option<Kind>,
    ) -> bool {
        Stats::add(&self.stats.dirs, 1);
        if !self.opts.recursive {
            self.out.info([b"skipping directory ", quoted(&rel)].concat());
            return false;
        }
        let mut changed = false;
        let mut pre = None;
        match dst_kind {
            Some(Kind::Dir) => match ctx.lstat_dst(dst_path.file_name().unwrap_or_default(), &dst_path) {
                Ok(m) if m.kind == Kind::Dir => pre = Some(m),
                Ok(_) => {}
                Err(e) if e.kind() == io::ErrorKind::NotFound => {}
                Err(e) => {
                    self.err(path_msg("stat \"", &dst_path, format!("\" failed: {e}")));
                    return false;
                }
            },
            Some(_) => {
                if !self.dry()
                    && let Err(e) = fs::remove_file(&dst_path)
                {
                    self.err(path_msg("unlink \"", &dst_path, format!("\" failed: {e}")));
                    return false;
                }
                changed = true;
            }
            None => {}
        }
        let is_new = pre.is_none();
        if is_new || pre.is_some_and(|p| self.opts.times && !self.rules.same_mtime(&meta, &p)) {
            self.out.item(&rel, Kind::Dir, None);
        }
        if is_new {
            Stats::add(&self.stats.created, 1);
            if !self.dry() {
                let r = fs::DirBuilder::new().mode(0o700 | self.new_mode(&meta)).create(&dst_path);
                if let Err(e) = r {
                    self.err(path_msg("mkdir \"", &dst_path, format!("\" failed: {e}")));
                    return changed;
                }
                changed = true;
            }
        }
        let root_dev = ctx.root_dev.unwrap_or(meta.dev);
        let job = DirJob {
            src: src_path,
            dst: dst_path,
            rel,
            depth: ctx.depth + 1,
            dst_is_new: is_new,
            root_dev: Some(root_dev),
            src_meta: meta,
            pre,
        };
        if self.opts.one_file_system && meta.dev != root_dev {
            self.push_fixup(&job, false);
        } else {
            s.spawn(move |s| self.process_dir(s, job));
        }
        changed
    }

    fn handle_symlink(&self, meta: &Meta, rel: &[u8], src_path: &Path, dst_path: &Path, dst_kind: Option<Kind>) -> bool {
        let target = match fs::read_link(src_path) {
            Ok(t) => t,
            Err(e) => {
                self.err(path_msg("readlink \"", src_path, format!("\" failed: {e}")));
                return false;
            }
        };
        match dst_kind {
            Some(Kind::Symlink) => {
                if fs::read_link(dst_path).is_ok_and(|cur| cur == target) {
                    if !self.dry()
                        && let Ok(d) = fsops::lstat(dst_path)
                    {
                        let a = self.rules.attrs_differ(meta, &d);
                        if a.owner {
                            let _ = fsops::lchown(dst_path, self.rules.wanted_uid(meta), self.rules.wanted_gid(meta));
                        }
                        if a.mtime {
                            let _ = fsops::set_symlink_mtime(dst_path, meta.mtime, meta.mtime_ns);
                        }
                    }
                    return false;
                }
            }
            Some(Kind::Dir) => {
                if !self.remove_conflicting_dir(dst_path, rel) {
                    return false;
                }
            }
            _ => {}
        }
        self.out.item(rel, Kind::Symlink, Some(target.as_os_str().as_bytes()));
        if dst_kind.is_none() {
            Stats::add(&self.stats.created, 1);
        }
        if self.dry() {
            return true;
        }
        let (dir, name) = split_dst(dst_path);
        let res = fsops::with_temp(dir, name, |tmp| std::os::unix::fs::symlink(&target, tmp)).and_then(|(tmp, ())| {
            let r = (|| {
                fsops::lchown(&tmp, self.rules.wanted_uid(meta), self.rules.wanted_gid(meta))?;
                if self.opts.times {
                    fsops::set_symlink_mtime(&tmp, meta.mtime, meta.mtime_ns)?;
                }
                fs::rename(&tmp, dst_path)
            })();
            finish_temp(&tmp, r)
        });
        if let Err(e) = res {
            self.err(path_msg("symlink \"", dst_path, format!("\" failed: {e}")));
        }
        true
    }

    fn handle_special(&self, meta: &Meta, rel: &[u8], dst_path: &Path, dst_kind: Option<Kind>) -> bool {
        match dst_kind {
            Some(k) if k == meta.kind => {
                if let Ok(d) = fsops::lstat(dst_path)
                    && d.rdev == meta.rdev
                {
                    let a = self.rules.attrs_differ(meta, &d);
                    if a.any() && !self.dry() {
                        self.fix_attrs(dst_path, meta, a);
                    }
                    return false;
                }
            }
            Some(Kind::Dir) => {
                if !self.remove_conflicting_dir(dst_path, rel) {
                    return false;
                }
            }
            _ => {}
        }
        self.out.item(rel, meta.kind, None);
        Stats::add(&self.stats.created, 1);
        if self.dry() {
            return true;
        }
        let (dir, name) = split_dst(dst_path);
        let res = fsops::with_temp(dir, name, |tmp| fsops::mknod(tmp, meta.kind, meta.mode, meta.rdev)).and_then(
            |(tmp, ())| {
                let r = (|| {
                    fsops::chmod(&tmp, self.new_mode(meta))?;
                    fsops::lchown(&tmp, self.rules.wanted_uid(meta), self.rules.wanted_gid(meta))?;
                    if self.opts.times {
                        fsops::set_mtime(&tmp, meta.mtime, meta.mtime_ns)?;
                    }
                    fs::rename(&tmp, dst_path)
                })();
                finish_temp(&tmp, r)
            },
        );
        if let Err(e) = res {
            self.err(path_msg("mknod \"", dst_path, format!("\" failed: {e}")));
        }
        true
    }

    fn fix_attrs(&self, path: &Path, src: &Meta, a: AttrDiff) {
        let r = (|| {
            if a.owner {
                fsops::lchown(path, self.rules.wanted_uid(src), self.rules.wanted_gid(src))?;
            }
            if a.mode {
                fsops::chmod(path, src.mode)?;
            }
            if a.mtime {
                fsops::set_mtime(path, src.mtime, src.mtime_ns)?;
            }
            Ok::<(), io::Error>(())
        })();
        if let Err(e) = r {
            self.err(path_msg("failed to set attributes on \"", path, format!("\": {e}")));
        }
    }

    // -----------------------------------------------------------------------
    // File data
    // -----------------------------------------------------------------------

    fn transfer_file(&self, job: &FileJob) {
        if fsops::interrupted() {
            return;
        }
        self.out.item(&job.rel, Kind::File, None);
        Stats::add(&self.stats.transferred, 1);
        Stats::add(&self.stats.transferred_bytes, job.meta.size);
        if job.dst_meta.is_none() {
            Stats::add(&self.stats.created, 1);
        }
        if self.dry() {
            return;
        }
        if let Err(e) = self.write_file(job)
            && !fsops::interrupted()
        {
            let to = path_msg("\" -> \"", &job.dst_path, format!("\" failed: {e}"));
            self.err(path_msg("copy \"", &job.src_path, to));
        }
    }

    fn write_file(&self, job: &FileJob) -> io::Result<()> {
        let src = &job.meta;
        let mode = match (&job.dst_meta, self.opts.perms) {
            (Some(d), false) if d.kind == Kind::File => d.mode,
            _ => self.new_mode(src),
        };
        let uid = self.rules.wanted_uid(src);
        let gid = self.rules.wanted_gid(src);

        if self.delta_candidate(job) {
            match self.write_delta(job, mode, uid, gid) {
                Ok(true) => return Ok(()),
                Ok(false) => {}
                Err(e) if fsops::interrupted() => return Err(e),
                // E.g. a file changed size while it was being hashed: send it whole.
                Err(_) => {}
            }
        }

        if self.opts.inplace && job.dst_meta.is_some_and(|d| d.kind == Kind::File) {
            fsops::copy_into_existing(&job.src_path, &job.dst_path)?;
            fsops::lchown(&job.dst_path, uid, gid)?;
            fsops::chmod(&job.dst_path, mode)?;
            if self.opts.times {
                fsops::set_mtime(&job.dst_path, src.mtime, src.mtime_ns)?;
            }
            if self.opts.fsync {
                fsops::fsync_path(&job.dst_path)?;
            }
            return Ok(());
        }

        let allow_fast = !self.opts.no_clone;
        let mtime = self.opts.times.then_some((src.mtime, src.mtime_ns));
        if self.opts.inplace && job.dst_meta.is_none() {
            // Like rsync --inplace, new files are written at their final name: no temp
            // file and no rename (the rename is ~1/3 of the cost of a small file on APFS).
            // The mtime is set last, so an interrupted copy never passes the quick check;
            // on Ctrl-C the partial file is removed like a temp file.
            fsops::register_temp(&job.dst_path);
            let r = fsops::copy_new(&job.src_path, &job.dst_path, src.size, allow_fast, mode)
                .and_then(|copied| self.apply_new_file_attrs(&job.dst_path, copied, mode, uid, gid, mtime))
                .and_then(|()| fsops::check_interrupt());
            return finish_temp(&job.dst_path, r);
        }
        let (dir, name) = split_dst(&job.dst_path);
        let (tmp, copied) =
            fsops::with_temp(dir, name, |tmp| fsops::copy_new(&job.src_path, tmp, src.size, allow_fast, mode))?;
        let r = self
            .apply_new_file_attrs(&tmp, copied, mode, uid, gid, mtime)
            .and_then(|()| fsops::check_interrupt())
            .and_then(|()| fs::rename(&tmp, &job.dst_path));
        finish_temp(&tmp, r)
    }

    /// Delta transfer is opt-in locally (`--no-whole-file`), like rsync, and needs an
    /// existing destination file to use as the basis.
    fn delta_candidate(&self, job: &FileJob) -> bool {
        self.opts.whole_file == Some(false)
            && job.meta.size >= delta::MIN_DELTA_SIZE
            && job.dst_meta.is_some_and(|d| d.kind == Kind::File && d.size > 0)
    }

    /// Update `job.dst_path` from the old destination file plus the changed ranges of
    /// the source. Returns `Ok(false)` when nothing matched (a whole-file copy is cheaper).
    ///
    /// When matched ranges sit at the same offsets, the old file is cloned (APFS,
    /// btrfs, XFS) and only the changed ranges are written; otherwise the new file is
    /// rebuilt in parallel from both files. With `--inplace` and an in-place delta,
    /// the changed ranges are written straight into the destination.
    fn write_delta(&self, job: &FileJob, mode: u32, uid: Option<u32>, gid: Option<u32>) -> io::Result<bool> {
        let new = fs::File::open(&job.src_path)?;
        let basis = fs::File::open(&job.dst_path)?;
        let basis_size = basis.metadata()?.len();
        let d = delta::local_delta(&new, job.meta.size, &basis, basis_size, &delta::Params::DEFAULT)?;
        if d.matched_bytes() == 0 {
            return Ok(false);
        }
        Stats::add(&self.stats.delta_files, 1);
        Stats::add(&self.stats.literal_bytes, d.literal_bytes());
        Stats::add(&self.stats.matched_bytes, d.matched_bytes());

        if self.opts.inplace && d.is_in_place() {
            let out = fs::OpenOptions::new().write(true).open(&job.dst_path)?;
            apply(&d, &basis, &new, &out, true)?;
            self.finish_delta_file(&out, &job.meta, mode, uid, gid)?;
            return Ok(true);
        }

        let (dir, name) = split_dst(&job.dst_path);
        let want_clone = !self.opts.no_clone && d.in_place_bytes() > 0;
        let (tmp, cloned) = fsops::with_temp(dir, name, |tmp| {
            if want_clone && fsops::clone_into_new(&basis, tmp)? {
                return Ok(true);
            }
            fsops::create_new(tmp, 0o600).map(|_| false)
        })?;
        if cloned {
            Stats::add(&self.stats.cloned, 1);
        }
        let r = (|| {
            let out = fs::OpenOptions::new().write(true).open(&tmp)?;
            apply(&d, &basis, &new, &out, cloned)?;
            self.finish_delta_file(&out, &job.meta, mode, uid, gid)?;
            drop(out);
            fsops::check_interrupt()?;
            fs::rename(&tmp, &job.dst_path)
        })();
        finish_temp(&tmp, r).map(|()| true)
    }

    /// Attributes for a delta-built file. It may be a clone of the old file, which
    /// carries the old mode and mtime, so both are always set.
    fn finish_delta_file(&self, f: &fs::File, src: &Meta, mode: u32, uid: Option<u32>, gid: Option<u32>) -> io::Result<()> {
        fsops::fchown(f, uid, gid)?;
        fsops::fchmod(f, mode)?;
        if self.opts.times {
            fsops::set_fd_mtime(f, src.mtime, src.mtime_ns)?;
        } else {
            fsops::set_fd_mtime_now(f)?;
        }
        if self.opts.fsync {
            f.sync_all()?;
        }
        Ok(())
    }

    /// Set owner, mode and mtime on a freshly written file, by descriptor when we
    /// still hold one (one metadata update each, no path lookups).
    fn apply_new_file_attrs(
        &self,
        path: &Path,
        copied: Copied,
        mode: u32,
        uid: Option<u32>,
        gid: Option<u32>,
        mtime: Option<(i64, u32)>,
    ) -> io::Result<()> {
        match copied {
            Copied::Written(f) => {
                fsops::fchown(&f, uid, gid)?;
                // open() already applied `mode & !umask`; only fix up what it couldn't set
                // (bits masked by the umask, setuid/setgid/sticky, or bits cleared by chown).
                if mode & !self.umask & 0o777 != mode || uid.is_some() || gid.is_some() {
                    fsops::fchmod(&f, mode)?;
                }
                if let Some((s, ns)) = mtime {
                    fsops::set_fd_mtime(&f, s, ns)?;
                }
                if self.opts.fsync {
                    f.sync_all()?;
                }
                Ok(())
            }
            Copied::Clone => {
                Stats::add(&self.stats.cloned, 1);
                fsops::lchown(path, uid, gid)?;
                fsops::chmod(path, mode)?;
                match mtime {
                    Some((s, ns)) => fsops::set_mtime(path, s, ns)?,
                    // Clones inherit the source mtime; without -t rsync leaves "now".
                    None => fsops::touch_now(path)?,
                }
                if self.opts.fsync {
                    fsops::fsync_path(path)?;
                }
                Ok(())
            }
        }
    }
}

impl Ctx<'_> {
    fn lstat_dst(&self, name: &OsStr, path: &Path) -> io::Result<Meta> {
        match self.dst_handle {
            Some(h) => h.lstat(name),
            None => fsops::lstat(path),
        }
    }
}

fn split_dst(path: &Path) -> (&Path, &OsStr) {
    let dir = path.parent().filter(|p| !p.as_os_str().is_empty()).unwrap_or(Path::new("."));
    (dir, path.file_name().unwrap_or(OsStr::new("")))
}

/// Unregister a temp file that was renamed into place, or delete it on failure.
fn finish_temp(tmp: &Path, r: io::Result<()>) -> io::Result<()> {
    match r {
        Ok(()) => {
            fsops::unregister_temp(tmp);
            Ok(())
        }
        Err(e) => {
            fsops::drop_temp(tmp);
            Err(e)
        }
    }
}

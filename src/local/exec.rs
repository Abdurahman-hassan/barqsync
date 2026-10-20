//! The sync engine (receiving side): one rayon task per directory.
//!
//! Each directory task takes the source directory's listing, compares it with the
//! matching destination directory, deletes extraneous entries (`--delete`), creates
//! subdirectories and descends into them, then has its changed files written.
//! Directory attributes are applied after everything else, deepest first, so a
//! read-only source directory stays writable while it is being filled.
//!
//! Locally the listing comes from `readdir` and files are copied in parallel. As a
//! remote receiver (see `remote::receiver`), listings arrive as `DirBatch` frames,
//! subdirectories wait for their batch, and changed files are requested from the sender.
//! With `--scanner=jwalk` (see `local::pipeline`), listings come from a parallel tree
//! scan, destination entries from a pre-scanned index, and files go through queues.

use super::Root;
use crate::cli::Options;
use crate::delta::{self, DeltaMode, apply::apply, cdc};
use super::extra::{DstExtra, Src, XSource};
use crate::diff::{AttrDiff, DiffRules, FileAction, file_action_ext};
use crate::entry::{DirEnt, Extra, Kind, Meta};
use crate::filter::Filter;
use crate::fsops::{self, Copied};
use crate::output::{Change, Link, Output, Stats, path_msg};
use crate::paths::join_rel;
use crate::checksum::{self, Sum};
use crate::select::{self, Descend, Sel};
use rayon::Scope;
use rayon::prelude::*;
use std::collections::{HashMap, HashSet};
use std::ffi::{OsStr, OsString};
use std::fs;
use std::io;
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::DirBuilderExt;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

pub struct Engine<'a> {
    pub opts: &'a Options,
    pub rules: DiffRules,
    pub filter: Filter,
    pub out: &'a Output,
    pub stats: Stats,
    fixups: Mutex<Vec<DirFixup>>,
    /// `-H`: hard-link groups keyed by the source `(dev, ino)`.
    hardlinks: Mutex<HashMap<(u64, u64), HardGroup>>,
    pub(crate) umask: u32,
    /// Set when this engine is the receiving end of a remote transfer.
    pub(crate) remote: Option<crate::remote::receiver::RemoteState>,
    /// The destination directory transfer roots write into (alt-dest and backup paths
    /// are relative to it).
    pub(crate) dest_root: PathBuf,
    /// `--link-dest` / `--compare-dest` directories that exist.
    pub(crate) alt_dirs: Vec<PathBuf>,
    /// `--backup-dir`, resolved against `dest_root`.
    pub(crate) backup_root: Option<PathBuf>,
    /// `--scanner=jwalk`, local transfers: the scan feed and the transfer queues.
    pub(crate) pipeline: Option<super::pipeline::Pipeline>,
    /// `--scanner=jwalk`: destination entries scanned ahead (local and remote receiver).
    pub(crate) dst_index: Option<Arc<crate::scan::DstIndex>>,
    /// `-X`: which attribute names take part.
    pub(crate) xrules: crate::xattrs::Rules,
}

pub(crate) struct DirJob {
    pub(crate) src: PathBuf,
    pub(crate) dst: PathBuf,
    pub(crate) rel: Vec<u8>,
    /// Transfer root this directory belongs to, and its id on the wire (remote only).
    pub(crate) root: u16,
    pub(crate) wire_dir: u32,
    depth: usize,
    /// The destination directory was just created (or would be, in a dry run): it is empty.
    dst_is_new: bool,
    root_dev: Option<u64>,
    src_meta: Meta,
    /// Destination directory metadata before we touched it (`None` when new).
    pre: Option<Meta>,
    /// `-R`/`--files-from`: only these entries are transferred (`None`: all of them).
    pub(crate) sel: Option<&'static Sel>,
}

pub(crate) struct Ctx<'p> {
    src: &'p Path,
    dst: &'p Path,
    /// Open handle on `dst` for `fstatat` (cheaper than resolving full paths).
    dst_handle: Option<&'p fsops::DirHandle>,
    /// `--scanner=jwalk`: `dst`'s entries from the destination index (no `fstatat`).
    dst_metas: Option<&'p HashMap<OsString, Meta>>,
    rel: &'p [u8],
    root: u16,
    depth: usize,
    dst_is_new: bool,
    root_dev: Option<u64>,
    sel: Option<&'static Sel>,
    /// Listing id on the wire (remote: finds the sender's `-c` checksums).
    wire_dir: u32,
}

pub(crate) struct FileJob {
    /// Index in the source listing (remote: identifies the file on the wire).
    pub(crate) idx: u32,
    pub(crate) src_path: PathBuf,
    pub(crate) dst_path: PathBuf,
    pub(crate) rel: Vec<u8>,
    pub(crate) meta: Meta,
    pub(crate) dst_meta: Option<Meta>,
    pub(crate) root: u16,
    /// Delta basis other than the destination file: a `--partial-dir` file or an
    /// alt-dest file.
    pub(crate) basis: Option<PathBuf>,
    /// The alt-dest file the new file is itemized against (no destination file).
    pub(crate) alt: Option<Meta>,
    /// `--partial-dir` file to remove once the file is written.
    pub(crate) partial: Option<PathBuf>,
    /// `-X`/`-A`: the source's attributes and ACLs (receiver's form).
    pub(crate) extra: Option<Box<Extra>>,
    /// Remote: the attribute values in full, sent with the data when the listing only
    /// had checksums of some.
    pub(crate) full_xattrs: Option<Vec<crate::xattrs::Xattr>>,
}

impl FileJob {
    /// The file a delta is computed against.
    pub(crate) fn basis_path(&self) -> &Path {
        self.basis.as_deref().unwrap_or(&self.dst_path)
    }

    /// The source side, for writing its attributes onto the new file.
    pub(crate) fn src(&self, remote: bool) -> Src<'_> {
        let from = match (&self.full_xattrs, remote) {
            (Some(f), _) => XSource::Full(f),
            (None, false) => XSource::Local(&self.src_path),
            (None, true) => XSource::Full(&[]),
        };
        Src { kind: Kind::File, meta: &self.meta, extra: self.extra.as_deref(), from }
    }
}

/// Source entries of one directory, each with its index in the source listing and an
/// optional destination-name override (a single file copied to a new name).
pub(crate) type Entries = Vec<(u32, DirEnt, Option<OsString>)>;

/// Names of one multiply-linked source file. The first name reached is transferred
/// normally (the leader); the others are hard-linked to it once all data is written.
struct HardGroup {
    leader_dst: PathBuf,
    leader_rel: Vec<u8>,
    followers: Vec<Follower>,
    /// `-X`: the leader's attributes were updated in place, so names already linked to
    /// it had the old ones too (see [`Follower::xattr`]).
    xattr: bool,
}

struct Follower {
    root: u16,
    src_path: PathBuf,
    dst_path: PathBuf,
    rel: Vec<u8>,
    meta: Meta,
    /// `-X`: the destination name's attributes differed when it was reached. rsync shows
    /// that on a name already linked to its leader too (its receiver writes attributes
    /// after the generator has looked at every name, unlike modes and ACLs).
    xattr: bool,
}

struct DirFixup {
    depth: usize,
    path: PathBuf,
    /// `-K`: `path` is a symlink standing in for the directory; like rsync, its own
    /// time is set and the mode is left alone.
    link: bool,
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
            checksum: opts.checksum,
        };
        Engine {
            opts,
            rules,
            filter,
            out,
            stats: Stats::default(),
            fixups: Mutex::new(Vec::new()),
            hardlinks: Mutex::new(HashMap::new()),
            umask: fsops::current_umask(),
            remote: None,
            dest_root: PathBuf::from("."),
            alt_dirs: Vec::new(),
            backup_root: None,
            pipeline: None,
            dst_index: None,
            xrules: crate::xattrs::Rules { level: opts.xattrs, am_root },
        }
    }

    /// A remote receiver's roots that write into the same destination (see `Overlap`).
    fn overlap(&self) -> Option<&crate::remote::receiver::Overlap> {
        self.remote.as_ref()?.overlap.as_ref()
    }

    /// Put the new symlink `tmp` in place at `dst`.
    pub(crate) fn place_link(&self, tmp: &Path, dst: &Path) -> io::Result<()> {
        match self.overlap() {
            Some(o) => o.place_link(dst, || fs::rename(tmp, dst)),
            None => fs::rename(tmp, dst),
        }
    }

    pub(crate) fn err(&self, msg: impl AsRef<[u8]>) {
        Stats::add(&self.stats.errors, 1);
        self.out.error(msg);
    }

    pub(crate) fn dry(&self) -> bool {
        self.opts.dry_run
    }

    /// What differs between a source item and the existing destination item, for `-i`.
    /// Without `-t`, an mtime difference is only reported for data that is rewritten
    /// (shown as `T`: the time will be the transfer time).
    pub(crate) fn attr_change(&self, src: &Meta, dst: &Meta, transferring: bool) -> Change {
        Change {
            time: (self.opts.times || transferring) && !self.rules.same_mtime(src, dst),
            perms: self.opts.perms && src.mode != dst.mode,
            owner: self.rules.wanted_uid(src).is_some_and(|u| u != dst.uid),
            group: self.rules.wanted_gid(src).is_some_and(|g| g != dst.gid),
            ..Change::default()
        }
    }

    /// Report a directory: new (`cd+++++++++`), or what differs. Plain `-v` shows it
    /// only when new or its time differs.
    fn report_dir(&self, rel: &[u8], src: &Meta, pre: Option<&Meta>, (xattr, acl): (bool, bool)) {
        if pre.is_none()
            && let Some(alt) = self.alt_dir_meta(rel)
        {
            // A new directory with an alt-dest counterpart is itemized against it.
            let significant = self.dir_times() && !self.rules.same_mtime(src, &alt);
            let change = Change { local: true, time: significant, ..self.attr_change(src, &alt, false) };
            if change.any_attr() || self.opts.itemize >= 2 || significant {
                self.out.item(rel, Kind::Dir, Link::None, change, significant);
            }
            return;
        }
        let (change, significant) = match pre {
            None => (Change::new_local(), true),
            Some(p) => {
                let differs = self.dir_times() && !self.rules.same_mtime(src, p);
                (Change { time: differs, xattr, acl, ..self.attr_change(src, p, false) }, differs)
            }
        };
        self.out.item(rel, Kind::Dir, Link::None, change, significant);
    }

    /// Directory mtimes are preserved: `-t`, unless `-O` (which `-b` without
    /// `--backup-dir` forces on, as in rsync).
    fn dir_times(&self) -> bool {
        self.opts.times && !self.opts.omit_dir_times
    }

    /// Permissions for a newly written file or directory.
    pub(crate) fn new_mode(&self, src: &Meta) -> u32 {
        if self.opts.perms { src.mode } else { src.mode & 0o777 & !self.umask }
    }

    /// `p` for an in-place update of a non-directory. rsync on Linux writes a changed
    /// access ACL before it itemizes the entry, and that write sets the permission bits:
    /// only setuid, setgid and sticky can still differ then.
    fn perms_shown(&self, src: &Meta, dst: &Meta, perms: bool, acl_rewritten: bool) -> bool {
        if acl_rewritten && crate::acls::Flavor::native() == crate::acls::Flavor::Posix {
            perms && src.mode & 0o7000 != dst.mode & 0o7000
        } else {
            perms
        }
    }

    /// Where an entry's full attribute values come from (see [`XSource`]).
    fn xsource<'p>(&self, ctx: &Ctx, idx: u32, src_path: &'p Path, rel: &'p [u8], dst_path: &'p Path) -> XSource<'p> {
        match self.remote {
            Some(_) => XSource::Remote { dir: ctx.wire_dir, idx, root: ctx.root, rel, dst: dst_path },
            None => XSource::Local(src_path),
        }
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
                    Ok(mut m) => {
                        super::flags::adjust_mode(self.opts, &mut m);
                        m
                    }
                    Err(e) => return self.err(path_msg("link_stat \"", src, format!("\" failed: {e}"))),
                };
                let extra = self.local_root_extra(src, &src_meta);
                let job = self.root_job(src.clone(), dst.clone(), src_meta, 0, dest_exists && !dest_fresh, extra);
                match &self.pipeline {
                    Some(p) => p.expect_dir(s, self, job),
                    None => s.spawn(move |s| self.process_dir(s, job)),
                }
            }
            Root::Items { src_dir, dst_dir, items } => {
                let mut ents = Vec::with_capacity(items.len());
                for (sname, _) in items {
                    let sp = src_dir.join(sname);
                    let meta = match fsops::lstat(&sp) {
                        Ok(m) => m,
                        Err(e) => {
                            self.err(path_msg("link_stat \"", &sp, format!("\" failed: {e}")));
                            continue;
                        }
                    };
                    let link = if meta.kind == Kind::Symlink { fsops::read_link_bytes(&sp).ok() } else { None };
                    ents.push(DirEnt::new(sname.clone(), meta, link));
                }
                let ents = self.adjust_listing(src_dir, ents);
                let entries = ents
                    .into_iter()
                    .enumerate()
                    .map(|(i, e)| {
                        let dname = items.iter().find(|(n, _)| *n == e.name).map(|(_, d)| d.clone());
                        (i as u32, e, dname)
                    })
                    .collect();
                self.run_items(s, src_dir, dst_dir, 0, 0, entries, !dest_exists || dest_fresh, None);
            }
            Root::Select { base, dst, sel, with_root } => {
                if *with_root {
                    let src_meta = match fsops::stat(base) {
                        Ok(mut m) => {
                            super::flags::adjust_mode(self.opts, &mut m);
                            m
                        }
                        Err(e) => return self.err(path_msg("link_stat \"", base, format!("\" failed: {e}"))),
                    };
                    let extra = self.local_root_extra(base, &src_meta);
                    let mut job = self.root_job(base.clone(), dst.clone(), src_meta, 0, dest_exists && !dest_fresh, extra);
                    job.sel = sel.at_root(self.opts.recursive);
                    s.spawn(move |s| self.process_dir(s, job));
                } else {
                    let ents = self.read_selected(base, sel);
                    let entries = ents.into_iter().enumerate().map(|(i, e)| (i as u32, e, None)).collect();
                    self.run_items(s, base, dst, 0, 0, entries, !dest_exists || dest_fresh, Some(sel));
                }
            }
        }
    }

    /// The selected entries of `dir`, with missing names reported as errors.
    fn read_selected(&self, dir: &Path, sel: &Sel) -> Vec<DirEnt> {
        let (ents, errs) = select::read_selected(dir, sel);
        for (p, e) in errs {
            self.err(path_msg("link_stat \"", &p, format!("\" failed: {e}")));
        }
        self.adjust_listing(dir, ents)
    }

    /// The job for a `src/` root: `dst` itself gets the source directory's attributes
    /// (shown as `./`). `check_dst`: `dst` may already exist (it wasn't just created).
    /// `extra`: the root's attributes and ACLs, with attribute values in full.
    pub(crate) fn root_job(
        &self,
        src: PathBuf,
        dst: PathBuf,
        src_meta: Meta,
        root: u16,
        check_dst: bool,
        extra: Option<Box<Extra>>,
    ) -> DirJob {
        Stats::add(&self.stats.dirs, 1);
        let pre = if check_dst { fsops::lstat(&dst).ok().filter(|m| m.kind == Kind::Dir) } else { None };
        let x = Src { kind: Kind::Dir, meta: &src_meta, extra: extra.as_deref(), from: XSource::Full(extra.as_ref().map_or(&[][..], |e| &e.xattrs)) };
        let (ext, have) = match &pre {
            Some(p) => self.extra_change(&x, &dst, p),
            None => ((false, false), None),
        };
        self.report_dir(b"", &src_meta, pre.as_ref(), ext);
        if !self.dry() && self.extras_on() && (pre.is_none() || ext.0 || ext.1) {
            let cur = pre.map_or(0o777 & !self.umask, |p| p.mode);
            if let Err(e) = self.apply_extra(&dst, &x, have, cur) {
                self.err(path_msg("set_xattr/set_acl \"", &dst, format!("\" failed: {e}")));
            }
        }
        DirJob {
            src,
            dst,
            rel: Vec::new(),
            root,
            wire_dir: 0,
            depth: 0,
            dst_is_new: pre.is_none(),
            root_dev: Some(src_meta.dev),
            src_meta,
            pre,
            sel: None,
        }
    }

    /// Named top-level entries (`src` without a trailing slash, or single files) synced
    /// into `dst_dir`. Unlike a directory, `dst_dir` itself is neither pruned nor
    /// given the source's attributes.
    #[allow(clippy::too_many_arguments)]
    pub(crate) fn run_items<'s>(
        &'s self,
        s: &Scope<'s>,
        src_dir: &Path,
        dst_dir: &Path,
        root: u16,
        wire_dir: u32,
        entries: Entries,
        dst_is_new: bool,
        sel: Option<&'static Sel>,
    ) {
        let entries: Entries = entries
            .into_iter()
            .filter(|(_, e, _)| !self.filter.is_excluded(e.name.as_bytes(), e.meta.kind == Kind::Dir))
            .collect();
        let mut dst_map = HashMap::new();
        if !dst_is_new {
            for (_, e, dname) in &entries {
                let dname = dname.as_ref().unwrap_or(&e.name);
                if let Ok(dm) = fsops::lstat(&dst_dir.join(dname)) {
                    dst_map.insert(dname.clone(), dm.kind);
                }
            }
        }
        let ctx = Ctx {
            src: src_dir,
            dst: dst_dir,
            dst_handle: None,
            dst_metas: None,
            rel: b"",
            root,
            depth: 0,
            dst_is_new,
            root_dev: None,
            sel,
            wire_dir,
        };
        let (_, files) = self.process_entries(s, &ctx, entries, &dst_map);
        self.transfer(wire_dir, files);
    }

    /// Have changed files written: copied locally, or requested from the remote sender.
    /// Remotely this must run exactly once per listing, even with no files.
    fn transfer(&self, wire_dir: u32, files: Vec<FileJob>) {
        if let Some(r) = &self.remote {
            return r.request(self, wire_dir, files);
        }
        if let Some(p) = &self.pipeline {
            return files.into_iter().for_each(|f| p.queue_file(f));
        }
        match files.len() {
            0 => {}
            1 => self.transfer_file(&files[0]),
            _ => files.par_iter().for_each(|j| self.transfer_file(j)),
        }
    }

    // -----------------------------------------------------------------------
    // Directories
    // -----------------------------------------------------------------------

    /// Read a directory and sync it.
    pub(crate) fn process_dir<'s>(&'s self, s: &Scope<'s>, job: DirJob) {
        if fsops::interrupted() {
            return;
        }
        let src_ents = match job.sel {
            Some(sel) if !sel.contents => self.read_selected(&job.src, sel),
            _ => match fsops::read_dir_meta(&job.src) {
                Ok(v) => self.adjust_listing(&job.src, v),
                Err(e) => {
                    self.err(path_msg("opendir \"", &job.src, format!("\" failed: {e}")));
                    return self.push_fixup(&job, false);
                }
            },
        };
        let entries = src_ents.into_iter().enumerate().map(|(i, e)| (i as u32, e, None)).collect();
        self.process_listing(s, job, entries);
    }

    /// `--scanner=jwalk`: sync a directory with its listing from the scan.
    pub(crate) fn process_scanned<'s>(&'s self, s: &Scope<'s>, job: DirJob, ents: Vec<DirEnt>) {
        if fsops::interrupted() {
            return;
        }
        let ents = self.adjust_listing(&job.src, ents);
        let entries = ents.into_iter().enumerate().map(|(i, e)| (i as u32, e, None)).collect();
        self.process_listing(s, job, entries);
    }

    /// Sync one directory given its source listing.
    pub(crate) fn process_listing<'s>(&'s self, s: &Scope<'s>, job: DirJob, entries: Entries) {
        let files = self.diff_listing(s, &job, entries);
        self.transfer(job.wire_dir, files);
    }

    fn diff_listing<'s>(&'s self, s: &Scope<'s>, job: &DirJob, entries: Entries) -> Vec<FileJob> {
        let entries: Entries = if self.filter.is_empty() {
            entries
        } else {
            entries
                .into_iter()
                .filter(|(_, e, _)| !self.filter.is_excluded(&join_rel(&job.rel, &e.name), e.meta.kind == Kind::Dir))
                .collect()
        };
        let mut dst_metas = None;
        let dst_map = if job.dst_is_new {
            HashMap::new()
        } else if let Some(m) = self.dst_index.as_ref().and_then(|ix| ix.take(&job.dst)) {
            let kinds = m.iter().map(|(n, m)| (n.clone(), m.kind)).collect();
            dst_metas = Some(m);
            kinds
        } else {
            match fsops::read_dir_kinds(&job.dst) {
                Ok(m) => m,
                Err(e) => {
                    self.err(path_msg("opendir \"", &job.dst, format!("\" failed: {e}")));
                    return Vec::new();
                }
            }
        };

        let mut changed = false;
        if self.opts.delete && !job.dst_is_new && select::may_delete(job.sel) {
            changed |= self.delete_extraneous(job, &entries, &dst_map);
        }
        let dst_handle = if job.dst_is_new || dst_metas.is_some() { None } else { fsops::DirHandle::open(&job.dst).ok() };
        let ctx = Ctx {
            src: &job.src,
            dst: &job.dst,
            dst_handle: dst_handle.as_ref(),
            dst_metas: dst_metas.as_ref(),
            rel: &job.rel,
            root: job.root,
            depth: job.depth,
            dst_is_new: job.dst_is_new,
            root_dev: job.root_dev,
            sel: job.sel,
            wire_dir: job.wire_dir,
        };
        let (c, files) = self.process_entries(s, &ctx, entries, &dst_map);
        self.push_fixup(job, changed || c);
        files
    }

    fn delete_extraneous(&self, job: &DirJob, entries: &Entries, dst_map: &HashMap<OsString, Kind>) -> bool {
        let src_names: HashSet<&OsStr> = entries.iter().map(|(_, e, _)| e.name.as_os_str()).collect();
        let mut extra: Vec<(&OsString, Kind)> = dst_map
            .iter()
            .filter(|(n, _)| !src_names.contains(n.as_os_str()))
            .filter(|(n, k)| {
                self.opts.delete_excluded
                    || (!self.filter.is_excluded(&join_rel(&job.rel, n), **k == Kind::Dir) && !self.is_protected_backup(n))
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
                && let Err(e) = self.remove_or_backup(path)
            {
                return self.err(path_msg("delete_file: unlink \"", path, format!("\" failed: {e}")));
            }
        }
        Stats::add(&self.stats.deleted, 1);
    }

    /// A directory sits where a non-directory must go. rsync only removes it when
    /// deletions are enabled or it is empty.
    fn remove_conflicting_dir(&self, path: &Path, rel: &[u8]) -> bool {
        // Several roots: another one's directory stays (rsync keeps the directory too).
        if self.overlap().is_some_and(|o| o.taken(path)) {
            self.err([b"cannot replace directory of another source: ", quoted(rel)].concat());
            return false;
        }
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
        let link = self.opts.keep_dirlinks && !is_new && fsops::lstat(&job.dst).is_ok_and(|m| m.kind == Kind::Symlink);
        let mode = match &job.pre {
            _ if link => None,
            None => Some(self.new_mode(src)),
            Some(p) if self.opts.perms && p.mode != src.mode => Some(src.mode),
            _ => None,
        };
        let mtime = (self.dir_times()
            && (is_new || changed || link || job.pre.is_some_and(|p| !self.rules.same_mtime(src, &p))))
        .then_some((src.mtime, src.mtime_ns));
        let uid = self.rules.wanted_uid(src).filter(|&u| job.pre.is_none_or(|p| p.uid != u));
        let gid = self.rules.wanted_gid(src).filter(|&g| job.pre.is_none_or(|p| p.gid != g));
        if mode.is_some() || mtime.is_some() || uid.is_some() || gid.is_some() {
            self.fixups.lock().unwrap().push(DirFixup {
                depth: job.depth,
                path: job.dst.clone(),
                link,
                mode,
                mtime,
                uid,
                gid,
            });
        }
    }

    /// `-H`: true if this name leads its hard-link group (and is transferred normally);
    /// otherwise it is queued to be linked to the leader at the end.
    fn claim_leader(&self, root: u16, x: &Src, src_path: &Path, dst_path: &Path, rel: &[u8]) -> bool {
        let meta = x.meta;
        let mut groups = self.hardlinks.lock().unwrap();
        match groups.entry((meta.dev, meta.ino)) {
            std::collections::hash_map::Entry::Vacant(v) => {
                v.insert(HardGroup { leader_dst: dst_path.to_path_buf(), leader_rel: rel.to_vec(), followers: Vec::new(), xattr: false });
                true
            }
            std::collections::hash_map::Entry::Occupied(mut o) => {
                let xattr = self.want_xattrs(Kind::File)
                    && self.out.itemizes()
                    && fsops::lstat(dst_path).is_ok_and(|d| d.kind == Kind::File && self.extra_change(x, dst_path, &d).0.0);
                o.get_mut().followers.push(Follower {
                    root,
                    src_path: src_path.to_path_buf(),
                    dst_path: dst_path.to_path_buf(),
                    rel: rel.to_vec(),
                    meta: *meta,
                    xattr,
                });
                false
            }
        }
    }

    /// `-H`: the leader of `meta`'s group gets its attributes updated in place.
    fn leader_xattrs_changed(&self, meta: &Meta) {
        if let Some(g) = self.hardlinks.lock().unwrap().get_mut(&(meta.dev, meta.ino)) {
            g.xattr = true;
        }
    }

    /// `-H`: link every follower to its leader's destination (after all file data is
    /// written, before directory attributes are fixed). Already-linked names are left alone.
    pub fn link_followers(&self) {
        let groups = std::mem::take(&mut *self.hardlinks.lock().unwrap());
        let mut groups: Vec<HardGroup> = groups.into_values().filter(|g| !g.followers.is_empty()).collect();
        groups.sort_by(|a, b| a.leader_rel.cmp(&b.leader_rel));
        for mut g in groups {
            g.followers.sort_by(|a, b| a.rel.cmp(&b.rel));
            let leader = fsops::lstat(&g.leader_dst).ok().filter(|m| m.kind == Kind::File);
            for f in g.followers {
                if fsops::interrupted() {
                    return;
                }
                let dst = fsops::lstat(&f.dst_path).ok();
                if let (Some(l), Some(d)) = (leader, dst)
                    && (l.dev, l.ino) == (d.dev, d.ino)
                {
                    // Already linked: rsync shows `hf` without naming the leader.
                    let change = Change { hardlink: true, xattr: f.xattr || g.xattr, ..Change::default() };
                    if change.xattr || self.opts.itemize >= 2 {
                        self.out.item(&f.rel, Kind::File, Link::None, change, false);
                    }
                    self.source_done(f.root, &f.src_path, &f.dst_path, &f.rel, &f.meta);
                    continue;
                }
                if leader.is_none() && !self.dry() {
                    // The leader wasn't written (an error, already reported): send this name on its own.
                    if self.remote.is_none() {
                        let dst_meta = dst.filter(|d| d.kind == Kind::File);
                        let job = FileJob {
                            idx: 0,
                            src_path: f.src_path,
                            dst_path: f.dst_path,
                            rel: f.rel,
                            meta: f.meta,
                            dst_meta,
                            root: f.root,
                            basis: None,
                            alt: None,
                            partial: None,
                            extra: None,
                            full_xattrs: None,
                        };
                        self.transfer_file(&job);
                    } else {
                        self.err([&b"hard link target missing for "[..], &f.rel].concat());
                    }
                    continue;
                }
                let change = match dst {
                    Some(d) if d.kind == Kind::File => Change {
                        hardlink: true,
                        size: d.size != f.meta.size,
                        ..self.attr_change(&f.meta, &d, false)
                    },
                    _ => Change { hardlink: true, new: true, ..Change::default() },
                };
                self.out.item(&f.rel, Kind::File, Link::Hard(&g.leader_rel), change, true);
                if dst.is_none() {
                    Stats::add(&self.stats.created, 1);
                }
                if self.dry() {
                    continue;
                }
                if dst.is_some_and(|d| d.kind == Kind::Dir) && !self.remove_conflicting_dir(&f.dst_path, &f.rel) {
                    continue;
                }
                if !self.backup_existing(&f.src_path, &f.dst_path, false) {
                    continue;
                }
                let (dir, name) = split_dst(&f.dst_path);
                let r = fsops::with_temp(dir, name, |tmp| fs::hard_link(&g.leader_dst, tmp))
                    .and_then(|(tmp, ())| finish_temp(&tmp, fs::rename(&tmp, &f.dst_path)));
                match r {
                    Ok(()) => self.source_done(f.root, &f.src_path, &f.dst_path, &f.rel, &f.meta),
                    Err(e) => self.err(path_msg("link \"", &f.dst_path, format!("\" failed: {e}"))),
                }
            }
        }
    }

    /// Apply directory attributes, deepest level first; directories on one level are
    /// independent so each level is applied in parallel.
    pub fn apply_fixups(&self) {
        let mut fixups = std::mem::take(&mut *self.fixups.lock().unwrap());
        fixups.sort_unstable_by_key(|f| std::cmp::Reverse(f.depth));
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
                let set = if f.link { fsops::set_symlink_mtime } else { fsops::set_mtime };
                if let Some((s, ns)) = f.mtime
                    && let Err(e) = set(&f.path, s, ns)
                {
                    self.err(path_msg("failed to set times on \"", &f.path, format!("\": {e}")));
                }
            });
        }
    }

    // -----------------------------------------------------------------------
    // Entries
    // -----------------------------------------------------------------------

    /// Handle the children of one directory. Returns whether the destination directory
    /// itself was modified, and the files whose data must be written.
    fn process_entries<'s>(
        &'s self,
        s: &Scope<'s>,
        ctx: &Ctx,
        entries: Entries,
        dst_map: &HashMap<OsString, Kind>,
    ) -> (bool, Vec<FileJob>) {
        let mut changed = false;
        let mut files: Vec<FileJob> = Vec::new();
        let same_sums = if self.opts.checksum { self.checksum_matches(ctx, &entries, dst_map) } else { HashSet::new() };
        for (idx, e, dname_override) in entries {
            if fsops::interrupted() {
                break;
            }
            let dname: &OsStr = dname_override.as_deref().unwrap_or(&e.name);
            let rel = join_rel(ctx.rel, &e.name);
            let src_path = ctx.src.join(&e.name);
            let dst_path = ctx.dst.join(dname);
            let dst_kind = if ctx.dst_is_new { None } else { dst_map.get(dname).copied() };
            if self.opts.existing && dst_kind.is_none() && !(e.meta.kind == Kind::Dir && !self.opts.recursive && !self.opts.dirs) {
                // --existing: nothing new is created, not even directories.
                continue;
            }
            match e.meta.kind {
                Kind::Dir => changed |= self.handle_dir(s, ctx, e.meta, e.extra.as_deref(), idx, rel, src_path, dst_path, dst_kind),
                Kind::File => {
                    Stats::add(&self.stats.files, 1);
                    Stats::add(&self.stats.total_size, e.meta.size);
                    if self.size_filtered(&e.meta, &rel) {
                        continue;
                    }
                    let leads = || {
                        let x = Src { kind: Kind::File, meta: &e.meta, extra: e.extra.as_deref(), from: XSource::Full(&[]) };
                        self.claim_leader(ctx.root, &x, &src_path, &dst_path, &rel)
                    };
                    if self.opts.hard_links && e.meta.nlink > 1 && !leads() {
                        // Linked at the end, which modifies this directory.
                        changed = true;
                        continue;
                    }
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
                    let from = self.xsource(ctx, idx, &src_path, &rel, &dst_path);
                    let src = Src { kind: Kind::File, meta: &e.meta, extra: e.extra.as_deref(), from };
                    let mut have: Option<DstExtra> = None;
                    let action = file_action_ext(&e.meta, dst_meta.as_ref(), &self.rules, same_sums.contains(&idx), &mut || {
                        let Some(d) = &dst_meta else { return (false, false) };
                        let (diff, h) = self.extra_change(&src, &dst_path, d);
                        have = h;
                        diff
                    });
                    let mut alt = None;
                    if !self.alt_dirs.is_empty()
                        && action == FileAction::Transfer
                        && dst_meta.is_none_or(|d| d.kind == Kind::File)
                    {
                        let mut src_sum = || self.src_sum(ctx.wire_dir, idx, &src_path);
                        match self.try_alt_file(ctx.root, &src, &rel, &src_path, &dst_path, dst_meta.as_ref(), &mut src_sum) {
                            super::flags::AltFile::Done => {
                                changed = true;
                                continue;
                            }
                            super::flags::AltFile::Basis(p, m) => alt = Some((p, m)),
                            super::flags::AltFile::None => {}
                        }
                    }
                    match action {
                        FileAction::Skip => {
                            self.out.item(&rel, Kind::File, Link::None, Change::default(), false);
                            self.source_done(ctx.root, &src_path, &dst_path, &rel, &e.meta);
                        }
                        FileAction::Attrs(a) => {
                            if a.xattr && self.opts.hard_links && e.meta.nlink > 1 {
                                self.leader_xattrs_changed(&e.meta);
                            }
                            if let Some(d) = &dst_meta {
                                let mut change = Change { xattr: a.xattr, acl: a.acl, ..self.attr_change(&e.meta, d, false) };
                                change.perms = self.perms_shown(&e.meta, d, change.perms, a.acl);
                                self.out.item(&rel, Kind::File, Link::None, change, false);
                            }
                            if !self.dry() {
                                match &self.pipeline {
                                    Some(p) => p.queue_attrs(super::pipeline::AttrJob {
                                        root: ctx.root,
                                        src_path,
                                        dst_path,
                                        rel,
                                        meta: e.meta,
                                        diff: a,
                                        extra: e.extra,
                                        have,
                                    }),
                                    None => {
                                        if self.fix_attrs(&dst_path, &src, a, have) {
                                            self.source_done(ctx.root, &src_path, &dst_path, &rel, &e.meta);
                                        }
                                    }
                                }
                            }
                        }
                        FileAction::Transfer => {
                            changed = true;
                            let (basis, alt) = match alt {
                                Some((p, m)) if dst_meta.is_none() => (Some(p), Some(m)),
                                _ => (None, None),
                            };
                            let mut job = FileJob {
                                idx,
                                src_path,
                                dst_path,
                                rel,
                                meta: e.meta,
                                dst_meta,
                                root: ctx.root,
                                basis,
                                alt,
                                partial: None,
                                extra: e.extra,
                                full_xattrs: None,
                            };
                            self.find_partial(&mut job);
                            files.push(job);
                        }
                    }
                }
                Kind::Symlink => {
                    Stats::add(&self.stats.links, 1);
                    if !self.opts.links {
                        self.out.info([b"skipping non-regular file \"", quoted(&rel), b"\""].concat());
                        continue;
                    }
                    changed |= self.handle_symlink(ctx.root, &e, self.xsource(ctx, idx, &src_path, &rel, &dst_path), &rel, &src_path, &dst_path, dst_kind);
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
                    let src = Src { kind, meta: &e.meta, extra: e.extra.as_deref(), from: self.xsource(ctx, idx, &src_path, &rel, &dst_path) };
                    changed |= self.handle_special(ctx.root, &src, &rel, &src_path, &dst_path, dst_kind);
                }
            }
        }
        (changed && !self.dry(), files)
    }

    /// The source file's `-c` checksum: sent by the remote sender, or computed here.
    fn src_sum(&self, wire_dir: u32, idx: u32, src_path: &Path) -> Option<Sum> {
        match &self.remote {
            Some(r) => r.src_sum(wire_dir, idx),
            None => checksum::file_sum(src_path).ok(),
        }
    }

    /// `-c`: which files of this listing have a same-size destination file with the
    /// same checksum. All needed files are hashed in parallel.
    fn checksum_matches(&self, ctx: &Ctx, entries: &Entries, dst_map: &HashMap<OsString, Kind>) -> HashSet<u32> {
        if ctx.dst_is_new {
            return HashSet::new();
        }
        let mut cands = Vec::new();
        for (idx, e, dname) in entries {
            let dname = dname.as_deref().unwrap_or(&e.name);
            if e.meta.kind != Kind::File || dst_map.get(dname) != Some(&Kind::File) {
                continue;
            }
            let dst_path = ctx.dst.join(dname);
            if ctx.lstat_dst(dname, &dst_path).is_ok_and(|d| d.kind == Kind::File && d.size == e.meta.size) {
                cands.push((*idx, ctx.src.join(&e.name), dst_path));
            }
        }
        if cands.is_empty() {
            return HashSet::new();
        }
        let local = self.remote.is_none();
        let mut paths = Vec::with_capacity(cands.len() * 2);
        for (_, src, dst) in &cands {
            paths.push(dst.clone());
            if local {
                paths.push(src.clone());
            }
        }
        let mut sums = checksum::sums(paths).into_iter();
        let mut same = HashSet::new();
        for (idx, _, _) in &cands {
            let dst = sums.next().and_then(Result::ok);
            let src = if local { sums.next().and_then(Result::ok) } else { self.src_sum(ctx.wire_dir, *idx, Path::new("")) };
            if dst.is_some() && dst == src {
                same.insert(*idx);
            }
        }
        same
    }

    #[allow(clippy::too_many_arguments)]
    fn handle_dir<'s>(
        &'s self,
        s: &Scope<'s>,
        ctx: &Ctx,
        meta: Meta,
        extra: Option<&Extra>,
        idx: u32,
        rel: Vec<u8>,
        src_path: PathBuf,
        dst_path: PathBuf,
        dst_kind: Option<Kind>,
    ) -> bool {
        Stats::add(&self.stats.dirs, 1);
        let descend = select::descend(ctx.sel, rel.rsplit(|&b| b == b'/').next().unwrap_or(&rel), self.opts.recursive);
        if descend == Descend::No && !self.opts.dirs {
            self.out.info([b"skipping directory ", quoted(&rel)].concat());
            return false;
        }
        // -K: a symlink to a directory on the receiver stands in for the directory (not
        // one this transfer made, see `Overlap`).
        let kept_link = if self.opts.keep_dirlinks
            && dst_kind == Some(Kind::Symlink)
            && !self.overlap().is_some_and(|o| o.made_link(&dst_path))
        {
            fsops::stat(&dst_path).ok().filter(|m| m.kind == Kind::Dir)
        } else {
            None
        };
        let mut changed = false;
        let mut pre = None;
        match dst_kind {
            _ if kept_link.is_some() => pre = kept_link,
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
                    && let Err(e) = self.remove_or_backup(&dst_path)
                {
                    self.err(path_msg("unlink \"", &dst_path, format!("\" failed: {e}")));
                    return false;
                }
                changed = true;
            }
            None => {}
        }
        let is_new = pre.is_none();
        let x = Src { kind: Kind::Dir, meta: &meta, extra, from: self.xsource(ctx, idx, &src_path, &rel, &dst_path) };
        let (ext, have) = match &pre {
            Some(p) if kept_link.is_none() => self.extra_change(&x, &dst_path, p),
            _ => ((false, false), None),
        };
        self.report_dir(&rel, &meta, pre.as_ref(), ext);
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
        // Several roots: from here on this stays the directory its entries go into.
        if let Some(o) = self.overlap()
            && let Err(e) = o.take_dir(&dst_path, kept_link.is_some())
        {
            self.err(path_msg("mkdir \"", &dst_path, format!("\" failed: {e}")));
            return changed;
        }
        // Like rsync, a directory gets its attributes and ACLs right away; the ACL keeps
        // the current (owner-writable) mode bits, the mode itself is fixed up last.
        if !self.dry() && self.extras_on() && kept_link.is_none() && (is_new || ext.0 || ext.1) {
            let cur = pre.map_or(0o700 | self.new_mode(&meta), |p| p.mode);
            if let Err(e) = self.apply_extra(&dst_path, &x, have, cur) {
                self.err(path_msg("set_xattr/set_acl \"", &dst_path, format!("\" failed: {e}")));
            }
        }
        let root_dev = ctx.root_dev.unwrap_or(meta.dev);
        let job = DirJob {
            src: src_path,
            dst: dst_path,
            rel,
            root: ctx.root,
            wire_dir: 0,
            depth: ctx.depth + 1,
            dst_is_new: is_new,
            root_dev: Some(root_dev),
            src_meta: meta,
            pre,
            sel: match descend {
                Descend::Some(sel) => Some(sel),
                _ => None,
            },
        };
        if descend == Descend::No {
            // -d: the directory itself, without its contents.
            self.push_fixup(&job, false);
        } else if let Some(r) = &self.remote {
            // The sender applies -x itself; the listing arrives as its own batch.
            r.expect_dir(s, self, job);
        } else if self.opts.one_file_system && meta.dev != root_dev {
            self.push_fixup(&job, false);
        } else if let Some(p) = &self.pipeline {
            p.expect_dir(s, self, job);
        } else {
            s.spawn(move |s| self.process_dir(s, job));
        }
        changed
    }

    #[allow(clippy::too_many_arguments)]
    fn handle_symlink(
        &self,
        root: u16,
        e: &DirEnt,
        from: XSource,
        rel: &[u8],
        src_path: &Path,
        dst_path: &Path,
        dst_kind: Option<Kind>,
    ) -> bool {
        let meta = &e.meta;
        let x = Src { kind: Kind::Symlink, meta, extra: e.extra.as_deref(), from };
        let Some(target) = e.link.as_deref().map(|t| PathBuf::from(OsStr::from_bytes(t))) else {
            self.err(path_msg("readlink \"", src_path, "\" failed"));
            return false;
        };
        match dst_kind {
            Some(Kind::Symlink) => {
                if fs::read_link(dst_path).is_ok_and(|cur| cur == target) {
                    let d = fsops::lstat(dst_path).ok();
                    let ((xattr, _), have) = match &d {
                        Some(d) => self.extra_change(&x, dst_path, d),
                        None => ((false, false), None),
                    };
                    let change = d.map(|d| self.attr_change(meta, &d, false)).unwrap_or_default();
                    // Symlink permissions aren't synced.
                    let change = Change { perms: false, xattr, ..change };
                    self.out.item(rel, Kind::Symlink, Link::Sym(target.as_os_str().as_bytes()), change, false);
                    if !self.dry()
                        && let Some(d) = d
                    {
                        let a = self.rules.attrs_differ(meta, &d);
                        if a.owner {
                            let _ = fsops::lchown(dst_path, self.rules.wanted_uid(meta), self.rules.wanted_gid(meta));
                        }
                        if xattr && let Err(e) = self.apply_extra(dst_path, &x, have, d.mode) {
                            self.err(path_msg("set_xattr \"", dst_path, format!("\" failed: {e}")));
                        }
                        if a.mtime {
                            let _ = fsops::set_symlink_mtime(dst_path, meta.mtime, meta.mtime_ns);
                        }
                    }
                    self.source_done(root, src_path, dst_path, rel, meta);
                    return false;
                }
            }
            Some(Kind::Dir) if !self.remove_conflicting_dir(dst_path, rel) => return false,
            None if self.alt_symlink(rel, target.as_os_str().as_bytes(), dst_path) => {
                self.source_done(root, src_path, dst_path, rel, meta);
                return true;
            }
            _ => {}
        }
        let change = match dst_kind {
            // The target changed: rsync shows `cLc` plus the attributes that differ.
            Some(Kind::Symlink) => match fsops::lstat(dst_path) {
                Ok(d) => {
                    let xattr = self.out.itemizes() && self.extra_change(&x, dst_path, &d).0.0;
                    Change { local: true, value: true, perms: false, xattr, ..self.attr_change(meta, &d, false) }
                }
                Err(_) => Change::new_local(),
            },
            _ => Change::new_local(),
        };
        self.out.item(rel, Kind::Symlink, Link::Sym(target.as_os_str().as_bytes()), change, true);
        if dst_kind.is_none() {
            Stats::add(&self.stats.created, 1);
        }
        if self.dry() {
            return true;
        }
        if dst_kind.is_some_and(|k| k != Kind::Dir) && !self.backup_existing(src_path, dst_path, false) {
            return false;
        }
        let (dir, name) = split_dst(dst_path);
        let res = fsops::with_temp(dir, name, |tmp| std::os::unix::fs::symlink(&target, tmp)).and_then(|(tmp, ())| {
            let r = (|| {
                fsops::lchown(&tmp, self.rules.wanted_uid(meta), self.rules.wanted_gid(meta))?;
                if self.opts.times {
                    fsops::set_symlink_mtime(&tmp, meta.mtime, meta.mtime_ns)?;
                }
                self.place_link(&tmp, dst_path)
            })();
            finish_temp(&tmp, r)
        });
        match res {
            Err(e) => self.err(path_msg("symlink \"", dst_path, format!("\" failed: {e}"))),
            Ok(()) => {
                if self.want_xattrs(Kind::Symlink)
                    && let Err(e) = self.apply_extra(dst_path, &x, None, 0o777)
                {
                    self.err(path_msg("set_xattr \"", dst_path, format!("\" failed: {e}")));
                }
                self.source_done(root, src_path, dst_path, rel, meta)
            }
        }
        true
    }

    fn handle_special(&self, root: u16, x: &Src, rel: &[u8], src_path: &Path, dst_path: &Path, dst_kind: Option<Kind>) -> bool {
        let meta = x.meta;
        match dst_kind {
            Some(k) if k == meta.kind => {
                if let Ok(d) = fsops::lstat(dst_path)
                    && d.rdev == meta.rdev
                {
                    let ((xattr, acl), have) = self.extra_change(x, dst_path, &d);
                    let mut change = Change { xattr, acl, ..self.attr_change(meta, &d, false) };
                    change.perms = self.perms_shown(meta, &d, change.perms, acl);
                    self.out.item(rel, meta.kind, Link::None, change, false);
                    let a = AttrDiff { xattr, acl, ..self.rules.attrs_differ(meta, &d) };
                    if !a.any() || self.dry() || self.fix_attrs(dst_path, x, a, have) {
                        self.source_done(root, src_path, dst_path, rel, meta);
                    }
                    return false;
                }
            }
            Some(Kind::Dir) if !self.remove_conflicting_dir(dst_path, rel) => return false,
            _ => {}
        }
        self.out.item(rel, meta.kind, Link::None, Change::new_local(), true);
        Stats::add(&self.stats.created, 1);
        if self.dry() {
            return true;
        }
        if dst_kind.is_some_and(|k| k != Kind::Dir) && !self.backup_existing(src_path, dst_path, false) {
            return false;
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
        if res.is_ok() {
            if self.extras_on()
                && let Err(e) = self.apply_extra(dst_path, x, None, self.new_mode(meta))
            {
                self.err(path_msg("set_xattr/set_acl \"", dst_path, format!("\" failed: {e}")));
            }
            self.source_done(root, src_path, dst_path, rel, meta);
        }
        if let Err(e) = res {
            self.err(path_msg("mknod \"", dst_path, format!("\" failed: {e}")));
        }
        true
    }

    /// Returns whether the attributes were set.
    /// Queue A: a metadata-only fix.
    pub(crate) fn attr_job(&self, j: super::pipeline::AttrJob) {
        let x = Src { kind: Kind::File, meta: &j.meta, extra: j.extra.as_deref(), from: XSource::Local(&j.src_path) };
        if self.fix_attrs(&j.dst_path, &x, j.diff, j.have) {
            self.source_done(j.root, &j.src_path, &j.dst_path, &j.rel, &j.meta);
        }
    }

    /// Owner, `-X`/`-A`, mode and time, in that order (as rsync's `set_file_attrs`).
    fn fix_attrs(&self, path: &Path, x: &Src, a: AttrDiff, have: Option<DstExtra>) -> bool {
        let src = x.meta;
        let r = (|| {
            if a.owner {
                fsops::lchown(path, self.rules.wanted_uid(src), self.rules.wanted_gid(src))?;
            }
            if a.xattr || a.acl {
                self.fix_extra(path, x, have)?;
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
            return false;
        }
        true
    }

    // -----------------------------------------------------------------------
    // File data
    // -----------------------------------------------------------------------

    /// Report a file that is about to be (re)written: the item line and the counters.
    pub(crate) fn note_transfer(&self, job: &FileJob) {
        // With -c, rsync marks a rewritten file's checksum as changed (`>fc`).
        let change = match job.dst_meta.filter(|d| d.kind == Kind::File).or(job.alt) {
            Some(d) => {
                // -X/-A differences of the old file only matter for the item line.
                let (xattr, acl) = if self.out.itemizes() && self.extras_on() {
                    self.extra_change(&job.src(true), job.basis_path(), &d).0
                } else {
                    (false, false)
                };
                Change {
                    transfer: true,
                    value: self.opts.checksum,
                    size: d.size != job.meta.size,
                    xattr,
                    acl,
                    ..self.attr_change(&job.meta, &d, true)
                }
            }
            None => Change { new: true, transfer: true, ..Change::default() },
        };
        self.out.item(&job.rel, Kind::File, Link::None, change, true);
        crate::progress::planned(job.meta.size);
        Stats::add(&self.stats.transferred, 1);
        Stats::add(&self.stats.transferred_bytes, job.meta.size);
        if job.dst_meta.is_none() {
            Stats::add(&self.stats.created, 1);
        }
    }

    /// Mode, owner and group the written file must end up with.
    pub(crate) fn target_attrs(&self, job: &FileJob) -> (u32, Option<u32>, Option<u32>) {
        let src = &job.meta;
        let mode = match (&job.dst_meta, self.opts.perms) {
            (Some(d), false) if d.kind == Kind::File => d.mode,
            _ => self.new_mode(src),
        };
        (mode, self.rules.wanted_uid(src), self.rules.wanted_gid(src))
    }

    pub(crate) fn transfer_file(&self, job: &FileJob) {
        if fsops::interrupted() {
            return;
        }
        self.note_transfer(job);
        if self.dry() {
            crate::progress::done(job.meta.size);
            return;
        }
        if !self.backup_existing_file(job) {
            crate::progress::done(job.meta.size);
            return;
        }
        // A big copy moves the progress line as it goes, not only once it is done.
        let (r, counted) = crate::progress::tracking(|| self.write_file(job));
        crate::progress::done(job.meta.size.saturating_sub(counted));
        match r {
            Ok(()) => self.file_written(job),
            Err(e) if !fsops::interrupted() => {
                let to = path_msg("\" -> \"", &job.dst_path, format!("\" failed: {e}"));
                self.err(path_msg("copy \"", &job.src_path, to));
            }
            Err(_) => {}
        }
    }

    fn write_file(&self, job: &FileJob) -> io::Result<()> {
        let src = &job.meta;
        let (mode, uid, gid) = self.target_attrs(job);

        if self.delta_candidate(job) {
            match self.write_delta(job, mode, uid, gid) {
                Ok(true) => return Ok(()),
                Ok(false) => {}
                Err(e) if fsops::interrupted() => return Err(e),
                // E.g. a file changed size while it was being hashed: send it whole.
                Err(_) => {}
            }
        }
        // All of a file sent whole is literal data, as rsync counts it.
        Stats::add(&self.stats.literal_bytes, src.size);

        if self.opts.inplace && job.dst_meta.is_some_and(|d| d.kind == Kind::File) {
            fsops::copy_into_existing(&job.src_path, &job.dst_path, src.size)?;
            fsops::lchown(&job.dst_path, uid, gid)?;
            self.written_extra(&job.dst_path, job, mode)?;
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
            let r = self
                .copy_new(&job.src_path, &job.dst_path, src.size, allow_fast, mode)
                .and_then(|copied| self.apply_new_file_attrs(&job.dst_path, copied, mode, uid, gid, mtime, Some(&job.src(false))))
                .and_then(|()| fsops::check_interrupt());
            return self.finish_or_keep(&job.dst_path, job, r);
        }
        let (dir, name) = split_dst(&job.dst_path);
        let (tmp, copied) = fsops::with_temp(dir, name, |tmp| {
            // An interrupted copy is kept with --partial before the temp is discarded.
            self.copy_new(&job.src_path, tmp, src.size, allow_fast, mode).inspect_err(|_| {
                if fsops::interrupted() {
                    self.keep_partial(tmp, &job.dst_path);
                }
            })
        })?;
        let r = self
            .apply_new_file_attrs(&tmp, copied, mode, uid, gid, mtime, Some(&job.src(false)))
            .and_then(|()| fsops::check_interrupt())
            .and_then(|()| fs::rename(&tmp, &job.dst_path));
        self.finish_or_keep(&tmp, job, r)
    }

    /// Delta transfer is opt-in locally (`--no-whole-file`), like rsync, and needs an
    /// existing destination file to use as the basis.
    fn delta_candidate(&self, job: &FileJob) -> bool {
        self.opts.whole_file == Some(false)
            && delta::worth_delta(job.meta.size)
            && (job.basis.is_some() || job.dst_meta.is_some_and(|d| d.kind == Kind::File && d.size > 0))
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
        let basis = fs::File::open(job.basis_path())?;
        let basis_size = basis.metadata()?.len();
        let (maps, reads);
        let (d, new_r, basis_r): (delta::Delta, &dyn delta::ReadAt, &dyn delta::ReadAt) = if self.opts.delta == DeltaMode::Cdc {
            maps = match (cdc::Mapped::new(&new, job.meta.size), cdc::Mapped::new(&basis, basis_size)) {
                (Ok(n), Ok(b)) => (n, b),
                // Changed since it was listed: copy it whole instead.
                (Err(e), _) | (_, Err(e)) if cdc::is_changed(&e) => return Ok(false),
                (Err(e), _) | (_, Err(e)) => return Err(e),
            };
            (cdc::local_delta(maps.0.bytes(), maps.1.bytes(), &delta::Params::DEFAULT), &maps.0, &maps.1)
        } else {
            reads = (delta::ReadOnce::new(&new, job.meta.size), delta::ReadOnce::new(&basis, basis_size));
            (delta::local_delta(&reads.0, job.meta.size, &reads.1, basis_size, &delta::Params::DEFAULT)?, &reads.0, &reads.1)
        };
        if d.matched_bytes() == 0 {
            return Ok(false);
        }
        Stats::add(&self.stats.delta_files, 1);
        Stats::add(&self.stats.literal_bytes, d.literal_bytes());
        Stats::add(&self.stats.matched_bytes, d.matched_bytes());

        if self.opts.inplace && d.is_in_place() && job.basis.is_none() {
            let out = fs::OpenOptions::new().write(true).open(&job.dst_path)?;
            apply(&d, basis_r, new_r, &out, true)?;
            self.finish_delta_file(&out, &job.meta, mode, uid, gid, Some((&job.dst_path, &job.src(false))))?;
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
            self.cloned(&tmp);
        }
        let r = (|| {
            let out = fs::OpenOptions::new().write(true).open(&tmp)?;
            apply(&d, basis_r, new_r, &out, cloned)?;
            self.finish_delta_file(&out, &job.meta, mode, uid, gid, Some((&tmp, &job.src(false))))?;
            drop(out);
            fsops::check_interrupt()?;
            fs::rename(&tmp, &job.dst_path)
        })();
        finish_temp(&tmp, r).map(|()| true)
    }

    /// Attributes for a delta-built file. It may be a clone of the old file, which
    /// carries the old mode and mtime, so both are always set. `ext`: the file's path
    /// and source, for `-X`/`-A`.
    #[allow(clippy::too_many_arguments)]
    pub(crate) fn finish_delta_file(
        &self,
        f: &fs::File,
        src: &Meta,
        mode: u32,
        uid: Option<u32>,
        gid: Option<u32>,
        ext: Option<(&Path, &Src)>,
    ) -> io::Result<()> {
        fsops::fchown(f, uid, gid)?;
        if let Some((path, x)) = ext {
            self.apply_extra(path, x, None, mode)?;
        }
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
    /// Owner, mode and time of a file this run created and wrote, through its descriptor.
    /// `ext`: the file's path and source, for `-X`/`-A`.
    #[allow(clippy::too_many_arguments)]
    pub(crate) fn set_written_attrs(
        &self,
        f: &fs::File,
        mode: u32,
        uid: Option<u32>,
        gid: Option<u32>,
        mtime: Option<(i64, u32)>,
        ext: Option<(&Path, &Src)>,
    ) -> io::Result<()> {
        fsops::fchown(f, uid, gid)?;
        if let Some((path, x)) = ext {
            self.apply_extra(path, x, None, mode)?;
        }
        // open() already applied `mode & !umask`; only fix up what it couldn't set:
        // bits masked by the umask, and setuid/setgid/sticky (which also covers the
        // bits chown clears). With -A a parent's default ACL may have masked the mode
        // instead of the umask.
        if mode & !self.umask & 0o777 != mode || self.opts.acls {
            fsops::fchmod(f, mode)?;
        }
        if let Some((s, ns)) = mtime {
            fsops::set_fd_mtime(f, s, ns)?;
        }
        if self.opts.fsync {
            f.sync_all()?;
        }
        Ok(())
    }

    /// `src`: the source, for `-X`/`-A`.
    #[allow(clippy::too_many_arguments)]
    pub(crate) fn apply_new_file_attrs(
        &self,
        path: &Path,
        copied: Copied,
        mode: u32,
        uid: Option<u32>,
        gid: Option<u32>,
        mtime: Option<(i64, u32)>,
        src: Option<&Src>,
    ) -> io::Result<()> {
        match copied {
            Copied::Written(f) => self.set_written_attrs(&f, mode, uid, gid, mtime, src.map(|x| (path, x))),
            Copied::Clone => {
                Stats::add(&self.stats.cloned, 1);
                self.cloned(path);
                fsops::lchown(path, uid, gid)?;
                if let Some(x) = src {
                    self.apply_extra(path, x, None, mode)?;
                }
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
        if let Some(m) = self.dst_metas {
            return m.get(name).copied().ok_or_else(|| io::ErrorKind::NotFound.into());
        }
        match self.dst_handle {
            Some(h) => h.lstat(name),
            None => fsops::lstat(path),
        }
    }
}

pub(crate) fn split_dst(path: &Path) -> (&Path, &OsStr) {
    let dir = path.parent().filter(|p| !p.as_os_str().is_empty()).unwrap_or(Path::new("."));
    (dir, path.file_name().unwrap_or(OsStr::new("")))
}

/// Unregister a temp file that was renamed into place, or delete it on failure.
pub(crate) fn finish_temp(tmp: &Path, r: io::Result<()>) -> io::Result<()> {
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

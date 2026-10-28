//! The sync engine (receiving side): one rayon task per directory.
//!
//! Each directory task takes the source directory's listing, compares it with the
//! matching destination directory, queues extraneous entries (`--delete`), creates
//! subdirectories and descends into them, then has its changed files written.
//! Queued deletions run only after the source scan succeeds and every entry that was
//! to be written reached the destination. Directory attributes are applied after
//! everything else, deepest first, so a read-only source directory stays writable
//! while it is being filled.
//!
//! Locally the listing comes from `readdir` and files are copied in parallel. As a
//! remote receiver (see `remote::receiver`), listings arrive as `DirBatch` frames,
//! subdirectories wait for their batch, and changed files are requested from the sender.
//! With `--scanner=jwalk` (see `local::pipeline`), listings come from a parallel tree
//! scan, destination entries from a pre-scanned index, and files go through queues.

use super::Root;
use super::merge::{self, SrcDir, Sources};
use crate::cli::Options;
use crate::delta::{self, DeltaMode, apply::apply, cdc};
use super::extra::{DstExtra, Src, XSource};
use crate::diff::{AttrDiff, DiffRules, FileAction, file_action_ext};
use crate::entry::{DirEnt, Extra, Kind, Meta};
use crate::filter::{DirRules, Filter, Side};
use crate::fsops::{self, Copied};
use crate::output::{Change, Link, Output, Stats, path_msg};
use crate::paths::join_rel;
use crate::checksum::{self, Sum};
use crate::select::{self, Descend, Sel};
use rayon::Scope;
use rayon::prelude::*;
use std::cmp::Ordering;
use std::collections::{HashMap, HashSet};
use std::ffi::{OsStr, OsString};
use std::fs;
use std::io;
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::DirBuilderExt;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering::SeqCst};
use std::sync::{Arc, Mutex};

pub struct Engine<'a> {
    pub opts: &'a Options,
    pub rules: DiffRules,
    pub filter: Filter,
    pub out: &'a Output,
    pub stats: Stats,
    fixups: Mutex<Vec<DirFixup>>,
    /// Entries absent from a listing; removal waits for the complete source scan.
    deletions: Mutex<HashMap<PathBuf, Deletion>>,
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
    /// The source side could not be read completely (rsync's `io_error`): its listings
    /// may lack entries, so nothing is deleted from now on. Also set when an entry the
    /// source has could not be written to the destination.
    io_error: AtomicBool,
    /// "IO error encountered -- skipping file deletion" was shown.
    io_warned: AtomicBool,
    /// `-m`: which source directories have content (local transfers).
    pub(crate) prune: super::prune::Prune,
}

/// Per-directory filter state (see [`DirRules`]): the source side's decides what is sent
/// (local transfers: a remote sender checks its own), the destination side's what
/// `--delete` leaves alone.
#[derive(Debug, Clone, Default)]
pub(crate) struct Rules {
    pub(crate) src: DirRules,
    pub(crate) dst: DirRules,
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
    /// `-x`: the device of the top destination directory above (rsync's: the root, a
    /// named directory, the end of a `-R` path). Nothing is deleted in a directory on
    /// another one.
    dst_dev: Option<u64>,
    src_meta: Meta,
    /// Destination directory metadata before we touched it (`None` when new).
    pre: Option<Meta>,
    /// `-R`/`--files-from`: only these entries are transferred (`None`: all of them).
    pub(crate) sel: Option<&'static Sel>,
    /// Same-named directories of later sources merged into this one (several SRC
    /// arguments land in one destination, see [`merge`]).
    pub(crate) more: Vec<SrcDir>,
    /// Filter state of the parent directory, which this one's own starts from.
    pub(crate) rules: Rules,
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
    /// See [`DirJob::dst_dev`] (`None` for named items: each directory is a top one).
    dst_dev: Option<u64>,
    sel: Option<&'static Sel>,
    /// Listing id on the wire (remote: finds the sender's `-c` checksums).
    wire_dir: u32,
    /// A listing merged from several source directories: where each entry comes from.
    sources: Option<&'p Sources>,
    /// This directory's own filter state (the source side's: when not merged).
    rules: &'p Rules,
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
    /// Destination-side filter state of its directory.
    rules: DirRules,
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

struct Deletion {
    rel: Vec<u8>,
    meta: Meta,
    /// Destination-side filter state of the directory it is in.
    rules: DirRules,
}

/// How deleting an entry went (rsync's `enum delret`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Del {
    Done,
    /// Something in it stays: protected by a rule, or not deleted.
    NotEmpty,
    /// `--max-delete` was reached.
    AtLimit,
    Failed,
}

/// A name for a message: the root shows as `.`.
fn quoted(rel: &[u8]) -> &[u8] {
    if rel.is_empty() { b"." } else { rel }
}

/// Whether `path` is on another filesystem than the directory it is in (a mount point),
/// or can't be told apart from one.
fn mount_point(path: &Path) -> bool {
    match (fsops::lstat(path), fsops::stat(split_dst(path).0)) {
        (Ok(m), Ok(dir)) => m.dev != dir.dev,
        _ => true,
    }
}

/// rsync's file-list order for two names in one directory (each with whether it is a
/// directory), reversed, which is the order it deletes them in: subdirectories first,
/// then the rest, each by descending name.
fn in_dir_order(a: (&[u8], bool), b: (&[u8], bool)) -> Ordering {
    match (a.1, b.1) {
        (true, false) => Ordering::Less,
        (false, true) => Ordering::Greater,
        (true, true) => dir_name_cmp(b.0, a.0),
        (false, false) => b.0.cmp(a.0),
    }
}

/// Two directory names as rsync sorts them: as if each ended in `/`.
fn dir_name_cmp(a: &[u8], b: &[u8]) -> Ordering {
    a.iter().chain(b"/").cmp(b.iter().chain(b"/"))
}

/// The order rsync deletes extraneous entries in (each a path relative to the transfer
/// root, and whether it is a directory): directory by directory as its file list visits
/// them (a directory before the ones in it, siblings by name), in each as
/// [`in_dir_order`].
fn deletion_order(a: (&[u8], bool), b: (&[u8], bool)) -> Ordering {
    fn split(rel: &[u8]) -> (&[u8], &[u8]) {
        match rel.iter().rposition(|&c| c == b'/') {
            Some(i) => (&rel[..i], &rel[i + 1..]),
            None => (b"", rel),
        }
    }
    let ((pa, na), (pb, nb)) = (split(a.0), split(b.0));
    if pa == pb {
        return in_dir_order((na, a.1), (nb, b.1));
    }
    let mut ca = pa.split(|&c| c == b'/').filter(|c| !c.is_empty());
    let mut cb = pb.split(|&c| c == b'/').filter(|c| !c.is_empty());
    loop {
        match (ca.next(), cb.next()) {
            (Some(x), Some(y)) if x == y => {}
            (Some(x), Some(y)) => return dir_name_cmp(x, y),
            // One directory is above the other: its deletions come first.
            (None, _) => return Ordering::Less,
            (_, None) => return Ordering::Greater,
        }
    }
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
            deletions: Mutex::new(HashMap::new()),
            hardlinks: Mutex::new(HashMap::new()),
            umask: fsops::current_umask(),
            remote: None,
            dest_root: PathBuf::from("."),
            alt_dirs: Vec::new(),
            backup_root: None,
            pipeline: None,
            dst_index: None,
            xrules: crate::xattrs::Rules { level: opts.xattrs, am_root },
            io_error: AtomicBool::new(false),
            io_warned: AtomicBool::new(false),
            prune: Default::default(),
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

    /// An error reading the source side (a listing, an entry's metadata or attributes):
    /// listings may now lack entries the source has, so deletions stop.
    pub(crate) fn src_err(&self, msg: impl AsRef<[u8]>) {
        self.io_error.store(true, SeqCst);
        self.err(msg);
    }

    /// The remote sender had a source-side error (see [`Engine::src_err`]).
    pub(crate) fn sender_io_error(&self) {
        self.io_error.store(true, SeqCst);
    }

    /// An entry the source has did not reach the destination: it could not be written
    /// there (a full disk, say), or its data never arrived complete. Deletions stop too,
    /// or a file renamed in the source could lose its old name on the destination without
    /// getting the new one. rsync's `--delete-after` keeps it as well: a write error ends
    /// rsync's run before it deletes.
    pub(crate) fn dst_err(&self, msg: impl AsRef<[u8]>) {
        self.io_error.store(true, SeqCst);
        self.err(msg);
    }

    /// Whether `--delete` may delete now: not after a source-side error, as rsync
    /// ("IO error encountered -- skipping file deletion", said once), nor after an
    /// entry failed to reach the destination.
    fn may_delete(&self) -> bool {
        if !self.io_error.load(SeqCst) {
            return true;
        }
        if !self.io_warned.swap(true, SeqCst) {
            self.out.notice("IO error encountered -- skipping file deletion");
        }
        false
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
                    Err(e) => return self.src_err(path_msg("link_stat \"", src, format!("\" failed: {e}"))),
                };
                let extra = self.local_root_extra(src, &src_meta);
                let mut job = self.root_job(src.clone(), dst.clone(), src_meta, 0, dest_exists && !dest_fresh, extra);
                job.rules = self.root_rules(src, src, dst);
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
                            self.src_err(path_msg("link_stat \"", &sp, format!("\" failed: {e}")));
                            continue;
                        }
                    };
                    let link = if meta.kind == Kind::Symlink { fsops::read_link_bytes(&sp).ok() } else { None };
                    ents.push(DirEnt::new(sname.clone(), meta, link));
                }
                let ents = self.adjust_listing(src_dir, ents);
                // The first directory read is the first item that is one.
                let first = ents.iter().find(|e| e.meta.kind == Kind::Dir).map_or_else(|| src_dir.clone(), |e| src_dir.join(&e.name));
                let rules = self.root_rules(src_dir, &first, dst_dir);
                let entries = ents
                    .into_iter()
                    .enumerate()
                    .map(|(i, e)| {
                        let dname = items.iter().find(|(n, _)| *n == e.name).map(|(_, d)| d.clone());
                        (i as u32, e, dname)
                    })
                    .collect();
                self.run_items(s, src_dir, dst_dir, 0, 0, entries, !dest_exists || dest_fresh, None, None, &rules);
            }
            Root::Select { base, dst, sel, with_root } => {
                let rules = self.root_rules(base, base, dst);
                if *with_root {
                    let src_meta = match fsops::stat(base) {
                        Ok(mut m) => {
                            super::flags::adjust_mode(self.opts, &mut m);
                            m
                        }
                        Err(e) => return self.src_err(path_msg("link_stat \"", base, format!("\" failed: {e}"))),
                    };
                    let extra = self.local_root_extra(base, &src_meta);
                    let mut job = self.root_job(base.clone(), dst.clone(), src_meta, 0, dest_exists && !dest_fresh, extra);
                    job.sel = sel.at_root(self.opts.recursive);
                    job.rules = rules;
                    s.spawn(move |s| self.process_dir(s, job));
                } else {
                    let ents = self.read_selected(base, sel);
                    let entries = ents.into_iter().enumerate().map(|(i, e)| (i as u32, e, None)).collect();
                    self.run_items(s, base, dst, 0, 0, entries, !dest_exists || dest_fresh, Some(sel), None, &rules);
                }
            }
        }
    }

    /// All of a run's roots. Several SRC arguments write into the same destination
    /// directory, so they are synced as one merged tree (see [`merge`]): a name several
    /// have is taken once, and `--delete` keeps whatever any of them has.
    pub fn run_roots<'s>(&'s self, s: &Scope<'s>, roots: &[Root], dest_exists: bool, dest_fresh: bool) {
        let [first, ..] = roots else { return };
        if roots.len() == 1 {
            return self.run_root(s, first, dest_exists, dest_fresh);
        }
        let dst = match first {
            Root::Contents { dst, .. } | Root::Select { dst, .. } => dst.clone(),
            Root::Items { dst_dir, .. } => dst_dir.clone(),
        };
        // Each root's top directory, and the first one transferred itself (`src/`), whose
        // attributes the destination gets.
        let mut dirs: Vec<SrcDir> = Vec::with_capacity(roots.len());
        let mut tops: Vec<bool> = Vec::with_capacity(roots.len());
        let mut top: Option<(PathBuf, Meta)> = None;
        let mut dst_rules = DirRules::default();
        for root in roots {
            let (path, sel, with_root) = match root {
                Root::Contents { src, .. } => (src, None, true),
                Root::Items { src_dir, items, .. } => {
                    (src_dir, Some(select::leak(Sel::named(items.iter().map(|(n, _)| n.as_bytes())))), false)
                }
                Root::Select { base, sel, with_root, .. } => {
                    (base, if *with_root { sel.at_root(self.opts.recursive) } else { Some(*sel) }, *with_root)
                }
            };
            let mut dev = None;
            if with_root {
                match fsops::stat(path) {
                    Ok(mut m) => {
                        super::flags::adjust_mode(self.opts, &mut m);
                        top.get_or_insert((path.clone(), m));
                        dev = Some(m.dev);
                    }
                    Err(e) => {
                        self.src_err(path_msg("link_stat \"", path, format!("\" failed: {e}")));
                        continue;
                    }
                }
            }
            if dirs.iter().all(|d| d.path != *path || d.sel != sel) {
                let rules = self.root_rules(path, path, &dst);
                dst_rules = rules.dst;
                dirs.push(SrcDir { path: path.clone(), sel, dev, rules: rules.src });
                tops.push(with_root);
            }
        }
        let mut lists: Vec<Vec<DirEnt>> = Vec::with_capacity(dirs.len());
        for (d, with_root) in dirs.iter_mut().zip(&tops) {
            // A `src/` directory is read, so its own filter files count; named items' isn't.
            let ents = match if *with_root { self.src_rules(&d.rules, &d.path, b"") } else { Some(d.rules.clone()) } {
                Some(rules) => {
                    d.rules = rules;
                    self.read_source(&d.path, d.sel).unwrap_or_default()
                }
                None => Vec::new(),
            };
            lists.push(ents.into_iter().filter(|e| self.sends(&d.path, b"", &d.rules, d.sel, d.dev, e)).collect());
        }
        let (ents, sources) = merge::merge(dirs, lists);
        let entries: Entries = ents.into_iter().enumerate().map(|(i, e)| (i as u32, e, None)).collect();
        let rules = Rules { src: DirRules::default(), dst: dst_rules };
        let Some((path, meta)) = top else {
            // Only named items: the destination itself is neither pruned nor changed.
            let base = sources.dirs.first().map(|d| d.path.clone()).unwrap_or_default();
            return self.run_items(s, &base, &dst, 0, 0, entries, !dest_exists || dest_fresh, None, Some(&sources), &rules);
        };
        let extra = self.local_root_extra(&path, &meta);
        let mut job = self.root_job(path, dst, meta, 0, dest_exists && !dest_fresh, extra);
        job.rules = rules;
        s.spawn(move |s| {
            let files = self.diff_listing(s, &job, entries, Some(&sources), &DirRules::default());
            self.transfer(job.wire_dir, files);
        });
    }

    /// The filter state of a transfer root: `transfer_root` is what its paths are
    /// relative to, `first_dir` the first directory read in it, `dst` where it goes.
    pub(crate) fn root_rules(&self, transfer_root: &Path, first_dir: &Path, dst: &Path) -> Rules {
        Rules {
            src: if self.remote.is_some() { DirRules::default() } else { self.filter.root(transfer_root, first_dir, Side::Sender) },
            dst: if self.opts.delete { self.filter.root(dst, dst, Side::Receiver) } else { DirRules::default() },
        }
    }

    /// The source-side filter state of directory `dir` (at `rel`), from its parent's: its
    /// per-directory filter files are read. `None` after reporting one that could not be
    /// read: the directory is left out, since what it would exclude is unknown.
    fn src_rules(&self, inherited: &DirRules, dir: &Path, rel: &[u8]) -> Option<DirRules> {
        if self.remote.is_some() {
            return Some(DirRules::default());
        }
        self.filter.enter(inherited, dir, rel).map_err(|m| self.src_err(m)).ok()
    }

    /// Whether entry `e` of source directory `dir` (at `rel`, its own filter state `rules`,
    /// `-R` selection `sel`, `-x` device `root_dev`) is transferred: no rule excludes it,
    /// and with `-m` a directory has something to transfer below it. A remote sender
    /// applied the rules already; the ones only it can check (per-directory files,
    /// absolute paths) are trusted, as rsync trusts them.
    fn sends(&self, dir: &Path, rel: &[u8], rules: &DirRules, sel: Option<&'static Sel>, root_dev: Option<u64>, e: &DirEnt) -> bool {
        if self.remote.is_some() {
            let erel = join_rel(rel, &e.name);
            return !self.filter.is_static() || !self.filter.excluded(&DirRules::default(), &erel, e.meta.kind == Kind::Dir, Side::Sender);
        }
        super::prune::sends(self.opts, &self.filter, &self.prune, dir, rel, rules, sel, root_dev, e, &|m| self.src_err(m))
    }

    /// The selected entries of `dir`, with missing names reported as errors.
    fn read_selected(&self, dir: &Path, sel: &Sel) -> Vec<DirEnt> {
        let (ents, errs) = select::read_selected(dir, sel);
        for (p, e) in errs {
            self.src_err(path_msg("link_stat \"", &p, format!("\" failed: {e}")));
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
            dst_dev: pre.map(|p| p.dev),
            src_meta,
            pre,
            sel: None,
            more: Vec::new(),
            rules: Rules::default(),
        }
    }

    /// Named top-level entries (`src` without a trailing slash, or single files) synced
    /// into `dst_dir`. Unlike a directory, `dst_dir` itself is neither pruned nor
    /// given the source's attributes. `rules`: the transfer root's filter state.
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
        sources: Option<&Sources>,
        rules: &Rules,
    ) {
        // Merged roots were filtered source by source.
        let entries: Entries = match sources {
            Some(_) => entries,
            None => entries.into_iter().filter(|(_, e, _)| self.sends(src_dir, b"", &rules.src, sel, None, e)).collect(),
        };
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
            dst_dev: None,
            sel,
            wire_dir,
            sources,
            rules,
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
        let scanned = vec![None; 1 + job.more.len()];
        self.process_sources(s, job, scanned);
    }

    /// Sync a directory: its listing from each of its sources (`job.src`, then
    /// `job.more`) is `scanned[i]` (`--scanner=jwalk`) or is read now.
    pub(crate) fn process_sources<'s>(&'s self, s: &Scope<'s>, mut job: DirJob, mut scanned: Vec<Option<Vec<DirEnt>>>) {
        if fsops::interrupted() {
            return;
        }
        if job.more.is_empty() {
            let ents = match scanned.pop().flatten() {
                Some(ents) => self.adjust_listing(&job.src, ents),
                None => match self.read_source(&job.src, job.sel) {
                    Some(ents) => ents,
                    None => return self.push_fixup(&job, false),
                },
            };
            let entries = ents.into_iter().enumerate().map(|(i, e)| (i as u32, e, None)).collect();
            return self.process_listing(s, job, entries);
        }
        let own = SrcDir { path: job.src.clone(), sel: job.sel, dev: job.root_dev, rules: job.rules.src.clone() };
        let mut dirs: Vec<SrcDir> = std::iter::once(own).chain(std::mem::take(&mut job.more)).collect();
        let mut lists = Vec::with_capacity(dirs.len());
        for (d, scanned) in dirs.iter_mut().zip(scanned) {
            let ents = match self.src_rules(&d.rules, &d.path, &job.rel) {
                Some(rules) => {
                    d.rules = rules;
                    match scanned {
                        Some(ents) => Some(self.adjust_listing(&d.path, ents)),
                        None => self.read_source(&d.path, d.sel),
                    }
                }
                None => None,
            };
            // Each source's entries go by its own rules, before a name in several is chosen.
            lists.push(ents.map(|l| l.into_iter().filter(|e| self.sends(&d.path, &job.rel, &d.rules, d.sel, d.dev, e)).collect::<Vec<_>>()));
        }
        if lists.iter().all(Option::is_none) {
            return self.push_fixup(&job, false);
        }
        let (ents, sources) = merge::merge(dirs, lists.into_iter().map(Option::unwrap_or_default).collect());
        let entries = ents.into_iter().enumerate().map(|(i, e)| (i as u32, e, None)).collect();
        let files = self.diff_listing(s, &job, entries, Some(&sources), &DirRules::default());
        self.transfer(job.wire_dir, files);
    }

    /// The listing of source directory `dir` (only `sel`'s entries, if given), or `None`
    /// after reporting that it could not be read.
    fn read_source(&self, dir: &Path, sel: Option<&Sel>) -> Option<Vec<DirEnt>> {
        match sel {
            Some(sel) if !sel.contents => Some(self.read_selected(dir, sel)),
            _ => match fsops::read_dir_meta(dir) {
                Ok(v) => Some(self.adjust_listing(dir, v)),
                Err(e) => {
                    self.src_err(path_msg("opendir \"", dir, format!("\" failed: {e}")));
                    None
                }
            },
        }
    }

    /// Sync one directory given its source listing.
    pub(crate) fn process_listing<'s>(&'s self, s: &Scope<'s>, job: DirJob, entries: Entries) {
        let Some(rules) = self.src_rules(&job.rules.src, &job.src, &job.rel) else { return self.push_fixup(&job, false) };
        let files = self.diff_listing(s, &job, entries, None, &rules);
        self.transfer(job.wire_dir, files);
    }

    /// `src_rules`: the directory's own source-side filter state, which a listing from a
    /// single source is filtered with here (merged ones were filtered source by source).
    fn diff_listing<'s>(&'s self, s: &Scope<'s>, job: &DirJob, entries: Entries, sources: Option<&Sources>, src_rules: &DirRules) -> Vec<FileJob> {
        let entries: Entries = if sources.is_some() || (self.filter.is_empty() && !self.opts.prune_empty_dirs) {
            entries
        } else {
            entries.into_iter().filter(|(_, e, _)| self.sends(&job.src, &job.rel, src_rules, job.sel, job.root_dev, e)).collect()
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
        // The destination's own filter files decide what --delete leaves alone; one that
        // can't be read leaves the whole directory alone.
        let dst_rules = if self.opts.delete && !job.dst_is_new {
            self.filter.enter(&job.rules.dst, &job.dst, &job.rel).map_err(|m| self.err(m)).ok()
        } else {
            Some(job.rules.dst.clone())
        };

        let mut changed = false;
        // A merged directory is pruned when any of its sources sends its full contents.
        let full = match sources {
            Some(src) => src.dirs.iter().any(|d| select::may_delete(d.sel)),
            None => select::may_delete(job.sel),
        };
        // -x: as in rsync, nothing is deleted in a directory on another filesystem than
        // the top one, and a directory on another one than this (a mount point) stays.
        let dev = job.pre.map(|p| p.dev).filter(|_| self.opts.one_file_system);
        if let Some(dst_rules) = &dst_rules
            && self.opts.delete
            && !job.dst_is_new
            && full
            && (dev.is_none() || dev == job.dst_dev)
            && self.may_delete()
        {
            changed |= self.defer_extraneous(job, &entries, &dst_map, dst_rules, dev);
        }
        let dst_handle = if job.dst_is_new || dst_metas.is_some() { None } else { fsops::DirHandle::open(&job.dst).ok() };
        let rules = Rules { src: src_rules.clone(), dst: dst_rules.unwrap_or_else(|| job.rules.dst.clone()) };
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
            dst_dev: job.dst_dev,
            sel: job.sel,
            wire_dir: job.wire_dir,
            sources,
            rules: &rules,
        };
        let (c, files) = self.process_entries(s, &ctx, entries, &dst_map);
        self.push_fixup(job, changed || c);
        files
    }

    /// Queue what `dst_map` has and the listing doesn't for deletion, unless a
    /// destination-side rule (`dst_rules`) protects it. `dev` (`-x`): the directory's
    /// device; a directory on another one (a mount point) stays.
    fn defer_extraneous(&self, job: &DirJob, entries: &Entries, dst_map: &HashMap<OsString, Kind>, dst_rules: &DirRules, dev: Option<u64>) -> bool {
        let src_names: HashSet<&OsStr> = entries.iter().map(|(_, e, _)| e.name.as_os_str()).collect();
        // rsync's --info=mount (on with -vv) names each mount point here, listed or not.
        let mounts = dev.is_some() && self.out.verbose() > 1;
        let extra: Vec<&OsString> = dst_map
            .iter()
            .filter(|(n, k)| !src_names.contains(n.as_os_str()) || (mounts && **k == Kind::Dir))
            .filter(|(n, k)| !self.protected(dst_rules, &join_rel(&job.rel, n), n, **k == Kind::Dir, Side::Receiver))
            .map(|(n, _)| n)
            .collect();
        let mut changed = false;
        let mut pending = self.deletions.lock().unwrap();
        for name in extra {
            let path = job.dst.join(name);
            let meta = fsops::lstat(&path);
            if let (Ok(m), Some(dev)) = (&meta, dev)
                && m.kind == Kind::Dir
                && m.dev != dev
            {
                if mounts {
                    self.out.note([b"cannot delete mount point: ", &join_rel(&job.rel, name)[..]].concat());
                }
                continue;
            }
            if src_names.contains(name.as_os_str()) {
                continue;
            }
            changed = true;
            match meta {
                Ok(meta) => {
                    pending.entry(path).or_insert_with(|| Deletion { rel: join_rel(&job.rel, name), meta, rules: dst_rules.clone() });
                }
                Err(e) if e.kind() == io::ErrorKind::NotFound => {}
                Err(e) => self.err(path_msg("stat before deletion \"", &path, format!("\" failed: {e}"))),
            }
        }
        changed
    }

    /// Whether `--delete` leaves destination entry `name` (at `rel`, in a directory with
    /// destination-side filter state `rules`) alone: a rule protects it, or it is a backup
    /// (without `--backup-dir`, which rsync protects with a `P *~` rule).
    fn protected(&self, rules: &DirRules, rel: &[u8], name: &OsStr, is_dir: bool, side: Side) -> bool {
        self.filter.excluded(rules, rel, is_dir, side) || (!self.opts.delete_excluded && self.is_protected_backup(name))
    }

    /// Called only after all scanning/transfer tasks have joined, and for a remote
    /// transfer after the sender's completion was checked. A later source error
    /// invalidates every earlier absence decision, and so does an entry that failed to
    /// reach the destination. Run before directory fixups.
    /// Deletions go in rsync's order, which decides what `--max-delete` lets through.
    pub(crate) fn finish_deletions(&self) {
        let pending = std::mem::take(&mut *self.deletions.lock().unwrap());
        if pending.is_empty() || fsops::interrupted() || !self.may_delete() {
            return;
        }
        let mut pending: Vec<_> = pending.into_iter().collect();
        pending.sort_unstable_by(|a, b| deletion_order((&a.1.rel, a.1.meta.kind == Kind::Dir), (&b.1.rel, b.1.meta.kind == Kind::Dir)));
        for (path, deletion) in pending {
            if fsops::interrupted() {
                break;
            }
            // An older/misbehaving peer can send separate overlapping roots. An
            // entry selected by any of them must survive another root's absence.
            if self.overlap().is_some_and(|o| o.selected(&path)) {
                continue;
            }
            match fsops::lstat(&path) {
                Ok(now) if now.kind == deletion.meta.kind && fsops::same_file(&now, &deletion.meta) => {
                    self.delete_item(&path, &deletion.rel, now.kind, &deletion.rules, false, false);
                }
                Ok(_) => self.err(path_msg("skipping deletion of changed entry \"", &path, "\"")),
                Err(e) if e.kind() == io::ErrorKind::NotFound => {}
                Err(e) => self.err(path_msg("stat before deletion \"", &path, format!("\" failed: {e}"))),
            }
        }
        let skipped = Stats::get(&self.stats.del_skipped);
        if skipped > 0 {
            self.out.warning(format!("Deletions stopped due to --max-delete limit ({skipped} skipped)"));
        }
    }

    /// rsync's `delete_item`: remove `path` (at `rel`), a directory's contents first
    /// unless `emptied` says that was done. `rules`: the destination-side filter state of
    /// the directory it is in. `room`: it makes way for another type of entry, which is
    /// neither reported nor counted, nor stopped by `--max-delete` (its contents are).
    fn delete_item(&self, path: &Path, rel: &[u8], kind: Kind, rules: &DirRules, emptied: bool, room: bool) -> Del {
        if fsops::interrupted() {
            return Del::Failed;
        }
        if kind == Kind::Dir && !emptied {
            let dev = match self.opts.one_file_system.then(|| fsops::lstat(path)) {
                None => None,
                Some(Ok(m)) => Some(m.dev),
                Some(Err(e)) => {
                    self.err(path_msg("stat \"", path, format!("\" failed: {e}")));
                    return Del::Failed;
                }
            };
            let r = self.delete_contents(path, rel, rules, dev);
            if r != Del::Done {
                return r;
            }
        }
        if !room && self.opts.max_delete.is_some_and(|max| Stats::get(&self.stats.deleted) >= max) {
            Stats::add(&self.stats.del_skipped, 1);
            return Del::AtLimit;
        }
        let r = if kind == Kind::Dir {
            if self.dry() { Ok(()) } else { fs::remove_dir(path) }
        } else if self.dry() {
            Ok(())
        } else {
            self.remove_or_backup(path)
        };
        match r {
            Ok(()) => {
                if !room {
                    self.out.deleting(rel, kind == Kind::Dir);
                    Stats::add(&self.stats.deleted, 1);
                }
                Del::Done
            }
            Err(e) if kind == Kind::Dir && matches!(e.raw_os_error(), Some(libc::ENOTEMPTY | libc::EEXIST)) => {
                self.out.note([b"cannot delete non-empty directory: ", quoted(rel)].concat());
                Del::NotEmpty
            }
            Err(e) if e.kind() == io::ErrorKind::NotFound => Del::Done,
            Err(e) => {
                let what = if kind == Kind::Dir { "rmdir" } else { "unlink" };
                self.err(path_msg(&format!("delete_file: {what} \""), path, format!("\" failed: {e}")));
                Del::Failed
            }
        }
    }

    /// rsync's `delete_dir_contents`: delete what directory `path` (at `rel`, in a
    /// directory with destination-side filter state `parent`) holds, as it is being
    /// deleted. Its own filter files count; what a rule protects stays, perishable rules
    /// aside, and so does the directory then. With `-x`, so does a directory that isn't
    /// on device `dev` (`path`'s): a mount point.
    fn delete_contents(&self, path: &Path, rel: &[u8], parent: &DirRules, dev: Option<u64>) -> Del {
        let rules = match self.filter.enter(parent, path, rel) {
            Ok(r) => r,
            Err(m) => {
                self.err(m);
                return Del::Failed;
            }
        };
        let kids = match fsops::read_dir_kinds(path) {
            Ok(m) => m,
            Err(e) => {
                self.err(path_msg("opendir \"", path, format!("\" failed: {e}")));
                return Del::Failed;
            }
        };
        let mut ret = Del::Done;
        // rsync: "mount point, X, pins parent directory". One that can't be checked stays
        // too, as an error.
        let mount = |n: &OsStr| {
            let p = path.join(n);
            match self.opts.one_file_system.then(|| fsops::lstat(&p)) {
                None => false,
                Some(Ok(m)) => Some(m.dev) != dev,
                Some(Err(e)) => {
                    self.err(path_msg("stat \"", &p, format!("\" failed: {e}")));
                    true
                }
            }
        };
        let mut kids: Vec<(OsString, Kind)> = kids
            .into_iter()
            .filter(|(n, k)| {
                let kept = self.protected(&rules, &join_rel(rel, n), n, *k == Kind::Dir, Side::Deleting) || (*k == Kind::Dir && mount(n));
                if kept {
                    ret = Del::NotEmpty;
                }
                !kept
            })
            .collect();
        kids.sort_unstable_by(|a, b| in_dir_order((a.0.as_bytes(), a.1 == Kind::Dir), (b.0.as_bytes(), b.1 == Kind::Dir)));
        for (name, kind) in kids {
            if fsops::interrupted() {
                return Del::Failed;
            }
            let (p, r) = (path.join(&name), join_rel(rel, &name));
            if kind == Kind::Dir && self.delete_contents(&p, &r, &rules, dev) != Del::Done {
                ret = Del::NotEmpty;
            }
            if self.delete_item(&p, &r, kind, &rules, true, false) != Del::Done {
                ret = Del::NotEmpty;
            }
        }
        if ret == Del::NotEmpty {
            self.out.note([b"cannot delete non-empty directory: ", quoted(rel)].concat());
        }
        ret
    }

    /// A directory sits where a non-directory (`what`, as in "regular file") must go.
    /// rsync only removes it when deletions are enabled (what its contents' rules
    /// protect staying, `rules` being those of the directory it is in) or it is empty.
    fn remove_conflicting_dir(&self, path: &Path, rel: &[u8], rules: &DirRules, what: &str) -> bool {
        // Several roots: another one's directory stays (rsync keeps the directory too).
        if self.overlap().is_some_and(|o| o.taken(path)) {
            self.err([b"cannot replace directory of another source: ", quoted(rel)].concat());
            return false;
        }
        let r = if self.opts.delete && self.opts.one_file_system && mount_point(path) {
            // -x: not even to make way (rsync deletes the files at the top of it).
            Del::NotEmpty
        } else if self.opts.delete {
            self.delete_item(path, rel, Kind::Dir, rules, false, true)
        } else if self.dry() {
            return true;
        } else {
            match fs::remove_dir(path) {
                Ok(()) => Del::Done,
                Err(_) => {
                    self.out.note([b"cannot delete non-empty directory: ", quoted(rel)].concat());
                    Del::NotEmpty
                }
            }
        };
        if r != Del::Done {
            self.err([format!("could not make way for new {what}: ").as_bytes(), quoted(rel)].concat());
            return false;
        }
        true
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
    fn claim_leader(&self, root: u16, x: &Src, src_path: &Path, dst_path: &Path, rel: &[u8], rules: &DirRules) -> bool {
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
                    rules: rules.clone(),
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
                        self.dst_err([&b"hard link target missing for "[..], &f.rel].concat());
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
                if dst.is_some_and(|d| d.kind == Kind::Dir) && !self.remove_conflicting_dir(&f.dst_path, &f.rel, &f.rules, "regular file") {
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
                    Err(e) => self.dst_err(path_msg("link \"", &f.dst_path, format!("\" failed: {e}"))),
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
            let src_path = ctx.src_dir(idx).join(&e.name);
            let dst_path = ctx.dst.join(dname);
            if self.opts.delete
                && let Some(overlap) = self.overlap()
            {
                overlap.select(&dst_path);
            }
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
                        self.claim_leader(ctx.root, &x, &src_path, &dst_path, &rel, &ctx.rules.dst)
                    };
                    if self.opts.hard_links && e.meta.nlink > 1 && !leads() {
                        // Linked at the end, which modifies this directory.
                        changed = true;
                        continue;
                    }
                    let mut dst_meta = None;
                    match dst_kind {
                        Some(Kind::Dir) => {
                            if !self.remove_conflicting_dir(&dst_path, &rel, &ctx.rules.dst, "regular file") {
                                continue;
                            }
                            changed = true;
                        }
                        Some(_) => match ctx.lstat_dst(dname, &dst_path) {
                            Ok(m) => dst_meta = Some(m),
                            Err(e) if e.kind() == io::ErrorKind::NotFound => {}
                            Err(e) => {
                                self.dst_err(path_msg("stat \"", &dst_path, format!("\" failed: {e}")));
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
                    let from = self.xsource(ctx, idx, &src_path, &rel, &dst_path);
                    changed |= self.handle_symlink(ctx.root, &e, from, &rel, &src_path, &dst_path, dst_kind, &ctx.rules.dst);
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
                    changed |= self.handle_special(ctx.root, &src, &rel, &src_path, &dst_path, dst_kind, &ctx.rules.dst);
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
                cands.push((*idx, ctx.src_dir(*idx).join(&e.name), dst_path));
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
        let (sel, src_dev) = ctx.src_sel(idx);
        let descend = select::descend(sel, rel.rsplit(|&b| b == b'/').next().unwrap_or(&rel), self.opts.recursive);
        if descend == Descend::No && !self.opts.dirs {
            self.out.note([b"skipping directory ", quoted(&rel)].concat());
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
                    self.dst_err(path_msg("stat \"", &dst_path, format!("\" failed: {e}")));
                    return false;
                }
            },
            Some(_) => {
                if !self.dry()
                    && let Err(e) = self.remove_or_backup(&dst_path)
                {
                    self.dst_err(path_msg("unlink \"", &dst_path, format!("\" failed: {e}")));
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
                    self.dst_err(path_msg("mkdir \"", &dst_path, format!("\" failed: {e}")));
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
        let root_dev = src_dev.unwrap_or(meta.dev);
        let name = src_path.file_name().unwrap_or_default();
        let more = match ctx.sources {
            Some(src) if descend != Descend::No => src.also_dirs(idx, name, self.opts.recursive, self.opts.one_file_system),
            _ => Vec::new(),
        };
        // -x: a top directory (for any source that has it, when several merge) starts
        // from its own device.
        let top = match ctx.sources {
            Some(src) => src.top(idx, name, self.opts.recursive),
            None => select::top_dir(sel, name.as_bytes(), self.opts.recursive),
        };
        let job = DirJob {
            src: src_path,
            dst: dst_path,
            rel,
            root: ctx.root,
            wire_dir: 0,
            depth: ctx.depth + 1,
            dst_is_new: is_new,
            root_dev: Some(root_dev),
            dst_dev: ctx.dst_dev.filter(|_| !top).or(pre.map(|p| p.dev)),
            src_meta: meta,
            pre,
            sel: match descend {
                Descend::Some(sel) => Some(sel),
                _ => None,
            },
            more,
            rules: Rules { src: ctx.src_rules(idx).clone(), dst: ctx.rules.dst.clone() },
        };
        // -x: a directory on another filesystem than its transfer root (a mount point) is
        // created, but nothing in it is synced. A remote sender decides, and marks the ones
        // it doesn't list.
        let mount = match &self.remote {
            Some(r) => r.mount(ctx.wire_dir, idx),
            None => self.opts.one_file_system && meta.dev != root_dev,
        };
        if descend == Descend::No || mount {
            // -d, or a mount point: the directory itself, without its contents.
            self.push_fixup(&job, false);
        } else if let Some(r) = &self.remote {
            // Its listing arrives as its own batch.
            r.expect_dir(s, self, job);
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
        rules: &DirRules,
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
            Some(Kind::Dir) if !self.remove_conflicting_dir(dst_path, rel, rules, "symlink") => return false,
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
            Err(e) => self.dst_err(path_msg("symlink \"", dst_path, format!("\" failed: {e}"))),
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

    #[allow(clippy::too_many_arguments)]
    fn handle_special(&self, root: u16, x: &Src, rel: &[u8], src_path: &Path, dst_path: &Path, dst_kind: Option<Kind>, rules: &DirRules) -> bool {
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
            Some(Kind::Dir) if !self.remove_conflicting_dir(dst_path, rel, rules, if meta.kind.is_device() { "device file" } else { "special file" }) => {
                return false;
            }
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
            self.dst_err(path_msg("mknod \"", dst_path, format!("\" failed: {e}")));
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
        fsops::test_vanish(&job.src_path);
        // A big copy moves the progress line as it goes, not only once it is done.
        let (r, counted) = crate::progress::tracking(|| self.write_file(job));
        crate::progress::done(job.meta.size.saturating_sub(counted));
        match r {
            Ok(()) => self.file_written(job),
            Err(e) if fsops::interrupted() => drop(e),
            Err(e) if e.kind() == io::ErrorKind::NotFound && fsops::lstat(&job.src_path).is_err_and(|e| e.kind() == io::ErrorKind::NotFound) => {
                self.vanished(&job.src_path)
            }
            Err(e) => {
                let to = path_msg("\" -> \"", &job.dst_path, format!("\" failed: {e}"));
                self.dst_err(path_msg("copy \"", &job.src_path, to));
            }
        }
    }

    /// A source file was gone when it was to be copied: a warning, not an error, and the
    /// run ends with 24 (rsync's "file has vanished").
    pub(crate) fn vanished(&self, path: &Path) {
        Stats::add(&self.stats.vanished, 1);
        self.out.warning(path_msg("file has vanished: \"", &fsops::full_path(path), "\""));
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
        // Opened once, before anything is created: what is read must be the listed file.
        let src_file = fsops::open_listed(&job.src_path, src, self.opts.copy_links)?;
        // All of a file sent whole is literal data, as rsync counts it.
        Stats::add(&self.stats.literal_bytes, src.size);

        if self.opts.inplace && job.dst_meta.is_some_and(|d| d.kind == Kind::File) {
            fsops::copy_into_existing(&src_file, &job.dst_path, src.size)?;
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
                .copy_from(&src_file, &job.dst_path, src.size, allow_fast, mode)
                .and_then(|copied| self.apply_new_file_attrs(&job.dst_path, copied, mode, uid, gid, mtime, Some(&job.src(false))))
                .and_then(|()| fsops::check_interrupt());
            return self.finish_or_keep(&job.dst_path, job, r);
        }
        let (dir, name) = split_dst(&job.dst_path);
        let (tmp, copied) = fsops::with_temp(dir, name, |tmp| {
            // An interrupted copy is kept with --partial before the temp is discarded.
            self.copy_from(&src_file, tmp, src.size, allow_fast, mode).inspect_err(|_| {
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
        let new = fsops::open_listed(&job.src_path, &job.meta, self.opts.copy_links)?;
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
    /// The source-side filter state entry `idx` was checked with: its directory's own.
    fn src_rules(&self, idx: u32) -> &DirRules {
        self.sources.map_or(&self.rules.src, |s| &s.dir_of(idx).rules)
    }

    /// The source directory entry `idx` of this listing is in.
    fn src_dir(&self, idx: u32) -> &Path {
        self.sources.map_or(self.src, |s| &s.dir_of(idx).path)
    }

    /// The selection and transfer-root device of the source entry `idx` comes from.
    fn src_sel(&self, idx: u32) -> (Option<&'static Sel>, Option<u64>) {
        match self.sources {
            Some(s) => (s.dir_of(idx).sel, s.dir_of(idx).dev),
            None => (self.sel, self.root_dev),
        }
    }

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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn deletions_follow_rsyncs_order() {
        let mut v: Vec<(&[u8], bool)> = vec![
            (b"z", false),
            (b"a-b", true),
            (b"a/s/t", false),
            (b"a", true),
            (b"b/q", false),
            (b"y", false),
            (b"a/x", false),
            (b"a/s", true),
        ];
        v.sort_by(|a, b| deletion_order(*a, *b));
        let want: Vec<(&[u8], bool)> = vec![
            // The top directory first: subdirectories (`a` sorts after `a-b` as `a/`), then
            // files, each descending; then `a`, `a/s` and `b`, in the order rsync visits them.
            (b"a", true),
            (b"a-b", true),
            (b"z", false),
            (b"y", false),
            (b"a/s", true),
            (b"a/x", false),
            (b"a/s/t", false),
            (b"b/q", false),
        ];
        assert_eq!(v, want);
    }
}

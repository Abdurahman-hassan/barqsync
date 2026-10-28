//! The sending end of a remote transfer (the client for a push, the server for a pull).
//!
//! Scans the source tree in parallel and streams one `DirBatch` per directory as soon
//! as it is read, while answering the receiver's `Need`s concurrently: small files go
//! out packed many-per-frame, larger ones as `Data` streams, and files the receiver
//! already has an old version of as deltas against the signature it sent.

use super::{SendFailure, Stop, Strategy, WORKER_STACK, adapt, recv_unless};
use crate::cli::Options;
use crate::checksum::{self, Sum};
use crate::delta::refine::{self, GapSig, Grain};
use crate::delta::{self, Delta, Op, Params, ReadAt, ReadOnce, cdc};
use crate::entry::{DirEnt, Kind, Meta};
use crate::ids;
use crate::local::extra;
use crate::local::merge::{self, SrcDir, Sources};
use crate::local::flags::{adjust_listing, adjust_mode, remove_source};
use crate::xattrs::{Rules, Xattr};
use crate::select::{self, Descend, Sel};
use crate::exit;
use crate::filter::{DirRules, Filter, Side};
use crate::fsops;
use crate::local::prune::{self, Prune};
use crate::output::{Output, StatsSnapshot, path_msg};
use crate::paths::{join_rel, split_source, wants_contents};
use crate::proto::{self, Conn, Frame, FrameSender, Msg, NeedReq, RootInfo, Sig, SyncedFiles, file_id};
use rayon::Scope;
use std::collections::HashMap;
use std::ffi::OsString;
use std::fs::File;
use std::io;
use std::os::unix::ffi::OsStrExt;
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering::SeqCst};

struct Sender<'a> {
    out_frames: FrameSender,
    opts: &'a Options,
    out: &'a Output,
    filter: Filter,
    /// Listings sent and not yet answered, by dir id.
    listings: Mutex<HashMap<u32, Listing>>,
    next_dir: AtomicU32,
    needs_done: AtomicU32,
    /// Large-file sends still running (each is its own task).
    pending_files: AtomicU32,
    scan_done: AtomicBool,
    done_sent: AtomicBool,
    errors: AtomicU64,
    /// Errors counted in `SenderDone`.
    reported: AtomicU64,
    /// Ids already named to the receiver (no `--numeric-ids`).
    ids: Mutex<ids::Sent>,
    /// `--remove-source-files`: the entries listed so far and not yet removed, by root and
    /// path relative to it: where each is and what it was when listed. The receiver
    /// names what it has; only these may go, and whether they changed meanwhile is
    /// judged by this record.
    removable: Mutex<Removable>,
    /// `-X`: which attribute names are sent.
    xrules: Rules,
    /// Deltas waiting for the receiver's answer to a `Refine`: file id -> where it goes.
    refining: Mutex<HashMap<u64, crossbeam_channel::Sender<Vec<GapSig>>>>,
    /// `IoError` was sent. A lock, not a flag: a listing read after an error must not
    /// overtake the `IoError` on the wire.
    io_error: Mutex<bool>,
    /// A message too large to send stops the transfer.
    failed: SendFailure,
    /// Files that were gone when they were to be sent (rsync's "file has vanished").
    vanished: AtomicU64,
    /// `-m`: which source directories have content.
    prune: Prune,
}

/// `--remove-source-files`: (root, path) -> where the entry is and what it was.
type Removable = HashMap<(u16, Vec<u8>), (PathBuf, Meta)>;

/// A top directory of the transfer root, with its listing when it is named items.
type Top = (SrcDir, Option<Vec<DirEnt>>);

/// A listing sent and not yet answered: its entries and the source directories they
/// are in (several when SRC arguments merge into one destination directory).
struct Listing {
    entries: Vec<DirEnt>,
    sources: Sources,
}

impl Listing {
    fn path(&self, idx: usize) -> PathBuf {
        self.sources.dir_of(idx as u32).path.join(&self.entries[idx].name)
    }
}

/// A file the receiver asked for: where it is and what it was when listed, which it
/// must still be when its data is read (see [`fsops::open_listed`]).
struct Source {
    id: u64,
    path: PathBuf,
    meta: Meta,
    /// Its attribute values go along in full (the listing only had checksums of some).
    full: bool,
}

/// A source argument resolved on disk.
struct SrcRoot {
    info: RootInfo,
    /// The `src/` directory itself, or the directory holding the named items.
    base: PathBuf,
    /// `-R`/`--files-from`: the selected paths below `base`.
    sel: Option<&'static Sel>,
    /// Its transfer root's filter state (see [`DirRules`]).
    rules: DirRules,
}

/// Whether `e` failed to open because its file is gone (rsync's "file has vanished").
fn gone(e: &io::Error, path: &Path) -> bool {
    e.kind() == io::ErrorKind::NotFound && fsops::lstat(path).is_err_and(|e| e.kind() == io::ErrorKind::NotFound)
}

impl Sender<'_> {
    fn err(&self, msg: impl AsRef<[u8]>) {
        self.errors.fetch_add(1, SeqCst);
        self.out.error(msg);
    }

    /// A file listed was gone when it was to be sent: a warning, not an error, and the
    /// run ends with 24 (rsync's "file has vanished").
    fn vanished(&self, path: &Path) {
        self.vanished.fetch_add(1, SeqCst);
        self.out.warning(path_msg("file has vanished: \"", &fsops::full_path(path), "\""));
    }

    /// The filter state of directory `dir` (at `rel`) from its parent's: its
    /// per-directory filter files are read. `None` after reporting one that could not be
    /// read: the directory is left out, since what it would exclude is unknown.
    fn enter(&self, inherited: &DirRules, dir: &Path, rel: &[u8]) -> Option<DirRules> {
        self.filter.enter(inherited, dir, rel).map_err(|m| self.scan_err(m)).ok()
    }

    /// An error reading the source: the listings may now lack entries, which the
    /// receiver must not delete (`IoError`, queued before any later listing).
    fn scan_err(&self, msg: impl AsRef<[u8]>) {
        self.err(msg);
        let mut sent = self.io_error.lock().unwrap();
        if !*sent {
            self.send(Msg::IoError);
            *sent = true;
        }
    }

    fn send(&self, m: Msg) -> bool {
        self.send_named(m, || b"a message".to_vec())
    }

    /// [`Sender::send`] a message about `path`: `what` (`"the listing of \""`) names it
    /// in an error.
    fn send_for(&self, m: Msg, what: &str, path: &Path) -> bool {
        self.send_named(m, || path_msg(what, path, "\""))
    }

    fn send_named(&self, m: Msg, what: impl FnOnce() -> Vec<u8>) -> bool {
        let sent = self.failed.send(&self.out_frames, m, what);
        if !sent {
            // A message the receiver never got: what it acknowledges then can't be
            // trusted to authorize removing sources.
            self.errors.fetch_add(1, SeqCst);
        }
        sent
    }

    /// Register a listing and send it. Registered first, so a quick `Need` finds it.
    /// `mounts`: its `-x` mount points (see `Msg::DirBatch`).
    fn send_listing(&self, root: u16, rel: Vec<u8>, listing: Listing, mounts: Vec<u32>) {
        let sums = if self.opts.checksum { self.checksums(&listing) } else { Vec::new() };
        let dir = self.next_dir.fetch_add(1, SeqCst);
        if self.removes() {
            let mut removable = self.removable.lock().unwrap();
            for (i, e) in listing.entries.iter().enumerate().filter(|(_, e)| e.meta.kind != Kind::Dir) {
                removable.insert((root, join_rel(&rel, &e.name)), (listing.path(i), e.meta));
            }
        }
        let entries = listing.entries.clone();
        // Errors name the listing by its (first) source directory.
        let path = listing.sources.dirs.first().map(|d| d.path.clone()).unwrap_or_default();
        self.listings.lock().unwrap().insert(dir, listing);
        self.name_ids(entries.iter().map(|e| (&e.meta, e.extra.as_deref())));
        self.send_for(Msg::DirBatch { root, dir, rel, entries, sums, mounts }, "the listing of \"", &path);
    }

    /// `-c`: checksums of the listing's regular files, hashed in parallel.
    fn checksums(&self, l: &Listing) -> Vec<Option<Sum>> {
        let is_file = |i: &usize| l.entries[*i].meta.kind == Kind::File;
        let files: Vec<PathBuf> = (0..l.entries.len()).filter(is_file).map(|i| l.path(i)).collect();
        let mut sums = checksum::sums(files).into_iter();
        (0..l.entries.len()).map(|i| if is_file(&i) { sums.next().and_then(Result::ok) } else { None }).collect()
    }

    /// Without `--numeric-ids`, name each uid/gid before the first listing that uses it
    /// (as an owner, or in an ACL entry). The names are queued under the lock, so no
    /// listing using an id can overtake them.
    fn name_ids<'m>(&self, ents: impl Iterator<Item = (&'m Meta, Option<&'m crate::entry::Extra>)>) {
        if self.opts.numeric_ids || !(self.opts.owner || self.opts.group || self.opts.acls) {
            return;
        }
        let mut sent = self.ids.lock().unwrap();
        let (mut users, mut groups) = (Vec::new(), Vec::new());
        for (m, x) in ents {
            let (u, g) = sent.new_names(std::iter::once(m), self.opts.owner, self.opts.group);
            users.extend(u);
            groups.extend(g);
            if let Some(acl) = x.and_then(|x| x.acl.as_ref()) {
                let (u, g) = sent.new_acl_names(acl.ids());
                users.extend(u);
                groups.extend(g);
            }
        }
        if !users.is_empty() || !groups.is_empty() {
            self.send(Msg::IdNames { users, groups });
        }
    }

    /// `-L`/`-k`/`--chmod`, then (`-X`/`-A`) the entries' attributes and ACLs.
    fn listing(&self, dir: &Path, ents: Vec<DirEnt>) -> Vec<DirEnt> {
        let ents = adjust_listing(self.opts, dir, ents, &mut |m| self.scan_err(m));
        extra::collect(self.opts, &self.xrules, dir, ents, &mut |m| self.scan_err(m))
    }

    /// The listing of source directory `dir` (only `sel`'s entries, if given), or `None`
    /// after reporting that it could not be read.
    fn read_dir(&self, dir: &Path, sel: Option<&Sel>) -> Option<Vec<DirEnt>> {
        match sel {
            Some(sel) if !sel.contents => Some(self.read_selected(dir, sel)),
            _ => match fsops::read_dir_meta(dir) {
                Ok(v) => Some(self.listing(dir, v)),
                Err(e) => {
                    self.scan_err(path_msg("opendir \"", dir, format!("\" failed: {e}")));
                    None
                }
            },
        }
    }

    /// Read and send the directory at `rel` of transfer root `root` (one source directory,
    /// or several merging), then walk its subdirectories.
    fn walk_dir<'s>(&'s self, s: &Scope<'s>, root: u16, rel: Vec<u8>, mut dirs: Vec<SrcDir>) {
        if fsops::interrupted() {
            return;
        }
        let mut lists: Vec<Option<Vec<DirEnt>>> = Vec::with_capacity(dirs.len());
        for d in &mut dirs {
            lists.push(self.enter(&d.rules, &d.path, &rel).and_then(|r| {
                d.rules = r;
                self.read_dir(&d.path, d.sel)
            }));
        }
        if lists.iter().all(Option::is_none) {
            return;
        }
        self.send_dir(s, root, rel, dirs, lists.into_iter().map(Option::unwrap_or_default).collect());
    }

    /// Send the listing at `rel`, merged from `lists` (`dirs[i]`'s), and walk on.
    fn send_dir<'s>(&'s self, s: &Scope<'s>, root: u16, rel: Vec<u8>, dirs: Vec<SrcDir>, lists: Vec<Vec<DirEnt>>) {
        let report = |m: Vec<u8>| self.scan_err(m);
        let sends = |d: &SrcDir, e: &DirEnt| prune::sends(self.opts, &self.filter, &self.prune, &d.path, &rel, &d.rules, d.sel, d.dev, e, &report);
        let lists = dirs.iter().zip(lists).map(|(d, l)| l.into_iter().filter(|e| sends(d, e)).collect()).collect();
        let (entries, sources) = merge::merge(dirs, lists);
        let (recursive, one_fs) = (self.opts.recursive, self.opts.one_file_system);
        let mut mounts = Vec::new();
        let subdirs: Vec<(Vec<u8>, Vec<SrcDir>)> = entries
            .iter()
            .enumerate()
            .filter_map(|(i, e)| {
                let d = sources.dir_of(i as u32);
                let dev = select::root_dev(d.sel, e.name.as_bytes(), e.meta.dev, d.dev);
                // -x: a mount point isn't listed from its root. Later sources that have the
                // directory on their own filesystem fill it alone (rsync's receiver merges
                // their FLAG_CONTENT_DIR); with none, its contents aren't sent, and the
                // receiver is told so, since it can't tell from merged listings which root
                // the directory is in.
                let mount = one_fs && e.meta.kind == Kind::Dir && e.meta.dev != dev;
                let Some(sub) = self.descend_into(d.sel, e) else {
                    if mount {
                        mounts.push(i as u32);
                    }
                    return None;
                };
                let own = (!mount).then(|| SrcDir { path: d.path.join(&e.name), sel: sub, dev: Some(dev), rules: d.rules.clone() });
                let srcs: Vec<SrcDir> = own.into_iter().chain(sources.also_dirs(i as u32, &e.name, recursive, one_fs)).collect();
                if srcs.is_empty() {
                    mounts.push(i as u32);
                    return None;
                }
                Some((join_rel(&rel, &e.name), srcs))
            })
            .collect();
        self.send_listing(root, rel, Listing { entries, sources }, mounts);
        for (crel, srcs) in subdirs {
            s.spawn(move |s| self.walk_dir(s, root, crel, srcs));
        }
    }

    /// The selected entries of `dir`, with missing names reported as errors.
    fn read_selected(&self, dir: &Path, sel: &Sel) -> Vec<DirEnt> {
        let (ents, errs) = select::read_selected(dir, sel);
        for (p, e) in errs {
            self.scan_err(path_msg("link_stat \"", &p, format!("\" failed: {e}")));
        }
        self.listing(dir, ents)
    }

    /// Whether the listing of subdirectory `e` is sent (unless `-x` stops at it), and with
    /// which selection. The receiver makes the same decision with [`select::descend`].
    fn descend_into(&self, sel: Option<&'static Sel>, e: &DirEnt) -> Option<Option<&'static Sel>> {
        if e.meta.kind != Kind::Dir {
            return None;
        }
        match select::descend(sel, e.name.as_bytes(), self.opts.recursive) {
            Descend::No => None,
            Descend::All => Some(None),
            Descend::Some(s) => Some(Some(s)),
        }
    }

    /// Send the top of the (single) transfer root: the source arguments' top directories
    /// merged, `src/` ones read now and named items as resolved.
    fn walk_top<'s>(&'s self, s: &Scope<'s>, tops: Vec<Top>) {
        if tops.is_empty() || fsops::interrupted() {
            return;
        }
        let (dirs, lists): (Vec<SrcDir>, Vec<Option<Vec<DirEnt>>>) = tops
            .into_iter()
            .map(|(mut d, items)| {
                // A `src/` directory is read, so its own filter files count; named items' isn't.
                let l = items.or_else(|| {
                    let r = self.enter(&d.rules, &d.path, b"")?;
                    d.rules = r;
                    self.read_dir(&d.path, d.sel)
                });
                (d, l)
            })
            .unzip();
        if lists.iter().all(Option::is_none) {
            return;
        }
        self.send_dir(s, 0, Vec::new(), dirs, lists.into_iter().map(Option::unwrap_or_default).collect());
    }

    /// Whether the receiver may report entries for removal (`Synced`).
    fn removes(&self) -> bool {
        self.opts.remove_source_files && !self.opts.dry_run
    }

    /// `--remove-source-files`: the receiver has these entries now. Each must be one this
    /// run listed and hasn't removed yet; anything else is an error and stays.
    fn remove_synced(&self, root: u16, files: SyncedFiles) {
        for rel in files {
            let key = (root, rel);
            let listed = self.removable.lock().unwrap().remove(&key);
            let rel = key.1;
            let Some((path, listed)) = listed else {
                self.err([&b"protocol error: removal of an entry not sent or already removed: "[..], &rel].concat());
                continue;
            };
            match remove_source(self.opts, &path, &listed, None) {
                Ok(()) if self.out.verbose() >= 2 => self.out.info([&b"sender removed "[..], &rel].concat()),
                Ok(()) => {}
                Err(m) => self.err(m),
            }
        }
    }

    /// Send `SenderDone` once the scan is finished and every listing has been answered.
    fn check_done(&self) {
        if self.scan_done.load(SeqCst)
            && self.needs_done.load(SeqCst) == self.next_dir.load(SeqCst)
            && self.pending_files.load(SeqCst) == 0
            && !self.done_sent.swap(true, SeqCst)
        {
            // Data streams first: the receiver finishes once it has seen every
            // stream's end as well as SenderDone (which travels on the ssh connection).
            if let Err(e) = self.out_frames.end_data_streams()
                && !self.out_frames.stopped()
            {
                self.err(format!("closing data streams failed: {e}"));
            }
            let errors = self.errors.load(SeqCst);
            self.reported.store(errors, SeqCst);
            self.send(Msg::SenderDone { errors, vanished: self.vanished.load(SeqCst) });
            crate::timing::mark("SenderDone sent");
        }
    }

    // -----------------------------------------------------------------------
    // Serving requests
    // -----------------------------------------------------------------------

    fn serve<'s>(&'s self, s: &Scope<'s>, dir: u32, reqs: Vec<NeedReq>, xattrs: Vec<u32>) {
        let listing = self.listings.lock().unwrap().remove(&dir);
        match listing {
            Some(l) => {
                self.send_xattrs(dir, &l, xattrs);
                self.serve_listing(s, dir, &l, reqs)
            }
            None => self.err(format!("protocol error: request for unknown directory {dir}")),
        }
        self.needs_done.fetch_add(1, SeqCst);
        self.check_done();
    }

    /// `-X`: the attributes of these entries in full, for the receiver to update them.
    fn send_xattrs(&self, dir: u32, l: &Listing, idxs: Vec<u32>) {
        for idx in idxs {
            if idx as usize >= l.entries.len() {
                self.err(format!("protocol error: request for unknown entry {dir}/{idx}"));
                continue;
            }
            let path = l.path(idx as usize);
            let xattrs = self.full_xattrs(&path);
            self.send_for(Msg::Xattrs { id: file_id(dir, idx), xattrs }, "the attributes of \"", &path);
        }
    }

    /// An entry's attributes with every value in full (`None` after reporting an error).
    fn full_xattrs(&self, path: &Path) -> Option<Vec<Xattr>> {
        match crate::xattrs::read_wire(path, &self.xrules, true) {
            Ok(x) => Some(x),
            Err(e) => {
                self.err(path_msg("get_xattr \"", path, format!("\" failed: {e}")));
                None
            }
        }
    }

    /// Answer one `Need`. No nested parallel loops here: these tasks block on the
    /// connection's backpressure, and a blocked parallel loop would let its thread
    /// steal more blocking tasks onto the same stack. Parallelism comes from many
    /// listings being served at once, and each large file being its own task.
    fn serve_listing<'s>(&'s self, s: &Scope<'s>, dir: u32, l: &Listing, reqs: Vec<NeedReq>) {
        let mut small = Vec::new();
        let mut large = Vec::new();
        for req in reqs {
            let Some(e) = l.entries.get(req.idx as usize) else {
                self.err(format!("protocol error: request for unknown file {dir}/{}", req.idx));
                continue;
            };
            let id = file_id(dir, req.idx);
            let path = l.path(req.idx as usize);
            // Values the listing only had checksums of go along with the data.
            let full = extra::has_summed(e.extra.as_deref());
            crate::progress::planned(e.meta.size);
            let file = Source { id, path, meta: e.meta, full };
            if req.sig.is_none() && e.meta.size <= proto::SMALL_FILE {
                small.push(file);
            } else {
                large.push((file, req.sig));
            }
        }
        if !small.is_empty() {
            // Files that grew past the small-file size since they were listed go whole.
            large.extend(self.send_small(small).into_iter().map(|f| (f, None)));
        }
        for (file, sig) in large {
            self.pending_files.fetch_add(1, SeqCst);
            s.spawn(move |_| {
                if !fsops::interrupted() {
                    fsops::test_vanish(&file.path);
                    let size = std::fs::metadata(&file.path).map(|m| m.len()).unwrap_or(0);
                    let xattrs = if file.full { self.full_xattrs(&file.path) } else { None };
                    let r = match sig {
                        Some(sig) => self.send_delta(&file, sig, xattrs),
                        None => self.send_whole(&file, xattrs),
                    };
                    match r {
                        // Sent bytes were counted as they went.
                        Ok(sent) => crate::progress::done(size.saturating_sub(sent)),
                        // A failed file doesn't count as transferred; once the run is
                        // stopping (Ctrl-C, a failed peer) that is expected, not worth a message.
                        Err(e) => {
                            if self.out_frames.stopped() || self.failed.reason().is_some() {
                            } else if gone(&e, &file.path) {
                                self.vanished(&file.path);
                            } else {
                                self.err(path_msg("send_files failed to read \"", &file.path, format!("\": {e}")));
                            }
                            self.send(Msg::FileEnd { id: file.id, ok: false });
                        }
                    }
                }
                self.pending_files.fetch_sub(1, SeqCst);
                self.check_done();
            });
        }
    }

    /// Read small files in parallel and send them packed into frames of ~4 MiB, with
    /// their attribute values in full where `full`. Returns the files that are no longer
    /// small.
    fn send_small(&self, files: Vec<Source>) -> Vec<Source> {
        let follow = self.opts.copy_links;
        let read = files.into_iter().map(|f| {
            fsops::test_vanish(&f.path);
            (read_small(&f.path, &f.meta, follow), f)
        });
        let mut pack: Vec<(u64, u32)> = Vec::new();
        let mut blob: Vec<u8> = Vec::new();
        let mut xattrs: Vec<(u64, Vec<Xattr>)> = Vec::new();
        let mut grown = Vec::new();
        for (data, f) in read {
            match data {
                Ok(None) => grown.push(f),
                Ok(Some(d)) => {
                    crate::progress::done(d.len() as u64);
                    if !blob.is_empty() && blob.len() + d.len() > proto::PACK_BYTES {
                        self.send(Msg::SmallPack {
                            files: std::mem::take(&mut pack),
                            blob: std::mem::take(&mut blob),
                            xattrs: std::mem::take(&mut xattrs),
                        });
                    }
                    if f.full && let Some(x) = self.full_xattrs(&f.path) {
                        xattrs.push((f.id, x));
                    }
                    pack.push((f.id, d.len() as u32));
                    blob.extend_from_slice(&d);
                }
                Err(e) => {
                    if gone(&e, &f.path) {
                        self.vanished(&f.path);
                    } else {
                        self.err(path_msg("send_files failed to open \"", &f.path, format!("\": {e}")));
                    }
                    self.send(Msg::FileEnd { id: f.id, ok: false });
                }
            }
        }
        if !pack.is_empty() {
            self.send(Msg::SmallPack { files: pack, blob, xattrs });
        }
        grown
    }

    fn send_range(&self, f: &dyn ReadAt, id: u64, off: u64, len: u64) -> io::Result<()> {
        let mut pos = 0;
        while pos < len {
            fsops::check_interrupt()?;
            let n = (len - pos).min(proto::DATA_CHUNK as u64) as usize;
            let mut buf = vec![0u8; n];
            f.read_exact_at(&mut buf, off + pos)?;
            self.out_frames.send(Frame::Data { id, off: off + pos, bytes: buf })?;
            crate::progress::bytes(n as u64);
            pos += n as u64;
        }
        Ok(())
    }

    /// The sending functions return the size of the file as sent, all of it counted for
    /// progress by then. `xattrs`: attribute values to send in full with the data.
    fn send_whole(&self, src: &Source, xattrs: Option<Vec<Xattr>>) -> io::Result<u64> {
        let f = fsops::open_listed(&src.path, &src.meta, self.opts.copy_links)?;
        self.send_whole_file(src.id, &f, xattrs)
    }

    fn send_whole_file(&self, id: u64, f: &File, xattrs: Option<Vec<Xattr>>) -> io::Result<u64> {
        let size = f.metadata()?.len();
        if !self.send(Msg::FileBegin { id, size, xattrs }) {
            return Err(io::Error::new(io::ErrorKind::BrokenPipe, "connection closed"));
        }
        self.send_range(&ReadOnce::new(f, size), id, 0, size)?;
        self.send(Msg::FileEnd { id, ok: true });
        Ok(size)
    }

    /// Diff the source file against the receiver's basis signature and send only the
    /// ranges it doesn't have.
    fn send_delta(&self, src: &Source, sig: Sig, xattrs: Option<Vec<Xattr>>) -> io::Result<u64> {
        let (id, path) = (src.id, src.path.as_path());
        let file = fsops::open_listed(path, &src.meta, self.opts.copy_links)?;
        let size = file.metadata()?.len();
        if let Sig::Append { len, sum } = sig {
            return self.send_append(src, &file, size, len, sum, xattrs);
        }
        if let Sig::Cdc(basis) = &sig {
            return match cdc::Mapped::new(&file, size) {
                Ok(m) => self.send_ops(id, &file, &cdc::diff(m.bytes(), basis, &Params::DEFAULT), &m, xattrs),
                Err(e) if cdc::is_changed(&e) => self.send_whole_file(id, &file, xattrs),
                Err(e) => Err(e),
            };
        }
        let f = ReadOnce::new(&file, size);
        // Hash this file with the receiver's block or chunk sizes; refuse sizes that make
        // no sense (they come from the other side) by sending the file whole.
        let (d, basis_size, unit) = match sig {
            Sig::Blocks(basis) => {
                let bs = basis.block_size as usize;
                // The block size comes from the other side; the receiver only ever sends
                // the coarse default. A zero or oversized one (which would size this
                // side's hashing buffers) is refused by sending the file whole.
                if !(1..=Params::DEFAULT.block_size).contains(&bs) {
                    return self.send_whole_file(id, &file, xattrs);
                }
                let p = Params { block_size: bs, segment: (Params::DEFAULT.segment / bs).max(1) * bs, ..Params::DEFAULT };
                (delta::diff_blocks(&delta::block_signature(&f, size, &p)?, &basis), basis.size, bs)
            }
            Sig::Chunks(basis) => {
                let Some(p) = basis.params(Params::DEFAULT) else { return self.send_whole_file(id, &file, xattrs) };
                (delta::diff_chunks(&delta::chunk_signature(&f, size, &p)?, &basis), basis.size, p.cdc_avg)
            }
            Sig::Cdc(_) | Sig::Append { .. } => unreachable!(),
        };
        let d = self.refine(id, path, &f, size, basis_size, unit, d)?;
        self.send_ops(id, &file, &d, &f, xattrs)
    }

    /// `--append`: the receiver has the first `len` bytes of this file (`size` bytes now);
    /// send the rest as a delta keeping them. With `--append-verify` (`sum`) only if
    /// they match this file's start: otherwise the file goes whole, which tells the
    /// receiver. A file now shorter than it was listed is skipped, as rsync skips a
    /// "diminished" file.
    fn send_append(&self, src: &Source, file: &File, size: u64, len: u64, sum: Option<Sum>, xattrs: Option<Vec<Xattr>>) -> io::Result<u64> {
        if size < src.meta.size {
            self.out.warning(path_msg("skipped diminished file: \"", &fsops::full_path(&src.path), "\""));
            self.send(Msg::FileEnd { id: src.id, ok: false });
            return Ok(0);
        }
        if let Some(sum) = sum
            && checksum::prefix_sum(file, len)? != sum
        {
            return self.send_whole_file(src.id, file, xattrs);
        }
        let mut d = Delta::new(size);
        d.push(Op::Copy { off: 0, basis_off: 0, len });
        d.push(Op::Literal { off: len, len: size - len });
        self.send_ops(src.id, file, &d, &ReadOnce::new(file, size), xattrs)
    }

    /// Refine a coarse delta where it is worth it ([`refine::plan`]): ask the receiver for
    /// fine signatures of the basis stretches between matched ranges, hash this file's side
    /// meanwhile, then combine. One round trip, which an append (nothing to refine) skips.
    #[allow(clippy::too_many_arguments)]
    fn refine(&self, id: u64, path: &Path, new: &dyn ReadAt, size: u64, basis_size: u64, unit: usize, d: Delta) -> io::Result<Delta> {
        let p = Params::for_size(basis_size);
        let Some(gaps) = refine::plan(&d, basis_size, unit, &p, refine::REFINE_MIN) else { return Ok(d) };
        let (tx, rx) = crossbeam_channel::bounded(1);
        self.refining.lock().unwrap().insert(id, tx);
        let req = Msg::Refine { id, grain: Grain::of(&p), gaps: gaps.iter().map(refine::Gap::request).collect() };
        if !self.send_for(req, "the delta request for \"", path) {
            // No answer will come.
            self.refining.lock().unwrap().remove(&id);
        }
        let ours = refine::new_side(new, size, basis_size, &p, &gaps);
        // No answer (the connection ended): keep the coarse delta, sending will fail anyway.
        let theirs = rx.recv().unwrap_or_default();
        Ok(refine::combine(&d, basis_size, &p, &gaps, &ours?, &theirs))
    }

    /// Send a delta: the copy ops up front, then the literal ranges read from `src`. The
    /// copied ranges count for progress at once, as rsync counts matched data.
    fn send_ops(&self, id: u64, file: &File, d: &Delta, src: &dyn ReadAt, xattrs: Option<Vec<Xattr>>) -> io::Result<u64> {
        if d.matched_bytes() == 0 {
            return self.send_whole_file(id, file, xattrs);
        }
        let size = d.size;
        let copies: Vec<Op> = d.ops.iter().copied().filter(|o| matches!(o, Op::Copy { .. })).collect();
        if !self.send(Msg::DeltaBegin { id, size, copies, xattrs }) {
            return Err(io::Error::new(io::ErrorKind::BrokenPipe, "connection closed"));
        }
        crate::progress::bytes(d.matched_bytes());
        for op in &d.ops {
            if let Op::Literal { off, len } = *op {
                self.send_range(src, id, off, len)?;
            }
        }
        self.send(Msg::FileEnd { id, ok: true });
        Ok(size)
    }
}

/// A file listed as small (as `listed`), read whole; `None` if it has grown past the
/// small-file size since (it is then sent like a large file).
fn read_small(path: &Path, listed: &Meta, follow: bool) -> io::Result<Option<Vec<u8>>> {
    use std::io::Read;
    let mut d = Vec::new();
    fsops::open_listed(path, listed, follow)?.take(proto::SMALL_FILE + 1).read_to_end(&mut d)?;
    Ok((d.len() as u64 <= proto::SMALL_FILE).then_some(d))
}

/// `-R`/`--files-from` roots: each base directory with its selected paths.
fn resolve_select_roots(opts: &Options, sources: &[OsString], filter: &Filter, prune: &Prune, out: &Output, errors: &AtomicU64) -> Vec<SrcRoot> {
    let sel_roots = match &opts.files_from {
        Some(list) => select::from_list(&sources[0], list),
        None => select::from_relative_args(sources),
    };
    let mut err = |m: Vec<u8>| {
        errors.fetch_add(1, SeqCst);
        out.error(m);
    };
    let xrules = extra::rules(opts);
    let mut roots = Vec::new();
    for r in sel_roots {
        let sel = select::leak(r.sel);
        let rules = filter.root(&r.base, &r.base, Side::Sender);
        let info = if r.with_root {
            match fsops::stat(&r.base) {
                Ok(mut m) => {
                    adjust_mode(opts, &mut m);
                    let extra = root_extra(opts, &xrules, &r.base, &m, &mut err);
                    RootInfo { contents: true, meta: Some(m), items: Vec::new(), sel: Some(sel.clone()), extra }
                }
                Err(e) => {
                    err(path_msg("link_stat \"", &r.base, format!("\" failed: {e}")));
                    continue;
                }
            }
        } else {
            let (ents, errs) = select::read_selected(&r.base, sel);
            for (p, e) in errs {
                err(path_msg("link_stat \"", &p, format!("\" failed: {e}")));
            }
            let items = adjust_listing(opts, &r.base, ents, &mut err)
                .into_iter()
                .filter(|e| prune::sends(opts, filter, prune, &r.base, b"", &rules, Some(sel), None, e, &err))
                .collect();
            let items = extra::collect(opts, &xrules, &r.base, items, &mut err);
            RootInfo { contents: false, meta: None, items, sel: Some(sel.clone()), extra: None }
        };
        roots.push(SrcRoot { info, base: r.base, sel: Some(sel), rules });
    }
    roots
}

/// Resolve SRC arguments the way rsync does: `src/` sends the directory's contents,
/// `src` sends the directory (or file) itself under its name.
fn resolve_roots(opts: &Options, sources: &[OsString], filter: &Filter, prune: &Prune, out: &Output, errors: &AtomicU64) -> Vec<SrcRoot> {
    if opts.relative {
        return resolve_select_roots(opts, sources, filter, prune, out, errors);
    }
    let xrules = extra::rules(opts);
    let mut err = |m: Vec<u8>| {
        errors.fetch_add(1, SeqCst);
        out.error(m);
    };
    let mut roots = Vec::new();
    for s in sources {
        let sp = PathBuf::from(s);
        match split_source(&sp) {
            (parent, Some(name)) if !wants_contents(s) => {
                let meta = match fsops::lstat(&sp) {
                    Ok(m) => m,
                    Err(e) => {
                        err(path_msg("link_stat \"", &sp, format!("\" failed: {e}")));
                        continue;
                    }
                };
                let link = if meta.kind == Kind::Symlink { fsops::read_link_bytes(&sp).ok() } else { None };
                let items = adjust_listing(opts, &parent, vec![DirEnt::new(name, meta, link)], &mut err);
                // A named directory is the first directory read; its parent is the root.
                let first = if items.iter().any(|e| e.meta.kind == Kind::Dir) { sp.clone() } else { parent.clone() };
                let rules = filter.root(&parent, &first, Side::Sender);
                let items: Vec<DirEnt> = items.into_iter().filter(|e| prune::sends(opts, filter, prune, &parent, b"", &rules, None, None, e, &err)).collect();
                if items.is_empty() {
                    continue;
                }
                let items = extra::collect(opts, &xrules, &parent, items, &mut err);
                let info = RootInfo { contents: false, meta: None, items, sel: None, extra: None };
                roots.push(SrcRoot { info, base: parent, sel: None, rules });
            }
            // Without -r or -d, rsync transfers no directory, not even what a `src/` holds.
            _ if !opts.recursive && !opts.dirs && fsops::stat(&sp).is_ok_and(|m| m.kind == Kind::Dir) => {
                out.note("skipping directory .");
            }
            _ => match fsops::stat(&sp) {
                Ok(mut m) if m.kind == Kind::Dir => {
                    adjust_mode(opts, &mut m);
                    let extra = root_extra(opts, &xrules, &sp, &m, &mut err);
                    let info = RootInfo { contents: true, meta: Some(m), items: Vec::new(), sel: None, extra };
                    let rules = filter.root(&sp, &sp, Side::Sender);
                    roots.push(SrcRoot { info, base: sp, sel: None, rules })
                }
                Ok(_) => err(path_msg("change_dir \"", &sp, "\" failed: Not a directory")),
                Err(e) => err(path_msg("link_stat \"", &sp, format!("\" failed: {e}"))),
            },
        }
    }
    roots
}

/// The transfer root the receiver sees, and its top directories with the named items
/// already listed: several SRC arguments are one root, merged like rsync's file list
/// (see [`merge`]). `None` when no source could be resolved.
fn merge_roots(roots: &[SrcRoot], recursive: bool) -> (Option<RootInfo>, Vec<Top>) {
    let tops: Vec<Top> = roots
        .iter()
        .map(|r| match r.info.meta {
            Some(m) if r.info.contents => (
                SrcDir { path: r.base.clone(), sel: r.sel.and_then(|s| s.at_root(recursive)), dev: Some(m.dev), rules: r.rules.clone() },
                None,
            ),
            _ => (SrcDir { path: r.base.clone(), sel: r.sel, dev: None, rules: r.rules.clone() }, Some(r.info.items.clone())),
        })
        .collect();
    let info = match roots {
        [] => None,
        [r] => Some(r.info.clone()),
        _ => {
            // The destination itself gets the first `src/` directory's attributes.
            let top = roots.iter().find(|r| r.info.contents);
            let items = match top {
                Some(_) => Vec::new(),
                None => {
                    let lists = tops.iter().map(|(_, items)| items.clone().unwrap_or_default()).collect();
                    merge::merge(tops.iter().map(|(d, _)| d.clone()).collect(), lists).0
                }
            };
            let sel = roots.iter().filter_map(|r| r.info.sel.as_ref()).fold(None, |all: Option<Sel>, s| {
                let mut all = all.unwrap_or_default();
                all.merge(s);
                Some(all)
            });
            Some(RootInfo {
                contents: top.is_some(),
                meta: top.and_then(|r| r.info.meta),
                items,
                sel,
                extra: top.and_then(|r| r.info.extra.clone()),
            })
        }
    };
    (info, tops)
}

/// A `src/` root directory's own attributes (values in full) and ACLs.
fn root_extra(opts: &Options, r: &Rules, path: &Path, meta: &Meta, err: &mut dyn FnMut(Vec<u8>)) -> Option<Box<crate::entry::Extra>> {
    extra::read_wire(opts, r, path, Kind::Dir, meta.mode, true)
        .map_err(|e| err(path_msg("get_xattr/get_acl \"", path, format!("\" failed: {e}"))))
        .ok()
        .flatten()
}

/// Run the sending side until the receiver reports completion. Returns the exit code
/// and the receiver's counters (when it got that far). `report_removals`: end with
/// `RemovalsDone` (see [`super::reports_removals`]).
pub fn run(conn: &Conn, opts: &Options, sources: &[OsString], out: &Output, report_removals: bool) -> (i32, Option<StatsSnapshot>) {
    let filter = match Filter::new(&opts.filter_rules, opts.delete_excluded, opts.from0) {
        Ok(f) => f,
        Err(e) => {
            out.error(format!("{e:#}"));
            let _ = conn.msg(Msg::Fatal { msg: format!("{e:#}") });
            return (exit::SYNTAX, None);
        }
    };
    let errors = AtomicU64::new(0);
    if opts.files_from.is_some() && sources.len() != 1 {
        let msg = "--files-from takes exactly one source directory".to_string();
        out.error(&msg);
        let _ = conn.msg(Msg::Fatal { msg });
        return (exit::SYNTAX, None);
    }
    if opts.strategy == Strategy::Adaptive && opts.whole_file.is_none() {
        match adapt::probe(conn, out) {
            Ok(link) => conn.out.set_link(link),
            Err(Stop::Interrupted) => return (exit::SIGNAL, None),
            Err(Stop::Io(e)) => {
                out.error(format!("link probe failed: {e}"));
                return (exit::PROTOCOL, None);
            }
        }
    }
    let prune = Prune::default();
    let roots = resolve_roots(opts, sources, &filter, &prune, out, &errors);
    let sender = Sender {
        out_frames: conn.out.clone(),
        opts,
        out,
        filter,
        listings: Mutex::new(HashMap::new()),
        next_dir: AtomicU32::new(0),
        needs_done: AtomicU32::new(0),
        pending_files: AtomicU32::new(0),
        scan_done: AtomicBool::new(false),
        done_sent: AtomicBool::new(false),
        errors,
        reported: AtomicU64::new(0),
        ids: Mutex::new(ids::Sent::default()),
        removable: Mutex::new(HashMap::new()),
        xrules: extra::rules(opts),
        refining: Mutex::new(HashMap::new()),
        io_error: Mutex::new(false),
        failed: SendFailure::default(),
        vanished: AtomicU64::new(0),
        prune,
    };
    // Every error so far was about reading a source root.
    if sender.errors.load(SeqCst) > 0 {
        *sender.io_error.lock().unwrap() = true;
        sender.send(Msg::IoError);
    }
    sender.name_ids(roots.iter().flat_map(|r| {
        let root = r.info.meta.as_ref().map(|m| (m, r.info.extra.as_deref()));
        root.into_iter().chain(r.info.items.iter().map(|e| (&e.meta, e.extra.as_deref())))
    }));
    // Every root writes into the destination: several are sent as one merged root.
    let (info, tops) = merge_roots(&roots, opts.recursive);
    sender.send_named(Msg::Roots(info.into_iter().collect()), || b"the list of sources".to_vec());

    let pool = match rayon::ThreadPoolBuilder::new()
        .num_threads(opts.threads)
        .thread_name(|i| format!("barqsync-send-{i}"))
        .stack_size(WORKER_STACK)
        .build()
    {
        Ok(p) => p,
        Err(e) => {
            out.error(format!("failed to start worker threads: {e}"));
            return (exit::IPC, None);
        }
    };
    let sender = &sender;
    // Keep blocking protocol coordination on the caller; every configured
    // worker remains available for finite scan, hash and write tasks.
    let result = pool.in_place_scope(|s| {
        s.spawn(move |_| {
            rayon::scope(|ws| sender.walk_top(ws, tops));
            sender.send(Msg::ScanDone { dirs: sender.next_dir.load(SeqCst) });
            sender.scan_done.store(true, SeqCst);
            sender.check_done();
        });
        let r = loop {
            match recv_unless(conn, &sender.failed) {
                Ok(Frame::Msg(Msg::Need { dir, files, xattrs })) => s.spawn(move |s| sender.serve(s, dir, files, xattrs)),
                Ok(Frame::Msg(Msg::Refined { id, gaps })) => {
                    if let Some(tx) = sender.refining.lock().unwrap().remove(&id) {
                        let _ = tx.send(gaps);
                    }
                }
                Ok(Frame::Msg(Msg::Synced { root, files })) if sender.removes() => {
                    s.spawn(move |_| sender.remove_synced(root, files))
                }
                Ok(Frame::Msg(Msg::Synced { .. })) => {
                    out.error("protocol error: source removal without --remove-source-files, or in a dry run");
                    break Err(exit::PROTOCOL);
                }
                Ok(Frame::Msg(Msg::ReceiverDone { stats })) => {
                    crate::timing::mark("ReceiverDone received");
                    if !sender.done_sent.load(SeqCst) {
                        sender.err("protocol error: receiver completed before the sender");
                    }
                    break Ok(stats);
                }
                Ok(Frame::Msg(Msg::Log { stderr, escape, text })) => out.print_forwarded(stderr, escape, &text),
                Ok(Frame::Msg(Msg::Fatal { msg })) => {
                    out.error(format!("remote: {msg}"));
                    break Err(exit::PROTOCOL);
                }
                Ok(other) => {
                    out.error(format!("protocol error: unexpected {other:?}"));
                    break Err(exit::PROTOCOL);
                }
                // After Ctrl-C, which also stops ssh, a closed connection is expected.
                Err(Stop::Interrupted) => break Err(exit::SIGNAL),
                Err(Stop::Io(_)) if fsops::interrupted() => break Err(exit::SIGNAL),
                Err(Stop::Io(_)) if sender.failed.reason().is_some() => {
                    out.error(sender.failed.reason().unwrap_or_default());
                    break Err(exit::PROTOCOL);
                }
                Err(Stop::Io(e)) => {
                    out.error(format!("connection error: {e}"));
                    break Err(exit::PROTOCOL);
                }
            }
        };
        // Release deltas still waiting for an answer that won't come, and tasks waiting to
        // send to a peer that may no longer read.
        sender.refining.lock().unwrap().clear();
        if r.is_err() {
            sender.out_frames.cancel();
        }
        r
    });
    match result {
        Ok(stats) => {
            // The scope has waited for the removals; their output went out before this.
            if report_removals {
                let errors = sender.errors.load(SeqCst) - sender.reported.load(SeqCst);
                sender.send(Msg::RemovalsDone { errors });
                crate::timing::mark("RemovalsDone sent");
            }
            // The receiver's counters include what this side said in `SenderDone`.
            let code = if sender.errors.load(SeqCst) > 0 { exit::PARTIAL } else { stats.exit_code() };
            (code, Some(stats))
        }
        Err(code) => (code, None),
    }
}

//! The sending end of a remote transfer (the client for a push, the server for a pull).
//!
//! Scans the source tree in parallel and streams one `DirBatch` per directory as soon
//! as it is read, while answering the receiver's `Need`s concurrently: small files go
//! out packed many-per-frame, larger ones as `Data` streams, and files the receiver
//! already has an old version of as deltas against the signature it sent.

use super::{Stop, Strategy, WORKER_STACK, adapt, recv};
use crate::cli::Options;
use crate::checksum::{self, Sum};
use crate::delta::refine::{self, GapSig, Grain};
use crate::delta::{self, Delta, Op, Params, ReadAt, ReadOnce, cdc};
use crate::entry::{DirEnt, Kind, Meta};
use crate::ids;
use crate::local::extra;
use crate::local::flags::{adjust_listing, adjust_mode, remove_source};
use crate::xattrs::{Rules, Xattr};
use crate::select::{self, Descend, Sel};
use crate::exit;
use crate::filter::Filter;
use crate::fsops;
use crate::output::{Output, StatsSnapshot, path_msg};
use crate::paths::{join_rel, split_source, wants_contents};
use crate::proto::{self, Conn, Frame, FrameSender, Msg, NeedReq, RootInfo, Sig, SyncedFiles, file_id};
use rayon::Scope;
use std::collections::{HashMap, HashSet};
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
    /// Listings sent and not yet answered: dir id -> (directory path, entries).
    listings: Mutex<HashMap<u32, (PathBuf, Vec<DirEnt>)>>,
    next_dir: AtomicU32,
    needs_done: AtomicU32,
    /// Large-file sends still running (each is its own task).
    pending_files: AtomicU32,
    scan_done: AtomicBool,
    done_sent: AtomicBool,
    errors: AtomicU64,
    /// Ids already named to the receiver (no `--numeric-ids`).
    ids: Mutex<ids::Sent>,
    /// Sender-owned deletion authority. Peer-supplied paths and timestamps are
    /// never used to resolve a file or decide whether it is unchanged.
    removals: Mutex<RemovalLedger>,
    /// `-X`: which attribute names are sent.
    xrules: Rules,
    /// Deltas waiting for the receiver's answer to a `Refine`: file id -> where it goes.
    refining: Mutex<HashMap<u64, crossbeam_channel::Sender<Vec<GapSig>>>>,
}

#[derive(Default)]
struct RemovalLedger {
    entries: HashMap<(u16, Vec<u8>), (PathBuf, Meta)>,
    acknowledged: HashSet<(u16, Vec<u8>)>,
}

/// A source argument resolved on disk.
struct SrcRoot {
    info: RootInfo,
    /// The `src/` directory itself, or the directory holding the named items.
    base: PathBuf,
    /// `-R`/`--files-from`: the selected paths below `base`.
    sel: Option<&'static Sel>,
}

impl Sender<'_> {
    fn err(&self, msg: impl AsRef<[u8]>) {
        self.errors.fetch_add(1, SeqCst);
        self.out.error(msg);
    }

    fn send(&self, m: Msg) {
        if let Err(e) = self.out_frames.msg(m) {
            self.err(format!("sending protocol message failed: {e}"));
        }
    }

    /// Register a listing and send it. Registered first, so a quick `Need` finds it.
    fn send_listing(&self, root: u16, rel: Vec<u8>, path: PathBuf, entries: Vec<DirEnt>) {
        let sums = if self.opts.checksum { self.checksums(&path, &entries) } else { Vec::new() };
        let dir = self.next_dir.fetch_add(1, SeqCst);
        if self.opts.remove_source_files && !self.opts.dry_run {
            let mut ledger = self.removals.lock().unwrap();
            for e in entries.iter().filter(|e| e.meta.kind != Kind::Dir) {
                ledger.entries.insert((root, join_rel(&rel, &e.name)), (path.join(&e.name), e.meta));
            }
        }
        self.listings.lock().unwrap().insert(dir, (path, entries.clone()));
        self.name_ids(entries.iter().map(|e| (&e.meta, e.extra.as_deref())));
        self.send(Msg::DirBatch { root, dir, rel, entries, sums });
    }

    /// `-c`: checksums of the listing's regular files, hashed in parallel.
    fn checksums(&self, dir: &Path, entries: &[DirEnt]) -> Vec<Option<Sum>> {
        let files: Vec<PathBuf> =
            entries.iter().filter(|e| e.meta.kind == Kind::File).map(|e| dir.join(&e.name)).collect();
        let mut sums = checksum::sums(files).into_iter();
        entries
            .iter()
            .map(|e| if e.meta.kind == Kind::File { sums.next().and_then(Result::ok) } else { None })
            .collect()
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
        let ents = adjust_listing(self.opts, dir, ents, &mut |m| self.err(m));
        extra::collect(self.opts, &self.xrules, dir, ents, &mut |m| self.err(m))
    }

    fn walk_dir<'s>(&'s self, s: &Scope<'s>, root: u16, rel: Vec<u8>, path: PathBuf, root_dev: u64, sel: Option<&'static Sel>) {
        if fsops::interrupted() {
            return;
        }
        let ents = match sel {
            Some(sel) if !sel.contents => self.read_selected(&path, sel),
            _ => match fsops::read_dir_meta(&path) {
                Ok(v) => self.listing(&path, v),
                Err(e) => return self.err(path_msg("opendir \"", &path, format!("\" failed: {e}"))),
            },
        };
        let ents: Vec<DirEnt> = ents
            .into_iter()
            .filter(|e| !self.filter.is_excluded(&join_rel(&rel, &e.name), e.meta.kind == Kind::Dir))
            .collect();
        let subdirs: Vec<(Vec<u8>, PathBuf, Option<&'static Sel>)> = ents
            .iter()
            .filter_map(|e| {
                let sub = self.descend_into(sel, e, root_dev)?;
                Some((join_rel(&rel, &e.name), path.join(&e.name), sub))
            })
            .collect();
        self.send_listing(root, rel, path, ents);
        for (crel, cpath, sub) in subdirs {
            s.spawn(move |s| self.walk_dir(s, root, crel, cpath, root_dev, sub));
        }
    }

    /// The selected entries of `dir`, with missing names reported as errors.
    fn read_selected(&self, dir: &Path, sel: &Sel) -> Vec<DirEnt> {
        let (ents, errs) = select::read_selected(dir, sel);
        for (p, e) in errs {
            self.err(path_msg("link_stat \"", &p, format!("\" failed: {e}")));
        }
        self.listing(dir, ents)
    }

    /// Whether the listing of subdirectory `e` is sent (and with which selection). The
    /// receiver makes the same decision with [`select::descend`].
    fn descend_into(&self, sel: Option<&'static Sel>, e: &DirEnt, root_dev: u64) -> Option<Option<&'static Sel>> {
        if e.meta.kind != Kind::Dir || (self.opts.one_file_system && e.meta.dev != root_dev) {
            return None;
        }
        match select::descend(sel, e.name.as_bytes(), self.opts.recursive) {
            Descend::No => None,
            Descend::All => Some(None),
            Descend::Some(s) => Some(Some(s)),
        }
    }

    fn walk_roots<'s>(&'s self, s: &Scope<'s>, roots: &'s [SrcRoot]) {
        for (r, root) in roots.iter().enumerate() {
            let r = r as u16;
            if root.info.contents {
                let dev = root.info.meta.map(|m| m.dev).unwrap_or(0);
                let sel = root.sel.and_then(|sel| sel.at_root(self.opts.recursive));
                s.spawn(move |s| self.walk_dir(s, r, Vec::new(), root.base.clone(), dev, sel));
            } else {
                // The named items form a virtual top-level listing.
                for item in &root.info.items {
                    if let Some(sub) = self.descend_into(root.sel, item, item.meta.dev) {
                        let rel = item.name.as_bytes().to_vec();
                        let path = root.base.join(&item.name);
                        let dev = item.meta.dev;
                        s.spawn(move |s| self.walk_dir(s, r, rel, path, dev, sub));
                    }
                }
                self.send_listing(r, Vec::new(), root.base.clone(), root.info.items.clone());
            }
        }
    }

    /// Record acknowledgements only for entries this sender actually listed.
    /// Deletion waits for successful session completion so an acknowledgement
    /// cannot race outstanding data or deferred metadata work.
    fn remove_synced(&self, root: u16, files: SyncedFiles) {
        if !self.opts.remove_source_files || self.opts.dry_run {
            return self.err("protocol error: source removal is not authorized");
        }
        let mut ledger = self.removals.lock().unwrap();
        for (rel, _, _) in files {
            let key = (root, rel);
            if !ledger.entries.contains_key(&key) {
                self.err("protocol error: source acknowledgement for an unlisted entry");
            } else if !ledger.acknowledged.insert(key) {
                self.err("protocol error: duplicate source acknowledgement");
            }
        }
    }

    fn finish_removals(&self) {
        let ledger = std::mem::take(&mut *self.removals.lock().unwrap());
        for key in ledger.acknowledged {
            let (path, meta) = &ledger.entries[&key];
            match remove_source(self.opts, path, meta) {
                Ok(()) if self.out.verbose() >= 2 => self.out.info([&b"sender removed "[..], &key.1].concat()),
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
            if let Err(e) = self.out_frames.end_data_streams() {
                self.err(format!("closing data streams failed: {e}"));
            }
            self.send(Msg::SenderDone { errors: self.errors.load(SeqCst) });
        }
    }

    // -----------------------------------------------------------------------
    // Serving requests
    // -----------------------------------------------------------------------

    fn serve<'s>(&'s self, s: &Scope<'s>, dir: u32, reqs: Vec<NeedReq>, xattrs: Vec<u32>) {
        let listing = self.listings.lock().unwrap().remove(&dir);
        match listing {
            Some((base, entries)) => {
                self.send_xattrs(dir, &base, &entries, xattrs);
                self.serve_listing(s, dir, &base, &entries, reqs)
            }
            None => self.err(format!("protocol error: request for unknown directory {dir}")),
        }
        self.needs_done.fetch_add(1, SeqCst);
        self.check_done();
    }

    /// `-X`: the attributes of these entries in full, for the receiver to update them.
    fn send_xattrs(&self, dir: u32, base: &Path, entries: &[DirEnt], idxs: Vec<u32>) {
        for idx in idxs {
            let Some(e) = entries.get(idx as usize) else {
                self.err(format!("protocol error: request for unknown entry {dir}/{idx}"));
                continue;
            };
            let xattrs = self.full_xattrs(&base.join(&e.name));
            self.send(Msg::Xattrs { id: file_id(dir, idx), xattrs });
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
    fn serve_listing<'s>(&'s self, s: &Scope<'s>, dir: u32, base: &Path, entries: &[DirEnt], reqs: Vec<NeedReq>) {
        let mut small = Vec::new();
        let mut large = Vec::new();
        for req in reqs {
            let Some(e) = entries.get(req.idx as usize) else {
                self.err(format!("protocol error: request for unknown file {dir}/{}", req.idx));
                continue;
            };
            let id = file_id(dir, req.idx);
            let path = base.join(&e.name);
            // Values the listing only had checksums of go along with the data.
            let full = extra::has_summed(e.extra.as_deref());
            crate::progress::planned(e.meta.size);
            if req.sig.is_none() && e.meta.size <= proto::SMALL_FILE {
                small.push((id, path, full));
            } else {
                large.push((id, path, req.sig, full));
            }
        }
        if !small.is_empty() {
            self.send_small(small);
        }
        for (id, path, sig, full) in large {
            self.pending_files.fetch_add(1, SeqCst);
            s.spawn(move |_| {
                if !fsops::interrupted() {
                    let size = std::fs::metadata(&path).map(|m| m.len()).unwrap_or(0);
                    let xattrs = if full { self.full_xattrs(&path) } else { None };
                    let r = match sig {
                        Some(sig) => self.send_delta(id, &path, sig, xattrs),
                        None => self.send_whole(id, &path, xattrs),
                    };
                    match r {
                        // Sent bytes were counted as they went.
                        Ok(sent) => crate::progress::done(size.saturating_sub(sent)),
                        // A failed file doesn't count as transferred; after Ctrl-C the
                        // closed connection is expected, not worth a message.
                        Err(e) => {
                            if !fsops::interrupted() {
                                self.err(path_msg("send_files failed to read \"", &path, format!("\": {e}")));
                            }
                            self.send(Msg::FileEnd { id, ok: false });
                        }
                    }
                }
                self.pending_files.fetch_sub(1, SeqCst);
                self.check_done();
            });
        }
    }

    /// Read small files in parallel and send them packed into frames of ~4 MiB.
    /// `full`: the file's attribute values go along in full.
    fn send_small(&self, files: Vec<(u64, PathBuf, bool)>) {
        let read = files.into_iter().map(|(id, path, full)| (id, std::fs::read(&path), path, full));
        let mut pack: Vec<(u64, u32)> = Vec::new();
        let mut blob: Vec<u8> = Vec::new();
        let mut xattrs: Vec<(u64, Vec<Xattr>)> = Vec::new();
        for (id, data, path, full) in read {
            match data {
                Ok(d) => {
                    crate::progress::done(d.len() as u64);
                    if !blob.is_empty() && blob.len() + d.len() > proto::PACK_BYTES {
                        self.send(Msg::SmallPack {
                            files: std::mem::take(&mut pack),
                            blob: std::mem::take(&mut blob),
                            xattrs: std::mem::take(&mut xattrs),
                        });
                    }
                    if full && let Some(x) = self.full_xattrs(&path) {
                        xattrs.push((id, x));
                    }
                    pack.push((id, d.len() as u32));
                    blob.extend_from_slice(&d);
                }
                Err(e) => {
                    self.err(path_msg("send_files failed to open \"", &path, format!("\": {e}")));
                    self.send(Msg::FileEnd { id, ok: false });
                }
            }
        }
        if !pack.is_empty() {
            self.send(Msg::SmallPack { files: pack, blob, xattrs });
        }
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
    fn send_whole(&self, id: u64, path: &Path, xattrs: Option<Vec<Xattr>>) -> io::Result<u64> {
        let f = File::open(path)?;
        self.send_whole_file(id, &f, xattrs)
    }

    fn send_whole_file(&self, id: u64, f: &File, xattrs: Option<Vec<Xattr>>) -> io::Result<u64> {
        let size = f.metadata()?.len();
        self.send(Msg::FileBegin { id, size, xattrs });
        self.send_range(&ReadOnce::new(f, size), id, 0, size)?;
        self.send(Msg::FileEnd { id, ok: true });
        Ok(size)
    }

    /// Diff the source file against the receiver's basis signature and send only the
    /// ranges it doesn't have.
    fn send_delta(&self, id: u64, path: &Path, sig: Sig, xattrs: Option<Vec<Xattr>>) -> io::Result<u64> {
        let file = File::open(path)?;
        let size = file.metadata()?.len();
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
                if bs == 0 {
                    return self.send_whole_file(id, &file, xattrs);
                }
                let p = Params { block_size: bs, segment: (Params::DEFAULT.segment / bs).max(1) * bs, ..Params::DEFAULT };
                (delta::diff_blocks(&delta::block_signature(&f, size, &p)?, &basis), basis.size, bs)
            }
            Sig::Chunks(basis) => {
                let Some(p) = basis.params(Params::DEFAULT) else { return self.send_whole_file(id, &file, xattrs) };
                (delta::diff_chunks(&delta::chunk_signature(&f, size, &p)?, &basis), basis.size, p.cdc_avg)
            }
            Sig::Cdc(_) => unreachable!(),
        };
        let d = self.refine(id, &f, size, basis_size, unit, d)?;
        self.send_ops(id, &file, &d, &f, xattrs)
    }

    /// Refine a coarse delta where it is worth it ([`refine::plan`]): ask the receiver for
    /// fine signatures of the basis stretches between matched ranges, hash this file's side
    /// meanwhile, then combine. One round trip, which an append (nothing to refine) skips.
    fn refine(&self, id: u64, new: &dyn ReadAt, size: u64, basis_size: u64, unit: usize, d: Delta) -> io::Result<Delta> {
        let p = Params::for_size(basis_size);
        let Some(gaps) = refine::plan(&d, basis_size, unit, &p, refine::REFINE_MIN) else { return Ok(d) };
        let (tx, rx) = crossbeam_channel::bounded(1);
        self.refining.lock().unwrap().insert(id, tx);
        self.send(Msg::Refine { id, grain: Grain::of(&p), gaps: gaps.iter().map(refine::Gap::request).collect() });
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
        self.send(Msg::DeltaBegin { id, size, copies, xattrs });
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

/// `-R`/`--files-from` roots: each base directory with its selected paths.
fn resolve_select_roots(opts: &Options, sources: &[OsString], filter: &Filter, out: &Output, errors: &AtomicU64) -> Vec<SrcRoot> {
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
                .filter(|e| !filter.is_excluded(e.name.as_bytes(), e.meta.kind == Kind::Dir))
                .collect();
            let items = extra::collect(opts, &xrules, &r.base, items, &mut err);
            RootInfo { contents: false, meta: None, items, sel: Some(sel.clone()), extra: None }
        };
        roots.push(SrcRoot { info, base: r.base, sel: Some(sel) });
    }
    roots
}

/// Resolve SRC arguments the way rsync does: `src/` sends the directory's contents,
/// `src` sends the directory (or file) itself under its name.
fn resolve_roots(opts: &Options, sources: &[OsString], filter: &Filter, out: &Output, errors: &AtomicU64) -> Vec<SrcRoot> {
    if opts.relative {
        return resolve_select_roots(opts, sources, filter, out, errors);
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
                if filter.is_excluded(name.as_bytes(), meta.kind == Kind::Dir) {
                    continue;
                }
                let link = if meta.kind == Kind::Symlink { fsops::read_link_bytes(&sp).ok() } else { None };
                let items = adjust_listing(opts, &parent, vec![DirEnt::new(name, meta, link)], &mut err);
                let items = extra::collect(opts, &xrules, &parent, items, &mut err);
                let info = RootInfo { contents: false, meta: None, items, sel: None, extra: None };
                roots.push(SrcRoot { info, base: parent, sel: None });
            }
            _ => match fsops::stat(&sp) {
                Ok(mut m) if m.kind == Kind::Dir => {
                    adjust_mode(opts, &mut m);
                    let extra = root_extra(opts, &xrules, &sp, &m, &mut err);
                    let info = RootInfo { contents: true, meta: Some(m), items: Vec::new(), sel: None, extra };
                    roots.push(SrcRoot { info, base: sp, sel: None })
                }
                Ok(_) => err(path_msg("change_dir \"", &sp, "\" failed: Not a directory")),
                Err(e) => err(path_msg("link_stat \"", &sp, format!("\" failed: {e}"))),
            },
        }
    }
    roots
}

/// A `src/` root directory's own attributes (values in full) and ACLs.
fn root_extra(opts: &Options, r: &Rules, path: &Path, meta: &Meta, err: &mut dyn FnMut(Vec<u8>)) -> Option<Box<crate::entry::Extra>> {
    extra::read_wire(opts, r, path, Kind::Dir, meta.mode, true)
        .map_err(|e| err(path_msg("get_xattr/get_acl \"", path, format!("\" failed: {e}"))))
        .ok()
        .flatten()
}

/// Run the sending side until the receiver reports completion. Returns the exit code
/// and the receiver's counters (when it got that far).
pub fn run(conn: &Conn, opts: &Options, sources: &[OsString], out: &Output) -> (i32, Option<StatsSnapshot>) {
    let filter = match Filter::new(&opts.filter_rules) {
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
    let roots = resolve_roots(opts, sources, &filter, out, &errors);
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
        ids: Mutex::new(ids::Sent::default()),
        removals: Mutex::new(RemovalLedger::default()),
        xrules: extra::rules(opts),
        refining: Mutex::new(HashMap::new()),
    };
    sender.name_ids(roots.iter().flat_map(|r| {
        let root = r.info.meta.as_ref().map(|m| (m, r.info.extra.as_deref()));
        root.into_iter().chain(r.info.items.iter().map(|e| (&e.meta, e.extra.as_deref())))
    }));
    sender.send(Msg::Roots(roots.iter().map(|r| r.info.clone()).collect()));

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
    let roots = &roots;
    // Keep blocking protocol coordination on the caller; every configured
    // worker remains available for finite scan, hash and write tasks.
    let result = pool.in_place_scope(|s| {
        s.spawn(move |_| {
            rayon::scope(|ws| sender.walk_roots(ws, roots));
            sender.send(Msg::ScanDone { dirs: sender.next_dir.load(SeqCst) });
            sender.scan_done.store(true, SeqCst);
            sender.check_done();
        });
        let r = loop {
            match recv(conn) {
                Ok(Frame::Msg(Msg::Need { dir, files, xattrs })) => s.spawn(move |s| sender.serve(s, dir, files, xattrs)),
                Ok(Frame::Msg(Msg::Refined { id, gaps })) => {
                    if let Some(tx) = sender.refining.lock().unwrap().remove(&id) {
                        let _ = tx.send(gaps);
                    }
                }
                Ok(Frame::Msg(Msg::Synced { root, files })) => sender.remove_synced(root, files),
                Ok(Frame::Msg(Msg::ReceiverDone { stats })) => {
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
                Err(Stop::Io(e)) => {
                    out.error(format!("connection error: {e}"));
                    break Err(exit::PROTOCOL);
                }
            }
        };
        // Release deltas still waiting for an answer that won't come.
        sender.refining.lock().unwrap().clear();
        r
    });
    match result {
        Ok(stats) => {
            // Fail closed: a partial transfer leaves every source available for
            // retry. The peer's `(size, mtime)` fields carry no deletion authority.
            if stats.errors() == 0 && sender.errors.load(SeqCst) == 0 {
                sender.finish_removals();
            }
            let failed = stats.errors() > 0 || sender.errors.load(SeqCst) > 0;
            (if failed { exit::PARTIAL } else { exit::OK }, Some(stats))
        }
        Err(code) => (code, None),
    }
}

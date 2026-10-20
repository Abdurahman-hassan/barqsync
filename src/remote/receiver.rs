//! The receiving end of a remote transfer (the client for a pull, the server for a push).
//!
//! The same [`Engine`] as a local sync does the work: listings arrive as `DirBatch`
//! frames instead of from `readdir`, and changed files are requested with a `Need`
//! frame instead of being copied. A directory is processed once both its listing has
//! arrived and its parent has created it (see [`Slot`]); file data is assembled from
//! `SmallPack`, `FileBegin`/`DeltaBegin` + `Data` + `FileEnd` frames.

use super::{MAX_PACKS_IN_FLIGHT, Stop, Strategy, WORKER_STACK, adapt, recv};
use crate::cli::Options;
use crate::checksum::Sum;
use crate::delta::refine::{self, GapReq, Grain};
use crate::delta::{self, Delta, DeltaMode, Op, ReadAt, ReadOnce, apply::apply, cdc};
use crate::entry::{DirEnt, Kind, Meta};
use crate::ids::IdMap;
use crate::local::extra;
use crate::local::flags::write_sparse_at;
use crate::xattrs::Xattr;
use crate::select::{self, Sel};
use crate::exit;
use crate::filter::Filter;
use crate::fsops::{self, Copied, cache};
use crate::local::exec::{DirJob, Engine, FileJob, finish_temp, split_dst};
use crate::output::{Output, Stats, StatsSnapshot, path_msg};
use crate::paths::{split_source, wants_contents};
use crate::proto::{Conn, Frame, FrameSender, Msg, NeedReq, RootInfo, Sig, SyncedFiles, file_id};
use crate::scan::{DstIndex, Scanner};
use std::sync::Arc;
use rayon::Scope;
use std::collections::HashMap;
use std::ffi::{OsStr, OsString};
use std::fs::{self, File};
use std::io;
use std::os::unix::fs::FileExt;
use std::path::{Path, PathBuf};
use std::sync::{Condvar, Mutex};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering::SeqCst};

/// Rendezvous between a directory's listing and the receiver being ready for it.
enum Slot {
    /// Ready (created by its parent), waiting for the listing.
    Job(DirJob),
    /// A `src` root (no trailing slash): named items synced into `dst_dir`.
    Items { dst_dir: PathBuf, rename: Option<OsString>, dst_is_new: bool, sel: Option<&'static Sel> },
    /// The listing arrived first.
    Batch { dir: u32, entries: Vec<DirEnt> },
}

/// A file whose data is arriving.
struct Incoming {
    file: File,
    /// Where the data is written: a temp file, or the destination itself (`--inplace`).
    path: PathBuf,
    /// `path` is a temp file to rename over the destination when complete.
    rename: bool,
    /// `path` is in the interrupt-cleanup registry (temp files and new `--inplace` files).
    registered: bool,
    /// `path` was created by us with the target mode (as opposed to an existing file or a clone).
    created: bool,
    delta: bool,
    size: u64,
    job: FileJob,
    /// Page-cache hints for the incoming data.
    hint: cache::Writer,
    /// Data not written yet: offset and contiguous bytes (see [`Incoming::write`]).
    held: (u64, Vec<u8>),
    /// How much of the file, from offset 0, holds received data (kept by `--partial`).
    prefix: Prefix,
    /// Bytes already counted for progress: copied ranges, and data as it arrives.
    counted: u64,
}

impl Incoming {
    /// Write one piece of incoming data. Big files on macOS are written uncached, which
    /// is synchronous and would stall the receive loop once per frame, so their
    /// contiguous frames are collected into writes of `cache::WRITE_UNIT` bytes.
    fn write(&mut self, off: u64, bytes: &[u8]) -> io::Result<()> {
        if !(cache::SYNC_WRITES && self.hint.is_on()) {
            self.file.write_all_at(bytes, off)?;
            self.hint.written_to(&self.file, off + bytes.len() as u64);
            return Ok(());
        }
        let held = self.held.1.len();
        if held > 0 && (self.held.0 + held as u64 != off || held + bytes.len() > cache::WRITE_UNIT) {
            self.flush()?;
        }
        if self.held.1.is_empty() {
            self.held.0 = off;
            self.held.1.reserve(cache::WRITE_UNIT);
        }
        self.held.1.extend_from_slice(bytes);
        Ok(())
    }

    fn flush(&mut self) -> io::Result<()> {
        let (off, buf) = &mut self.held;
        if !buf.is_empty() {
            self.file.write_all_at(buf, *off)?;
            self.hint.written_to(&self.file, *off + buf.len() as u64);
            buf.clear();
        }
        Ok(())
    }
}

/// Tracks the contiguous received prefix of a file: copy ranges are in place from the
/// start, literal data arrives in order.
#[derive(Default)]
struct Prefix {
    done: u64,
    /// Copy ranges `(off, len)` not yet reached, in ascending order.
    copies: std::collections::VecDeque<(u64, u64)>,
}

impl Prefix {
    fn with_copies(ops: &[Op]) -> Prefix {
        let mut copies: Vec<(u64, u64)> = ops.iter().map(|o| (o.off(), o.len())).collect();
        copies.sort_unstable();
        let mut p = Prefix { done: 0, copies: copies.into() };
        p.advance();
        p
    }

    fn advance(&mut self) {
        while let Some(&(off, len)) = self.copies.front()
            && off <= self.done
        {
            self.done = self.done.max(off + len);
            self.copies.pop_front();
        }
    }

    fn data(&mut self, off: u64, len: u64) {
        if off <= self.done {
            self.done = self.done.max(off + len);
            self.advance();
        }
    }
}

pub struct RemoteState {
    out: FrameSender,
    slots: Mutex<HashMap<(u16, Vec<u8>), Slot>>,
    needed: Mutex<HashMap<u64, FileJob>>,
    incoming: Mutex<HashMap<u64, Incoming>>,
    /// Listing tasks spawned and not yet finished.
    inflight: AtomicUsize,
    scan_done: AtomicBool,
    /// Ask for deltas of changed files (rsync's remote default; off with `-W`).
    delta: bool,
    /// Adaptive strategy: the link the sender measured; files it says are better sent
    /// whole are requested without a signature.
    link: Option<adapt::LinkInfo>,
    /// Small-file packs queued or being written (bounded, see `MAX_PACKS_IN_FLIGHT`).
    packs: Mutex<usize>,
    pack_done: Condvar,
    /// `-c`: the sender's checksums, per listing.
    sums: Mutex<HashMap<u32, Vec<Option<Sum>>>>,
    /// Sender uid/gid -> local uid/gid, by name (see [`crate::ids`]).
    ids: Mutex<IdMap>,
    /// `--remove-source-files`: entries to report back to the sender, per root.
    synced: Mutex<HashMap<u16, SyncedFiles>>,
    /// `-X`: entries waiting for their attribute values (by file id), and the requests
    /// for them not sent yet (by listing; they go out with the listing's `Need`).
    xattr_wait: Mutex<HashMap<u64, XattrWait>>,
    xattr_reqs: Mutex<HashMap<u32, Vec<u32>>>,
    /// `--remove-source-files`: entries (by root and name) whose source must stay until
    /// their deferred attribute values are written.
    held: Mutex<HashMap<(u16, Vec<u8>), Held>>,
}

/// An entry waiting for its attribute values.
struct XattrWait {
    path: PathBuf,
    kind: Kind,
    /// Its key in `held`.
    hold: Option<(u16, Vec<u8>)>,
}

/// Where a held entry is at.
enum Held {
    /// Its values haven't been written yet.
    Waiting,
    /// The rest of it is done too: report it once the values are written.
    Synced(u64, i64),
    /// Writing the values failed: its source stays.
    Failed,
}

/// Literal ranges arrive as `Data` frames, never through [`apply`].
struct NoLiteral;

impl ReadAt for NoLiteral {
    fn read_exact_at(&self, _: &mut [u8], _: u64) -> io::Result<()> {
        Err(io::Error::other("literal data comes from the wire"))
    }
}

impl RemoteState {
    fn new(out: FrameSender, opts: &Options, ids: IdMap) -> RemoteState {
        RemoteState {
            out,
            slots: Mutex::new(HashMap::new()),
            needed: Mutex::new(HashMap::new()),
            incoming: Mutex::new(HashMap::new()),
            inflight: AtomicUsize::new(0),
            scan_done: AtomicBool::new(false),
            delta: opts.whole_file != Some(true),
            link: None,
            packs: Mutex::new(0),
            pack_done: Condvar::new(),
            sums: Mutex::new(HashMap::new()),
            ids: Mutex::new(ids),
            synced: Mutex::new(HashMap::new()),
            xattr_wait: Mutex::new(HashMap::new()),
            xattr_reqs: Mutex::new(HashMap::new()),
            held: Mutex::new(HashMap::new()),
        }
    }

    /// `-X`: entry `idx` of listing `dir` (at `path`) needs attribute values only the
    /// sender has in full: ask for them with the listing's `Need`. `hold` (`root`, `rel`):
    /// with `--remove-source-files`, its source stays until they are written.
    pub(crate) fn defer_xattrs(&self, dir: u32, idx: u32, path: &Path, kind: Kind, hold: Option<(u16, &[u8])>) {
        let hold = hold.map(|(root, rel)| (root, rel.to_vec()));
        if let Some(k) = &hold {
            self.held.lock().unwrap().insert(k.clone(), Held::Waiting);
        }
        self.xattr_wait.lock().unwrap().insert(file_id(dir, idx), XattrWait { path: path.to_path_buf(), kind, hold });
        self.xattr_reqs.lock().unwrap().entry(dir).or_default().push(idx);
    }

    /// Withdraw a deferral whose entry wasn't written after all (while its request is
    /// still unsent; afterwards the values are expected).
    pub(crate) fn forget_xattrs(&self, dir: u32, idx: u32) {
        {
            let mut reqs = self.xattr_reqs.lock().unwrap();
            let Some(v) = reqs.get_mut(&dir) else { return };
            let Some(i) = v.iter().position(|&x| x == idx) else { return };
            v.remove(i);
        }
        if let Some(XattrWait { hold: Some(k), .. }) = self.xattr_wait.lock().unwrap().remove(&file_id(dir, idx)) {
            self.held.lock().unwrap().remove(&k);
        }
    }

    /// The attribute values asked for with [`RemoteState::defer_xattrs`] arrived.
    fn on_xattrs(&self, eng: &Engine, id: u64, xattrs: Option<Vec<Xattr>>) {
        let Some(XattrWait { path, kind, hold }) = self.xattr_wait.lock().unwrap().remove(&id) else {
            return eng.err(format!("protocol error: attributes for unrequested entry {id:#x}"));
        };
        // `None`: the sender couldn't read them and has said so.
        let written = xattrs.is_some_and(|xattrs| {
            let want = eng.xrules.to_local(xattrs);
            let r = crate::xattrs::read_local(&path, &eng.xrules)
                .and_then(|have| crate::xattrs::apply(&path, kind, &want, &have, None, &eng.xrules));
            r.map_err(|e| eng.err(path_msg("set_xattr \"", &path, format!("\" failed: {e}")))).is_ok()
        });
        if let Some(k) = hold {
            let mut held = self.held.lock().unwrap();
            match held.remove(&k) {
                Some(Held::Synced(size, mtime)) if written => {
                    drop(held);
                    self.push_synced(k.0, k.1, size, mtime);
                }
                Some(Held::Waiting) if !written => {
                    held.insert(k, Held::Failed);
                }
                _ => {}
            }
        }
    }


    pub(crate) fn src_sum(&self, dir: u32, idx: u32) -> Option<Sum> {
        self.sums.lock().unwrap().get(&dir)?.get(idx as usize).copied().flatten()
    }

    /// `--remove-source-files`: the receiver has this entry; tell the sender (in batches),
    /// or, while it waits for attribute values, once they are written.
    pub(crate) fn synced(&self, root: u16, rel: &[u8], meta: &Meta) {
        {
            let mut held = self.held.lock().unwrap();
            if !held.is_empty() {
                let k = (root, rel.to_vec());
                match held.get_mut(&k) {
                    Some(Held::Failed) => {
                        held.remove(&k);
                        return;
                    }
                    Some(h) => {
                        *h = Held::Synced(meta.size, meta.mtime);
                        return;
                    }
                    None => {}
                }
            }
        }
        self.push_synced(root, rel.to_vec(), meta.size, meta.mtime);
    }

    fn push_synced(&self, root: u16, rel: Vec<u8>, size: u64, mtime: i64) {
        let mut map = self.synced.lock().unwrap();
        let v = map.entry(root).or_default();
        v.push((rel, size, mtime));
        if v.len() >= 1000 {
            let files = std::mem::take(v);
            drop(map);
            let _ = self.out.msg(Msg::Synced { root, files });
        }
    }

    fn flush_synced(&self) {
        for (root, files) in self.synced.lock().unwrap().drain() {
            if !files.is_empty() {
                let _ = self.out.msg(Msg::Synced { root, files });
            }
        }
    }

    /// Wait for room to queue another small-file pack.
    fn acquire_pack(&self) {
        let mut n = self.packs.lock().unwrap();
        while *n >= MAX_PACKS_IN_FLIGHT {
            n = self.pack_done.wait(n).unwrap();
        }
        *n += 1;
    }

    fn release_pack(&self) {
        *self.packs.lock().unwrap() -= 1;
        self.pack_done.notify_one();
    }

    // -----------------------------------------------------------------------
    // Listings
    // -----------------------------------------------------------------------

    /// Called by the engine for each subdirectory it created or found.
    pub(crate) fn expect_dir<'s, 'a: 's>(&self, s: &Scope<'s>, eng: &'s Engine<'a>, mut job: DirJob) {
        let key = (job.root, job.rel.clone());
        let mut slots = self.slots.lock().unwrap();
        match slots.remove(&key) {
            Some(Slot::Batch { dir, entries }) => {
                drop(slots);
                job.wire_dir = dir;
                self.spawn_listing(s, eng, job, entries);
            }
            _ => {
                slots.insert(key, Slot::Job(job));
            }
        }
    }

    #[allow(clippy::too_many_arguments)]
    fn on_batch<'s, 'a: 's>(
        &self,
        s: &Scope<'s>,
        eng: &'s Engine<'a>,
        root: u16,
        dir: u32,
        rel: Vec<u8>,
        mut entries: Vec<DirEnt>,
        sums: Vec<Option<Sum>>,
    ) {
        if !sums.is_empty() {
            self.sums.lock().unwrap().insert(dir, sums);
        }
        {
            let ids = self.ids.lock().unwrap();
            if !ids.is_empty() {
                entries.iter_mut().for_each(|e| ids.apply(&mut e.meta));
            }
        }
        if eng.extras_on() {
            let ids = self.ids.lock().unwrap();
            for e in &mut entries {
                e.extra = local_extra(&ids, &eng.xrules, e.extra.take(), e.meta.mode);
            }
        }
        let key = (root, rel);
        let mut slots = self.slots.lock().unwrap();
        match slots.remove(&key) {
            Some(Slot::Job(mut job)) => {
                drop(slots);
                job.wire_dir = dir;
                self.spawn_listing(s, eng, job, entries);
            }
            Some(Slot::Items { dst_dir, rename, dst_is_new, sel }) => {
                drop(slots);
                self.inflight.fetch_add(1, SeqCst);
                s.spawn(move |s| {
                    let entries = entries.into_iter().enumerate().map(|(i, e)| (i as u32, e, rename.clone())).collect();
                    eng.run_items(s, Path::new(""), &dst_dir, root, dir, entries, dst_is_new, sel);
                    let r = eng.remote.as_ref().unwrap();
                    r.sums.lock().unwrap().remove(&dir);
                    r.task_done();
                });
            }
            _ => {
                slots.insert(key, Slot::Batch { dir, entries });
            }
        }
    }

    fn spawn_listing<'s, 'a: 's>(&self, s: &Scope<'s>, eng: &'s Engine<'a>, job: DirJob, entries: Vec<DirEnt>) {
        self.inflight.fetch_add(1, SeqCst);
        s.spawn(move |s| {
            let dir = job.wire_dir;
            let entries = entries.into_iter().enumerate().map(|(i, e)| (i as u32, e, None)).collect();
            eng.process_listing(s, job, entries);
            let r = eng.remote.as_ref().unwrap();
            r.sums.lock().unwrap().remove(&dir);
            r.task_done();
        });
    }

    fn task_done(&self) {
        if self.inflight.fetch_sub(1, SeqCst) == 1 {
            self.flush_orphans();
        }
    }

    fn on_scan_done(&self) {
        self.scan_done.store(true, SeqCst);
        self.flush_orphans();
    }

    /// Once every listing has arrived and no directory task is running, listings that
    /// were never claimed (their parent failed or was skipped) will never be processed.
    /// Answer them with empty `Need`s so the sender knows it is done.
    fn flush_orphans(&self) {
        if !self.scan_done.load(SeqCst) || self.inflight.load(SeqCst) != 0 {
            return;
        }
        let mut slots = self.slots.lock().unwrap();
        let orphans: Vec<_> = slots
            .iter()
            .filter(|(_, v)| matches!(v, Slot::Batch { .. }))
            .map(|(k, _)| k.clone())
            .collect();
        for k in orphans {
            if let Some(Slot::Batch { dir, .. }) = slots.remove(&k) {
                let _ = self.out.msg(Msg::Need { dir, files: Vec::new(), xattrs: Vec::new() });
            }
        }
    }

    // -----------------------------------------------------------------------
    // Requests
    // -----------------------------------------------------------------------

    /// Ask the sender for the changed files of one listing (exactly one `Need` per listing).
    pub(crate) fn request(&self, eng: &Engine, wire_dir: u32, files: Vec<FileJob>) {
        let mut reqs = Vec::with_capacity(files.len());
        for job in files {
            if fsops::interrupted() {
                break;
            }
            eng.note_transfer(&job);
            if eng.dry() {
                crate::progress::done(job.meta.size);
                continue;
            }
            let routed = self.link.is_none_or(|l| l.wants_delta(job.meta.size));
            let sig = if self.delta && routed { basis_signature(&job, eng.opts.delta) } else { None };
            reqs.push(NeedReq { idx: job.idx, sig });
            self.needed.lock().unwrap().insert(file_id(wire_dir, job.idx), job);
        }
        let xattrs = self.xattr_reqs.lock().unwrap().remove(&wire_dir).unwrap_or_default();
        let _ = self.out.msg(Msg::Need { dir: wire_dir, files: reqs, xattrs });
    }

    // -----------------------------------------------------------------------
    // File data
    // -----------------------------------------------------------------------

    /// Answer a `Refine`: fine signatures of the requested stretches of the basis file the
    /// sender is about to send a delta against (its job is still waiting in `needed`).
    fn refine<'s>(&'s self, s: &Scope<'s>, id: u64, grain: Grain, gaps: Vec<GapReq>) {
        let basis = self.needed.lock().unwrap().get(&id).map(|job| job.basis_path().to_path_buf());
        s.spawn(move |_| {
            let sigs = (|| {
                let p = grain.params()?;
                let f = File::open(basis?).ok()?;
                let size = f.metadata().ok()?.len();
                refine::basis_sigs(&f, size, &p, &gaps).ok()
            })();
            let _ = self.out.msg(Msg::Refined { id, gaps: sigs.unwrap_or_default() });
        });
    }

    fn take_job(&self, id: u64) -> io::Result<FileJob> {
        self.needed
            .lock()
            .unwrap()
            .remove(&id)
            .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidData, format!("data for unrequested file {id:#x}")))
    }

    /// Open the file the data goes into: a temp file next to the destination, or (with
    /// `--inplace`) the destination itself.
    fn open_target(&self, eng: &Engine, job: &FileJob, mode: u32) -> io::Result<(File, PathBuf, bool, bool, bool)> {
        let existing = job.dst_meta.is_some_and(|d| d.kind == Kind::File);
        if eng.opts.inplace && existing {
            let f = fs::OpenOptions::new().write(true).open(&job.dst_path)?;
            return Ok((f, job.dst_path.clone(), false, false, false));
        }
        if eng.opts.inplace && job.dst_meta.is_none() {
            fsops::register_temp(&job.dst_path);
            return match fsops::create_new(&job.dst_path, mode) {
                Ok(f) => Ok((f, job.dst_path.clone(), false, true, true)),
                Err(e) => {
                    fsops::unregister_temp(&job.dst_path);
                    Err(e)
                }
            };
        }
        let (dir, name) = split_dst(&job.dst_path);
        let (tmp, f) = fsops::with_temp(dir, name, |t| fsops::create_new(t, mode))?;
        Ok((f, tmp, true, true, true))
    }

    /// The requested file `id`, with the attribute values that came with its data.
    fn take_job_with(&self, eng: &Engine, id: u64, xattrs: Option<Vec<Xattr>>) -> io::Result<FileJob> {
        let mut job = self.take_job(id)?;
        job.full_xattrs = xattrs.map(|x| eng.xrules.to_local(x));
        Ok(job)
    }

    fn file_begin(&self, eng: &Engine, id: u64, size: u64, xattrs: Option<Vec<Xattr>>) -> io::Result<()> {
        let job = self.take_job_with(eng, id, xattrs)?;
        // A whole file is all literal data, as rsync counts it.
        Stats::add(&eng.stats.literal_bytes, size);
        eng.backup_existing_file(&job);
        let (mode, _, _) = eng.target_attrs(&job);
        let (file, path, rename, registered, created) = self.open_target(eng, &job, mode)?;
        let hint = cache::Writer::new(&file, size);
        let inc = Incoming {
            file,
            path,
            rename,
            registered,
            created,
            delta: false,
            size,
            job,
            hint,
            held: (0, Vec::new()),
            prefix: Prefix::default(),
            counted: 0,
        };
        self.incoming.lock().unwrap().insert(id, inc);
        Ok(())
    }

    fn delta_begin(&self, eng: &Engine, id: u64, size: u64, copies: Vec<Op>, xattrs: Option<Vec<Xattr>>) -> io::Result<()> {
        let job = self.take_job_with(eng, id, xattrs)?;
        eng.backup_existing_file(&job);
        let basis = File::open(job.basis_path())?;
        let d = Delta { size, ops: copies };
        let matched: u64 = d.ops.iter().map(Op::len).sum();
        Stats::add(&eng.stats.delta_files, 1);
        Stats::add(&eng.stats.matched_bytes, matched);
        Stats::add(&eng.stats.literal_bytes, size.saturating_sub(matched));

        let (file, path, rename, registered, has_basis) = if eng.opts.inplace && d.is_in_place() && job.basis.is_none() {
            let f = fs::OpenOptions::new().write(true).open(&job.dst_path)?;
            (f, job.dst_path.clone(), false, false, true)
        } else {
            let want_clone = !eng.opts.no_clone && d.in_place_bytes() > 0;
            let (dir, name) = split_dst(&job.dst_path);
            let (tmp, cloned) = fsops::with_temp(dir, name, |t| {
                if want_clone && fsops::clone_into_new(&basis, t)? {
                    return Ok(true);
                }
                fsops::create_new(t, 0o600).map(|_| false)
            })?;
            if cloned {
                Stats::add(&eng.stats.cloned, 1);
                eng.cloned(&tmp);
            }
            match fs::OpenOptions::new().write(true).open(&tmp) {
                Ok(f) => (f, tmp, true, true, cloned),
                Err(e) => {
                    fsops::drop_temp(&tmp);
                    return Err(e);
                }
            }
        };
        let r = if eng.opts.delta == DeltaMode::Cdc {
            basis.metadata().and_then(|m| cdc::Mapped::new(&basis, m.len())).and_then(|m| apply(&d, &m, &NoLiteral, &file, has_basis))
        } else {
            let basis_size = job.dst_meta.map_or(0, |m| m.size);
            apply(&d, &ReadOnce::new(&basis, basis_size), &NoLiteral, &file, has_basis)
        };
        if let Err(e) = r {
            if registered {
                fsops::drop_temp(&path);
            }
            return Err(e);
        }
        let hint = cache::Writer::new(&file, size);
        let inc = Incoming {
            file,
            path,
            rename,
            registered,
            created: false,
            delta: true,
            size,
            job,
            hint,
            held: (0, Vec::new()),
            prefix: Prefix::with_copies(&d.ops),
            counted: matched,
        };
        crate::progress::bytes(matched);
        self.incoming.lock().unwrap().insert(id, inc);
        Ok(())
    }

    fn data(&self, eng: &Engine, id: u64, off: u64, bytes: &[u8]) -> io::Result<()> {
        let mut incoming = self.incoming.lock().unwrap();
        match incoming.get_mut(&id) {
            Some(inc) => {
                if eng.opts.sparse && inc.created && !inc.delta {
                    write_sparse_at(&inc.file, bytes, off)?;
                    inc.hint.written_to(&inc.file, off + bytes.len() as u64);
                } else {
                    inc.write(off, bytes)?;
                }
                inc.prefix.data(off, bytes.len() as u64);
                inc.counted += bytes.len() as u64;
                crate::progress::bytes(bytes.len() as u64);
                Ok(())
            }
            // The file failed to open on our side; its error was already reported.
            None => Ok(()),
        }
    }

    fn file_end(&self, eng: &Engine, id: u64, ok: bool) {
        let Some(inc) = self.incoming.lock().unwrap().remove(&id) else {
            // Never started: the sender couldn't read it (and reported why).
            self.needed.lock().unwrap().remove(&id);
            return;
        };
        if !ok {
            if inc.registered {
                fsops::drop_temp(&inc.path);
            }
            return;
        }
        let dst = inc.job.dst_path.clone();
        crate::progress::done(inc.job.meta.size.saturating_sub(inc.counted));
        let (path, registered) = (inc.path.clone(), inc.registered);
        let (job, r) = self.finish(eng, inc);
        let r = if registered { finish_temp(&path, r) } else { r };
        match r {
            Ok(()) => eng.file_written(&job),
            Err(e) if !fsops::interrupted() => eng.err(path_msg("receiving \"", &dst, format!("\" failed: {e}"))),
            Err(_) => {}
        }
    }

    fn finish(&self, eng: &Engine, mut inc: Incoming) -> (FileJob, io::Result<()>) {
        let flushed = inc.flush();
        // Every byte arrived (and sparse writes left no hole at the end).
        let full = inc.prefix.done == inc.size && !(eng.opts.sparse && inc.created && !inc.delta);
        let Incoming { file, path, rename, created, delta, size, job, .. } = inc;
        let r = flushed.and_then(|()| Self::finish_file(eng, file, &path, rename, created, delta, size, full, &job));
        (job, r)
    }

    #[allow(clippy::too_many_arguments)]
    fn finish_file(
        eng: &Engine,
        file: File,
        path: &Path,
        rename: bool,
        created: bool,
        delta: bool,
        size: u64,
        full: bool,
        job: &FileJob,
    ) -> io::Result<()> {
        // A file we created and wrote completely is already `size` long. Truncating it
        // anyway costs a syscall, and on XFS a flush when it is closed (XFS writes back
        // truncated files on close), 2.8x slower for 20,000 small files.
        if !(full && created && !delta) {
            file.set_len(size)?;
        }
        let (mode, uid, gid) = eng.target_attrs(job);
        let meta = job.meta;
        let src = job.src(true);
        if created && !delta {
            let mtime = eng.opts.times.then_some((meta.mtime, meta.mtime_ns));
            eng.apply_new_file_attrs(path, Copied::Written(file), mode, uid, gid, mtime, Some(&src))?;
        } else {
            eng.finish_delta_file(&file, &meta, mode, uid, gid, Some((path, &src)))?;
            drop(file);
        }
        fsops::check_interrupt()?;
        if rename {
            fs::rename(path, &job.dst_path)?;
        }
        Ok(())
    }

    fn small_pack(&self, eng: &Engine, files: Vec<(u64, u32)>, blob: Vec<u8>, xattrs: Vec<(u64, Vec<Xattr>)>) {
        let mut xattrs: HashMap<u64, Vec<Xattr>> = xattrs.into_iter().collect();
        #[cfg(target_os = "linux")]
        if eng.opts.io_uring && !eng.opts.inplace && !eng.opts.sparse && fsops::uring::available() {
            return self.small_pack_uring(eng, files, &blob, xattrs);
        }
        let mut off = 0usize;
        for (id, len) in files {
            let bytes = blob.get(off..off + len as usize).unwrap_or(&[]);
            off += len as usize;
            if fsops::interrupted() {
                return;
            }
            let job = match self.take_job_with(eng, id, xattrs.remove(&id)) {
                Ok(j) => j,
                Err(e) => {
                    eng.err(e.to_string());
                    continue;
                }
            };
            let dst = job.dst_path.clone();
            crate::progress::done(job.meta.size);
            Stats::add(&eng.stats.literal_bytes, len as u64);
            if let Err(e) = self.write_small(eng, job, bytes)
                && !fsops::interrupted()
            {
                eng.err(path_msg("receiving \"", &dst, format!("\" failed: {e}")));
            }
        }
    }

    /// `--io-uring`: create, write and close the pack's files in three ring batches.
    /// Files the batch can't create (a temp-name collision, say) take the normal path.
    #[cfg(target_os = "linux")]
    fn small_pack_uring(&self, eng: &Engine, files: Vec<(u64, u32)>, blob: &[u8], mut xattrs: HashMap<u64, Vec<Xattr>>) {
        use crate::fsops::uring;
        use std::ffi::CString;
        use std::os::unix::ffi::OsStrExt;
        let report = |dst: &Path, e: io::Error| {
            if !fsops::interrupted() {
                eng.err(path_msg("receiving \"", dst, format!("\" failed: {e}")));
            }
        };
        let mut jobs = Vec::with_capacity(files.len());
        let mut off = 0usize;
        for (id, len) in files {
            let bytes = blob.get(off..off + len as usize).unwrap_or(&[]);
            off += len as usize;
            match self.take_job_with(eng, id, xattrs.remove(&id)) {
                Ok(job) => {
                    crate::progress::done(job.meta.size);
                    Stats::add(&eng.stats.literal_bytes, len as u64);
                    eng.backup_existing_file(&job);
                    let (dir, name) = split_dst(&job.dst_path);
                    let tmp = dir.join(fsops::temp_name(name));
                    fsops::register_temp(&tmp);
                    jobs.push((job, bytes, tmp));
                }
                Err(e) => eng.err(e.to_string()),
            }
        }
        if fsops::interrupted() {
            return jobs.iter().for_each(|(_, _, tmp)| fsops::drop_temp(tmp));
        }
        let paths: Vec<CString> =
            jobs.iter().map(|(_, _, t)| CString::new(t.as_os_str().as_bytes()).unwrap_or_default()).collect();
        let modes: Vec<u32> = jobs.iter().map(|(j, _, _)| eng.target_attrs(j).0).collect();
        let created = match uring::create_new(&paths, &modes) {
            Ok(c) => c,
            Err(_) => paths.iter().map(|_| Err(io::ErrorKind::Unsupported.into())).collect(),
        };
        let mut open = Vec::new();
        for ((job, bytes, tmp), fd) in jobs.into_iter().zip(created) {
            match fd {
                Ok(fd) => open.push((job, bytes, tmp, fd)),
                Err(_) => {
                    fsops::unregister_temp(&tmp);
                    let dst = job.dst_path.clone();
                    if let Err(e) = self.write_small(eng, job, bytes) {
                        report(&dst, e);
                    }
                }
            }
        }
        let batch: Vec<(i32, &[u8])> = open.iter().map(|(_, b, _, fd)| (uring::raw(fd), *b)).collect();
        let written = uring::write_all(&batch).unwrap_or_else(|e| batch.iter().map(|_| Err(io::Error::new(e.kind(), e.to_string()))).collect());
        let mut fds = Vec::with_capacity(open.len());
        let mut done = Vec::with_capacity(open.len());
        for ((job, _, tmp, fd), w) in open.into_iter().zip(written) {
            let file = File::from(fd);
            let (mode, uid, gid) = eng.target_attrs(&job);
            let mtime = eng.opts.times.then_some((job.meta.mtime, job.meta.mtime_ns));
            let r = w.and_then(|()| eng.set_written_attrs(&file, mode, uid, gid, mtime, Some((&tmp, &job.src(true)))));
            done.push((job, tmp, r));
            fds.push(std::os::fd::OwnedFd::from(file));
        }
        let _ = uring::close_all(fds);
        for (job, tmp, r) in done {
            let r = r.and_then(|()| fsops::check_interrupt()).and_then(|()| fs::rename(&tmp, &job.dst_path));
            match finish_temp(&tmp, r) {
                Ok(()) => eng.file_written(&job),
                Err(e) => report(&job.dst_path, e),
            }
        }
    }

    fn write_small(&self, eng: &Engine, job: FileJob, bytes: &[u8]) -> io::Result<()> {
        eng.backup_existing_file(&job);
        let (mode, _, _) = eng.target_attrs(&job);
        let (file, path, rename, registered, created) = self.open_target(eng, &job, mode)?;
        let size = bytes.len() as u64;
        let write = if eng.opts.sparse && created { write_sparse_at(&file, bytes, 0) } else { file.write_all_at(bytes, 0) };
        let full = !(eng.opts.sparse && created);
        let r = write.and_then(|()| Self::finish_file(eng, file, &path, rename, created, false, size, full, &job));
        let r = if registered { finish_temp(&path, r) } else { r };
        if r.is_ok() {
            eng.file_written(&job);
        }
        r
    }

    /// Deal with every partially received file after a failed transfer: discarded, or
    /// with `--partial` cut to the data received so far and kept.
    fn abandon(&self, eng: &Engine) {
        for (_, inc) in self.incoming.lock().unwrap().drain() {
            if !inc.registered {
                continue;
            }
            let mut inc = inc;
            let keep = eng.opts.partial
                && inc.flush().is_ok()
                && inc.prefix.done > 0
                && inc.file.set_len(inc.prefix.done).is_ok()
                && eng.keep_partial(&inc.path, &inc.job.dst_path);
            if !keep {
                fsops::drop_temp(&inc.path);
            }
        }
    }
}

/// A sender's `-X`/`-A` data in the receiver's form: ACL ids mapped by name, then local
/// attribute names and ACL masks (`mode`: the entry's mode as sent).
fn local_extra(ids: &IdMap, r: &crate::xattrs::Rules, x: Option<Box<crate::entry::Extra>>, mode: u32) -> Option<Box<crate::entry::Extra>> {
    let mut x = x?;
    if let Some(acl) = &mut x.acl
        && !ids.is_empty()
    {
        acl.map_ids(|user, id| ids.map(user, id));
    }
    extra::to_local(r, Some(x), mode)
}

/// Coarse signature of the destination file to send along with a request, so the sender
/// can reply with a delta: aligned blocks when the size is unchanged (in-place edits),
/// content-defined chunks when it changed (inserts, deletes, appends). The sender may then
/// ask for finer signatures of the parts that didn't match ([`RemoteState::refine`]).
/// `--delta=cdc` always sends its own chunks.
fn basis_signature(job: &FileJob, mode: DeltaMode) -> Option<Sig> {
    let d = match &job.basis {
        Some(p) => fsops::lstat(p).ok()?,
        None => job.dst_meta?,
    };
    if d.kind != Kind::File || d.size == 0 || !delta::worth_delta(job.meta.size) {
        return None;
    }
    let f = File::open(job.basis_path()).ok()?;
    if mode == DeltaMode::Cdc {
        let m = cdc::Mapped::new(&f, d.size).ok()?;
        return Some(Sig::Cdc(cdc::signature(m.bytes(), &delta::Params::DEFAULT)));
    }
    let f = ReadOnce::new(&f, d.size);
    let p = delta::Params::DEFAULT;
    if job.meta.size == d.size {
        delta::block_signature(&f, d.size, &p).ok().map(Sig::Blocks)
    } else {
        delta::chunk_signature(&f, d.size, &p).ok().map(Sig::Chunks)
    }
}

/// Decide what each root maps to on this side: rsync's destination rules (a single
/// file may be copied to a new name; otherwise DEST is a directory, created if needed).
/// Also returns the directory the roots write into.
/// Records the destination in `eng` (see [`Engine::set_dest`]).
fn plan_dest(eng: &mut Engine, roots: &[RootInfo], dest: &OsStr, out: &Output) -> Result<Vec<(u16, Slot)>, i32> {
    let dest_path = PathBuf::from(dest);
    let dest_meta = fs::metadata(&dest_path).ok();
    let dest_is_dir = dest_meta.as_ref().is_some_and(|m| m.is_dir());

    if let [root] = roots
        && let [item] = root.items.as_slice()
        && !root.contents
        && root.sel.is_none()
        && (item.meta.kind != Kind::Dir || (!eng.opts.recursive && dest_meta.is_none()))
        && !dest_is_dir
        && !wants_contents(dest)
    {
        let (dst_dir, Some(dst_name)) = split_source(&dest_path) else {
            out.error(path_msg("invalid destination \"", &dest_path, "\""));
            return Err(exit::SYNTAX);
        };
        eng.set_dest(&dst_dir);
        return Ok(vec![(0, Slot::Items { dst_dir, rename: Some(dst_name), dst_is_new: false, sel: None })]);
    }

    let mut dest_exists = dest_is_dir;
    let mut created = false;
    if !dest_is_dir {
        if dest_meta.is_some() {
            out.error(path_msg("destination \"", &dest_path, "\" exists and is not a directory"));
            return Err(exit::FILE_IO);
        }
        let shown = dest.to_string_lossy();
        let shown = if shown.len() > 1 { shown.trim_end_matches('/') } else { &shown };
        if eng.dry() {
            out.info(format!("created directory {shown}"));
        } else if let Err(e) = fs::create_dir(&dest_path) {
            out.error(path_msg("mkdir \"", &dest_path, format!("\" failed: {e}")));
            return Err(exit::FILE_IO);
        } else {
            out.info(format!("created directory {shown}"));
            dest_exists = true;
            created = true;
        }
    }
    // Only a single root may assume a freshly created destination is empty.
    let fresh = created && roots.len() == 1;
    eng.set_dest(&dest_path);
    let eng = &*eng;
    let slots = roots
        .iter()
        .enumerate()
        .map(|(r, root)| {
            let r = r as u16;
            let sel = root.sel.clone().map(select::leak);
            let slot = match (root.contents, root.meta) {
                (true, Some(meta)) => {
                    let mut job = eng.root_job(PathBuf::new(), dest_path.clone(), meta, r, dest_exists && !fresh, root.extra.clone());
                    job.sel = sel.and_then(|s| s.at_root(eng.opts.recursive));
                    Slot::Job(job)
                }
                _ => Slot::Items { dst_dir: dest_path.clone(), rename: None, dst_is_new: !dest_exists || fresh, sel },
            };
            (r, slot)
        })
        .collect();
    Ok(slots)
}

/// Run the receiving side until the sender is done. Sends `ReceiverDone` with the
/// final counters and returns the exit code plus those counters.
pub fn run(conn: &Conn, opts: &Options, dest: &OsStr, out: &Output) -> (i32, StatsSnapshot) {
    let fail = |code: i32, msg: String| {
        out.error(&msg);
        let _ = conn.msg(Msg::Fatal { msg });
        (code, StatsSnapshot::default())
    };
    let mut ids = IdMap::default();
    let mut probe = adapt::ProbeReceiver::default();
    let mut link = None;
    let mut roots = loop {
        let f = recv(conn);
        if let Ok(f) = &f {
            match probe.handle(conn, f) {
                Ok(true) => continue,
                Ok(false) => {}
                Err(e) => return fail(exit::PROTOCOL, format!("connection error: {e}")),
            }
        }
        match f {
            Ok(Frame::Msg(Msg::LinkInfo(l))) => link = Some(l),
            Ok(Frame::Msg(Msg::Roots(r))) => break r,
            Ok(Frame::Msg(Msg::IdNames { users, groups })) => ids.add(&users, &groups),
            Ok(Frame::Msg(Msg::Log { stderr, escape, text })) => out.print_forwarded(stderr, escape, &text),
            Ok(Frame::Msg(Msg::Fatal { msg })) => {
                out.error(format!("remote: {msg}"));
                return (exit::PROTOCOL, StatsSnapshot::default());
            }
            Ok(other) => return fail(exit::PROTOCOL, format!("protocol error: expected Roots, got {other:?}")),
            Err(Stop::Interrupted) => return (exit::SIGNAL, StatsSnapshot::default()),
            Err(Stop::Io(e)) => return fail(exit::PROTOCOL, format!("connection error: {e}")),
        }
    };
    let filter = match Filter::new(&opts.filter_rules) {
        Ok(f) => f,
        Err(e) => return fail(exit::SYNTAX, format!("{e:#}")),
    };
    for root in &mut roots {
        root.meta.iter_mut().for_each(|m| ids.apply(m));
        root.items.iter_mut().for_each(|e| ids.apply(&mut e.meta));
    }
    let mut engine = Engine::new(opts, filter, out);
    for root in roots.iter_mut().filter(|_| engine.extras_on()) {
        let mode = root.meta.map_or(0, |m| m.mode);
        root.extra = local_extra(&ids, &engine.xrules, root.extra.take(), mode);
    }
    // --scanner=jwalk indexes the destination while listings arrive, if it existed
    // before this run (a directory created now is being filled at the same time).
    let index_dest = opts.scanner == Scanner::Jwalk && opts.recursive && fs::metadata(dest).is_ok_and(|m| m.is_dir());
    let slots = match plan_dest(&mut engine, &roots, dest, out) {
        Ok(s) => s,
        Err(code) => {
            let _ = conn.msg(Msg::Fatal { msg: "the receiver could not prepare the destination".into() });
            return (code, StatsSnapshot::default());
        }
    };
    let mut state = RemoteState::new(conn.out.clone(), opts, ids);
    // Adaptive strategy: route by the measured link unless -W / --no-whole-file decided.
    state.link = link.filter(|_| opts.strategy == Strategy::Adaptive && opts.whole_file.is_none());
    engine.remote = Some(state);
    let pool = match rayon::ThreadPoolBuilder::new()
        .num_threads(opts.threads)
        .thread_name(|i| format!("barqsync-recv-{i}"))
        .stack_size(WORKER_STACK)
        .build()
    {
        Ok(p) => p,
        Err(e) => return fail(exit::IPC, format!("failed to start worker threads: {e}")),
    };
    let dst_scan = index_dest.then(|| {
        let index = Arc::new(DstIndex::default());
        engine.dst_index = Some(Arc::clone(&index));
        let (top, threads, scan) = (PathBuf::from(dest), opts.threads, Arc::clone(&index));
        let handle = std::thread::spawn(move || {
            scan.scan(&top, b"", threads);
            scan.finish();
        });
        (index, handle)
    });

    let engine = &engine;
    let remote = engine.remote.as_ref().unwrap();
    let streams = conn.data_streams_in();
    let (sender_errors, failure) = pool.install(|| {
        rayon::scope(|s| {
            {
                let mut map = remote.slots.lock().unwrap();
                for (r, slot) in slots {
                    map.insert((r, Vec::new()), slot);
                }
            }
            let mut streams_ended = 0;
            let mut sender_done: Option<u64> = None;
            loop {
                if let Some(errors) = sender_done
                    && streams_ended == streams
                {
                    break (errors, None);
                }
                let frame = match recv(conn) {
                    Ok(f) => f,
                    // After Ctrl-C, which also stops ssh, a closed connection is expected.
                    Err(Stop::Interrupted) => break (0, Some(exit::SIGNAL)),
                    Err(Stop::Io(_)) if fsops::interrupted() => break (0, Some(exit::SIGNAL)),
                    Err(Stop::Io(e)) => {
                        out.error(format!("connection error: {e}"));
                        break (0, Some(exit::PROTOCOL));
                    }
                };
                let r = match frame {
                    Frame::Data { id, off, bytes } => remote.data(engine, id, off, &bytes),
                    Frame::Msg(m) => match m {
                        Msg::DirBatch { root, dir, rel, entries, sums } => {
                            remote.on_batch(s, engine, root, dir, rel, entries, sums);
                            Ok(())
                        }
                        Msg::IdNames { users, groups } => {
                            remote.ids.lock().unwrap().add(&users, &groups);
                            Ok(())
                        }
                        Msg::FileBegin { id, size, xattrs } => remote.file_begin(engine, id, size, xattrs),
                        Msg::DeltaBegin { id, size, copies, xattrs } => remote.delta_begin(engine, id, size, copies, xattrs),
                        Msg::Xattrs { id, xattrs } => {
                            remote.on_xattrs(engine, id, xattrs);
                            Ok(())
                        }
                        Msg::Refine { id, grain, gaps } => {
                            remote.refine(s, id, grain, gaps);
                            Ok(())
                        }
                        Msg::FileEnd { id, ok } => {
                            remote.file_end(engine, id, ok);
                            Ok(())
                        }
                        Msg::SmallPack { files, blob, xattrs } => {
                            remote.acquire_pack();
                            s.spawn(move |_| {
                                remote.small_pack(engine, files, blob, xattrs);
                                remote.release_pack();
                            });
                            Ok(())
                        }
                        Msg::ScanDone { .. } => {
                            remote.on_scan_done();
                            Ok(())
                        }
                        Msg::SenderDone { errors } => {
                            sender_done = Some(errors);
                            Ok(())
                        }
                        Msg::StreamEnd => {
                            streams_ended += 1;
                            Ok(())
                        }
                        Msg::Log { stderr, escape, text } => {
                            out.print_forwarded(stderr, escape, &text);
                            Ok(())
                        }
                        Msg::Fatal { msg } => {
                            out.error(format!("remote: {msg}"));
                            break (0, Some(exit::PROTOCOL));
                        }
                        other => {
                            out.error(format!("protocol error: unexpected {other:?}"));
                            break (0, Some(exit::PROTOCOL));
                        }
                    },
                };
                if let Err(e) = r {
                    engine.err(format!("receiving file data failed: {e}"));
                }
            }
        })
    });

    if let Some((index, scan)) = dst_scan {
        index.cancel();
        let _ = scan.join();
    }
    if let Some(code) = failure {
        remote.abandon(engine);
        fsops::cleanup_temps();
        return (code, engine.stats.snapshot());
    }
    let unanswered = remote.needed.lock().unwrap().len() + remote.incoming.lock().unwrap().len();
    if unanswered > 0 {
        engine.err(format!("{unanswered} requested files were never received"));
        remote.abandon(engine);
    }
    let waiting = remote.xattr_wait.lock().unwrap().len();
    if waiting > 0 {
        engine.err(format!("attributes of {waiting} entries were never received"));
    }
    pool.install(|| {
        engine.link_followers();
        engine.apply_fixups();
    });
    remote.flush_synced();
    Stats::add(&engine.stats.errors, sender_errors);
    let stats = engine.stats.snapshot();
    let _ = conn.msg(Msg::ReceiverDone { stats: stats.clone() });
    let code = if Stats::get(&engine.stats.errors) > 0 { exit::PARTIAL } else { exit::OK };
    (code, stats)
}

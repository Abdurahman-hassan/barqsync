//! The receiving end of a remote transfer (the client for a pull, the server for a push).
//!
//! The same [`Engine`] as a local sync does the work: listings arrive as `DirBatch`
//! frames instead of from `readdir`, and changed files are requested with a `Need`
//! frame instead of being copied. A directory is processed once both its listing has
//! arrived and its parent has created it (see [`Slot`]); file data is assembled from
//! `SmallPack`, `FileBegin`/`DeltaBegin` + `Data` + `FileEnd` frames.

use super::{MAX_PACKS_IN_FLIGHT, Stop, WORKER_STACK, recv};
use crate::cli::Options;
use crate::delta::{self, Delta, Op, ReadAt, apply::apply};
use crate::entry::{DirEnt, Kind};
use crate::exit;
use crate::filter::Filter;
use crate::fsops::{self, Copied};
use crate::local::exec::{DirJob, Engine, FileJob, finish_temp, split_dst};
use crate::output::{Output, Stats, StatsSnapshot, path_msg};
use crate::paths::{split_source, wants_contents};
use crate::proto::{Conn, Frame, FrameSender, Msg, NeedReq, RootInfo, Sig, file_id};
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
    Items { dst_dir: PathBuf, rename: Option<OsString>, dst_is_new: bool },
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
    /// Small-file packs queued or being written (bounded, see `MAX_PACKS_IN_FLIGHT`).
    packs: Mutex<usize>,
    pack_done: Condvar,
}

/// Literal ranges arrive as `Data` frames, never through [`apply`].
struct NoLiteral;

impl ReadAt for NoLiteral {
    fn read_exact_at(&self, _: &mut [u8], _: u64) -> io::Result<()> {
        Err(io::Error::other("literal data comes from the wire"))
    }
}

impl RemoteState {
    fn new(out: FrameSender, opts: &Options) -> RemoteState {
        RemoteState {
            out,
            slots: Mutex::new(HashMap::new()),
            needed: Mutex::new(HashMap::new()),
            incoming: Mutex::new(HashMap::new()),
            inflight: AtomicUsize::new(0),
            scan_done: AtomicBool::new(false),
            delta: opts.whole_file != Some(true),
            packs: Mutex::new(0),
            pack_done: Condvar::new(),
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

    fn on_batch<'s, 'a: 's>(&self, s: &Scope<'s>, eng: &'s Engine<'a>, root: u16, dir: u32, rel: Vec<u8>, entries: Vec<DirEnt>) {
        let key = (root, rel);
        let mut slots = self.slots.lock().unwrap();
        match slots.remove(&key) {
            Some(Slot::Job(mut job)) => {
                drop(slots);
                job.wire_dir = dir;
                self.spawn_listing(s, eng, job, entries);
            }
            Some(Slot::Items { dst_dir, rename, dst_is_new }) => {
                drop(slots);
                self.inflight.fetch_add(1, SeqCst);
                s.spawn(move |s| {
                    let entries = entries.into_iter().enumerate().map(|(i, e)| (i as u32, e, rename.clone())).collect();
                    eng.run_items(s, Path::new(""), &dst_dir, root, dir, entries, dst_is_new);
                    eng.remote.as_ref().unwrap().task_done();
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
            let entries = entries.into_iter().enumerate().map(|(i, e)| (i as u32, e, None)).collect();
            eng.process_listing(s, job, entries);
            eng.remote.as_ref().unwrap().task_done();
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
                let _ = self.out.msg(Msg::Need { dir, files: Vec::new() });
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
                continue;
            }
            let sig = if self.delta { basis_signature(&job) } else { None };
            reqs.push(NeedReq { idx: job.idx, sig });
            self.needed.lock().unwrap().insert(file_id(wire_dir, job.idx), job);
        }
        let _ = self.out.msg(Msg::Need { dir: wire_dir, files: reqs });
    }

    // -----------------------------------------------------------------------
    // File data
    // -----------------------------------------------------------------------

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

    fn file_begin(&self, eng: &Engine, id: u64, size: u64) -> io::Result<()> {
        let job = self.take_job(id)?;
        let (mode, _, _) = eng.target_attrs(&job);
        let (file, path, rename, registered, created) = self.open_target(eng, &job, mode)?;
        let inc = Incoming { file, path, rename, registered, created, delta: false, size, job };
        self.incoming.lock().unwrap().insert(id, inc);
        Ok(())
    }

    fn delta_begin(&self, eng: &Engine, id: u64, size: u64, copies: Vec<Op>) -> io::Result<()> {
        let job = self.take_job(id)?;
        let basis = File::open(&job.dst_path)?;
        let d = Delta { size, ops: copies };
        let matched: u64 = d.ops.iter().map(Op::len).sum();
        Stats::add(&eng.stats.delta_files, 1);
        Stats::add(&eng.stats.matched_bytes, matched);
        Stats::add(&eng.stats.literal_bytes, size.saturating_sub(matched));

        let (file, path, rename, registered, has_basis) = if eng.opts.inplace && d.is_in_place() {
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
            }
            match fs::OpenOptions::new().write(true).open(&tmp) {
                Ok(f) => (f, tmp, true, true, cloned),
                Err(e) => {
                    fsops::drop_temp(&tmp);
                    return Err(e);
                }
            }
        };
        if let Err(e) = apply(&d, &basis, &NoLiteral, &file, has_basis) {
            if registered {
                fsops::drop_temp(&path);
            }
            return Err(e);
        }
        let inc = Incoming { file, path, rename, registered, created: false, delta: true, size, job };
        self.incoming.lock().unwrap().insert(id, inc);
        Ok(())
    }

    fn data(&self, id: u64, off: u64, bytes: &[u8]) -> io::Result<()> {
        let incoming = self.incoming.lock().unwrap();
        match incoming.get(&id) {
            Some(inc) => inc.file.write_all_at(bytes, off),
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
        let (path, registered) = (inc.path.clone(), inc.registered);
        let r = self.finish(eng, inc);
        let r = if registered { finish_temp(&path, r) } else { r };
        if let Err(e) = r
            && !fsops::interrupted()
        {
            eng.err(path_msg("receiving \"", &dst, format!("\" failed: {e}")));
        }
    }

    fn finish(&self, eng: &Engine, inc: Incoming) -> io::Result<()> {
        inc.file.set_len(inc.size)?;
        let (mode, uid, gid) = eng.target_attrs(&inc.job);
        let meta = inc.job.meta;
        if inc.created && !inc.delta {
            let mtime = eng.opts.times.then_some((meta.mtime, meta.mtime_ns));
            eng.apply_new_file_attrs(&inc.path, Copied::Written(inc.file), mode, uid, gid, mtime)?;
        } else {
            eng.finish_delta_file(&inc.file, &meta, mode, uid, gid)?;
            drop(inc.file);
        }
        fsops::check_interrupt()?;
        if inc.rename {
            fs::rename(&inc.path, &inc.job.dst_path)?;
        }
        Ok(())
    }

    fn small_pack(&self, eng: &Engine, files: Vec<(u64, u32)>, blob: Vec<u8>) {
        let mut off = 0usize;
        for (id, len) in files {
            let bytes = blob.get(off..off + len as usize).unwrap_or(&[]);
            off += len as usize;
            if fsops::interrupted() {
                return;
            }
            let job = match self.take_job(id) {
                Ok(j) => j,
                Err(e) => {
                    eng.err(e.to_string());
                    continue;
                }
            };
            let dst = job.dst_path.clone();
            if let Err(e) = self.write_small(eng, job, bytes)
                && !fsops::interrupted()
            {
                eng.err(path_msg("receiving \"", &dst, format!("\" failed: {e}")));
            }
        }
    }

    fn write_small(&self, eng: &Engine, job: FileJob, bytes: &[u8]) -> io::Result<()> {
        let (mode, _, _) = eng.target_attrs(&job);
        let (file, path, rename, registered, created) = self.open_target(eng, &job, mode)?;
        let size = bytes.len() as u64;
        let write = file.write_all_at(bytes, 0);
        let inc = Incoming { file, path: path.clone(), rename, registered, created, delta: false, size, job };
        let r = write.and_then(|()| self.finish(eng, inc));
        if registered { finish_temp(&path, r) } else { r }
    }

    /// Discard every partially received file (after a failed transfer).
    fn abandon(&self) {
        for (_, inc) in self.incoming.lock().unwrap().drain() {
            if inc.registered {
                fsops::drop_temp(&inc.path);
            }
        }
    }
}

/// Signature of the destination file to send along with a request, so the sender can
/// reply with a delta: aligned blocks when the size is unchanged (in-place edits),
/// content-defined chunks when it changed (inserts, deletes, appends).
fn basis_signature(job: &FileJob) -> Option<Sig> {
    let d = job.dst_meta?;
    if d.kind != Kind::File || d.size == 0 || job.meta.size < delta::MIN_DELTA_SIZE {
        return None;
    }
    let f = File::open(&job.dst_path).ok()?;
    let p = delta::Params::DEFAULT;
    if job.meta.size == d.size {
        delta::block_signature(&f, d.size, &p).ok().map(Sig::Blocks)
    } else {
        delta::chunk_signature(&f, d.size, &p).ok().map(Sig::Chunks)
    }
}

/// Decide what each root maps to on this side: rsync's destination rules (a single
/// file may be copied to a new name; otherwise DEST is a directory, created if needed).
fn plan_dest(eng: &Engine, roots: &[RootInfo], dest: &OsStr, out: &Output) -> Result<Vec<(u16, Slot)>, i32> {
    let dest_path = PathBuf::from(dest);
    let dest_meta = fs::metadata(&dest_path).ok();
    let dest_is_dir = dest_meta.as_ref().is_some_and(|m| m.is_dir());

    if let [root] = roots
        && let [item] = root.items.as_slice()
        && !root.contents
        && item.meta.kind != Kind::Dir
        && !dest_is_dir
        && !wants_contents(dest)
    {
        let (dst_dir, Some(dst_name)) = split_source(&dest_path) else {
            out.error(path_msg("invalid destination \"", &dest_path, "\""));
            return Err(exit::SYNTAX);
        };
        return Ok(vec![(0, Slot::Items { dst_dir, rename: Some(dst_name), dst_is_new: false })]);
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
    Ok(roots
        .iter()
        .enumerate()
        .map(|(r, root)| {
            let r = r as u16;
            let slot = match (root.contents, root.meta) {
                (true, Some(meta)) => {
                    Slot::Job(eng.root_job(PathBuf::new(), dest_path.clone(), meta, r, dest_exists && !fresh))
                }
                _ => Slot::Items { dst_dir: dest_path.clone(), rename: None, dst_is_new: !dest_exists || fresh },
            };
            (r, slot)
        })
        .collect())
}

/// Run the receiving side until the sender is done. Sends `ReceiverDone` with the
/// final counters and returns the exit code plus those counters.
pub fn run(conn: &Conn, opts: &Options, dest: &OsStr, out: &Output) -> (i32, StatsSnapshot) {
    let fail = |code: i32, msg: String| {
        out.error(&msg);
        let _ = conn.msg(Msg::Fatal { msg });
        (code, StatsSnapshot::default())
    };
    let roots = loop {
        match recv(conn) {
            Ok(Frame::Msg(Msg::Roots(r))) => break r,
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
    let mut engine = Engine::new(opts, filter, out);
    let slots = match plan_dest(&engine, &roots, dest, out) {
        Ok(s) => s,
        Err(code) => {
            let _ = conn.msg(Msg::Fatal { msg: "the receiver could not prepare the destination".into() });
            return (code, StatsSnapshot::default());
        }
    };
    engine.remote = Some(RemoteState::new(conn.out.clone(), opts));
    let pool = match rayon::ThreadPoolBuilder::new()
        .num_threads(opts.threads)
        .thread_name(|i| format!("fastrsync-recv-{i}"))
        .stack_size(WORKER_STACK)
        .build()
    {
        Ok(p) => p,
        Err(e) => return fail(exit::IPC, format!("failed to start worker threads: {e}")),
    };

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
                    Err(Stop::Interrupted) => break (0, Some(exit::SIGNAL)),
                    Err(Stop::Io(e)) => {
                        out.error(format!("connection error: {e}"));
                        break (0, Some(exit::PROTOCOL));
                    }
                };
                let r = match frame {
                    Frame::Data { id, off, bytes } => remote.data(id, off, &bytes),
                    Frame::Msg(m) => match m {
                        Msg::DirBatch { root, dir, rel, entries } => {
                            remote.on_batch(s, engine, root, dir, rel, entries);
                            Ok(())
                        }
                        Msg::FileBegin { id, size } => remote.file_begin(engine, id, size),
                        Msg::DeltaBegin { id, size, copies } => remote.delta_begin(engine, id, size, copies),
                        Msg::FileEnd { id, ok } => {
                            remote.file_end(engine, id, ok);
                            Ok(())
                        }
                        Msg::SmallPack { files, blob } => {
                            remote.acquire_pack();
                            s.spawn(move |_| {
                                remote.small_pack(engine, files, blob);
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

    if let Some(code) = failure {
        remote.abandon();
        fsops::cleanup_temps();
        return (code, engine.stats.snapshot());
    }
    let unanswered = remote.needed.lock().unwrap().len() + remote.incoming.lock().unwrap().len();
    if unanswered > 0 {
        engine.err(format!("{unanswered} requested files were never received"));
        remote.abandon();
    }
    pool.install(|| engine.apply_fixups());
    Stats::add(&engine.stats.errors, sender_errors);
    let stats = engine.stats.snapshot();
    let _ = conn.msg(Msg::ReceiverDone { stats: stats.clone() });
    let code = if Stats::get(&engine.stats.errors) > 0 { exit::PARTIAL } else { exit::OK };
    (code, stats)
}

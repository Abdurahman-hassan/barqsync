//! The sending end of a remote transfer (the client for a push, the server for a pull).
//!
//! Scans the source tree in parallel and streams one `DirBatch` per directory as soon
//! as it is read, while answering the receiver's `Need`s concurrently: small files go
//! out packed many-per-frame, larger ones as `Data` streams, and files the receiver
//! already has an old version of as deltas against the signature it sent.

use super::{Stop, WORKER_STACK, recv};
use crate::cli::Options;
use crate::delta::{self, Op, Params};
use crate::checksum::{self, Sum};
use crate::entry::{DirEnt, Kind, Meta};
use crate::ids;
use crate::local::flags::{adjust_listing, adjust_mode, remove_source};
use crate::select::{self, Descend, Sel};
use crate::exit;
use crate::filter::Filter;
use crate::fsops;
use crate::output::{Output, StatsSnapshot, path_msg};
use crate::paths::{join_rel, split_source, wants_contents};
use crate::proto::{self, Conn, Frame, FrameSender, Msg, NeedReq, RootInfo, Sig, SyncedFiles, file_id};
use rayon::Scope;
use std::collections::HashMap;
use std::ffi::{OsStr, OsString};
use std::fs::File;
use std::io;
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::FileExt;
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
    /// Each root's base directory (`--remove-source-files` paths are relative to it).
    bases: Vec<PathBuf>,
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
        let _ = self.out_frames.msg(m);
    }

    /// Register a listing and send it. Registered first, so a quick `Need` finds it.
    fn send_listing(&self, root: u16, rel: Vec<u8>, path: PathBuf, entries: Vec<DirEnt>) {
        let sums = if self.opts.checksum { self.checksums(&path, &entries) } else { Vec::new() };
        let dir = self.next_dir.fetch_add(1, SeqCst);
        self.listings.lock().unwrap().insert(dir, (path, entries.clone()));
        self.name_ids(entries.iter().map(|e| &e.meta));
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

    /// Without `--numeric-ids`, name each uid/gid before the first listing that uses it.
    /// The names are queued under the lock, so no listing using an id can overtake them.
    fn name_ids<'m>(&self, metas: impl Iterator<Item = &'m Meta>) {
        if self.opts.numeric_ids || !(self.opts.owner || self.opts.group) {
            return;
        }
        let mut sent = self.ids.lock().unwrap();
        let (users, groups) = sent.new_names(metas, self.opts.owner, self.opts.group);
        if !users.is_empty() || !groups.is_empty() {
            self.send(Msg::IdNames { users, groups });
        }
    }

    fn walk_dir<'s>(&'s self, s: &Scope<'s>, root: u16, rel: Vec<u8>, path: PathBuf, root_dev: u64, sel: Option<&'static Sel>) {
        if fsops::interrupted() {
            return;
        }
        let ents = match sel {
            Some(sel) if !sel.contents => self.read_selected(&path, sel),
            _ => match fsops::read_dir_meta(&path) {
                Ok(v) => adjust_listing(self.opts, &path, v, &mut |m| self.err(m)),
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
        adjust_listing(self.opts, dir, ents, &mut |m| self.err(m))
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

    /// `--remove-source-files`: the receiver has these entries now.
    fn remove_synced(&self, root: u16, files: SyncedFiles) {
        let Some(base) = self.bases.get(root as usize) else {
            return self.err(format!("protocol error: unknown root {root}"));
        };
        for (rel, size, mtime) in files {
            let path = base.join(OsStr::from_bytes(&rel));
            match remove_source(self.opts, &path, size, mtime) {
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
            if let Err(e) = self.out_frames.end_data_streams() {
                self.err(format!("closing data streams failed: {e}"));
            }
            self.send(Msg::SenderDone { errors: self.errors.load(SeqCst) });
        }
    }

    // -----------------------------------------------------------------------
    // Serving requests
    // -----------------------------------------------------------------------

    fn serve<'s>(&'s self, s: &Scope<'s>, dir: u32, reqs: Vec<NeedReq>) {
        let listing = self.listings.lock().unwrap().remove(&dir);
        match listing {
            Some((base, entries)) => self.serve_listing(s, dir, &base, &entries, reqs),
            None => self.err(format!("protocol error: request for unknown directory {dir}")),
        }
        self.needs_done.fetch_add(1, SeqCst);
        self.check_done();
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
            crate::progress::planned(e.meta.size);
            if req.sig.is_none() && e.meta.size <= proto::SMALL_FILE {
                small.push((id, path));
            } else {
                large.push((id, path, req.sig));
            }
        }
        if !small.is_empty() {
            self.send_small(small);
        }
        for (id, path, sig) in large {
            self.pending_files.fetch_add(1, SeqCst);
            s.spawn(move |_| {
                if !fsops::interrupted() {
                    let size = std::fs::metadata(&path).map(|m| m.len()).unwrap_or(0);
                    let r = match sig {
                        Some(sig) => self.send_delta(id, &path, sig),
                        None => self.send_whole(id, &path),
                    };
                    if let Err(e) = r {
                        self.err(path_msg("send_files failed to read \"", &path, format!("\": {e}")));
                        self.send(Msg::FileEnd { id, ok: false });
                    }
                    crate::progress::done(size);
                }
                self.pending_files.fetch_sub(1, SeqCst);
                self.check_done();
            });
        }
    }

    /// Read small files in parallel and send them packed into frames of ~4 MiB.
    fn send_small(&self, files: Vec<(u64, PathBuf)>) {
        let read = files.into_iter().map(|(id, path)| (id, std::fs::read(&path), path));
        let mut pack: Vec<(u64, u32)> = Vec::new();
        let mut blob: Vec<u8> = Vec::new();
        for (id, data, path) in read {
            match data {
                Ok(d) => {
                    crate::progress::done(d.len() as u64);
                    if !blob.is_empty() && blob.len() + d.len() > proto::PACK_BYTES {
                        self.send(Msg::SmallPack { files: std::mem::take(&mut pack), blob: std::mem::take(&mut blob) });
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
            self.send(Msg::SmallPack { files: pack, blob });
        }
    }

    fn send_range(&self, f: &File, id: u64, off: u64, len: u64) -> io::Result<()> {
        let mut pos = 0;
        while pos < len {
            fsops::check_interrupt()?;
            let n = (len - pos).min(proto::DATA_CHUNK as u64) as usize;
            let mut buf = vec![0u8; n];
            f.read_exact_at(&mut buf, off + pos)?;
            self.out_frames.send(Frame::Data { id, off: off + pos, bytes: buf })?;
            pos += n as u64;
        }
        Ok(())
    }

    fn send_whole(&self, id: u64, path: &Path) -> io::Result<()> {
        let f = File::open(path)?;
        self.send_whole_file(id, &f)
    }

    fn send_whole_file(&self, id: u64, f: &File) -> io::Result<()> {
        let size = f.metadata()?.len();
        self.send(Msg::FileBegin { id, size });
        self.send_range(f, id, 0, size)?;
        self.send(Msg::FileEnd { id, ok: true });
        Ok(())
    }

    /// Diff the source file against the receiver's basis signature and send only the
    /// ranges it doesn't have.
    fn send_delta(&self, id: u64, path: &Path, sig: Sig) -> io::Result<()> {
        let f = File::open(path)?;
        let size = f.metadata()?.len();
        let d = match sig {
            Sig::Blocks(basis) => {
                let bs = basis.block_size as usize;
                if bs == 0 {
                    return self.send_whole_file(id, &f);
                }
                let p = Params { block_size: bs, segment: (Params::DEFAULT.segment / bs).max(1) * bs, ..Params::DEFAULT };
                delta::diff_blocks(&delta::block_signature(&f, size, &p)?, &basis)
            }
            Sig::Chunks(basis) => delta::diff_chunks(&delta::chunk_signature(&f, size, &Params::DEFAULT)?, &basis),
        };
        if d.matched_bytes() == 0 {
            return self.send_whole_file(id, &f);
        }
        let copies: Vec<Op> = d.ops.iter().copied().filter(|o| matches!(o, Op::Copy { .. })).collect();
        self.send(Msg::DeltaBegin { id, size, copies });
        for op in &d.ops {
            if let Op::Literal { off, len } = *op {
                self.send_range(&f, id, off, len)?;
            }
        }
        self.send(Msg::FileEnd { id, ok: true });
        Ok(())
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
    let mut roots = Vec::new();
    for r in sel_roots {
        let sel = select::leak(r.sel);
        let info = if r.with_root {
            match fsops::stat(&r.base) {
                Ok(mut m) => {
                    adjust_mode(opts, &mut m);
                    RootInfo { contents: true, meta: Some(m), items: Vec::new(), sel: Some(sel.clone()) }
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
            RootInfo { contents: false, meta: None, items, sel: Some(sel.clone()) }
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
    let mut roots = Vec::new();
    for s in sources {
        let sp = PathBuf::from(s);
        match split_source(&sp) {
            (parent, Some(name)) if !wants_contents(s) => {
                let meta = match fsops::lstat(&sp) {
                    Ok(m) => m,
                    Err(e) => {
                        errors.fetch_add(1, SeqCst);
                        out.error(path_msg("link_stat \"", &sp, format!("\" failed: {e}")));
                        continue;
                    }
                };
                if filter.is_excluded(name.as_bytes(), meta.kind == Kind::Dir) {
                    continue;
                }
                let link = if meta.kind == Kind::Symlink { fsops::read_link_bytes(&sp).ok() } else { None };
                let mut err = |m: Vec<u8>| {
                    errors.fetch_add(1, SeqCst);
                    out.error(m);
                };
                let items = adjust_listing(opts, &parent, vec![DirEnt { name, meta, link }], &mut err);
                let info = RootInfo { contents: false, meta: None, items, sel: None };
                roots.push(SrcRoot { info, base: parent, sel: None });
            }
            _ => match fsops::stat(&sp) {
                Ok(mut m) if m.kind == Kind::Dir => {
                    adjust_mode(opts, &mut m);
                    let info = RootInfo { contents: true, meta: Some(m), items: Vec::new(), sel: None };
                    roots.push(SrcRoot { info, base: sp, sel: None })
                }
                Ok(_) => {
                    errors.fetch_add(1, SeqCst);
                    out.error(path_msg("change_dir \"", &sp, "\" failed: Not a directory"));
                }
                Err(e) => {
                    errors.fetch_add(1, SeqCst);
                    out.error(path_msg("link_stat \"", &sp, format!("\" failed: {e}")));
                }
            },
        }
    }
    roots
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
        bases: roots.iter().map(|r| r.base.clone()).collect(),
    };
    sender.name_ids(roots.iter().flat_map(|r| r.info.meta.iter().chain(r.info.items.iter().map(|e| &e.meta))));
    sender.send(Msg::Roots(roots.iter().map(|r| r.info.clone()).collect()));

    let pool = match rayon::ThreadPoolBuilder::new()
        .num_threads(opts.threads)
        .thread_name(|i| format!("fastrsync-send-{i}"))
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
    let result = pool.install(|| {
        rayon::scope(|s| {
            s.spawn(move |_| {
                rayon::scope(|ws| sender.walk_roots(ws, roots));
                sender.send(Msg::ScanDone { dirs: sender.next_dir.load(SeqCst) });
                sender.scan_done.store(true, SeqCst);
                sender.check_done();
            });
            loop {
                match recv(conn) {
                    Ok(Frame::Msg(Msg::Need { dir, files })) => s.spawn(move |s| sender.serve(s, dir, files)),
                    Ok(Frame::Msg(Msg::Synced { root, files })) => s.spawn(move |_| sender.remove_synced(root, files)),
                    Ok(Frame::Msg(Msg::ReceiverDone { stats })) => break Ok(stats),
                    Ok(Frame::Msg(Msg::Log { stderr, escape, text })) => out.print_forwarded(stderr, escape, &text),
                    Ok(Frame::Msg(Msg::Fatal { msg })) => {
                        out.error(format!("remote: {msg}"));
                        break Err(exit::PROTOCOL);
                    }
                    Ok(other) => {
                        out.error(format!("protocol error: unexpected {other:?}"));
                        break Err(exit::PROTOCOL);
                    }
                    Err(Stop::Interrupted) => break Err(exit::SIGNAL),
                    Err(Stop::Io(e)) => {
                        out.error(format!("connection error: {e}"));
                        break Err(exit::PROTOCOL);
                    }
                }
            }
        })
    });
    match result {
        Ok(stats) => {
            let failed = stats.errors() > 0 || sender.errors.load(SeqCst) > 0;
            (if failed { exit::PARTIAL } else { exit::OK }, Some(stats))
        }
        Err(code) => (code, None),
    }
}

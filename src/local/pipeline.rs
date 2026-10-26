//! `--scanner=jwalk`: the original plan's four-stage pipeline around the engine.
//!
//! 1. **Scan**: [`scan::walk`] reads the source tree on its own pool and emits each
//!    directory's listing as soon as it is read; the destination tree is read into a
//!    [`DstIndex`] at the same time.
//! 2. **Diff**: a listing is processed once its directory's job exists (the parent
//!    creates the destination directory first, see [`Feed`]), against the destination
//!    entries from the index, so no per-file `lstat`. Directories, symlinks and
//!    specials are handled inline, in scan order; `--delete` candidates wait until
//!    scanning succeeds. Files are classified into queues: A metadata-only,
//!    B small (≤ 128 KiB), C large and new, D large and changed.
//! 3. **Transfer**: a pool of worker threads per queue.
//! 4. **Commit**: each file is written to a temp file and renamed into place when
//!    complete; hard links and directory attributes follow once everything is written.

use super::Root;
use super::exec::{DirJob, Engine, FileJob};
use crate::diff::AttrDiff;
use crate::entry::{DirEnt, Kind, Meta};
use crate::filter::{DirRules, Filter, Side};
use crate::fsops;
use crate::proto::SMALL_FILE;
use crate::scan::{self, Descend, DstIndex, Emit};
use crossbeam_channel::{Receiver, Sender, bounded, unbounded};
use rayon::Scope;
use std::collections::HashMap;
use std::os::unix::ffi::OsStrExt;
use std::path::PathBuf;
use std::sync::{Arc, Condvar, Mutex, RwLock};

/// A metadata-only fix (queue A).
pub(crate) struct AttrJob {
    pub(crate) root: u16,
    pub(crate) src_path: PathBuf,
    pub(crate) dst_path: PathBuf,
    pub(crate) rel: Vec<u8>,
    pub(crate) meta: Meta,
    pub(crate) diff: AttrDiff,
    pub(crate) extra: Option<Box<crate::entry::Extra>>,
    /// What the destination has (`-X`/`-A`), read when the file was compared.
    pub(crate) have: Option<super::extra::DstExtra>,
}

struct Senders {
    a: Sender<AttrJob>,
    b: Sender<FileJob>,
    c: Sender<FileJob>,
    d: Sender<FileJob>,
}

/// Queue receivers, one pool of workers each (see [`Pipeline::run_workers`]).
pub(crate) struct Receivers {
    a: Receiver<AttrJob>,
    b: Receiver<FileJob>,
    c: Receiver<FileJob>,
    d: Receiver<FileJob>,
}

/// Jobs queued per worker before Stage 2 waits for the workers to catch up.
const QUEUE_PER_WORKER: usize = 64;

/// A source directory's place in the feed.
enum Slot {
    /// A directory job (by id in [`FeedState::waiting`]) waits for this listing, its
    /// sources' listing number `.1`.
    Wait(u64, usize),
    /// The listing arrived first.
    Listing(Vec<DirEnt>),
}

/// A directory that exists on the destination side, waiting for its sources' listings
/// (more than one when several SRC arguments merge into it).
struct Waiting {
    job: DirJob,
    lists: Vec<Option<Vec<DirEnt>>>,
    missing: usize,
}

/// Listings and jobs, by source directory.
#[derive(Default)]
struct FeedState {
    slots: HashMap<PathBuf, Slot>,
    waiting: HashMap<u64, Waiting>,
    next: u64,
    /// The scan has emitted every listing it will.
    done: bool,
}

pub(crate) struct Pipeline {
    feed: Mutex<FeedState>,
    pub(crate) index: Arc<DstIndex>,
    queues: RwLock<Option<Senders>>,
    /// Jobs queued and not finished yet.
    pending: Mutex<usize>,
    idle: Condvar,
    threads: usize,
}

impl Pipeline {
    pub(crate) fn new(threads: usize) -> (Pipeline, Receivers) {
        let threads = threads.max(1);
        let (a, ra) = bounded(QUEUE_PER_WORKER * Self::workers(threads).0);
        let (b, rb) = bounded(QUEUE_PER_WORKER * Self::workers(threads).1);
        let (c, rc) = bounded(QUEUE_PER_WORKER * Self::workers(threads).2);
        let (d, rd) = bounded(QUEUE_PER_WORKER * Self::workers(threads).3);
        let p = Pipeline {
            feed: Mutex::new(FeedState::default()),
            index: Arc::new(DstIndex::default()),
            queues: RwLock::new(Some(Senders { a, b, c, d })),
            pending: Mutex::new(0),
            idle: Condvar::new(),
            threads,
        };
        (p, Receivers { a: ra, b: rb, c: rc, d: rd })
    }

    /// Workers per queue (A, B, C, D): small files are latency-bound and want many;
    /// big new files stream; deltas parallelise internally.
    fn workers(threads: usize) -> (usize, usize, usize, usize) {
        (2, threads, (threads / 2).max(2), (threads / 4).max(2))
    }

    /// Run the queue workers until [`Pipeline::close`]; call inside a thread scope.
    pub(crate) fn run_workers<'s, 'e: 's>(&'e self, ts: &'s std::thread::Scope<'s, 'e>, eng: &'e Engine<'e>, rx: Receivers) {
        let (na, nb, nc, nd) = Self::workers(self.threads);
        for _ in 0..na {
            let rx = rx.a.clone();
            ts.spawn(move || {
                for j in rx {
                    eng.attr_job(j);
                    self.job_done();
                }
            });
        }
        for (n, rx) in [(nb, rx.b), (nc, rx.c), (nd, rx.d)] {
            for _ in 0..n {
                let rx = rx.clone();
                ts.spawn(move || {
                    for j in rx {
                        eng.transfer_file(&j);
                        self.job_done();
                    }
                });
            }
        }
    }

    fn job_queued(&self) {
        *self.pending.lock().unwrap() += 1;
    }

    fn job_done(&self) {
        let mut n = self.pending.lock().unwrap();
        *n -= 1;
        if *n == 0 {
            self.idle.notify_all();
        }
    }

    /// Wait until every queued job has finished.
    fn wait_idle(&self) {
        let mut n = self.pending.lock().unwrap();
        while *n > 0 {
            n = self.idle.wait(n).unwrap();
        }
    }

    /// No more jobs: the workers finish what is queued and exit.
    pub(crate) fn close(&self) {
        self.queues.write().unwrap().take();
    }

    pub(crate) fn queue_attrs(&self, job: AttrJob) {
        if let Some(q) = self.queues.read().unwrap().as_ref() {
            self.job_queued();
            if q.a.send(job).is_err() {
                self.job_done();
            }
        }
    }

    /// Stage 2 → 3: route a changed file to its queue.
    pub(crate) fn queue_file(&self, job: FileJob) {
        if let Some(q) = self.queues.read().unwrap().as_ref() {
            let to = if job.meta.size <= SMALL_FILE {
                &q.b
            } else if job.dst_meta.is_none() && job.basis.is_none() {
                &q.c
            } else {
                &q.d
            };
            self.job_queued();
            if to.send(job).is_err() {
                self.job_done();
            }
        }
    }

    /// Start a transfer root: nothing is listed yet.
    pub(crate) fn begin_root(&self) {
        *self.feed.lock().unwrap() = FeedState::default();
        self.index.reset();
    }

    /// A directory is ready (created or found on the destination side): process it
    /// with its sources' scanned listings, now or once they have arrived. A source the
    /// scan is over without listing is read directly.
    pub(crate) fn expect_dir<'s, 'a: 's>(&self, s: &Scope<'s>, eng: &'s Engine<'a>, job: DirJob) {
        let paths: Vec<PathBuf> = std::iter::once(&job.src).chain(job.more.iter().map(|d| &d.path)).cloned().collect();
        let mut feed = self.feed.lock().unwrap();
        let id = feed.next;
        feed.next += 1;
        let mut lists = vec![None; paths.len()];
        let mut missing = 0;
        for (i, path) in paths.into_iter().enumerate() {
            match feed.slots.remove(&path) {
                Some(Slot::Listing(ents)) => lists[i] = Some(ents),
                Some(wait) => {
                    // Another job's source (never expected): leave it alone, read this one.
                    feed.slots.insert(path, wait);
                }
                None if !feed.done => {
                    feed.slots.insert(path, Slot::Wait(id, i));
                    missing += 1;
                }
                None => {}
            }
        }
        if missing == 0 {
            drop(feed);
            s.spawn(move |s| eng.process_sources(s, job, lists));
        } else {
            feed.waiting.insert(id, Waiting { job, lists, missing });
        }
    }

    /// A listing from the scan.
    pub(crate) fn on_listing<'s, 'a: 's>(&self, s: &Scope<'s>, eng: &'s Engine<'a>, dir: PathBuf, ents: Vec<DirEnt>) {
        let mut feed = self.feed.lock().unwrap();
        match feed.slots.remove(&dir) {
            Some(Slot::Wait(id, i)) => {
                let w = feed.waiting.get_mut(&id).expect("waiting job");
                w.lists[i] = Some(ents);
                w.missing -= 1;
                if w.missing == 0 {
                    let w = feed.waiting.remove(&id).expect("waiting job");
                    drop(feed);
                    s.spawn(move |s| eng.process_sources(s, w.job, w.lists));
                }
            }
            _ => {
                feed.slots.insert(dir, Slot::Listing(ents));
            }
        }
    }

    /// The scan has finished. Directories still waiting were never listed (pruned by
    /// the scan, unreadable, or reached through a symlink with `-L`/`-k`): they are
    /// read directly. Listings not claimed yet stay for their directories' jobs.
    pub(crate) fn scan_done<'s, 'a: 's>(&self, s: &Scope<'s>, eng: &'s Engine<'a>) {
        let waiting: Vec<Waiting> = {
            let mut feed = self.feed.lock().unwrap();
            feed.done = true;
            feed.slots.retain(|_, v| matches!(v, Slot::Listing(_)));
            feed.waiting.drain().map(|(_, w)| w).collect()
        };
        for w in waiting {
            s.spawn(move |s| eng.process_sources(s, w.job, w.lists));
        }
    }
}

/// Sync the transfer roots (merged when there are several, see
/// [`Engine::run_roots`]) through the pipeline: scan their source and destination trees
/// while the engine works through the listings, then wait for the queues to drain.
pub(crate) fn run_roots(eng: &Engine, p: &Pipeline, roots: &[Root], dest_exists: bool, fresh: bool) {
    p.begin_root();
    let threads = eng.opts.threads;
    // (source directory, its path relative to the transfer root, destination directory)
    let mut tops: Vec<(PathBuf, Vec<u8>, PathBuf)> = Vec::new();
    for root in roots {
        match root {
            Root::Contents { src, dst } => tops.push((src.clone(), Vec::new(), dst.clone())),
            Root::Items { src_dir, dst_dir, items } => tops.extend(
                items
                    .iter()
                    .filter(|(s, _)| fsops::lstat(&src_dir.join(s)).is_ok_and(|m| m.kind == Kind::Dir))
                    .map(|(s, d)| (src_dir.join(s), s.as_bytes().to_vec(), dst_dir.join(d))),
            ),
            Root::Select { .. } => {}
        }
    }
    // The same source given twice is scanned once.
    let mut seen = std::collections::HashSet::new();
    tops.retain(|(src, _, _)| seen.insert(src.clone()));

    let (tx, rx) = unbounded::<Option<(PathBuf, Vec<DirEnt>)>>();
    let filter = Arc::new(Filter::new(&eng.opts.filter_rules, eng.opts.delete_excluded, eng.opts.from0).unwrap_or_default());
    let one_fs = eng.opts.one_file_system;
    let src_tops: Vec<(PathBuf, Vec<u8>)> = tops.iter().map(|(s, r, _)| (s.clone(), r.clone())).collect();
    let src_scan = std::thread::spawn(move || {
        for (top, rel) in src_tops {
            // Like the engine: the device of the directory a walk starts from.
            let root_dev = fsops::stat(&top).map(|m| m.dev).ok();
            let filter = Arc::clone(&filter);
            let descend: Descend = Arc::new(move |rel, meta| {
                // Only rules needing no per-directory state can skip a subtree up front; the
                // engine checks each listing itself.
                let excluded = filter.is_static() && filter.excluded(&DirRules::default(), rel, true, Side::Sender);
                !(fsops::interrupted() || excluded || (one_fs && Some(meta.dev) != root_dev))
            });
            let tx = tx.clone();
            let emit: Emit = Arc::new(move |dir, _, ents| {
                let _ = tx.send(Some((dir.to_path_buf(), ents)));
            });
            scan::walk(&top, &rel, threads, descend, emit);
        }
        let _ = tx.send(None);
    });
    let mut dst_tops: Vec<(PathBuf, Vec<u8>)> =
        if dest_exists && !fresh { tops.into_iter().map(|(_, r, d)| (d, r)).collect() } else { Vec::new() };
    // Several roots writing into one destination directory: it is indexed once.
    let mut seen = std::collections::HashSet::new();
    dst_tops.retain(|(dst, _)| seen.insert(dst.clone()));
    let index = Arc::clone(&p.index);
    let dst_scan = std::thread::spawn(move || {
        for (top, rel) in dst_tops {
            index.scan(&top, &rel, threads);
        }
        index.finish();
    });

    rayon::scope(|s| {
        eng.run_roots(s, roots, dest_exists, fresh);
        while let Ok(Some((dir, ents))) = rx.recv() {
            p.on_listing(s, eng, dir, ents);
        }
        p.scan_done(s, eng);
    });
    p.index.cancel();
    let _ = src_scan.join();
    let _ = dst_scan.join();
    p.wait_idle();
}

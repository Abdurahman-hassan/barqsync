//! `--scanner=jwalk`: the original plan's four-stage pipeline around the engine.
//!
//! 1. **Scan**: [`scan::walk`] reads the source tree on its own pool and emits each
//!    directory's listing as soon as it is read; the destination tree is read into a
//!    [`DstIndex`] at the same time.
//! 2. **Diff**: a listing is processed once its directory's job exists (the parent
//!    creates the destination directory first, see [`Feed`]), against the destination
//!    entries from the index, so no per-file `lstat`. Directories, symlinks and
//!    specials are handled inline, in scan order; `--delete` runs per directory as in
//!    the per-directory engine. Files are classified into queues: A metadata-only,
//!    B small (≤ 128 KiB), C large and new, D large and changed.
//! 3. **Transfer**: a pool of worker threads per queue.
//! 4. **Commit**: each file is written to a temp file and renamed into place when
//!    complete; hard links and directory attributes follow once everything is written.

use super::Root;
use super::exec::{DirJob, Engine, FileJob};
use crate::diff::AttrDiff;
use crate::entry::{DirEnt, Kind, Meta};
use crate::filter::Filter;
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

enum Slot {
    /// The directory exists on the destination side, waiting for its listing.
    Job(Box<DirJob>),
    /// The listing arrived first.
    Listing(Vec<DirEnt>),
}

#[derive(Default)]
struct FeedState {
    slots: HashMap<Vec<u8>, Slot>,
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
    /// with its scanned listing, now or once the listing arrives.
    pub(crate) fn expect_dir<'s, 'a: 's>(&self, s: &Scope<'s>, eng: &'s Engine<'a>, job: DirJob) {
        let mut feed = self.feed.lock().unwrap();
        match feed.slots.remove(&job.rel) {
            Some(Slot::Listing(ents)) => {
                drop(feed);
                s.spawn(move |s| eng.process_scanned(s, job, ents));
            }
            // The scan is over and never listed it: read it directly.
            _ if feed.done => {
                drop(feed);
                s.spawn(move |s| eng.process_dir(s, job));
            }
            _ => {
                feed.slots.insert(job.rel.clone(), Slot::Job(Box::new(job)));
            }
        }
    }

    /// A listing from the scan.
    pub(crate) fn on_listing<'s, 'a: 's>(&self, s: &Scope<'s>, eng: &'s Engine<'a>, rel: Vec<u8>, ents: Vec<DirEnt>) {
        let mut feed = self.feed.lock().unwrap();
        match feed.slots.remove(&rel) {
            Some(Slot::Job(job)) => {
                drop(feed);
                s.spawn(move |s| eng.process_scanned(s, *job, ents));
            }
            _ => {
                feed.slots.insert(rel, Slot::Listing(ents));
            }
        }
    }

    /// The scan has finished. Directories still waiting were never listed (pruned by
    /// the scan, unreadable, or reached through a symlink with `-L`/`-k`): they are
    /// read directly. Listings not claimed yet stay for their directories' jobs.
    pub(crate) fn scan_done<'s, 'a: 's>(&self, s: &Scope<'s>, eng: &'s Engine<'a>) {
        let waiting: Vec<DirJob> = {
            let mut feed = self.feed.lock().unwrap();
            feed.done = true;
            let rels: Vec<Vec<u8>> =
                feed.slots.iter().filter(|(_, v)| matches!(v, Slot::Job(_))).map(|(k, _)| k.clone()).collect();
            rels.into_iter()
                .filter_map(|k| match feed.slots.remove(&k) {
                    Some(Slot::Job(j)) => Some(*j),
                    _ => None,
                })
                .collect()
        };
        for job in waiting {
            s.spawn(move |s| eng.process_dir(s, job));
        }
    }
}

/// Sync one transfer root through the pipeline: scan its source and destination trees
/// while the engine works through the listings, then wait for the queues to drain.
pub(crate) fn run_root(eng: &Engine, p: &Pipeline, root: &Root, dest_exists: bool, fresh: bool) {
    p.begin_root();
    let threads = eng.opts.threads;
    // (source directory, its path relative to the transfer root, destination directory)
    let tops: Vec<(PathBuf, Vec<u8>, PathBuf)> = match root {
        Root::Contents { src, dst } => vec![(src.clone(), Vec::new(), dst.clone())],
        Root::Items { src_dir, dst_dir, items } => items
            .iter()
            .filter(|(s, _)| fsops::lstat(&src_dir.join(s)).is_ok_and(|m| m.kind == Kind::Dir))
            .map(|(s, d)| (src_dir.join(s), s.as_bytes().to_vec(), dst_dir.join(d)))
            .collect(),
        Root::Select { .. } => Vec::new(),
    };

    let (tx, rx) = unbounded::<Option<(Vec<u8>, Vec<DirEnt>)>>();
    let filter = Arc::new(Filter::new(&eng.opts.filter_rules).unwrap_or_default());
    let one_fs = eng.opts.one_file_system;
    let src_tops: Vec<(PathBuf, Vec<u8>)> = tops.iter().map(|(s, r, _)| (s.clone(), r.clone())).collect();
    let src_scan = std::thread::spawn(move || {
        for (top, rel) in src_tops {
            // Like the engine: the device of the directory a walk starts from.
            let root_dev = fsops::stat(&top).map(|m| m.dev).ok();
            let filter = Arc::clone(&filter);
            let descend: Descend = Arc::new(move |rel, meta| {
                !(fsops::interrupted() || filter.is_excluded(rel, true) || (one_fs && Some(meta.dev) != root_dev))
            });
            let tx = tx.clone();
            let emit: Emit = Arc::new(move |_, rel, ents| {
                let _ = tx.send(Some((rel, ents)));
            });
            scan::walk(&top, &rel, threads, descend, emit);
        }
        let _ = tx.send(None);
    });
    let dst_tops: Vec<(PathBuf, Vec<u8>)> =
        if dest_exists && !fresh { tops.into_iter().map(|(_, r, d)| (d, r)).collect() } else { Vec::new() };
    let index = Arc::clone(&p.index);
    let dst_scan = std::thread::spawn(move || {
        for (top, rel) in dst_tops {
            index.scan(&top, &rel, threads);
        }
        index.finish();
    });

    rayon::scope(|s| {
        eng.run_root(s, root, dest_exists, fresh);
        while let Ok(Some((rel, ents))) = rx.recv() {
            p.on_listing(s, eng, rel, ents);
        }
        p.scan_done(s, eng);
    });
    p.index.cancel();
    let _ = src_scan.join();
    let _ = dst_scan.join();
    p.wait_idle();
}

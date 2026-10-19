//! The original plan's Stage 1 scanner (`--scanner=jwalk`): `jwalk` walks a tree on its
//! own rayon pool, and a `process_read_dir` hook stats each directory's children
//! (big directories on several threads) through a [`DirHandle`] (`statx` on Linux, `fstatat` on macOS), prunes
//! the subdirectories that won't be synced, and hands the listing on.
//!
//! Listings come out as soon as each directory is read, in no particular order: the
//! source side feeds them to the engine through [`Feed`](crate::local::pipeline::Feed),
//! the destination side into a [`DstIndex`].

use crate::entry::{DirEnt, Kind, Meta};
use crate::fsops::{self, DirHandle};
use crate::paths::join_rel;
use dashmap::DashMap;
use jwalk::{Parallelism, WalkDirGeneric};
use std::collections::HashMap;
use std::ffi::OsString;
use std::io;
use std::os::unix::ffi::OsStrExt;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering::Relaxed};
use std::sync::{Arc, Condvar, Mutex};

/// Which directory scanner the engine uses (`--scanner`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, serde::Serialize, serde::Deserialize, clap::ValueEnum)]
#[derive(rkyv::Archive, rkyv::Serialize, rkyv::Deserialize)]
pub enum Scanner {
    /// One task per directory: `readdir` + `getattrlistbulk` (macOS) / `statx` (Linux).
    #[default]
    Dirtask,
    /// jwalk tree scans, a destination index and transfer queues A-D.
    Jwalk,
}

/// Whether to descend into a subdirectory: its path relative to the transfer root and
/// its metadata.
pub type Descend = Arc<dyn Fn(&[u8], &Meta) -> bool + Send + Sync>;

/// Receives one directory's listing: its path, its path relative to the transfer
/// root, and its entries sorted by name.
pub type Emit = Arc<dyn Fn(&Path, Vec<u8>, Vec<DirEnt>) + Send + Sync>;

/// Walk the tree under `top` (whose path relative to the transfer root is `rel`),
/// emitting every directory's listing, `top` included. Returns once the whole tree
/// has been read. Directories that can't be read are skipped: the engine reads them
/// itself and reports the error.
pub fn walk(top: &Path, rel: &[u8], threads: usize, descend: Descend, emit: Emit) {
    let top_buf = top.to_path_buf();
    let prefix = rel.to_vec();
    let walker = WalkDirGeneric::<((), ())>::new(top)
        .skip_hidden(false)
        .follow_links(false)
        .sort(false)
        .parallelism(Parallelism::RayonNewPool(threads.max(1)))
        .process_read_dir(move |depth, dir, _, children| {
            if depth.is_none() {
                // The list holding just `top` itself.
                return;
            }
            let rel = match dir.strip_prefix(&top_buf) {
                Ok(r) if r.as_os_str().is_empty() => prefix.clone(),
                Ok(r) => join_rel(&prefix, r.as_os_str()),
                Err(_) => return,
            };
            let Ok(handle) = DirHandle::open(dir) else { return };
            let stat = |c: &mut jwalk::Result<jwalk::DirEntry<((), ())>>| -> Option<DirEnt> {
                let c = c.as_mut().ok()?;
                let ent = stat_child(&handle, c.file_name.clone()).ok()??;
                if ent.meta.kind != Kind::Dir || !descend(&join_rel(&rel, &ent.name), &ent.meta) {
                    c.read_children = None;
                }
                Some(ent)
            };
            // jwalk's workers block inside rayon's `par_bridge`, so rayon work here could
            // be stolen by one of them and never finish: big directories get plain threads.
            let mut ents: Vec<DirEnt> = if children.len() < BIG_DIR {
                children.iter_mut().filter_map(stat).collect()
            } else {
                let per = children.len().div_ceil(STAT_THREADS);
                std::thread::scope(|ts| {
                    let parts: Vec<_> =
                        children.chunks_mut(per).map(|part| ts.spawn(|| part.iter_mut().filter_map(stat).collect::<Vec<_>>())).collect();
                    parts.into_iter().flat_map(|h| h.join().unwrap_or_default()).collect()
                })
            };
            ents.sort_unstable_by(|a, b| a.name.as_bytes().cmp(b.name.as_bytes()));
            emit(dir, rel, ents);
        });
    for _ in walker {}
}

/// Directories with at least this many entries are stat'ed on several threads.
const BIG_DIR: usize = 512;
const STAT_THREADS: usize = 8;

/// `Ok(None)`: the entry vanished since it was listed.
fn stat_child(dir: &DirHandle, name: OsString) -> io::Result<Option<DirEnt>> {
    let meta = match dir.lstat(&name) {
        Ok(m) => m,
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(None),
        Err(e) => return Err(e),
    };
    let link = match meta.kind {
        Kind::Symlink => match dir.readlink(&name) {
            Ok(t) => Some(t),
            Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(None),
            Err(e) => return Err(e),
        },
        _ => None,
    };
    Ok(Some(DirEnt::new(name, meta, link)))
}

/// The destination tree's metadata, scanned while the source is: directory path ->
/// its entries. The engine takes a directory's entries from here instead of reading
/// the directory and `lstat`ing each name.
#[derive(Default)]
pub struct DstIndex {
    dirs: DashMap<PathBuf, HashMap<OsString, Meta>>,
    done: Mutex<bool>,
    ready: Condvar,
    cancel: AtomicBool,
}

impl DstIndex {
    /// Scan `top` (relative path `rel`) into the index; call [`DstIndex::finish`] once
    /// every scan has returned.
    pub fn scan(self: &Arc<Self>, top: &Path, rel: &[u8], threads: usize) {
        let index = Arc::clone(self);
        let emit: Emit = Arc::new(move |dir, _, ents| {
            let map = ents.into_iter().map(|e| (e.name, e.meta)).collect();
            // Under the lock, so a `take` between its lookup and its wait can't miss it.
            let _done = index.done.lock().unwrap();
            index.dirs.insert(dir.to_path_buf(), map);
            index.ready.notify_all();
        });
        let index = Arc::clone(self);
        let descend: Descend = Arc::new(move |_, _| !index.cancel.load(Relaxed) && !fsops::interrupted());
        walk(top, rel, threads, descend, emit);
    }

    /// Every scan has returned: directories not listed by now never will be.
    pub fn finish(&self) {
        *self.done.lock().unwrap() = true;
        self.ready.notify_all();
    }

    /// The transfer no longer needs the index: stop descending (the scan returns soon).
    pub fn cancel(&self) {
        self.cancel.store(true, Relaxed);
    }

    /// Empty the index for the next transfer root.
    pub fn reset(&self) {
        self.dirs.clear();
        *self.done.lock().unwrap() = false;
        self.cancel.store(false, Relaxed);
    }

    /// The entries of directory `dir`, waiting for the scan to reach it. `None` when the
    /// scan never lists it (it didn't exist, couldn't be read, or is a symlink kept by
    /// `-K`): the caller reads the directory itself.
    pub fn take(&self, dir: &Path) -> Option<HashMap<OsString, Meta>> {
        let mut done = self.done.lock().unwrap();
        loop {
            if let Some((_, v)) = self.dirs.remove(dir) {
                return Some(v);
            }
            if *done {
                return None;
            }
            done = self.ready.wait(done).unwrap();
        }
    }
}

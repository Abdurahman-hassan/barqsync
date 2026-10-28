//! Several SRC arguments that land in one destination are one tree, as in rsync's file
//! list. A name several of them have comes from the first one, except that a directory
//! wins over a non-directory, and directories of the same name merge their contents.
//! `--delete` then removes only what none of them has.
//!
//! A destination directory is fed by its sources' directories of that path; their
//! listings are merged here. The local engine and the remote sender both do this, so a
//! remote receiver sees a single, already merged root.

use crate::entry::{DirEnt, Kind};
use crate::filter::DirRules;
use crate::select::{self, Descend, Sel};
use std::collections::HashMap;
use std::ffi::OsStr;
use std::os::unix::ffi::OsStrExt;
use std::path::PathBuf;

/// One source directory feeding a destination directory.
#[derive(Debug, Clone)]
pub(crate) struct SrcDir {
    pub(crate) path: PathBuf,
    /// `-R`/`--files-from`, or the names given as `src` arguments: only these entries.
    pub(crate) sel: Option<&'static Sel>,
    /// `-x`: the device of the transfer root this directory is in (`None`: each entry
    /// is a root of its own, as named items are).
    pub(crate) dev: Option<u64>,
    /// Filter state (see [`DirRules`]): its parent's until this directory is read, then
    /// its own.
    pub(crate) rules: DirRules,
}

/// Where the entries of a merged listing come from.
#[derive(Debug, Default)]
pub(crate) struct Sources {
    /// The directories merged, in precedence order.
    pub(crate) dirs: Vec<SrcDir>,
    /// For each entry, the index in `dirs` of the directory it comes from (empty: there
    /// is one directory). Full width: any number of sources can be given.
    origin: Vec<usize>,
    /// Directory entries that later directories have as directories too: entry index ->
    /// those directories (index in `dirs`) and the device of each one's directory.
    also: HashMap<u32, Vec<(usize, u64)>>,
}

impl Sources {
    /// The source directory holding entry `idx`.
    pub(crate) fn dir_of(&self, idx: u32) -> &SrcDir {
        &self.dirs[self.origin.get(idx as usize).copied().unwrap_or(0)]
    }

    /// Whether directory entry `idx` (named `name`) is a top directory (see
    /// [`select::top_dir`]) for any of the sources that have it, as rsync's file list
    /// keeps that mark when it merges names.
    pub(crate) fn top(&self, idx: u32, name: &OsStr, recursive: bool) -> bool {
        let own = self.origin.get(idx as usize).copied().unwrap_or(0);
        let also = self.also.get(&idx).into_iter().flatten().map(|&(k, _)| k);
        std::iter::once(own).chain(also).any(|k| select::top_dir(self.dirs[k].sel, name.as_bytes(), recursive))
    }

    /// For each entry, the index in `dirs` of the directory it is from (empty: the first).
    pub(crate) fn origins(&self) -> &[usize] {
        &self.origin
    }

    /// The later sources' directories that merge into directory entry `idx` (named
    /// `name`), each with its selection and root device. Those that `-x` or their
    /// selection leave out, and repeats of a directory already merged, are dropped. With
    /// `-x`, these fill the directory alone when entry `idx` is a mount point.
    pub(crate) fn also_dirs(&self, idx: u32, name: &OsStr, recursive: bool, one_fs: bool) -> Vec<SrcDir> {
        let Some(also) = self.also.get(&idx) else { return Vec::new() };
        let own = self.dir_of(idx).path.join(name);
        let mut out: Vec<SrcDir> = Vec::new();
        for &(k, dev) in also {
            let d = &self.dirs[k];
            let root_dev = select::root_dev(d.sel, name.as_bytes(), dev, d.dev);
            if one_fs && dev != root_dev {
                continue;
            }
            let sel = match select::descend(d.sel, name.as_bytes(), recursive) {
                Descend::No => continue,
                Descend::All => None,
                Descend::Some(s) => Some(s),
            };
            let path = d.path.join(name);
            if path != own && out.iter().all(|o| o.path != path) {
                out.push(SrcDir { path, sel, dev: Some(root_dev), rules: d.rules.clone() });
            }
        }
        out
    }
}

/// Merge the listings of `dirs` (each sorted by name; `lists[i]` is `dirs[i]`'s) into one
/// listing sorted by name, keeping rsync's precedence for names in several.
pub(crate) fn merge(dirs: Vec<SrcDir>, mut lists: Vec<Vec<DirEnt>>) -> (Vec<DirEnt>, Sources) {
    if let ([_], [_]) = (dirs.as_slice(), lists.as_slice()) {
        // One directory: nothing to merge.
        return (lists.pop().unwrap_or_default(), Sources { dirs, ..Sources::default() });
    }
    let mut all: Vec<(usize, DirEnt)> =
        lists.into_iter().enumerate().flat_map(|(k, l)| l.into_iter().map(move |e| (k, e))).collect();
    // Stable: entries of one name stay in source order.
    all.sort_by(|a, b| a.1.name.as_bytes().cmp(b.1.name.as_bytes()));
    let mut entries = Vec::with_capacity(all.len());
    let mut src = Sources { dirs, ..Sources::default() };
    let mut all = all.into_iter().peekable();
    while let Some(first) = all.next() {
        let mut group = vec![first];
        while let Some(next) = all.next_if(|n| n.1.name == group[0].1.name) {
            group.push(next);
        }
        let win = group.iter().position(|(_, e)| e.meta.kind == Kind::Dir).unwrap_or(0);
        if group[win].1.meta.kind == Kind::Dir {
            let also: Vec<(usize, u64)> = group[win + 1..]
                .iter()
                .filter(|(_, e)| e.meta.kind == Kind::Dir)
                .map(|(k, e)| (*k, e.meta.dev))
                .collect();
            if !also.is_empty() {
                src.also.insert(entries.len() as u32, also);
            }
        }
        let (k, e) = group.swap_remove(win);
        src.origin.push(k);
        entries.push(e);
    }
    (entries, src)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::entry::Meta;
    use std::ffi::OsString;

    fn ent(name: &str, kind: Kind, size: u64) -> DirEnt {
        let meta = Meta { kind, size, mtime: 0, mtime_ns: 0, mode: 0o755, uid: 0, gid: 0, dev: 1, ino: size, rdev: 0, nlink: 1 };
        DirEnt::new(OsString::from(name), meta, None)
    }

    fn dir(p: &str) -> SrcDir {
        SrcDir { path: PathBuf::from(p), sel: None, dev: Some(1), rules: DirRules::default() }
    }

    #[test]
    fn first_source_wins_and_directories_merge() {
        let a = vec![ent("d", Kind::File, 1), ent("f", Kind::File, 2), ent("m", Kind::Dir, 3), ent("x", Kind::Dir, 4)];
        let b = vec![ent("d", Kind::Dir, 5), ent("f", Kind::File, 6), ent("m", Kind::Dir, 7), ent("only-b", Kind::File, 8), ent("x", Kind::File, 9)];
        let (entries, src) = merge(vec![dir("a"), dir("b")], vec![a, b]);
        let got: Vec<(String, u64, PathBuf)> = entries
            .iter()
            .enumerate()
            .map(|(i, e)| (e.name.to_string_lossy().into_owned(), e.meta.size, src.dir_of(i as u32).path.clone()))
            .collect();
        let want = [("d", 5, "b"), ("f", 2, "a"), ("m", 3, "a"), ("only-b", 8, "b"), ("x", 4, "a")];
        assert_eq!(got, want.map(|(n, s, p)| (n.to_string(), s, PathBuf::from(p))));
        // Only `m` is a directory in both: b/m merges into a/m.
        let m = src.also_dirs(2, OsStr::new("m"), true, false);
        assert_eq!(m.iter().map(|d| d.path.clone()).collect::<Vec<_>>(), [PathBuf::from("b/m")]);
        assert!(src.also_dirs(0, OsStr::new("d"), true, false).is_empty());
        assert!(src.also_dirs(4, OsStr::new("x"), true, false).is_empty());
    }

    #[test]
    fn any_number_of_sources_keep_their_own_entries() {
        // More sources than a u16 can count: each entry still comes from its own.
        let n = 70_000;
        let dirs: Vec<SrcDir> = (0..n).map(|i| dir(&format!("s{i}"))).collect();
        let lists: Vec<Vec<DirEnt>> = (0..n).map(|i| vec![ent(&format!("e{i:06}"), Kind::File, i as u64)]).collect();
        let (entries, src) = merge(dirs, lists);
        assert_eq!(entries.len(), n);
        for (i, e) in entries.iter().enumerate() {
            let k = e.meta.size as usize;
            assert_eq!(src.dir_of(i as u32).path, PathBuf::from(format!("s{k}")), "{:?}", e.name);
        }
    }

    #[test]
    fn the_same_directory_twice_is_merged_once() {
        let a = vec![ent("m", Kind::Dir, 1)];
        let (_, src) = merge(vec![dir("a"), dir("a"), dir("c")], vec![a.clone(), a.clone(), a]);
        let m = src.also_dirs(0, OsStr::new("m"), true, false);
        assert_eq!(m.iter().map(|d| d.path.clone()).collect::<Vec<_>>(), [PathBuf::from("c/m")]);
    }
}

//! Path selection for `-R/--relative` and `--files-from`: the named paths form a tree.
//! Directories on the way to a named path are "implied": they are created with their
//! source attributes, but only the selected children inside them are transferred, and
//! nothing is deleted from them.

use crate::entry::{DirEnt, Kind};
use crate::fsops;
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::ffi::{OsStr, OsString};
use std::io;
use std::os::unix::ffi::OsStrExt;
use std::path::{Path, PathBuf};

#[derive(Debug, Default, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[derive(rkyv::Archive, rkyv::Serialize, rkyv::Deserialize)]
// The tree is recursive: rkyv needs its bounds spelled out.
#[rkyv(serialize_bounds(__S: rkyv::ser::Writer + rkyv::ser::Allocator, __S::Error: rkyv::rancor::Source))]
#[rkyv(deserialize_bounds(__D::Error: rkyv::rancor::Source))]
#[rkyv(bytecheck(bounds(__C: rkyv::validation::ArchiveContext, __C::Error: rkyv::rancor::Source)))]
pub struct Sel {
    /// The path itself was named (a SRC argument or a `--files-from` line).
    pub named: bool,
    /// Named with a trailing `/` (or as `.`): all its children are listed, one level
    /// deep unless recursing.
    pub contents: bool,
    #[rkyv(omit_bounds)]
    pub children: BTreeMap<Vec<u8>, Sel>,
}

/// The selected paths below one base directory.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SelRoot {
    pub base: PathBuf,
    /// The base directory itself is part of the transfer (shown as `./`).
    pub with_root: bool,
    pub sel: Sel,
}

/// What happens to a subdirectory of a directory being transferred.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Descend<'a> {
    /// Created (with `-d`), but its contents are not transferred.
    No,
    /// Everything below it is transferred.
    All,
    /// Only the selected entries below it are transferred.
    Some(&'a Sel),
}

impl Sel {
    fn insert(&mut self, comps: &[&[u8]], contents: bool) {
        match comps.split_first() {
            None => {
                self.named = true;
                self.contents |= contents;
            }
            Some((first, rest)) => self.children.entry(first.to_vec()).or_default().insert(rest, contents),
        }
    }

    /// The selection to apply at the top of a transfer root (`None`: everything).
    pub fn at_root(&self, recursive: bool) -> Option<&Sel> {
        if recursive && self.named { None } else { Some(self) }
    }

    /// The named entries of one directory (`src` arguments without a trailing slash),
    /// each transferred whole.
    pub fn named<'a>(names: impl IntoIterator<Item = &'a [u8]>) -> Sel {
        let mut s = Sel::default();
        for n in names {
            s.children.insert(n.to_vec(), Sel { named: true, ..Sel::default() });
        }
        s
    }

    /// Add `other`'s paths to this selection (several `-R` roots land in one destination).
    pub fn merge(&mut self, other: &Sel) {
        self.named |= other.named;
        self.contents |= other.contents;
        for (name, c) in &other.children {
            self.children.entry(name.clone()).or_default().merge(c);
        }
    }
}

pub fn leak(sel: Sel) -> &'static Sel {
    Box::leak(Box::new(sel))
}

/// How to handle child directory `name` of a directory whose selection is `parent`
/// (`None`: the whole directory is being transferred).
pub fn descend<'a>(parent: Option<&'a Sel>, name: &[u8], recursive: bool) -> Descend<'a> {
    let all_or_no = if recursive { Descend::All } else { Descend::No };
    let Some(p) = parent else { return all_or_no };
    match p.children.get(name) {
        // Listed because its parent's contents are being sent.
        None => all_or_no,
        Some(c) if recursive && c.named => Descend::All,
        Some(c) if c.contents || !c.children.is_empty() => Descend::Some(c),
        Some(_) => Descend::No,
    }
}

/// `--delete` only prunes directories whose full contents are being sent.
pub fn may_delete(sel: Option<&Sel>) -> bool {
    sel.is_none_or(|s| s.contents)
}

/// Normalize a relative path: drop empty and `.` components, fold `x/..`. Returns the
/// components and whether the path started with `./` (or was `.`).
fn components(rel: &[u8]) -> (Vec<&[u8]>, bool) {
    let mut out: Vec<&[u8]> = Vec::new();
    let mut dot_first = false;
    for (i, c) in rel.split(|&b| b == b'/').enumerate() {
        match c {
            b"" => {}
            b"." => dot_first |= i == 0,
            b".." => {
                out.pop();
            }
            c => out.push(c),
        }
    }
    (out, dot_first)
}

fn add(roots: &mut Vec<SelRoot>, base: PathBuf, rel: &[u8]) {
    let contents = rel.ends_with(b"/") || rel.is_empty();
    let (comps, dot_first) = components(rel);
    let idx = match roots.iter().position(|r| r.base == base) {
        Some(i) => i,
        None => {
            roots.push(SelRoot { base, with_root: false, sel: Sel::default() });
            roots.len() - 1
        }
    };
    let r = &mut roots[idx];
    r.with_root |= dot_first || comps.is_empty();
    r.sel.insert(&comps, contents || comps.is_empty());
}

/// Split at a `/./` marker: the part before it is not part of the transferred name.
fn split_marker(p: &[u8]) -> Option<(&[u8], &[u8])> {
    let i = p.windows(3).position(|w| w == b"/./")?;
    Some((&p[..i], &p[i + 3..]))
}

fn path_of(b: &[u8]) -> PathBuf {
    PathBuf::from(OsStr::from_bytes(b))
}

/// `-R`: each SRC is transferred under its full relative path (or the part after `/./`).
pub fn from_relative_args(sources: &[OsString]) -> Vec<SelRoot> {
    let mut roots = Vec::new();
    for s in sources {
        let b = s.as_bytes();
        let (base, rel) = match split_marker(b) {
            Some((pre, rel)) => (if pre.is_empty() { PathBuf::from("/") } else { path_of(pre) }, rel),
            None if b.starts_with(b"/") => (PathBuf::from("/"), b),
            None => (PathBuf::from("."), b),
        };
        add(&mut roots, base, rel);
    }
    roots
}

/// `--files-from`: each line names a path relative to `src` (a leading `/` is ignored).
pub fn from_list(src: &OsStr, lines: &[Vec<u8>]) -> Vec<SelRoot> {
    let src = Path::new(src);
    let mut roots = Vec::new();
    for l in lines {
        let l = &l[l.iter().take_while(|&&c| c == b'/').count()..];
        match split_marker(l) {
            Some((pre, rel)) => add(&mut roots, src.join(OsStr::from_bytes(pre)), rel),
            None => add(&mut roots, src.to_path_buf(), l),
        }
    }
    roots
}

/// The listing of a directory restricted to `sel`: `lstat` of each selected child, or
/// the whole directory when its contents were named. Missing names are returned as errors.
pub fn read_selected(dir: &Path, sel: &Sel) -> (Vec<DirEnt>, Vec<(PathBuf, io::Error)>) {
    if sel.contents {
        return match fsops::read_dir_meta(dir) {
            Ok(v) => (v, Vec::new()),
            Err(e) => (Vec::new(), vec![(dir.to_path_buf(), e)]),
        };
    }
    let mut ents = Vec::with_capacity(sel.children.len());
    let mut errs = Vec::new();
    for name in sel.children.keys() {
        let p = dir.join(OsStr::from_bytes(name));
        match fsops::lstat(&p) {
            Ok(meta) => {
                let link = if meta.kind == Kind::Symlink { fsops::read_link_bytes(&p).ok() } else { None };
                ents.push(DirEnt::new(OsStr::from_bytes(name).to_os_string(), meta, link));
            }
            Err(e) => errs.push((p, e)),
        }
    }
    (ents, errs)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn names(s: &Sel) -> Vec<String> {
        s.children.keys().map(|k| String::from_utf8_lossy(k).into_owned()).collect()
    }

    #[test]
    fn relative_args() {
        let roots = from_relative_args(&["src/x/y/f".into(), "src/z".into(), "a/./b/c".into(), "/abs/f".into()]);
        assert_eq!(roots.len(), 3);
        assert_eq!(roots[0].base, PathBuf::from("."));
        let src = &roots[0].sel.children[&b"src".to_vec()];
        assert!(!src.named);
        assert_eq!(names(src), ["x", "z"]);
        assert!(src.children[&b"z".to_vec()].named);
        assert_eq!(roots[1].base, PathBuf::from("a"));
        assert_eq!(names(&roots[1].sel), ["b"]);
        assert_eq!(roots[2].base, PathBuf::from("/"));
        assert_eq!(names(&roots[2].sel), ["abs"]);
    }

    #[test]
    fn list_lines() {
        let lines: Vec<Vec<u8>> = ["w/", "./x/g", "/z", "w/deep/../i", "x/./y/f"].iter().map(|l| l.as_bytes().to_vec()).collect();
        let roots = from_list(OsStr::new("src"), &lines);
        assert_eq!(roots.len(), 2);
        let r = &roots[0];
        assert!(r.with_root, "./x/g sends the root itself");
        assert_eq!(names(&r.sel), ["w", "x", "z"]);
        let w = &r.sel.children[&b"w".to_vec()];
        assert!(w.named && w.contents);
        assert_eq!(names(w), ["i"]);
        assert_eq!(roots[1].base, PathBuf::from("src/x"));
        assert_eq!(names(&roots[1].sel), ["y"]);
    }

    #[test]
    fn descend_rules() {
        let roots = from_list(OsStr::new("s"), &[b"a/b/c".to_vec(), b"d".to_vec(), b"e/".to_vec()]);
        let s = &roots[0].sel;
        assert!(matches!(descend(Some(s), b"a", false), Descend::Some(_)));
        assert_eq!(descend(Some(s), b"d", false), Descend::No);
        assert_eq!(descend(Some(s), b"d", true), Descend::All);
        assert!(matches!(descend(Some(s), b"e", false), Descend::Some(_)));
        assert_eq!(descend(Some(s), b"unlisted", false), Descend::No);
        assert_eq!(descend(None, b"x", true), Descend::All);
        assert!(!may_delete(Some(s)));
        assert!(may_delete(None));
    }
}

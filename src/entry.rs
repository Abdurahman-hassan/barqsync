//! File metadata model shared by the scanner, the diff engine and the wire protocol.

use serde::{Deserialize, Serialize};
use std::ffi::OsString;
use std::fs::{FileType, Metadata};
use std::os::unix::fs::{FileTypeExt, MetadataExt};
use std::sync::atomic::AtomicU8;
use std::sync::atomic::Ordering::Relaxed;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[derive(rkyv::Archive, rkyv::Serialize, rkyv::Deserialize)]
pub enum Kind {
    File,
    Dir,
    Symlink,
    Fifo,
    CharDev,
    BlockDev,
    Socket,
}

impl Kind {
    pub fn from_file_type(ft: FileType) -> Kind {
        if ft.is_file() {
            Kind::File
        } else if ft.is_dir() {
            Kind::Dir
        } else if ft.is_symlink() {
            Kind::Symlink
        } else if ft.is_fifo() {
            Kind::Fifo
        } else if ft.is_char_device() {
            Kind::CharDev
        } else if ft.is_block_device() {
            Kind::BlockDev
        } else {
            Kind::Socket
        }
    }

    pub fn is_device(self) -> bool {
        matches!(self, Kind::CharDev | Kind::BlockDev)
    }

    pub fn is_special(self) -> bool {
        matches!(self, Kind::Fifo | Kind::CharDev | Kind::BlockDev | Kind::Socket)
    }
}

/// The subset of `lstat` results that sync decisions depend on.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[derive(rkyv::Archive, rkyv::Serialize, rkyv::Deserialize)]
pub struct Meta {
    pub kind: Kind,
    pub size: u64,
    pub mtime: i64,
    pub mtime_ns: u32,
    /// Permission bits including setuid/setgid/sticky (`st_mode & 0o7777`).
    pub mode: u32,
    pub uid: u32,
    pub gid: u32,
    pub dev: u64,
    pub ino: u64,
    pub rdev: u64,
    /// Hard link count (`-H` groups files with `nlink > 1` by `(dev, ino)`).
    pub nlink: u64,
    /// `-U`: the access time, in seconds (as rsync keeps it); 0 for a directory, and
    /// unless read (see [`read_times`]).
    pub atime: i64,
    /// `-N`: the creation time, in seconds (macOS); 0 unless read.
    pub crtime: i64,
}

impl Meta {
    pub fn from_metadata(md: &Metadata) -> Meta {
        let kind = Kind::from_file_type(md.file_type());
        #[cfg(target_os = "macos")]
        let crtime = std::os::macos::fs::MetadataExt::st_birthtime(md);
        #[cfg(not(target_os = "macos"))]
        let crtime = 0;
        let (atime, crtime) = Meta::extra_times(kind, md.atime(), crtime);
        Meta {
            kind,
            size: md.len(),
            mtime: md.mtime(),
            mtime_ns: md.mtime_nsec() as u32,
            mode: md.mode() & 0o7777,
            uid: md.uid(),
            gid: md.gid(),
            dev: md.dev(),
            ino: md.ino(),
            rdev: md.rdev(),
            nlink: md.nlink(),
            atime,
            crtime,
        }
    }

    /// The access and creation times metadata keeps, of those `lstat` gave (see
    /// [`read_times`]).
    pub(crate) fn extra_times(kind: Kind, atime: i64, crtime: i64) -> (i64, i64) {
        let t = EXTRA_TIMES.load(Relaxed);
        (if t & ATIME != 0 && kind != Kind::Dir { atime } else { 0 }, if t & CRTIME != 0 { crtime } else { 0 })
    }
}

/// `-U`, `-N`: which of the access and creation times metadata carries from now on.
/// Left out, they are 0: nothing compares them, and the file list stays small.
static EXTRA_TIMES: AtomicU8 = AtomicU8::new(0);
const ATIME: u8 = 1;
const CRTIME: u8 = 2;

pub fn read_times(atime: bool, crtime: bool) {
    EXTRA_TIMES.store(if atime { ATIME } else { 0 } | if crtime { CRTIME } else { 0 }, Relaxed);
}

/// Whether metadata carries access times (`-U`).
pub fn reads_atime() -> bool {
    EXTRA_TIMES.load(Relaxed) & ATIME != 0
}

/// Whether metadata carries creation times (`-N`).
pub fn reads_crtime() -> bool {
    EXTRA_TIMES.load(Relaxed) & CRTIME != 0
}

/// One directory child: its name and `lstat` metadata.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[derive(rkyv::Archive, rkyv::Serialize, rkyv::Deserialize)]
pub struct DirEnt {
    #[rkyv(with = crate::proto::wire::OsBytes)]
    pub name: OsString,
    pub meta: Meta,
    /// Symlink target, read at scan time so a listing is self-contained (it may be
    /// applied on another machine).
    pub link: Option<Vec<u8>>,
    /// `-X`/`-A`: extended attributes and ACLs (`None`: none, or not asked for).
    pub extra: Option<Box<Extra>>,
    /// `--delete-missing-args`: a named source that doesn't exist, whose destination is
    /// deleted (rsync's file-list entry of mode 0). Its `meta` means nothing.
    pub missing: bool,
}

impl DirEnt {
    pub fn new(name: OsString, meta: Meta, link: Option<Vec<u8>>) -> DirEnt {
        DirEnt { name, meta, link, extra: None, missing: false }
    }

    /// A missing named source (see [`DirEnt::missing`]).
    pub fn missing(name: OsString) -> DirEnt {
        let meta = Meta { kind: Kind::File, size: 0, mtime: 0, mtime_ns: 0, mode: 0, uid: 0, gid: 0, dev: 0, ino: 0, rdev: 0, nlink: 0, atime: 0, crtime: 0 };
        DirEnt { name, meta, link: None, extra: None, missing: true }
    }
}

/// An entry's extended attributes (`-X`, sorted by name) and ACLs (`-A`; `None` when the
/// mode says it all). In a listing they are in the sender's form (wire names, condensed
/// ACLs); the engine works with the receiver's form (see `Engine::local_extra`).
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[derive(rkyv::Archive, rkyv::Serialize, rkyv::Deserialize)]
pub struct Extra {
    pub xattrs: Vec<crate::xattrs::Xattr>,
    pub acl: Option<crate::acls::Acls>,
}

impl Extra {
    /// Boxed, or `None` when there is nothing to carry.
    pub fn boxed(self) -> Option<Box<Extra>> {
        (self != Extra::default()).then(|| Box::new(self))
    }
}

//! File metadata model shared by the scanner, the diff engine and the wire protocol.

use serde::{Deserialize, Serialize};
use std::ffi::OsString;
use std::fs::{FileType, Metadata};
use std::os::unix::fs::{FileTypeExt, MetadataExt};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
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

    pub fn is_special(self) -> bool {
        matches!(self, Kind::Fifo | Kind::CharDev | Kind::BlockDev | Kind::Socket)
    }
}

/// The subset of `lstat` results that sync decisions depend on.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
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
}

impl Meta {
    pub fn from_metadata(md: &Metadata) -> Meta {
        Meta {
            kind: Kind::from_file_type(md.file_type()),
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
        }
    }
}

/// One directory child: its name and `lstat` metadata.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DirEnt {
    pub name: OsString,
    pub meta: Meta,
    /// Symlink target, read at scan time so a listing is self-contained (it may be
    /// applied on another machine).
    pub link: Option<Vec<u8>>,
}

//! barqsync: a parallel, rsync-compatible file synchronizer.

pub mod acls;
pub mod bwlimit;
pub mod checksum;
pub mod chmod;
pub mod cli;
pub mod delta;
pub mod diff;
pub mod entry;
pub mod filter;
pub mod fsops;
pub mod ids;
pub mod local;
pub mod logformat;
pub mod output;
pub mod paths;
pub mod progress;
pub mod proto;
pub mod remote;
pub mod scan;
pub mod select;
pub mod timing;
pub mod xattrs;

use cli::Options;
use paths::{Location, parse_location};

/// Exit codes, matching rsync's where they overlap.
pub mod exit {
    pub const OK: i32 = 0;
    pub const SYNTAX: i32 = 1;
    pub const INCOMPATIBLE: i32 = 2;
    /// Requested action not supported (rsync's `RERR_UNSUPPORTED`).
    pub const UNSUPPORTED: i32 = 4;
    pub const FILE_IO: i32 = 11;
    pub const PROTOCOL: i32 = 12;
    pub const IPC: i32 = 14;
    pub const SIGNAL: i32 = 20;
    pub const PARTIAL: i32 = 23;
    /// Source files vanished before they could be sent.
    pub const VANISHED: i32 = 24;
    /// `--max-delete` stopped deletions.
    pub const DEL_LIMIT: i32 = 25;
    /// `--timeout`: no data moved for that long.
    pub const TIMEOUT: i32 = 30;
    /// The remote shell failed (e.g. ssh could not connect).
    pub const RSH: i32 = 255;
}

pub fn run(opts: Options) -> i32 {
    timing::init(opts.server);
    proto::async_io::set_enabled(opts.net_io_tokio);
    if opts.server {
        return remote::server::run();
    }
    fsops::cache::set_enabled(opts.drop_cache);
    // rsync: a munged symlink must not lead anywhere, which it would if this existed.
    if opts.munge_links && std::fs::metadata("/rsyncd-munged").is_ok_and(|m| m.is_dir()) {
        eprintln!("barqsync: Symlink munging is unsafe when a /rsyncd-munged directory exists.");
        return exit::UNSUPPORTED;
    }
    let paths = &opts.paths;
    if paths.len() < 2 {
        eprintln!("barqsync: need at least one SRC and a DEST (listing mode is not supported)");
        return exit::SYNTAX;
    }
    if opts.files_from.is_some() && paths.len() != 2 {
        eprintln!("barqsync: --files-from takes exactly one SRC and a DEST");
        return exit::SYNTAX;
    }
    let (sources, dest) = paths.split_at(paths.len() - 1);
    let dest = &dest[0];
    let mut any_remote = false;
    for path in paths {
        match parse_location(path) {
            Ok(Location::Remote { .. }) => any_remote = true,
            Ok(Location::Local(_)) => {}
            Err(e) => {
                eprintln!("barqsync: {e}");
                return exit::SYNTAX;
            }
        }
    }
    if any_remote {
        return remote::client::run(&opts, sources, dest);
    }
    local::run(&opts, sources, dest)
}

//! fastrsync: a parallel, rsync-compatible file synchronizer.

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
pub mod output;
pub mod paths;
pub mod progress;
pub mod proto;
pub mod remote;
pub mod select;

use cli::Options;
use paths::{Location, parse_location};

/// Exit codes, matching rsync's where they overlap.
pub mod exit {
    pub const OK: i32 = 0;
    pub const SYNTAX: i32 = 1;
    pub const INCOMPATIBLE: i32 = 2;
    pub const FILE_IO: i32 = 11;
    pub const PROTOCOL: i32 = 12;
    pub const IPC: i32 = 14;
    pub const SIGNAL: i32 = 20;
    pub const PARTIAL: i32 = 23;
    /// The remote shell failed (e.g. ssh could not connect).
    pub const RSH: i32 = 255;
}

pub fn run(opts: Options) -> i32 {
    if opts.server {
        return remote::server::run();
    }
    fsops::cache::set_enabled(opts.drop_cache);
    let paths = &opts.paths;
    if paths.len() < 2 {
        eprintln!("fastrsync: need at least one SRC and a DEST (listing mode is not supported)");
        return exit::SYNTAX;
    }
    if opts.files_from.is_some() && paths.len() != 2 {
        eprintln!("fastrsync: --files-from takes exactly one SRC and a DEST");
        return exit::SYNTAX;
    }
    let (sources, dest) = paths.split_at(paths.len() - 1);
    let dest = &dest[0];
    let any_remote = paths.iter().any(|p| matches!(parse_location(p), Location::Remote { .. }));
    if any_remote {
        return remote::client::run(&opts, sources, dest);
    }
    local::run(&opts, sources, dest)
}

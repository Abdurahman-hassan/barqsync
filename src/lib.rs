//! fastrsync: a parallel, rsync-compatible file synchronizer.

pub mod cli;
pub mod diff;
pub mod entry;
pub mod filter;
pub mod fsops;
pub mod local;
pub mod output;
pub mod paths;

use cli::Options;
use paths::{Location, parse_location};

/// Exit codes, matching rsync's where they overlap.
pub mod exit {
    pub const OK: i32 = 0;
    pub const SYNTAX: i32 = 1;
    pub const FILE_IO: i32 = 11;
    pub const IPC: i32 = 14;
    pub const SIGNAL: i32 = 20;
    pub const PARTIAL: i32 = 23;
}

pub fn run(opts: Options) -> i32 {
    if opts.server {
        eprintln!("fastrsync: --server is not implemented yet");
        return exit::SYNTAX;
    }
    let paths = &opts.paths;
    if paths.len() < 2 {
        eprintln!("fastrsync: need at least one SRC and a DEST (listing mode is not supported)");
        return exit::SYNTAX;
    }
    let (sources, dest) = paths.split_at(paths.len() - 1);
    let dest = &dest[0];
    let any_remote = paths.iter().any(|p| matches!(parse_location(p), Location::Remote { .. }));
    if any_remote {
        eprintln!("fastrsync: remote transfers (host:path) are not implemented yet");
        return exit::SYNTAX;
    }
    local::run(&opts, sources, dest)
}

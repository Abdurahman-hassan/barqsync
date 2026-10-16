//! Command-line interface: an rsync-compatible subset of flags, resolved into [`Options`].

use crate::filter::{RuleSpec, read_rules_file};
use anyhow::Result;
use clap::{ArgAction, ArgMatches, CommandFactory, FromArgMatches, Parser};
use serde::{Deserialize, Serialize};
use std::ffi::OsString;
use std::path::PathBuf;

#[derive(Parser, Debug)]
#[command(
    name = "fastrsync",
    version,
    about = "A fast, parallel, rsync-compatible file synchronizer",
    disable_help_flag = true
)]
pub struct Cli {
    /// Print help
    #[arg(long, action = ArgAction::Help)]
    help: Option<bool>,

    /// Archive mode; equals -rlptgoD
    #[arg(short = 'a', long)]
    archive: bool,
    /// Recurse into directories
    #[arg(short = 'r', long)]
    recursive: bool,
    /// Copy symlinks as symlinks
    #[arg(short = 'l', long)]
    links: bool,
    /// Preserve hard links
    #[arg(short = 'H', long = "hard-links")]
    hard_links: bool,
    /// Preserve permissions
    #[arg(short = 'p', long)]
    perms: bool,
    /// Preserve modification times
    #[arg(short = 't', long)]
    times: bool,
    /// Preserve group
    #[arg(short = 'g', long)]
    group: bool,
    /// Preserve owner (super-user only)
    #[arg(short = 'o', long)]
    owner: bool,
    /// Same as --devices --specials
    #[arg(short = 'D')]
    devices_specials: bool,
    /// Preserve device files (super-user only)
    #[arg(long)]
    devices: bool,
    /// Preserve special files (FIFOs)
    #[arg(long)]
    specials: bool,

    /// Increase verbosity
    #[arg(short = 'v', long, action = ArgAction::Count)]
    verbose: u8,
    /// Output a change-summary for all updates (twice: also unchanged items)
    #[arg(short = 'i', long = "itemize-changes", action = ArgAction::Count)]
    itemize: u8,
    /// Suppress non-error messages
    #[arg(short = 'q', long)]
    quiet: bool,
    /// Perform a trial run with no changes made
    #[arg(short = 'n', long = "dry-run")]
    dry_run: bool,
    /// Output updates using the specified FORMAT (%n name, %L link target)
    #[arg(long = "out-format", value_name = "FORMAT")]
    out_format: Option<String>,
    /// Show progress during transfer
    #[arg(long)]
    progress: bool,
    /// Same as --partial --progress
    #[arg(short = 'P')]
    partial_progress: bool,
    /// Fine-grained output flags; `progress2` (or `progress`) shows overall progress
    #[arg(long, value_name = "FLAGS")]
    info: Vec<String>,
    /// Give some file-transfer stats
    #[arg(long)]
    stats: bool,
    /// Output numbers in a human-readable format
    #[arg(short = 'h', long = "human-readable")]
    human: bool,
    /// Leave high-bit chars unescaped in output
    #[arg(short = '8', long = "8-bit-output")]
    eight_bit_output: bool,

    /// Compress file data during the transfer
    #[arg(short = 'z', long)]
    compress: bool,
    /// Copy files whole (no delta-transfer)
    #[arg(short = 'W', long = "whole-file")]
    whole_file: bool,
    /// Always use the delta-transfer algorithm
    #[arg(long = "no-whole-file", overrides_with = "whole_file")]
    no_whole_file: bool,
    /// Write files directly at their final name (no temp file + rename)
    #[arg(long)]
    inplace: bool,
    /// fsync every written file
    #[arg(long)]
    fsync: bool,
    /// Never use copy-on-write clones / reflinks (benchmarking aid)
    #[arg(long = "no-clone", hide = true)]
    no_clone: bool,
    /// Keep big files (8 MiB+) out of the page cache (default)
    #[arg(long = "drop-cache")]
    drop_cache: bool,
    /// Let big files fill the page cache like small ones
    #[arg(long = "no-drop-cache", overrides_with = "drop_cache")]
    no_drop_cache: bool,

    /// Delete extraneous files from destination dirs
    #[arg(long)]
    delete: bool,
    /// Also delete excluded files from destination dirs
    #[arg(long = "delete-excluded")]
    delete_excluded: bool,
    /// Exclude files matching PATTERN
    #[arg(long, value_name = "PATTERN")]
    exclude: Vec<String>,
    /// Don't exclude files matching PATTERN
    #[arg(long, value_name = "PATTERN")]
    include: Vec<String>,
    /// Read exclude patterns from FILE
    #[arg(long = "exclude-from", value_name = "FILE")]
    exclude_from: Vec<PathBuf>,
    /// Read include patterns from FILE
    #[arg(long = "include-from", value_name = "FILE")]
    include_from: Vec<PathBuf>,

    /// Skip files that are newer on the receiver
    #[arg(short = 'u', long)]
    update: bool,
    /// Skip files that match in size
    #[arg(long = "size-only")]
    size_only: bool,
    /// Don't skip files that match size and time
    #[arg(short = 'I', long = "ignore-times")]
    ignore_times: bool,
    /// Skip updating files that exist on receiver
    #[arg(long = "ignore-existing")]
    ignore_existing: bool,
    /// Don't cross filesystem boundaries
    #[arg(short = 'x', long = "one-file-system")]
    one_file_system: bool,
    /// Set the accuracy for mod-time comparisons (negative compares nanoseconds)
    #[arg(short = '@', long = "modify-window", value_name = "NUM", allow_negative_numbers = true, default_value_t = 0)]
    modify_window: i64,

    /// Specify the remote shell to use
    #[arg(short = 'e', long = "rsh", value_name = "COMMAND")]
    rsh: Option<String>,
    /// Path to fastrsync on the remote machine
    #[arg(long = "remote-path", visible_alias = "rsync-path", value_name = "PROGRAM")]
    remote_path: Option<String>,

    /// Number of worker threads (default: 2x CPU cores)
    #[arg(long, value_name = "N")]
    threads: Option<usize>,
    /// Number of parallel TCP data streams for remote transfers
    #[arg(long, value_name = "N")]
    streams: Option<usize>,

    #[arg(long, hide = true)]
    server: bool,
    #[arg(long, hide = true)]
    sender: bool,

    /// SRC... DEST
    #[arg(value_name = "PATH", required_unless_present = "server", num_args = 1..)]
    paths: Vec<OsString>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Options {
    pub recursive: bool,
    pub links: bool,
    pub hard_links: bool,
    pub perms: bool,
    pub times: bool,
    pub group: bool,
    pub owner: bool,
    pub devices: bool,
    pub specials: bool,

    pub verbose: u8,
    pub itemize: u8,
    pub quiet: bool,
    pub dry_run: bool,
    pub out_format: Option<String>,
    pub progress: bool,
    pub stats: bool,
    pub human: bool,
    pub eight_bit_output: bool,

    pub compress: bool,
    /// `None` means "decide automatically" (whole-file for local copies, delta for remote).
    pub whole_file: Option<bool>,
    pub inplace: bool,
    pub fsync: bool,
    pub no_clone: bool,
    /// Page-cache hints for big files (see `fsops::cache`).
    pub drop_cache: bool,

    pub delete: bool,
    pub delete_excluded: bool,
    pub filter_rules: Vec<RuleSpec>,

    pub update: bool,
    pub size_only: bool,
    pub ignore_times: bool,
    pub ignore_existing: bool,
    pub one_file_system: bool,
    pub modify_window: i64,

    pub rsh: Option<String>,
    pub remote_path: Option<String>,
    pub threads: usize,
    pub streams: usize,

    pub server: bool,
    pub sender: bool,
    pub paths: Vec<OsString>,
}

impl Options {
    pub fn parse_from<I, T>(args: I) -> Result<Options>
    where
        I: IntoIterator<Item = T>,
        T: Into<OsString> + Clone,
    {
        let matches = Cli::command().try_get_matches_from(args)?;
        let cli = Cli::from_arg_matches(&matches)?;
        let filter_rules = ordered_filter_rules(&cli, &matches)?;
        Ok(Options::from_cli(cli, filter_rules))
    }

    fn from_cli(c: Cli, filter_rules: Vec<RuleSpec>) -> Options {
        let a = c.archive;
        let cores = std::thread::available_parallelism().map(|n| n.get()).unwrap_or(4);
        Options {
            recursive: c.recursive || a,
            links: c.links || a,
            hard_links: c.hard_links,
            perms: c.perms || a,
            times: c.times || a,
            group: c.group || a,
            owner: c.owner || a,
            devices: c.devices || c.devices_specials || a,
            specials: c.specials || c.devices_specials || a,
            verbose: if c.quiet { 0 } else { c.verbose },
            itemize: if c.quiet { 0 } else { c.itemize },
            quiet: c.quiet,
            dry_run: c.dry_run,
            out_format: c.out_format,
            progress: c.progress
                || c.partial_progress
                || c.info.iter().flat_map(|f| f.split(',')).any(|f| f.starts_with("progress") && f != "progress0"),
            stats: c.stats,
            human: c.human,
            eight_bit_output: c.eight_bit_output,
            compress: c.compress,
            whole_file: if c.whole_file {
                Some(true)
            } else if c.no_whole_file {
                Some(false)
            } else {
                None
            },
            inplace: c.inplace,
            fsync: c.fsync,
            no_clone: c.no_clone,
            drop_cache: c.drop_cache || !c.no_drop_cache,
            delete: c.delete || c.delete_excluded,
            delete_excluded: c.delete_excluded,
            filter_rules,
            update: c.update,
            size_only: c.size_only,
            ignore_times: c.ignore_times,
            ignore_existing: c.ignore_existing,
            one_file_system: c.one_file_system,
            modify_window: c.modify_window,
            rsh: c.rsh,
            remote_path: c.remote_path,
            // Two workers per core: on APFS that gives the fastest metadata-bound syncs
            // (filesystem latency is hidden by queue depth). Fewer threads use far less
            // kernel CPU for nearly the same speed; see README.
            threads: c.threads.filter(|&n| n > 0).unwrap_or((cores * 2).clamp(4, 64)),
            streams: c.streams.unwrap_or(0),
            server: c.server,
            sender: c.sender,
            paths: c.paths,
        }
    }
}

/// Include/exclude options are order-sensitive across flags, so rebuild the
/// combined list using each value's position on the command line.
fn ordered_filter_rules(cli: &Cli, m: &ArgMatches) -> Result<Vec<RuleSpec>> {
    let mut indexed: Vec<(usize, Vec<RuleSpec>)> = Vec::new();
    let mut push_patterns = |id: &str, values: &[String], include: bool| {
        if let Some(idx) = m.indices_of(id) {
            for (i, v) in idx.zip(values) {
                indexed.push((i, vec![RuleSpec { include, pattern: v.clone() }]));
            }
        }
    };
    push_patterns("exclude", &cli.exclude, false);
    push_patterns("include", &cli.include, true);
    for (id, files, include) in [
        ("exclude_from", &cli.exclude_from, false),
        ("include_from", &cli.include_from, true),
    ] {
        if let Some(idx) = m.indices_of(id) {
            for (i, f) in idx.zip(files) {
                indexed.push((i, read_rules_file(f, include)?));
            }
        }
    }
    indexed.sort_by_key(|(i, _)| *i);
    Ok(indexed.into_iter().flat_map(|(_, r)| r).collect())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn archive_expands() {
        let o = Options::parse_from(["fastrsync", "-a", "s/", "d"]).unwrap();
        assert!(o.recursive && o.links && o.perms && o.times && o.group && o.owner);
        assert!(o.devices && o.specials);
    }

    #[test]
    fn combined_short_flags() {
        let o = Options::parse_from(["fastrsync", "-avzn", "s/", "d"]).unwrap();
        assert!(o.recursive && o.compress && o.dry_run);
        assert_eq!(o.verbose, 1);
    }

    #[test]
    fn filter_order_is_preserved_across_flags() {
        let o = Options::parse_from([
            "fastrsync", "--include", "keep.log", "--exclude", "*.log", "--include", "x", "s", "d",
        ])
        .unwrap();
        let pats: Vec<_> = o.filter_rules.iter().map(|r| (r.include, r.pattern.as_str())).collect();
        assert_eq!(pats, vec![(true, "keep.log"), (false, "*.log"), (true, "x")]);
    }

    #[test]
    fn eight_bit_output() {
        assert!(!Options::parse_from(["fastrsync", "-a", "s", "d"]).unwrap().eight_bit_output);
        for args in [&["fastrsync", "-8", "s", "d"][..], &["fastrsync", "-av8", "s", "d"], &["fastrsync", "--8-bit-output", "s", "d"]] {
            assert!(Options::parse_from(args).unwrap().eight_bit_output, "{args:?}");
        }
    }

    #[test]
    fn negative_modify_window() {
        let o = Options::parse_from(["fastrsync", "--modify-window=-1", "s", "d"]).unwrap();
        assert_eq!(o.modify_window, -1);
    }
}

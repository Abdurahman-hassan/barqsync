//! Command-line interface: an rsync-compatible subset of flags, resolved into [`Options`].

use crate::chmod::ChmodRule;
use crate::delta::DeltaMode;
use crate::proto::Wire;
use crate::remote::{Strategy, Transport};
use crate::scan::Scanner;
use crate::filter::{RuleSpec, read_rules_file};
use anyhow::{Context, Result, bail};
use std::os::unix::ffi::OsStrExt;
use clap::{ArgAction, ArgMatches, CommandFactory, FromArgMatches, Parser};
use serde::{Deserialize, Serialize};
use std::ffi::OsString;
use std::path::PathBuf;

#[derive(Parser, Debug)]
#[command(
    name = "barqsync",
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
    /// Omit directories from --times
    #[arg(short = 'O', long = "omit-dir-times")]
    omit_dir_times: bool,
    /// Preserve group
    #[arg(short = 'g', long)]
    group: bool,
    /// Preserve owner (super-user only)
    #[arg(short = 'o', long)]
    owner: bool,
    /// Preserve ACLs (implies --perms)
    #[arg(short = 'A', long)]
    acls: bool,
    /// Preserve extended attributes (twice: also rsync's internal rsync.% ones)
    #[arg(short = 'X', long, action = ArgAction::Count)]
    xattrs: u8,
    /// Transform symlinks into the files/directories they point to
    #[arg(short = 'L', long = "copy-links")]
    copy_links: bool,
    /// Transform symlinks to directories into directories
    #[arg(short = 'k', long = "copy-dirlinks")]
    copy_dirlinks: bool,
    /// Treat symlinked directories on the receiver as directories
    #[arg(short = 'K', long = "keep-dirlinks")]
    keep_dirlinks: bool,
    /// Transfer directories without recursing
    #[arg(short = 'd', long)]
    dirs: bool,
    /// Use relative path names
    #[arg(short = 'R', long)]
    relative: bool,
    /// Read the list of source files from FILE (`-` for stdin)
    #[arg(long = "files-from", value_name = "FILE")]
    files_from: Option<OsString>,
    /// --files-from lists are delimited by NUL bytes
    #[arg(short = '0', long)]
    from0: bool,
    /// Affect file and/or directory permissions (e.g. `Dg+s,Fo-w`)
    #[arg(long, value_name = "CHMOD")]
    chmod: Vec<String>,
    /// Don't map uid/gid values by user/group name
    #[arg(long = "numeric-ids")]
    numeric_ids: bool,
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
    /// Keep partially transferred files
    #[arg(long)]
    partial: bool,
    /// Put a partially transferred file into DIR
    #[arg(long = "partial-dir", value_name = "DIR")]
    partial_dir: Option<OsString>,
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
    /// Directory scanner: one task per directory, or a jwalk tree scan feeding queues
    #[arg(long, value_enum, value_name = "MODE", default_value_t, env = "BARQSYNC_SCANNER", hide_env = true)]
    scanner: Scanner,
    /// Delta design: aligned blocks with a FastCDC fallback, or FastCDC throughout
    #[arg(long, value_enum, value_name = "MODE", default_value_t, env = "BARQSYNC_DELTA", hide_env = true)]
    delta: DeltaMode,
    /// Write files directly at their final name (no temp file + rename)
    #[arg(long)]
    inplace: bool,
    /// fsync every written file
    #[arg(long)]
    fsync: bool,
    /// Turn sequences of nulls into sparse blocks
    #[arg(short = 'S', long)]
    sparse: bool,
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
    /// Skip based on checksum, not mod-time & size
    #[arg(short = 'c', long)]
    checksum: bool,
    /// Skip creating new files on receiver
    #[arg(long, visible_alias = "ignore-non-existing")]
    existing: bool,
    /// Don't transfer any file larger than SIZE
    #[arg(long = "max-size", value_name = "SIZE")]
    max_size: Option<String>,
    /// Don't transfer any file smaller than SIZE
    #[arg(long = "min-size", value_name = "SIZE")]
    min_size: Option<String>,
    /// Make backups (see --suffix & --backup-dir)
    #[arg(short = 'b', long)]
    backup: bool,
    /// Make backups into hierarchy based in DIR
    #[arg(long = "backup-dir", value_name = "DIR")]
    backup_dir: Option<OsString>,
    /// Backup suffix (default ~ w/o --backup-dir)
    #[arg(long, value_name = "SUFFIX")]
    suffix: Option<String>,
    /// Also compare destination files relative to DIR
    #[arg(long = "compare-dest", value_name = "DIR")]
    compare_dest: Vec<OsString>,
    /// Hardlink to files in DIR when unchanged
    #[arg(long = "link-dest", value_name = "DIR")]
    link_dest: Vec<OsString>,
    /// Sender removes synchronized files (non-directory)
    #[arg(long = "remove-source-files")]
    remove_source_files: bool,
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
    /// Path to barqsync on the remote machine
    #[arg(long = "remote-path", visible_alias = "rsync-path", value_name = "PROGRAM")]
    remote_path: Option<String>,

    /// Number of worker threads (default: 2x CPU cores)
    #[arg(long, value_name = "N")]
    threads: Option<usize>,
    /// Number of parallel data streams for remote transfers (TCP, or QUIC streams)
    #[arg(long, value_name = "N")]
    streams: Option<usize>,
    /// Remote data path: the ssh pipe alone, TLS-over-TCP streams, QUIC, or auto (QUIC on long links)
    #[arg(long, value_enum, value_name = "MODE", env = "BARQSYNC_TRANSPORT", hide_env = true)]
    transport: Option<Transport>,
    /// Measure the link and pick delta vs whole files and compression from it, or don't
    #[arg(long, value_enum, value_name = "MODE", env = "BARQSYNC_STRATEGY", hide_env = true)]
    strategy: Option<Strategy>,
    /// zstd level for compressed frames (default: 1, or chosen from the link speed)
    #[arg(long = "compress-level", value_name = "N", value_parser = clap::value_parser!(i32).range(1..=19))]
    compress_level: Option<i32>,
    /// Linux: write received small files in io_uring batches
    #[arg(long = "io-uring", env = "BARQSYNC_IO_URING", hide_env = true, value_parser = clap::builder::FalseyValueParser::new())]
    io_uring: bool,
    /// Network I/O layer for the ssh pipe and TCP streams (benchmarking aid)
    #[arg(long = "net-io", value_name = "LAYER", value_parser = ["threads", "tokio"], hide = true, env = "BARQSYNC_NET_IO", hide_env = true)]
    net_io: Option<String>,
    /// Control-message encoding on the wire
    #[arg(long, value_enum, value_name = "FORMAT", default_value_t, env = "BARQSYNC_WIRE", hide_env = true)]
    wire: Wire,

    #[arg(long, hide = true)]
    server: bool,
    #[arg(long, hide = true)]
    sender: bool,

    /// SRC... DEST
    #[arg(value_name = "PATH", required_unless_present = "server", num_args = 1..)]
    paths: Vec<OsString>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[derive(rkyv::Archive, rkyv::Serialize, rkyv::Deserialize)]
pub struct Options {
    pub recursive: bool,
    pub links: bool,
    pub hard_links: bool,
    pub perms: bool,
    pub times: bool,
    /// `-O`, also forced by `--backup` without `--backup-dir` (rsync).
    pub omit_dir_times: bool,
    pub group: bool,
    pub owner: bool,
    /// `-A`
    pub acls: bool,
    /// Number of `-X` (0: off).
    pub xattrs: u8,
    pub devices: bool,
    pub specials: bool,
    pub copy_links: bool,
    pub copy_dirlinks: bool,
    pub keep_dirlinks: bool,
    pub dirs: bool,
    pub relative: bool,
    /// `--files-from`: the listed paths, read by the client (also sent to the server).
    pub files_from: Option<Vec<Vec<u8>>>,
    pub chmod: Vec<ChmodRule>,
    pub numeric_ids: bool,

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
    /// The receiver's choice decides a remote transfer (its signature type says which).
    pub delta: DeltaMode,
    pub scanner: Scanner,
    pub inplace: bool,
    pub fsync: bool,
    pub no_clone: bool,
    /// Page-cache hints for big files (see `fsops::cache`).
    pub drop_cache: bool,
    pub sparse: bool,
    /// `--partial` (also implied by `--partial-dir` and `-P`).
    pub partial: bool,
    #[rkyv(with = rkyv::with::Map<crate::proto::wire::OsBytes>)]
    pub partial_dir: Option<OsString>,

    pub delete: bool,
    pub delete_excluded: bool,
    pub filter_rules: Vec<RuleSpec>,

    pub update: bool,
    pub size_only: bool,
    pub ignore_times: bool,
    pub ignore_existing: bool,
    pub one_file_system: bool,
    pub modify_window: i64,
    pub checksum: bool,
    pub existing: bool,
    pub max_size: Option<u64>,
    pub min_size: Option<u64>,
    pub backup: bool,
    #[rkyv(with = rkyv::with::Map<crate::proto::wire::OsBytes>)]
    pub backup_dir: Option<OsString>,
    /// Resolved backup suffix: `--suffix`, else `~` without `--backup-dir`, else empty.
    pub suffix: String,
    /// `--link-dest` / `--compare-dest` directories, in order.
    #[rkyv(with = rkyv::with::Map<crate::proto::wire::OsBytes>)]
    pub alt_dest: Vec<OsString>,
    pub alt_link: bool,
    pub remove_source_files: bool,

    pub rsh: Option<String>,
    pub remote_path: Option<String>,
    pub threads: usize,
    /// Data streams for `transport` (0: everything on the ssh pipe).
    pub streams: usize,
    pub transport: Transport,
    pub wire: Wire,
    /// `--net-io=tokio`: the plan's tokio-driven connection layer (see `proto::async_io`).
    pub net_io_tokio: bool,
    pub strategy: Strategy,
    pub compress_level: Option<i32>,
    /// `--io-uring` (Linux): batched small-file writes on the receiver (see `fsops::uring`).
    pub io_uring: bool,

    pub server: bool,
    pub sender: bool,
    #[rkyv(with = rkyv::with::Map<crate::proto::wire::OsBytes>)]
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
        let mut filter_rules = ordered_filter_rules(&cli, &matches)?;
        // rsync protects a relative partial dir (and hides one on the sender) with a
        // trailing exclude rule.
        if let Some(d) = &cli.partial_dir
            && !d.as_bytes().starts_with(b"/")
        {
            let d = String::from_utf8_lossy(d.as_bytes()).trim_end_matches('/').to_string();
            filter_rules.push(RuleSpec { include: false, pattern: format!("{d}/") });
        }
        let chmod = parse_chmod_args(&cli.chmod)?;
        let max_size = cli.max_size.as_deref().map(|s| parse_size(s, "--max-size")).transpose()?;
        let min_size = cli.min_size.as_deref().map(|s| parse_size(s, "--min-size")).transpose()?;
        if !cli.link_dest.is_empty() && !cli.compare_dest.is_empty() {
            bail!("--link-dest cannot be combined with --compare-dest");
        }
        // The backup would be the file itself.
        if cli.backup && cli.backup_dir.is_none() && cli.suffix.as_deref() == Some("") {
            bail!("--suffix cannot be empty without a --backup-dir");
        }
        let files_from = match &cli.files_from {
            Some(f) => Some(read_files_from(f, cli.from0)?),
            None => None,
        };
        let mut o = Options::from_cli(cli, filter_rules);
        o.chmod = chmod;
        o.max_size = max_size;
        o.min_size = min_size;
        if files_from.is_some() {
            // rsync: --files-from implies -R and -d, and -a no longer implies -r.
            o.relative = true;
            o.dirs = true;
            o.recursive = matches.get_flag("recursive");
        }
        o.files_from = files_from;
        Ok(o)
    }

    fn from_cli(c: Cli, filter_rules: Vec<RuleSpec>) -> Options {
        let a = c.archive;
        let cores = std::thread::available_parallelism().map(|n| n.get()).unwrap_or(4);
        Options {
            recursive: c.recursive || a,
            links: c.links || a,
            hard_links: c.hard_links,
            // rsync: -A implies -p.
            perms: c.perms || a || c.acls,
            times: c.times || a,
            omit_dir_times: c.omit_dir_times || (c.backup && c.backup_dir.is_none()),
            group: c.group || a,
            owner: c.owner || a,
            acls: c.acls,
            xattrs: c.xattrs,
            devices: c.devices || c.devices_specials || a,
            specials: c.specials || c.devices_specials || a,
            copy_links: c.copy_links,
            copy_dirlinks: c.copy_dirlinks,
            keep_dirlinks: c.keep_dirlinks,
            dirs: c.dirs,
            relative: c.relative,
            files_from: None,
            chmod: Vec::new(),
            numeric_ids: c.numeric_ids,
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
            delta: c.delta,
            scanner: c.scanner,
            inplace: c.inplace,
            fsync: c.fsync,
            no_clone: c.no_clone,
            drop_cache: c.drop_cache || !c.no_drop_cache,
            sparse: c.sparse,
            partial: c.partial || c.partial_progress || c.partial_dir.is_some(),
            partial_dir: c.partial_dir,
            delete: c.delete || c.delete_excluded,
            delete_excluded: c.delete_excluded,
            filter_rules,
            update: c.update,
            size_only: c.size_only,
            ignore_times: c.ignore_times,
            ignore_existing: c.ignore_existing,
            one_file_system: c.one_file_system,
            modify_window: c.modify_window,
            checksum: c.checksum,
            existing: c.existing,
            max_size: None,
            min_size: None,
            backup: c.backup || c.backup_dir.is_some(),
            suffix: c.suffix.unwrap_or_else(|| if c.backup_dir.is_some() { String::new() } else { "~".to_string() }),
            backup_dir: c.backup_dir,
            alt_link: !c.link_dest.is_empty(),
            alt_dest: if c.link_dest.is_empty() { c.compare_dest } else { c.link_dest },
            remove_source_files: c.remove_source_files,
            rsh: c.rsh,
            remote_path: c.remote_path,
            // Two workers per core: on APFS that gives the fastest metadata-bound syncs
            // (filesystem latency is hidden by queue depth). Fewer threads use far less
            // kernel CPU for nearly the same speed; see README.
            threads: c.threads.filter(|&n| n > 0).unwrap_or((cores * 2).clamp(4, 64)),
            // `--streams N` alone means TCP streams, as before `--transport` existed.
            transport: c.transport.unwrap_or(if c.streams.is_some_and(|n| n > 0) { Transport::Tcp } else { Transport::DEFAULT }),
            streams: match c.transport {
                Some(Transport::Ssh) => 0,
                Some(Transport::Quic | Transport::Auto) => c.streams.unwrap_or(Transport::QUIC_STREAMS).clamp(1, Transport::MAX_STREAMS),
                Some(Transport::Tcp) => c.streams.unwrap_or(Transport::TCP_STREAMS).clamp(1, Transport::MAX_STREAMS),
                None if Transport::DEFAULT.quic() => c.streams.unwrap_or(Transport::QUIC_STREAMS).min(Transport::MAX_STREAMS),
                None => c.streams.unwrap_or(0).min(Transport::MAX_STREAMS),
            },
            wire: c.wire,
            net_io_tokio: c.net_io.as_deref() == Some("tokio"),
            strategy: c.strategy.unwrap_or(Strategy::DEFAULT),
            io_uring: c.io_uring,
            compress_level: c.compress_level,
            server: c.server,
            sender: c.sender,
            paths: c.paths,
        }
    }
}

fn parse_chmod_args(specs: &[String]) -> Result<Vec<ChmodRule>> {
    crate::chmod::parse_all(specs, crate::fsops::current_umask())
}

/// rsync's size syntax: a number with an optional fraction, an optional unit (`b k m g t
/// p`; `KB` = 1000, `K`/`KiB` = 1024), and an optional `+1`/`-1` adjustment.
pub fn parse_size(arg: &str, opt: &str) -> Result<u64> {
    let invalid = || anyhow::anyhow!("{opt} value is invalid: {arg}");
    let b = arg.as_bytes();
    let mut i = b.iter().take_while(|c| c.is_ascii_digit()).count();
    if b.get(i) == Some(&b'.') {
        i += 1 + b[i + 1..].iter().take_while(|c| c.is_ascii_digit()).count();
    }
    let number: f64 = if i == 0 { return Err(invalid()) } else { arg[..i].parse().map_err(|_| invalid())? };
    let mut rest = &b[i..];
    let reps = match rest.first() {
        None | Some(b'+') | Some(b'-') => 0,
        Some(&c) => {
            rest = &rest[1..];
            match c.to_ascii_lowercase() {
                b'b' => 0,
                b'k' => 1,
                b'm' => 2,
                b'g' => 3,
                b't' => 4,
                b'p' => 5,
                _ => return Err(invalid()),
            }
        }
    };
    let mult: u64 = match rest.first() {
        Some(b'b') | Some(b'B') => {
            rest = &rest[1..];
            1000
        }
        None | Some(b'+') | Some(b'-') => 1024,
        _ if rest.len() >= 2 && rest[..2].eq_ignore_ascii_case(b"ib") => {
            rest = &rest[2..];
            1024
        }
        _ => return Err(invalid()),
    };
    let mut size = (mult.pow(reps) as f64 * number) as i128;
    if let [sign @ (b'+' | b'-'), b'1', tail @ ..] = rest {
        size += if *sign == b'+' { 1 } else { -1 };
        rest = tail;
    }
    if !rest.is_empty() || size < 0 {
        return Err(invalid());
    }
    Ok(size as u64)
}

/// Read a `--files-from` list: one path per line (or NUL-separated with `--from0`);
/// blank lines and, without `--from0`, lines starting with `#` or `;` are skipped.
fn read_files_from(file: &OsString, from0: bool) -> Result<Vec<Vec<u8>>> {
    use std::io::Read;
    let data = if file == "-" {
        let mut v = Vec::new();
        std::io::stdin().read_to_end(&mut v).context("reading --files-from from stdin")?;
        v
    } else {
        if let crate::paths::Location::Remote { .. } = crate::paths::parse_location(file)? {
            bail!("a remote --files-from list is not supported");
        }
        std::fs::read(file).with_context(|| format!("failed to open files-from file {}", file.to_string_lossy()))?
    };
    let sep = if from0 { 0 } else { b'\n' };
    Ok(data
        .split(|&c| c == sep)
        .map(|l| if !from0 { l.strip_suffix(b"\r").unwrap_or(l) } else { l })
        .filter(|l| !l.is_empty() && (from0 || !matches!(l[0], b'#' | b';')))
        .map(<[u8]>::to_vec)
        .collect())
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
        let o = Options::parse_from(["barqsync", "-a", "s/", "d"]).unwrap();
        assert!(o.recursive && o.links && o.perms && o.times && o.group && o.owner);
        assert!(o.devices && o.specials);
    }

    #[test]
    fn xattrs_and_acls() {
        let o = Options::parse_from(["barqsync", "-aXX", "src/", "dst"]).unwrap();
        assert_eq!((o.xattrs, o.acls), (2, false));
        let o = Options::parse_from(["barqsync", "-rA", "src/", "dst"]).unwrap();
        assert!(o.acls && o.perms, "-A implies -p");
        let o = Options::parse_from(["barqsync", "-a", "src/", "dst"]).unwrap();
        assert_eq!((o.xattrs, o.acls), (0, false), "-a includes neither");
    }

    #[test]
    fn combined_short_flags() {
        let o = Options::parse_from(["barqsync", "-avzn", "s/", "d"]).unwrap();
        assert!(o.recursive && o.compress && o.dry_run);
        assert_eq!(o.verbose, 1);
    }

    #[test]
    fn filter_order_is_preserved_across_flags() {
        let o = Options::parse_from([
            "barqsync", "--include", "keep.log", "--exclude", "*.log", "--include", "x", "s", "d",
        ])
        .unwrap();
        let pats: Vec<_> = o.filter_rules.iter().map(|r| (r.include, r.pattern.as_str())).collect();
        assert_eq!(pats, vec![(true, "keep.log"), (false, "*.log"), (true, "x")]);
    }

    #[test]
    fn eight_bit_output() {
        assert!(!Options::parse_from(["barqsync", "-a", "s", "d"]).unwrap().eight_bit_output);
        for args in [&["barqsync", "-8", "s", "d"][..], &["barqsync", "-av8", "s", "d"], &["barqsync", "--8-bit-output", "s", "d"]] {
            assert!(Options::parse_from(args).unwrap().eight_bit_output, "{args:?}");
        }
    }

    #[test]
    fn sizes_parse_like_rsync() {
        for (arg, want) in [
            ("100", 100),
            ("4K", 4096),
            ("4k", 4096),
            ("4KB", 4000),
            ("4KiB", 4096),
            ("1.5M", 1_572_864),
            ("1M-1", 1_048_575),
            ("1G+1", 1_073_741_825),
            ("10b", 10),
        ] {
            assert_eq!(parse_size(arg, "--max-size").unwrap(), want, "{arg}");
        }
        for bad in ["", "K", "4X", "4KQ", "4K+2", "-1"] {
            assert!(parse_size(bad, "--max-size").is_err(), "{bad}");
        }
    }

    #[test]
    fn files_from_implies_relative_and_dirs_but_not_recursion() {
        let tmp = tempfile::tempdir().unwrap();
        let list = tmp.path().join("list");
        std::fs::write(&list, "# comment\na/b\n\n; c\nc\r\n").unwrap();
        let l = list.to_str().unwrap();
        let o = Options::parse_from(["barqsync", "-a", "--files-from", l, "s", "d"]).unwrap();
        assert!(o.relative && o.dirs && !o.recursive);
        assert_eq!(o.files_from.unwrap(), vec![b"a/b".to_vec(), b"c".to_vec()]);
        let o = Options::parse_from(["barqsync", "-ar", "--files-from", l, "s", "d"]).unwrap();
        assert!(o.recursive);
    }

    #[test]
    fn backup_suffix_defaults() {
        let o = Options::parse_from(["barqsync", "-b", "s", "d"]).unwrap();
        assert_eq!(o.suffix, "~");
        let o = Options::parse_from(["barqsync", "--backup-dir=bk", "s", "d"]).unwrap();
        assert!(o.backup);
        assert_eq!(o.suffix, "");
        assert!(Options::parse_from(["barqsync", "--link-dest=a", "--compare-dest=b", "s", "d"]).is_err());
        assert!(Options::parse_from(["barqsync", "-b", "--suffix=", "s", "d"]).is_err());
        assert!(Options::parse_from(["barqsync", "-b", "--suffix=", "--backup-dir=bk", "s", "d"]).is_ok());
    }

    #[test]
    fn negative_modify_window() {
        let o = Options::parse_from(["barqsync", "--modify-window=-1", "s", "d"]).unwrap();
        assert_eq!(o.modify_window, -1);
    }
}

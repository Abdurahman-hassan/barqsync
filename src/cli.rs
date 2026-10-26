//! Command-line interface: an rsync-compatible subset of flags, resolved into [`Options`].

use crate::chmod::ChmodRule;
use crate::delta::DeltaMode;
use crate::proto::Wire;
use crate::remote::{Strategy, Transport};
use crate::scan::Scanner;
use crate::filter::{RuleParser, RuleSpec};
use anyhow::{Context, Result, bail};
use std::os::unix::ffi::OsStrExt;
use clap::parser::ValueSource;
use clap::{ArgAction, ArgMatches, CommandFactory, FromArgMatches, Parser};
use serde::{Deserialize, Serialize};
use std::ffi::OsString;
use std::path::PathBuf;

#[derive(Parser, Debug)]
#[command(
    name = "barqsync",
    version,
    about = "A fast, parallel, rsync-compatible file synchronizer",
    disable_help_flag = true,
    after_help = "Options implied by others (-a, -D, ...) or given earlier can be turned off with \
                  --no-OPTION: --no-r, --no-l, --no-p, --no-t, --no-g, --no-o, --no-D, --no-devices, \
                  --no-specials, --no-H, --no-A, --no-X, --no-O, --no-d, --no-x, --no-S, --no-c, --no-z, \
                  --no-W, --no-v, --no-i, --no-h, --no-8, --no-m, --no-s, --no-inplace, --no-append, --no-partial, \
                  --no-progress, --no-numeric-ids, --no-backup, --no-mkpath, --no-timeout, --no-from0 \
                  (and their long forms, e.g. --no-owner)."
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
    #[arg(short = 'X', long, action = ArgAction::Append, num_args = 0, default_missing_value = "1")]
    xattrs: Vec<u8>,
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
    /// Custom username mapping: FROM:TO pairs (names, ids, LOW-HIGH or wildcards), comma-separated
    #[arg(long, value_name = "STRING")]
    usermap: Vec<String>,
    /// Custom groupname mapping: FROM:TO pairs, comma-separated
    #[arg(long, value_name = "STRING")]
    groupmap: Vec<String>,
    /// Simple username/groupname mapping (same as --usermap=*:USER --groupmap=*:GROUP)
    #[arg(long, value_name = "USER:GROUP")]
    chown: Vec<String>,
    /// Same as --devices --specials
    #[arg(short = 'D')]
    devices_specials: bool,
    /// Preserve device files (super-user only)
    #[arg(long)]
    devices: bool,
    /// Preserve special files (FIFOs)
    #[arg(long)]
    specials: bool,

    // Counted flags are lists, so that each occurrence keeps its place on the command
    // line (a later --no-v resets the count, as in rsync; see `count_since`).
    /// Increase verbosity
    #[arg(short = 'v', long, action = ArgAction::Append, num_args = 0, default_missing_value = "1")]
    verbose: Vec<u8>,
    /// Output a change-summary for all updates (twice: also unchanged items)
    #[arg(short = 'i', long = "itemize-changes", action = ArgAction::Append, num_args = 0, default_missing_value = "1")]
    itemize: Vec<u8>,
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
    #[arg(long = "no-whole-file", alias = "no-W", overrides_with = "whole_file")]
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
    /// Append data onto shorter files (implies --inplace)
    #[arg(long)]
    append: bool,
    /// Like --append, but with the old data in the file checksum
    #[arg(long = "append-verify")]
    append_verify: bool,
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

    /// Delete extraneous files from destination dirs, once the transfer is done
    #[arg(long)]
    delete: bool,
    /// Same as --delete (rsync deletes before the transfer; see docs/implementation.md)
    #[arg(long = "delete-before")]
    delete_before: bool,
    /// Same as --delete (rsync deletes during the transfer; see docs/implementation.md)
    #[arg(long = "delete-during", alias = "del")]
    delete_during: bool,
    /// Find deletions during the transfer, delete after it (what --delete does)
    #[arg(long = "delete-delay")]
    delete_delay: bool,
    /// Delete after the transfer (what --delete does)
    #[arg(long = "delete-after")]
    delete_after: bool,
    /// Also delete excluded files from destination dirs
    #[arg(long = "delete-excluded")]
    delete_excluded: bool,
    /// Don't delete more than NUM files (more due: warn and exit 25)
    #[arg(long = "max-delete", value_name = "NUM", allow_negative_numbers = true)]
    max_delete: Option<i64>,
    /// Add a file-filtering RULE (`- PATTERN`, `+ PATTERN`, `. FILE`, `: FILE`, `P`, `H`, ...)
    #[arg(short = 'f', long, value_name = "RULE", value_parser = clap::value_parser!(OsString), allow_hyphen_values = true)]
    filter: Vec<OsString>,
    /// Same as --filter='dir-merge /.rsync-filter'; repeated: --filter='- .rsync-filter'
    #[arg(short = 'F', action = ArgAction::Append, num_args = 0, default_missing_value = "1")]
    rsync_filter: Vec<u8>,
    /// Auto-ignore files in the same way CVS does
    #[arg(short = 'C', long = "cvs-exclude")]
    cvs_exclude: bool,
    /// Exclude files matching PATTERN
    #[arg(long, value_name = "PATTERN", value_parser = clap::value_parser!(OsString), allow_hyphen_values = true)]
    exclude: Vec<OsString>,
    /// Don't exclude files matching PATTERN
    #[arg(long, value_name = "PATTERN", value_parser = clap::value_parser!(OsString), allow_hyphen_values = true)]
    include: Vec<OsString>,
    /// Read exclude patterns from FILE
    #[arg(long = "exclude-from", value_name = "FILE")]
    exclude_from: Vec<PathBuf>,
    /// Read include patterns from FILE
    #[arg(long = "include-from", value_name = "FILE")]
    include_from: Vec<PathBuf>,
    /// Prune empty directory chains from the file list
    #[arg(short = 'm', long = "prune-empty-dirs")]
    prune_empty_dirs: bool,
    /// Create the destination's missing path components
    #[arg(long)]
    mkpath: bool,
    /// Accepted for rsync compatibility: paths always travel in the protocol, never on the remote command line
    #[arg(short = 's', long = "secluded-args", alias = "protect-args")]
    secluded_args: bool,
    /// Exit (code 30) when no data moves over a remote connection for SECONDS
    #[arg(long, value_name = "SECONDS")]
    timeout: Option<u64>,
    /// Limit the data rate (KiB per second unless RATE has a unit; 0: no limit)
    #[arg(long, value_name = "RATE", allow_hyphen_values = true)]
    bwlimit: Option<String>,

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

    // --no-OPTION: turns off what an earlier option (or -a, -D) turned on; see `toggle`.
    #[arg(long = "no-recursive", alias = "no-r", hide = true)]
    no_recursive: bool,
    #[arg(long = "no-links", alias = "no-l", hide = true)]
    no_links: bool,
    #[arg(long = "no-perms", alias = "no-p", hide = true)]
    no_perms: bool,
    #[arg(long = "no-times", alias = "no-t", hide = true)]
    no_times: bool,
    #[arg(long = "no-group", alias = "no-g", hide = true)]
    no_group: bool,
    #[arg(long = "no-owner", alias = "no-o", hide = true)]
    no_owner: bool,
    #[arg(long = "no-D", hide = true)]
    no_devices_specials: bool,
    #[arg(long = "no-devices", hide = true)]
    no_devices: bool,
    #[arg(long = "no-specials", hide = true)]
    no_specials: bool,
    #[arg(long = "no-hard-links", alias = "no-H", hide = true)]
    no_hard_links: bool,
    #[arg(long = "no-acls", alias = "no-A", hide = true)]
    no_acls: bool,
    #[arg(long = "no-xattrs", alias = "no-X", hide = true)]
    no_xattrs: bool,
    #[arg(long = "no-omit-dir-times", alias = "no-O", hide = true)]
    no_omit_dir_times: bool,
    #[arg(long = "no-dirs", alias = "no-d", hide = true)]
    no_dirs: bool,
    #[arg(long = "no-one-file-system", alias = "no-x", hide = true)]
    no_one_file_system: bool,
    #[arg(long = "no-sparse", alias = "no-S", hide = true)]
    no_sparse: bool,
    #[arg(long = "no-checksum", alias = "no-c", hide = true)]
    no_checksum: bool,
    #[arg(long = "no-compress", alias = "no-z", hide = true)]
    no_compress: bool,
    #[arg(long = "no-verbose", alias = "no-v", hide = true)]
    no_verbose: bool,
    #[arg(long = "no-itemize-changes", alias = "no-i", hide = true)]
    no_itemize: bool,
    #[arg(long = "no-human-readable", alias = "no-h", hide = true)]
    no_human: bool,
    #[arg(long = "no-8-bit-output", alias = "no-8", hide = true)]
    no_eight_bit_output: bool,
    #[arg(long = "no-prune-empty-dirs", alias = "no-m", hide = true)]
    no_prune_empty_dirs: bool,
    #[arg(long = "no-secluded-args", aliases = ["no-protect-args", "no-s"], hide = true)]
    no_secluded_args: bool,
    #[arg(long = "no-inplace", hide = true)]
    no_inplace: bool,
    #[arg(long = "no-append", hide = true)]
    no_append: bool,
    #[arg(long = "no-partial", hide = true)]
    no_partial: bool,
    #[arg(long = "no-progress", hide = true)]
    no_progress: bool,
    #[arg(long = "no-numeric-ids", hide = true)]
    no_numeric_ids: bool,
    #[arg(long = "no-backup", hide = true)]
    no_backup: bool,
    #[arg(long = "no-mkpath", hide = true)]
    no_mkpath: bool,
    #[arg(long = "no-timeout", hide = true)]
    no_timeout: bool,
    #[arg(long = "no-from0", hide = true)]
    no_from0: bool,

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
    /// `--usermap` (or `--chown`'s user, as `*:USER`): applied by the receiver.
    pub usermap: Option<String>,
    /// `--groupmap` (or `--chown`'s group, as `*:GROUP`): applied by the receiver.
    pub groupmap: Option<String>,

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
    /// `--append` (1) or `--append-verify` (2): shorter destination files only get the
    /// rest of the data, longer ones are left alone (implies `inplace`).
    pub append: u8,
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
    /// `--max-delete`: at most this many deletions (`None`: no limit).
    pub max_delete: Option<u64>,
    pub filter_rules: Vec<RuleSpec>,
    /// `--from0`: rules files, per-directory ones included, have NUL-separated rules.
    pub from0: bool,
    /// `-m`: directories with nothing but directories below them are left out.
    pub prune_empty_dirs: bool,
    /// `--mkpath`: missing directories leading to the destination are created.
    pub mkpath: bool,
    /// `--timeout`: seconds a remote connection may go without data (0: no limit).
    pub timeout: u64,
    /// `--bwlimit`: bytes per second, in whole KiB (0: no limit).
    pub bwlimit: u64,

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
        // Like rsync, a flag or option given again is no error: the last one counts.
        // (Counted flags such as -v and lists such as --exclude keep every occurrence.)
        let cmd = Cli::command().mut_args(|a| match a.get_action() {
            ArgAction::SetTrue | ArgAction::Set => {
                let id = a.get_id().clone();
                a.overrides_with(id)
            }
            _ => a,
        });
        let matches = cmd.try_get_matches_from(args)?;
        let cli = Cli::from_arg_matches(&matches)?;
        let m = &matches;
        let from0 = toggle(m, &["from0"], &["no_from0"]).unwrap_or(false);
        let filter_rules = ordered_filter_rules(&cli, m, from0)?;
        let chmod = parse_chmod_args(&cli.chmod)?;
        let max_size = cli.max_size.as_deref().map(|s| parse_size(s, "--max-size")).transpose()?;
        let min_size = cli.min_size.as_deref().map(|s| parse_size(s, "--min-size")).transpose()?;
        let bwlimit = cli.bwlimit.as_deref().map(parse_bwlimit).transpose()?.unwrap_or(0);
        let [usermap, groupmap] = id_maps(&cli, m)?;
        if !cli.link_dest.is_empty() && !cli.compare_dest.is_empty() {
            bail!("--link-dest cannot be combined with --compare-dest");
        }
        // The backup would be the file itself.
        if cli.backup && cli.backup_dir.is_none() && cli.suffix.as_deref() == Some("") {
            bail!("--suffix cannot be empty without a --backup-dir");
        }
        // --del, --delete-during and --delete-delay set one choice, as in rsync.
        let when = [cli.delete_before, cli.delete_during || cli.delete_delay, cli.delete_after];
        if when.iter().filter(|&&w| w).count() > 1 {
            bail!("You may not combine multiple --delete-WHEN options.");
        }
        let files_from = match &cli.files_from {
            Some(f) => Some(read_files_from(f, from0)?),
            None => None,
        };
        let mut o = Options::from_cli(cli, m, filter_rules);
        o.chmod = chmod;
        o.max_size = max_size;
        o.min_size = min_size;
        o.bwlimit = bwlimit;
        // rsync: --usermap implies -o and --groupmap -g, where they are given.
        let at = |map: &Option<(String, usize)>| map.as_ref().map(|&(_, i)| i);
        o.owner = toggle_with(m, &["archive", "owner"], at(&usermap), &["no_owner"]).unwrap_or(false);
        o.group = toggle_with(m, &["archive", "group"], at(&groupmap), &["no_group"]).unwrap_or(false);
        o.usermap = usermap.map(|(map, _)| map);
        o.groupmap = groupmap.map(|(map, _)| map);
        if o.append > 0 {
            if o.whole_file == Some(true) {
                bail!("--append cannot be used with --whole-file");
            }
            if o.partial_dir.is_some() {
                bail!("--append cannot be used with --partial-dir");
            }
        }
        if files_from.is_some() {
            // rsync: --files-from implies -R and -d, and -a no longer implies -r.
            o.relative = true;
            o.dirs = true;
            o.recursive = toggle(m, &["recursive"], &["no_recursive"]).unwrap_or(false);
        }
        o.files_from = files_from;
        if o.delete && !o.recursive && !o.dirs {
            bail!("--delete does not work without --recursive (-r) or --dirs (-d).");
        }
        Ok(o)
    }

    fn from_cli(c: Cli, m: &ArgMatches, filter_rules: Vec<RuleSpec>) -> Options {
        let on = |on: &[&str], off: &[&str]| toggle(m, on, off).unwrap_or(false);
        let cores = std::thread::available_parallelism().map(|n| n.get()).unwrap_or(4);
        let backup = on(&["backup"], &["no_backup"]) || c.backup_dir.is_some();
        let quiet = c.quiet;
        // The last of --append, --append-verify and --no-append counts.
        let append = match [&["append"], &["append_verify"], &["no_append"]].map(|id| last_index(m, id)) {
            [a, v, off] if a.max(v) > off => 1 + u8::from(v > a),
            _ => 0,
        };
        Options {
            recursive: on(&["archive", "recursive"], &["no_recursive"]),
            links: on(&["archive", "links"], &["no_links"]),
            hard_links: on(&["hard_links"], &["no_hard_links"]),
            // rsync: -A implies -p.
            perms: on(&["archive", "perms", "acls"], &["no_perms"]),
            times: on(&["archive", "times"], &["no_times"]),
            omit_dir_times: on(&["omit_dir_times"], &["no_omit_dir_times"]) || (backup && c.backup_dir.is_none()),
            group: on(&["archive", "group"], &["no_group"]),
            owner: on(&["archive", "owner"], &["no_owner"]),
            acls: on(&["acls"], &["no_acls"]),
            xattrs: count_since(m, "xattrs", "no_xattrs"),
            devices: on(&["archive", "devices_specials", "devices"], &["no_devices_specials", "no_devices"]),
            specials: on(&["archive", "devices_specials", "specials"], &["no_devices_specials", "no_specials"]),
            copy_links: c.copy_links,
            copy_dirlinks: c.copy_dirlinks,
            keep_dirlinks: c.keep_dirlinks,
            dirs: on(&["dirs"], &["no_dirs"]),
            relative: c.relative,
            files_from: None,
            chmod: Vec::new(),
            numeric_ids: on(&["numeric_ids"], &["no_numeric_ids"]),
            usermap: None,
            groupmap: None,
            verbose: if quiet { 0 } else { count_since(m, "verbose", "no_verbose") },
            itemize: if quiet { 0 } else { count_since(m, "itemize", "no_itemize") },
            quiet,
            dry_run: c.dry_run,
            out_format: c.out_format,
            progress: on(&["progress", "partial_progress"], &["no_progress"])
                || c.info.iter().flat_map(|f| f.split(',')).any(|f| f.starts_with("progress") && f != "progress0"),
            stats: c.stats,
            human: on(&["human"], &["no_human"]),
            eight_bit_output: on(&["eight_bit_output"], &["no_eight_bit_output"]),
            compress: on(&["compress"], &["no_compress"]),
            whole_file: if c.whole_file {
                Some(true)
            } else if c.no_whole_file {
                Some(false)
            } else {
                None
            },
            delta: c.delta,
            scanner: c.scanner,
            // rsync: --append implies --inplace.
            inplace: on(&["inplace"], &["no_inplace"]) || append > 0,
            append,
            fsync: c.fsync,
            no_clone: c.no_clone,
            drop_cache: c.drop_cache || !c.no_drop_cache,
            sparse: on(&["sparse"], &["no_sparse"]),
            partial: on(&["partial", "partial_progress"], &["no_partial"]) || c.partial_dir.is_some(),
            partial_dir: c.partial_dir,
            delete: c.delete || c.delete_before || c.delete_during || c.delete_delay || c.delete_after || c.delete_excluded,
            delete_excluded: c.delete_excluded,
            // rsync: a negative limit means no deletions at all.
            max_delete: c.max_delete.map(|n| n.max(0) as u64),
            filter_rules,
            from0: on(&["from0"], &["no_from0"]),
            prune_empty_dirs: on(&["prune_empty_dirs"], &["no_prune_empty_dirs"]),
            mkpath: on(&["mkpath"], &["no_mkpath"]),
            timeout: match toggle(m, &["timeout"], &["no_timeout"]) {
                Some(true) => c.timeout.unwrap_or(0),
                _ => 0,
            },
            bwlimit: 0,
            update: c.update,
            size_only: c.size_only,
            ignore_times: c.ignore_times,
            ignore_existing: c.ignore_existing,
            one_file_system: on(&["one_file_system"], &["no_one_file_system"]),
            modify_window: c.modify_window,
            checksum: on(&["checksum"], &["no_checksum"]),
            existing: c.existing,
            max_size: None,
            min_size: None,
            backup,
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

/// Where the last of these arguments is on the command line, if any was given there.
fn last_index(m: &ArgMatches, ids: &[&str]) -> Option<usize> {
    ids.iter()
        .filter(|id| m.value_source(id) == Some(ValueSource::CommandLine))
        .filter_map(|id| m.indices_of(id).and_then(Iterator::max))
        .max()
}

/// rsync processes options in order: the last of `on` (e.g. `-a`, `-o`) and `off`
/// (`--no-o`) given decides. `None`: neither was given.
fn toggle(m: &ArgMatches, on: &[&str], off: &[&str]) -> Option<bool> {
    match (last_index(m, on), last_index(m, off)) {
        (None, None) => None,
        (a, b) => Some(a > b),
    }
}

/// [`toggle`], with one more `on` at command-line position `at` (an option implying it).
fn toggle_with(m: &ArgMatches, on: &[&str], at: Option<usize>, off: &[&str]) -> Option<bool> {
    match (last_index(m, on).max(at), last_index(m, off)) {
        (None, None) => None,
        (a, b) => Some(a > b),
    }
}

/// `--usermap`, `--groupmap` and `--chown` (`*:USER`, `*:GROUP`) in command-line order,
/// with rsync's errors: each map once, and `--chown` not mixed with the one it sets.
/// Returns each map with its position on the command line (it implies `-o` or `-g`
/// from there). The maps' syntax is checked here; their names are the receiver's to
/// look up.
fn id_maps(cli: &Cli, m: &ArgMatches) -> Result<[Option<(String, usize)>; 2]> {
    let indices = |id: &str| m.indices_of(id).into_iter().flatten();
    let mut opts: Vec<(usize, Option<usize>, &str)> = Vec::new();
    opts.extend(indices("usermap").zip(&cli.usermap).map(|(i, s)| (i, Some(0), s.as_str())));
    opts.extend(indices("groupmap").zip(&cli.groupmap).map(|(i, s)| (i, Some(1), s.as_str())));
    opts.extend(indices("chown").zip(&cli.chown).map(|(i, s)| (i, None, s.as_str())));
    opts.sort_by_key(|o| o.0);
    const NAMES: [(&str, &str); 2] = [("usermap", "user"), ("groupmap", "group")];
    // Each map, where it was given, and whether --chown set it.
    let mut maps: [Option<(String, usize, bool)>; 2] = [None, None];
    for (i, which, arg) in opts {
        let Some(k) = which else {
            let (user, group) = arg.split_once(':').unwrap_or((arg, ""));
            for (k, part) in [user, group].into_iter().enumerate() {
                if part.is_empty() {
                    continue;
                }
                let (name, what) = NAMES[k];
                match &maps[k] {
                    Some((_, _, false)) => bail!("--chown conflicts with prior --{name}."),
                    Some(_) => bail!("You can only specify a {what}-affecting --chown once."),
                    None => maps[k] = Some((format!("*:{part}"), i, true)),
                }
            }
            continue;
        };
        let name = NAMES[k].0;
        match &maps[k] {
            Some((_, _, true)) => bail!("--{name} conflicts with prior --chown."),
            Some(_) => bail!("You can only specify --{name} once."),
            None => maps[k] = Some((arg.to_string(), i, false)),
        }
    }
    for (k, map) in maps.iter().enumerate() {
        if let Some((map, ..)) = map {
            crate::ids::NameMap::parse(map, k == 0, None, &mut |_| {}).map_err(|e| anyhow::anyhow!(e))?;
        }
    }
    Ok(maps.map(|m| m.map(|(map, i, _)| (map, i))))
}

/// How many times a counted flag (`-v`) was given after its last `--no-` form.
fn count_since(m: &ArgMatches, id: &str, off: &str) -> u8 {
    let since = last_index(m, &[off]);
    let given = m.value_source(id) == Some(ValueSource::CommandLine);
    let n = m.indices_of(id).filter(|_| given).map_or(0, |i| i.filter(|&i| Some(i) > since).count());
    n.min(u8::MAX as usize) as u8
}

fn parse_chmod_args(specs: &[String]) -> Result<Vec<ChmodRule>> {
    crate::chmod::parse_all(specs, crate::fsops::current_umask())
}

/// rsync's size syntax: a number with an optional fraction, an optional unit (`b k m g t
/// p`; `KB` = 1000, `K`/`KiB` = 1024), and an optional `+1`/`-1` adjustment.
pub fn parse_size(arg: &str, opt: &str) -> Result<u64> {
    let invalid = || anyhow::anyhow!("{opt} value is invalid: {arg}");
    // A number must come first (rsync would take `K`, or nothing, as 0).
    let b = arg.as_bytes();
    if !b.first().is_some_and(u8::is_ascii_digit) && !(b.first() == Some(&b'.') && b.get(1).is_some_and(u8::is_ascii_digit)) {
        return Err(invalid());
    }
    size_arg(arg, b'b').map_err(|_| invalid())
}

/// rsync's `parse_size_arg`: [`parse_size`]'s syntax with `unit` when none is given.
/// Fails with what is wrong, in rsync's words: `invalid`, or `too large` (a negative
/// number counts as that, as in rsync).
fn size_arg(arg: &str, unit: u8) -> Result<u64, &'static str> {
    let b = arg.as_bytes();
    let mut i = b.iter().take_while(|c| c.is_ascii_digit()).count();
    if b.get(i) == Some(&b'.') {
        i += 1 + b[i + 1..].iter().take_while(|c| c.is_ascii_digit()).count();
    }
    // The number as `strtod` reads it from the start, sign included.
    let signed = usize::from(matches!(b.first(), Some(b'+' | b'-')) && i == 0);
    let mut end = signed + b[signed..].iter().take_while(|c| c.is_ascii_digit()).count();
    if b.get(end) == Some(&b'.') {
        end += 1 + b[end + 1..].iter().take_while(|c| c.is_ascii_digit()).count();
    }
    let mut rest = &b[i..];
    let suffix = match rest.first() {
        Some(&c) if c != b'+' && c != b'-' => {
            rest = &rest[1..];
            c
        }
        _ => unit,
    };
    let reps = match suffix.to_ascii_lowercase() {
        b'b' => 0,
        b'k' => 1,
        b'm' => 2,
        b'g' => 3,
        b't' => 4,
        b'p' => 5,
        _ => return Err("invalid"),
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
        _ => return Err("invalid"),
    };
    let number: f64 = arg[..end].parse().unwrap_or(0.0);
    if number < 0.0 {
        return Err("too large");
    }
    let mut size = (mult.pow(reps) as f64 * number) as i128;
    if let [sign @ (b'+' | b'-'), b'1', tail @ ..] = rest
        && rest.len() < b.len()
    {
        size += if *sign == b'+' { 1 } else { -1 };
        rest = tail;
    }
    if !rest.is_empty() {
        return Err("invalid");
    }
    u64::try_from(size).map_err(|_| "too large")
}

/// `--bwlimit=RATE`: rsync's size syntax, in KiB when no unit is given (`1.5m`,
/// `100KB`), rounded to whole KiB per second as rsync does. 0 means no limit; anything
/// else under 512 bytes is refused.
fn parse_bwlimit(arg: &str) -> Result<u64> {
    let size = size_arg(arg, b'k').map_err(|e| anyhow::anyhow!("--bwlimit={arg} is {e}"))?;
    if size != 0 && size < 512 {
        bail!("--bwlimit={arg} is too small (min: 512 or 0 for unlimited)");
    }
    Ok((size + 512) / 1024 * 1024)
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

/// The filter rules, in command-line order across `--filter`, `--include`, `--exclude`,
/// their `-from` files and `-F`; `.` merge files are read now. Then, as in rsync, the
/// rule hiding a relative `--partial-dir`, and `-C`'s rules last.
fn ordered_filter_rules(cli: &Cli, m: &ArgMatches, from0: bool) -> Result<Vec<RuleSpec>> {
    enum Opt<'a> {
        Filter(&'a OsString),
        Pattern(&'a OsString, bool),
        File(&'a PathBuf, bool),
        RsyncFilter(usize),
    }
    let mut opts: Vec<(usize, Opt)> = Vec::new();
    let indices = |id: &str| m.indices_of(id).into_iter().flatten();
    opts.extend(indices("filter").zip(&cli.filter).map(|(i, v)| (i, Opt::Filter(v))));
    opts.extend(indices("exclude").zip(&cli.exclude).map(|(i, v)| (i, Opt::Pattern(v, false))));
    opts.extend(indices("include").zip(&cli.include).map(|(i, v)| (i, Opt::Pattern(v, true))));
    opts.extend(indices("exclude_from").zip(&cli.exclude_from).map(|(i, f)| (i, Opt::File(f, false))));
    opts.extend(indices("include_from").zip(&cli.include_from).map(|(i, f)| (i, Opt::File(f, true))));
    opts.extend(indices("rsync_filter").enumerate().map(|(n, i)| (i, Opt::RsyncFilter(n))));
    opts.sort_by_key(|(i, _)| *i);
    let mut p = RuleParser::new(from0);
    for (_, opt) in opts {
        match opt {
            Opt::Filter(rule) => p.filter(rule.as_bytes()),
            Opt::Pattern(pattern, include) => p.pattern(pattern.as_bytes(), include),
            Opt::File(f, include) => p.file(f, include),
            // -F: the first one reads `.rsync-filter` files, a second hides them.
            Opt::RsyncFilter(0) => p.filter(b": /.rsync-filter"),
            Opt::RsyncFilter(1) => p.filter(b"- .rsync-filter"),
            Opt::RsyncFilter(_) => Ok(()),
        }?;
    }
    // rsync protects a relative partial dir (and hides one on the sender) with a
    // trailing exclude rule.
    if let Some(d) = &cli.partial_dir
        && !d.as_bytes().starts_with(b"/")
    {
        let d = d.as_bytes();
        let end = d.iter().rposition(|&c| c != b'/').map_or(0, |e| e + 1);
        p.rules.push(RuleSpec::exclude([&d[..end], b"/"].concat()));
    }
    if cli.cvs_exclude {
        p.filter(b":C")?;
        p.filter(b"-C")?;
    }
    Ok(p.rules)
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
        let pats: Vec<_> = o
            .filter_rules
            .iter()
            .map(|r| match r {
                RuleSpec::Pattern { include, pattern, .. } => (*include, String::from_utf8_lossy(pattern).into_owned()),
                other => panic!("{other:?}"),
            })
            .collect();
        let want = [(true, "keep.log"), (false, "*.log"), (true, "x")];
        assert_eq!(pats, want.map(|(i, p)| (i, p.to_string())));
    }

    #[test]
    fn filter_options_keep_command_line_order() {
        use crate::filter::{MergeMods, Mods};
        let o = Options::parse_from([
            "barqsync", "-C", "--exclude", "a", "-f", "+ b", "-F", "--include=- c", "-F", "-f", "P d", "-F", "s", "d",
        ])
        .unwrap();
        let pat = |include: bool, p: &str, mods: Mods| RuleSpec::Pattern { include, pattern: p.as_bytes().to_vec(), mods };
        let none = Mods::default();
        let cvs = MergeMods { only: Some(false), words: true, no_inherit: true, cvs: true };
        assert_eq!(
            o.filter_rules,
            vec![
                pat(false, "a", none),
                pat(true, "b", none),
                RuleSpec::DirMerge { name: b"/.rsync-filter".to_vec(), merge: MergeMods::default(), mods: none },
                // An old-style --include takes a `- ` prefix.
                pat(false, "c", none),
                pat(false, ".rsync-filter", none),
                pat(false, "d", Mods { receiver: true, ..none }),
                // -C's rules come last, wherever it was given.
                RuleSpec::DirMerge { name: b".cvsignore".to_vec(), merge: cvs, mods: none },
                RuleSpec::Cvs { mods: none },
            ]
        );
        assert!(Options::parse_from(["barqsync", "-f", "bogus", "s", "d"]).is_err());
        assert!(Options::parse_from(["barqsync", "--exclude-from=/nonexistent/x", "s", "d"]).is_err());
    }

    #[test]
    fn no_option_negates_what_came_before() {
        let o = Options::parse_from(["barqsync", "-a", "--no-o", "--no-g", "--no-perms", "--no-t", "--no-D", "s", "d"]).unwrap();
        assert!(o.recursive && o.links);
        assert!(!o.owner && !o.group && !o.perms && !o.times && !o.devices && !o.specials);
        // The last of the two wins, as in rsync.
        let o = Options::parse_from(["barqsync", "--no-o", "-a", "--no-l", "--no-r", "-d", "s", "d"]).unwrap();
        assert!(o.owner && !o.links && !o.recursive && o.dirs);
        let o = Options::parse_from(["barqsync", "-a", "--no-recursive", "-r", "--no-specials", "s", "d"]).unwrap();
        assert!(o.recursive && o.devices && !o.specials);
        let o = Options::parse_from(["barqsync", "-avv", "--no-v", "-v", "-X", "--no-X", "s", "d"]).unwrap();
        assert_eq!((o.verbose, o.xattrs), (1, 0));
        let o = Options::parse_from(["barqsync", "-aA", "--no-A", "--no-W", "-z", "--no-z", "s", "d"]).unwrap();
        assert!(!o.acls && o.perms && o.whole_file == Some(false) && !o.compress);
        // Options may be repeated, as with rsync.
        let o = Options::parse_from(["barqsync", "-a", "-a", "-P", "--partial", "--stats", "--stats", "s", "d"]).unwrap();
        assert!(o.recursive && o.partial && o.progress && o.stats);
        let o = Options::parse_from(["barqsync", "--exclude=x", "--exclude=y", "s", "d"]).unwrap();
        assert_eq!(o.filter_rules.len(), 2, "repeated values add up");
    }

    #[test]
    fn delete_timing_and_limits() {
        for flag in ["--delete-before", "--delete-during", "--del", "--delete-delay", "--delete-after"] {
            let o = Options::parse_from(["barqsync", "-r", flag, "s", "d"]).unwrap();
            assert!(o.delete, "{flag}");
        }
        assert!(Options::parse_from(["barqsync", "-r", "--delete-before", "--delete-after", "s", "d"]).is_err());
        assert!(Options::parse_from(["barqsync", "-r", "--delete-delay", "--del", "s", "d"]).is_ok(), "one choice");
        assert!(Options::parse_from(["barqsync", "--delete", "s", "d"]).is_err(), "needs -r or -d");
        assert!(Options::parse_from(["barqsync", "-a", "--no-r", "--delete", "s", "d"]).is_err());
        assert!(Options::parse_from(["barqsync", "-d", "--delete", "s", "d"]).is_ok());
        let o = Options::parse_from(["barqsync", "-r", "--max-delete=3", "s", "d"]).unwrap();
        assert_eq!((o.max_delete, o.delete), (Some(3), false));
        let o = Options::parse_from(["barqsync", "-r", "--max-delete=-1", "s", "d"]).unwrap();
        assert_eq!(o.max_delete, Some(0), "negative: no deletions");
        let o = Options::parse_from(["barqsync", "-r", "s", "d"]).unwrap();
        assert_eq!(o.max_delete, None);
    }

    #[test]
    fn accepted_rsync_options() {
        let o = Options::parse_from(["barqsync", "-asm", "--mkpath", "--protect-args", "--timeout=30", "s", "d"]).unwrap();
        assert!(o.prune_empty_dirs && o.mkpath);
        assert_eq!(o.timeout, 30);
        let o = Options::parse_from(["barqsync", "-a", "--timeout=30", "--no-timeout", "--secluded-args", "--no-s", "s", "d"]).unwrap();
        assert_eq!(o.timeout, 0);
        let o = Options::parse_from(["barqsync", "-am", "--no-m", "--mkpath", "--no-mkpath", "s", "d"]).unwrap();
        assert!(!o.prune_empty_dirs && !o.mkpath);
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
    fn bwlimit_parses_like_rsync() {
        let rate = |args: &[&str]| Options::parse_from([&["barqsync"], args, &["s", "d"]].concat()).map(|o| o.bwlimit);
        // KiB by default, rounded to whole KiB per second.
        for (arg, want) in [("0", 0), ("100", 100 << 10), ("1.5", 2 << 10), ("1.5m", 1536 << 10), ("512b", 1 << 10), ("1000KB", 977 << 10), ("5-1", 5 << 10)] {
            assert_eq!(rate(&[&format!("--bwlimit={arg}")]).unwrap(), want, "{arg}");
        }
        assert_eq!(rate(&[]).unwrap(), 0);
        for (arg, why) in [("100b", "too small (min: 512 or 0 for unlimited)"), ("x", "invalid"), ("-5", "too large"), ("+5", "invalid"), ("1kq", "invalid")] {
            let e = rate(&[&format!("--bwlimit={arg}")]).unwrap_err();
            assert_eq!(e.to_string(), format!("--bwlimit={arg} is {why}"));
        }
    }

    #[test]
    fn id_maps_and_chown() {
        let o = Options::parse_from(["barqsync", "-r", "--chown=me:staff", "s", "d"]).unwrap();
        assert_eq!((o.usermap.as_deref(), o.groupmap.as_deref()), (Some("*:me"), Some("*:staff")));
        assert!(o.owner && o.group, "--chown implies -o and -g");
        let o = Options::parse_from(["barqsync", "-r", "--chown=:staff", "s", "d"]).unwrap();
        assert!(o.usermap.is_none() && !o.owner && o.group);
        let o = Options::parse_from(["barqsync", "-a", "--groupmap=0-9:wheel", "--no-g", "s", "d"]).unwrap();
        assert!(!o.group && o.groupmap.is_some(), "a later --no-g wins");
        let o = Options::parse_from(["barqsync", "--no-o", "--usermap=*:x", "--chown=:y", "s", "d"]).unwrap();
        assert!(o.owner && o.group);
        let err = |args: &[&str]| Options::parse_from([&["barqsync"], args, &["s", "d"]].concat()).unwrap_err().to_string();
        assert_eq!(err(&["--chown=a:b", "--usermap=x:y"]), "--usermap conflicts with prior --chown.");
        assert_eq!(err(&["--groupmap=x:y", "--chown=a:b"]), "--chown conflicts with prior --groupmap.");
        assert_eq!(err(&["--usermap=x:y", "--usermap=x:y"]), "You can only specify --usermap once.");
        assert_eq!(err(&["--chown=a", "--chown=b"]), "You can only specify a user-affecting --chown once.");
        assert_eq!(err(&["--groupmap=7x:y"]), "Invalid number in --groupmap: 7x");
        assert!(Options::parse_from(["barqsync", "--chown=a", "--chown=:b", "s", "d"]).is_ok());
    }

    #[test]
    fn append_modes() {
        let mode = |args: &[&str]| Options::parse_from([&["barqsync"], args, &["s", "d"]].concat()).map(|o| (o.append, o.inplace));
        assert_eq!(mode(&["--append"]).unwrap(), (1, true));
        assert_eq!(mode(&["--append", "--append-verify"]).unwrap(), (2, true));
        assert_eq!(mode(&["--append-verify", "--append"]).unwrap(), (1, true));
        assert_eq!(mode(&["--append", "--no-append"]).unwrap(), (0, false));
        assert_eq!(mode(&["--append", "--no-append", "-W"]).unwrap(), (0, false));
        assert_eq!(mode(&["--append", "-W"]).unwrap_err().to_string(), "--append cannot be used with --whole-file");
        assert!(mode(&["--append", "-W", "--no-W"]).is_ok());
        assert_eq!(mode(&["--append-verify", "--partial-dir=p"]).unwrap_err().to_string(), "--append cannot be used with --partial-dir");
    }

    #[test]
    fn negative_modify_window() {
        let o = Options::parse_from(["barqsync", "--modify-window=-1", "s", "d"]).unwrap();
        assert_eq!(o.modify_window, -1);
    }
}

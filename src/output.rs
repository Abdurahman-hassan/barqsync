//! User-facing output: items (rsync's `--out-format`), deletions, messages, errors and
//! stats, each shown when rsync's `--info` levels would show it.
//!
//! Every line that can carry a file name goes through [`escape`], which mirrors rsync's
//! log.c (`rwrite` + `filtered_fwrite`), so a name cannot put control characters or
//! terminal escape sequences on the user's terminal.

use crate::cli::{Info, Options};
use crate::entry::{Kind, Meta};
use crate::logformat::{Fields, Format};
use crate::proto::Msg;
use std::io::{BufWriter, Stdout, Write};
use std::os::unix::ffi::OsStrExt;
use std::path::Path;
use std::sync::atomic::{AtomicU64, Ordering::Relaxed};
use std::sync::{Mutex, OnceLock};
use std::time::Duration;

/// Where a server sends its output instead of its own stdout/stderr: `Log`, `Item` and
/// `Deleted` messages, which the client shows (see [`Output::print_forwarded`]).
pub type Forward = Box<dyn Fn(Msg) + Send + Sync>;

struct Sink {
    w: BufWriter<Stdout>,
    /// The progress line currently drawn (without a newline), if any.
    progress: Option<Vec<u8>>,
}

impl Sink {
    /// Write complete lines, keeping the progress line (if any) drawn below them.
    fn write_lines(&mut self, bytes: &[u8]) {
        if self.progress.is_some() {
            let _ = self.w.write_all(b"\r\x1b[K");
        }
        let _ = self.w.write_all(bytes);
        if let Some(p) = &self.progress {
            let _ = self.w.write_all(b"\r");
            let _ = self.w.write_all(p);
            let _ = self.w.flush();
        }
    }
}

/// What the client is doing, which decides how items look: as rsync's client shows them,
/// its sender in a local copy or a push, its receiver in a pull.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Side {
    /// `%o` is `send` and `%f` the source path, but transfers show `>f`.
    Local,
    /// To a remote receiver: `send`, the source path, and `<f`.
    Push,
    /// From a remote sender: `recv`, and `%f` is the name in the destination.
    Pull,
}

pub struct Output {
    out: Mutex<Sink>,
    /// stdout is a terminal (progress is redrawn in place only then).
    tty: bool,
    verbose: u8,
    /// `-q`: nothing on stdout.
    quiet: bool,
    /// Number of `-i`.
    itemize: u8,
    info: Info,
    /// rsync's `stdout_format`: `--out-format`, else `%i %n%L` with `-i`, else `%n%L` with
    /// `--info=name` (`-v`). Items are only reported with one.
    format: Option<Format>,
    /// rsync's `stdout_format_has_i`: 0, or the format has `%i` (2+: so does `-ii`).
    has_i: u8,
    /// Deletions are shown in the format (it has `%o` or `%i`), else as `deleting NAME`.
    del_format: bool,
    /// `%b`: a transfer is reported once its data is known (see [`Output::after`]).
    after: bool,
    /// `-t` is on: an mtime difference shows as `t`, otherwise as `T` (rsync).
    times: bool,
    /// `-o`, `-g`: `%U` and `%G` show the ids (rsync shows 0 and `DEFAULT` otherwise).
    owner: bool,
    group: bool,
    side: Side,
    /// [`Side::Pull`]: whether this process may give a file the group `%G` shows (root,
    /// or the groups it is in); rsync shows `DEFAULT` for the others.
    groups: OnceLock<(bool, Vec<u32>)>,
    charset: Charset,
    forward: Option<Forward>,
    /// `--log-file`: what the client's log reports (a server whose client logs reports
    /// it to the client too).
    log: Option<LogSpec>,
    /// The client's log file, once opened (see [`Output::open_log`]).
    log_file: OnceLock<Mutex<std::fs::File>>,
}

/// What `--log-file` logs (rsync's client with a log file open): every message it shows,
/// even with `-q`; items and deletions in `--log-file-format`; and as the transfer starts
/// and ends, the file-list line and the byte counts. Each line starts with the local time
/// and the process id, and is escaped as rsync's log is.
struct LogSpec {
    /// `--log-file-format` (`%i %n%L` by default); `None` when given empty: no items.
    format: Option<Format>,
    has_i: bool,
    /// Deletions are logged in the format (it has `%o` or `%i`), else as `deleting NAME`.
    del_format: bool,
    /// A dry run: no transfers or deletions are logged (they don't happen).
    dry_run: bool,
}

/// What changed about one item, rendered by `%i` as rsync's `YXcstpoguax`.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[derive(rkyv::Archive, rkyv::Serialize, rkyv::Deserialize)]
pub struct Change {
    /// Didn't exist (or had another type): all attribute columns show `+`.
    pub new: bool,
    /// File data written (`>`, or `<` from the sender's view).
    pub transfer: bool,
    /// Created or changed in place: directories, symlinks, specials (`c`).
    pub local: bool,
    /// Hard-linked to an earlier name (`h`).
    pub hardlink: bool,
    /// Symlink target or device number changed (`c` column).
    pub value: bool,
    pub size: bool,
    pub time: bool,
    pub perms: bool,
    pub owner: bool,
    pub group: bool,
    /// `-A`: ACLs differ (`a`).
    pub acl: bool,
    /// `-X`: extended attributes differ (`x`).
    pub xattr: bool,
}

impl Change {
    pub fn new_local() -> Change {
        Change { new: true, local: true, ..Change::default() }
    }

    pub fn any_attr(&self) -> bool {
        self.value || self.size || self.time || self.perms || self.owner || self.group || self.acl || self.xattr
    }

    /// rsync's `SIGNIFICANT_ITEM_FLAGS`: anything but a local change or a name.
    fn significant(&self) -> bool {
        self.new || self.transfer || self.any_attr()
    }
}

/// What `%L` shows after a name.
#[derive(Debug, Clone, Copy)]
pub enum Link<'a> {
    None,
    /// ` -> target`
    Sym(&'a [u8]),
    /// ` => other name` (a hard link)
    Hard(&'a [u8]),
}

/// An entry to report (rsync's `log_item`): created, changed, or (`-ii`,
/// `--info=name2`) found up to date.
#[derive(Clone, Copy)]
pub struct Item<'a> {
    /// Relative to the transfer root (empty: the root itself).
    pub rel: &'a [u8],
    /// The source entry as listed: what `%l`, `%M`, `%B`, `%U` and `%G` show.
    pub meta: &'a Meta,
    pub change: Change,
    pub link: Link<'a>,
    /// Where the source entry is on this machine (`%f` of a local copy).
    pub src: &'a Path,
    /// A remote receiver's: the listing the entry came in and its index there, by which
    /// the sending client finds its `%f` (`None`: the transfer root).
    pub at: Option<(u32, u32)>,
    /// `%b`: the file data sent for it.
    pub bytes: u64,
}

impl<'a> Item<'a> {
    pub fn new(rel: &'a [u8], meta: &'a Meta, change: Change) -> Item<'a> {
        Item { rel, meta, change, link: Link::None, src: Path::new(""), at: None, bytes: 0 }
    }

    pub fn link(self, link: Link<'a>) -> Item<'a> {
        Item { link, ..self }
    }

    pub fn from(self, src: &'a Path, at: Option<(u32, u32)>) -> Item<'a> {
        Item { src, at, ..self }
    }

    pub fn bytes(self, bytes: u64) -> Item<'a> {
        Item { bytes, ..self }
    }
}

/// An item a remote receiver reports for the client to show: in a push, what rsync's
/// sending client logs about the files it sent.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
#[derive(rkyv::Archive, rkyv::Serialize, rkyv::Deserialize)]
pub struct ItemRec {
    pub rel: Vec<u8>,
    pub at: Option<(u32, u32)>,
    pub meta: Meta,
    /// `%L`: a symlink's target, or (`true`) the name it is hard-linked to.
    pub link: Option<(bool, Vec<u8>)>,
    pub change: Change,
    pub bytes: u64,
}

impl ItemRec {
    fn of(it: &Item) -> ItemRec {
        let link = match it.link {
            Link::None => None,
            Link::Sym(t) => Some((false, t.to_vec())),
            Link::Hard(n) => Some((true, n.to_vec())),
        };
        ItemRec { rel: it.rel.to_vec(), at: it.at, meta: *it.meta, link, change: it.change, bytes: it.bytes }
    }
}

impl Output {
    /// Output for the client of a transfer, configured from the command line; `-8` skips
    /// the charset check when escaping.
    pub fn new(opts: &Options, side: Side) -> Output {
        // Checked when the options were parsed (a server's arrive from the client).
        let user = opts.out_format.as_deref().and_then(|f| Format::parse(f).ok());
        let (format, has_i) = match user {
            // rsync: `itemize_changes | 1`.
            Some(f) if f.has(b'i') => (Some(f), if opts.itemize > 1 { 3 } else { 1 }),
            Some(f) => (Some(f), 0),
            None if opts.itemize > 0 => (Format::parse("%i %n%L").ok(), opts.itemize),
            None if opts.info.name > 0 => (Format::parse("%n%L").ok(), 0),
            None => (None, 0),
        };
        Output {
            out: Mutex::new(Sink { w: BufWriter::with_capacity(64 * 1024, std::io::stdout()), progress: None }),
            tty: std::io::IsTerminal::is_terminal(&std::io::stdout()),
            verbose: opts.verbose,
            quiet: opts.quiet,
            itemize: opts.itemize,
            info: opts.info,
            del_format: has_i > 0 || format.as_ref().is_some_and(|f| f.has(b'o')),
            // %b in either format: transfers are reported once their data is known.
            after: format.as_ref().is_some_and(|f| f.has(b'b'))
                || opts.log_file_format.as_deref().is_some_and(|f| Format::parse(f).is_ok_and(|f| f.has(b'b'))),
            format,
            has_i,
            times: opts.times,
            owner: opts.owner,
            group: opts.group,
            side,
            groups: OnceLock::new(),
            charset: Charset::for_output(opts.eight_bit_output),
            forward: None,
            log: opts.log_file_format.as_deref().map(|f| {
                let format = Some(f).filter(|f| !f.is_empty()).and_then(|f| Format::parse(f).ok());
                let has_i = format.as_ref().is_some_and(|f| f.has(b'i'));
                LogSpec { del_format: has_i || format.as_ref().is_some_and(|f| f.has(b'o')), format, has_i, dry_run: opts.dry_run }
            }),
            log_file: OnceLock::new(),
        }
    }

    /// `--log-file`: open it to append to (created with mode 0644, as rsync does). One that
    /// can't be opened is reported, and the run goes on without it.
    pub fn open_log(&self, opts: &Options) {
        use std::os::unix::fs::OpenOptionsExt;
        let Some(path) = &opts.log_file else { return };
        match std::fs::OpenOptions::new().append(true).create(true).mode(0o644).open(path) {
            Ok(f) => {
                let _ = self.log_file.set(Mutex::new(f));
            }
            Err(e) => {
                self.error(path_msg("failed to open log-file ", Path::new(path), format!(": {e}")));
                self.notice("Ignoring \"log file\" setting.");
            }
        }
    }

    /// Write `text` to the log, a line each, after the time and process id.
    fn log(&self, text: &[u8]) {
        let Some(file) = self.log_file.get() else { return };
        let stamp = format!("{} [{}] ", timestamp(), std::process::id());
        let mut buf = Vec::with_capacity(stamp.len() + text.len() + 1);
        for line in text.split(|&b| b == b'\n').filter(|l| !l.is_empty()) {
            buf.extend_from_slice(stamp.as_bytes());
            escape(line, Charset::Any, &mut buf);
            buf.push(b'\n');
        }
        let _ = file.lock().unwrap_or_else(|e| e.into_inner()).write_all(&buf);
    }

    /// The log's first line: rsync's sending side builds the file list, a receiving client
    /// receives it.
    pub fn log_start(&self, receiving: bool) {
        self.log(if receiving { b"receiving file list" } else { b"building file list" });
    }

    /// The log's last line: the bytes moved and the size of the source files.
    pub fn log_end(&self, io: Io, total_size: u64) {
        let (sent, received, size) = (rsync_num(io.sent, false), rsync_num(io.received, false), rsync_num(total_size, false));
        self.log(format!("sent {sent} bytes  received {received} bytes  total size {size}").as_bytes());
    }

    /// Whether the log takes item `it` (rsync's `maybe_log_item`): what stdout's rule
    /// would list (see [`Output::shows`]; without `-i`, so not a file whose attributes
    /// alone changed), if it changed or the log's format has `%i`; never a dry run's
    /// transfers, which don't happen.
    fn logs_item(&self, it: &Item) -> bool {
        let Some(log) = &self.log else { return false };
        log.format.is_some() && !(log.dry_run && it.change.transfer) && self.listed(it) && (it.change.significant() || log.has_i)
    }

    /// Output for a server process: nothing is written locally. Lines go to `forward`
    /// unescaped, so the client escapes them for its own terminal and charset; items and
    /// deletions go as records, which the client formats as its side.
    pub fn forwarding(opts: &Options, forward: Forward) -> Output {
        Output { forward: Some(forward), ..Output::new(opts, Side::Local) }
    }

    /// Print a line a server forwarded. The text came from the other machine, so it is
    /// always escaped for this terminal whatever the peer marked it: a malicious or
    /// compromised server must not be able to put control characters or terminal escape
    /// sequences on the user's screen through a `Log` frame. `_escape_it` (the peer's own
    /// mark) is ignored for that reason; the server's forwarded lines are single lines of
    /// item text, errors or notices, which escaping leaves intact.
    pub fn print_forwarded(&self, stderr: bool, _escape_it: bool, text: &[u8]) {
        self.log(text);
        if self.quiet && !stderr {
            return;
        }
        let mut line = Vec::with_capacity(text.len() + 1);
        escape(text, self.charset, &mut line);
        line.push(b'\n');
        if stderr {
            self.flush();
            let _ = std::io::stderr().lock().write_all(&line);
        } else {
            self.out.lock().unwrap().write_lines(&line);
        }
    }

    /// Draw the progress status line. Redrawn in place on a terminal; elsewhere only
    /// the final status is printed. `last`: finish it with a newline.
    pub fn progress(&self, status: &[u8], last: bool) {
        if self.forward.is_some() || self.quiet || (!self.tty && !last) {
            return;
        }
        let mut sink = self.out.lock().unwrap();
        let _ = sink.w.write_all(b"\r");
        let _ = sink.w.write_all(status);
        if self.tty {
            let _ = sink.w.write_all(b"\x1b[K");
        }
        if last {
            let _ = sink.w.write_all(b"\n");
            sink.progress = None;
        } else {
            sink.progress = Some(status.to_vec());
        }
        let _ = sink.w.flush();
    }

    pub fn verbose(&self) -> u8 {
        self.verbose
    }

    /// The `--info` levels.
    pub fn levels(&self) -> &Info {
        &self.info
    }

    /// Item lines show `%i` change summaries (so attribute differences are worth finding).
    pub fn itemizes(&self) -> bool {
        self.has_i > 0
    }

    /// Unchanged items are reported too: listed (`-ii`, `--info=name2` with `%i`), or
    /// "X is uptodate" (`--info=name2`).
    pub fn all_items(&self) -> bool {
        self.format.is_some() && (self.has_i > 1 || self.info.name > 1)
    }

    /// The format has `%b`: a transfer is reported once the data sent for it is known,
    /// not when it is decided on (rsync's `log_before_transfer` off).
    pub fn after(&self) -> bool {
        self.after
    }

    /// Item lines, on stdout or in the log, use the escape `letter`.
    pub fn uses(&self, letter: u8) -> bool {
        let log = self.log.as_ref().and_then(|l| l.format.as_ref());
        self.format.iter().chain(log).any(|f| f.has(letter))
    }

    fn line(&self, bytes: &[u8]) {
        if let Some(f) = &self.forward {
            return f(Msg::Log { stderr: false, escape: false, text: bytes.to_vec() });
        }
        let mut line = Vec::with_capacity(bytes.len() + 1);
        line.extend_from_slice(bytes);
        line.push(b'\n');
        self.out.lock().unwrap().write_lines(&line);
    }

    /// Write a line that may contain names. Like rsync, the whole line is escaped,
    /// `--out-format` text included.
    fn escaped_line(&self, text: &[u8]) {
        if let Some(f) = &self.forward {
            return f(Msg::Log { stderr: false, escape: true, text: text.to_vec() });
        }
        let mut line = Vec::with_capacity(text.len() + 1);
        escape(text, self.charset, &mut line);
        line.push(b'\n');
        self.out.lock().unwrap().write_lines(&line);
    }

    /// `%i`: rsync's 11-character change summary.
    fn itemize_field(&self, kind: Kind, c: &Change) -> [u8; 11] {
        let mut f = [b'.'; 11];
        f[0] = if c.hardlink {
            b'h'
        } else if c.local {
            b'c'
        } else if c.transfer {
            if self.side == Side::Push { b'<' } else { b'>' }
        } else {
            b'.'
        };
        f[1] = match kind {
            Kind::File => b'f',
            Kind::Dir => b'd',
            Kind::Symlink => b'L',
            Kind::CharDev | Kind::BlockDev => b'D',
            Kind::Fifo | Kind::Socket => b'S',
        };
        if c.new {
            f[2..].fill(b'+');
        } else if !c.any_attr() {
            // rsync: no attribute changed, so the columns are blank (`hf`, `cd`, `>f`).
            f[2..].fill(b' ');
        } else {
            let set = |on: bool, ch: u8| if on { ch } else { b'.' };
            f[2] = set(c.value, b'c');
            f[3] = set(c.size, b's');
            f[4] = set(c.time, if self.times { b't' } else { b'T' });
            f[5] = set(c.perms, b'p');
            f[6] = set(c.owner, b'o');
            f[7] = set(c.group, b'g');
            f[9] = set(c.acl, b'a');
            f[10] = set(c.xattr, b'x');
        }
        f
    }

    /// Report an item, as rsync's client does: a transfer always; with `%i`, any change
    /// (`-ii`, `--info=name2`: any item); otherwise what was created or changed in place,
    /// a hard link, or a directory that changed. With `--info=name2` and no `%i`, rsync
    /// names the other entries that aren't directories: "X is uptodate", or just "X"
    /// when only their attributes were updated.
    pub fn item(&self, it: &Item) {
        let logged = self.logs_item(it);
        if (self.quiet || self.format.is_none()) && !logged {
            return;
        }
        // A server reports what the client may show or log: the client decides.
        if self.shows(it) || logged {
            return match &self.forward {
                Some(f) => f(Msg::Item(Box::new(ItemRec::of(it)))),
                None => self.report(it, self.long_name(it.rel, it.src)),
            };
        }
        if !self.quiet && self.format.is_some() && self.has_i == 0 && self.info.name > 1 && it.meta.kind != Kind::Dir {
            let state: &[u8] = if it.change.any_attr() { b"" } else { b" is uptodate" };
            self.escaped_line(&[it.rel, state].concat());
        }
    }

    /// An item a remote receiver reported (see [`ItemRec`]); `src` is where its source
    /// is on this side.
    pub fn peer_item(&self, rec: &ItemRec, src: &Path) {
        if (self.quiet || self.format.is_none()) && self.log.is_none() {
            return;
        }
        let link = match &rec.link {
            None => Link::None,
            Some((false, t)) => Link::Sym(t),
            Some((true, n)) => Link::Hard(n),
        };
        let it = Item { rel: &rec.rel, meta: &rec.meta, change: rec.change, link, src, at: rec.at, bytes: rec.bytes };
        self.report(&it, self.long_name(&rec.rel, src));
    }

    /// Whether stdout lists item `it` (its format given), as rsync's client does: a
    /// transfer always; with `%i`, any change (`-ii`, `--info=name2`: any item);
    /// otherwise what was created or changed in place, a hard link, or a directory that
    /// changed.
    fn shows(&self, it: &Item) -> bool {
        self.format.is_some() && self.listed(it)
    }

    /// [`Output::shows`]' rule, whatever the format.
    fn listed(&self, it: &Item) -> bool {
        let c = it.change;
        let significant = c.significant();
        let named = matches!(it.link, Link::Hard(_));
        c.transfer
            || (self.has_i > 0 && (significant || named || self.has_i > 1 || self.info.name > 1))
            || ((c.local || c.hardlink) && significant)
            || named
            || (it.meta.kind == Kind::Dir && significant)
    }

    /// Show item `it` on stdout and in the log, as each wants it; `fname` is its `%f`.
    fn report(&self, it: &Item, fname: Vec<u8>) {
        if !self.quiet && self.shows(it) {
            self.show(it, &fname);
        }
        if self.logs_item(it)
            && let Some(format) = self.log.as_ref().and_then(|l| l.format.as_ref())
        {
            self.log(&self.render(format, it, &fname));
        }
    }

    /// `%f`: the source path for a sender (a transfer root's directory ends in `/.`, a
    /// leading `/` is dropped), the name in the destination for a receiver.
    fn long_name(&self, rel: &[u8], src: &Path) -> Vec<u8> {
        if self.side == Side::Pull {
            return clean_fname(rel);
        }
        let mut path = src.as_os_str().as_bytes().to_vec();
        if rel.is_empty() {
            path.extend_from_slice(b"/.");
        }
        let mut name = clean_fname(&path);
        if name.len() > 1 && name[0] == b'/' {
            name.remove(0);
        }
        name
    }

    fn show(&self, it: &Item, fname: &[u8]) {
        let Some(format) = &self.format else { return };
        self.escaped_line(&self.render(format, it, fname));
    }

    /// Item `it` in `format`; `fname` is its `%f`.
    fn render(&self, format: &Format, it: &Item, fname: &[u8]) -> Vec<u8> {
        let kind = it.meta.kind;
        let mut name = if it.rel.is_empty() { b".".to_vec() } else { it.rel.to_vec() };
        if kind == Kind::Dir {
            name.push(b'/');
        }
        let changes = self.itemize_field(kind, &it.change);
        let link = match it.link {
            Link::None => None,
            Link::Sym(t) => Some((&b" -> "[..], t)),
            Link::Hard(n) => Some((&b" => "[..], n)),
        };
        let fields = Fields {
            op: if self.side == Side::Pull { b"recv" } else { b"send" },
            name: &name,
            fname,
            link,
            changes: &changes,
            size: it.meta.size,
            bytes: it.bytes,
            mtime: it.meta.mtime,
            mode: it.meta.mode,
            uid: if self.owner { it.meta.uid } else { 0 },
            gid: self.shown_gid(it.meta.gid),
        };
        format.render(&fields)
    }

    /// `%G`: rsync's receiver shows `DEFAULT` for a group it can't give a file.
    fn shown_gid(&self, gid: u32) -> Option<u32> {
        if !self.group {
            return None;
        }
        if self.side != Side::Pull {
            return Some(gid);
        }
        let (root, mine) = self.groups.get_or_init(|| (crate::fsops::am_root(), crate::fsops::my_groups()));
        (*root || mine.contains(&gid)).then_some(gid)
    }

    /// A deletion: in the format when it has `%o` or `%i` (`del.`, `*deleting  `), else
    /// `deleting NAME`; shown with `--info=del` (`-v`) or any item format. `mode`: the
    /// deleted entry's permissions, which a receiving client's `%B` shows.
    pub fn deleting(&self, rel: &[u8], is_dir: bool, mode: u32) {
        let shown = !self.quiet && (self.info.del > 0 || self.format.is_some());
        let logged = self.log.as_ref().is_some_and(|l| !l.dry_run);
        if !shown && !logged {
            return;
        }
        if let Some(f) = &self.forward {
            return f(Msg::Deleted { rel: rel.to_vec(), dir: is_dir });
        }
        if logged && let Some(log) = &self.log {
            let format = log.format.as_ref().filter(|_| log.del_format);
            self.log(&self.deletion_line(format, rel, is_dir, mode));
        }
        if shown {
            let format = self.format.as_ref().filter(|_| self.del_format);
            self.escaped_line(&self.deletion_line(format, rel, is_dir, mode));
        }
    }

    /// A deletion in `format`, or as `deleting NAME`.
    fn deletion_line(&self, format: Option<&Format>, rel: &[u8], is_dir: bool, mode: u32) -> Vec<u8> {
        let mut name = rel.to_vec();
        if is_dir {
            name.push(b'/');
        }
        match format {
            Some(format) => {
                let fields = Fields {
                    op: b"del.",
                    name: &name,
                    fname: &clean_fname(rel),
                    changes: b"*deleting  ",
                    // rsync's sending client is told only names; its receiver knows more.
                    mode: if self.side == Side::Pull { mode } else { 0 },
                    gid: self.group.then_some(0),
                    ..Fields::default()
                };
                format.render(&fields)
            }
            None => [&b"deleting "[..], &name].concat(),
        }
    }

    /// Deletions show their permissions (`%B` in a receiving client's deletion format, on
    /// stdout or in the log).
    pub fn deletion_modes(&self) -> bool {
        let shown = self.del_format && self.format.as_ref().is_some_and(|f| f.has(b'B'));
        let logged = self.log.as_ref().is_some_and(|l| l.del_format && l.format.as_ref().is_some_and(|f| f.has(b'B')));
        self.side == Side::Pull && (shown || logged)
    }

    /// "created directory DEST" and `--mkpath`'s "created N directories for DEST": with
    /// `--info=name` (`-v`) or `%i`, as in rsync.
    pub fn created(&self, msg: impl AsRef<[u8]>) {
        if self.info.name > 0 || self.has_i > 0 {
            self.log(msg.as_ref());
            if !self.quiet {
                self.escaped_line(msg.as_ref());
            }
        }
    }

    /// rsync's file-list message as the transfer starts (`--info=flist`, which `-v`
    /// implies): incremental recursion's, or (without it, `-r` or `-d`) the one for a
    /// file list built first. `receiving`: this side receives the list.
    pub fn flist_start(&self, opts: &Options, receiving: bool) {
        if self.quiet || self.info.flist == 0 {
            return;
        }
        let msg = match (opts.inc_recurse, receiving) {
            (true, false) => "sending incremental file list",
            (true, true) => "receiving incremental file list",
            _ if !(opts.recursive || opts.dirs) || self.info.flist > 1 => return,
            (false, false) => "building file list ... done",
            (false, true) => "receiving file list ... done",
        };
        self.line(msg.as_bytes());
    }

    /// `--info=flist2` without incremental recursion: how many entries the file list had,
    /// with rsync's running count while building it (rsync shows this before the
    /// transfer, barqsync once it is done).
    pub fn flist_end(&self, opts: &Options, receiving: bool, count: u64) {
        if self.quiet || self.info.flist < 2 || opts.inc_recurse || !(opts.recursive || opts.dirs) {
            return;
        }
        let mut s = String::from(if receiving { "receiving file list ... " } else { "building file list ... \n" });
        if !receiving {
            for n in (0..count).step_by(100) {
                s += &format!(" {n} files...\r");
            }
        }
        s += &format!("{count} file{}to consider", if count == 1 { " " } else { "s " });
        self.line(s.as_bytes());
    }

    /// Informational message of barqsync's own, shown with `-v` or `-i`; may contain raw
    /// name bytes.
    pub fn info(&self, msg: impl AsRef<[u8]>) {
        if self.verbose > 0 || self.itemize > 0 {
            self.log(msg.as_ref());
            self.escaped_line(msg.as_ref());
        }
    }

    /// A notice shown without `-v` but not with `-q` (rsync's `FINFO` messages, which its
    /// log gets either way). Not escaped: it must not contain names.
    pub fn notice(&self, msg: impl AsRef<[u8]>) {
        self.log(msg.as_ref());
        if !self.quiet {
            self.line(msg.as_ref());
        }
    }

    /// A message shown without `-v` but not with `-q` that may name entries (rsync's
    /// `FINFO` lines, such as "cannot delete non-empty directory", or those an `--info`
    /// level asks for).
    pub fn note(&self, msg: impl AsRef<[u8]>) {
        self.log(msg.as_ref());
        if !self.quiet {
            self.escaped_line(msg.as_ref());
        }
    }

    /// Summary lines (not with `-q`). Not escaped, so the message may contain newlines
    /// but must not contain names.
    pub fn always(&self, msg: impl AsRef<[u8]>) {
        self.log(msg.as_ref());
        if !self.quiet {
            self.line(msg.as_ref());
        }
    }

    /// A warning to stderr, without the program name in front (rsync's `FWARNING`, such
    /// as "file has vanished"); escaped like errors.
    pub fn warning(&self, msg: impl AsRef<[u8]>) {
        if let Some(f) = &self.forward {
            return f(Msg::Log { stderr: true, escape: true, text: msg.as_ref().to_vec() });
        }
        self.log(msg.as_ref());
        self.flush();
        let mut line = Vec::new();
        escape(msg.as_ref(), self.charset, &mut line);
        line.push(b'\n');
        let _ = std::io::stderr().lock().write_all(&line);
    }

    /// Error to stderr, escaped like the other lines. Build names into the message with
    /// [`path_msg`], not `Path::display()`, which turns invalid UTF-8 into U+FFFD.
    pub fn error(&self, msg: impl AsRef<[u8]>) {
        if let Some(f) = &self.forward {
            return f(Msg::Log { stderr: true, escape: true, text: [b"barqsync: ", msg.as_ref()].concat() });
        }
        self.log(&[b"barqsync: ", msg.as_ref()].concat());
        self.flush();
        let mut line = b"barqsync: ".to_vec();
        escape(msg.as_ref(), self.charset, &mut line);
        line.push(b'\n');
        let _ = std::io::stderr().lock().write_all(&line);
    }

    pub fn flush(&self) {
        let _ = self.out.lock().unwrap().w.flush();
    }
}

/// The local time as rsync's log shows it (its `timestring`).
fn timestamp() -> String {
    let t = unsafe { libc::time(std::ptr::null_mut()) };
    // SAFETY: `tm` is filled by `localtime_r` before it is read.
    let mut tm: libc::tm = unsafe { std::mem::zeroed() };
    if unsafe { libc::localtime_r(&t, &mut tm) }.is_null() {
        return "(time out of range)".into();
    }
    format!("{:4}/{:02}/{:02} {:02}:{:02}:{:02}", tm.tm_year + 1900, tm.tm_mon + 1, tm.tm_mday, tm.tm_hour, tm.tm_min, tm.tm_sec)
}

/// rsync's `clean_fname` without options: no doubled `/`, no `.` components but a last
/// one (`dir/.` stays), no trailing `/`; `.` when nothing is left.
fn clean_fname(p: &[u8]) -> Vec<u8> {
    let anchored = p.first() == Some(&b'/');
    let mut out = Vec::with_capacity(p.len());
    if anchored {
        out.push(b'/');
    }
    let mut i = usize::from(anchored);
    while i < p.len() {
        if p[i] == b'/' {
            i += 1;
        } else if p[i] == b'.' && p.get(i + 1) == Some(&b'/') {
            i += 2;
        } else {
            while i < p.len() {
                out.push(p[i]);
                i += 1;
                if p[i - 1] == b'/' {
                    break;
                }
            }
        }
    }
    if out.len() > usize::from(anchored) && out.last() == Some(&b'/') {
        out.pop();
    }
    if out.is_empty() {
        out.push(b'.');
    }
    out
}

/// A message naming `path`, built from its raw bytes: invalid UTF-8 then reaches
/// [`escape`] and prints as `\#ooo`, as in rsync.
pub fn path_msg(before: &str, path: &Path, after: impl AsRef<[u8]>) -> Vec<u8> {
    [before.as_bytes(), path.as_os_str().as_bytes(), after.as_ref()].concat()
}

/// The charset names are checked against before printing (rsync's `ic_chck`).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Charset {
    /// 7-bit ASCII (the C/POSIX locale on Linux): every byte >= 0x80 is escaped.
    Ascii,
    /// UTF-8: each byte that does not start a well-formed sequence is escaped.
    Utf8,
    /// No check (`-8`, or a single-byte locale charset): only control characters are escaped.
    Any,
}

impl Charset {
    /// The charset rsync would check output against: none with `-8`, else the locale's,
    /// which is looked up once per process.
    pub fn for_output(eight_bit: bool) -> Charset {
        static LOCALE: OnceLock<Charset> = OnceLock::new();
        if eight_bit { Charset::Any } else { *LOCALE.get_or_init(locale_charset) }
    }
}

/// Linux: rsync runs `setlocale(LC_CTYPE, "")` and checks names with an iconv converter
/// from `nl_langinfo(CODESET)` to itself. `newlocale` resolves the same LC_ALL /
/// LC_CTYPE / LANG settings without changing the process locale; a locale that is not
/// installed leaves glibc rsync in "C", i.e. ASCII.
#[cfg(target_os = "linux")]
fn locale_charset() -> Charset {
    // SAFETY: the locale object is ours; CODESET is copied out before it is freed.
    let codeset = unsafe {
        let loc = libc::newlocale(libc::LC_CTYPE_MASK, c"".as_ptr(), std::ptr::null_mut());
        if loc.is_null() {
            return Charset::Ascii;
        }
        let name = std::ffi::CStr::from_ptr(libc::nl_langinfo_l(libc::CODESET, loc)).to_bytes().to_vec();
        libc::freelocale(loc);
        name
    };
    charset_from_codeset(&codeset)
}

/// macOS: Homebrew rsync takes the charset from libcharset's `locale_charset()`, which
/// reports UTF-8 on Darwin whatever the locale (measured with rsync 3.5.1 under LC_ALL=C
/// and en_US.ISO8859-1), so valid UTF-8 is never escaped there. Mirrored rather than
/// applying the Linux rule: APFS only stores UTF-8 names, and escaping them under
/// LC_ALL=C would only make barqsync's output differ from the local rsync's.
#[cfg(not(target_os = "linux"))]
fn locale_charset() -> Charset {
    Charset::Utf8
}

/// Map a CODESET name to the check rsync's same-charset iconv converter performs.
/// Multibyte legacy charsets (EUC-JP, GB18030, ...) map to [`Charset::Any`]: an
/// approximation, since iconv would also reject their malformed sequences.
#[cfg(any(target_os = "linux", test))]
fn charset_from_codeset(name: &[u8]) -> Charset {
    let norm: Vec<u8> = name.iter().filter(|c| c.is_ascii_alphanumeric()).map(u8::to_ascii_lowercase).collect();
    match norm.as_slice() {
        b"utf8" => Charset::Utf8,
        b"ansix341968" | b"ascii" | b"usascii" | b"646" => Charset::Ascii,
        _ => Charset::Any,
    }
}

/// Append `text` to `out` escaped the way rsync prints log lines, each escaped byte
/// written as `\#ooo` (3-digit octal): bytes not valid in `charset`; control characters
/// other than TAB; and the backslash of a literal `\#ddd`, so the output stays
/// unambiguous. DEL and valid non-ASCII characters (C1 controls included) pass through.
pub fn escape(text: &[u8], charset: Charset, out: &mut Vec<u8>) {
    // rsync runs the iconv check first and filters each valid run separately, so a
    // literal `\#ddd` interrupted by an invalid byte is not treated as one.
    let mut run = 0;
    let mut i = 0;
    while i < text.len() {
        match valid_len(&text[i..], charset) {
            Some(n) => i += n,
            None => {
                escape_controls(&text[run..i], out);
                push_octal(text[i], out);
                i += 1;
                run = i;
            }
        }
    }
    escape_controls(&text[run..], out);
}

/// Length of the character at the start of `s` (non-empty) if it is valid in `charset`.
fn valid_len(s: &[u8], charset: Charset) -> Option<usize> {
    match charset {
        Charset::Any => Some(1),
        Charset::Ascii => s[0].is_ascii().then_some(1),
        Charset::Utf8 => utf8_len(s),
    }
}

/// Length of the UTF-8 sequence at the start of `s` as glibc's and libiconv's decoders
/// accept it: shortest form and no surrogates but, unlike `str::from_utf8`, the old
/// 5- and 6-byte forms and code points above U+10FFFF are fine.
fn utf8_len(s: &[u8]) -> Option<usize> {
    let (n, lead) = match s[0] {
        0x00..=0x7f => return Some(1),
        b @ 0xc2..=0xdf => (2, b & 0x1f),
        b @ 0xe0..=0xef => (3, b & 0x0f),
        b @ 0xf0..=0xf7 => (4, b & 0x07),
        b @ 0xf8..=0xfb => (5, b & 0x03),
        b @ 0xfc..=0xfd => (6, b & 0x01),
        _ => return None,
    };
    let mut cp = u32::from(lead);
    for &b in s.get(1..n)? {
        if b & 0xc0 != 0x80 {
            return None;
        }
        cp = cp << 6 | u32::from(b & 0x3f);
    }
    const MIN: [u32; 7] = [0, 0, 0x80, 0x800, 0x1_0000, 0x20_0000, 0x400_0000];
    (cp >= MIN[n] && !(0xd800..=0xdfff).contains(&cp)).then_some(n)
}

/// rsync's `filtered_fwrite` without its isprint() check.
fn escape_controls(run: &[u8], out: &mut Vec<u8>) {
    for (i, &b) in run.iter().enumerate() {
        let literal_escape = b == b'\\'
            && run.len() - i > 4
            && run[i + 1] == b'#'
            && run[i + 2..i + 5].iter().all(u8::is_ascii_digit);
        if (b < b' ' && b != b'\t') || literal_escape {
            push_octal(b, out);
        } else {
            out.push(b);
        }
    }
}

fn push_octal(b: u8, out: &mut Vec<u8>) {
    out.extend_from_slice(&[b'\\', b'#', b'0' + (b >> 6), b'0' + (b >> 3 & 7), b'0' + (b & 7)]);
}

#[derive(Default)]
pub struct Stats {
    pub files: AtomicU64,
    pub dirs: AtomicU64,
    pub links: AtomicU64,
    pub specials: AtomicU64,
    pub total_size: AtomicU64,
    pub created: AtomicU64,
    pub transferred: AtomicU64,
    pub transferred_bytes: AtomicU64,
    pub cloned: AtomicU64,
    pub delta_files: AtomicU64,
    pub literal_bytes: AtomicU64,
    pub matched_bytes: AtomicU64,
    /// Entries `--delete` removed (not those replaced by another type of entry).
    pub deleted: AtomicU64,
    pub errors: AtomicU64,
    /// Source files that were gone when they were to be sent (rsync's "file has vanished").
    pub vanished: AtomicU64,
    /// Deletions `--max-delete` stopped.
    pub del_skipped: AtomicU64,
}

/// Plain copy of [`Stats`] for sending over the wire.
#[derive(Debug, Clone, Default, serde::Serialize, serde::Deserialize)]
#[derive(rkyv::Archive, rkyv::Serialize, rkyv::Deserialize)]
// Boxed: inline, it would make every protocol message as big as these counters.
pub struct StatsSnapshot(pub Box<[u64; 16]>);

impl StatsSnapshot {
    pub fn errors(&self) -> u64 {
        self.0[13]
    }

    /// The exit code these counters call for (see [`exit_code`]).
    pub fn exit_code(&self) -> i32 {
        exit_code(self.0[13], self.0[14], self.0[15])
    }
}

/// rsync's exit code after a transfer that ran to the end: 23 after errors, else 24 when
/// source files vanished, else 25 when `--max-delete` stopped deletions, else 0.
pub fn exit_code(errors: u64, vanished: u64, del_skipped: u64) -> i32 {
    if errors > 0 {
        crate::exit::PARTIAL
    } else if vanished > 0 {
        crate::exit::VANISHED
    } else if del_skipped > 0 {
        crate::exit::DEL_LIMIT
    } else {
        crate::exit::OK
    }
}

impl Stats {
    fn fields(&self) -> [&AtomicU64; 16] {
        [
            &self.files,
            &self.dirs,
            &self.links,
            &self.specials,
            &self.total_size,
            &self.created,
            &self.transferred,
            &self.transferred_bytes,
            &self.cloned,
            &self.delta_files,
            &self.literal_bytes,
            &self.matched_bytes,
            &self.deleted,
            &self.errors,
            &self.vanished,
            &self.del_skipped,
        ]
    }

    /// The exit code these counters call for (see [`exit_code`]).
    pub fn exit_code(&self) -> i32 {
        exit_code(Self::get(&self.errors), Self::get(&self.vanished), Self::get(&self.del_skipped))
    }

    /// Entries in the file list ("Number of files").
    pub fn entries(&self) -> u64 {
        [&self.files, &self.dirs, &self.links, &self.specials].into_iter().map(Self::get).sum()
    }

    pub fn snapshot(&self) -> StatsSnapshot {
        StatsSnapshot(Box::new(self.fields().map(Self::get)))
    }

    /// Add another process's counters into these.
    pub fn absorb(&self, s: &StatsSnapshot) {
        for (c, v) in self.fields().into_iter().zip(*s.0) {
            Self::add(c, v);
        }
    }

    pub fn add(c: &AtomicU64, n: u64) {
        c.fetch_add(n, Relaxed);
    }

    pub fn get(c: &AtomicU64) -> u64 {
        c.load(Relaxed)
    }

    /// The summary at `--info=stats` level `level`: none at 0, the closing lines of `-v`
    /// at 1, also `--stats`' counters from 2 on. `io`: the bytes this side sent and
    /// received, which rsync's lines report.
    pub fn print(&self, out: &Output, elapsed: Duration, human: bool, level: u8, io: Io) {
        if level == 0 {
            return;
        }
        let g = Self::get;
        let num = |n: u64| if human { human_num(n) } else { n.to_string() };
        let secs = elapsed.as_secs_f64().max(1e-9);
        if level > 1 {
            let total = g(&self.files) + g(&self.dirs) + g(&self.links) + g(&self.specials);
            out.always(format!(
                "\nNumber of files: {} (reg: {}, dir: {}, link: {}, special: {})",
                num(total),
                num(g(&self.files)),
                num(g(&self.dirs)),
                num(g(&self.links)),
                num(g(&self.specials))
            ));
            out.always(format!("Number of created files: {}", num(g(&self.created))));
            out.always(format!("Number of deleted files: {}", num(g(&self.deleted))));
            out.always(format!(
                "Number of regular files transferred: {} ({} cloned)",
                num(g(&self.transferred)),
                num(g(&self.cloned))
            ));
            out.always(format!("Number of files updated by delta: {}", num(g(&self.delta_files))));
            out.always(format!("Total file size: {} bytes", num(g(&self.total_size))));
            out.always(format!(
                "Total transferred file size: {} bytes",
                num(g(&self.transferred_bytes))
            ));
            out.always(format!("Literal data: {} bytes", num(g(&self.literal_bytes))));
            out.always(format!("Matched data: {} bytes", num(g(&self.matched_bytes))));
            out.always(format!("Total bytes sent: {}", rsync_num(io.sent, human)));
            out.always(format!("Total bytes received: {}", rsync_num(io.received, human)));
            out.always(format!("Elapsed: {secs:.3}s"));
        }
        out.always(format!(
            "\ntransferred {} files, {} bytes in {:.3}s ({}/s); total size is {}",
            num(g(&self.transferred)),
            num(g(&self.transferred_bytes)),
            secs,
            human_num((g(&self.transferred_bytes) as f64 / secs) as u64),
            num(g(&self.total_size)),
        ));
        // rsync's own two lines, which scripts look for.
        let moved = io.sent + io.received;
        out.always(format!(
            "\nsent {} bytes  received {} bytes  {} bytes/sec",
            rsync_num(io.sent, human),
            rsync_num(io.received, human),
            rsync_dnum(moved as f64 / secs, human),
        ));
        out.always(format!(
            "total size is {}  speedup is {}{}",
            rsync_num(g(&self.total_size), human),
            rsync_dnum(g(&self.total_size) as f64 / moved.max(1) as f64, false),
            if io.dry_run { " (DRY RUN)" } else { "" },
        ));
    }
}

/// What rsync's summary lines report besides the counters.
#[derive(Debug, Clone, Copy, Default)]
pub struct Io {
    /// Bytes this side wrote to the connection (a local copy: the file data written).
    pub sent: u64,
    /// Bytes it read from the connection.
    pub received: u64,
    pub dry_run: bool,
}

/// A number as rsync prints it: with thousands separators, or with `-h` in units of
/// 1000 (`1.23K`).
pub fn rsync_num(n: u64, human: bool) -> String {
    if human && n >= 1000 {
        let mut v = n as f64 / 1000.0;
        let mut units = b"KMGTP".iter();
        let mut unit = units.next().copied().unwrap_or(b'K');
        while v >= 1000.0 {
            match units.next() {
                Some(&u) => {
                    v /= 1000.0;
                    unit = u;
                }
                None => break,
            }
        }
        return format!("{v:.2}{}", unit as char);
    }
    let digits = n.to_string();
    let mut out = String::with_capacity(digits.len() + digits.len() / 3);
    for (i, c) in digits.chars().enumerate() {
        if i > 0 && (digits.len() - i).is_multiple_of(3) {
            out.push(',');
        }
        out.push(c);
    }
    out
}

/// A fractional number as rsync prints it (two decimals; see [`rsync_num`]).
pub fn rsync_dnum(v: f64, human: bool) -> String {
    let v = if v.is_finite() && v >= 0.0 { v } else { 0.0 };
    if v < 1000.0 {
        return format!("{v:.2}");
    }
    if human {
        return rsync_num(v as u64, true);
    }
    let fixed = format!("{v:.2}");
    let (int, frac) = fixed.split_once('.').unwrap_or((&fixed, "00"));
    format!("{}.{frac}", rsync_num(int.parse().unwrap_or(0), false))
}

pub fn human_num(n: u64) -> String {
    const UNITS: [&str; 6] = ["", "K", "M", "G", "T", "P"];
    let mut v = n as f64;
    let mut i = 0;
    while v >= 1024.0 && i < UNITS.len() - 1 {
        v /= 1024.0;
        i += 1;
    }
    if i == 0 { format!("{n}") } else { format!("{v:.2}{}", UNITS[i]) }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn esc(text: &[u8], charset: Charset) -> Vec<u8> {
        let mut out = Vec::new();
        escape(text, charset, &mut out);
        out
    }

    /// Expected output measured with glibc rsync 3.2.7 (LC_ALL=C, C.UTF-8, and `-8`) and
    /// Homebrew rsync 3.5.1 on macOS, which agree wherever both can represent the name.
    #[test]
    fn escapes_like_rsync() {
        use Charset::{Any, Ascii, Utf8};
        type Case = (&'static [u8], &'static [u8], &'static [u8], &'static [u8]);
        #[rustfmt::skip]
        let cases: &[Case] = &[
            // input, Ascii, Utf8, Any
            (b"plain name", b"plain name", b"plain name", b"plain name"),
            (b"nl\nx", br"nl\#012x", br"nl\#012x", br"nl\#012x"),
            (b"esc\x1b[1m", br"esc\#033[1m", br"esc\#033[1m", br"esc\#033[1m"),
            (b"tab\tx", b"tab\tx", b"tab\tx", b"tab\tx"),
            (b"del\x7fx", b"del\x7fx", b"del\x7fx", b"del\x7fx"),
            (br"lit\#123x", br"lit\#134#123x", br"lit\#134#123x", br"lit\#134#123x"),
            (br"x\#999", br"x\#134#999", br"x\#134#999", br"x\#134#999"),
            (br"lit\#12x", br"lit\#12x", br"lit\#12x", br"lit\#12x"),
            (b"caf\xc3\xa9", br"caf\#303\#251", b"caf\xc3\xa9", b"caf\xc3\xa9"),
            (b"c1\xc2\x9bx", br"c1\#302\#233x", b"c1\xc2\x9bx", b"c1\xc2\x9bx"),
            (b"nonchar\xef\xbf\xbe", br"nonchar\#357\#277\#276", b"nonchar\xef\xbf\xbe", b"nonchar\xef\xbf\xbe"),
            (b"bad\xffx", br"bad\#377x", br"bad\#377x", b"bad\xffx"),
            (b"lat\xe9x", br"lat\#351x", br"lat\#351x", b"lat\xe9x"),
            (b"overlong\xc0\x80", br"overlong\#300\#200", br"overlong\#300\#200", b"overlong\xc0\x80"),
            (b"surr\xed\xa0\x80", br"surr\#355\#240\#200", br"surr\#355\#240\#200", b"surr\xed\xa0\x80"),
            (b"trunc\xe2\x82", br"trunc\#342\#202", br"trunc\#342\#202", b"trunc\xe2\x82"),
            (b"trunc2\xe2\x82(", br"trunc2\#342\#202(", br"trunc2\#342\#202(", b"trunc2\xe2\x82("),
            // A backslash-hash cut off by an invalid byte is not a literal escape.
            (b"a\\#\xff123", br"a\#\#377123", br"a\#\#377123", b"a\\#\xff123"),
        ];
        for &(input, ascii, utf8, any) in cases {
            for (charset, want) in [(Ascii, ascii), (Utf8, utf8), (Any, any)] {
                let got = esc(input, charset);
                assert_eq!(got.escape_ascii().to_string(), want.escape_ascii().to_string(), "{charset:?} {}", input.escape_ascii());
            }
        }
    }

    #[test]
    fn utf8_check_matches_iconv_not_std() {
        // Beyond U+10FFFF and the old 5/6-byte forms: accepted by glibc and libiconv.
        for s in [&b"\xf4\x90\x80\x80"[..], b"\xf5\x80\x80\x80", b"\xf8\x88\x80\x80\x80", b"\xfc\x84\x80\x80\x80\x80"] {
            assert_eq!(utf8_len(s), Some(s.len()), "{s:?}");
        }
        // Overlong forms of each length, lone continuation bytes, and 0xfe/0xff.
        for s in [&b"\xe0\x80\x80"[..], b"\xf0\x80\x80\x80", b"\xf8\x80\x80\x80\x80", b"\xfc\x80\x80\x80\x80\x80", b"\x80", b"\xfe", b"\xff"] {
            assert_eq!(utf8_len(s), None, "{s:?}");
        }
    }

    #[test]
    fn numbers_print_like_rsync() {
        assert_eq!(rsync_num(0, false), "0");
        assert_eq!(rsync_num(999, false), "999");
        assert_eq!(rsync_num(1000, false), "1,000");
        assert_eq!(rsync_num(1_234_567, false), "1,234,567");
        assert_eq!(rsync_num(999, true), "999");
        assert_eq!(rsync_num(1234, true), "1.23K");
        assert_eq!(rsync_num(5_600_000, true), "5.60M");
        assert_eq!(rsync_dnum(874.0, false), "874.00");
        assert_eq!(rsync_dnum(12_345.678, false), "12,345.68");
        assert_eq!(rsync_dnum(12_345.678, true), "12.35K");
        assert_eq!(rsync_dnum(f64::NAN, false), "0.00");
        assert_eq!(exit_code(1, 1, 1), 23);
        assert_eq!(exit_code(0, 1, 1), 24);
        assert_eq!(exit_code(0, 0, 1), 25);
        assert_eq!(exit_code(0, 0, 0), 0);
    }

    #[test]
    fn codeset_names() {
        assert_eq!(charset_from_codeset(b"ANSI_X3.4-1968"), Charset::Ascii);
        assert_eq!(charset_from_codeset(b"ASCII"), Charset::Ascii);
        assert_eq!(charset_from_codeset(b"US-ASCII"), Charset::Ascii);
        assert_eq!(charset_from_codeset(b"UTF-8"), Charset::Utf8);
        assert_eq!(charset_from_codeset(b"utf8"), Charset::Utf8);
        assert_eq!(charset_from_codeset(b"ISO-8859-1"), Charset::Any);
        assert_eq!(Charset::for_output(true), Charset::Any);
    }
}

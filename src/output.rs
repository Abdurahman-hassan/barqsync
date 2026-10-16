//! User-facing output: itemized names (rsync `%n%L` style), deletions, errors and stats.
//!
//! Every line that can carry a file name goes through [`escape`], which mirrors rsync's
//! log.c (`rwrite` + `filtered_fwrite`), so a name cannot put control characters or
//! terminal escape sequences on the user's terminal.

use crate::entry::Kind;
use std::io::{BufWriter, Stdout, Write};
use std::os::unix::ffi::OsStrExt;
use std::path::Path;
use std::sync::atomic::{AtomicU64, Ordering::Relaxed};
use std::sync::{Mutex, OnceLock};
use std::time::Duration;

/// Where a server sends its output instead of its own stdout/stderr:
/// `(stderr, escape, text)`, printed by the client (see [`Output::print_forwarded`]).
pub type Forward = Box<dyn Fn(bool, bool, &[u8]) + Send + Sync>;

pub struct Output {
    out: Mutex<BufWriter<Stdout>>,
    verbose: u8,
    format: Option<String>,
    charset: Charset,
    forward: Option<Forward>,
}

impl Output {
    /// `eight_bit` is `-8`/`--8-bit-output`: skip the charset check, escape only control characters.
    pub fn new(verbose: u8, out_format: Option<String>, eight_bit: bool) -> Output {
        let format = out_format.or_else(|| (verbose > 0).then(|| "%n%L".to_string()));
        Output {
            out: Mutex::new(BufWriter::with_capacity(64 * 1024, std::io::stdout())),
            verbose,
            format,
            charset: Charset::for_output(eight_bit),
            forward: None,
        }
    }

    /// Output for a server process: nothing is written locally, every line goes to
    /// `forward` unescaped, so the client escapes it for its own terminal and charset.
    pub fn forwarding(verbose: u8, out_format: Option<String>, eight_bit: bool, forward: Forward) -> Output {
        Output { forward: Some(forward), ..Output::new(verbose, out_format, eight_bit) }
    }

    /// Print a line a server forwarded.
    pub fn print_forwarded(&self, stderr: bool, escape_it: bool, text: &[u8]) {
        let mut line = Vec::with_capacity(text.len() + 1);
        if escape_it {
            escape(text, self.charset, &mut line);
        } else {
            line.extend_from_slice(text);
        }
        line.push(b'\n');
        if stderr {
            self.flush();
            let _ = std::io::stderr().lock().write_all(&line);
        } else {
            let _ = self.out.lock().unwrap().write_all(&line);
        }
    }

    pub fn verbose(&self) -> u8 {
        self.verbose
    }

    fn line(&self, bytes: &[u8]) {
        if let Some(f) = &self.forward {
            return f(false, false, bytes);
        }
        let mut out = self.out.lock().unwrap();
        let _ = out.write_all(bytes);
        let _ = out.write_all(b"\n");
    }

    /// Write a line that may contain names. Like rsync, the whole line is escaped,
    /// `--out-format` text included.
    fn escaped_line(&self, text: &[u8]) {
        if let Some(f) = &self.forward {
            return f(false, true, text);
        }
        let mut line = Vec::with_capacity(text.len() + 1);
        escape(text, self.charset, &mut line);
        line.push(b'\n');
        let _ = self.out.lock().unwrap().write_all(&line);
    }

    /// Report a transferred/created item. `rel` is relative to the transfer root
    /// (empty for the root itself, shown as `./`).
    pub fn item(&self, rel: &[u8], kind: Kind, link_target: Option<&[u8]>) {
        let Some(fmt) = &self.format else { return };
        let mut name = if rel.is_empty() { b".".to_vec() } else { rel.to_vec() };
        if kind == Kind::Dir {
            name.push(b'/');
        }
        let mut line = Vec::with_capacity(name.len() + 16);
        let mut chars = fmt.as_bytes().iter().copied().peekable();
        while let Some(c) = chars.next() {
            if c != b'%' {
                line.push(c);
                continue;
            }
            match chars.next() {
                Some(b'n') => line.extend_from_slice(&name),
                Some(b'L') => {
                    if let Some(t) = link_target {
                        line.extend_from_slice(b" -> ");
                        line.extend_from_slice(t);
                    }
                }
                Some(b'%') => line.push(b'%'),
                Some(other) => {
                    line.push(b'%');
                    line.push(other);
                }
                None => line.push(b'%'),
            }
        }
        self.escaped_line(&line);
    }

    pub fn deleting(&self, rel: &[u8], is_dir: bool) {
        if self.verbose == 0 {
            return;
        }
        let mut line = b"deleting ".to_vec();
        line.extend_from_slice(rel);
        if is_dir {
            line.push(b'/');
        }
        self.escaped_line(&line);
    }

    /// Informational message shown with `-v`; may contain raw name bytes.
    pub fn info(&self, msg: impl AsRef<[u8]>) {
        if self.verbose > 0 {
            self.escaped_line(msg.as_ref());
        }
    }

    /// Always shown (unless redirected); used for summaries. Not escaped, so the
    /// message may contain newlines but must not contain names.
    pub fn always(&self, msg: impl AsRef<[u8]>) {
        self.line(msg.as_ref());
    }

    /// Error to stderr, escaped like the other lines. Build names into the message with
    /// [`path_msg`], not `Path::display()`, which turns invalid UTF-8 into U+FFFD.
    pub fn error(&self, msg: impl AsRef<[u8]>) {
        if let Some(f) = &self.forward {
            return f(true, true, &[b"fastrsync: ", msg.as_ref()].concat());
        }
        self.flush();
        let mut line = b"fastrsync: ".to_vec();
        escape(msg.as_ref(), self.charset, &mut line);
        line.push(b'\n');
        let _ = std::io::stderr().lock().write_all(&line);
    }

    pub fn flush(&self) {
        let _ = self.out.lock().unwrap().flush();
    }
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
/// LC_ALL=C would only make fastrsync's output differ from the local rsync's.
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
    pub deleted: AtomicU64,
    pub errors: AtomicU64,
}

/// Plain copy of [`Stats`] for sending over the wire.
#[derive(Debug, Clone, Default, serde::Serialize, serde::Deserialize)]
pub struct StatsSnapshot(pub [u64; 14]);

impl StatsSnapshot {
    pub fn errors(&self) -> u64 {
        self.0[13]
    }
}

impl Stats {
    fn fields(&self) -> [&AtomicU64; 14] {
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
        ]
    }

    pub fn snapshot(&self) -> StatsSnapshot {
        StatsSnapshot(self.fields().map(Self::get))
    }

    /// Add another process's counters into these.
    pub fn absorb(&self, s: &StatsSnapshot) {
        for (c, v) in self.fields().into_iter().zip(s.0) {
            Self::add(c, v);
        }
    }

    pub fn add(c: &AtomicU64, n: u64) {
        c.fetch_add(n, Relaxed);
    }

    pub fn get(c: &AtomicU64) -> u64 {
        c.load(Relaxed)
    }

    pub fn print(&self, out: &Output, elapsed: Duration, human: bool, full: bool) {
        let g = Self::get;
        let num = |n: u64| if human { human_num(n) } else { n.to_string() };
        let secs = elapsed.as_secs_f64().max(1e-9);
        if full {
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
    }
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

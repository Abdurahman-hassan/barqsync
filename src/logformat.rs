//! rsync's log-format escapes (`--out-format`), parsed once and expanded for each line
//! the way rsync's log.c `log_formatted` does.
//!
//! An escape is `%`, then optional `'`s, `-` and width digits (`'` may also follow the
//! digits), then a letter. `%%` is a `%`, as in rsync 3.5 (3.2 printed it as it is). As
//! in rsync, an escape that expands to nothing
//! outside a daemon (`%a`, `%h`, `%P`, `%u`), an unknown letter, and a `%` at the end stay
//! in the line as they are; `%m` (the daemon module) is empty. `%c` and `%C` (checksum
//! data, checksums) are refused: barqsync's protocol and checksums are not rsync's.
//!
//! [`Format::render`] is a plain function of the parsed format and a line's [`Fields`],
//! so other outputs (a log file) can reuse it.

use std::fmt;
use std::sync::Once;

/// A parsed `--out-format` string.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Format {
    parts: Vec<Part>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum Part {
    Text(Vec<u8>),
    Esc(u8, Spec),
}

/// An escape's modifiers, which rsync turns into a `printf` conversion.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
struct Spec {
    /// A `-` or width digits were given (rsync's `fmt[1]`): `%L` pads even without a link.
    mods: bool,
    left: bool,
    /// The width digits start with `0`.
    zero: bool,
    width: usize,
    /// The number of `'`: thousands separators (1), units of 1000 (2) or 1024 (3+).
    human: u8,
}

/// What the escapes of one line show (rsync's file entry and the logging side's state).
#[derive(Debug, Clone, Copy, Default)]
pub struct Fields<'a> {
    /// `%o`: `send`, `recv` or `del.`.
    pub op: &'a [u8],
    /// `%n`: the name, ending in `/` for a directory.
    pub name: &'a [u8],
    /// `%f`: the long name (the sender's path; a receiver's name without the `/`).
    pub fname: &'a [u8],
    /// `%L`: ` -> ` or ` => ` and the name that follows it.
    pub link: Option<(&'a [u8], &'a [u8])>,
    /// `%i`: the change summary, `*deleting  ` for a deletion.
    pub changes: &'a [u8],
    /// `%l`
    pub size: u64,
    /// `%b`
    pub bytes: u64,
    /// `%M`
    pub mtime: i64,
    /// `%B`: the permission bits (with setuid, setgid and sticky).
    pub mode: u32,
    /// `%U`
    pub uid: u32,
    /// `%G`; `None` shows `DEFAULT`.
    pub gid: Option<u32>,
}

/// Why a format is refused.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Unsupported(pub char);

impl fmt::Display for Unsupported {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        let what = if self.0 == 'c' { "the size of the block checksums" } else { "the file checksum" };
        write!(f, "--out-format escape %{} ({what}) is not supported", self.0)
    }
}

impl std::error::Error for Unsupported {}

/// Letters that expand outside a daemon (`h`, `a`, `P` and `u` don't, and stay as they are).
const EXPANDS: &[u8] = b"lUGpMBofnLmtbi%";

/// glibc ignores the `0` flag of `%s` (spaces), the BSD libc of macOS pads with zeros:
/// rsync's string escapes (`%08l`) pad as the libc of the platform does.
const ZERO_PADS_STRINGS: bool = !cfg!(target_os = "linux");

impl Format {
    pub fn parse(format: &str) -> Result<Format, Unsupported> {
        let b = format.as_bytes();
        let mut parts = Vec::new();
        let mut text = Vec::new();
        let mut i = 0;
        while i < b.len() {
            if b[i] != b'%' {
                text.push(b[i]);
                i += 1;
                continue;
            }
            let (spec, p) = modifiers(b, i + 1);
            match b.get(p) {
                Some(&c @ (b'c' | b'C')) => return Err(Unsupported(c as char)),
                Some(&c) if EXPANDS.contains(&c) => {
                    if !text.is_empty() {
                        parts.push(Part::Text(std::mem::take(&mut text)));
                    }
                    parts.push(Part::Esc(c, spec));
                    i = p + 1;
                }
                // Left as it is; the scan goes on from the letter.
                _ => {
                    text.extend_from_slice(&b[i..p]);
                    i = p;
                }
            }
        }
        if !text.is_empty() {
            parts.push(Part::Text(text));
        }
        Ok(Format { parts })
    }

    /// Whether the format has the escape `letter` (rsync's `log_format_has`).
    pub fn has(&self, letter: u8) -> bool {
        self.parts.iter().any(|p| matches!(p, Part::Esc(c, _) if *c == letter))
    }

    /// The line for `f`, without a newline. Not escaped: the caller escapes the line for
    /// the terminal, as rsync does.
    pub fn render(&self, f: &Fields) -> Vec<u8> {
        let mut out = Vec::with_capacity(f.name.len() + 32);
        for part in &self.parts {
            match part {
                Part::Text(t) => out.extend_from_slice(t),
                Part::Esc(c, spec) => expand(*c, spec, f, &mut out),
            }
        }
        out
    }
}

/// Parse the modifiers that start at `p` (after the `%`): the spec and where the letter is.
fn modifiers(b: &[u8], mut p: usize) -> (Spec, usize) {
    let mut human = 0u8;
    while b.get(p) == Some(&b'\'') {
        human = human.saturating_add(1);
        p += 1;
    }
    let left = b.get(p) == Some(&b'-');
    if left {
        p += 1;
    }
    // rsync copies `-` and the digits into a 32-byte `fmt` after its `%`, keeping 8
    // bytes free: the digits that don't fit are the escape's letter.
    let start = p;
    while b.get(p).is_some_and(u8::is_ascii_digit) && 1 + usize::from(left) + (p - start) < 24 {
        p += 1;
    }
    let digits = &b[start..p];
    while b.get(p) == Some(&b'\'') {
        human = human.saturating_add(1);
        p += 1;
    }
    let width = digits.iter().fold(0usize, |w, d| (w * 10 + usize::from(d - b'0')).min(4096));
    let spec = Spec { mods: left || !digits.is_empty(), left, zero: digits.first() == Some(&b'0'), width, human };
    (spec, p)
}

fn expand(c: u8, s: &Spec, f: &Fields, out: &mut Vec<u8>) {
    match c {
        b'l' => pad(s, &big_num(f.size, s.human), false, out),
        b'b' => pad(s, &big_num(f.bytes, s.human), false, out),
        b'U' => pad(s, f.uid.to_string().as_bytes(), true, out),
        b'G' => match f.gid {
            Some(g) => pad(s, g.to_string().as_bytes(), true, out),
            None => pad(s, b"DEFAULT", false, out),
        },
        b'p' => pad(s, std::process::id().to_string().as_bytes(), true, out),
        b'M' => pad(s, &timestring(f.mtime, true), false, out),
        b't' => {
            let now = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map_or(0, |d| d.as_secs() as i64);
            pad(s, &timestring(now, false), false, out)
        }
        b'B' => pad(s, &permstring(f.mode), false, out),
        b'o' => pad(s, f.op, false, out),
        b'f' => pad(s, f.fname, false, out),
        b'n' => pad(s, f.name, false, out),
        b'L' => match f.link {
            Some((arrow, target)) => {
                out.extend_from_slice(arrow);
                pad(s, target, false, out);
            }
            // With modifiers, rsync pads an empty name after four spaces.
            None if s.mods => {
                out.extend_from_slice(b"    ");
                pad(s, b"", false, out);
            }
            None => {}
        },
        // The module name, which is empty outside a daemon.
        b'm' => pad(s, b"", false, out),
        b'i' => pad(s, f.changes, false, out),
        _ => pad(s, b"%", false, out),
    }
}

/// `printf` with the escape's flags and width: `%[-][0]N` of a string or a number.
fn pad(s: &Spec, text: &[u8], numeric: bool, out: &mut Vec<u8>) {
    let fill = s.width.saturating_sub(text.len());
    if fill == 0 {
        return out.extend_from_slice(text);
    }
    if s.left {
        out.extend_from_slice(text);
        out.resize(out.len() + fill, b' ');
        return;
    }
    let ch = if s.zero && (numeric || ZERO_PADS_STRINGS) { b'0' } else { b' ' };
    out.resize(out.len() + fill, ch);
    out.extend_from_slice(text);
}

/// rsync's `do_big_num`: digits; with `human` 1, thousands separated by `,`; with 2 or
/// more, from 1000 on, two decimals and a unit (K, M, G, T, P) of 1000 or (3+) 1024.
pub fn big_num(n: u64, human: u8) -> Vec<u8> {
    if human > 1 {
        let mult = if human == 2 { 1000.0 } else { 1024.0 };
        if n as f64 >= mult {
            let mut d = n as f64 / mult;
            let mut unit = 'K';
            for u in ['M', 'G', 'T', 'P'] {
                if d < mult {
                    break;
                }
                d /= mult;
                unit = u;
            }
            return format!("{d:.2}{unit}").into_bytes();
        }
    }
    let digits = n.to_string().into_bytes();
    if human == 0 {
        return digits;
    }
    let mut out = Vec::with_capacity(digits.len() + digits.len() / 3);
    for (i, &c) in digits.iter().enumerate() {
        if i > 0 && (digits.len() - i).is_multiple_of(3) {
            out.push(b',');
        }
        out.push(c);
    }
    out
}

/// rsync's `timestring`: local time as `YYYY/MM/DD hh:mm:ss`; `%M` (`dashes`) joins the
/// two halves with a `-`.
pub fn timestring(t: i64, dashes: bool) -> Vec<u8> {
    unsafe extern "C" {
        fn tzset();
    }
    static TZ: Once = Once::new();
    // SAFETY: `tzset` runs once, before any `localtime_r`; `tm` is ours.
    TZ.call_once(|| unsafe { tzset() });
    let mut tm: libc::tm = unsafe { std::mem::zeroed() };
    let ok = libc::time_t::try_from(t).is_ok_and(|t| unsafe { !libc::localtime_r(&t, &mut tm).is_null() });
    let s = if ok {
        format!(
            "{:4}/{:02}/{:02} {:02}:{:02}:{:02}",
            i64::from(tm.tm_year) + 1900,
            tm.tm_mon + 1,
            tm.tm_mday,
            tm.tm_hour,
            tm.tm_min,
            tm.tm_sec
        )
    } else {
        "(time out of range)".to_string()
    };
    if dashes { s.replace(' ', "-").into_bytes() } else { s.into_bytes() }
}

/// rsync's `permstring` without the type letter: `rwxr-xr-x`, `s`/`S` for setuid and
/// setgid, `t`/`T` for sticky.
pub fn permstring(mode: u32) -> [u8; 9] {
    let mut p = *b"---------";
    for (i, &c) in b"rwxrwxrwx".iter().enumerate() {
        if mode & (0o400 >> i) != 0 {
            p[i] = c;
        }
    }
    let special = |set: u32, exec: u32, on: u8, off: u8| if mode & set == 0 { None } else { Some(if mode & exec != 0 { on } else { off }) };
    if let Some(c) = special(0o4000, 0o100, b's', b'S') {
        p[2] = c;
    }
    if let Some(c) = special(0o2000, 0o010, b's', b'S') {
        p[5] = c;
    }
    if let Some(c) = special(0o1000, 0o001, b't', b'T') {
        p[8] = c;
    }
    p
}

#[cfg(test)]
mod tests {
    use super::*;

    fn line(format: &str, f: &Fields) -> String {
        String::from_utf8(Format::parse(format).unwrap().render(f)).unwrap()
    }

    fn file() -> Fields<'static> {
        Fields {
            op: b"send",
            name: b"sub/b",
            fname: b"src/sub/b",
            changes: b">f+++++++++",
            size: 1_234_000,
            bytes: 46,
            mode: 0o640,
            uid: 501,
            gid: None,
            ..Fields::default()
        }
    }

    /// Expected output measured with rsync 3.5.1 (macOS) and 3.2.7 (Linux).
    #[test]
    fn escapes_expand_like_rsync() {
        let f = file();
        assert_eq!(line("%o|%f|%n|%l|%b|%B|%U|%G|%L|%i", &f), "send|src/sub/b|sub/b|1234000|46|rw-r-----|501|DEFAULT||>f+++++++++");
        // Outside a daemon these stay as they are; %m is empty; unknown letters and a
        // trailing % are kept; %% is one %.
        assert_eq!(line("%a|%h|%P|%u|%m|%x|%%|%", &f), "%a|%h|%P|%u||%x|%|%");
        assert_eq!(line("%-5x%n %%n %5%", &f), "%-5xsub/b %n     %");
        // Widths, left alignment and digit grouping.
        assert_eq!(line("[%-10n][%10n][%12l][%-8L][%8B]", &f), "[sub/b     ][     sub/b][     1234000][            ][rw-r-----]");
        assert_eq!(line("[%'l][%''l][%'''l][%'b][%''5l]", &f), "[1,234,000][1.23M][1.18M][46][1.23M]");
        // A `'` after the letter is text.
        assert_eq!(line("[%l']", &f), "[1234000']");
        let link = Fields { link: Some((b" -> ", b"a")), ..f };
        assert_eq!(line("%n%L|%-6L|%L", &link), "sub/b -> a| -> a     | -> a");
        let numbers = Fields { gid: Some(20), ..f };
        assert_eq!(line("[%05U][%-5G][%05G]", &numbers), "[00501][20   ][00020]");
        let zeros = if ZERO_PADS_STRINGS { "[00000046]" } else { "[      46]" };
        assert_eq!(line("[%08b]", &f), zeros);
        assert_eq!(line("[%-08b]", &f), "[46      ]");
        let del = Fields { changes: b"*deleting  ", ..Fields::default() };
        assert_eq!(line("%i|", &del), "*deleting  |");
    }

    #[test]
    fn checksum_escapes_are_refused() {
        assert_eq!(Format::parse("%n %C"), Err(Unsupported('C')));
        assert_eq!(Format::parse("%-5c"), Err(Unsupported('c')));
        assert!(Format::parse("%%C %Q").is_ok(), "a literal %, then C");
    }

    #[test]
    fn has_finds_escapes_as_rsync_does() {
        let f = Format::parse("%%i %'-9b %o").unwrap();
        assert!(!f.has(b'i') && f.has(b'b') && f.has(b'o') && !f.has(b'n'));
        // Too many width digits: the 24th is the escape's letter, as in rsync.
        let wide = format!("%{}n", "1".repeat(24));
        let f = Format::parse(&wide).unwrap();
        assert!(!f.has(b'n'));
    }

    #[test]
    fn numbers_permissions_and_times() {
        assert_eq!(big_num(999, 1), b"999");
        assert_eq!(big_num(1000, 1), b"1,000");
        assert_eq!(big_num(999, 2), b"999");
        assert_eq!(big_num(1000, 2), b"1.00K");
        assert_eq!(big_num(5 << 30, 3), b"5.00G");
        assert_eq!(&permstring(0o4755), b"rwsr-xr-x");
        assert_eq!(&permstring(0o2644), b"rw-r-Sr--");
        assert_eq!(&permstring(0o1777), b"rwxrwxrwt");
        assert_eq!(&permstring(0o1770), b"rwxrwx--T");
        let t = String::from_utf8(timestring(1_600_000_000, true)).unwrap();
        assert!(t.len() == 19 && !t.contains(' ') && t.as_bytes()[10] == b'-', "{t}");
        assert_eq!(timestring(0, false).len(), 19);
    }
}

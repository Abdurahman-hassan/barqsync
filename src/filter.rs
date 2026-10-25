//! rsync's filter rules: `--filter`/`-f`, `--include`/`--exclude` and their `-from`
//! files, `-F` and `-C`, following rsync's exclude.c and wildmatch.c.
//!
//! - Rules are checked in order and the first match wins; no match means "included".
//! - A rule applies on the sending side (what is sent), the receiving side (what
//!   `--delete` may remove), or both: the default, which `--delete-excluded` turns into
//!   the sending side alone for plain `+`/`-` rules.
//! - A trailing `/` only matches directories; a leading `/` anchors the pattern at the
//!   transfer root (at its file's directory, for a rule read from a per-directory file);
//!   a pattern containing `/` or `**` is matched against the path, otherwise against the
//!   final component; `*` and `?` stop at `/`, `**` crosses it, and `dir/***` matches
//!   `dir` and everything below.
//! - `. FILE` merges FILE's rules in its place, read where the command line is parsed;
//!   `: FILE` reads FILE in each directory of the transfer, whose rules apply there and,
//!   unless `n`, below. `: /NAME` (`-F`) also reads NAME in the directories above the
//!   first transfer directory.

use anyhow::{Result, bail};
use std::ffi::OsStr;
use std::io::Read;
use std::os::unix::ffi::OsStrExt;
use std::path::{Component, Path, PathBuf};
use std::sync::{Arc, OnceLock};

/// rsync's `--cvs-exclude` defaults (its `DEFAULT_CVSIGNORE`).
const DEFAULT_CVSIGNORE: &[u8] = b"RCS SCCS CVS CVS.adm RCSLOG cvslog.* tags TAGS .make.state .nse_depinfo \
    *~ #* .#* ,* _$* *$ *.old *.bak *.BAK *.orig *.rej .del-* *.a *.olb *.o *.obj *.so *.exe *.Z *.elc *.ln \
    core .svn/ .git/ .hg/ .bzr/";

/// Merge files may name further merge files this deep (rsync's limit).
const MAX_MERGE_DEPTH: usize = 32;

/// Rule modifiers: rsync's `! / p s r`.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[derive(rkyv::Archive, rkyv::Serialize, rkyv::Deserialize)]
pub struct Mods {
    /// `!`: the rule is for the names its pattern does not match.
    pub negate: bool,
    /// `/`: the pattern is matched against the absolute path.
    pub abs: bool,
    /// `p`: ignored for what is inside a directory being deleted.
    pub perishable: bool,
    /// `s` (or `hide`/`show`): applies on the sending side.
    pub sender: bool,
    /// `r` (or `protect`/`risk`): applies on the receiving side. Neither: both sides.
    pub receiver: bool,
}

/// How a merge file is read: rsync's `- + w n C` merge modifiers.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[derive(rkyv::Archive, rkyv::Serialize, rkyv::Deserialize)]
pub struct MergeMods {
    /// `-`/`+`: each line is an exclude (`false`) or include (`true`) pattern.
    pub only: Option<bool>,
    /// `w`: rules are separated by whitespace, not lines (and there are no comments).
    pub words: bool,
    /// `n`: the rules don't apply in subdirectories.
    pub no_inherit: bool,
    /// `C`: `.cvsignore` style: words, excludes only, not inherited, `!` clears.
    pub cvs: bool,
}

/// One rule as given on the command line or read from a `.` merge file. The client sends
/// these to the server.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[derive(rkyv::Archive, rkyv::Serialize, rkyv::Deserialize)]
pub enum RuleSpec {
    /// An include (`+`) or exclude (`-`) pattern.
    Pattern { include: bool, pattern: Vec<u8>, mods: Mods },
    /// `:` (dir-merge): the rules in the file `name` of each directory.
    DirMerge { name: Vec<u8>, merge: MergeMods, mods: Mods },
    /// `-C` as a rule: the CVS excludes (rsync's defaults, then `~/.cvsignore` and
    /// `$CVSIGNORE` of the side checking them).
    Cvs { mods: Mods },
}

impl RuleSpec {
    /// A plain exclude rule.
    pub fn exclude(pattern: impl Into<Vec<u8>>) -> RuleSpec {
        RuleSpec::Pattern { include: false, pattern: pattern.into(), mods: Mods::default() }
    }
}

// ---------------------------------------------------------------------------
// Parsing (rsync's parse_rule_tok, parse_filter_str and parse_filter_file)
// ---------------------------------------------------------------------------

/// What the rules being read default to: rsync's rule template and parse flags.
#[derive(Debug, Clone, Copy, Default)]
struct Template {
    /// Modifiers every rule gets (`/`, `p`, and the sides of a side-specific merge file).
    mods: Mods,
    /// Rules without a prefix are includes (`--include`, a `+` merge file).
    include: bool,
    /// No prefixes at all: each line or word is a pattern (`-`, `+` and `C` merge files).
    no_prefixes: bool,
    /// Rules are separated by whitespace.
    words: bool,
    /// A `.cvsignore`-style list, where `!` clears.
    cvs: bool,
    /// Old-style `--include`/`--exclude` patterns: only `+ ` and `- ` prefixes (and `!`).
    old_prefixes: bool,
}

impl Template {
    /// How the rules of a merge file with these modifiers are read.
    fn merge(merge: MergeMods, mods: Mods) -> Template {
        Template {
            mods: Mods { negate: false, ..mods },
            include: merge.only == Some(true),
            no_prefixes: merge.only.is_some(),
            words: merge.words,
            cvs: merge.cvs,
            old_prefixes: false,
        }
    }
}

/// One parsed rule, borrowing its pattern from the text.
#[derive(Debug)]
enum Tok<'a> {
    /// `!`: clear the list.
    Clear,
    Pattern { include: bool, pattern: &'a [u8], mods: Mods },
    /// `.` (or, with `per_dir`, `:`) and the file's name.
    Merge { name: &'a [u8], per_dir: bool, merge: MergeMods, mods: Mods, exclude_self: bool },
    Cvs { mods: Mods },
}

/// C's `isspace` (Rust's ASCII whitespace lacks the vertical tab).
fn is_space(c: u8) -> bool {
    matches!(c, b' ' | b'\t' | b'\n' | b'\x0b' | b'\x0c' | b'\r')
}

fn shown(text: &[u8]) -> String {
    String::from_utf8_lossy(text).into_owned()
}

/// A long rule name at the start of `s`: the index the modifiers start after (the
/// name's last character, or the `,` that follows it).
fn rule_word(s: &[u8], word: &[u8]) -> Option<usize> {
    if !s.starts_with(word) {
        return None;
    }
    match s.get(word.len()) {
        None | Some(b'_') => Some(word.len() - 1),
        Some(&c) if is_space(c) => Some(word.len() - 1),
        Some(b',') => Some(word.len()),
        _ => None,
    }
}

/// The next rule in `s` (advanced past it), read with `tpl`; `None` at the end.
fn next_tok<'a>(s: &mut &'a [u8], tpl: &Template) -> Result<Option<Tok<'a>>> {
    if tpl.words {
        let n = s.iter().take_while(|&&c| is_space(c)).count();
        *s = &s[n..];
    }
    let text = *s;
    if text.is_empty() {
        return Ok(None);
    }
    let mut mods = Mods { abs: tpl.mods.abs, perishable: tpl.mods.perishable, ..Mods::default() };
    let mut include = tpl.include;
    let mut clear = false;
    let (mut merge, mut per_dir, mut exclude_self) = (false, false, false);
    let mut mm = MergeMods::default();
    let mut cvs = false;
    let mut start = 0;
    if tpl.no_prefixes {
        clear = text[0] == b'!' && tpl.cvs;
    } else if tpl.old_prefixes {
        if text.starts_with(b"- ") {
            include = false;
            start = 2;
        } else if text.starts_with(b"+ ") {
            include = true;
            start = 2;
        } else {
            clear = text[0] == b'!';
        }
    } else {
        let long = |word: &[u8], ch: u8| rule_word(text, word).map(|i| (ch, i));
        let named = match text[0] {
            b'c' => long(b"clear", b'!'),
            b'd' => long(b"dir-merge", b':'),
            b'e' => long(b"exclude", b'-'),
            b'h' => long(b"hide", b'H'),
            b'i' => long(b"include", b'+'),
            b'm' => long(b"merge", b'.'),
            b'p' => long(b"protect", b'P'),
            b'r' => long(b"risk", b'R'),
            b's' => long(b"show", b'S'),
            c => Some((c, usize::from(text.get(1) == Some(&b',')))),
        };
        let unknown = || anyhow::anyhow!("Unknown filter rule: `{}'", shown(text));
        let (ch, mut i) = named.ok_or_else(unknown)?;
        let mut side_by_prefix = false;
        match ch {
            b':' => (merge, per_dir) = (true, true),
            b'.' => merge = true,
            b'+' => include = true,
            b'-' => include = false,
            b'S' | b'H' => {
                include = ch == b'S';
                mods.sender = true;
                side_by_prefix = true;
            }
            b'R' | b'P' => {
                include = ch == b'R';
                mods.receiver = true;
                side_by_prefix = true;
            }
            b'!' => clear = true,
            _ => return Err(unknown()),
        }
        let mut no_prefixes = false;
        // Modifiers, up to the space or `_` before the pattern (`!` takes none).
        if ch != b'!' {
            loop {
                i += 1;
                let Some(&c) = text.get(i) else { break };
                if c == b' ' || c == b'_' {
                    break;
                }
                if tpl.words && is_space(c) {
                    i -= 1;
                    break;
                }
                let ok = match c {
                    b'-' | b'+' if merge && !no_prefixes => {
                        no_prefixes = true;
                        mm.only = Some(c == b'+');
                        true
                    }
                    b'/' => {
                        mods.abs = true;
                        true
                    }
                    b'!' if !merge => {
                        mods.negate = true;
                        true
                    }
                    b'C' if !no_prefixes && !side_by_prefix => {
                        no_prefixes = true;
                        (mm.only, mm.words, mm.no_inherit, mm.cvs) = (Some(false), true, true, true);
                        cvs = true;
                        true
                    }
                    b'e' if merge => {
                        exclude_self = true;
                        true
                    }
                    b'n' if merge => {
                        mm.no_inherit = true;
                        true
                    }
                    b'w' if merge => {
                        mm.words = true;
                        true
                    }
                    b'p' => {
                        mods.perishable = true;
                        true
                    }
                    b'r' | b's' if !side_by_prefix => {
                        if c == b'r' {
                            mods.receiver = true;
                        } else {
                            mods.sender = true;
                        }
                        true
                    }
                    b'x' => bail!("xattr filter rules (the `x' modifier) are not supported: {}", shown(text)),
                    _ => false,
                };
                if !ok {
                    bail!("invalid modifier '{}' at position {i} in filter rule: {}", c as char, shown(text));
                }
            }
        }
        start = (i + 1).min(text.len());
    }
    if tpl.mods.sender || tpl.mods.receiver {
        if mods.sender || mods.receiver {
            bail!("specified-side merge file contains specified-side filter: {}", shown(text));
        }
        mods.sender = tpl.mods.sender;
        mods.receiver = tpl.mods.receiver;
    }
    let rest = &text[start..];
    let len = if tpl.words { rest.iter().position(|&c| is_space(c)).unwrap_or(rest.len()) } else { rest.len() };
    let pattern = &rest[..len];
    *s = &rest[len..];
    if clear {
        if !tpl.no_prefixes && !tpl.old_prefixes && len > 0 {
            bail!("'!' rule has trailing characters: {}", shown(text));
        }
        // `!` alone clears; anything longer (where prefixes don't apply) is a pattern.
        let pattern = if tpl.no_prefixes || tpl.old_prefixes { pattern } else { &[][..] };
        if pattern.len() <= 1 {
            return Ok(Some(Tok::Clear));
        }
        return Ok(Some(Tok::Pattern { include, pattern, mods }));
    }
    if len == 0 && !cvs {
        bail!("unexpected end of filter rule: {}", shown(text));
    }
    if merge {
        return Ok(Some(Tok::Merge { name: pattern, per_dir, merge: mm, mods, exclude_self }));
    }
    if cvs {
        return Ok(Some(Tok::Cvs { mods }));
    }
    Ok(Some(Tok::Pattern { include, pattern, mods }))
}

/// The rules of a file: its lines (NUL-separated with `--from0`) without empty lines and
/// `#`/`;` comments, or with `words` its whitespace-separated words.
fn split_rules(data: &[u8], words: bool, from0: bool) -> Vec<&[u8]> {
    let mut out = Vec::new();
    let mut start = 0;
    let mut i = 0;
    while i <= data.len() {
        let end = i == data.len();
        let c = data.get(i).copied().unwrap_or(0);
        let sep = end || if from0 { c == 0 } else { c == b'\n' || c == b'\r' } || (words && is_space(c));
        if sep {
            let tok = &data[start..i];
            if !tok.is_empty() && (words || !matches!(tok[0], b';' | b'#')) {
                out.push(tok);
            }
            // CR LF is one line end.
            if c == b'\r' && data.get(i + 1) == Some(&b'\n') {
                i += 1;
            }
            start = i + 1;
        }
        i += 1;
    }
    out
}

/// A rules file an option names (`--exclude-from`, a `.` merge rule) could not be read:
/// rsync's file I/O error (exit 11), not a usage error.
#[derive(Debug)]
pub struct RulesFileError(String);

impl std::fmt::Display for RulesFileError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

impl std::error::Error for RulesFileError {}

fn basename(name: &[u8]) -> &[u8] {
    name.rsplit(|&c| c == b'/').next().unwrap_or(name)
}

/// Reads command-line filter options into the ordered rule list, `.` merge files
/// included (rsync's client side).
#[derive(Debug, Default)]
pub struct RuleParser {
    pub rules: Vec<RuleSpec>,
    /// `--from0`: lines of files are separated by NUL bytes.
    pub from0: bool,
    depth: usize,
}

impl RuleParser {
    pub fn new(from0: bool) -> RuleParser {
        RuleParser { rules: Vec::new(), from0, depth: 0 }
    }

    /// `--filter=RULE`.
    pub fn filter(&mut self, rule: &[u8]) -> Result<()> {
        self.parse(rule, &Template::default())
    }

    /// `--exclude=PATTERN` / `--include=PATTERN` (old-style: `+ `/`- ` prefixes only).
    pub fn pattern(&mut self, pattern: &[u8], include: bool) -> Result<()> {
        self.parse(pattern, &Template { include, old_prefixes: true, ..Template::default() })
    }

    /// `--exclude-from=FILE` / `--include-from=FILE` (`-`: stdin).
    pub fn file(&mut self, file: &Path, include: bool) -> Result<()> {
        let what = if include { "include" } else { "exclude" };
        self.merge_file(file.as_os_str().as_bytes(), &Template { include, old_prefixes: true, ..Template::default() }, what)
    }

    fn parse(&mut self, text: &[u8], tpl: &Template) -> Result<()> {
        let mut s = text;
        while let Some(tok) = next_tok(&mut s, tpl)? {
            match tok {
                Tok::Clear => self.rules.clear(),
                Tok::Pattern { include, pattern, mods } => {
                    self.rules.push(RuleSpec::Pattern { include, pattern: pattern.to_vec(), mods })
                }
                Tok::Cvs { mods } => self.rules.push(RuleSpec::Cvs { mods }),
                Tok::Merge { name, per_dir, merge, mods, exclude_self } => {
                    let name = if name.is_empty() { &b".cvsignore"[..] } else { name };
                    if exclude_self {
                        self.rules.push(RuleSpec::exclude(basename(name)));
                    }
                    if !per_dir {
                        let what = if merge.only == Some(true) { "include" } else { "exclude" };
                        self.merge_file(name, &Template::merge(merge, mods), what)?;
                    } else if !self.rules.iter().any(|r| matches!(r, RuleSpec::DirMerge { name: n, .. } if basename(n) == basename(name))) {
                        // A per-directory file named again is not read twice.
                        self.rules.push(RuleSpec::DirMerge { name: name.to_vec(), merge, mods });
                    }
                }
            }
        }
        Ok(())
    }

    fn merge_file(&mut self, name: &[u8], tpl: &Template, what: &str) -> Result<()> {
        if self.depth >= MAX_MERGE_DEPTH {
            bail!("merge-file include depth limit ({MAX_MERGE_DEPTH}) exceeded at {}", shown(name));
        }
        let read = if name == b"-" {
            let mut v = Vec::new();
            std::io::stdin().read_to_end(&mut v).map(|_| v)
        } else {
            std::fs::read(Path::new(OsStr::from_bytes(name)))
        };
        let data = read.map_err(|e| RulesFileError(format!("failed to open {what} file {}: {e}", shown(name))))?;
        self.depth += 1;
        let r = split_rules(&data, tpl.words, self.from0).into_iter().try_for_each(|line| self.parse(line, tpl));
        self.depth -= 1;
        r
    }
}

// ---------------------------------------------------------------------------
// Compiled rules and matching (rsync's add_rule and rule_matches)
// ---------------------------------------------------------------------------

/// Where a leading `/` anchors a rule read from a per-directory file: rsync turns such a
/// rule into an absolute-path rule for its file's directory.
#[derive(Debug, Clone, Copy)]
enum Anchor<'a> {
    /// A global rule: at the transfer root.
    Root,
    /// A file in this directory of the transfer (relative to its root).
    Dir(&'a [u8]),
    /// A file above the transfer, in this absolute directory.
    Abs(&'a [u8]),
}

#[derive(Debug, Clone)]
struct Rule {
    include: bool,
    negate: bool,
    perishable: bool,
    sender: bool,
    receiver: bool,
    abs: bool,
    /// The pattern ended with `/`: it only matches directories.
    dir_only: bool,
    /// Without the trailing `/`; a leading `/` anchors it.
    pattern: Vec<u8>,
    slash_cnt: usize,
    wild: bool,
    /// Contains `**`.
    wild2: bool,
    wild2_prefix: bool,
    /// Ends with `***`.
    wild3_suffix: bool,
}

impl Rule {
    fn new(include: bool, pat: &[u8], mods: Mods, anchor: Anchor, delete_excluded: bool) -> Rule {
        let mut pattern = pat.to_vec();
        let mut dir_only = false;
        if pattern.len() > 1 && pattern.last() == Some(&b'/') {
            pattern.pop();
            dir_only = true;
        }
        let mut abs = mods.abs;
        if !abs && pattern.first() == Some(&b'/') {
            let prefix = match anchor {
                Anchor::Root => None,
                Anchor::Dir(rel) => Some(rel),
                Anchor::Abs(dir) => {
                    abs = true;
                    Some(dir)
                }
            };
            if let Some(prefix) = prefix.map(|p| p.strip_suffix(b"/").unwrap_or(p)).filter(|p| !p.is_empty() && *p != b"/") {
                let mut p = Vec::with_capacity(prefix.len() + pattern.len() + 1);
                if prefix[0] != b'/' {
                    p.push(b'/');
                }
                p.extend_from_slice(prefix);
                p.extend_from_slice(&pattern);
                pattern = p;
            }
        }
        let has = |needle: &[u8]| pattern.windows(needle.len()).any(|w| w == needle);
        let wild = pattern.iter().any(|c| matches!(c, b'*' | b'[' | b'?'));
        let wild2 = wild && has(b"**");
        // Without a side, both; `--delete-excluded` makes that the sending side.
        let (sender, receiver) = match (mods.sender, mods.receiver) {
            (false, false) => (true, !delete_excluded),
            sides => sides,
        };
        Rule {
            include,
            negate: mods.negate,
            perishable: mods.perishable,
            sender,
            receiver,
            abs,
            dir_only,
            slash_cnt: pattern.iter().filter(|&&c| c == b'/').count(),
            wild,
            wild2,
            wild2_prefix: wild2 && pattern.starts_with(b"**"),
            wild3_suffix: wild2 && pattern.ends_with(b"***"),
            pattern,
        }
    }

    fn applies(&self, side: Side) -> bool {
        match side {
            Side::Sender => self.sender,
            Side::Receiver => self.receiver,
            Side::Deleting => self.receiver && !self.perishable,
        }
    }

    /// rsync's `rule_matches`. `fname`: the path relative to the transfer root;
    /// `abs_root`: the transfer root's absolute path, for `/` rules.
    fn matches(&self, fname: &[u8], is_dir: bool, abs_root: Option<&[u8]>) -> bool {
        let hit = !self.negate;
        if fname.is_empty() {
            return false;
        }
        let mut name = fname;
        // What rsync puts before the name: the absolute root and a `/`, or just a `/`.
        let mut lead: [&[u8]; 2] = [b"", b""];
        if self.slash_cnt == 0 && !self.wild2 {
            // No `/` and no `**`: only the final component counts.
            if let Some(p) = name.iter().rposition(|&c| c == b'/') {
                name = &name[p + 1..];
            }
        } else if let Some(root) = abs_root.filter(|r| self.abs && r.len() > 1) {
            lead = [root.strip_prefix(b"/").unwrap_or(root), b"/"];
        } else if self.wild2_prefix {
            // Lets `**/` match at the start of the path.
            lead = [b"", b"/"];
        }
        let trail: &[u8] = if is_dir && self.wild3_suffix { b"/" } else { b"" };
        if !is_dir && self.dir_only {
            return !hit;
        }
        let joined = !lead[1].is_empty() || !trail.is_empty();
        let buf: Vec<u8>;
        let text: &[u8] = if joined {
            buf = [lead[0], lead[1], name, trail].concat();
            &buf
        } else {
            name
        };
        let (pattern, anchored) = match self.pattern.strip_prefix(b"/") {
            Some(p) => (p, true),
            None => (&self.pattern[..], false),
        };
        let where_: isize = if !anchored && self.slash_cnt > 0 && !self.wild2 {
            // An infix `/` without `**`: the last slash_cnt+1 path elements.
            self.slash_cnt as isize + 1
        } else if !anchored && !self.wild2_prefix && self.wild2 {
            // An infix or trailing `**`: after any `/`.
            -1
        } else {
            0
        };
        let matched = if self.wild {
            wildmatch_array(pattern, text, where_)
        } else if joined {
            litmatch_array(pattern, text, where_)
        } else if anchored {
            name == pattern
        } else {
            let (l1, l2) = (name.len(), pattern.len());
            l2 <= l1 && name.ends_with(pattern) && (l1 == l2 || name[l1 - l2 - 1] == b'/')
        };
        if matched { hit } else { !hit }
    }
}

const FALSE: i8 = 0;
const TRUE: i8 = 1;
const ABORT_ALL: i8 = -1;
const ABORT_TO_STARSTAR: i8 = -2;

/// rsync's `dowild`: `?` and `*` never match `/`, `**` matches anything, `[...]` is a
/// character class (`!`/`^` negates, ranges, `[:alpha:]`-style names), `\` escapes the
/// next character. The aborts let callers stop trying further positions.
fn dowild(p: &[u8], t: &[u8]) -> i8 {
    let (mut pi, mut ti) = (0, 0);
    while pi < p.len() {
        let p_ch = p[pi];
        if ti >= t.len() && p_ch != b'*' {
            return ABORT_ALL;
        }
        let t_ch = t.get(ti).copied().unwrap_or(0);
        match p_ch {
            b'\\' => {
                // A trailing backslash compares with the end of the pattern.
                if t_ch != p.get(pi + 1).copied().unwrap_or(0) {
                    return FALSE;
                }
                pi += 2;
            }
            b'?' => {
                if t_ch == b'/' {
                    return FALSE;
                }
                pi += 1;
            }
            b'*' => {
                pi += 1;
                let special = p.get(pi) == Some(&b'*');
                while p.get(pi) == Some(&b'*') {
                    pi += 1;
                }
                if pi >= p.len() {
                    // A trailing `**` matches everything, a trailing `*` anything without a slash.
                    return if special || !t[ti..].contains(&b'/') { TRUE } else { FALSE };
                }
                while ti < t.len() {
                    let m = dowild(&p[pi..], &t[ti..]);
                    if m != FALSE {
                        if !special || m != ABORT_TO_STARSTAR {
                            return m;
                        }
                    } else if !special && t[ti] == b'/' {
                        return ABORT_TO_STARSTAR;
                    }
                    ti += 1;
                }
                return ABORT_ALL;
            }
            b'[' => match class(p, &mut pi, t_ch) {
                Some(true) if t_ch != b'/' => {}
                Some(_) => return FALSE,
                None => return ABORT_ALL,
            },
            c => {
                if t_ch != c {
                    return FALSE;
                }
                pi += 1;
            }
        }
        ti += 1;
    }
    if ti < t.len() { FALSE } else { TRUE }
}

/// The character class at `p[*pi]` (`[`): whether `t_ch` is in it, with `*pi` moved past
/// it. `None`: the class is malformed.
fn class(p: &[u8], pi: &mut usize, t_ch: u8) -> Option<bool> {
    let at = |i: usize| p.get(i).copied().unwrap_or(0);
    let mut i = *pi + 1;
    let mut p_ch = at(i);
    if p_ch == b'^' {
        p_ch = b'!';
    }
    let negated = p_ch == b'!';
    if negated {
        i += 1;
        p_ch = at(i);
    }
    let mut prev_ch = 0u8;
    let mut matched = false;
    loop {
        if p_ch == 0 {
            return None;
        }
        if p_ch == b'\\' {
            i += 1;
            p_ch = at(i);
            if p_ch == 0 {
                return None;
            }
            matched |= t_ch == p_ch;
        } else if p_ch == b'-' && prev_ch != 0 && at(i + 1) != 0 && at(i + 1) != b']' {
            i += 1;
            p_ch = at(i);
            if p_ch == b'\\' {
                i += 1;
                p_ch = at(i);
                if p_ch == 0 {
                    return None;
                }
            }
            matched |= t_ch <= p_ch && t_ch >= prev_ch;
            p_ch = 0;
        } else if p_ch == b'[' && at(i + 1) == b':' {
            let s = i + 2;
            let mut e = s;
            while e < p.len() && p[e] != b']' {
                e += 1;
            }
            if e >= p.len() {
                return None;
            }
            if e < s + 1 || p[e - 1] != b':' {
                // No `:]`: an ordinary `[`.
                matched |= t_ch == b'[';
            } else {
                let c = t_ch;
                matched |= match &p[s..e - 1] {
                    b"alnum" => c.is_ascii_alphanumeric(),
                    b"alpha" => c.is_ascii_alphabetic(),
                    b"blank" => c == b' ' || c == b'\t',
                    b"cntrl" => c.is_ascii_control(),
                    b"digit" => c.is_ascii_digit(),
                    b"graph" => c.is_ascii_graphic(),
                    b"lower" => c.is_ascii_lowercase(),
                    b"print" => c.is_ascii_graphic() || c == b' ',
                    b"punct" => c.is_ascii_punctuation(),
                    b"space" => is_space(c),
                    b"upper" => c.is_ascii_uppercase(),
                    b"xdigit" => c.is_ascii_hexdigit(),
                    _ => return None,
                };
                i = e;
                p_ch = 0;
            }
        } else {
            matched |= t_ch == p_ch;
        }
        prev_ch = p_ch;
        i += 1;
        p_ch = at(i);
        if p_ch == b']' {
            break;
        }
    }
    *pi = i + 1;
    Some(matched != negated)
}

/// The last `count` path elements of `t`, or `None` if it has fewer.
fn trailing_elements(t: &[u8], mut count: usize) -> Option<&[u8]> {
    for i in (0..t.len()).rev() {
        if t[i] == b'/' {
            count -= 1;
            if count == 0 {
                return Some(&t[i + 1..]);
            }
        }
    }
    (count == 1).then_some(t)
}

/// rsync's `wildmatch_array`: `where_` > 0 matches the last that many elements, < 0 at
/// the start or after any `/`, 0 the whole text.
fn wildmatch_array(p: &[u8], text: &[u8], where_: isize) -> bool {
    let t = if where_ > 0 {
        match trailing_elements(text, where_ as usize) {
            Some(t) => t,
            None => return false,
        }
    } else {
        text
    };
    let mut matched = dowild(p, t);
    if matched != TRUE && where_ < 0 && matched != ABORT_ALL {
        let mut i = 0;
        loop {
            if i >= t.len() {
                return false;
            }
            i += 1;
            if t[i - 1] == b'/' {
                matched = dowild(p, &t[i..]);
                if matched != FALSE && matched != ABORT_TO_STARSTAR {
                    break;
                }
            }
        }
    }
    matched == TRUE
}

fn litmatch_array(p: &[u8], text: &[u8], where_: isize) -> bool {
    if where_ > 0 {
        return trailing_elements(text, where_ as usize).is_some_and(|t| t == p);
    }
    text == p
}

/// rsync's wildcard matching of `t` against the whole pattern `p`.
pub fn wildmatch(p: &[u8], t: &[u8]) -> bool {
    dowild(p, t) == TRUE
}

// ---------------------------------------------------------------------------
// The compiled filter and per-directory state
// ---------------------------------------------------------------------------

/// Which rules a check uses.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Side {
    /// Whether an entry is sent.
    Sender,
    /// Whether `--delete` leaves a destination entry alone.
    Receiver,
    /// The same, inside a directory being deleted, where perishable rules don't count.
    Deleting,
}

impl Side {
    fn slot(self) -> usize {
        usize::from(self != Side::Sender)
    }
}

#[derive(Debug)]
enum Item {
    Rule(Rule),
    /// A `:` rule: index in [`Filter::merges`].
    Merge(usize),
    /// A `-C` rule: the CVS excludes.
    Cvs,
}

/// A `:` rule.
#[derive(Debug)]
struct DirMerge {
    /// The file read in each directory.
    name: Vec<u8>,
    /// `: /NAME`, `: ../NAME`: the directory from which the ones above the first transfer
    /// directory are read too.
    above: Option<Vec<u8>>,
    tpl: Template,
    no_inherit: bool,
}

/// The rules read from per-directory files of one `:` rule: one directory's, then those
/// it inherited.
#[derive(Debug)]
struct Layer {
    rules: Vec<Rule>,
    next: Option<Arc<Layer>>,
}

#[derive(Debug)]
struct DirState {
    /// The transfer root's absolute path, which `/` rules match under.
    abs_root: Arc<[u8]>,
    /// One per `:` rule.
    lists: Vec<Option<Arc<Layer>>>,
}

/// What one directory adds to the global rules: the per-directory files of it and its
/// parents, and its transfer root's absolute path (for `/` rules). Empty when the rules
/// need neither.
#[derive(Debug, Clone, Default)]
pub struct DirRules(Option<Arc<DirState>>);

#[derive(Debug, Default)]
pub struct Filter {
    items: Vec<Item>,
    merges: Vec<DirMerge>,
    /// What `-C` rules check: rsync's defaults, `~/.cvsignore` and `$CVSIGNORE`.
    cvs: Vec<Rule>,
    delete_excluded: bool,
    /// `--from0`: per-directory files hold NUL-separated rules.
    from0: bool,
    /// Some rule matches absolute paths.
    abs: bool,
    /// Per side (sender, receiver): the per-directory rules read above the first transfer
    /// directory, read once (see [`Filter::root`]).
    above: [OnceLock<Vec<Option<Arc<Layer>>>>; 2],
}

impl Filter {
    /// `from0`: rules files have NUL-separated rules (`--from0`).
    pub fn new(specs: &[RuleSpec], delete_excluded: bool, from0: bool) -> Result<Filter> {
        let mut f = Filter { delete_excluded, from0, ..Filter::default() };
        for spec in specs {
            let item = match spec {
                RuleSpec::Pattern { include, pattern, mods } => {
                    if pattern.is_empty() {
                        bail!("empty filter pattern");
                    }
                    f.abs |= mods.abs;
                    Item::Rule(Rule::new(*include, pattern, *mods, Anchor::Root, delete_excluded))
                }
                RuleSpec::DirMerge { name, merge, mods } => {
                    let (dir, base) = match name.iter().rposition(|&c| c == b'/') {
                        Some(i) => (Some(name[..i.max(1)].to_vec()), name[i + 1..].to_vec()),
                        None => (None, name.clone()),
                    };
                    if base.is_empty() || base == b"." || base == b".." {
                        bail!("invalid per-directory filter file name: {}", shown(name));
                    }
                    f.abs |= mods.abs;
                    f.merges.push(DirMerge {
                        name: base,
                        above: dir,
                        tpl: Template::merge(*merge, *mods),
                        no_inherit: merge.no_inherit,
                    });
                    Item::Merge(f.merges.len() - 1)
                }
                RuleSpec::Cvs { mods } => {
                    if !f.items.iter().any(|i| matches!(i, Item::Cvs)) {
                        f.cvs = cvs_rules(*mods, delete_excluded, from0)?;
                    }
                    Item::Cvs
                }
            };
            f.items.push(item);
        }
        Ok(f)
    }

    pub fn is_empty(&self) -> bool {
        self.items.is_empty()
    }

    /// No per-directory files and no absolute-path rules: any side can check a path the
    /// same way.
    pub fn is_static(&self) -> bool {
        self.merges.is_empty() && !self.abs
    }

    /// The rules need a [`DirRules`] per directory.
    fn stateful(&self) -> bool {
        !self.is_static()
    }

    /// The state for the directories of a transfer root on `side`: `transfer_root` is the
    /// directory paths are relative to, `first_dir` the first directory whose files are
    /// read (for `: /NAME`, the directories above it are read, once per side).
    pub fn root(&self, transfer_root: &Path, first_dir: &Path, side: Side) -> DirRules {
        if !self.stateful() {
            return DirRules::default();
        }
        let lists = self.above[side.slot()].get_or_init(|| self.read_above(first_dir)).clone();
        let abs_root: Arc<[u8]> = absolute(transfer_root).as_os_str().as_bytes().into();
        DirRules(Some(Arc::new(DirState { abs_root, lists })))
    }

    /// The state of directory `dir` (at `rel` in the transfer) given its parent's: its
    /// per-directory files are read. An error names a file that could not be read or has
    /// an invalid rule.
    pub fn enter(&self, parent: &DirRules, dir: &Path, rel: &[u8]) -> Result<DirRules, Vec<u8>> {
        let Some(st) = &parent.0 else { return Ok(DirRules::default()) };
        if self.merges.is_empty() {
            return Ok(parent.clone());
        }
        let mut lists = Vec::with_capacity(self.merges.len());
        let mut changed = false;
        for (m, inherited) in self.merges.iter().zip(&st.lists) {
            let layer = match self.read_layer(m, dir, Anchor::Dir(rel))? {
                Some((rules, cleared)) => {
                    changed = true;
                    let next = if cleared || m.no_inherit { None } else { inherited.clone() };
                    Some(Arc::new(Layer { rules, next }))
                }
                None if m.no_inherit => {
                    changed |= inherited.is_some();
                    None
                }
                None => inherited.clone(),
            };
            lists.push(layer);
        }
        if !changed {
            return Ok(parent.clone());
        }
        Ok(DirRules(Some(Arc::new(DirState { abs_root: st.abs_root.clone(), lists }))))
    }

    /// Whether the entry at `rel` (relative to the transfer root) is excluded on `side`,
    /// in the directory whose state is `at`.
    pub fn excluded(&self, at: &DirRules, rel: &[u8], is_dir: bool, side: Side) -> bool {
        if self.items.is_empty() {
            return false;
        }
        let st = at.0.as_deref();
        let abs_root = st.map(|s| &*s.abs_root);
        let first = |rules: &[Rule]| {
            rules.iter().find(|r| r.applies(side) && r.matches(rel, is_dir, abs_root)).map(|r| r.include)
        };
        for item in &self.items {
            let found = match item {
                Item::Rule(r) => first(std::slice::from_ref(r)),
                Item::Cvs => first(&self.cvs),
                Item::Merge(i) => {
                    let mut layer = st.and_then(|s| s.lists.get(*i)).and_then(|l| l.as_deref());
                    let mut found = None;
                    while let Some(l) = layer {
                        found = first(&l.rules);
                        if found.is_some() {
                            break;
                        }
                        layer = l.next.as_deref();
                    }
                    found
                }
            };
            if let Some(include) = found {
                return !include;
            }
        }
        false
    }

    /// One `:` rule's file in `dir`: its rules and whether it cleared the inherited ones,
    /// or `None` when there is no such file.
    fn read_layer(&self, m: &DirMerge, dir: &Path, anchor: Anchor) -> Result<Option<(Vec<Rule>, bool)>, Vec<u8>> {
        let path = dir.join(OsStr::from_bytes(&m.name));
        let fail = |what: String| [b"failed to read filter rules from \"", path.as_os_str().as_bytes(), b"\": ", what.as_bytes()].concat();
        let data = match std::fs::read(&path) {
            Ok(d) => d,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound || e.raw_os_error() == Some(libc::EISDIR) => return Ok(None),
            Err(e) => return Err(fail(e.to_string())),
        };
        let mut rules = Vec::new();
        let mut cleared = false;
        for line in split_rules(&data, m.tpl.words, self.from0) {
            let mut s = line;
            loop {
                match next_tok(&mut s, &m.tpl) {
                    Ok(None) => break,
                    Ok(Some(Tok::Clear)) => {
                        rules.clear();
                        cleared = true;
                    }
                    Ok(Some(Tok::Pattern { include, pattern, mods })) => {
                        rules.push(Rule::new(include, pattern, mods, anchor, self.delete_excluded))
                    }
                    Ok(Some(Tok::Merge { .. } | Tok::Cvs { .. })) => {
                        return Err(fail("merge and CVS rules are not supported in per-directory files".into()));
                    }
                    Err(e) => return Err(fail(e.to_string())),
                }
            }
        }
        Ok(Some((rules, cleared)))
    }

    /// `: /NAME` and `: ../NAME`: the files in the directories from the named one down to
    /// the parent of `first_dir` (rsync reads them before the transfer starts).
    fn read_above(&self, first_dir: &Path) -> Vec<Option<Arc<Layer>>> {
        let first = absolute(first_dir);
        let mut lists = vec![None; self.merges.len()];
        for (m, list) in self.merges.iter().zip(&mut lists) {
            let Some(above) = m.above.as_deref().filter(|_| !m.no_inherit) else { continue };
            let from = Path::new(OsStr::from_bytes(above));
            let from = clean(&if from.is_absolute() { from.to_path_buf() } else { first.join(from) });
            let Ok(below) = first.strip_prefix(&from) else { continue };
            let mut dir = from.clone();
            let mut dirs = vec![dir.clone()];
            let parts: Vec<_> = below.components().collect();
            for c in parts.iter().take(parts.len().saturating_sub(1)) {
                dir.push(c);
                dirs.push(dir.clone());
            }
            if parts.is_empty() {
                continue;
            }
            for d in dirs {
                let abs = d.as_os_str().as_bytes().to_vec();
                // Unreadable or invalid files here are left out, as rsync's are when not fatal.
                if let Ok(Some((rules, cleared))) = self.read_layer(m, &d, Anchor::Abs(&abs)) {
                    let next = if cleared { None } else { list.take() };
                    *list = Some(Arc::new(Layer { rules, next }));
                }
            }
        }
        lists
    }
}

/// The CVS excludes a `-C` rule checks, with its modifiers: rsync's defaults
/// (perishable), then `~/.cvsignore` and `$CVSIGNORE`.
fn cvs_rules(mods: Mods, delete_excluded: bool, from0: bool) -> Result<Vec<Rule>> {
    let mods = match (mods.sender, mods.receiver) {
        (false, false) if delete_excluded => Mods { sender: true, ..mods },
        _ => mods,
    };
    let tpl = Template { mods: Mods { negate: false, ..mods }, no_prefixes: true, words: true, cvs: true, ..Template::default() };
    let mut rules = Vec::new();
    let mut add = |text: &[u8], tpl: &Template| -> Result<()> {
        let mut s = text;
        while let Some(tok) = next_tok(&mut s, tpl)? {
            match tok {
                Tok::Clear => rules.clear(),
                Tok::Pattern { include, pattern, mods } => rules.push(Rule::new(include, pattern, mods, Anchor::Root, false)),
                _ => {}
            }
        }
        Ok(())
    };
    add(DEFAULT_CVSIGNORE, &Template { mods: Mods { perishable: true, ..tpl.mods }, ..tpl })?;
    if let Some(home) = std::env::var_os("HOME")
        && let Ok(data) = std::fs::read(Path::new(&home).join(".cvsignore"))
    {
        for word in split_rules(&data, true, from0) {
            add(word, &tpl)?;
        }
    }
    if let Some(v) = std::env::var_os("CVSIGNORE") {
        add(v.as_bytes(), &tpl)?;
    }
    Ok(rules)
}

/// `p` as an absolute path: resolved like the working directory rsync changes into, or
/// made absolute and cleaned when it doesn't exist.
fn absolute(p: &Path) -> PathBuf {
    std::fs::canonicalize(p).unwrap_or_else(|_| clean(&std::env::current_dir().unwrap_or_default().join(p)))
}

/// `p` with `.` and `..` resolved lexically.
fn clean(p: &Path) -> PathBuf {
    let mut out = PathBuf::new();
    for c in p.components() {
        match c {
            Component::CurDir => {}
            Component::ParentDir => {
                out.pop();
            }
            c => out.push(c),
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse(rules: &[&str]) -> Vec<RuleSpec> {
        let mut p = RuleParser::new(false);
        for r in rules {
            p.filter(r.as_bytes()).unwrap();
        }
        p.rules
    }

    fn f(rules: &[(&str, bool)]) -> Filter {
        let specs: Vec<RuleSpec> = rules
            .iter()
            .map(|(p, inc)| RuleSpec::Pattern { include: *inc, pattern: p.as_bytes().to_vec(), mods: Mods::default() })
            .collect();
        Filter::new(&specs, false, false).unwrap()
    }

    fn excl(flt: &Filter, rel: &str, is_dir: bool) -> bool {
        flt.excluded(&DirRules::default(), rel.as_bytes(), is_dir, Side::Sender)
    }

    #[test]
    fn basename_patterns_match_at_any_depth() {
        let flt = f(&[("*.o", false)]);
        assert!(excl(&flt, "a.o", false));
        assert!(excl(&flt, "x/y/a.o", false));
        assert!(!excl(&flt, "a.c", false));
    }

    #[test]
    fn anchored_patterns_only_match_at_root() {
        let flt = f(&[("/build", false)]);
        assert!(excl(&flt, "build", true));
        assert!(!excl(&flt, "src/build", true));
    }

    #[test]
    fn slash_patterns_match_path_suffixes() {
        let flt = f(&[("foo/bar", false)]);
        assert!(excl(&flt, "foo/bar", false));
        assert!(excl(&flt, "x/foo/bar", false));
        assert!(!excl(&flt, "xfoo/bar", false));
        let flt = f(&[("f*o/bar", false)]);
        assert!(excl(&flt, "x/foo/bar", false));
        assert!(!excl(&flt, "x/foo/x/bar", false));
    }

    #[test]
    fn dir_only_patterns() {
        let flt = f(&[("cache/", false)]);
        assert!(excl(&flt, "a/cache", true));
        assert!(!excl(&flt, "a/cache", false));
    }

    #[test]
    fn star_does_not_cross_slash_but_double_star_does() {
        let flt = f(&[("/a/*.txt", false)]);
        assert!(excl(&flt, "a/x.txt", false));
        assert!(!excl(&flt, "a/b/x.txt", false));
        let flt = f(&[("/a/**.txt", false)]);
        assert!(excl(&flt, "a/b/x.txt", false));
        let flt = f(&[("a/**/c", false)]);
        assert!(excl(&flt, "x/a/b/c", false));
        assert!(!excl(&flt, "x/a/c", false), "the `/` of `**/` must be there");
        let flt = f(&[("**/c", false)]);
        assert!(excl(&flt, "c", false) && excl(&flt, "a/b/c", false));
    }

    #[test]
    fn first_match_wins() {
        let flt = f(&[("keep.log", true), ("*.log", false)]);
        assert!(!excl(&flt, "keep.log", false));
        assert!(excl(&flt, "drop.log", false));
    }

    #[test]
    fn wildmatch_basics() {
        assert!(wildmatch(b"a?c", b"abc"));
        assert!(!wildmatch(b"a?c", b"a/c"));
        assert!(wildmatch(b"[a-c]x", b"bx"));
        assert!(!wildmatch(b"[!a-c]x", b"bx"));
        assert!(wildmatch(b"[^a-c]x", b"dx"));
        assert!(wildmatch(b"[[:digit:]]*", b"7up"));
        assert!(wildmatch(b"[]a]", b"]"));
        assert!(wildmatch(b"\\*", b"*"));
        assert!(!wildmatch(b"\\*", b"x"));
        assert!(wildmatch(b"**", b"a/b/c"));
        assert!(wildmatch(b"a/**/c", b"a/b/x/c"));
        // A malformed class never matches (rsync accepts the pattern).
        assert!(!wildmatch(b"[abc", b"a"));
        assert!(!wildmatch(b"[[:nope:]]", b"a"));
    }

    #[test]
    fn triple_star_matches_dir_and_contents() {
        let flt = f(&[("/data/***", false)]);
        assert!(excl(&flt, "data", true));
        assert!(excl(&flt, "data/x/y", false));
        assert!(!excl(&flt, "database", true));
    }

    #[test]
    fn filter_rule_syntax() {
        let r = parse(&["- *.o", "+ keep", "-_under", "exclude long", "include,s side", "P prot", "H hidden", "S shown", "R risk"]);
        let p = |inc: bool, pat: &str, s: bool, rcv: bool| RuleSpec::Pattern {
            include: inc,
            pattern: pat.as_bytes().to_vec(),
            mods: Mods { sender: s, receiver: rcv, ..Mods::default() },
        };
        assert_eq!(
            r,
            vec![
                p(false, "*.o", false, false),
                p(true, "keep", false, false),
                p(false, "under", false, false),
                p(false, "long", false, false),
                p(true, "side", true, false),
                p(false, "prot", false, true),
                p(false, "hidden", true, false),
                p(true, "shown", true, false),
                p(true, "risk", false, true),
            ]
        );
        // Modifiers, with and without the comma.
        let r = parse(&["-!p foo", "+,/ bar"]);
        assert_eq!(r[0], RuleSpec::Pattern { include: false, pattern: b"foo".to_vec(), mods: Mods { negate: true, perishable: true, ..Mods::default() } });
        assert_eq!(r[1], RuleSpec::Pattern { include: true, pattern: b"bar".to_vec(), mods: Mods { abs: true, ..Mods::default() } });
        // Spaces after the separator belong to the pattern.
        assert_eq!(parse(&["-  two"])[0], RuleSpec::exclude(" two"));
        // `!` clears what came before.
        assert_eq!(parse(&["- a", "!", "- b"]), vec![RuleSpec::exclude("b")]);
        assert_eq!(parse(&["- a", "clear", "- b"]), vec![RuleSpec::exclude("b")]);
        for bad in ["x foo", "-", "! x", "-q foo", "exclud foo", "-n foo", "-x foo", ":! .f"] {
            let mut p = RuleParser::new(false);
            assert!(p.filter(bad.as_bytes()).is_err(), "{bad}");
        }
    }

    #[test]
    fn old_style_patterns_take_plus_and_minus_prefixes() {
        let mut p = RuleParser::new(false);
        p.pattern(b"+ keep", false).unwrap();
        p.pattern(b"- drop", true).unwrap();
        p.pattern(b"-plain", false).unwrap();
        p.pattern(b"!x", false).unwrap();
        assert_eq!(
            p.rules,
            vec![
                RuleSpec::Pattern { include: true, pattern: b"keep".to_vec(), mods: Mods::default() },
                RuleSpec::exclude("drop"),
                RuleSpec::exclude("-plain"),
                RuleSpec::exclude("!x"),
            ]
        );
        p.pattern(b"!", false).unwrap();
        assert!(p.rules.is_empty(), "a lone `!` clears");
    }

    #[test]
    fn merge_files_are_read_in_place() {
        let tmp = tempfile::tempdir().unwrap();
        let a = tmp.path().join("a.rules");
        let b = tmp.path().join("b.rules");
        std::fs::write(&a, format!("# comment\n- one\n\n; also\n. {}\n+ three\r\n", b.display())).unwrap();
        std::fs::write(&b, "- two\n: .per-dir\n").unwrap();
        let mut p = RuleParser::new(false);
        p.filter(format!(". {}", a.display()).as_bytes()).unwrap();
        p.filter(b"merge,+ /nonexistent").unwrap_err();
        assert_eq!(
            p.rules,
            vec![
                RuleSpec::exclude("one"),
                RuleSpec::exclude("two"),
                RuleSpec::DirMerge { name: b".per-dir".to_vec(), merge: MergeMods::default(), mods: Mods::default() },
                RuleSpec::Pattern { include: true, pattern: b"three".to_vec(), mods: Mods::default() },
            ]
        );
        // `-`/`+` merge files hold bare patterns; `e` excludes the file itself.
        std::fs::write(&b, "x y\n- z\n").unwrap();
        let mut p = RuleParser::new(false);
        p.filter(format!("merge,+e {}", b.display()).as_bytes()).unwrap();
        let inc = |s: &str| RuleSpec::Pattern { include: true, pattern: s.as_bytes().to_vec(), mods: Mods::default() };
        assert_eq!(p.rules, vec![RuleSpec::exclude("b.rules"), inc("x y"), inc("- z")]);
        // Word-split.
        let mut p = RuleParser::new(false);
        p.filter(format!(".-w {}", b.display()).as_bytes()).unwrap();
        assert_eq!(p.rules, vec![RuleSpec::exclude("x"), RuleSpec::exclude("y"), RuleSpec::exclude("-"), RuleSpec::exclude("z")]);
        // A dir-merge named twice is read once.
        assert_eq!(parse(&[": .f", "dir-merge sub/.f"]).len(), 1);
    }

    #[test]
    fn sides_and_delete_excluded() {
        let specs = parse(&["- both", "H hide", "P prot"]);
        let flt = Filter::new(&specs, false, false).unwrap();
        let x = |side, rel: &str| flt.excluded(&DirRules::default(), rel.as_bytes(), false, side);
        assert!(x(Side::Sender, "both") && x(Side::Receiver, "both"));
        assert!(x(Side::Sender, "hide") && !x(Side::Receiver, "hide"));
        assert!(!x(Side::Sender, "prot") && x(Side::Receiver, "prot"));
        // --delete-excluded: plain rules are for the sender only; `P` still protects.
        let flt = Filter::new(&specs, true, false).unwrap();
        let x = |side, rel: &str| flt.excluded(&DirRules::default(), rel.as_bytes(), false, side);
        assert!(x(Side::Sender, "both") && !x(Side::Receiver, "both"));
        assert!(x(Side::Receiver, "prot"));
        // Perishable rules don't protect inside directories being deleted.
        let flt = Filter::new(&parse(&["-p *.o", "- *.keep"]), false, false).unwrap();
        let x = |side, rel: &str| flt.excluded(&DirRules::default(), rel.as_bytes(), false, side);
        assert!(x(Side::Receiver, "a.o") && !x(Side::Deleting, "a.o"));
        assert!(x(Side::Deleting, "a.keep"));
    }

    #[test]
    fn negated_rules() {
        let flt = Filter::new(&parse(&["-! */"]), false, false).unwrap();
        assert!(excl(&flt, "file", false), "`-! */` excludes non-directories");
        assert!(!excl(&flt, "dir", true));
        let flt = Filter::new(&parse(&["+! *.c", "- *"]), false, false).unwrap();
        assert!(!excl(&flt, "x.o", false));
        assert!(excl(&flt, "x.c", false));
    }

    #[test]
    fn cvs_rules_exclude_the_defaults() {
        let flt = Filter::new(&parse(&["-C"]), false, false).unwrap();
        for rel in ["core", "a/b.o", "x~", "#tmp", "CVS"] {
            assert!(excl(&flt, rel, false), "{rel}");
        }
        assert!(excl(&flt, ".git", true) && !excl(&flt, ".git", false));
        assert!(!excl(&flt, "main.c", false));
        assert!(!flt.excluded(&DirRules::default(), b"x.o", false, Side::Deleting), "the defaults are perishable");
    }

    #[test]
    fn per_directory_files_are_inherited_and_anchored_at_their_directory() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().join("src");
        std::fs::create_dir_all(root.join("sub/deep")).unwrap();
        std::fs::write(root.join(".f"), "- *.tmp\n- /top-only\n").unwrap();
        std::fs::write(root.join("sub/.f"), "+ keep.tmp\n- /anch\n").unwrap();
        std::fs::write(root.join("sub/deep/.f"), "!\n- other\n").unwrap();
        let flt = Filter::new(&parse(&[": .f"]), false, false).unwrap();
        let base = flt.root(&root, &root, Side::Sender);
        let top = flt.enter(&base, &root, b"").unwrap();
        let sub = flt.enter(&top, &root.join("sub"), b"sub").unwrap();
        let deep = flt.enter(&sub, &root.join("sub/deep"), b"sub/deep").unwrap();
        let x = |st: &DirRules, rel: &str| flt.excluded(st, rel.as_bytes(), false, Side::Sender);
        assert!(x(&top, "a.tmp") && x(&top, "top-only"));
        assert!(!x(&sub, "sub/top-only"), "anchored at the root's directory");
        assert!(x(&sub, "sub/x.tmp") && !x(&sub, "sub/keep.tmp"), "deeper rules come first");
        assert!(x(&sub, "sub/anch") && !x(&deep, "sub/deep/anch"));
        assert!(!x(&deep, "sub/deep/x.tmp") && x(&deep, "sub/deep/other"), "`!` clears the inherited rules");
        // `n`: not inherited.
        let flt = Filter::new(&parse(&[":n .f"]), false, false).unwrap();
        let top = flt.enter(&flt.root(&root, &root, Side::Sender), &root, b"").unwrap();
        let sub = flt.enter(&top, &root.join("sub"), b"sub").unwrap();
        assert!(flt.excluded(&top, b"a.tmp", false, Side::Sender));
        assert!(!flt.excluded(&sub, b"sub/a.tmp", false, Side::Sender));
        // A bad rule is an error naming the file.
        std::fs::write(root.join("sub/.f"), "bogus rule\n").unwrap();
        let flt = Filter::new(&parse(&[": .f"]), false, false).unwrap();
        let top = flt.enter(&flt.root(&root, &root, Side::Sender), &root, b"").unwrap();
        let err = flt.enter(&top, &root.join("sub"), b"sub").unwrap_err();
        assert!(String::from_utf8_lossy(&err).contains("sub/.f"), "{}", String::from_utf8_lossy(&err));
    }

    #[test]
    fn from0_per_directory_files_are_nul_separated() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().join("src");
        std::fs::create_dir_all(&root).unwrap();
        std::fs::write(root.join(".f"), "- secret\0+ keep\0").unwrap();
        for (from0, excluded) in [(true, true), (false, false)] {
            let flt = Filter::new(&parse(&[": .f"]), false, from0).unwrap();
            let st = flt.enter(&flt.root(&root, &root, Side::Sender), &root, b"").unwrap();
            assert_eq!(flt.excluded(&st, b"secret", false, Side::Sender), excluded, "from0={from0}");
        }
    }

    #[test]
    fn rooted_per_directory_files_are_also_read_above_the_transfer() {
        let tmp = tempfile::tempdir().unwrap();
        let top = std::fs::canonicalize(tmp.path()).unwrap();
        let root = top.join("a/src");
        std::fs::create_dir_all(&root).unwrap();
        std::fs::write(top.join(".f"), "- from-top\n- /a/src/anchored\n").unwrap();
        std::fs::write(top.join("a/.f"), "- from-a\n").unwrap();
        let flt = Filter::new(&parse(&[": /.f"]), false, false).unwrap();
        let st = flt.enter(&flt.root(&root, &root, Side::Sender), &root, b"").unwrap();
        let x = |rel: &str| flt.excluded(&st, rel.as_bytes(), false, Side::Sender);
        assert!(x("from-top") && x("x/from-a"));
        assert!(x("anchored") && !x("x/anchored"), "anchored at its own directory, above the transfer");
        let flt = Filter::new(&parse(&[": .f"]), false, false).unwrap();
        let st = flt.enter(&flt.root(&root, &root, Side::Sender), &root, b"").unwrap();
        assert!(!flt.excluded(&st, b"from-top", false, Side::Sender));
    }
}

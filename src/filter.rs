//! rsync-style include/exclude rules (the `--include`/`--exclude` subset).
//!
//! Semantics follow rsync's "FILTER RULES" section:
//! - rules are checked in order and the first match wins; no match means "included";
//! - a trailing `/` only matches directories;
//! - a leading `/` anchors the pattern at the transfer root;
//! - a pattern containing `/` (or `**`) is matched against the full relative path,
//!   otherwise against the final path component;
//! - `*` and `?` stop at `/`, `**` crosses it, and `dir/***` matches `dir` and everything below.

use anyhow::{Context, Result, bail};
use std::path::Path;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RuleSpec {
    pub include: bool,
    pub pattern: String,
}

#[derive(Debug)]
struct Rule {
    include: bool,
    dir_only: bool,
    /// Match against the final component only (pattern had no `/` and no `**`).
    basename_only: bool,
    /// Alternative wildcard patterns; the rule matches if any of them does.
    patterns: Vec<Vec<u8>>,
}

#[derive(Debug, Default)]
pub struct Filter {
    rules: Vec<Rule>,
}

impl Filter {
    pub fn new(specs: &[RuleSpec]) -> Result<Filter> {
        let rules = specs.iter().map(compile).collect::<Result<Vec<_>>>()?;
        Ok(Filter { rules })
    }

    pub fn is_empty(&self) -> bool {
        self.rules.is_empty()
    }

    /// `rel` is the path relative to the transfer root, `/`-separated, without a leading `./`.
    pub fn is_excluded(&self, rel: &[u8], is_dir: bool) -> bool {
        if self.rules.is_empty() {
            return false;
        }
        let base = match rel.iter().rposition(|&b| b == b'/') {
            Some(i) => &rel[i + 1..],
            None => rel,
        };
        for rule in &self.rules {
            if rule.dir_only && !is_dir {
                continue;
            }
            let candidate = if rule.basename_only { base } else { rel };
            if rule.patterns.iter().any(|p| wildmatch(p, candidate)) {
                return !rule.include;
            }
        }
        false
    }
}

fn compile(spec: &RuleSpec) -> Result<Rule> {
    let mut pat = spec.pattern.as_str();
    if pat.is_empty() {
        bail!("empty filter pattern");
    }
    let mut globs: Vec<String> = Vec::new();
    let mut dir_only = false;

    // `dir/***` matches the directory itself and everything inside it.
    let triple = pat.strip_suffix("/***");
    if let Some(base) = triple {
        pat = base;
    } else if let Some(stripped) = pat.strip_suffix('/') {
        dir_only = true;
        pat = stripped;
    }

    let anchored = pat.starts_with('/');
    let body = pat.trim_start_matches('/');
    let has_slash = body.contains('/') || body.contains("**") || triple.is_some();
    let basename_only = !anchored && !has_slash;

    let mut push = |g: String| {
        if triple.is_some() {
            globs.push(format!("{g}/**"));
        }
        globs.push(g);
    };
    if anchored || basename_only {
        push(body.to_string());
    } else {
        push(body.to_string());
        push(format!("**/{body}"));
    }

    for g in &globs {
        validate(g.as_bytes()).with_context(|| format!("invalid filter pattern {:?}", spec.pattern))?;
    }
    Ok(Rule {
        include: spec.include,
        dir_only,
        basename_only,
        patterns: globs.into_iter().map(String::into_bytes).collect(),
    })
}

fn validate(p: &[u8]) -> Result<()> {
    let mut i = 0;
    while i < p.len() {
        match p[i] {
            b'\\' => i += 2,
            b'[' => match class_len(&p[i..]) {
                Some(n) => i += n,
                None => bail!("unterminated character class"),
            },
            _ => i += 1,
        }
    }
    Ok(())
}

/// rsync's wildmatch: `?` and `*` never match `/`, `**` matches anything,
/// `[...]` is a character class (`!`/`^` negates, ranges, `[:alpha:]`-style names),
/// and `\\` escapes the next character.
pub fn wildmatch(p: &[u8], t: &[u8]) -> bool {
    let (mut pi, mut ti) = (0, 0);
    while pi < p.len() {
        match p[pi] {
            b'\\' if pi + 1 < p.len() => {
                if t.get(ti) != Some(&p[pi + 1]) {
                    return false;
                }
                pi += 2;
                ti += 1;
            }
            b'?' => {
                if ti >= t.len() || t[ti] == b'/' {
                    return false;
                }
                pi += 1;
                ti += 1;
            }
            b'*' => {
                let mut np = pi;
                while np < p.len() && p[np] == b'*' {
                    np += 1;
                }
                let double = np - pi >= 2;
                let rest = &p[np..];
                if rest.is_empty() {
                    return double || !t[ti..].contains(&b'/');
                }
                let mut k = ti;
                loop {
                    if wildmatch(rest, &t[k..]) {
                        return true;
                    }
                    if k >= t.len() || (!double && t[k] == b'/') {
                        return false;
                    }
                    k += 1;
                }
            }
            b'[' => {
                let Some(n) = class_len(&p[pi..]) else { return false };
                match t.get(ti) {
                    Some(&c) if c != b'/' && class_matches(&p[pi + 1..pi + n - 1], c) => {}
                    _ => return false,
                }
                pi += n;
                ti += 1;
            }
            c => {
                if t.get(ti) != Some(&c) {
                    return false;
                }
                pi += 1;
                ti += 1;
            }
        }
    }
    ti == t.len()
}

/// Length of the `[...]` class starting at `p[0]`, including both brackets.
fn class_len(p: &[u8]) -> Option<usize> {
    let mut i = 1;
    if matches!(p.get(i), Some(b'!' | b'^')) {
        i += 1;
    }
    if p.get(i) == Some(&b']') {
        i += 1;
    }
    while i < p.len() {
        match p[i] {
            b']' => return Some(i + 1),
            b'[' if p.get(i + 1) == Some(&b':') => {
                let end = p[i + 2..].windows(2).position(|w| w == b":]")?;
                i += 2 + end + 2;
            }
            b'\\' => i += 2,
            _ => i += 1,
        }
    }
    None
}

fn class_matches(body: &[u8], c: u8) -> bool {
    let (negate, body) = match body.first() {
        Some(b'!' | b'^') => (true, &body[1..]),
        _ => (false, body),
    };
    let mut i = 0;
    let mut hit = false;
    while i < body.len() {
        if body[i] == b'[' && body.get(i + 1) == Some(&b':') {
            let end = body[i + 2..].windows(2).position(|w| w == b":]").unwrap_or(body.len());
            let name = &body[i + 2..i + 2 + end];
            hit |= match name {
                b"alpha" => c.is_ascii_alphabetic(),
                b"digit" => c.is_ascii_digit(),
                b"alnum" => c.is_ascii_alphanumeric(),
                b"upper" => c.is_ascii_uppercase(),
                b"lower" => c.is_ascii_lowercase(),
                b"space" => c.is_ascii_whitespace(),
                b"punct" => c.is_ascii_punctuation(),
                b"xdigit" => c.is_ascii_hexdigit(),
                b"blank" => c == b' ' || c == b'\t',
                b"cntrl" => c.is_ascii_control(),
                b"graph" => c.is_ascii_graphic(),
                b"print" => c.is_ascii_graphic() || c == b' ',
                _ => false,
            };
            i += 2 + end + 2;
            continue;
        }
        let lo = if body[i] == b'\\' && i + 1 < body.len() {
            i += 1;
            body[i]
        } else {
            body[i]
        };
        if body.get(i + 1) == Some(&b'-') && i + 2 < body.len() {
            let hi = body[i + 2];
            hit |= lo <= c && c <= hi;
            i += 3;
        } else {
            hit |= lo == c;
            i += 1;
        }
    }
    hit != negate
}

/// Parse an `--exclude-from`/`--include-from` file. Blank lines and lines starting with
/// `;` or `#` are ignored; `+ ` and `- ` prefixes override the file's default polarity.
pub fn read_rules_file(path: &Path, include: bool) -> Result<Vec<RuleSpec>> {
    let text = if path.as_os_str() == "-" {
        std::io::read_to_string(std::io::stdin())?
    } else {
        std::fs::read_to_string(path).with_context(|| format!("reading {}", path.display()))?
    };
    let mut out = Vec::new();
    for line in text.lines() {
        let line = line.trim_end_matches('\r');
        if line.is_empty() || line.starts_with(';') || line.starts_with('#') {
            continue;
        }
        let (inc, pat) = if let Some(p) = line.strip_prefix("+ ") {
            (true, p)
        } else if let Some(p) = line.strip_prefix("- ") {
            (false, p)
        } else {
            (include, line)
        };
        out.push(RuleSpec { include: inc, pattern: pat.to_string() });
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn f(rules: &[(&str, bool)]) -> Filter {
        let specs: Vec<RuleSpec> = rules
            .iter()
            .map(|(p, inc)| RuleSpec { include: *inc, pattern: p.to_string() })
            .collect();
        Filter::new(&specs).unwrap()
    }

    #[test]
    fn basename_patterns_match_at_any_depth() {
        let flt = f(&[("*.o", false)]);
        assert!(flt.is_excluded(b"a.o", false));
        assert!(flt.is_excluded(b"x/y/a.o", false));
        assert!(!flt.is_excluded(b"a.c", false));
    }

    #[test]
    fn anchored_patterns_only_match_at_root() {
        let flt = f(&[("/build", false)]);
        assert!(flt.is_excluded(b"build", true));
        assert!(!flt.is_excluded(b"src/build", true));
    }

    #[test]
    fn slash_patterns_match_path_suffixes() {
        let flt = f(&[("foo/bar", false)]);
        assert!(flt.is_excluded(b"foo/bar", false));
        assert!(flt.is_excluded(b"x/foo/bar", false));
        assert!(!flt.is_excluded(b"xfoo/bar", false));
    }

    #[test]
    fn dir_only_patterns() {
        let flt = f(&[("cache/", false)]);
        assert!(flt.is_excluded(b"a/cache", true));
        assert!(!flt.is_excluded(b"a/cache", false));
    }

    #[test]
    fn star_does_not_cross_slash_but_double_star_does() {
        let flt = f(&[("/a/*.txt", false)]);
        assert!(flt.is_excluded(b"a/x.txt", false));
        assert!(!flt.is_excluded(b"a/b/x.txt", false));
        let flt = f(&[("/a/**.txt", false)]);
        assert!(flt.is_excluded(b"a/b/x.txt", false));
    }

    #[test]
    fn first_match_wins() {
        let flt = f(&[("keep.log", true), ("*.log", false)]);
        assert!(!flt.is_excluded(b"keep.log", false));
        assert!(flt.is_excluded(b"drop.log", false));
    }

    #[test]
    fn wildmatch_basics() {
        assert!(wildmatch(b"a?c", b"abc"));
        assert!(!wildmatch(b"a?c", b"a/c"));
        assert!(wildmatch(b"[a-c]x", b"bx"));
        assert!(!wildmatch(b"[!a-c]x", b"bx"));
        assert!(wildmatch(b"[[:digit:]]*", b"7up"));
        assert!(wildmatch(b"\\*", b"*"));
        assert!(!wildmatch(b"\\*", b"x"));
        assert!(wildmatch(b"**", b"a/b/c"));
        assert!(wildmatch(b"a/**/c", b"a/b/x/c"));
    }

    #[test]
    fn triple_star_matches_dir_and_contents() {
        let flt = f(&[("/data/***", false)]);
        assert!(flt.is_excluded(b"data", true));
        assert!(flt.is_excluded(b"data/x/y", false));
        assert!(!flt.is_excluded(b"database", true));
    }
}

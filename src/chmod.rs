//! `--chmod`: rsync's permission tweaks (chmod.c), applied to source modes as if the
//! sender had supplied them.

use anyhow::{Result, bail};
use serde::{Deserialize, Serialize};

const CHMOD_BITS: u32 = 0o7777;

/// One comma-separated clause, resolved to AND/OR masks when parsed.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct ChmodRule {
    dirs_only: bool,
    files_only: bool,
    /// `X`: only add execute bits to directories and already-executable files.
    x_keep: bool,
    and: u32,
    or: u32,
}

#[derive(Clone, Copy, PartialEq)]
enum Op {
    None,
    Add,
    Sub,
    Eq,
    Set,
}

#[derive(Clone, Copy, PartialEq)]
enum State {
    Who,
    Perms,
    Octal,
}

/// Parse one `--chmod` value (e.g. `Du+rwx,Fgo-w`, `D2775,F664`, `ugo=rwX`). `umask`
/// masks clauses that name no `ugoa`, as chmod(1) does.
pub fn parse_chmod(spec: &str, umask: u32) -> Result<Vec<ChmodRule>> {
    let mut rules = Vec::new();
    let (mut who, mut what, mut topbits, mut topoct) = (0u32, 0u32, 0u32, 0u32);
    let (mut dirs_only, mut files_only, mut x_keep) = (false, false, false);
    let mut op = Op::None;
    let mut state = State::Who;
    let bad = || anyhow::anyhow!("Invalid argument passed to --chmod ({spec})");
    let bytes = spec.as_bytes();
    let mut i = 0;
    loop {
        let c = bytes.get(i).copied();
        if c.is_none() || c == Some(b',') {
            if op == Op::None {
                return Err(bad());
            }
            let bits = if who != 0 {
                who * what
            } else {
                who = 0o111;
                (who * what) & !umask
            };
            let (and, or) = match op {
                Op::Add => (CHMOD_BITS, bits + topoct),
                Op::Sub => (CHMOD_BITS - bits - topoct, 0),
                Op::Eq => (CHMOD_BITS - who * 7 - if topoct != 0 { topbits } else { 0 }, bits + topoct),
                Op::Set => (0, bits),
                Op::None => unreachable!(),
            };
            rules.push(ChmodRule { dirs_only, files_only, x_keep, and, or });
            if c.is_none() {
                return Ok(rules);
            }
            (who, what, topbits, topoct) = (0, 0, 0, 0);
            (dirs_only, files_only, x_keep) = (false, false, false);
            op = Op::None;
            state = State::Who;
            i += 1;
            continue;
        }
        let c = c.unwrap();
        match state {
            State::Who => match c {
                b'D' if !files_only => dirs_only = true,
                b'F' if !dirs_only => files_only = true,
                b'u' => {
                    who |= 0o100;
                    topbits |= 0o4000;
                }
                b'g' => {
                    who |= 0o010;
                    topbits |= 0o2000;
                }
                b'o' => who |= 0o001,
                b'a' => who |= 0o111,
                b'+' | b'-' | b'=' => {
                    op = match c {
                        b'+' => Op::Add,
                        b'-' => Op::Sub,
                        _ => Op::Eq,
                    };
                    state = State::Perms;
                }
                b'0'..=b'7' if who == 0 => {
                    op = Op::Set;
                    state = State::Octal;
                    who = 1;
                    what = u32::from(c - b'0');
                }
                _ => return Err(bad()),
            },
            State::Perms => match c {
                b'r' => what |= 4,
                b'w' => what |= 2,
                b'x' => what |= 1,
                b'X' => {
                    x_keep = true;
                    what |= 1;
                }
                b's' => {
                    if topbits != 0 {
                        topoct |= topbits;
                    } else {
                        topoct = 0o4000;
                    }
                }
                b't' => topoct |= 0o1000,
                _ => return Err(bad()),
            },
            State::Octal => match c {
                b'0'..=b'7' => {
                    what = what * 8 + u32::from(c - b'0');
                    if what > CHMOD_BITS {
                        return Err(bad());
                    }
                }
                _ => return Err(bad()),
            },
        }
        i += 1;
    }
}

/// Apply the rules to permission bits `mode` (`& 0o7777`) of a file or directory.
pub fn tweak(rules: &[ChmodRule], mode: u32, is_dir: bool) -> u32 {
    let is_x = mode & 0o111 != 0;
    let mut mode = mode & CHMOD_BITS;
    for r in rules {
        if (r.dirs_only && !is_dir) || (r.files_only && is_dir) {
            continue;
        }
        mode &= r.and;
        if r.x_keep && !is_x && !is_dir {
            mode |= r.or & !0o111;
        } else {
            mode |= r.or;
        }
    }
    mode
}

/// All `--chmod` values, in order.
pub fn parse_all(specs: &[String], umask: u32) -> Result<Vec<ChmodRule>> {
    let mut rules = Vec::new();
    for s in specs {
        if s.is_empty() {
            bail!("Invalid argument passed to --chmod ()");
        }
        rules.extend(parse_chmod(s, umask)?);
    }
    Ok(rules)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn t(spec: &str, mode: u32, is_dir: bool) -> u32 {
        tweak(&parse_chmod(spec, 0o022).unwrap(), mode, is_dir)
    }

    #[test]
    fn matches_rsync() {
        // Expected values measured with rsync 3.5.1.
        assert_eq!(t("Du+rwx,Fgo-w", 0o640, false), 0o640);
        assert_eq!(t("Du+rwx,Fgo-w", 0o751, true), 0o751);
        assert_eq!(t("D2775,F664", 0o640, false), 0o664);
        assert_eq!(t("D2775,F664", 0o751, true), 0o2775);
        assert_eq!(t("ugo=rwX", 0o640, false), 0o666);
        assert_eq!(t("ugo=rwX", 0o751, true), 0o777);
        assert_eq!(t("a+x", 0o640, false), 0o751);
        assert_eq!(t("go-rwx,u+X", 0o644, false), 0o600);
        assert_eq!(t("go-rwx,u+X", 0o755, true), 0o700);
        assert_eq!(t("Fu+s", 0o640, false), 0o4640);
        assert_eq!(t("+x", 0o600, false), 0o711, "no ugoa: umask applies");
        assert_eq!(t("o=", 0o751, true), 0o750);
    }

    #[test]
    fn rejects_garbage() {
        for bad in ["Zq", "u", "u+q", "DF+x", "8", "u+rw,", "77777"] {
            assert!(parse_chmod(bad, 0o022).is_err(), "{bad}");
        }
    }
}

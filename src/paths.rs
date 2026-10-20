//! Interpreting SRC/DEST arguments the way rsync does: `host:path` detection and
//! trailing-slash semantics ("copy the contents" vs "copy the directory itself").

use std::ffi::{OsStr, OsString};
use std::io;
use std::net::Ipv6Addr;
use std::os::unix::ffi::OsStrExt;
use std::path::{Path, PathBuf};

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Location {
    Local(PathBuf),
    Remote { host: String, path: OsString },
}

/// A colon before any slash selects a remote location. IPv6 literals must be
/// bracketed, optionally after `user@`; only the brackets are removed for SSH.
/// Filename bytes remain opaque. Prefix a local name containing a colon with `./`.
pub fn parse_location(arg: &OsStr) -> io::Result<Location> {
    let bytes = arg.as_bytes();
    let invalid = |why: &str| {
        io::Error::new(io::ErrorKind::InvalidInput, format!("invalid remote location {arg:?}: {why}"))
    };
    if bytes.starts_with(b"rsync://") {
        return Err(invalid("rsync daemon mode is not supported"));
    }
    let Some(mut colon) = bytes.iter().position(|&b| b == b':') else {
        return Ok(Location::Local(PathBuf::from(arg)));
    };
    if colon == 0 || bytes.iter().position(|&b| b == b'/').is_some_and(|slash| slash < colon) {
        return Ok(Location::Local(PathBuf::from(arg)));
    }
    let host_start = bytes[..colon].iter().position(|&b| b == b'@').map_or(0, |at| at + 1);
    if bytes.get(host_start) == Some(&b'[') {
        let close = bytes[host_start + 1..]
            .iter()
            .position(|&b| b == b']')
            .map(|i| host_start + 1 + i)
            .ok_or_else(|| invalid("IPv6 address needs a closing ']' followed by ':'"))?;
        if bytes.get(close + 1) != Some(&b':') {
            return Err(invalid("IPv6 address needs ':' after ']'"));
        }
        colon = close + 1;
    }
    if bytes.get(colon + 1) == Some(&b':') {
        return Err(invalid("rsync daemon mode is not supported; bracket IPv6 addresses"));
    }
    let host = std::str::from_utf8(&bytes[..colon]).map_err(|_| invalid("host must be UTF-8"))?;
    let host = normalize_host(host).map_err(invalid)?;
    let path = OsStr::from_bytes(&bytes[colon + 1..]).to_os_string();
    Ok(Location::Remote { host, path })
}

fn normalize_host(input: &str) -> Result<String, &'static str> {
    let (user, host) = match input.split_once('@') {
        Some(("", _)) => return Err("no user name before '@' (an empty variable?)"),
        Some((user, host)) => {
            if !safe_host_part(user) {
                return Err("invalid user name");
            }
            (Some(user), host)
        }
        None => (None, input),
    };
    let host = if let Some(address) = host.strip_prefix('[').and_then(|s| s.strip_suffix(']')) {
        let ip = match address.split_once('%') {
            Some((ip, zone)) if safe_host_part(zone) => ip,
            Some(_) => return Err("invalid IPv6 scope identifier"),
            None => address,
        };
        if ip.parse::<Ipv6Addr>().is_err() {
            return Err("invalid bracketed IPv6 address");
        }
        address
    } else {
        if !safe_host_part(host) {
            return Err("invalid host: use a host name, SSH alias, IPv4 address or bracketed IPv6 address");
        }
        host
    };
    Ok(match user {
        Some(user) => format!("{user}@{host}"),
        None => host.to_string(),
    })
}

// Keep option prefixes, whitespace, controls and shell metacharacters out of
// SSH's destination argument (also used by configured ProxyCommand expansions).
fn safe_host_part(value: &str) -> bool {
    !value.is_empty()
        && !value.starts_with('-')
        && value.bytes().all(|b| b.is_ascii_alphanumeric() || b"._-+".contains(&b))
}

/// True when the argument names "the contents of" a directory: a trailing `/`,
/// or a final component of `.` (e.g. `.` or `dir/.`).
pub fn wants_contents(arg: &OsStr) -> bool {
    let b = arg.as_bytes();
    b.ends_with(b"/") || b == b"." || b.ends_with(b"/.")
}

/// Split a source argument into (parent directory, final component).
/// Returns `None` for the component when the path has no nameable final
/// component (`/`, `.`, `..`), which callers treat as "contents".
pub fn split_source(arg: &Path) -> (PathBuf, Option<OsString>) {
    match arg.file_name() {
        Some(name) if !arg.as_os_str().as_bytes().ends_with(b"/..") && arg != Path::new("..") => {
            let parent = arg
                .parent()
                .filter(|p| !p.as_os_str().is_empty())
                .map(Path::to_path_buf)
                .unwrap_or_else(|| PathBuf::from("."));
            (parent, Some(name.to_os_string()))
        }
        _ => (arg.to_path_buf(), None),
    }
}

/// Join a relative display path with a child name, both as raw bytes.
pub fn join_rel(rel: &[u8], name: &OsStr) -> Vec<u8> {
    let mut v = Vec::with_capacity(rel.len() + name.len() + 1);
    if !rel.is_empty() {
        v.extend_from_slice(rel);
        v.push(b'/');
    }
    v.extend_from_slice(name.as_bytes());
    v
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn remote_detection() {
        assert_eq!(
            parse_location(OsStr::new("user@host:/data")).unwrap(),
            Location::Remote { host: "user@host".into(), path: "/data".into() }
        );
        assert_eq!(
            parse_location(OsStr::new("host:")).unwrap(),
            Location::Remote { host: "host".into(), path: "".into() }
        );
        for path in ["./a:b", "/x/a:b", ":x", "./[::1]:file", "./rsync://host/module"] {
            assert_eq!(parse_location(OsStr::new(path)).unwrap(), Location::Local(path.into()));
        }
    }

    #[test]
    fn remote_hosts_and_raw_paths() {
        for (prefix, expected) in [
            ("127.0.0.1", "127.0.0.1"),
            ("host.example", "host.example"),
            ("my_ssh-alias", "my_ssh-alias"),
            ("user.name+tag@host", "user.name+tag@host"),
            ("[::1]", "::1"),
            ("user@[2001:db8::1]", "user@2001:db8::1"),
            ("[fe80::1%en0]", "fe80::1%en0"),
            ("[fe80::1%2]", "fe80::1%2"),
        ] {
            for path in [b"".as_slice(), b"~/dir/", b"/absolute/", b"space 'quote' $dollar :colon \xff/"] {
                let arg = [prefix.as_bytes(), b":", path].concat();
                assert_eq!(
                    parse_location(OsStr::from_bytes(&arg)).unwrap(),
                    Location::Remote { host: expected.into(), path: OsStr::from_bytes(path).into() }
                );
            }
        }
    }

    #[test]
    fn malformed_remote_locations_are_rejected() {
        for arg in [
            "-option:dst", "user@-option:dst", "@host:dst", "user@:dst", "a@b@host:dst",
            "host name:dst", "host\nname:dst", "host;command:dst", "user$(command)@host:dst",
            "[::1:dst", "[::1]", "[::1]junk:dst", "[host]:dst", "user@[]:dst",
            "[fe80::1%]:dst", "[fe80::1%a%b]:dst", "[fe80::1%a b]:dst",
            "host::module", "[::1]::module", "rsync://host/module",
        ] {
            assert!(parse_location(OsStr::new(arg)).is_err(), "accepted {arg:?}");
        }
        assert!(parse_location(OsStr::from_bytes(b"bad\xff:dst")).is_err());
    }

    #[test]
    fn trailing_slash() {
        assert!(wants_contents(OsStr::new("src/")));
        assert!(wants_contents(OsStr::new(".")));
        assert!(wants_contents(OsStr::new("a/.")));
        assert!(!wants_contents(OsStr::new("src")));
    }

    #[test]
    fn split() {
        assert_eq!(split_source(Path::new("a/b")), (PathBuf::from("a"), Some("b".into())));
        assert_eq!(split_source(Path::new("b")), (PathBuf::from("."), Some("b".into())));
        assert_eq!(split_source(Path::new("/")).1, None);
        assert_eq!(split_source(Path::new("..")).1, None);
    }
}

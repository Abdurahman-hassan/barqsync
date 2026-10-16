//! Interpreting SRC/DEST arguments the way rsync does: `host:path` detection and
//! trailing-slash semantics ("copy the contents" vs "copy the directory itself").

use std::ffi::{OsStr, OsString};
use std::os::unix::ffi::OsStrExt;
use std::path::{Path, PathBuf};

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Location {
    Local(PathBuf),
    Remote { host: String, path: OsString },
}

/// rsync treats `host:path` as remote when a `:` appears before any `/`.
/// `host::module` and `rsync://` (daemon mode) are rejected by the caller.
pub fn parse_location(arg: &OsStr) -> Location {
    let bytes = arg.as_bytes();
    if let Some(colon) = bytes.iter().position(|&b| b == b':') {
        let slash = bytes.iter().position(|&b| b == b'/');
        if colon > 0 && slash.is_none_or(|s| colon < s) {
            let host = String::from_utf8_lossy(&bytes[..colon]).into_owned();
            let path = OsStr::from_bytes(&bytes[colon + 1..]).to_os_string();
            return Location::Remote { host, path };
        }
    }
    Location::Local(PathBuf::from(arg))
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
            parse_location(OsStr::new("user@host:/data")),
            Location::Remote { host: "user@host".into(), path: "/data".into() }
        );
        assert_eq!(
            parse_location(OsStr::new("host:")),
            Location::Remote { host: "host".into(), path: "".into() }
        );
        assert_eq!(parse_location(OsStr::new("./a:b")), Location::Local("./a:b".into()));
        assert_eq!(parse_location(OsStr::new("/x/a:b")), Location::Local("/x/a:b".into()));
        assert_eq!(parse_location(OsStr::new(":x")), Location::Local(":x".into()));
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

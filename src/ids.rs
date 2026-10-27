//! User and group mapping by name for remote transfers (rsync's default; turned off by
//! `--numeric-ids`). The sender names each uid/gid it sends once; the receiver maps
//! names it knows to its local ids and keeps the number otherwise. Id 0 is never
//! mapped by name, as in rsync, though its name is sent for `--usermap`/`--groupmap`.
//!
//! `--usermap`/`--groupmap` (and `--chown`, which is `*:USER` and `*:GROUP`) come first:
//! the first pair whose `FROM` matches the sender's id or name decides (rsync's
//! uidlist.c), locally too.

use crate::entry::Meta;
use std::collections::{HashMap, HashSet};
use std::ffi::{CStr, CString};

pub type Names = Vec<(u32, Vec<u8>)>;

/// What a `--usermap`/`--groupmap` pair maps from.
#[derive(Debug, Clone, PartialEq, Eq)]
enum From {
    Id(u32),
    /// `LOW-HIGH`, inclusive.
    Range(u32, u32),
    /// The sender's name.
    Name(Vec<u8>),
    /// A wildcard (`*`, `?`, `[...]`) matched against the sender's name; an id without a
    /// name has an empty one, so `*` matches everything.
    Wild(Vec<u8>),
}

/// A user or group id by name, on the receiver.
pub type Lookup<'a> = dyn FnMut(&str) -> Option<u32> + 'a;

/// `--usermap`/`--groupmap`: the receiver's `FROM:TO` pairs, in order.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct NameMap(Vec<(From, u32)>);

impl NameMap {
    /// Parse `map` (`FROM:TO,...`) for users (`user`) or groups, with rsync's messages.
    /// A `TO` this system has no user or group by is reported through `unknown` and its
    /// pair left out, as rsync does; `lookup` is `None` to check the syntax alone. Like
    /// rsync, the pairs are read from the last one back, so that is the order errors
    /// come in.
    pub fn parse(map: &str, user: bool, lookup: Option<&mut Lookup<'_>>, unknown: &mut dyn FnMut(String)) -> Result<NameMap, String> {
        let what = if user { "user" } else { "group" };
        let mut lookup = lookup;
        let mut pairs = Vec::new();
        for item in map.split(',').rev() {
            let Some((from, to)) = item.split_once(':') else {
                return Err(format!("No colon found in --{what}map: {item}"));
            };
            if to.is_empty() {
                return Err(format!("No name found after colon --{what}map: {item}"));
            }
            let from = if from.starts_with(|c: char| c.is_ascii_digit()) {
                let bad = || format!("Invalid number in --{what}map: {from}");
                let (lo, hi) = match from.split_once('-') {
                    _ if !from.bytes().all(|c| c.is_ascii_digit() || c == b'-') => return Err(bad()),
                    Some((_, hi)) if hi.is_empty() || hi.contains('-') => return Err(bad()),
                    Some((lo, hi)) => (id_number(lo)?, id_number(hi)?),
                    None => (id_number(from)?, 0),
                };
                // A range ending at 0 is the single id, as in rsync.
                if hi == 0 { From::Id(lo) } else { From::Range(lo, hi) }
            } else if from.contains(['*', '[', '?']) {
                From::Wild(from.as_bytes().to_vec())
            } else {
                From::Name(from.as_bytes().to_vec())
            };
            let to_id = if to.bytes().all(|c| c.is_ascii_digit()) {
                id_number(to)?
            } else {
                match lookup.as_mut() {
                    Some(look) => match look(to) {
                        Some(id) => id,
                        None => {
                            unknown(format!("Unknown --{what}map name on receiver: {to}"));
                            continue;
                        }
                    },
                    None => 0,
                }
            };
            pairs.push((from, to_id));
        }
        pairs.reverse();
        Ok(NameMap(pairs))
    }

    /// [`NameMap::parse`] on the receiver: `TO` names are this system's users or groups.
    pub fn local(map: &str, user: bool, unknown: &mut dyn FnMut(String)) -> Result<NameMap, String> {
        let mut look = |name: &str| if user { user_id(name.as_bytes()) } else { group_id(name.as_bytes()) };
        NameMap::parse(map, user, Some(&mut look), unknown)
    }

    /// The local id the sender's `id` (named `name` there, empty if it has none) maps
    /// to, if a pair matches.
    pub fn get(&self, id: u32, name: &[u8]) -> Option<u32> {
        self.0
            .iter()
            .find(|(from, _)| match from {
                From::Id(i) => *i == id,
                From::Range(lo, hi) => (*lo..=*hi).contains(&id),
                From::Name(n) => n == name,
                From::Wild(p) => crate::filter::wildmatch(p, name),
            })
            .map(|&(_, to)| to)
    }
}

/// An id number in a map, as rsync's `id_parse` reads it.
fn id_number(s: &str) -> Result<u32, String> {
    s.parse().map_err(|_| format!("Invalid ID number: {s}"))
}

fn lookup<T>(mut call: impl FnMut(*mut libc::c_char, usize, *mut *mut T) -> libc::c_int) -> Option<*mut T> {
    let mut size = 1024;
    loop {
        let mut buf = vec![0 as libc::c_char; size];
        let mut res: *mut T = std::ptr::null_mut();
        let rc = call(buf.as_mut_ptr(), buf.len(), &mut res);
        if rc == libc::ERANGE && size < 1 << 20 {
            size *= 4;
            continue;
        }
        // The result points into `buf`, so callers copy what they need inside `call`.
        return (rc == 0 && !res.is_null()).then_some(res);
    }
}

pub fn user_name(uid: u32) -> Option<Vec<u8>> {
    let mut name = None;
    let mut pw: libc::passwd = unsafe { std::mem::zeroed() };
    lookup(|buf, len, res| {
        let rc = unsafe { libc::getpwuid_r(uid as libc::uid_t, &mut pw, buf, len, res) };
        if rc == 0 && !unsafe { *res }.is_null() {
            name = Some(unsafe { CStr::from_ptr(pw.pw_name) }.to_bytes().to_vec());
        }
        rc
    })?;
    name
}

pub fn group_name(gid: u32) -> Option<Vec<u8>> {
    let mut name = None;
    let mut gr: libc::group = unsafe { std::mem::zeroed() };
    lookup(|buf, len, res| {
        let rc = unsafe { libc::getgrgid_r(gid as libc::gid_t, &mut gr, buf, len, res) };
        if rc == 0 && !unsafe { *res }.is_null() {
            name = Some(unsafe { CStr::from_ptr(gr.gr_name) }.to_bytes().to_vec());
        }
        rc
    })?;
    name
}

pub fn user_id(name: &[u8]) -> Option<u32> {
    let c = CString::new(name).ok()?;
    let mut pw: libc::passwd = unsafe { std::mem::zeroed() };
    lookup(|buf, len, res| unsafe { libc::getpwnam_r(c.as_ptr(), &mut pw, buf, len, res) })?;
    Some(pw.pw_uid)
}

pub fn group_id(name: &[u8]) -> Option<u32> {
    let c = CString::new(name).ok()?;
    let mut gr: libc::group = unsafe { std::mem::zeroed() };
    lookup(|buf, len, res| unsafe { libc::getgrnam_r(c.as_ptr(), &mut gr, buf, len, res) })?;
    Some(gr.gr_gid)
}

/// Sender side: the ids already named to the receiver.
#[derive(Default)]
pub struct Sent {
    users: HashSet<u32>,
    groups: HashSet<u32>,
}

impl Sent {
    /// Names for the ids in `metas` that were not sent yet, id 0's included (as a modern
    /// rsync sends it): the receiver never maps it by name, but `--usermap` may.
    pub fn new_names<'m>(&mut self, metas: impl Iterator<Item = &'m Meta>, users: bool, groups: bool) -> (Names, Names) {
        let (mut u, mut g) = (Vec::new(), Vec::new());
        for m in metas {
            if users && self.users.insert(m.uid)
                && let Some(n) = user_name(m.uid)
            {
                u.push((m.uid, n));
            }
            if groups && self.groups.insert(m.gid)
                && let Some(n) = group_name(m.gid)
            {
                g.push((m.gid, n));
            }
        }
        (u, g)
    }

    /// Names for the ids of ACL entries (`(is_user, id)`) not sent yet.
    pub fn new_acl_names(&mut self, ids: impl Iterator<Item = (bool, u32)>) -> (Names, Names) {
        let (mut u, mut g) = (Vec::new(), Vec::new());
        for (user, id) in ids {
            if user {
                if self.users.insert(id)
                    && let Some(n) = user_name(id)
                {
                    u.push((id, n));
                }
            } else if self.groups.insert(id)
                && let Some(n) = group_name(id)
            {
                g.push((id, n));
            }
        }
        (u, g)
    }
}

/// Receiver side: sender id -> local id. `--usermap`/`--groupmap` pairs decide first;
/// otherwise a name that exists locally maps to its local id.
#[derive(Default, Debug)]
pub struct IdMap {
    users: Ids,
    groups: Ids,
}

/// One side of an [`IdMap`].
#[derive(Default, Debug)]
struct Ids {
    /// The sender's names, by id (only kept for `map`).
    names: HashMap<u32, Vec<u8>>,
    /// Sender id -> local id, for the names that exist locally (never id 0).
    by_name: HashMap<u32, u32>,
    /// `--usermap` or `--groupmap`.
    map: Option<NameMap>,
}

impl Ids {
    fn add(&mut self, names: &Names, local_id: impl Fn(&[u8]) -> Option<u32>) {
        for (id, name) in names {
            if self.map.is_some() {
                self.names.insert(*id, name.clone());
            }
            if *id != 0
                && let Some(local) = local_id(name)
            {
                self.by_name.insert(*id, local);
            }
        }
    }

    fn get(&self, id: u32) -> u32 {
        if let Some(map) = &self.map {
            let name = self.names.get(&id).map_or(&[][..], Vec::as_slice);
            if let Some(to) = map.get(id, name) {
                return to;
            }
        }
        self.by_name.get(&id).copied().unwrap_or(id)
    }

    fn is_identity(&self) -> bool {
        self.by_name.is_empty() && self.map.is_none()
    }
}

impl IdMap {
    /// An id map applying `--usermap` (`users`) and `--groupmap` (`groups`) pairs.
    pub fn with_maps(users: Option<NameMap>, groups: Option<NameMap>) -> IdMap {
        IdMap { users: Ids { map: users, ..Ids::default() }, groups: Ids { map: groups, ..Ids::default() } }
    }

    pub fn add(&mut self, users: &Names, groups: &Names) {
        self.add_with(users, groups, user_id, group_id);
    }

    pub fn add_with(
        &mut self,
        users: &Names,
        groups: &Names,
        user_id: impl Fn(&[u8]) -> Option<u32>,
        group_id: impl Fn(&[u8]) -> Option<u32>,
    ) {
        self.users.add(users, user_id);
        self.groups.add(groups, group_id);
    }

    /// The receiver's map for `opts`: its `--usermap`/`--groupmap` pairs, with the `TO`
    /// names this system doesn't know reported through `unknown`.
    pub fn for_receiver(opts: &crate::cli::Options, unknown: &mut dyn FnMut(String)) -> Result<IdMap, String> {
        let users = opts.usermap.as_deref().map(|m| NameMap::local(m, true, unknown)).transpose()?;
        let groups = opts.groupmap.as_deref().map(|m| NameMap::local(m, false, unknown)).transpose()?;
        Ok(IdMap::with_maps(users, groups))
    }

    /// Local transfers: a source id mapped. The sender's names are this system's, looked
    /// up as ids come up; with `--numeric-ids` (`named` false) ids are nameless, as rsync
    /// sends no names then.
    pub fn map_local(&mut self, user: bool, id: u32, named: bool) -> u32 {
        let ids = if user { &mut self.users } else { &mut self.groups };
        if named && ids.map.is_some() && !ids.names.contains_key(&id) {
            let name = if user { user_name(id) } else { group_name(id) };
            ids.names.insert(id, name.unwrap_or_default());
        }
        ids.get(id)
    }

    /// Whether every id maps to itself.
    pub fn is_empty(&self) -> bool {
        self.users.is_identity() && self.groups.is_identity()
    }

    /// A sender's user (`user`) or group id, mapped.
    pub fn map(&self, user: bool, id: u32) -> u32 {
        if user { self.users.get(id) } else { self.groups.get(id) }
    }

    pub fn apply(&self, m: &mut Meta) {
        m.uid = self.users.get(m.uid);
        m.gid = self.groups.get(m.gid);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn current_user_round_trips() {
        let uid = unsafe { libc::getuid() } as u32;
        let gid = unsafe { libc::getgid() } as u32;
        let name = user_name(uid).expect("current user has a name");
        assert_eq!(user_id(&name), Some(uid));
        let g = group_name(gid).expect("current group has a name");
        assert_eq!(group_id(&g), Some(gid));
        assert_eq!(user_id(b"no-such-user-barqsync"), None);
    }

    #[test]
    fn maps_known_names_only() {
        let mut m = IdMap::default();
        let users = vec![(1000, b"alice".to_vec()), (1001, b"ghost".to_vec())];
        let groups = vec![(2000, b"staff".to_vec())];
        m.add_with(&users, &groups, |n| (n == b"alice").then_some(501), |n| (n == b"staff").then_some(20));
        let mut meta = crate::entry::Meta {
            kind: crate::entry::Kind::File,
            size: 0,
            mtime: 0,
            mtime_ns: 0,
            mode: 0o644,
            uid: 1000,
            gid: 2000,
            dev: 0,
            ino: 0,
            rdev: 0,
            nlink: 1,
            atime: 0,
            crtime: 0,
        };
        m.apply(&mut meta);
        assert_eq!((meta.uid, meta.gid), (501, 20));
        meta.uid = 1001;
        m.apply(&mut meta);
        assert_eq!(meta.uid, 1001, "unknown names keep their number");
    }

    fn map(s: &str) -> NameMap {
        let mut look = |n: &str| match n {
            "staff" => Some(20),
            "wheel" => Some(0),
            _ => None,
        };
        NameMap::parse(s, false, Some(&mut look), &mut |m| panic!("{m}")).unwrap()
    }

    #[test]
    fn name_maps_match_like_rsync() {
        // The first matching pair decides: ids, ranges, names, wildcards.
        let m = map("0:staff,10-19:7,admin:wheel,ad*:99,*:staff");
        assert_eq!(m.get(0, b"root"), Some(20));
        assert_eq!((m.get(15, b"x"), m.get(19, b"x"), m.get(20, b"x")), (Some(7), Some(7), Some(20)));
        assert_eq!((m.get(80, b"admin"), m.get(81, b"adm")), (Some(0), Some(99)));
        assert_eq!(m.get(5000, b""), Some(20), "a nameless id matches `*`");
        let m = map(":wheel,5-0:staff");
        assert_eq!((m.get(9, b""), m.get(9, b"x")), (Some(0), None), "an empty FROM matches nameless ids");
        assert_eq!(m.get(5, b"x"), Some(20), "a range ending at 0 is one id, as in rsync");
        let m = map("w[h]eel:staff,?:wheel");
        assert_eq!((m.get(1, b"wheel"), m.get(1, b"q"), m.get(1, b"qq")), (Some(20), Some(0), None));
    }

    #[test]
    fn name_map_errors_like_rsync() {
        let parse = |s: &str| NameMap::parse(s, true, None, &mut |_| {});
        assert_eq!(parse("x").unwrap_err(), "No colon found in --usermap: x");
        assert_eq!(parse("x:").unwrap_err(), "No name found after colon --usermap: x:");
        assert_eq!(parse("1a:x").unwrap_err(), "Invalid number in --usermap: 1a");
        assert_eq!(parse("1-2-3:x").unwrap_err(), "Invalid number in --usermap: 1-2-3");
        assert_eq!(parse("99999999999:x").unwrap_err(), "Invalid ID number: 99999999999");
        // Read from the end, as rsync does: the last pair's error comes first.
        assert_eq!(parse("y,x:").unwrap_err(), "No name found after colon --usermap: x:");
        // Unknown names are reported and left out; numbers need no lookup.
        let mut unknown = Vec::new();
        let mut look = |_: &str| None;
        let m = NameMap::parse("a:ghost,b:42", false, Some(&mut look), &mut |e| unknown.push(e)).unwrap();
        assert_eq!(unknown, ["Unknown --groupmap name on receiver: ghost"]);
        assert_eq!((m.get(1, b"a"), m.get(1, b"b")), (None, Some(42)));
    }

    #[test]
    fn maps_come_before_names_and_id_0_keeps_its_number() {
        let mut m = IdMap::with_maps(None, Some(map("wheel:staff")));
        let groups = vec![(0, b"wheel".to_vec()), (30, b"staff".to_vec())];
        m.add_with(&Vec::new(), &groups, |_| None, |n| (n == b"staff").then_some(20).or((n == b"wheel").then_some(10)));
        assert_eq!(m.map(false, 0), 20, "id 0 matched by its name");
        assert_eq!(m.map(false, 30), 20, "mapped by name");
        let mut plain = IdMap::default();
        plain.add_with(&Vec::new(), &groups, |_| None, |n| (n == b"wheel").then_some(10));
        assert_eq!(plain.map(false, 0), 0, "id 0 is never mapped by name");
        assert!(plain.is_empty());
    }
}

//! User and group mapping by name for remote transfers (rsync's default; turned off by
//! `--numeric-ids`). The sender names each uid/gid it sends once; the receiver maps
//! names it knows to its local ids and keeps the number otherwise. Id 0 is never
//! mapped, as in rsync.

use crate::entry::Meta;
use std::collections::{HashMap, HashSet};
use std::ffi::{CStr, CString};

pub type Names = Vec<(u32, Vec<u8>)>;

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
    /// Names for the ids in `metas` that were not sent yet.
    pub fn new_names<'m>(&mut self, metas: impl Iterator<Item = &'m Meta>, users: bool, groups: bool) -> (Names, Names) {
        let (mut u, mut g) = (Vec::new(), Vec::new());
        for m in metas {
            if users && m.uid != 0 && self.users.insert(m.uid)
                && let Some(n) = user_name(m.uid)
            {
                u.push((m.uid, n));
            }
            if groups && m.gid != 0 && self.groups.insert(m.gid)
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
            if id == 0 {
                continue;
            }
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

/// Receiver side: sender id -> local id, for the names that exist locally.
#[derive(Default, Debug)]
pub struct IdMap {
    users: HashMap<u32, u32>,
    groups: HashMap<u32, u32>,
}

impl IdMap {
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
        for (id, name) in users {
            if let Some(local) = user_id(name) {
                self.users.insert(*id, local);
            }
        }
        for (id, name) in groups {
            if let Some(local) = group_id(name) {
                self.groups.insert(*id, local);
            }
        }
    }

    pub fn is_empty(&self) -> bool {
        self.users.is_empty() && self.groups.is_empty()
    }

    /// A sender's user (`user`) or group id, mapped.
    pub fn map(&self, user: bool, id: u32) -> u32 {
        let map = if user { &self.users } else { &self.groups };
        map.get(&id).copied().unwrap_or(id)
    }

    pub fn apply(&self, m: &mut Meta) {
        if let Some(&u) = self.users.get(&m.uid) {
            m.uid = u;
        }
        if let Some(&g) = self.groups.get(&m.gid) {
            m.gid = g;
        }
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
        };
        m.apply(&mut meta);
        assert_eq!((meta.uid, meta.gid), (501, 20));
        meta.uid = 1001;
        m.apply(&mut meta);
        assert_eq!(meta.uid, 1001, "unknown names keep their number");
    }
}

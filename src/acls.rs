//! `-A/--acls`: access control lists, modelled and compared the way rsync's acls.c does.
//!
//! An ACL is held as rsync holds it ([`RsyncAcl`]): the owner, owning-group, mask and
//! other entries (POSIX only) and the named user/group entries in order. macOS ACLs
//! have only named entries; each packs allow/deny, the inheritance flags and the
//! permissions into one number exactly as rsync's sysacls.c does (so, like rsync, the
//! `synchronize` permission and ACL-wide flags are not carried).
//!
//! The sender leaves out what the mode already says ([`Acls::condensed`], rsync's
//! `rsync_acl_strip_perms`), the receiver puts the mask back ([`Acls::received`],
//! `recv_rsync_acl`), compares with [`differs`] (`rsync_acl_equal_enough` for the access
//! ACL, plain equality for a directory's default ACL), and builds what it writes from the
//! target mode ([`write`], `pack_smb_acl` + `change_sacl_perms`).
//!
//! On Linux the ACLs are read and written as the kernel's `system.posix_acl_access` /
//! `system.posix_acl_default` attributes; on macOS through `acl_get_link_np` /
//! `acl_set_link_np` (`ACL_TYPE_EXTENDED`), with ids mapped to and from UUIDs.

use crate::entry::Kind;
use serde::{Deserialize, Serialize};
use std::io;
use std::path::Path;

/// Which system an ACL comes from: the two models don't translate into each other.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[derive(rkyv::Archive, rkyv::Serialize, rkyv::Deserialize)]
pub enum Flavor {
    Posix,
    Darwin,
}

impl Flavor {
    pub fn native() -> Flavor {
        if cfg!(target_os = "macos") { Flavor::Darwin } else { Flavor::Posix }
    }
}

/// A named entry: a user or group id and its access bits (POSIX: `rwx`; macOS: rsync's
/// packing of allow, flags and permissions).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[derive(rkyv::Archive, rkyv::Serialize, rkyv::Deserialize)]
pub struct AclName {
    pub id: u32,
    pub user: bool,
    pub access: u32,
}

/// One ACL. `None` object entries are absent (or, after condensing, implied by the mode).
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[derive(rkyv::Archive, rkyv::Serialize, rkyv::Deserialize)]
pub struct RsyncAcl {
    pub user_obj: Option<u8>,
    pub group_obj: Option<u8>,
    pub mask_obj: Option<u8>,
    pub other_obj: Option<u8>,
    pub names: Vec<AclName>,
}

impl RsyncAcl {
    /// The ACL a mode alone implies (no ACL stored, or ACLs unsupported).
    #[cfg_attr(target_os = "macos", allow(dead_code))]
    fn from_mode(mode: u32) -> RsyncAcl {
        RsyncAcl {
            user_obj: Some((mode >> 6 & 7) as u8),
            group_obj: Some((mode >> 3 & 7) as u8),
            other_obj: Some((mode & 7) as u8),
            ..RsyncAcl::default()
        }
    }

    fn is_empty(&self) -> bool {
        *self == RsyncAcl::default()
    }
}

/// An entry's ACLs: the access ACL and, for directories, the default ACL (POSIX; empty
/// when there is none, and always on macOS).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[derive(rkyv::Archive, rkyv::Serialize, rkyv::Deserialize)]
pub struct Acls {
    pub flavor: Flavor,
    pub access: RsyncAcl,
    pub default: RsyncAcl,
}

impl Acls {
    /// What a plain file sends: nothing beyond its mode.
    pub fn empty() -> Acls {
        Acls { flavor: Flavor::native(), access: RsyncAcl::default(), default: RsyncAcl::default() }
    }

    /// The sender's form (`rsync_acl_strip_perms`): entries the mode reproduces are left
    /// out. `None` when nothing is left, i.e. the entry has no ACL beyond its mode.
    pub fn condensed(mut self, mode: u32) -> Option<Acls> {
        let a = &mut self.access;
        a.user_obj = None;
        if a.mask_obj.is_none() {
            a.group_obj = None;
        } else {
            let group = (mode >> 3 & 7) as u8;
            if a.group_obj == Some(group) {
                a.group_obj = None;
            }
            if !a.names.is_empty() && a.mask_obj == Some(group) {
                a.mask_obj = None;
            }
        }
        a.other_obj = None;
        (!(self.access.is_empty() && self.default.is_empty())).then_some(self)
    }

    /// The receiver's form of a condensed ACL for an entry with `mode`: a POSIX ACL with
    /// named entries gets its mask back (`recv_rsync_acl`); macOS has none.
    pub fn received(&mut self, mode: u32) {
        if self.flavor == Flavor::Darwin {
            self.access.mask_obj = None;
            self.default.mask_obj = None;
            return;
        }
        let a = &mut self.access;
        if !a.names.is_empty() && a.mask_obj.is_none() {
            a.mask_obj = Some((mode >> 3 & 7) as u8);
        }
        let d = &mut self.default;
        if !d.names.is_empty() && d.mask_obj.is_none() {
            let bits = d.names.iter().fold(0u32, |m, n| m | n.access) as u8 | d.group_obj.unwrap_or(0);
            d.mask_obj = Some(bits & 7);
        }
    }

    /// Map the named entries' ids (remote transfers map them by name, like owners).
    pub fn map_ids(&mut self, f: impl Fn(bool, u32) -> u32) {
        for n in self.access.names.iter_mut().chain(self.default.names.iter_mut()) {
            n.id = f(n.user, n.id);
        }
    }

    /// The ids named in the entries, as `(is_user, id)`.
    pub fn ids(&self) -> impl Iterator<Item = (bool, u32)> + '_ {
        self.access.names.iter().chain(&self.default.names).map(|n| (n.user, n.id))
    }
}

/// `rsync_acl_equal_enough`: do the extended parts of the receiver's full access ACL
/// (`have`) match the received one (`want`) for a file of mode `mode`? The object
/// entries are the mode's business.
fn equal_enough(have: &RsyncAcl, want: &RsyncAcl, mode: u32) -> bool {
    if have.mask_obj.is_some() != want.mask_obj.is_some() {
        return false;
    }
    if have.mask_obj.is_some() {
        // A condensed ACL leaves out a group entry equal to the mode's group bits.
        let group = want.group_obj.unwrap_or((mode >> 3 & 7) as u8);
        if have.group_obj != Some(group) {
            return false;
        }
    }
    have.names == want.names
}

/// Would the receiver change `have` (read with [`read`]) to `want` (received; `None`: no
/// ACL)? ACLs from the other system's model are never compared.
pub fn differs(have: &Acls, want: Option<&Acls>, mode: u32, is_dir: bool) -> bool {
    let empty = Acls::empty();
    let want = want.unwrap_or(&empty);
    if want.flavor != have.flavor {
        return false;
    }
    !equal_enough(&have.access, &want.access, mode) || (is_dir && have.default != want.default)
}

/// Does rsync keep ACLs for this kind of entry? Not for symlinks; specials and devices
/// only when they are transferred at all.
pub fn kind_has_acls(kind: Kind) -> bool {
    kind != Kind::Symlink
}

/// The ACLs of `path` (an entry of `kind` and `mode`) as the system has them.
pub fn read(path: &Path, kind: Kind, mode: u32) -> io::Result<Acls> {
    sys::read(path, kind == Kind::Dir, mode)
}

/// Give `path` the received ACLs `want` (`None`: none) where they differ from `have`.
/// The owner, group-or-mask and other entries are built from `mode` (and the received
/// owning-group entry), so the mode doesn't change unless it differs from `mode`.
/// Returns whether anything was written.
pub fn write(path: &Path, kind: Kind, want: Option<&Acls>, have: &Acls, mode: u32) -> io::Result<bool> {
    let empty = Acls::empty();
    let want = want.unwrap_or(&empty);
    if want.flavor != Flavor::native() {
        return Ok(false);
    }
    let mut changed = false;
    if !equal_enough(&have.access, &want.access, mode) {
        sys::write_access(path, &want.access, mode)?;
        changed = true;
    }
    if kind == Kind::Dir && have.default != want.default {
        sys::write_default(path, &want.default)?;
        changed = true;
    }
    Ok(changed)
}

// ---------------------------------------------------------------------------
// Linux: POSIX ACLs as the kernel's xattr encoding
// ---------------------------------------------------------------------------

/// `struct posix_acl_xattr_entry` tags.
#[cfg(any(target_os = "linux", test))]
mod posix {
    use super::{AclName, RsyncAcl};

    #[cfg_attr(not(target_os = "linux"), allow(dead_code))]
    pub const ACCESS: &[u8] = b"system.posix_acl_access";
    #[cfg_attr(not(target_os = "linux"), allow(dead_code))]
    pub const DEFAULT: &[u8] = b"system.posix_acl_default";
    const VERSION: u32 = 2;
    const USER_OBJ: u16 = 0x01;
    const USER: u16 = 0x02;
    const GROUP_OBJ: u16 = 0x04;
    const GROUP: u16 = 0x08;
    const MASK: u16 = 0x10;
    const OTHER: u16 = 0x20;
    const UNDEFINED_ID: u32 = u32::MAX;

    /// Decode the kernel's `system.posix_acl_*` value.
    pub fn decode(v: &[u8]) -> Option<RsyncAcl> {
        if v.len() < 4 || (v.len() - 4) % 8 != 0 || u32::from_le_bytes(v[..4].try_into().ok()?) != VERSION {
            return None;
        }
        let mut acl = RsyncAcl::default();
        for e in v[4..].chunks_exact(8) {
            let tag = u16::from_le_bytes([e[0], e[1]]);
            let perm = (u16::from_le_bytes([e[2], e[3]]) & 7) as u8;
            let id = u32::from_le_bytes([e[4], e[5], e[6], e[7]]);
            match tag {
                USER_OBJ => acl.user_obj = Some(perm),
                GROUP_OBJ => acl.group_obj = Some(perm),
                MASK => acl.mask_obj = Some(perm),
                OTHER => acl.other_obj = Some(perm),
                USER | GROUP => acl.names.push(AclName { id, user: tag == USER, access: perm as u32 }),
                _ => return None,
            }
        }
        Some(acl)
    }

    /// Encode an ACL with every object entry given, in the order the kernel requires
    /// (named users and groups sorted by id, as libacl keeps them).
    pub fn encode(user_obj: u8, group_obj: u8, mask: Option<u8>, other: u8, names: &[AclName]) -> Vec<u8> {
        let mut users: Vec<&AclName> = names.iter().filter(|n| n.user).collect();
        let mut groups: Vec<&AclName> = names.iter().filter(|n| !n.user).collect();
        users.sort_by_key(|n| n.id);
        groups.sort_by_key(|n| n.id);
        let mut out = VERSION.to_le_bytes().to_vec();
        let mut push = |tag: u16, perm: u32, id: u32| {
            out.extend_from_slice(&tag.to_le_bytes());
            out.extend_from_slice(&((perm & 7) as u16).to_le_bytes());
            out.extend_from_slice(&id.to_le_bytes());
        };
        push(USER_OBJ, user_obj as u32, UNDEFINED_ID);
        for n in users {
            push(USER, n.access, n.id);
        }
        push(GROUP_OBJ, group_obj as u32, UNDEFINED_ID);
        for n in groups {
            push(GROUP, n.access, n.id);
        }
        if let Some(m) = mask {
            push(MASK, m as u32, UNDEFINED_ID);
        }
        push(OTHER, other as u32, UNDEFINED_ID);
        out
    }

    /// The access ACL to write for `want` on an entry of `mode` (`pack_smb_acl` +
    /// `change_sacl_perms`): owner, mask and other bits come from the mode; the owning
    /// group from the ACL when it carries one, else from the mode.
    pub fn access_value(want: &RsyncAcl, mode: u32) -> Vec<u8> {
        let group_bits = (mode >> 3 & 7) as u8;
        let mask = (!want.names.is_empty() || want.mask_obj.is_some()).then_some(group_bits);
        encode((mode >> 6 & 7) as u8, want.group_obj.unwrap_or(group_bits), mask, (mode & 7) as u8, &want.names)
    }

    /// The default ACL to write, or `None` to remove it.
    pub fn default_value(want: &RsyncAcl) -> Option<Vec<u8>> {
        let user_obj = want.user_obj?;
        let group_obj = want.group_obj.unwrap_or(0);
        let mask = want.mask_obj.or_else(|| (!want.names.is_empty()).then_some(group_obj));
        Some(encode(user_obj, group_obj, mask, want.other_obj.unwrap_or(0), &want.names))
    }
}

#[cfg(target_os = "linux")]
mod sys {
    use super::{Acls, Flavor, RsyncAcl, posix};
    use crate::fsops::xattr;
    use std::io;
    use std::path::Path;

    fn get(path: &Path, name: &[u8]) -> io::Result<Option<RsyncAcl>> {
        match xattr::get(path, name) {
            Ok(Some(v)) => posix::decode(&v)
                .map(Some)
                .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidData, "malformed POSIX ACL")),
            Ok(None) => Ok(None),
            Err(e) if xattr::is_unsupported(&e) => Ok(None),
            Err(e) => Err(e),
        }
    }

    pub fn read(path: &Path, is_dir: bool, mode: u32) -> io::Result<Acls> {
        let access = get(path, posix::ACCESS)?.unwrap_or_else(|| RsyncAcl::from_mode(mode));
        let default = if is_dir { get(path, posix::DEFAULT)?.unwrap_or_default() } else { RsyncAcl::default() };
        Ok(Acls { flavor: Flavor::Posix, access, default })
    }

    pub fn write_access(path: &Path, want: &RsyncAcl, mode: u32) -> io::Result<()> {
        xattr::set(path, posix::ACCESS, &posix::access_value(want, mode))
    }

    pub fn write_default(path: &Path, want: &RsyncAcl) -> io::Result<()> {
        match posix::default_value(want) {
            Some(v) => xattr::set(path, posix::DEFAULT, &v),
            None => xattr::remove(path, posix::DEFAULT),
        }
    }
}

// ---------------------------------------------------------------------------
// macOS: extended ACLs through the acl(3) API
// ---------------------------------------------------------------------------

#[cfg(target_os = "macos")]
mod sys {
    use super::{AclName, Acls, Flavor, RsyncAcl};
    use crate::fsops::cstr;
    use std::ffi::c_void;
    use std::io;
    use std::os::raw::{c_char, c_int};
    use std::path::Path;

    type Acl = *mut c_void;
    type Entry = *mut c_void;
    type Set = *mut c_void;

    const ACL_TYPE_EXTENDED: c_int = 0x100;
    const ACL_FIRST_ENTRY: c_int = 0;
    const ACL_NEXT_ENTRY: c_int = -1;
    const ACL_EXTENDED_ALLOW: c_int = 1;
    const ACL_EXTENDED_DENY: c_int = 2;
    const ID_TYPE_UID: c_int = 0;

    unsafe extern "C" {
        fn acl_get_link_np(path: *const c_char, ty: c_int) -> Acl;
        fn acl_set_link_np(path: *const c_char, ty: c_int, acl: Acl) -> c_int;
        fn acl_init(count: c_int) -> Acl;
        fn acl_free(obj: *mut c_void) -> c_int;
        fn acl_get_entry(acl: Acl, id: c_int, entry: *mut Entry) -> c_int;
        fn acl_create_entry(acl: *mut Acl, entry: *mut Entry) -> c_int;
        fn acl_get_tag_type(e: Entry, tag: *mut c_int) -> c_int;
        fn acl_set_tag_type(e: Entry, tag: c_int) -> c_int;
        fn acl_get_flagset_np(obj: *mut c_void, fs: *mut Set) -> c_int;
        fn acl_set_flagset_np(obj: *mut c_void, fs: Set) -> c_int;
        fn acl_get_flag_np(fs: Set, flag: c_int) -> c_int;
        fn acl_add_flag_np(fs: Set, flag: c_int) -> c_int;
        fn acl_clear_flags_np(fs: Set) -> c_int;
        fn acl_get_permset(e: Entry, ps: *mut Set) -> c_int;
        fn acl_set_permset(e: Entry, ps: Set) -> c_int;
        fn acl_get_perm_np(ps: Set, perm: c_int) -> c_int;
        fn acl_add_perm(ps: Set, perm: c_int) -> c_int;
        fn acl_clear_perms(ps: Set) -> c_int;
        fn acl_get_qualifier(e: Entry) -> *mut c_void;
        fn acl_set_qualifier(e: Entry, q: *const c_void) -> c_int;
        fn mbr_uuid_to_id(uu: *const u8, id: *mut u32, id_type: *mut c_int) -> c_int;
        fn mbr_uid_to_uuid(uid: u32, uu: *mut u8) -> c_int;
        fn mbr_gid_to_uuid(gid: u32, uu: *mut u8) -> c_int;
    }

    /// rsync's packing (sysacls.c, OS X): bit 0 allow; entry flags `1<<4 ..` in bits
    /// 1-11; permissions `1<<1 ..` in bits 12-30.
    const FLAGS: std::ops::Range<u32> = 0..11;
    const PERMS: std::ops::Range<u32> = 0..19;

    struct Owned(Acl);
    impl Drop for Owned {
        fn drop(&mut self) {
            unsafe { acl_free(self.0) };
        }
    }

    fn check(r: c_int, what: &str) -> io::Result<()> {
        if r == 0 {
            return Ok(());
        }
        let e = io::Error::last_os_error();
        Err(io::Error::new(e.kind(), format!("{what}: {e}")))
    }

    pub fn read(path: &Path, _is_dir: bool, _mode: u32) -> io::Result<Acls> {
        let c = cstr(path)?;
        let acl = unsafe { acl_get_link_np(c.as_ptr(), ACL_TYPE_EXTENDED) };
        let mut access = RsyncAcl::default();
        if acl.is_null() {
            let e = io::Error::last_os_error();
            // ENOENT: no ACL; the others: none possible here (rsync's no_acl_syscall_error).
            return match e.raw_os_error() {
                Some(libc::ENOENT | libc::ENOTSUP | libc::EINVAL | libc::ENOSYS) => {
                    Ok(Acls { flavor: Flavor::Darwin, access, default: RsyncAcl::default() })
                }
                _ => Err(e),
            };
        }
        let acl = Owned(acl);
        let mut entry: Entry = std::ptr::null_mut();
        let mut which = ACL_FIRST_ENTRY;
        while unsafe { acl_get_entry(acl.0, which, &mut entry) } == 0 {
            which = ACL_NEXT_ENTRY;
            let (mut tag, mut flags, mut perms): (c_int, Set, Set) = (0, std::ptr::null_mut(), std::ptr::null_mut());
            check(unsafe { acl_get_tag_type(entry, &mut tag) }, "acl_get_tag_type")?;
            check(unsafe { acl_get_flagset_np(entry, &mut flags) }, "acl_get_flagset_np")?;
            check(unsafe { acl_get_permset(entry, &mut perms) }, "acl_get_permset")?;
            let q = unsafe { acl_get_qualifier(entry) };
            if q.is_null() {
                return Err(io::Error::other(format!("acl_get_qualifier: {}", io::Error::last_os_error())));
            }
            let (mut id, mut id_type) = (0u32, -1 as c_int);
            let rc = unsafe { mbr_uuid_to_id(q.cast(), &mut id, &mut id_type) };
            unsafe { acl_free(q) };
            if rc != 0 {
                return Err(io::Error::other(format!("mbr_uuid_to_id: {}", io::Error::from_raw_os_error(rc))));
            }
            let mut bits = u32::from(tag == ACL_EXTENDED_ALLOW);
            for i in FLAGS {
                if unsafe { acl_get_flag_np(flags, 1 << (4 + i)) } == 1 {
                    bits |= 1 << (1 + i);
                }
            }
            for i in PERMS {
                if unsafe { acl_get_perm_np(perms, 1 << (1 + i)) } == 1 {
                    bits |= 1 << (12 + i);
                }
            }
            access.names.push(AclName { id, user: id_type == ID_TYPE_UID, access: bits });
        }
        Ok(Acls { flavor: Flavor::Darwin, access, default: RsyncAcl::default() })
    }

    pub fn write_access(path: &Path, want: &RsyncAcl, _mode: u32) -> io::Result<()> {
        let c = cstr(path)?;
        let mut acl = unsafe { acl_init(want.names.len() as c_int) };
        if acl.is_null() {
            return Err(io::Error::last_os_error());
        }
        // `acl_create_entry` may move the ACL, so it is freed by hand afterwards.
        let r = (|| {
            for n in &want.names {
                add_entry(&mut acl, n)?;
            }
            if unsafe { acl_set_link_np(c.as_ptr(), ACL_TYPE_EXTENDED, acl) } != 0 {
                return Err(io::Error::last_os_error());
            }
            Ok(())
        })();
        unsafe { acl_free(acl) };
        r
    }

    fn add_entry(acl: &mut Acl, n: &AclName) -> io::Result<()> {
        let mut entry: Entry = std::ptr::null_mut();
        check(unsafe { acl_create_entry(acl, &mut entry) }, "acl_create_entry")?;
        let (mut flags, mut perms): (Set, Set) = (std::ptr::null_mut(), std::ptr::null_mut());
        check(unsafe { acl_get_flagset_np(entry, &mut flags) }, "acl_get_flagset_np")?;
        check(unsafe { acl_get_permset(entry, &mut perms) }, "acl_get_permset")?;
        unsafe {
            acl_clear_flags_np(flags);
            acl_clear_perms(perms);
        }
        for i in FLAGS {
            if n.access & 1 << (1 + i) != 0 {
                unsafe { acl_add_flag_np(flags, 1 << (4 + i)) };
            }
        }
        for i in PERMS {
            if n.access & 1 << (12 + i) != 0 {
                unsafe { acl_add_perm(perms, 1 << (1 + i)) };
            }
        }
        let mut uu = [0u8; 16];
        let rc = unsafe { if n.user { mbr_uid_to_uuid(n.id, uu.as_mut_ptr()) } else { mbr_gid_to_uuid(n.id, uu.as_mut_ptr()) } };
        if rc != 0 {
            return Err(io::Error::other(format!("mbr_id_to_uuid: {}", io::Error::from_raw_os_error(rc))));
        }
        let tag = if n.access & 1 != 0 { ACL_EXTENDED_ALLOW } else { ACL_EXTENDED_DENY };
        check(unsafe { acl_set_tag_type(entry, tag) }, "acl_set_tag_type")?;
        check(unsafe { acl_set_qualifier(entry, uu.as_ptr().cast()) }, "acl_set_qualifier")?;
        check(unsafe { acl_set_permset(entry, perms) }, "acl_set_permset")?;
        check(unsafe { acl_set_flagset_np(entry, flags) }, "acl_set_flagset_np")
    }

    /// macOS has no default ACLs (inheritance is part of the entries' flags).
    pub fn write_default(_path: &Path, _want: &RsyncAcl) -> io::Result<()> {
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn name(id: u32, user: bool, access: u32) -> AclName {
        AclName { id, user, access }
    }

    /// A POSIX file with named entries: what the mode says is dropped, the mask comes
    /// back on the receiver, and the result compares equal to the original.
    #[test]
    fn condense_and_receive_round_trip_posix() {
        let full = RsyncAcl {
            user_obj: Some(6),
            group_obj: Some(4),
            mask_obj: Some(6),
            other_obj: Some(4),
            names: vec![name(1000, true, 6), name(50, false, 4)],
        };
        let mode = 0o664; // group bits = mask
        let acls = Acls { flavor: Flavor::Posix, access: full.clone(), default: RsyncAcl::default() };
        let mut sent = acls.condensed(mode).expect("named entries survive");
        assert_eq!(sent.access.user_obj, None);
        assert_eq!(sent.access.mask_obj, None, "mask equal to the group bits is implied");
        assert_eq!(sent.access.group_obj, Some(4), "owning group differs from the mask");
        sent.received(mode);
        assert_eq!(sent.access.mask_obj, Some(6));
        let have = Acls { flavor: Flavor::Posix, access: full, default: RsyncAcl::default() };
        assert!(!differs(&have, Some(&sent), mode, false));
        let mut other = sent.clone();
        other.access.names[0].access = 7;
        assert!(differs(&have, Some(&other), mode, false));
    }

    #[test]
    fn plain_modes_have_no_acl() {
        let acls = Acls { flavor: Flavor::native(), access: RsyncAcl::from_mode(0o751), default: RsyncAcl::default() };
        assert_eq!(acls.clone().condensed(0o751), None);
        assert!(!differs(&acls, None, 0o751, true));
        let mut with_names = acls.clone();
        with_names.access.mask_obj = Some(5);
        with_names.access.names.push(name(7, false, 5));
        assert!(differs(&with_names, None, 0o751, false), "extended entries on the receiver go away");
    }

    #[test]
    fn default_acls_compare_exactly() {
        let mut have = Acls::empty();
        have.flavor = Flavor::Posix;
        let mut want = have.clone();
        want.default = RsyncAcl { user_obj: Some(7), group_obj: Some(5), mask_obj: None, other_obj: Some(0), names: vec![name(9, true, 5)] };
        want.received(0o755);
        assert_eq!(want.default.mask_obj, Some(5), "mask from the named and group bits");
        assert!(differs(&have, Some(&want), 0o755, true));
        assert!(!differs(&have, Some(&want), 0o755, false), "files have no default ACL");
    }

    #[test]
    fn posix_encoding_round_trips_in_kernel_order() {
        let v = posix::encode(7, 5, Some(7), 0, &[name(2000, false, 7), name(1001, true, 6), name(1000, true, 4)]);
        let back = posix::decode(&v).unwrap();
        assert_eq!(back.user_obj, Some(7));
        assert_eq!(back.mask_obj, Some(7));
        let ids: Vec<(bool, u32)> = back.names.iter().map(|n| (n.user, n.id)).collect();
        assert_eq!(ids, vec![(true, 1000), (true, 1001), (false, 2000)]);
        assert!(posix::decode(&v[..v.len() - 1]).is_none());
        // Access values take the owner, mask and other bits from the mode.
        let want = RsyncAcl { group_obj: Some(4), mask_obj: Some(6), names: vec![name(1000, true, 6)], ..RsyncAcl::default() };
        let a = posix::decode(&posix::access_value(&want, 0o640)).unwrap();
        assert_eq!((a.user_obj, a.group_obj, a.mask_obj, a.other_obj), (Some(6), Some(4), Some(4), Some(0)));
        assert_eq!(posix::default_value(&RsyncAcl::default()), None);
    }

    #[test]
    fn foreign_flavors_are_left_alone() {
        let have = Acls { flavor: Flavor::native(), access: RsyncAcl::default(), default: RsyncAcl::default() };
        let mut want = have.clone();
        want.flavor = if Flavor::native() == Flavor::Posix { Flavor::Darwin } else { Flavor::Posix };
        want.access.names.push(name(1, true, 7));
        assert!(!differs(&have, Some(&want), 0o644, false));
    }

    /// The system's own ACLs survive a read / write / read cycle.
    #[test]
    fn system_acls_round_trip() {
        let tmp = tempfile::tempdir().unwrap();
        let p = tmp.path().join("f");
        std::fs::write(&p, b"x").unwrap();
        let uid = unsafe { libc::getuid() } as u32;
        let have = read(&p, Kind::File, 0o644).unwrap();
        assert!(!differs(&have, None, 0o644, false), "a new file has no ACL: {have:?}");
        let mut want = Acls::empty();
        let access = if cfg!(target_os = "macos") { 1 | 1 << 12 | 1 << 13 } else { 6 };
        want.access.names.push(name(uid, true, access));
        want.received(0o644);
        match write(&p, Kind::File, Some(&want), &have, 0o644) {
            Err(e) if crate::fsops::xattr::is_unsupported(&e) => return, // no ACLs on this filesystem
            r => assert!(r.unwrap()),
        }
        let now = read(&p, Kind::File, 0o644).unwrap();
        assert!(!differs(&now, Some(&want), 0o644, false), "{now:?} vs {want:?}");
        assert!(!write(&p, Kind::File, Some(&want), &now, 0o644).unwrap());
        // And back to none.
        assert!(write(&p, Kind::File, None, &now, 0o644).unwrap());
        let gone = read(&p, Kind::File, 0o644).unwrap();
        assert!(!differs(&gone, None, 0o644, false), "{gone:?}");
    }
}

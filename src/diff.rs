//! Pure decision logic: given source and destination metadata, what needs to happen?

use crate::entry::{Kind, Meta};

/// Option subset that drives the quick check, resolved once per run.
#[derive(Debug, Clone)]
pub struct DiffRules {
    pub perms: bool,
    /// `-E` without `-p`: whether a regular file is executable (any `x` bit) is kept.
    pub executability: bool,
    pub times: bool,
    /// `-U`: access times (not directories').
    pub atimes: bool,
    /// `-N`: creation times.
    pub crtimes: bool,
    /// Owner is only applied when running as root (rsync semantics).
    pub owner: bool,
    /// Group is applied when root, or when the user belongs to the source group.
    pub group: bool,
    pub am_root: bool,
    pub my_groups: Vec<u32>,
    pub update: bool,
    pub size_only: bool,
    pub ignore_times: bool,
    pub ignore_existing: bool,
    pub modify_window: i64,
    /// `-c`: same-size files are compared by checksum instead of mtime.
    pub checksum: bool,
}

impl DiffRules {
    pub fn wanted_uid(&self, src: &Meta) -> Option<u32> {
        (self.owner && self.am_root).then_some(src.uid)
    }

    pub fn wanted_gid(&self, src: &Meta) -> Option<u32> {
        (self.group && (self.am_root || self.my_groups.contains(&src.gid))).then_some(src.gid)
    }

    pub fn same_mtime(&self, a: &Meta, b: &Meta) -> bool {
        if self.modify_window < 0 {
            a.mtime == b.mtime && a.mtime_ns == b.mtime_ns
        } else {
            (a.mtime - b.mtime).abs() <= self.modify_window
        }
    }

    /// Whether the permissions differ as far as `-p` (all bits) or `-E` (whether any `x`
    /// bit is on) care, as rsync's `perms_differ`.
    pub fn perms_differ(&self, src: &Meta, dst: &Meta) -> bool {
        if self.perms {
            src.mode != dst.mode
        } else {
            self.executability && (src.mode & 0o111 != 0) != (dst.mode & 0o111 != 0)
        }
    }

    /// The permissions an existing file keeps when they aren't copied (`-p` off): its own,
    /// with `-E` made executable, or not, like `src` (rsync's `dest_mode`): no `x` bit at
    /// all, or one for each `r`.
    pub fn kept_mode(&self, src: &Meta, dst_mode: u32) -> u32 {
        let mut mode = dst_mode & 0o7777;
        if self.executability && src.kind == Kind::File {
            if src.mode & 0o111 == 0 {
                mode &= !0o111;
            } else if dst_mode & 0o111 == 0 {
                mode |= (mode & 0o444) >> 2;
            }
        }
        mode
    }

    /// Does `dst` already carry the attributes we would apply from `src`?
    pub fn attrs_differ(&self, src: &Meta, dst: &Meta) -> AttrDiff {
        AttrDiff {
            mode: self.perms_differ(src, dst),
            mtime: self.times && !self.same_mtime(src, dst),
            owner: self.wanted_uid(src).is_some_and(|u| u != dst.uid)
                || self.wanted_gid(src).is_some_and(|g| g != dst.gid),
            atime: self.atime_differs(src, dst),
            crtime: self.crtimes && src.crtime != dst.crtime,
            ..AttrDiff::default()
        }
    }

    /// `-U`: the access times differ (in seconds, as rsync compares them); directories'
    /// aren't kept.
    pub fn atime_differs(&self, src: &Meta, dst: &Meta) -> bool {
        self.atimes && src.kind != Kind::Dir && src.atime != dst.atime
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct AttrDiff {
    pub mode: bool,
    pub mtime: bool,
    pub owner: bool,
    /// `-X`: the extended attributes differ.
    pub xattr: bool,
    /// `-A`: the ACLs differ.
    pub acl: bool,
    /// `-U`: the access time differs.
    pub atime: bool,
    /// `-N`: the creation time differs.
    pub crtime: bool,
}

impl AttrDiff {
    pub fn any(&self) -> bool {
        self.mode || self.mtime || self.owner || self.xattr || self.acl || self.atime || self.crtime
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FileAction {
    Skip,
    /// Data must be (re)written.
    Transfer,
    /// Data is identical; only attributes need fixing.
    Attrs(AttrDiff),
}

/// The rsync "quick check" for a regular file. `dst` is `None` when nothing
/// exists at the destination path; a non-file destination always means Transfer.
pub fn file_action(src: &Meta, dst: Option<&Meta>, r: &DiffRules) -> FileAction {
    file_action_sum(src, dst, r, false)
}

/// [`file_action`] with `-c`: `same_sum` says whether the checksums of `src` and `dst`
/// match (only consulted when the sizes do).
pub fn file_action_sum(src: &Meta, dst: Option<&Meta>, r: &DiffRules, same_sum: bool) -> FileAction {
    file_action_ext(src, dst, r, same_sum, &mut || (false, false))
}

/// [`file_action_sum`] with `-X`/`-A`: `ext` says whether the extended attributes and
/// the ACLs differ. It is only asked when the data is the same: differing ones then
/// need an attribute update, as in rsync, never a transfer.
pub fn file_action_ext(
    src: &Meta,
    dst: Option<&Meta>,
    r: &DiffRules,
    same_sum: bool,
    ext: &mut dyn FnMut() -> (bool, bool),
) -> FileAction {
    let Some(dst) = dst else { return FileAction::Transfer };
    if dst.kind != Kind::File {
        return FileAction::Transfer;
    }
    if r.ignore_existing {
        return FileAction::Skip;
    }
    if r.update && dst.mtime > src.mtime && !r.same_mtime(src, dst) {
        return FileAction::Skip;
    }
    let data_same = if r.checksum {
        src.size == dst.size && same_sum
    } else if r.ignore_times {
        false
    } else if r.size_only {
        src.size == dst.size
    } else {
        src.size == dst.size && r.same_mtime(src, dst)
    };
    if !data_same {
        return FileAction::Transfer;
    }
    let (xattr, acl) = ext();
    let diff = AttrDiff { xattr, acl, ..r.attrs_differ(src, dst) };
    if diff.any() { FileAction::Attrs(diff) } else { FileAction::Skip }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rules() -> DiffRules {
        DiffRules {
            perms: true,
            executability: false,
            times: true,
            atimes: false,
            crtimes: false,
            owner: true,
            group: true,
            am_root: false,
            my_groups: vec![20],
            update: false,
            size_only: false,
            ignore_times: false,
            ignore_existing: false,
            modify_window: 0,
            checksum: false,
        }
    }

    fn file(size: u64, mtime: i64, ns: u32, mode: u32) -> Meta {
        Meta {
            kind: Kind::File,
            size,
            mtime,
            mtime_ns: ns,
            mode,
            uid: 501,
            gid: 20,
            dev: 1,
            ino: 1,
            rdev: 0,
            nlink: 1,
            atime: 0,
            crtime: 0,
        }
    }

    #[test]
    fn missing_or_changed_transfers() {
        let r = rules();
        let s = file(10, 100, 0, 0o644);
        assert_eq!(file_action(&s, None, &r), FileAction::Transfer);
        assert_eq!(file_action(&s, Some(&file(11, 100, 0, 0o644)), &r), FileAction::Transfer);
        assert_eq!(file_action(&s, Some(&file(10, 101, 0, 0o644)), &r), FileAction::Transfer);
    }

    #[test]
    fn seconds_only_by_default_nanos_when_negative_window() {
        let mut r = rules();
        let s = file(10, 100, 5, 0o644);
        assert_eq!(file_action(&s, Some(&file(10, 100, 9, 0o644)), &r), FileAction::Skip);
        r.modify_window = -1;
        assert_eq!(file_action(&s, Some(&file(10, 100, 9, 0o644)), &r), FileAction::Transfer);
    }

    #[test]
    fn attrs_only() {
        let r = rules();
        let s = file(10, 100, 0, 0o600);
        match file_action(&s, Some(&file(10, 100, 0, 0o644)), &r) {
            FileAction::Attrs(d) => assert!(d.mode && !d.mtime && !d.owner),
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn xattrs_and_acls_alone_update_attributes() {
        let r = rules();
        let s = file(10, 100, 0, 0o644);
        let d = s;
        let act = |x, a| file_action_ext(&s, Some(&d), &r, false, &mut || (x, a));
        assert_eq!(act(false, false), FileAction::Skip);
        assert_eq!(act(true, false), FileAction::Attrs(AttrDiff { xattr: true, ..AttrDiff::default() }));
        assert_eq!(act(false, true), FileAction::Attrs(AttrDiff { acl: true, ..AttrDiff::default() }));
        // Not asked when the data differs: that is a transfer anyway.
        let changed = file(11, 100, 0, 0o644);
        let mut asked = false;
        let a = file_action_ext(&s, Some(&changed), &r, false, &mut || {
            asked = true;
            (true, true)
        });
        assert_eq!((a, asked), (FileAction::Transfer, false));
    }

    #[test]
    fn update_skips_newer_receiver() {
        let mut r = rules();
        r.update = true;
        let s = file(10, 100, 0, 0o644);
        assert_eq!(file_action(&s, Some(&file(99, 200, 0, 0o644)), &r), FileAction::Skip);
    }

    #[test]
    fn checksum_decides_for_same_size_files() {
        let mut r = rules();
        r.checksum = true;
        let s = file(10, 100, 0, 0o644);
        assert_eq!(file_action_sum(&s, Some(&file(10, 100, 0, 0o644)), &r, false), FileAction::Transfer);
        assert_eq!(file_action_sum(&s, Some(&file(10, 200, 0, 0o644)), &r, true), FileAction::Attrs(AttrDiff { mtime: true, ..AttrDiff::default() }));
        assert_eq!(file_action_sum(&s, Some(&file(11, 100, 0, 0o644)), &r, true), FileAction::Transfer);
    }

    #[test]
    fn owner_ignored_when_not_root() {
        let r = rules();
        let s = file(10, 100, 0, 0o644);
        let mut d = s;
        d.uid = 0;
        assert_eq!(file_action(&s, Some(&d), &r), FileAction::Skip);
    }
}

//! Pure decision logic: given source and destination metadata, what needs to happen?

use crate::entry::{Kind, Meta};

/// Option subset that drives the quick check, resolved once per run.
#[derive(Debug, Clone)]
pub struct DiffRules {
    pub perms: bool,
    pub times: bool,
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

    /// Does `dst` already carry the attributes we would apply from `src`?
    pub fn attrs_differ(&self, src: &Meta, dst: &Meta) -> AttrDiff {
        AttrDiff {
            mode: self.perms && src.mode != dst.mode,
            mtime: self.times && !self.same_mtime(src, dst),
            owner: self.wanted_uid(src).is_some_and(|u| u != dst.uid)
                || self.wanted_gid(src).is_some_and(|g| g != dst.gid),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct AttrDiff {
    pub mode: bool,
    pub mtime: bool,
    pub owner: bool,
}

impl AttrDiff {
    pub fn any(&self) -> bool {
        self.mode || self.mtime || self.owner
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
    let data_same = if r.ignore_times {
        false
    } else if r.size_only {
        src.size == dst.size
    } else {
        src.size == dst.size && r.same_mtime(src, dst)
    };
    if !data_same {
        return FileAction::Transfer;
    }
    let diff = r.attrs_differ(src, dst);
    if diff.any() { FileAction::Attrs(diff) } else { FileAction::Skip }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rules() -> DiffRules {
        DiffRules {
            perms: true,
            times: true,
            owner: true,
            group: true,
            am_root: false,
            my_groups: vec![20],
            update: false,
            size_only: false,
            ignore_times: false,
            ignore_existing: false,
            modify_window: 0,
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
    fn update_skips_newer_receiver() {
        let mut r = rules();
        r.update = true;
        let s = file(10, 100, 0, 0o644);
        assert_eq!(file_action(&s, Some(&file(99, 200, 0, 0o644)), &r), FileAction::Skip);
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

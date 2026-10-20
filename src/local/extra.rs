//! `-X`/`-A` in the engine: reading an entry's extended attributes and ACLs on the
//! sending side, turning them into the receiver's form, comparing them with the
//! destination's and writing them, with rsync's semantics (see [`crate::xattrs`] and
//! [`crate::acls`] for the rules themselves).
//!
//! Like rsync, attributes and ACLs are written after the owner and before the mode:
//! `chown` would drop a file capability set before it, and the mode set last undoes
//! anything an ACL write did to the permission bits.

use super::exec::{Engine, FileJob};
use crate::acls::{self, Acls};
use crate::cli::Options;
use crate::entry::{DirEnt, Extra, Kind, Meta};
use crate::output::path_msg;
use crate::xattrs::{self, Rules, XValue, Xattr};
use std::io;
use std::path::Path;

/// The sending side's rules for one run.
pub fn rules(opts: &Options) -> Rules {
    Rules { level: opts.xattrs, am_root: crate::fsops::am_root() }
}

/// The sender's form of `path`'s attributes and ACLs: wire names, values over
/// [`xattrs::INLINE_MAX`] summed unless `full`, condensed ACLs (for `mode`, the mode as
/// sent). `None` when there is nothing to carry.
pub fn read_wire(opts: &Options, r: &Rules, path: &Path, kind: Kind, mode: u32, full: bool) -> io::Result<Option<Box<Extra>>> {
    let mut x = Extra::default();
    if opts.xattrs > 0 && xattrs::kind_has_xattrs(kind) {
        x.xattrs = xattrs::read_wire(path, r, full)?;
    }
    if opts.acls && acls::kind_has_acls(kind) {
        x.acl = acls::read(path, kind, mode)?.condensed(mode);
    }
    Ok(x.boxed())
}

/// Fill in the sender's form for a listing of `dir`. An entry whose attributes can't be
/// read is left out with an error, as rsync leaves it out of its file list.
pub fn collect(opts: &Options, r: &Rules, dir: &Path, ents: Vec<DirEnt>, err: &mut dyn FnMut(Vec<u8>)) -> Vec<DirEnt> {
    if opts.xattrs == 0 && !opts.acls {
        return ents;
    }
    ents.into_iter()
        .filter_map(|mut e| {
            let p = dir.join(&e.name);
            match read_wire(opts, r, &p, e.meta.kind, e.meta.mode, false) {
                Ok(x) => {
                    e.extra = x;
                    Some(e)
                }
                Err(er) => {
                    err(path_msg("get_xattr/get_acl \"", &p, format!("\" failed: {er}")));
                    None
                }
            }
        })
        .collect()
}

/// Turn a received (sender's form) `extra` into the receiver's: local names, and ACLs
/// with their mask back (`mode`: the entry's mode as sent).
pub fn to_local(r: &Rules, extra: Option<Box<Extra>>, mode: u32) -> Option<Box<Extra>> {
    let mut x = *extra?;
    if !x.xattrs.is_empty() {
        x.xattrs = r.to_local(std::mem::take(&mut x.xattrs));
    }
    if let Some(a) = &mut x.acl {
        a.received(mode);
    }
    x.boxed()
}

/// What the destination entry has now (only what the run preserves).
#[derive(Debug, Default)]
pub(crate) struct DstExtra {
    pub xattrs: Option<Vec<Xattr>>,
    pub acl: Option<Acls>,
}

/// Where full attribute values come from when a summed one must be written.
#[derive(Clone, Copy)]
pub(crate) enum XSource<'a> {
    /// The source entry on this machine.
    Local(&'a Path),
    /// Already at hand (sent along with a file's data, or a root's listing).
    Full(&'a [Xattr]),
    /// Entry `idx` of remote listing `dir`: asked for, and written to `dst`, its final
    /// path, when it arrives (whatever temp file is being set up meanwhile). `rel` in
    /// root `root`: its name for `--remove-source-files`, which waits for the values.
    Remote { dir: u32, idx: u32, root: u16, rel: &'a [u8], dst: &'a Path },
}

/// The source side of an entry, for comparing and writing.
pub(crate) struct Src<'a> {
    pub kind: Kind,
    pub meta: &'a Meta,
    pub extra: Option<&'a Extra>,
    pub from: XSource<'a>,
}

impl Engine<'_> {
    pub(crate) fn want_xattrs(&self, kind: Kind) -> bool {
        self.opts.xattrs > 0 && xattrs::kind_has_xattrs(kind)
    }

    pub(crate) fn want_acls(&self, kind: Kind) -> bool {
        self.opts.acls && acls::kind_has_acls(kind)
    }

    pub(crate) fn extras_on(&self) -> bool {
        self.opts.xattrs > 0 || self.opts.acls
    }

    /// Local transfers: a freshly read listing gets the sender's form of its entries'
    /// attributes, turned straight into the receiver's.
    pub(crate) fn local_listing(&self, dir: &Path, ents: Vec<DirEnt>) -> Vec<DirEnt> {
        if !self.extras_on() {
            return ents;
        }
        let mut ents = collect(self.opts, &self.xrules, dir, ents, &mut |m| self.src_err(m));
        for e in &mut ents {
            e.extra = to_local(&self.xrules, e.extra.take(), e.meta.mode);
        }
        ents
    }

    /// Local transfers: a root directory's attributes in the receiver's form.
    pub(crate) fn local_root_extra(&self, path: &Path, meta: &Meta) -> Option<Box<Extra>> {
        if !self.extras_on() {
            return None;
        }
        match read_wire(self.opts, &self.xrules, path, meta.kind, meta.mode, true) {
            Ok(x) => to_local(&self.xrules, x, meta.mode),
            Err(e) => {
                self.src_err(path_msg("get_xattr/get_acl \"", path, format!("\" failed: {e}")));
                None
            }
        }
    }

    /// Read what the destination entry at `path` has (`mode`: its current mode).
    pub(crate) fn dst_extra(&self, path: &Path, kind: Kind, mode: u32) -> io::Result<DstExtra> {
        let mut d = DstExtra::default();
        if self.want_xattrs(kind) {
            d.xattrs = Some(xattrs::read_local(path, &self.xrules)?);
        }
        if self.want_acls(kind) {
            d.acl = Some(acls::read(path, kind, mode)?);
        }
        Ok(d)
    }

    /// Whether the extended attributes and the ACLs of `have` differ from the source's.
    pub(crate) fn extra_diff(&self, src: &Src, have: &DstExtra) -> (bool, bool) {
        let x = have.xattrs.as_deref().is_some_and(|h| !xattrs::same(src.extra.map_or(&[][..], |e| &e.xattrs), h));
        let a = have.acl.as_ref().is_some_and(|h| {
            acls::differs(h, src.extra.and_then(|e| e.acl.as_ref()), src.meta.mode, src.kind == Kind::Dir)
        });
        (x, a)
    }

    /// Read the destination entry and compare (`(false, false)` when it can't be read,
    /// after reporting why).
    pub(crate) fn extra_change(&self, src: &Src, path: &Path, dst: &Meta) -> ((bool, bool), Option<DstExtra>) {
        if !self.extras_on() {
            return ((false, false), None);
        }
        match self.dst_extra(path, dst.kind, dst.mode) {
            Ok(have) => (self.extra_diff(src, &have), Some(have)),
            Err(e) => {
                self.err(path_msg("get_xattr/get_acl \"", path, format!("\" failed: {e}")));
                ((false, false), None)
            }
        }
    }

    /// Give the entry at `path` the source's attributes and ACLs. `have`: what it has now
    /// (read here when `None`); `acl_mode`: the mode the ACL's owner/group/other entries
    /// are built from (the mode the entry has or is about to get).
    pub(crate) fn apply_extra(&self, path: &Path, src: &Src, have: Option<DstExtra>, acl_mode: u32) -> io::Result<()> {
        if !self.extras_on() {
            return Ok(());
        }
        let have = match have {
            Some(h) => h,
            None => self.dst_extra(path, src.kind, acl_mode)?,
        };
        if let Some(have_x) = &have.xattrs {
            let want = src.extra.map_or(&[][..], |e| &e.xattrs);
            if !xattrs::same(want, have_x) {
                self.apply_xattrs(path, src, want, have_x)?;
            }
        }
        if let Some(have_a) = &have.acl {
            acls::write(path, src.kind, src.extra.and_then(|e| e.acl.as_ref()), have_a, acl_mode)
                .map_err(|e| io::Error::new(e.kind(), format!("set_acl: {e}")))?;
        }
        Ok(())
    }

    fn apply_xattrs(&self, path: &Path, src: &Src, want: &[Xattr], have: &[Xattr]) -> io::Result<()> {
        let fetched;
        let full: Option<&[Xattr]> = if !xattrs::needs_full(want, have) {
            None
        } else {
            match src.from {
                XSource::Full(f) => Some(f),
                XSource::Local(p) => {
                    fetched = self.xrules.to_local(xattrs::read_wire(p, &self.xrules, true)?);
                    Some(&fetched)
                }
                XSource::Remote { dir, idx, root, rel, dst } => {
                    // Written once the sender has sent the values: to the final path,
                    // as a temp file at `path` is renamed before then.
                    if let Some(r) = &self.remote {
                        let hold = self.opts.remove_source_files.then_some((root, rel));
                        r.defer_xattrs(dir, idx, dst, src.kind, hold);
                    }
                    return Ok(());
                }
            }
        };
        xattrs::apply(path, src.kind, want, have, full, &self.xrules).map(drop)
    }

    /// An entry set up with `src`'s attributes wasn't written after all: withdraw any
    /// request for its full values.
    pub(crate) fn not_written(&self, src: &Src) {
        if let (XSource::Remote { dir, idx, .. }, Some(r)) = (src.from, &self.remote) {
            r.forget_xattrs(dir, idx);
        }
    }

    /// `-X`/`-A` for a file this run wrote at `path` (a temp file, or the destination
    /// itself with `--inplace`), before its final mode is set. `mode`: that mode.
    pub(crate) fn written_extra(&self, path: &Path, job: &FileJob, mode: u32) -> io::Result<()> {
        self.apply_extra(path, &job.src(self.remote.is_some()), None, mode)
    }

    /// A copy-on-write clone carries its source's extended attributes: without `-X`,
    /// like rsync's freshly written file, it must not. (Its ACL is the inherited one,
    /// as for a new file; Linux reflinks copy neither.)
    pub(crate) fn cloned(&self, path: &Path) {
        if cfg!(target_os = "macos") && self.opts.xattrs == 0 {
            xattrs::strip_clone(path);
        }
    }

    /// The attributes of a file whose data is the same: `-X`/`-A` differences are
    /// written (with the owner already set and the mode still to come).
    pub(crate) fn fix_extra(&self, path: &Path, src: &Src, have: Option<DstExtra>) -> io::Result<()> {
        self.apply_extra(path, src, have, src.meta.mode)
    }
}

/// Whether some of the attribute values are only checksums (so a sender sends them in
/// full along with the file's data).
pub fn has_summed(extra: Option<&Extra>) -> bool {
    extra.is_some_and(|e| e.xattrs.iter().any(|x| matches!(x.value, XValue::Sum { .. })))
}

//! Receiver-side behaviour of the optional rsync flags: `--link-dest`/`--compare-dest`,
//! `--backup`, `--partial`/`--partial-dir`, `--sparse`, `--max-size`/`--min-size`,
//! `--remove-source-files`, plus the scan-time `-L`/`-k`/`--chmod` adjustments shared
//! with the remote sender.

use super::exec::{Engine, FileJob, finish_temp, split_dst};
use super::extra::Src;
use crate::checksum::{self, Sum};
use crate::cli::Options;
use crate::entry::{DirEnt, Kind, Meta};
use crate::fsops::{self, Copied};
use crate::output::{Change, Link, Stats, path_msg};
use std::ffi::OsStr;
use std::fs::{self, File};
use std::io::{self, Read};
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::FileExt;
use std::path::{Path, PathBuf};

/// `--chmod`: tweak the permissions a source entry is sent with.
pub fn adjust_mode(opts: &Options, meta: &mut Meta) {
    if !opts.chmod.is_empty() && meta.kind != Kind::Symlink {
        meta.mode = crate::chmod::tweak(&opts.chmod, meta.mode, meta.kind == Kind::Dir);
    }
}

/// Apply `-L`/`-k` (follow symlinks) and `--chmod` to a freshly read source listing of
/// `dir`. A dangling symlink under `-L` is dropped and reported through `err`.
pub fn adjust_listing(opts: &Options, dir: &Path, ents: Vec<DirEnt>, err: &mut dyn FnMut(Vec<u8>)) -> Vec<DirEnt> {
    if !opts.copy_links && !opts.copy_dirlinks && opts.chmod.is_empty() {
        return ents;
    }
    ents.into_iter()
        .filter_map(|mut e| {
            if e.meta.kind == Kind::Symlink && (opts.copy_links || opts.copy_dirlinks) {
                let p = dir.join(&e.name);
                match fsops::stat(&p) {
                    Ok(m) if opts.copy_links || m.kind == Kind::Dir => {
                        e.meta = m;
                        e.link = None;
                    }
                    Ok(_) => {}
                    Err(_) if opts.copy_links => {
                        err(path_msg("symlink has no referent: \"", &p, "\""));
                        return None;
                    }
                    Err(_) => {}
                }
            }
            adjust_mode(opts, &mut e.meta);
            Some(e)
        })
        .collect()
}

/// `--remove-source-files`: delete a source entry the receiver now has. `listed` is the
/// sender's own record of it; an entry that is no longer that file (type, device and
/// inode) with that size and modification time, to the nanosecond, changed since and
/// stays. `dst` (local transfers): the destination, which must not be the source itself
/// (`f f`, or a hard link). Returns the error message on failure.
pub fn remove_source(opts: &Options, path: &Path, listed: &Meta, dst: Option<&Path>) -> Result<(), Vec<u8>> {
    if !opts.remove_source_files || opts.dry_run {
        return Err(path_msg("ERROR: Skipping sender remove (not requested): ", path, ""));
    }
    let cur = match if opts.copy_links { fsops::stat(path) } else { fsops::lstat(path) } {
        Ok(c) if c.kind == Kind::Dir => return Err(path_msg("sender failed to remove ", path, ": Is a directory")),
        Ok(c) => c,
        Err(e) => return Err(path_msg("sender failed to remove ", path, format!(": {e}"))),
    };
    if dst.is_some_and(|d| fsops::lstat(d).is_ok_and(|d| fsops::same_file(&cur, &d))) {
        return Err(path_msg("ERROR: Skipping sender remove of destination file: ", path, ""));
    }
    let unchanged = (cur.kind, cur.size, cur.mtime, cur.mtime_ns) == (listed.kind, listed.size, listed.mtime, listed.mtime_ns)
        && fsops::same_file(&cur, listed);
    if !unchanged {
        return Err(path_msg("ERROR: Skipping sender remove for changed file: ", path, ""));
    }
    fs::remove_file(path).map_err(|e| path_msg("sender failed to remove ", path, format!(": {e}")))
}

/// Whether `a` and `b` name the same directory entry (the same name in the same
/// directory), not merely the same file.
fn same_entry(a: &Path, b: &Path) -> bool {
    let (da, na) = split_dst(a);
    let (db, nb) = split_dst(b);
    na == nb && matches!((fsops::stat(da), fsops::stat(db)), (Ok(x), Ok(y)) if fsops::same_file(&x, &y))
}

/// Write `buf` at `off`, skipping 4 KiB blocks of zeros so they become holes (`-S`).
/// The caller extends the file to its full length afterwards.
pub fn write_sparse_at(f: &File, buf: &[u8], off: u64) -> io::Result<()> {
    const BLOCK: usize = 4096;
    let mut start: Option<usize> = None;
    for (i, block) in buf.chunks(BLOCK).enumerate() {
        let pos = i * BLOCK;
        if block.iter().all(|&b| b == 0) {
            if let Some(s) = start.take() {
                f.write_all_at(&buf[s..pos], off + s as u64)?;
            }
        } else if start.is_none() {
            start = Some(pos);
        }
    }
    if let Some(s) = start {
        f.write_all_at(&buf[s..], off + s as u64)?;
    }
    Ok(())
}

fn copy_sparse(mut s: &File, tmp: &Path, mode: u32) -> io::Result<File> {
    let out = fsops::create_new(tmp, mode)?;
    let mut buf = vec![0u8; 1 << 20];
    let mut off = 0u64;
    loop {
        fsops::check_interrupt()?;
        let n = match s.read(&mut buf) {
            Ok(0) => break,
            Ok(n) => n,
            Err(e) if e.kind() == io::ErrorKind::Interrupted => continue,
            Err(e) => return Err(e),
        };
        write_sparse_at(&out, &buf[..n], off)?;
        off += n as u64;
    }
    out.set_len(off)?;
    Ok(out)
}

/// The result of looking for a file in the `--link-dest`/`--compare-dest` directories.
pub(crate) enum AltFile {
    None,
    /// Nothing identical, but this file is a delta basis (and what the new file is
    /// itemized against).
    Basis(PathBuf, Meta),
    /// Handled: linked, copied, or found identical.
    Done,
}

impl Engine<'_> {
    /// Record the destination directory and resolve the directories that are relative
    /// to it (`--link-dest`, `--compare-dest`, `--backup-dir`).
    pub fn set_dest(&mut self, dest_root: &Path) {
        self.dest_root = dest_root.to_path_buf();
        let resolve = |d: &OsStr| -> PathBuf {
            let p = Path::new(d);
            if p.is_absolute() { p.to_path_buf() } else { dest_root.join(p) }
        };
        let kind = if self.opts.alt_link { "link" } else { "compare" };
        for d in &self.opts.alt_dest {
            let p = resolve(d);
            if fs::metadata(&p).is_ok_and(|m| m.is_dir()) {
                self.alt_dirs.push(p);
            } else {
                let shown = std::env::current_dir().map(|c| c.join(&p)).unwrap_or(p);
                self.out.error(path_msg(&format!("--{kind}-dest arg does not exist: "), &shown, ""));
            }
        }
        self.backup_root = self.opts.backup_dir.as_deref().map(resolve);
    }

    /// `-L`/`-k`/`--chmod`, then (`-X`/`-A`) the entries' attributes and ACLs, for a
    /// listing read on this machine.
    pub(crate) fn adjust_listing(&self, dir: &Path, ents: Vec<DirEnt>) -> Vec<DirEnt> {
        let ents = adjust_listing(self.opts, dir, ents, &mut |m| self.src_err(m));
        self.local_listing(dir, ents)
    }

    /// `--max-size` / `--min-size`: regular files outside the range are skipped.
    pub(crate) fn size_filtered(&self, meta: &Meta, rel: &[u8]) -> bool {
        let (over, under) = (
            self.opts.max_size.is_some_and(|m| meta.size > m),
            self.opts.min_size.is_some_and(|m| meta.size < m),
        );
        if (over || under) && self.out.verbose() >= 2 {
            let why: &[u8] = if over { b" is over max-size" } else { b" is under min-size" };
            self.out.info([rel, why].concat());
        }
        over || under
    }

    /// `dst` relative to the destination directory (where alt-dest and backup paths mirror it).
    fn dest_rel(&self, dst: &Path) -> PathBuf {
        match dst.strip_prefix(&self.dest_root) {
            Ok(r) => r.to_path_buf(),
            Err(_) => dst.file_name().map(PathBuf::from).unwrap_or_default(),
        }
    }

    // -----------------------------------------------------------------------
    // --link-dest / --compare-dest
    // -----------------------------------------------------------------------

    /// The alt-dest counterpart of a new directory, which it is itemized against.
    pub(crate) fn alt_dir_meta(&self, rel: &[u8]) -> Option<Meta> {
        let rel = Path::new(OsStr::from_bytes(rel));
        self.alt_dirs.iter().find_map(|a| fsops::lstat(&a.join(rel)).ok().filter(|m| m.kind == Kind::Dir))
    }

    /// Would rsync's quick check accept `alt` as holding the source's data?
    fn alt_data_same(&self, src: &Meta, alt: &Meta, alt_path: &Path, src_sum: &mut dyn FnMut() -> Option<Sum>) -> bool {
        if src.size != alt.size {
            return false;
        }
        if self.opts.checksum {
            return src_sum().is_some_and(|s| checksum::file_sum(alt_path).is_ok_and(|a| a == s));
        }
        self.opts.size_only || (!self.opts.ignore_times && self.rules.same_mtime(src, alt))
    }

    /// Look for a regular file in the alt-dest directories. An identical one (data and
    /// attributes) is hard-linked (`--link-dest`) or makes the transfer unnecessary
    /// (`--compare-dest`); one with identical data but other attributes is copied.
    #[allow(clippy::too_many_arguments)]
    pub(crate) fn try_alt_file(
        &self,
        root: u16,
        x: &Src,
        rel: &[u8],
        src_path: &Path,
        dst_path: &Path,
        dst_meta: Option<&Meta>,
        src_sum: &mut dyn FnMut() -> Option<Sum>,
    ) -> AltFile {
        let src = x.meta;
        let dest_rel = self.dest_rel(dst_path);
        let mut basis: Option<(PathBuf, Meta)> = None;
        let mut same_data: Option<(PathBuf, Meta)> = None;
        let mut exact: Option<(PathBuf, Meta)> = None;
        let mut sum_cache: Option<Option<Sum>> = None;
        for alt in &self.alt_dirs {
            let p = alt.join(&dest_rel);
            let Ok(m) = fsops::lstat(&p) else { continue };
            if m.kind != Kind::File {
                continue;
            }
            if basis.is_none() {
                basis = Some((p.clone(), m));
            }
            let mut cached = || *sum_cache.get_or_insert_with(&mut *src_sum);
            if !self.alt_data_same(src, &m, &p, &mut cached) {
                continue;
            }
            // rsync's unchanged_attrs: with -X/-A the attributes and ACLs must match too.
            if !self.rules.attrs_differ(src, &m).any() && self.extra_change(x, &p, &m).0 == (false, false) {
                exact = Some((p, m));
                break;
            }
            same_data.get_or_insert((p, m));
        }
        if let Some((p, _)) = exact {
            if self.opts.alt_link {
                if self.opts.itemize >= 2 {
                    self.out.item(rel, Kind::File, Link::None, Change { hardlink: true, ..Change::default() }, false);
                }
                if dst_meta.is_none() {
                    Stats::add(&self.stats.created, 1);
                }
                if !self.dry() {
                    if !self.backup_existing(src_path, dst_path, false) {
                        return AltFile::Done;
                    }
                    let (dir, name) = split_dst(dst_path);
                    let r = fsops::with_temp(dir, name, |tmp| fs::hard_link(&p, tmp))
                        .and_then(|(tmp, ())| finish_temp(&tmp, fs::rename(&tmp, dst_path)));
                    if let Err(e) = r {
                        self.err(path_msg("link \"", dst_path, format!("\" failed: {e}")));
                        return AltFile::Done;
                    }
                }
            } else {
                self.out.item(rel, Kind::File, Link::None, Change::default(), false);
                if dst_meta.is_some()
                    && !self.dry()
                    && let Err(e) = self.remove_or_backup(dst_path)
                {
                    self.err(path_msg("remove \"", dst_path, format!("\" failed: {e}")));
                    return AltFile::Done;
                }
            }
            self.source_done(root, src_path, dst_path, rel, src);
            return AltFile::Done;
        }
        if let Some((p, m)) = same_data {
            // Compared with the alt file it is copied from, as rsync does.
            let (xattr, acl) = self.extra_change(x, &p, &m).0;
            let change = Change { local: true, xattr, acl, ..self.attr_change(src, &m, false) };
            self.out.item(rel, Kind::File, Link::None, change, true);
            if dst_meta.is_none() {
                Stats::add(&self.stats.created, 1);
            }
            if !self.dry() && !self.backup_existing(src_path, dst_path, false) {
                self.not_written(x);
                return AltFile::Done;
            }
            if !self.dry()
                && let Err(e) = self.copy_alt(&p, dst_path, x)
            {
                let to = path_msg("\" -> \"", dst_path, format!("\" failed: {e}"));
                self.err(path_msg("copy \"", &p, to));
                self.not_written(x);
                return AltFile::Done;
            }
            self.source_done(root, src_path, dst_path, rel, src);
            return AltFile::Done;
        }
        match basis {
            Some((p, m)) => AltFile::Basis(p, m),
            None => AltFile::None,
        }
    }

    /// Copy an alt-dest file with identical data into place, with the source's attributes.
    fn copy_alt(&self, alt: &Path, dst_path: &Path, x: &Src) -> io::Result<()> {
        let src = x.meta;
        let mode = self.new_mode(src);
        let (uid, gid) = (self.rules.wanted_uid(src), self.rules.wanted_gid(src));
        let mtime = self.opts.times.then_some((src.mtime, src.mtime_ns));
        let (dir, name) = split_dst(dst_path);
        let (tmp, copied) =
            fsops::with_temp(dir, name, |tmp| fsops::copy_new(alt, tmp, src.size, !self.opts.no_clone, mode))?;
        let r = self
            .apply_new_file_attrs(&tmp, copied, mode, uid, gid, mtime, Some(x))
            .and_then(|()| fs::rename(&tmp, dst_path));
        finish_temp(&tmp, r)
    }

    /// A new symlink identical to one in an alt-dest directory: hard-link it
    /// (`--link-dest`) or leave it out (`--compare-dest`).
    pub(crate) fn alt_symlink(&self, rel: &[u8], target: &[u8], dst_path: &Path) -> bool {
        let dest_rel = self.dest_rel(dst_path);
        let Some(p) = self.alt_dirs.iter().map(|a| a.join(&dest_rel)).find(|p| {
            fsops::lstat(p).is_ok_and(|m| m.kind == Kind::Symlink)
                && fsops::read_link_bytes(p).is_ok_and(|t| t == target)
        }) else {
            return false;
        };
        let change = Change { hardlink: self.opts.alt_link, ..Change::default() };
        if self.opts.itemize >= 2 {
            self.out.item(rel, Kind::Symlink, Link::Sym(target), change, false);
        }
        if self.opts.alt_link && !self.dry() {
            Stats::add(&self.stats.created, 1);
            let (dir, name) = split_dst(dst_path);
            let r = fsops::with_temp(dir, name, |tmp| fs::hard_link(&p, tmp))
                .and_then(|(tmp, ())| finish_temp(&tmp, self.place_link(&tmp, dst_path)));
            if let Err(e) = r {
                self.err(path_msg("link \"", dst_path, format!("\" failed: {e}")));
            }
        }
        true
    }

    // -----------------------------------------------------------------------
    // --partial / --partial-dir
    // -----------------------------------------------------------------------

    fn partial_path(&self, dst_path: &Path) -> Option<PathBuf> {
        let pd = Path::new(self.opts.partial_dir.as_deref()?);
        let (dir, name) = split_dst(dst_path);
        Some(if pd.is_absolute() { pd.join(name) } else { dir.join(pd).join(name) })
    }

    /// A file left in `--partial-dir` by an interrupted transfer becomes the basis.
    pub(crate) fn find_partial(&self, job: &mut FileJob) {
        if let Some(p) = self.partial_path(&job.dst_path)
            && fsops::lstat(&p).is_ok_and(|m| m.kind == Kind::File)
        {
            job.basis = Some(p.clone());
            job.partial = Some(p);
        }
    }

    /// `--partial`: after an interrupted or failed transfer, keep what was received
    /// (`tmp`) as the destination file, or in `--partial-dir`. Returns whether it was kept.
    pub(crate) fn keep_partial(&self, tmp: &Path, dst_path: &Path) -> bool {
        if !self.opts.partial || fs::metadata(tmp).map_or(true, |m| m.len() == 0) {
            return false;
        }
        let target = match self.partial_path(dst_path) {
            Some(p) => {
                // rsync creates the partial dir private to the user.
                use std::os::unix::fs::DirBuilderExt;
                if let Some(dir) = p.parent()
                    && fs::DirBuilder::new().recursive(true).mode(0o700).create(dir).is_err()
                {
                    return false;
                }
                p
            }
            None => dst_path.to_path_buf(),
        };
        if tmp != target && fs::rename(tmp, &target).is_err() {
            return false;
        }
        fsops::unregister_temp(tmp);
        true
    }

    /// Settle a written temp file: on an interrupt with `--partial` it is kept.
    pub(crate) fn finish_or_keep(&self, tmp: &Path, job: &FileJob, r: io::Result<()>) -> io::Result<()> {
        if let Err(e) = r {
            if fsops::interrupted() && self.keep_partial(tmp, &job.dst_path) {
                return Err(e);
            }
            return finish_temp(tmp, Err(e));
        }
        finish_temp(tmp, r)
    }

    /// A file's data is in place: drop its `--partial-dir` leftover (and the directory,
    /// once empty) and, with `--remove-source-files`, its source.
    pub(crate) fn file_written(&self, job: &FileJob) {
        if let Some(p) = &job.partial {
            let _ = fs::remove_file(p);
            if self.opts.partial_dir.as_deref().is_some_and(|d| !Path::new(d).is_absolute())
                && let Some(dir) = p.parent()
            {
                let _ = fs::remove_dir(dir);
            }
        }
        self.source_done(job.root, &job.src_path, &job.dst_path, &job.rel, &job.meta);
    }

    // -----------------------------------------------------------------------
    // --remove-source-files
    // -----------------------------------------------------------------------

    /// The receiver has this entry (now at `dst_path`): with `--remove-source-files`, the
    /// sender removes it.
    pub(crate) fn source_done(&self, root: u16, src_path: &Path, dst_path: &Path, rel: &[u8], meta: &Meta) {
        if !self.opts.remove_source_files || self.dry() {
            return;
        }
        if let Some(r) = &self.remote {
            return r.synced(root, rel);
        }
        match remove_source(self.opts, src_path, meta, Some(dst_path)) {
            Ok(()) if self.out.verbose() >= 2 => self.out.info([b"sender removed ", rel].concat()),
            Ok(()) => {}
            Err(m) => self.err(m),
        }
    }

    // -----------------------------------------------------------------------
    // --backup
    // -----------------------------------------------------------------------

    fn backup_path(&self, dst: &Path) -> PathBuf {
        let mut p = match &self.backup_root {
            Some(root) => root.join(self.dest_rel(dst)),
            None => dst.to_path_buf(),
        }
        .into_os_string();
        p.push(&self.opts.suffix);
        PathBuf::from(p)
    }

    /// Without `--backup-dir`, existing backups (`*~`) are protected from `--delete`.
    pub(crate) fn is_protected_backup(&self, name: &OsStr) -> bool {
        self.opts.backup
            && self.backup_root.is_none()
            && !self.opts.suffix.is_empty()
            && name.as_bytes().ends_with(self.opts.suffix.as_bytes())
    }

    /// Where `dst` (as `d`) is backed up. It must not be `dst` itself (an empty `--suffix`
    /// with a `--backup-dir` leading back to its directory) or `src` (`-b f~ f`), which
    /// the backup would replace. `Ok((b, true))`: `b` is a hard link to `dst` already.
    fn backup_target(&self, dst: &Path, d: &Meta, src: Option<&Path>) -> io::Result<(PathBuf, bool)> {
        let b = self.backup_path(dst);
        let bm = match fsops::lstat(&b) {
            Ok(m) => Some(m),
            Err(e) if e.kind() == io::ErrorKind::NotFound => None,
            Err(e) => return Err(e),
        };
        let linked = bm.is_some_and(|bm| fsops::same_file(d, &bm));
        if linked && same_entry(dst, &b) {
            return Err(io::Error::new(io::ErrorKind::InvalidInput, "the backup would replace the file itself"));
        }
        // A remote source's path means nothing on this machine.
        let is_src = |s: &Path, bm: &Meta| [fsops::lstat(s), fsops::stat(s)].into_iter().flatten().any(|m| fsops::same_file(&m, bm));
        if let (Some(bm), Some(src), None) = (bm, src, &self.remote)
            && is_src(src, &bm)
        {
            return Err(io::Error::new(io::ErrorKind::InvalidInput, "the backup would replace the source file"));
        }
        if self.backup_root.is_some()
            && let Some(parent) = b.parent()
        {
            fs::create_dir_all(parent)?;
        }
        Ok((b, linked))
    }

    fn report_backup(&self, dst: &Path, b: &Path) {
        if self.out.verbose() >= 2 {
            let to = b.strip_prefix(&self.dest_root).unwrap_or(b);
            let msg = path_msg("backed up ", &self.dest_rel(dst), path_msg(" to ", to, ""));
            self.out.info(msg);
        }
    }

    /// `--backup` before `dst` is replaced by a copy of `src`: keep the old version under
    /// its backup name. It is hard-linked (or copied, with `copy` or across filesystems),
    /// so `dst` stays in place until the new version is renamed over it. The backup is
    /// made under a temp name and renamed over the previous one, which a failure leaves
    /// as it was. Returns false, after reporting why, when there is no backup: `dst` must
    /// then not be replaced.
    pub(crate) fn backup_existing(&self, src: &Path, dst: &Path, copy: bool) -> bool {
        if !self.opts.backup || self.dry() {
            return true;
        }
        match self.make_backup(src, dst, copy) {
            Ok(()) => true,
            Err(e) => {
                self.err(path_msg("make_backup \"", dst, format!("\" failed: {e}")));
                false
            }
        }
    }

    fn make_backup(&self, src: &Path, dst: &Path, copy: bool) -> io::Result<()> {
        let d = match fsops::lstat(dst) {
            Ok(d) if d.kind != Kind::Dir => d,
            // Nothing to keep (a directory in the way is the caller's to deal with).
            Ok(_) => return Ok(()),
            Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(()),
            Err(e) => return Err(e),
        };
        let (b, linked) = self.backup_target(dst, &d, Some(src))?;
        // `copy`: `dst` is rewritten in place, which a hard link would see.
        if !linked || copy {
            let (dir, name) = split_dst(&b);
            let (tmp, ()) = fsops::with_temp(dir, name, |tmp| {
                if !copy && fs::hard_link(dst, tmp).is_ok() {
                    return Ok(());
                }
                match d.kind {
                    Kind::File => {
                        drop(fsops::copy_new(dst, tmp, d.size, !self.opts.no_clone, d.mode)?);
                        fsops::chmod(tmp, d.mode)?;
                        fsops::set_mtime(tmp, d.mtime, d.mtime_ns)?;
                        if self.opts.fsync {
                            fsops::fsync_path(tmp)?;
                        }
                        Ok(())
                    }
                    Kind::Symlink => std::os::unix::fs::symlink(fs::read_link(dst)?, tmp),
                    kind => fsops::mknod(tmp, kind, d.mode, d.rdev),
                }
            })?;
            // Interrupted: the previous backup stays.
            finish_temp(&tmp, fsops::check_interrupt().and_then(|()| fs::rename(&tmp, &b)))?;
        }
        self.report_backup(dst, &b);
        Ok(())
    }

    /// [`Engine::backup_existing`] for a file about to be rewritten.
    pub(crate) fn backup_existing_file(&self, job: &FileJob) -> bool {
        self.backup_existing(&job.src_path, &job.dst_path, self.opts.inplace)
    }

    /// Remove a non-directory, or with `--backup` move it to its backup name (replacing
    /// an older backup only once it is there).
    pub(crate) fn remove_or_backup(&self, path: &Path) -> io::Result<()> {
        if !self.opts.backup {
            return fs::remove_file(path);
        }
        let (b, linked) = self.backup_target(path, &fsops::lstat(path)?, None)?;
        if linked {
            // The backup holds this version already; renaming a link over itself does nothing.
            fs::remove_file(path)?;
        } else {
            fs::rename(path, &b)?;
        }
        self.report_backup(path, &b);
        Ok(())
    }

    // -----------------------------------------------------------------------
    // --sparse
    // -----------------------------------------------------------------------

    /// Copy a new file's data from `src`, a source opened with [`fsops::open_listed`];
    /// with `-S` runs of zeros become holes (no clone or kernel copy, which would keep
    /// them as data).
    pub(crate) fn copy_from(&self, src: &File, tmp: &Path, size: u64, allow_fast: bool, mode: u32) -> io::Result<Copied> {
        if self.opts.sparse {
            return copy_sparse(src, tmp, mode).map(Copied::Written);
        }
        fsops::copy_from(src, tmp, size, allow_fast, mode)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sparse_writes_skip_zero_blocks_but_keep_content() {
        let tmp = tempfile::tempdir().unwrap();
        let p = tmp.path().join("f");
        let f = fsops::create_new(&p, 0o644).unwrap();
        let mut data = vec![0u8; 64 << 10];
        data[5000] = 7;
        data[60_000] = 9;
        write_sparse_at(&f, &data, 4096).unwrap();
        f.set_len(4096 + data.len() as u64).unwrap();
        let back = fs::read(&p).unwrap();
        assert_eq!(&back[4096..], &data[..]);
        assert!(back[..4096].iter().all(|&b| b == 0));
    }
}

//! Extraction of an archive into a folder: all of it, or nothing.
//!
//! The content is first written into a temporary folder next to its
//! destination and moved into place when the last byte is there. A failure,
//! a cancellation or a damaged archive half way through leaves the
//! destination exactly as it was.
//!
//! - A destination that does not exist yet appears with one rename.
//! - A destination that exists is merged into: new files are moved in, and a
//!   file that replaces an existing one first moves the old one aside, so
//!   even a failure during the merge is undone.
//!
//! Every name passes through `boxit-security` on its way: an entry that
//! tries to leave the destination, or to be written through a link, is left
//! out and reported in the [`Outcome`], and the bytes written are counted
//! against the limits of the operation.

use std::cell::RefCell;
use std::cmp::Reverse;
use std::collections::HashSet;
use std::fs;
use std::io::{self, ErrorKind, Read, Write};
use std::mem;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Mutex, MutexGuard, PoisonError};

use boxit_core::{
    ArchiveReader, Conflict, ConflictAnswer, ConflictPolicy, Entry, EntryKind, EntryMeta,
    EntrySink, Error, ExtractOptions, OpContext, Result, Selection, SymlinkPolicy,
};
use boxit_security::{
    check_link_target, sanitize, Guarded, LinkTracker, Platform, SafePath, SizeGuard,
};
use serde::Serialize;

use crate::fsx::{self, Existing};
use crate::guarded::guarded;

/// Prefix of the temporary folder of an extraction.
pub const STAGING_PREFIX: &str = ".boxit-";

/// Size of the block the data of a file is copied in.
const COPY_BLOCK: usize = 256 * 1024;

/// Files at least this large get their space reserved before the data is
/// written, which keeps them in one piece on the disk.
const RESERVE_FROM: u64 = 1024 * 1024;

/// How many left-out entries an outcome lists by name; the rest are counted.
const SKIPPED_LISTED: usize = 500;

/// `FILE_ATTRIBUTE_DIRECTORY` of Windows.
const ATTRIBUTE_DIRECTORY: u32 = 0x10;

thread_local! {
    /// The copy block of this thread, kept between files.
    static BLOCK: RefCell<Vec<u8>> = const { RefCell::new(Vec::new()) };
}

pub struct ExtractRequest {
    /// The folder the content goes into; created when it does not exist.
    pub destination: PathBuf,
    /// A folder of the archive the selection is taken from, by its path in
    /// the archive; empty for the archive as a whole. What is inside the
    /// folder lands in the destination without the folders above it, the
    /// way a file manager copies out of a folder it shows.
    pub base: String,
    pub selection: Selection,
    pub options: ExtractOptions,
    /// Size of the archive file, for the limit on the compression ratio.
    pub archive_size: u64,
    /// The Mark of the Web of the archive, copied onto every file that is
    /// written. Empty when the archive has no such mark.
    pub mark: Vec<u8>,
}

/// An entry that was not extracted, and why. The reason is an error code of
/// the dictionaries: `pathTraversal`, `exists`, `symlinkSkipped` and so on.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct Skipped {
    pub path: String,
    pub reason: Error,
}

/// What an extraction did.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Outcome {
    pub files: u64,
    pub folders: u64,
    pub links: u64,
    /// Bytes of content written.
    pub bytes: u64,
    /// Entries left out, the first few hundred of them.
    pub skipped: Vec<Skipped>,
    /// How many entries were left out in all.
    pub skipped_total: u64,
}

fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(PoisonError::into_inner)
}

/// The temporary folder of one extraction. Dropping it removes everything
/// that was not moved into place, and the folders created on the way to a
/// new destination.
struct Staging {
    root: PathBuf,
    /// Parents of the destination that this extraction created, outermost
    /// first; removed again when the extraction does not complete.
    created: Vec<PathBuf>,
}

impl Staging {
    fn create(destination: &Path) -> Result<Self> {
        let mut created = Vec::new();
        let base = if fsx::existing(destination) == Existing::Nothing {
            let parent = destination.parent().ok_or_else(|| {
                Error::new("badDestination").with("path", fsx::display(destination))
            })?;
            let mut missing = Vec::new();
            let mut folder = parent;
            while fsx::existing(folder) == Existing::Nothing {
                missing.push(folder.to_path_buf());
                match folder.parent() {
                    Some(up) => folder = up,
                    None => break,
                }
            }
            for folder in missing.into_iter().rev() {
                fs::create_dir(&folder)?;
                created.push(folder);
            }
            parent.to_path_buf()
        } else {
            destination.to_path_buf()
        };
        let staging = Self {
            root: base.join(format!("{STAGING_PREFIX}{}", fsx::unique_suffix())),
            created,
        };
        // On a failure from here on, dropping `staging` cleans up.
        fs::create_dir(&staging.root)?;
        fs::create_dir(staging.content())?;
        Ok(staging)
    }

    /// Where the content is written before it is moved into place.
    fn content(&self) -> PathBuf {
        self.root.join("content")
    }

    /// Where replaced files wait until the extraction is complete.
    fn backup(&self) -> PathBuf {
        self.root.join("backup")
    }
}

impl Drop for Staging {
    fn drop(&mut self) {
        if let Err(error) = fsx::remove_any(&self.root) {
            tracing::warn!(folder = %fsx::display(&self.root), "a temporary folder was left behind: {error}");
        }
        for folder in self.created.iter().rev() {
            let _ = fs::remove_dir(folder);
        }
    }
}

/// One step of moving the content into an existing destination.
enum Step {
    /// Something of ours now stands at this path.
    Moved(PathBuf),
    /// What stood at `target` waits at `aside`.
    SetAside { aside: PathBuf, target: PathBuf },
}

/// The steps of a merge, so they can be taken back.
#[derive(Default)]
struct Journal {
    steps: Vec<Step>,
    set_aside: usize,
}

impl Journal {
    /// Takes every step back, last first.
    fn undo(self) {
        for step in self.steps.into_iter().rev() {
            let result = match &step {
                Step::Moved(path) => fsx::remove_any(path),
                Step::SetAside { aside, target } => fs::rename(aside, target),
            };
            if let Err(error) = result {
                tracing::error!("a step of an extraction could not be undone: {error}");
            }
        }
    }
}

/// Moves the content of `from` into the existing folder `to`. A folder that
/// is new there moves as a whole with one rename.
fn merge(from: &Path, to: &Path, backup: &Path, journal: &mut Journal) -> io::Result<()> {
    for entry in fs::read_dir(from)? {
        let entry = entry?;
        let source = entry.path();
        let target = to.join(entry.file_name());
        match fsx::existing(&target) {
            Existing::Nothing => {}
            Existing::Folder if entry.file_type()?.is_dir() => {
                merge(&source, &target, backup, journal)?;
                continue;
            }
            Existing::File | Existing::Folder | Existing::Link => {
                if journal.set_aside == 0 {
                    fs::create_dir_all(backup)?;
                }
                let aside = backup.join(journal.set_aside.to_string());
                fs::rename(&target, &aside)?;
                journal.set_aside += 1;
                journal.steps.push(Step::SetAside {
                    aside,
                    target: target.clone(),
                });
            }
        }
        fs::rename(&source, &target)?;
        journal.steps.push(Step::Moved(target));
    }
    Ok(())
}

/// Puts the staged content into place.
fn commit(staging: &mut Staging, destination: &Path) -> Result<()> {
    let content = staging.content();
    if fsx::existing(destination) == Existing::Nothing {
        match fs::rename(&content, destination) {
            Ok(()) => {
                staging.created.clear();
                return Ok(());
            }
            // Someone created the destination meanwhile: merge into it.
            Err(_) if fsx::existing(destination) == Existing::Folder => {}
            Err(error) => return Err(error.into()),
        }
    }
    let mut journal = Journal::default();
    match merge(&content, destination, &staging.backup(), &mut journal) {
        Ok(()) => {
            staging.created.clear();
            Ok(())
        }
        Err(error) => {
            journal.undo();
            Err(error.into())
        }
    }
}

/// A name for a file that must not replace an existing one: `name (2).ext`.
fn numbered(name: &str, number: u32) -> String {
    match name.rfind('.') {
        // A dot at the start makes a hidden file, not an extension.
        Some(dot) if dot > 0 => format!("{} ({number}){}", &name[..dot], &name[dot..]),
        _ => format!("{name} ({number})"),
    }
}

fn folded(path: &SafePath) -> Vec<String> {
    path.parts()
        .iter()
        .map(|part| part.to_lowercase())
        .collect()
}

/// What to do with an entry whose place is taken.
enum Settled {
    /// Extract it to this path.
    Go(SafePath),
    /// Leave it out, for this reason.
    Skip(Error),
}

/// The error of a selection that picked nothing: names that are not in the
/// archive are a mistake of whoever gave them, and doing nothing with
/// success is no answer to it.
pub(crate) fn nothing_matched(selection: &Selection) -> Error {
    let error = Error::new("nothingMatched");
    match selection {
        Selection::Paths(paths) => error.with("name", paths.join(", ")),
        Selection::All | Selection::Indexes(_) => error,
    }
}

#[derive(Default)]
struct Counts {
    /// Entries the archive handed over, extracted or not.
    seen: AtomicU64,
    files: AtomicU64,
    folders: AtomicU64,
    links: AtomicU64,
    bytes: AtomicU64,
    skipped: AtomicU64,
}

struct Sink<'a> {
    /// The staging folder the content is written into.
    content: PathBuf,
    destination: PathBuf,
    /// The folder of the archive that stands for the destination.
    base: SafePath,
    /// The destination existed before the extraction: its files matter.
    existed: bool,
    options: ExtractOptions,
    guard: SizeGuard,
    links: LinkTracker,
    ctx: &'a OpContext,
    /// The answer given "for all" conflicts; also serializes the questions.
    answer: Mutex<Option<ConflictPolicy>>,
    /// Names taken by entries extracted under a new name.
    renamed: Mutex<HashSet<Vec<String>>>,
    /// Folders of the destination known to be real folders or absent.
    clear: Mutex<HashSet<Vec<String>>>,
    /// Folders whose times are set once their content is complete.
    folders: Mutex<Vec<(SafePath, EntryMeta)>>,
    counts: Counts,
    skipped: Mutex<Vec<Skipped>>,
    /// Copied onto each file, when the archive carried a mark.
    mark: &'a [u8],
}

impl Sink<'_> {
    fn skip(&self, meta: &EntryMeta, reason: Error) {
        tracing::debug!(entry = %meta.path, "left out: {reason}");
        self.counts.skipped.fetch_add(1, Ordering::Relaxed);
        let mut skipped = lock(&self.skipped);
        if skipped.len() < SKIPPED_LISTED {
            skipped.push(Skipped {
                path: meta.path.clone(),
                reason,
            });
        }
    }

    /// Checks the folders of the existing destination on the way to `path`:
    /// when one of them is a file or a link, the entry cannot go there.
    fn blocked(&self, path: &SafePath) -> Option<Error> {
        if !self.existed {
            return None;
        }
        let parent = path.parent()?;
        if lock(&self.clear).contains(&folded(&parent)) {
            return None;
        }
        let mut folder = self.destination.clone();
        for part in parent.parts() {
            folder.push(part);
            match fsx::existing(&folder) {
                Existing::Folder => {}
                // Nothing here, so nothing below either.
                Existing::Nothing => break,
                Existing::File | Existing::Link => {
                    // Replacing was asked for: the merge moves it aside.
                    if self.options.conflicts == ConflictPolicy::Overwrite {
                        break;
                    }
                    return Some(Error::new("blockedByFile").with("path", fsx::display(&folder)));
                }
            }
        }
        lock(&self.clear).insert(folded(&parent));
        None
    }

    /// Asks what to do about an existing file, once for all files when the
    /// answer says so.
    fn ask(&self, path: &Path, meta: &EntryMeta) -> Result<ConflictPolicy> {
        let mut answer = lock(&self.answer);
        if let Some(policy) = *answer {
            return Ok(policy);
        }
        let metadata = fs::symlink_metadata(path).ok();
        let conflict = Conflict {
            path: fsx::display(path),
            existing_size: metadata.as_ref().map_or(0, fs::Metadata::len),
            existing_modified: metadata
                .and_then(|metadata| metadata.modified().ok())
                .map(fsx::timestamp),
            entry: meta.clone(),
        };
        let (policy, for_all) = match self.ctx.interaction.conflict(&conflict) {
            ConflictAnswer::Skip => (ConflictPolicy::Skip, false),
            ConflictAnswer::Overwrite => (ConflictPolicy::Overwrite, false),
            ConflictAnswer::Rename => (ConflictPolicy::Rename, false),
            ConflictAnswer::SkipAll => (ConflictPolicy::Skip, true),
            ConflictAnswer::OverwriteAll => (ConflictPolicy::Overwrite, true),
            ConflictAnswer::RenameAll => (ConflictPolicy::Rename, true),
            ConflictAnswer::Cancel => return Err(Error::cancelled()),
        };
        if for_all {
            *answer = Some(policy);
        }
        Ok(policy)
    }

    /// A free name next to `path`, taken for this entry.
    fn free_name(&self, path: &SafePath) -> SafePath {
        let name = path.name().unwrap_or_default();
        let mut renamed = lock(&self.renamed);
        let mut number = 2;
        loop {
            let candidate = path.with_name(&numbered(name, number));
            let free = fsx::existing(&candidate.to_path(&self.destination)) == Existing::Nothing
                && fsx::existing(&candidate.to_path(&self.content)) == Existing::Nothing
                && renamed.insert(folded(&candidate));
            if free {
                return candidate;
            }
            number += 1;
        }
    }

    /// Decides where a file or a link goes when something may be in its
    /// place already.
    fn settle(&self, path: &SafePath, meta: &EntryMeta) -> Result<Settled> {
        if !self.existed {
            return Ok(Settled::Go(path.clone()));
        }
        let target = path.to_path(&self.destination);
        let found = fsx::existing(&target);
        if found == Existing::Nothing {
            return Ok(Settled::Go(path.clone()));
        }
        let policy = match self.options.conflicts {
            ConflictPolicy::Ask => self.ask(&target, meta)?,
            policy => policy,
        };
        Ok(match policy {
            ConflictPolicy::Ask | ConflictPolicy::Skip => {
                Settled::Skip(Error::new("exists").with("path", fsx::display(&target)))
            }
            // A folder full of files is not replaced by one file.
            ConflictPolicy::Overwrite if found == Existing::Folder => {
                Settled::Skip(Error::new("existingFolder").with("path", fsx::display(&target)))
            }
            ConflictPolicy::Overwrite => Settled::Go(path.clone()),
            ConflictPolicy::Rename => Settled::Go(self.free_name(path)),
        })
    }

    /// Makes room in the staging folder for an entry at `path`: creates its
    /// folders and removes an earlier entry of the same name. Archives may
    /// hold a name twice; the later entry wins.
    fn prepare(&self, path: &SafePath) -> Result<Option<Error>> {
        let target = path.to_path(&self.content);
        if let Some(parent) = target.parent() {
            if let Err(error) = fs::create_dir_all(parent) {
                // An earlier entry is a file where this one needs a folder.
                return if matches!(
                    error.kind(),
                    ErrorKind::AlreadyExists | ErrorKind::NotADirectory
                ) {
                    Ok(Some(Error::new("nameClash").with("name", path)))
                } else {
                    Err(error.into())
                };
            }
        }
        if fsx::existing(&target) != Existing::Nothing {
            fsx::remove_any(&target)?;
            self.links.remove(path);
        }
        Ok(None)
    }

    fn folder(&self, path: &SafePath, meta: &EntryMeta) -> Result<Option<Error>> {
        if self.existed && self.options.conflicts != ConflictPolicy::Overwrite {
            // A file stands where the folder would go, and it is to be kept.
            let taken = path.to_path(&self.destination);
            if matches!(fsx::existing(&taken), Existing::File | Existing::Link) {
                return Ok(Some(
                    Error::new("blockedByFile").with("path", fsx::display(&taken)),
                ));
            }
        }
        let target = path.to_path(&self.content);
        match fsx::existing(&target) {
            Existing::Folder => {}
            found => {
                if found != Existing::Nothing {
                    fsx::remove_any(&target)?;
                    self.links.remove(path);
                }
                if let Err(error) = fs::create_dir_all(&target) {
                    return if matches!(
                        error.kind(),
                        ErrorKind::AlreadyExists | ErrorKind::NotADirectory
                    ) {
                        Ok(Some(Error::new("nameClash").with("name", path)))
                    } else {
                        Err(error.into())
                    };
                }
                self.counts.folders.fetch_add(1, Ordering::Relaxed);
            }
        }
        if meta.modified.is_some() {
            lock(&self.folders).push((path.clone(), meta.clone()));
        }
        Ok(None)
    }

    /// Copies the data of a file, counting it against the limits.
    fn copy(&self, meta: &EntryMeta, data: &mut dyn Read, file: &mut fs::File) -> Result<u64> {
        let mut writer = Guarded::new(file, self.guard.file(&meta.path));
        BLOCK.with_borrow_mut(|block| {
            block.resize(COPY_BLOCK, 0);
            loop {
                self.ctx.checkpoint()?;
                let count = match data.read(block) {
                    Ok(0) => break,
                    Ok(count) => count,
                    Err(error) if error.kind() == ErrorKind::Interrupted => continue,
                    Err(error) => return Err(Error::from(error)),
                };
                writer.write_all(&block[..count])?;
                self.ctx.progress.advance(count as u64);
            }
            Ok(writer.written())
        })
    }

    fn file(
        &self,
        path: &SafePath,
        meta: &EntryMeta,
        data: &mut dyn Read,
    ) -> Result<Option<Error>> {
        let path = match self.settle(path, meta)? {
            Settled::Go(path) => path,
            Settled::Skip(reason) => return Ok(Some(reason)),
        };
        if let Some(reason) = self.prepare(&path)? {
            return Ok(Some(reason));
        }
        let target = path.to_path(&self.content);
        let mut file = fsx::create_file(&target, meta)?;
        // The declared size is a hint from a stranger: it is used only when
        // it is within the limits, and corrected once the data is written.
        let reserved = meta.size.filter(|size| {
            *size >= RESERVE_FROM && self.guard.declare([(meta.path.as_str(), *size)]).is_ok()
        });
        if let Some(size) = reserved {
            // Reserving is an optimization; a file system that cannot do it
            // still takes the data.
            let _ = file.set_len(size);
        }
        let written = self.copy(meta, data, &mut file)?;
        if reserved.is_some_and(|size| size != written) {
            file.set_len(written)?;
        }
        if let Err(error) = fsx::set_file_times(&file, meta) {
            tracing::debug!(entry = %meta.path, "the times were not set: {error}");
        }
        drop(file);
        fsx::copy_mark(&target, self.mark);
        if let Err(error) = fsx::set_permissions(&target, meta) {
            tracing::debug!(entry = %meta.path, "the permissions were not set: {error}");
        }
        self.counts.files.fetch_add(1, Ordering::Relaxed);
        self.counts.bytes.fetch_add(written, Ordering::Relaxed);
        Ok(None)
    }

    /// Whether the target of a link, read from the folder of the link, is a
    /// folder among what was extracted so far. Windows needs to know.
    fn points_to_folder(&self, link: &SafePath, target: &str, meta: &EntryMeta) -> bool {
        if meta
            .windows_attributes
            .is_some_and(|attributes| attributes & ATTRIBUTE_DIRECTORY != 0)
            || target.ends_with(['/', '\\'])
        {
            return true;
        }
        let mut parts: Vec<&str> = link.parts().iter().map(String::as_str).collect();
        parts.pop();
        for part in target.split(['/', '\\']) {
            match part {
                "" | "." => {}
                ".." => {
                    parts.pop();
                }
                part => parts.push(part),
            }
        }
        let mut resolved = self.content.clone();
        resolved.extend(parts);
        resolved.is_dir()
    }

    fn symlink(&self, path: &SafePath, meta: &EntryMeta) -> Result<Option<Error>> {
        let wanted = match self.options.symlinks {
            SymlinkPolicy::Skip => false,
            SymlinkPolicy::Extract => true,
            SymlinkPolicy::Ask => self.ctx.interaction.symlink(meta),
        };
        if !wanted {
            return Ok(Some(Error::new("symlinkSkipped").with("name", path)));
        }
        let target = meta.link_target.as_deref().unwrap_or_default();
        if let Err(reason) = check_link_target(path, target, Platform::current()) {
            return Ok(Some(reason));
        }
        let path = match self.settle(path, meta)? {
            Settled::Go(path) => path,
            Settled::Skip(reason) => return Ok(Some(reason)),
        };
        if let Some(reason) = self.prepare(&path)? {
            return Ok(Some(reason));
        }
        let to_folder = self.points_to_folder(&path, target, meta);
        match fsx::symlink(target, &path.to_path(&self.content), to_folder) {
            Ok(()) => {}
            // Windows lets only administrators and developers create links.
            Err(error)
                if error.kind() == ErrorKind::PermissionDenied
                    || error.raw_os_error() == Some(1314) =>
            {
                return Ok(Some(Error::new("symlinkNotPermitted").with("name", &path)));
            }
            Err(error) => return Err(error.into()),
        }
        self.links.add(&path);
        self.counts.links.fetch_add(1, Ordering::Relaxed);
        Ok(None)
    }

    /// A hard link: another name of a file extracted earlier.
    fn hardlink(&self, path: &SafePath, meta: &EntryMeta) -> Result<Option<Error>> {
        let missing = || Error::new("linkTargetMissing").with("name", path);
        // A file outside the folder the selection is taken from is not
        // extracted, whatever stands at the same place inside it.
        let source = sanitize(meta.link_target.as_deref().unwrap_or_default())
            .ok()
            .and_then(|source| source.under(&self.base));
        let Some(source) = source else {
            return Ok(Some(missing()));
        };
        if let Err(reason) = self.links.check_parents(&source) {
            return Ok(Some(reason));
        }
        let original = source.to_path(&self.content);
        if self.links.is_link(&source) || fsx::existing(&original) != Existing::File {
            return Ok(Some(missing()));
        }
        let path = match self.settle(path, meta)? {
            Settled::Go(path) => path,
            Settled::Skip(reason) => return Ok(Some(reason)),
        };
        if let Some(reason) = self.prepare(&path)? {
            return Ok(Some(reason));
        }
        let target = path.to_path(&self.content);
        if fs::hard_link(&original, &target).is_err() {
            // The file system has no hard links: a copy takes real space.
            let size = fs::metadata(&original)?.len();
            self.guard.file(&meta.path).add(size)?;
            fs::copy(&original, &target)?;
            self.counts.bytes.fetch_add(size, Ordering::Relaxed);
        }
        self.counts.files.fetch_add(1, Ordering::Relaxed);
        Ok(None)
    }

    /// Extracts one entry; `Some` is the reason it was left out.
    fn place(&self, meta: &EntryMeta, data: &mut dyn Read) -> Result<Option<Error>> {
        let path = match sanitize(&meta.path) {
            Ok(path) => path,
            Err(reason) => return Ok(Some(reason)),
        };
        let Some(path) = path.under(&self.base) else {
            return Ok(Some(Error::new("outsideFolder").with("name", &meta.path)));
        };
        if path.is_root() {
            // "./" stands for the destination itself; a file has to have a name.
            return Ok((!meta.is_dir()).then(|| Error::new("invalidName").with("name", &meta.path)));
        }
        if let Err(reason) = self.links.check_parents(&path) {
            return Ok(Some(reason));
        }
        if let Some(reason) = self.blocked(&path) {
            return Ok(Some(reason));
        }
        match meta.kind {
            EntryKind::Directory => self.folder(&path, meta),
            EntryKind::File => self.file(&path, meta, data),
            EntryKind::Symlink => self.symlink(&path, meta),
            EntryKind::Hardlink => self.hardlink(&path, meta),
        }
    }

    /// Gives the extracted folders their times, deepest first: writing into
    /// a folder changes its time.
    fn finish_folders(&self) {
        let mut folders = mem::take(&mut *lock(&self.folders));
        folders.sort_by_key(|(path, _)| Reverse(path.parts().len()));
        for (path, meta) in folders {
            let folder = path.to_path(&self.content);
            if fsx::existing(&folder) == Existing::Folder {
                if let Err(error) = fsx::set_folder_times(&folder, &meta) {
                    tracing::debug!(entry = %meta.path, "the times were not set: {error}");
                }
            }
        }
    }

    fn outcome(&self) -> Outcome {
        Outcome {
            files: self.counts.files.load(Ordering::Relaxed),
            folders: self.counts.folders.load(Ordering::Relaxed),
            links: self.counts.links.load(Ordering::Relaxed),
            bytes: self.counts.bytes.load(Ordering::Relaxed),
            skipped: mem::take(&mut *lock(&self.skipped)),
            skipped_total: self.counts.skipped.load(Ordering::Relaxed),
        }
    }
}

impl EntrySink for Sink<'_> {
    fn accept(&self, entry: &Entry, data: &mut dyn Read) -> Result<()> {
        self.ctx.checkpoint()?;
        self.ctx.progress.entry(&entry.meta.path);
        self.counts.seen.fetch_add(1, Ordering::Relaxed);
        if let Some(reason) = self.place(&entry.meta, data)? {
            self.skip(&entry.meta, reason);
        }
        Ok(())
    }
}

/// Extracts the selected entries of an archive into a folder. Either the
/// whole selection arrives, or the destination stays as it was.
pub fn extract(
    reader: &mut dyn ArchiveReader,
    request: &ExtractRequest,
    ctx: &OpContext,
) -> Result<Outcome> {
    let destination = fsx::long(&request.destination)?;
    let existed = match fsx::existing(&destination) {
        Existing::Nothing => false,
        Existing::Folder => true,
        Existing::File | Existing::Link => {
            return Err(Error::new("badDestination").with("path", fsx::display(&destination)));
        }
    };
    let base = sanitize(&request.base)?;
    let mut staging = Staging::create(&destination)?;
    let guard = SizeGuard::new(
        request.options.limits,
        request.archive_size,
        fsx::free_space(&staging.root),
    );
    let sink = Sink {
        content: staging.content(),
        destination: destination.clone(),
        base,
        existed,
        options: request.options.clone(),
        guard,
        links: LinkTracker::new(),
        ctx,
        answer: Mutex::new(None),
        renamed: Mutex::new(HashSet::new()),
        clear: Mutex::new(HashSet::new()),
        folders: Mutex::new(Vec::new()),
        counts: Counts::default(),
        skipped: Mutex::new(Vec::new()),
        mark: &request.mark,
    };
    // A panic in a format library must not take the program down, and must
    // not leave half an archive behind: it becomes an error, and dropping
    // the staging folder removes what was written.
    guarded(|| {
        if reader.capabilities().has_index {
            // The list is at hand: what the archive declares is checked
            // before a byte is written, and the progress knows its total.
            let entries = reader.entries(ctx)?;
            let files = || {
                entries
                    .iter()
                    .filter(|entry| request.selection.contains(entry) && entry.meta.is_file())
            };
            sink.guard.declare(
                files().map(|entry| (entry.meta.path.as_str(), entry.meta.size.unwrap_or(0))),
            )?;
            let bytes: u64 = files().map(|entry| entry.meta.size.unwrap_or(0)).sum();
            let count = entries
                .iter()
                .filter(|entry| request.selection.contains(entry))
                .count();
            ctx.progress.total(Some(bytes), Some(count as u64));
        }
        reader.extract(&request.selection, &sink, ctx)?;
        ctx.checkpoint()?;
        if !request.selection.is_all() && sink.counts.seen.load(Ordering::Relaxed) == 0 {
            return Err(nothing_matched(&request.selection));
        }
        sink.finish_folders();
        Ok(())
    })?;
    commit(&mut staging, &destination)?;
    Ok(sink.outcome())
}

#[cfg(test)]
mod tests;

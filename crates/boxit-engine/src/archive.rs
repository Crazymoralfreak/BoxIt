//! An archive file opened for reading: its list, its content, its test.

use std::cell::RefCell;
use std::collections::{BTreeSet, HashSet};
use std::fmt::Write;
use std::io::{ErrorKind, Read};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Mutex, MutexGuard, PoisonError};

use boxit_codecs::names::NameEncoding;
use boxit_codecs::source::Source;
use boxit_codecs::{OpenOptions, Opened};
use boxit_core::{
    ArchiveFormat, ArchiveReader, Changes, Entry, EntrySink, Error, ExtractOptions, OpContext,
    Output, Password, Result, Selection,
};
use crc32fast::Hasher as Crc;
use serde::Serialize;
use sha2::{Digest, Sha256};

use crate::extract::{self, nothing_matched, ExtractRequest, Outcome};
use crate::fsx;
use crate::guarded::guarded;
use crate::text;

/// Size of the block the data of an entry is read in when it is tested.
const TEST_BLOCK: usize = 256 * 1024;

thread_local! {
    /// The block of this thread, kept between entries.
    static BLOCK: RefCell<Vec<u8>> = const { RefCell::new(Vec::new()) };
}

/// What the list of an archive shows.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Listing {
    /// The archive file.
    pub path: String,
    /// What the file is, whatever its name says.
    pub format: ArchiveFormat,
    /// Size of the archive: of all its volumes when it is cut into them.
    pub size: u64,
    /// The number of files the archive is in.
    pub volumes: u64,
    pub entries: Vec<Entry>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub comment: Option<String>,
    pub files: u64,
    pub folders: u64,
    /// Bytes of content, as far as the archive tells before it is unpacked.
    pub bytes: u64,
    /// Entries share compressed streams: taking one out means decoding
    /// the ones before it.
    pub solid: bool,
    /// Some content needs a password.
    pub encrypted: bool,
}

/// What a test does besides reading the content.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct TestOptions {
    /// Work out the sums of every file, and of the archive itself.
    pub sums: bool,
}

/// The sums of the content of a file of an archive.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Sum {
    pub path: String,
    pub size: u64,
    pub crc32: u32,
    /// In hexadecimal, the way `sha256sum` prints it.
    pub sha256: String,
}

/// An entry that did not pass the test, and what is wrong with it.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct Failed {
    /// The place of the entry in the archive.
    pub index: u64,
    pub path: String,
    pub reason: Error,
}

/// What reading an archive is for, when it is read to see what reads.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Purpose {
    /// To tell the user whether the archive is whole.
    Test,
    /// To write what reads into a new archive: every damaged entry is
    /// looked for, and an entry nobody has the password of is set apart
    /// and not given up on.
    Repair,
}

/// What reading an archive through told about its entries.
#[derive(Debug, Default)]
pub(crate) struct Survey {
    pub tested: Tested,
    /// The entries that were read to their end, files or not.
    pub read: u64,
    /// The entries that do not read.
    pub failed: BTreeSet<u64>,
    /// The entries that are locked with a password nobody gave, or one
    /// that opened nothing: they may be whole.
    pub unchecked: BTreeSet<u64>,
}

impl Survey {
    fn fail(&mut self, index: u64, path: String, reason: Error) {
        if self.failed.insert(index) {
            self.tested.failed_total += 1;
            if self.tested.failed.len() < FAILED_LISTED {
                self.tested.failed.push(Failed {
                    index,
                    path,
                    reason,
                });
            }
        }
    }
}

/// What a test of an archive read.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Tested {
    /// Files that passed the checks of the format.
    pub files: u64,
    /// Bytes of their content.
    pub bytes: u64,
    /// The entries that are damaged, the first [`FAILED_LISTED`] of them.
    pub failed: Vec<Failed>,
    pub failed_total: u64,
    /// Every entry that was asked about was read, or found damaged. An
    /// archive whose entries are compressed one after another cannot be
    /// read past a damaged one.
    pub complete: bool,
    /// The sums of the files, in the order of the archive, when they were
    /// asked for.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub sums: Option<Vec<Sum>>,
    /// SHA-256 of the archive file itself: of all its volumes put end to
    /// end when it is cut into them.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub archive_sha256: Option<String>,
}

/// How many damaged entries a test lists, and looks for: an archive with
/// more of them is damaged all over.
pub const FAILED_LISTED: usize = 1000;

/// Bytes as hexadecimal digits.
fn hex(bytes: &[u8]) -> String {
    let mut text = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        // Writing to a string cannot fail.
        let _ = write!(text, "{byte:02x}");
    }
    text
}

/// Which entries a test has dealt with.
#[derive(Default)]
struct Marks {
    /// Handed over by the archive and not read to their end.
    started: HashSet<u64>,
    /// Read to their end, or not files at all.
    done: HashSet<u64>,
}

/// Reads the content of every entry to its end and keeps none of it: the
/// formats check the data as it is decoded.
struct Check<'a> {
    ctx: &'a OpContext,
    options: TestOptions,
    files: AtomicU64,
    bytes: AtomicU64,
    /// Files that are locked with a password and passed their checks.
    locked: AtomicU64,
    marks: Mutex<Marks>,
    /// The sums, by the place of the entry in the archive.
    sums: Mutex<Vec<(u64, Sum)>>,
}

impl Check<'_> {
    fn marks(&self) -> MutexGuard<'_, Marks> {
        self.marks.lock().unwrap_or_else(PoisonError::into_inner)
    }

    fn finished(&self, entry: &Entry) {
        let mut marks = self.marks();
        marks.started.remove(&entry.index);
        marks.done.insert(entry.index);
    }

    /// The entry an error is about: one that was being read, by the name
    /// the error gives.
    fn blamed(&self, error: &Error, entries: &[Entry]) -> Option<u64> {
        let name = error.params.get("name")?;
        let marks = self.marks();
        entries
            .iter()
            .filter(|entry| entry.meta.path == *name && !marks.done.contains(&entry.index))
            .max_by_key(|entry| marks.started.contains(&entry.index))
            .map(|entry| entry.index)
    }
}

impl EntrySink for Check<'_> {
    fn accept(&self, entry: &Entry, data: &mut dyn Read) -> Result<()> {
        self.ctx.checkpoint()?;
        self.ctx.progress.entry(&entry.meta.path);
        if !entry.meta.is_file() {
            self.finished(entry);
            return Ok(());
        }
        self.marks().started.insert(entry.index);
        let mut sums = self.options.sums.then(|| (Crc::new(), Sha256::new()));
        let size = BLOCK.with_borrow_mut(|block| {
            block.resize(TEST_BLOCK, 0);
            let mut size = 0u64;
            loop {
                self.ctx.checkpoint()?;
                let count = match data.read(block) {
                    Ok(0) => break,
                    Ok(count) => count,
                    Err(error) if error.kind() == ErrorKind::Interrupted => continue,
                    Err(error) => return Err(Error::from(error)),
                };
                if let Some((crc, sha)) = &mut sums {
                    crc.update(&block[..count]);
                    sha.update(&block[..count]);
                }
                size += count as u64;
                self.ctx.progress.advance(count as u64);
            }
            Ok(size)
        })?;
        if let Some((crc, sha)) = sums {
            let crc32 = crc.finalize();
            // The format has checked this itself; a sum that is worked out
            // anyway is compared at no cost.
            if entry.crc32.is_some_and(|stored| stored != crc32) {
                return Err(Error::new("damagedEntry").with("name", &entry.meta.path));
            }
            let sum = Sum {
                path: entry.meta.path.clone(),
                size,
                crc32,
                sha256: hex(&sha.finalize()),
            };
            self.sums
                .lock()
                .unwrap_or_else(PoisonError::into_inner)
                .push((entry.index, sum));
        }
        self.bytes.fetch_add(size, Ordering::Relaxed);
        self.files.fetch_add(1, Ordering::Relaxed);
        if entry.encrypted {
            self.locked.fetch_add(1, Ordering::Relaxed);
        }
        self.finished(entry);
        Ok(())
    }
}

/// Keeps the start of the content of one file in memory.
struct Taken<'a> {
    ctx: &'a OpContext,
    limit: u64,
    /// The content, and whether there was more of it than the limit.
    content: Mutex<Option<(Vec<u8>, bool)>>,
}

impl EntrySink for Taken<'_> {
    fn accept(&self, entry: &Entry, data: &mut dyn Read) -> Result<()> {
        self.ctx.checkpoint()?;
        let mut content = Vec::new();
        let mut cut = false;
        if entry.meta.is_file() {
            BLOCK.with_borrow_mut(|block| {
                block.resize(TEST_BLOCK, 0);
                loop {
                    self.ctx.checkpoint()?;
                    let room = self.limit.saturating_sub(content.len() as u64);
                    // One byte past the limit tells whether there is more.
                    let wanted = (room.saturating_add(1)).min(TEST_BLOCK as u64) as usize;
                    let count = match data.read(&mut block[..wanted]) {
                        Ok(0) => break,
                        Ok(count) => count,
                        Err(error) if error.kind() == ErrorKind::Interrupted => continue,
                        Err(error) => return Err(Error::from(error)),
                    };
                    let kept = (count as u64).min(room) as usize;
                    content.extend_from_slice(&block[..kept]);
                    if kept < count {
                        cut = true;
                        break;
                    }
                }
                Ok(())
            })?;
        }
        *self.content.lock().unwrap_or_else(PoisonError::into_inner) = Some((content, cut));
        Ok(())
    }
}

/// What is known about an archive before it is opened. Names written
/// before archives agreed on UTF-8 are read the way the archives of the
/// country of the user usually are, unless an encoding is chosen.
pub fn open_options(password: Option<Password>, encoding: NameEncoding) -> OpenOptions {
    OpenOptions {
        password,
        encoding,
        hint: text::system_region(),
    }
}

/// The format of a file that is an archive BoxIt reads, told by the first
/// bytes of the file; `None` for any other file and for a folder.
pub fn recognize(path: &Path) -> Option<ArchiveFormat> {
    Archive::open(path, OpenOptions::default())
        .ok()
        .map(|archive| archive.format())
}

/// An archive file opened for reading.
pub struct Archive {
    path: PathBuf,
    format: ArchiveFormat,
    size: u64,
    volumes: u64,
    reader: Box<dyn ArchiveReader>,
}

impl Archive {
    /// Opens an archive; its format is told by its content. Nothing but
    /// the first bytes of the file is read yet.
    pub fn open(path: &Path, options: OpenOptions) -> Result<Self> {
        let path = fsx::long(path)?;
        let opened = guarded(|| boxit_codecs::open(&path, options)).map_err(|error| match error
            .code
            .as_str()
        {
            "notFound" | "accessDenied" => error.with("path", fsx::display(&path)),
            _ => error,
        })?;
        Ok(Self::opened(opened))
    }

    /// An archive that the formats have opened.
    pub(crate) fn opened(opened: Opened) -> Self {
        Self {
            // The first volume, whichever of them was opened.
            path: opened.path,
            format: opened.format,
            size: opened.size,
            volumes: opened.volumes as u64,
            reader: opened.reader,
        }
    }

    pub fn format(&self) -> ArchiveFormat {
        self.format
    }

    /// The path of the archive file, as it is shown.
    pub fn path(&self) -> String {
        fsx::display(&self.path)
    }

    /// Tells the progress how much work a selection is, when the archive
    /// knows without being read through.
    fn announce(&mut self, selection: &Selection, ctx: &OpContext) -> Result<()> {
        if !self.reader.capabilities().has_index {
            return Ok(());
        }
        let entries = self.reader.entries(ctx)?;
        let picked = || entries.iter().filter(|entry| selection.contains(entry));
        let bytes = picked().map(|entry| entry.meta.size.unwrap_or(0)).sum();
        ctx.progress
            .total(Some(bytes), Some(picked().count() as u64));
        Ok(())
    }

    /// The list of the archive. An archive without an index, such as a
    /// compressed tar, is read from its start to its end for it.
    pub fn list(&mut self, ctx: &OpContext) -> Result<Listing> {
        let (entries, comment) = guarded(|| {
            let entries = self.reader.entries(ctx)?;
            Ok((entries, self.reader.comment(ctx)?))
        })?;
        let folders = entries.iter().filter(|entry| entry.meta.is_dir()).count() as u64;
        let files = entries.iter().filter(|entry| entry.meta.is_file()).count() as u64;
        Ok(Listing {
            path: self.path(),
            format: self.format,
            size: self.size,
            volumes: self.volumes,
            comment,
            files,
            folders,
            bytes: entries
                .iter()
                .map(|entry| entry.meta.size.unwrap_or(0))
                .sum(),
            solid: self.reader.capabilities().solid,
            encrypted: entries.iter().any(|entry| entry.encrypted),
            entries,
        })
    }

    /// Extracts the selection into a folder; see [`extract::extract`].
    pub fn extract(
        &mut self,
        destination: &Path,
        selection: Selection,
        options: ExtractOptions,
        ctx: &OpContext,
    ) -> Result<Outcome> {
        self.extract_from("", destination, selection, options, ctx)
    }

    /// Extracts what is selected inside the folder `base` of the archive,
    /// without the folders above it; see [`ExtractRequest::base`].
    pub fn extract_from(
        &mut self,
        base: &str,
        destination: &Path,
        selection: Selection,
        options: ExtractOptions,
        ctx: &OpContext,
    ) -> Result<Outcome> {
        let request = ExtractRequest {
            destination: destination.to_path_buf(),
            base: base.to_owned(),
            selection,
            options,
            archive_size: self.size,
            mark: fsx::mark_of(&self.path),
        };
        extract::extract(self.reader.as_mut(), &request, ctx)
    }

    /// Size of the archive file when it was opened.
    pub fn size(&self) -> u64 {
        self.size
    }

    /// Reads the content of one file of the archive into memory, up to
    /// `limit` bytes of it. `true` comes with content that was cut short.
    /// Nothing is written to the disk.
    pub fn read(&mut self, index: u64, limit: u64, ctx: &OpContext) -> Result<(Vec<u8>, bool)> {
        let taken = Taken {
            ctx,
            limit,
            content: Mutex::new(None),
        };
        let selection = Selection::indexes(vec![index]);
        guarded(|| self.reader.extract(&selection, &taken, ctx))?;
        taken
            .content
            .into_inner()
            .unwrap_or_else(PoisonError::into_inner)
            .ok_or_else(|| nothing_matched(&selection))
    }

    /// SHA-256 of the archive file: of its volumes put end to end.
    fn digest(&self, ctx: &OpContext) -> Result<String> {
        let mut reader = Source::open(&self.path)?.reader()?;
        let mut sha = Sha256::new();
        BLOCK.with_borrow_mut(|block| {
            block.resize(TEST_BLOCK, 0);
            loop {
                ctx.checkpoint()?;
                match reader.read(block) {
                    Ok(0) => return Ok(()),
                    Ok(count) => sha.update(&block[..count]),
                    Err(error) if error.kind() == ErrorKind::Interrupted => {}
                    Err(error) => return Err(Error::from(error)),
                }
            }
        })?;
        Ok(hex(&sha.finalize()))
    }

    /// Reads the selection the way an extraction would and writes nothing.
    ///
    /// A damaged file does not end the test: the files that are damaged
    /// are listed, with the ones that share its compressed stream and
    /// cannot be read after it, and the rest are read. What is wrong with
    /// the archive as a whole, a password that does not fit and a
    /// cancellation end the test with an error.
    pub fn test(
        &mut self,
        selection: &Selection,
        options: TestOptions,
        ctx: &OpContext,
    ) -> Result<Tested> {
        let mut tested = Tested::default();
        if options.sums {
            tested.archive_sha256 = Some(self.digest(ctx)?);
        }
        guarded(|| self.announce(selection, ctx))?;
        let survey = self.survey(selection, options, Purpose::Test, ctx)?;
        if !selection.is_all() && survey.read == 0 && survey.failed.is_empty() {
            return Err(nothing_matched(selection));
        }
        Ok(Tested {
            archive_sha256: tested.archive_sha256,
            ..survey.tested
        })
    }

    /// Reads the selection and tells what of it reads; see [`Survey`].
    pub(crate) fn survey(
        &mut self,
        selection: &Selection,
        options: TestOptions,
        purpose: Purpose,
        ctx: &OpContext,
    ) -> Result<Survey> {
        let check = Check {
            ctx,
            options,
            files: AtomicU64::new(0),
            bytes: AtomicU64::new(0),
            locked: AtomicU64::new(0),
            marks: Mutex::default(),
            sums: Mutex::default(),
        };
        let mut survey = Survey {
            tested: Tested {
                complete: true,
                ..Tested::default()
            },
            ..Survey::default()
        };
        // The entries whose damage may be a password that does not fit.
        let mut doubted: Vec<u64> = Vec::new();
        let mut left = selection.clone();
        guarded(|| loop {
            let Err(error) = self.reader.extract(&left, &check, ctx) else {
                return ctx.checkpoint();
            };
            // Only an archive that knows its entries can go on past one.
            if !self.reader.capabilities().has_index {
                return Err(error);
            }
            let entries = self.reader.entries(ctx)?;
            let waits = |survey: &Survey, entry: &Entry| {
                selection.contains(entry)
                    && !check.marks().done.contains(&entry.index)
                    && !survey.failed.contains(&entry.index)
                    && !survey.unchecked.contains(&entry.index)
            };
            match (error.code.as_str(), purpose) {
                ("damagedEntry", _) | ("damagedOrWrongPassword", Purpose::Repair) => {
                    let Some(index) = check
                        .blamed(&error, &entries)
                        .filter(|index| !survey.failed.contains(index))
                    else {
                        return Err(error);
                    };
                    let name = entries[index as usize].meta.path.clone();
                    let tied: Vec<u64> = self
                        .reader
                        .tied(index, ctx)?
                        .into_iter()
                        .filter(|tied| {
                            entries
                                .get(*tied as usize)
                                .is_some_and(|entry| waits(&survey, entry))
                        })
                        .collect();
                    if error.code == "damagedOrWrongPassword" {
                        doubted.push(index);
                        doubted.extend(&tied);
                    }
                    survey.fail(index, name.clone(), error);
                    for tied in tied {
                        let path = entries[tied as usize].meta.path.clone();
                        let reason = Error::new("damagedBefore")
                            .with("name", &path)
                            .with("before", &name);
                        survey.fail(tied, path, reason);
                    }
                }
                // Nobody has the password: what it locks cannot be told
                // whole from damaged.
                ("passwordRequired" | "wrongPassword", Purpose::Repair) => {
                    let locked: Vec<u64> = entries
                        .iter()
                        .filter(|entry| entry.encrypted && waits(&survey, entry))
                        .map(|entry| entry.index)
                        .collect();
                    if locked.is_empty() {
                        return Err(error);
                    }
                    survey.unchecked.extend(locked);
                }
                _ => return Err(error),
            }
            let rest: Vec<u64> = entries
                .iter()
                .filter(|entry| waits(&survey, entry))
                .map(|entry| entry.index)
                .collect();
            if rest.is_empty() {
                return Ok(());
            }
            if purpose == Purpose::Test && survey.failed.len() >= FAILED_LISTED {
                survey.tested.complete = false;
                return Ok(());
            }
            left = Selection::indexes(rest);
        })?;
        // Not one locked file was read: the password itself is in doubt,
        // and with it the damage of what it did not open.
        if check.locked.load(Ordering::Relaxed) == 0 {
            for index in doubted {
                survey.failed.remove(&index);
                survey.unchecked.insert(index);
            }
            let failed = &survey.failed;
            survey
                .tested
                .failed
                .retain(|listed| failed.contains(&listed.index));
            survey.tested.failed_total = failed.len() as u64;
        }
        let marks = check
            .marks
            .into_inner()
            .unwrap_or_else(PoisonError::into_inner);
        survey.read = marks.done.len() as u64;
        survey.tested.files = check.files.into_inner();
        survey.tested.bytes = check.bytes.into_inner();
        if options.sums {
            let mut sums = check
                .sums
                .into_inner()
                .unwrap_or_else(PoisonError::into_inner);
            sums.sort_unstable_by_key(|(index, _)| *index);
            survey.tested.sums = Some(sums.into_iter().map(|(_, sum)| sum).collect());
        }
        Ok(survey)
    }

    /// Writes the archive anew, with the changes; see
    /// [`ArchiveReader::rewrite`].
    pub(crate) fn rewrite(
        &mut self,
        changes: Changes,
        output: &mut dyn Output,
        ctx: &OpContext,
    ) -> Result<()> {
        let format = self.format;
        guarded(|| self.reader.rewrite(changes, output, ctx)).map_err(|error| {
            if error.code == "cannotModify" {
                error.with("format", format.id())
            } else {
                error
            }
        })
    }

    /// The entries of the archive, as its reader lists them.
    pub fn entries(&mut self, ctx: &OpContext) -> Result<Vec<Entry>> {
        guarded(|| self.reader.entries(ctx))
    }
}
#[cfg(test)]
mod tests;

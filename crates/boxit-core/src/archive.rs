//! The contract between the formats and everything that uses them.
//!
//! Reading goes in one pass with a visitor: the reader walks the archive in
//! its own order and hands every selected entry to an [`EntrySink`] together
//! with its data. Streaming archives (`tar.gz`) and solid ones are never read
//! twice that way, and a format that can decode entries independently may
//! hand them over from several threads at once.

use std::collections::{BTreeMap, BTreeSet};
use std::io::{self, Read, Seek, Write};
use std::num::NonZero;
use std::sync::Arc;
use std::thread;

use crate::control::Control;
use crate::entry::{Entry, EntryMeta, Selection, Timestamp};
use crate::error::{Error, Result};
use crate::format::ArchiveFormat;
use crate::options::{CreateOptions, Password};

/// What a reader can do with its archive, so callers can plan around it.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Capabilities {
    /// Any entry can be read without reading the ones before it.
    pub random_access: bool,
    /// Entries share compressed streams: reading one may mean decoding many.
    pub solid: bool,
    /// Entries may reach the sink from several threads at once.
    pub parallel: bool,
    /// The list of entries is stored apart from the data, so listing does
    /// not read the whole archive.
    pub has_index: bool,
}

/// Receives progress of an operation. Called from the threads that do the
/// work, often: an implementation must be cheap and must not block.
pub trait Progress: Send + Sync {
    /// The amount of work, once it is known: unpacked bytes and entries.
    fn total(&self, _bytes: Option<u64>, _entries: Option<u64>) {}
    /// Work on an entry has started.
    fn entry(&self, _path: &str) {}
    /// More bytes of content have been processed.
    fn advance(&self, _bytes: u64) {}
}

/// For operations nobody watches.
pub struct NoProgress;

impl Progress for NoProgress {}

/// What a password is asked for.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PasswordRequest {
    /// Path of the archive.
    pub archive: String,
    /// The entry that needs the password; `None` when the list of entries
    /// itself is encrypted.
    pub entry: Option<String>,
    /// The password given before was wrong.
    pub retry: bool,
}

/// A file to extract meets one that exists.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Conflict {
    /// Where the file would go.
    pub path: String,
    pub existing_size: u64,
    pub existing_modified: Option<Timestamp>,
    pub entry: EntryMeta,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ConflictAnswer {
    Skip,
    Overwrite,
    Rename,
    /// The same for this file and every later conflict of the operation.
    SkipAll,
    OverwriteAll,
    RenameAll,
    /// Stop the whole operation.
    Cancel,
}

/// Questions an operation may have for the user. Called from the threads
/// that do the work; they wait for the answer.
pub trait Interaction: Send + Sync {
    /// A password is needed. `None` gives up on what needed it.
    fn password(&self, _request: &PasswordRequest) -> Option<Password> {
        None
    }

    /// A file exists where one is to be extracted and the policy is to ask.
    fn conflict(&self, _conflict: &Conflict) -> ConflictAnswer {
        ConflictAnswer::Skip
    }

    /// The archive holds a symbolic link and the policy is to ask. `true`
    /// creates the link.
    fn symlink(&self, _link: &EntryMeta) -> bool {
        false
    }
}

/// Nobody to ask: no password, existing files are kept, links are left out.
pub struct NoInteraction;

impl Interaction for NoInteraction {}

/// How much of the machine an operation may take.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Budget {
    /// Worker threads, at least one.
    pub threads: usize,
    /// Memory for buffers and dictionaries, in bytes.
    pub memory: u64,
}

impl Budget {
    /// Memory an operation may take unless told otherwise.
    pub const DEFAULT_MEMORY: u64 = 400 * 1024 * 1024;

    pub fn with_threads(threads: usize) -> Self {
        Self {
            threads: threads.max(1),
            memory: Self::DEFAULT_MEMORY,
        }
    }
}

impl Default for Budget {
    fn default() -> Self {
        let threads = thread::available_parallelism().map_or(1, NonZero::get);
        Self::with_threads(threads)
    }
}

/// Everything an operation shares with the outside while it runs.
#[derive(Clone)]
pub struct OpContext {
    pub control: Control,
    pub progress: Arc<dyn Progress>,
    pub interaction: Arc<dyn Interaction>,
    pub budget: Budget,
}

impl OpContext {
    /// A context for an operation nobody watches or answers.
    pub fn silent() -> Self {
        Self {
            control: Control::new(),
            progress: Arc::new(NoProgress),
            interaction: Arc::new(NoInteraction),
            budget: Budget::default(),
        }
    }

    /// See [`Control::checkpoint`].
    pub fn checkpoint(&self) -> Result<()> {
        self.control.checkpoint()
    }
}

/// Receives the entries of an archive as it is read.
///
/// `data` holds the content of the entry and is empty for everything but
/// files. The sink may leave any part of it unread. Readers that report
/// [`Capabilities::parallel`] call `accept` from several threads.
pub trait EntrySink: Sync {
    fn accept(&self, entry: &Entry, data: &mut dyn Read) -> Result<()>;
}

/// An open archive.
pub trait ArchiveReader: Send {
    fn capabilities(&self) -> Capabilities;

    /// The entries of the archive in its own order. For an archive without
    /// an index this reads the whole archive, and tells the progress about
    /// every entry it finds on the way.
    fn entries(&mut self, ctx: &OpContext) -> Result<Vec<Entry>>;

    /// Hands every entry of the selection to the sink, with its data.
    fn extract(&mut self, pick: &Selection, sink: &dyn EntrySink, ctx: &OpContext) -> Result<()>;

    /// The comment of the archive, for the formats that have one.
    fn comment(&mut self, _ctx: &OpContext) -> Result<Option<String>> {
        Ok(None)
    }

    /// The entries that cannot be read when the entry `index` cannot: the
    /// ones that follow it in the compressed stream they share.
    fn tied(&mut self, index: u64, ctx: &OpContext) -> Result<Vec<u64>> {
        if !self.capabilities().solid {
            return Ok(Vec::new());
        }
        let entries = self.entries(ctx)?;
        Ok(entries
            .iter()
            .map(|entry| entry.index)
            .filter(|other| *other > index)
            .collect())
    }

    /// Writes the archive anew with the changes, into `output`. What stays
    /// is copied as it is compressed wherever the format lets it, so a
    /// change costs a copy of the archive and not its compression. The
    /// archive that is read stays as it was.
    fn rewrite(
        &mut self,
        _changes: Changes,
        _output: &mut dyn Output,
        _ctx: &OpContext,
    ) -> Result<()> {
        Err(Error::new("cannotModify"))
    }
}

/// Where an archive is written: a file, or what stands for one.
pub trait Output: Write + Seek + Send {}

impl<T: Write + Seek + Send> Output for T {}

/// What an archive that is written anew differs in from the one it is
/// written from.
pub struct Changes {
    /// The entries that are left out, by their indexes.
    pub removed: BTreeSet<u64>,
    /// The new paths of entries, by their indexes.
    pub renamed: BTreeMap<u64, String>,
    /// The entries to add, after the ones that stay.
    pub added: Vec<Item>,
    /// How what is added is compressed. The format of the options is not
    /// looked at: an archive stays of the format it is.
    pub options: CreateOptions,
}

impl Changes {
    /// No changes at all: the archive is written as it is.
    pub fn none(format: ArchiveFormat) -> Self {
        Self {
            removed: BTreeSet::new(),
            renamed: BTreeMap::new(),
            added: Vec::new(),
            options: CreateOptions::new(format),
        }
    }
}

/// Content that can be read from its start more than once and from any
/// thread: a file on the disk, a block of memory.
pub trait DataSource: Send + Sync {
    fn open(&self) -> Result<Box<dyn Read + Send + '_>>;
}

impl DataSource for Vec<u8> {
    fn open(&self) -> Result<Box<dyn Read + Send + '_>> {
        Ok(Box::new(self.as_slice()))
    }
}

/// An entry to store in an archive, with the place its content comes from.
pub struct Item {
    pub meta: EntryMeta,
    /// The content of a file; `None` for everything else.
    pub data: Option<Box<dyn DataSource>>,
}

impl Item {
    pub fn new(meta: EntryMeta, data: impl DataSource + 'static) -> Self {
        Self {
            meta,
            data: Some(Box::new(data)),
        }
    }

    /// An entry without content: a folder, a link.
    pub fn bare(meta: EntryMeta) -> Self {
        Self { meta, data: None }
    }
}

/// An archive being written.
pub trait ArchiveWriter: Send {
    /// Appends an entry. `data` is read to its end for files and ignored for
    /// everything else.
    fn add(&mut self, meta: &EntryMeta, data: &mut dyn Read, ctx: &OpContext) -> Result<()>;

    /// Appends many entries, in the given order. A format that compresses
    /// entries apart from each other does it on several threads, and a solid
    /// one groups them into blocks; that is why this is the way to store a
    /// set of files, and [`ArchiveWriter::add`] the way to store one.
    fn add_all(&mut self, items: Vec<Item>, ctx: &OpContext) -> Result<()> {
        for item in items {
            ctx.checkpoint()?;
            match &item.data {
                Some(data) => self.add(&item.meta, &mut data.open()?, ctx)?,
                None => self.add(&item.meta, &mut io::empty(), ctx)?,
            }
        }
        Ok(())
    }

    /// Writes what closes the archive. An archive that was not finished is
    /// not a valid archive.
    fn finish(self: Box<Self>) -> Result<()>;
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_silent_context_has_safe_answers() {
        let ctx = OpContext::silent();
        assert!(ctx.checkpoint().is_ok());
        assert!(ctx.budget.threads >= 1);
        let request = PasswordRequest {
            archive: String::from("a.zip"),
            entry: None,
            retry: false,
        };
        assert_eq!(ctx.interaction.password(&request), None);
        assert!(!ctx.interaction.symlink(&EntryMeta::symlink("a", "b")));
        let conflict = Conflict {
            path: String::from("a"),
            existing_size: 0,
            existing_modified: None,
            entry: EntryMeta::file("a", 0),
        };
        assert_eq!(ctx.interaction.conflict(&conflict), ConflictAnswer::Skip);
        ctx.control.cancel();
        assert!(ctx.clone().checkpoint().is_err());
    }

    #[test]
    fn a_budget_always_has_a_thread() {
        assert_eq!(Budget::with_threads(0).threads, 1);
        assert_eq!(Budget::with_threads(8).threads, 8);
    }
}

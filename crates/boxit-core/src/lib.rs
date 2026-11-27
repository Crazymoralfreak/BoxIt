//! The model of BoxIt: archive formats, entries, options, errors and the
//! contract every format implements. This crate does no input or output, so
//! everything here can be tested without a disk.

#![forbid(unsafe_code)]

pub mod archive;
pub mod control;
pub mod entry;
pub mod error;
pub mod format;
pub mod options;

pub use archive::{
    ArchiveReader, ArchiveWriter, Budget, Capabilities, Changes, Conflict, ConflictAnswer,
    DataSource, EntrySink, Interaction, Item, NoInteraction, NoProgress, OpContext, Output,
    PasswordRequest, Progress,
};
pub use control::Control;
pub use entry::{Entry, EntryKind, EntryMeta, Selection, Timestamp};
pub use error::{Error, Result};
pub use format::{sniff, ArchiveFormat, Signature, Stream, SNIFF_LENGTH};
pub use options::{
    ConflictPolicy, CreateOptions, ExtractOptions, Level, Limits, Method, Password, SymlinkPolicy,
};

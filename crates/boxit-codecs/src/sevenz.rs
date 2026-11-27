//! 7z.
//!
//! A 7z archive keeps its list apart from the data, and the data in blocks:
//! every block is one compressed stream that holds one file or, in a solid
//! archive, many. A block is the unit of everything here. Reading decodes
//! the blocks that hold selected files, several at once, each from its start
//! up to the last file wanted. Writing sorts the files so that alike ones
//! meet in a block, compresses small blocks on all threads at once, and lets
//! the encoder of a large block use the threads itself.

use std::borrow::Cow;
use std::io::{self, BufReader, Read, Seek, Write};
use std::sync::mpsc::{self, Receiver, SyncSender};
use std::sync::Arc;
use std::thread;

use boxit_core::{
    ArchiveFormat, ArchiveReader, ArchiveWriter, Budget, Capabilities, Changes, CreateOptions,
    DataSource, Entry, EntryKind, EntryMeta, EntrySink, Error, Item, Level, Method, OpContext,
    Output, Password, Result, Selection, Timestamp,
};
use rayon::prelude::*;
use sevenz_rust2::encoder_options::{
    AesEncoderOptions, Bzip2Options, DeflateOptions, Lzma2Options, LzmaOptions, PpmdOptions,
};
use sevenz_rust2::{
    prepare_block, Archive, ArchiveEntry, ArchiveWriter as SzWriter, Block, BlockDecoder,
    EncoderConfiguration, EncoderMethod, Error as SzError, NtTime, Password as SzPassword,
    PreparedBlock, SourceReader as Tallied,
};

use crate::plan;
use crate::source::Source;
use crate::support::{
    self, damaged, decode_error, failed_named, from_windows_time, read_link_target,
    to_windows_time, unsupported_method, Passwords, Stoppable, Watched,
};
use crate::OpenOptions;

const KIB: u64 = 1024;
const MIB: u64 = 1024 * KIB;

/// Set in the attributes of an entry made on Unix: the upper half of them
/// holds the mode of the file, as 7-Zip writes it.
const UNIX_EXTENSION: u32 = 0x8000;
const MODE_TYPE: u32 = 0o170_000;
const MODE_LINK: u32 = 0o120_000;
const MODE_FILE: u32 = 0o100_000;
const MODE_FOLDER: u32 = 0o040_000;
const MODE_PERMISSIONS: u32 = 0o7777;
const ATTRIBUTE_DIRECTORY: u32 = 0x10;
const ATTRIBUTE_ARCHIVE: u32 = 0x20;
/// The attributes of Windows, without the mark of the Unix extension.
const ATTRIBUTES: u32 = 0x7FFF;

/// Size of the block unwanted data of a solid block is read in.
const DRAIN_BLOCK: usize = 64 * 1024;
/// Size of the buffer between a file and the encoder, which asks for four
/// kilobytes at a time.
const READ_BUFFER: usize = 64 * 1024;

/// Two to this power is how many times the password is hashed into the key:
/// what 7-Zip itself writes.
const KEY_CYCLES_POWER: u8 = 19;

/// The size of a solid block is the dictionary times this, within limits:
/// the rule of 7-Zip.
const SOLID_DICTIONARIES: u64 = 128;
const SOLID_BLOCK_MIN: u64 = 64 * MIB;
const SOLID_BLOCK_MAX: u64 = 4096 * MIB;

/// The most input the blocks compressed in memory at once may hold, and the
/// most blocks.
const WAVE_BYTES_MIN: u64 = 32 * MIB;
const WAVE_BLOCKS: usize = 4096;

/// Size of the pieces the files of a block that is compressed again go
/// from the decoder to the encoder in, and how many of them may wait.
const PIPE_BLOCK: usize = 256 * 1024;
const PIPE_DEPTH: usize = 8;

/// The smallest dictionary a block is given, however small its input.
const DICTIONARY_MIN: u64 = 64 * KIB;
const DICTIONARY_MAX: u64 = 1 << 30;

fn sz_password(password: &Password) -> SzPassword {
    SzPassword::new(password.expose())
}

/// An error of reading an archive.
fn convert(error: SzError) -> Error {
    match error {
        SzError::Io(error, _) | SzError::FileOpen(error, _) | SzError::MaybeBadPassword(error) => {
            decode_error(error)
        }
        SzError::PasswordRequired => Error::new("passwordRequired"),
        SzError::UnsupportedCompressionMethod(method) => unsupported_method(method),
        SzError::Unsupported(what) => Error::new("unsupportedArchive").detail(what),
        SzError::UnsupportedVersion { major, minor } => {
            Error::new("unsupportedArchive").detail(format!("7z {major}.{minor}"))
        }
        SzError::MaxMemLimited { actaul_kb, .. } => {
            Error::new("notEnoughMemory").with("needed", actaul_kb as u64 * KIB)
        }
        other => damaged(other),
    }
}

/// An error of writing an archive: of the output, or one of ours that came
/// through the encoder from the file being read.
fn write_error(error: SzError) -> Error {
    match error {
        SzError::Io(error, _) | SzError::FileOpen(error, _) | SzError::MaybeBadPassword(error) => {
            error.into()
        }
        other => Error::new("internal").detail(other),
    }
}

/// The name of a coder as the list of an archive shows it; `None` for
/// encryption, which is not a compression.
fn coder_name(id: &[u8]) -> Option<Cow<'static, str>> {
    Some(Cow::Borrowed(match id {
        EncoderMethod::ID_AES256_SHA256 => return None,
        EncoderMethod::ID_COPY => Method::Store.name(),
        EncoderMethod::ID_LZMA => Method::Lzma.name(),
        EncoderMethod::ID_LZMA2 => Method::Lzma2.name(),
        EncoderMethod::ID_PPMD => Method::Ppmd.name(),
        EncoderMethod::ID_BZIP2 => Method::Bzip2.name(),
        EncoderMethod::ID_DEFLATE => Method::Deflate.name(),
        EncoderMethod::ID_DEFLATE64 => Method::Deflate64.name(),
        EncoderMethod::ID_ZSTD => Method::Zstd.name(),
        other => match EncoderMethod::by_id(other) {
            Some(method) => method.name(),
            None => return Some(Cow::Owned(format!("{other:02X?}"))),
        },
    }))
}

fn method_name(block: &Block) -> Option<Cow<'static, str>> {
    let mut names = block
        .coders
        .iter()
        .filter_map(|coder| coder_name(coder.encoder_method_id()));
    let first = names.next()?;
    Some(names.fold(first, |all, name| Cow::Owned(format!("{all} + {name}"))))
}

fn is_encrypted(block: &Block) -> bool {
    block
        .coders
        .iter()
        .any(|coder| coder.encoder_method_id() == EncoderMethod::ID_AES256_SHA256)
}

/// The dictionary of LZMA2 from the byte that describes it.
fn lzma2_dictionary(bits: u8) -> u64 {
    let bits = u32::from(bits & 0x3F);
    if bits >= 40 {
        u64::from(u32::MAX)
    } else {
        u64::from(2 | (bits & 1)) << (bits / 2 + 11)
    }
}

/// Memory the decoder of a block takes: its dictionary or its model.
fn block_memory(block: &Block) -> u64 {
    let largest = block
        .coders
        .iter()
        .map(|coder| {
            let properties = coder.properties();
            match coder.encoder_method_id() {
                EncoderMethod::ID_LZMA2 => {
                    properties.first().map_or(0, |bits| lzma2_dictionary(*bits))
                }
                EncoderMethod::ID_LZMA | EncoderMethod::ID_PPMD => properties
                    .get(1..5)
                    .and_then(|bytes| bytes.try_into().ok())
                    .map_or(0, |bytes| u64::from(u32::from_le_bytes(bytes))),
                _ => 0,
            }
        })
        .max()
        .unwrap_or(0);
    largest + MIB
}

fn time(present: bool, time: NtTime) -> Option<Timestamp> {
    present.then(|| u64::from(time)).and_then(from_windows_time)
}

fn describe_entry(index: usize, file: &ArchiveEntry) -> Entry {
    let attributes = file
        .has_windows_attributes
        .then_some(file.windows_attributes);
    let mode = attributes
        .filter(|attributes| attributes & UNIX_EXTENSION != 0)
        .map(|attributes| attributes >> 16);
    let kind = if file.is_directory {
        EntryKind::Directory
    } else if mode.is_some_and(|mode| mode & MODE_TYPE == MODE_LINK) {
        EntryKind::Symlink
    } else {
        EntryKind::File
    };
    Entry {
        index: index as u64,
        meta: EntryMeta {
            // Archives made on Windows by other programs may keep its separator.
            path: file.name.replace('\\', "/"),
            kind,
            link_target: None,
            size: Some(if file.is_directory { 0 } else { file.size }),
            modified: time(file.has_last_modified_date, file.last_modified_date),
            created: time(file.has_creation_date, file.creation_date),
            accessed: time(file.has_access_date, file.access_date),
            unix_mode: mode.map(|mode| mode & MODE_PERMISSIONS),
            windows_attributes: attributes
                .map(|attributes| attributes & ATTRIBUTES)
                .filter(|attributes| *attributes != 0),
        },
        packed_size: None,
        crc32: file.has_crc.then_some(file.crc as u32),
        encrypted: false,
        method: None,
        comment: None,
    }
}

/// Size of every block of an archive as it is compressed.
fn packed_sizes(archive: &Archive) -> Vec<u64> {
    let first_pack = archive.stream_map.block_first_pack_stream_index();
    let pack_sizes = archive.pack_sizes();
    (0..archive.blocks.len())
        .map(|index| {
            let start = first_pack.get(index).copied().unwrap_or(0);
            let end = first_pack
                .get(index + 1)
                .copied()
                .unwrap_or(pack_sizes.len());
            pack_sizes
                .get(start..end)
                .unwrap_or_default()
                .iter()
                .fold(0u64, |sum, size| sum.saturating_add(*size))
        })
        .collect()
}

/// The entries of an archive, with what their blocks say about them.
fn describe(archive: &Archive) -> Vec<Entry> {
    let blocks: Vec<(u64, Option<Cow<'static, str>>, bool)> = archive
        .blocks
        .iter()
        .zip(packed_sizes(archive))
        .map(|(block, packed)| (packed, method_name(block), is_encrypted(block)))
        .collect();
    // The files of a solid block share its bytes: the first one shows them.
    let mut shown = vec![false; blocks.len()];
    let mut entries = Vec::with_capacity(archive.files.len());
    for (index, file) in archive.files.iter().enumerate() {
        // A mark that a file is to be deleted on extraction: of archives
        // made as updates of other archives, and nothing to show.
        if file.is_anti_item {
            continue;
        }
        let mut entry = describe_entry(index, file);
        let block = archive
            .stream_map
            .file_block_index
            .get(index)
            .copied()
            .flatten()
            .filter(|_| file.has_stream);
        if let Some((block, (packed, method, encrypted))) =
            block.and_then(|block| Some((block, blocks.get(block)?)))
        {
            entry.method.clone_from(method);
            entry.encrypted = *encrypted;
            if !shown[block] {
                shown[block] = true;
                entry.packed_size = Some(*packed);
            }
        }
        entries.push(entry);
    }
    entries
}

struct Loaded {
    archive: Archive,
    entries: Vec<Entry>,
    /// The list itself is encrypted.
    locked: bool,
}

/// A 7z archive opened for reading.
pub struct SevenZReader {
    source: Source,
    passwords: Passwords,
    loaded: Option<Loaded>,
}

impl SevenZReader {
    pub fn new(source: Source, options: OpenOptions) -> Self {
        let passwords = Passwords::new(
            source.path().to_string_lossy().into_owned(),
            options.password,
        );
        Self {
            source,
            passwords,
            loaded: None,
        }
    }

    /// Reads the list of the archive on first use. The list itself may be
    /// encrypted: the password is asked for until it opens.
    fn load(&mut self, ctx: &OpContext) -> Result<&Loaded> {
        if self.loaded.is_none() {
            let mut reader = self.source.reader()?;
            // Without a password first: a list that reads so is not
            // encrypted, whatever password is known for the content.
            let mut attempt: Option<(Password, u64)> = None;
            let mut locked = false;
            let archive = loop {
                ctx.checkpoint()?;
                let generation = attempt.as_ref().map(|(_, generation)| *generation);
                let password = attempt
                    .as_ref()
                    .map_or_else(SzPassword::empty, |(password, _)| sz_password(password));
                match Archive::read(&mut reader, &password) {
                    Ok(archive) => break archive,
                    Err(SzError::PasswordRequired) => {
                        locked = true;
                        attempt = Some(self.passwords.get(ctx, None, generation)?);
                    }
                    // A wrong password shows as a list that does not decode.
                    Err(SzError::MaybeBadPassword(_)) if !password.is_empty() => {
                        attempt = Some(self.passwords.get(ctx, None, generation)?);
                    }
                    Err(error) => return Err(convert(error)),
                }
            };
            // The password that opened the list is the right one.
            if let Some((_, generation)) = attempt {
                self.passwords.prove(generation);
            }
            let entries = describe(&archive);
            self.loaded = Some(Loaded {
                archive,
                entries,
                locked,
            });
        }
        Ok(self.loaded.as_ref().expect("the archive was just loaded"))
    }
}

/// Why the decoding of a block stopped.
struct Stopped {
    error: Error,
    /// The data did not decode, as opposed to the sink refusing it.
    data: bool,
}

/// Reads data nobody wants to its end: a solid block is decoded in order,
/// and the check of an entry is made at its end.
fn drain(data: &mut dyn Read, ctx: &OpContext) -> std::result::Result<(), Stopped> {
    let mut block = vec![0u8; DRAIN_BLOCK];
    loop {
        ctx.checkpoint()
            .map_err(|error| Stopped { error, data: false })?;
        match data.read(&mut block) {
            Ok(0) => return Ok(()),
            Ok(_) => {}
            Err(error) if error.kind() == io::ErrorKind::Interrupted => {}
            Err(error) => {
                return Err(Stopped {
                    error: decode_error(error),
                    data: true,
                })
            }
        }
    }
}

/// A piece of the files of a block on its way from the decoder to the
/// encoder that compresses them again.
enum Piece {
    Data(Vec<u8>),
    /// The file ends here.
    End,
    Failed(Error),
}

/// One file of a block that is compressed again, read as the decoder
/// gives it. The files of the block come one after another through one
/// pipe, and each of them reads up to its end.
struct Piped<'a> {
    pieces: &'a Receiver<Piece>,
    held: Vec<u8>,
    at: usize,
    done: bool,
}

impl<'a> Piped<'a> {
    fn new(pieces: &'a Receiver<Piece>) -> Self {
        Self {
            pieces,
            held: Vec::new(),
            at: 0,
            done: false,
        }
    }
}

impl Read for Piped<'_> {
    fn read(&mut self, buffer: &mut [u8]) -> io::Result<usize> {
        loop {
            let left = &self.held[self.at..];
            if !left.is_empty() {
                let count = left.len().min(buffer.len());
                buffer[..count].copy_from_slice(&left[..count]);
                self.at += count;
                return Ok(count);
            }
            if self.done || buffer.is_empty() {
                return Ok(0);
            }
            match self.pieces.recv() {
                Ok(Piece::Data(bytes)) => {
                    self.held = bytes;
                    self.at = 0;
                }
                Ok(Piece::End) => self.done = true,
                Ok(Piece::Failed(error)) => return Err(error.into_io()),
                Err(_) => {
                    let error = Error::new("internal").detail("the decoder of a block stopped");
                    return Err(error.into_io());
                }
            }
        }
    }
}

/// Sends the content of a file to the encoder, to its end.
fn pipe(data: &mut dyn Read, pieces: &SyncSender<Piece>, ctx: &OpContext) -> Result<()> {
    let gone = |_| Error::new("internal").detail("the encoder of a block stopped");
    loop {
        ctx.checkpoint()?;
        let mut block = vec![0u8; PIPE_BLOCK];
        let mut count = 0;
        while count < block.len() {
            match data.read(&mut block[count..]) {
                Ok(0) => break,
                Ok(read) => count += read,
                Err(error) if error.kind() == io::ErrorKind::Interrupted => {}
                Err(error) => return Err(decode_error(error)),
            }
        }
        let ended = count < block.len();
        if count > 0 {
            block.truncate(count);
            pieces.send(Piece::Data(block)).map_err(gone)?;
        }
        if ended {
            return pieces.send(Piece::End).map_err(gone);
        }
    }
}

/// An error of copying a block as it is: what the library says of the
/// block is said of the archive it is copied from.
fn copy_error(error: SzError) -> Error {
    match error {
        SzError::Other(what) => damaged(what),
        other => write_error(other),
    }
}

impl SevenZReader {
    /// Decodes a block up to the last of its files that stay, and sends
    /// those to the encoder.
    fn feed(
        &self,
        archive: &Archive,
        block: usize,
        staying: &[usize],
        password: &SzPassword,
        pieces: &SyncSender<Piece>,
        ctx: &OpContext,
    ) -> Result<()> {
        let files = &archive.files;
        let encrypted = archive.blocks.get(block).is_some_and(is_encrypted);
        let first = *archive
            .stream_map
            .block_first_file_index
            .get(block)
            .ok_or_else(|| damaged("a block without files"))?;
        let Some(last) = staying.last().copied() else {
            return Ok(());
        };
        let name = |index: usize| files.get(index).map_or("", |file| file.name.as_str());
        let threads = (ctx.budget.threads / 2).clamp(1, 256) as u32;
        let mut reader = self.source.reader()?;
        let decoder = BlockDecoder::new(threads, block, archive, password, &mut reader);
        let mut index = first;
        let mut stopped: Option<(usize, Error)> = None;
        let mut each = |file: &ArchiveEntry, data: &mut dyn Read| {
            let current = index;
            index += 1;
            if !file.has_stream {
                return Ok(true);
            }
            let outcome = if staying.binary_search(&current).is_ok() {
                pipe(data, pieces, ctx)
            } else {
                drain(data, ctx).map_err(|stopped| stopped.error)
            };
            match outcome {
                Ok(()) => Ok(current < last),
                Err(error) => {
                    stopped = Some((current, error));
                    Err(SzError::Other(Cow::Borrowed("stopped")))
                }
            }
        };
        match (decoder.for_each_entries(&mut each), stopped) {
            (Ok(_), _) => Ok(()),
            (Err(_), Some((at, error))) => Err(failed_named(name(at), encrypted, error)),
            (Err(error), None) => Err(failed_named(
                name(index.saturating_sub(1).max(first)),
                encrypted,
                convert(error),
            )),
        }
    }

    /// Decodes a block and compresses the files of it that stay into a
    /// block of their own: what leaving a file out of a stream that many
    /// files share costs.
    fn again(
        &self,
        creator: &mut SevenZCreator<Stoppable<'_>>,
        block: usize,
        staying: &[usize],
        entries: Vec<ArchiveEntry>,
        lock: &mut Option<Lock>,
        ctx: &OpContext,
    ) -> Result<()> {
        let archive = &self.loaded.as_ref().expect("the archive is loaded").archive;
        let files = &archive.files;
        let password = if archive.blocks.get(block).is_some_and(is_encrypted) {
            let name = staying.first().map(|index| files[*index].name.as_str());
            Some(self.passwords.get(ctx, name, None)?.0)
        } else {
            None
        };
        // The block stays under the password it was under.
        if let (Some(password), None) = (&password, &lock) {
            *lock = Some(Lock::new(password));
        }
        let lock = lock.as_ref().filter(|_| password.is_some());
        let bytes = staying.iter().map(|index| files[*index].size).sum();
        creator
            .archive
            .set_content_methods(coders(&creator.recipe, lock, bytes, ctx.budget));
        let key = password
            .as_ref()
            .map_or_else(SzPassword::empty, sz_password);
        let (sender, receiver) = mpsc::sync_channel(PIPE_DEPTH);
        thread::scope(|scope| {
            let decoding = scope.spawn(move || {
                let outcome = self.feed(archive, block, staying, &key, &sender, ctx);
                if let Err(error) = &outcome {
                    // The encoder waits for the rest of a file.
                    let _ = sender.send(Piece::Failed(error.clone()));
                }
                outcome
            });
            let readers = entries
                .iter()
                .map(|_| Tallied::new(Piped::new(&receiver)))
                .collect();
            let pushed = creator
                .archive
                .push_archive_entries(entries, readers)
                .map(|_| ())
                .map_err(write_error);
            // The decoder may wait for room in the pipe: it is let go.
            drop(receiver);
            let decoded = decoding.join().unwrap_or_else(|_| {
                Err(Error::new("internal").detail("the decoder of a block panicked"))
            });
            // A failure of the decoder reaches the encoder as an error of
            // reading and comes back from it as it was.
            pushed.and(decoded)
        })
    }
}

/// What the threads that decode blocks share.
struct Work<'a> {
    source: &'a Source,
    passwords: &'a Passwords,
    archive: &'a Archive,
    /// The selected entries, by the index of their files.
    picked: &'a [Option<&'a Entry>],
    sink: &'a dyn EntrySink,
    ctx: &'a OpContext,
}

impl Work<'_> {
    fn deliver(&self, entry: &Entry, data: &mut dyn Read) -> std::result::Result<(), Stopped> {
        let mut data = Watched::new(data);
        let accepted = if entry.meta.kind == EntryKind::Symlink {
            read_link_target(&mut data)
                .map_err(Error::from)
                .and_then(|target| {
                    let mut link = entry.clone();
                    link.meta.link_target = Some(target);
                    link.meta.size = Some(0);
                    self.sink.accept(&link, &mut io::empty())
                })
        } else {
            self.sink.accept(entry, &mut data)
        };
        match (accepted, data.failure()) {
            // What the sink left unread is still checked.
            (Ok(()), _) => drain(&mut data, self.ctx),
            (Err(_), Some(error)) => Err(Stopped { error, data: true }),
            (Err(error), None) => Err(Stopped { error, data: false }),
        }
    }

    /// Decodes a block up to its file `last` and hands the selected files
    /// to the sink.
    fn block(&self, block: usize, last: usize, threads: u32) -> Result<()> {
        let archive = self.archive;
        let files = &archive.files;
        let encrypted = archive.blocks.get(block).is_some_and(is_encrypted);
        let first = *archive
            .stream_map
            .block_first_file_index
            .get(block)
            .ok_or_else(|| damaged("a block without files"))?;
        let name = |index: usize| files.get(index).map_or("", |file| file.name.as_str());
        let mut rejected = None;
        loop {
            self.ctx.checkpoint()?;
            let (password, generation) = if encrypted {
                let (password, generation) =
                    self.passwords.get(self.ctx, Some(name(first)), rejected)?;
                (sz_password(&password), Some(generation))
            } else {
                (SzPassword::empty(), None)
            };
            let mut reader = self.source.reader()?;
            let decoder = BlockDecoder::new(threads, block, archive, &password, &mut reader);
            let mut index = first;
            // An entry passed its check: the password is the right one.
            let mut verified = false;
            let mut stopped: Option<(usize, Stopped)> = None;
            let mut each = |file: &ArchiveEntry, data: &mut dyn Read| {
                let current = index;
                index += 1;
                if !file.has_stream {
                    return Ok(true);
                }
                let outcome = match self.picked.get(current).copied().flatten() {
                    Some(entry) => self.deliver(entry, data),
                    None => drain(data, self.ctx),
                };
                match outcome {
                    Ok(()) => {
                        if let (false, Some(generation)) = (verified, generation) {
                            self.passwords.prove(generation);
                        }
                        verified = true;
                        Ok(current < last)
                    }
                    Err(reason) => {
                        stopped = Some((current, reason));
                        Err(SzError::Other(Cow::Borrowed("stopped")))
                    }
                }
            };
            let outcome = decoder.for_each_entries(&mut each);
            // Under a password that has opened something else of the
            // archive, data that does not decode is damaged.
            let doubted = encrypted && !generation.is_some_and(|it| self.passwords.proven(it));
            let error = match (outcome, stopped) {
                (Ok(_), _) => return Ok(()),
                (Err(_), Some((_, Stopped { error, data: false }))) => return Err(error),
                (Err(_), Some((at, Stopped { error, data: true }))) => {
                    failed_named(name(at), doubted, error)
                }
                // The decoder failed between two entries.
                (Err(error), None) => failed_named(
                    name(index.saturating_sub(1).max(first)),
                    doubted,
                    convert(error),
                ),
            };
            // The format has no check of the password: a wrong one shows as
            // data that does not decode, from the first byte on.
            let wrong_password = doubted
                && !verified
                && matches!(
                    error.code.as_str(),
                    "damagedOrWrongPassword" | "passwordRequired"
                );
            if !wrong_password {
                return Err(error);
            }
            rejected = generation;
        }
    }
}

impl ArchiveReader for SevenZReader {
    fn capabilities(&self) -> Capabilities {
        Capabilities {
            random_access: false,
            solid: self
                .loaded
                .as_ref()
                .is_none_or(|loaded| loaded.archive.is_solid),
            parallel: true,
            has_index: true,
        }
    }

    fn entries(&mut self, ctx: &OpContext) -> Result<Vec<Entry>> {
        Ok(self.load(ctx)?.entries.clone())
    }

    fn extract(&mut self, pick: &Selection, sink: &dyn EntrySink, ctx: &OpContext) -> Result<()> {
        self.load(ctx)?;
        let loaded = self.loaded.as_ref().expect("the archive is loaded");
        let archive = &loaded.archive;
        let mut picked: Vec<Option<&Entry>> = vec![None; archive.files.len()];
        for entry in loaded.entries.iter().filter(|entry| pick.contains(entry)) {
            picked[entry.index as usize] = Some(entry);
        }
        // Folders and empty files have no data and wait for no block. The
        // rest tells which blocks are needed, and up to which file.
        let mut last: Vec<Option<usize>> = vec![None; archive.blocks.len()];
        for (index, file) in archive.files.iter().enumerate() {
            let Some(entry) = picked[index] else {
                continue;
            };
            if !file.has_stream {
                ctx.checkpoint()?;
                sink.accept(entry, &mut io::empty())?;
                continue;
            }
            let block = archive
                .stream_map
                .file_block_index
                .get(index)
                .copied()
                .flatten()
                .and_then(|block| last.get_mut(block))
                .ok_or_else(|| damaged("a file without a block"))?;
            *block = Some(index);
        }
        let jobs: Vec<(usize, usize)> = last
            .iter()
            .enumerate()
            .filter_map(|(block, last)| last.map(|last| (block, last)))
            .collect();
        if jobs.is_empty() {
            return Ok(());
        }
        // Blocks are decoded side by side as far as their dictionaries fit
        // into the memory; the threads left over go to the decoders.
        let threads = ctx.budget.threads.max(1);
        let memory = jobs
            .iter()
            .map(|(block, _)| block_memory(&archive.blocks[*block]))
            .max()
            .unwrap_or(MIB);
        let fit = usize::try_from(ctx.budget.memory / memory).unwrap_or(usize::MAX);
        let side_by_side = threads.min(jobs.len()).min(fit).max(1);
        // A decoder on several threads holds a chunk of input and one of
        // output for each of them.
        let fit = usize::try_from(ctx.budget.memory / memory.saturating_mul(8))
            .unwrap_or(usize::MAX)
            / side_by_side;
        let each = (threads / side_by_side).min(fit).clamp(1, 256) as u32;
        let work = Work {
            source: &self.source,
            passwords: &self.passwords,
            archive,
            picked: &picked,
            sink,
            ctx,
        };
        if side_by_side == 1 {
            return jobs
                .iter()
                .try_for_each(|(block, last)| work.block(*block, *last, each));
        }
        support::pool(side_by_side)?.install(|| {
            jobs.par_iter()
                .try_for_each(|(block, last)| work.block(*block, *last, each))
        })
    }

    /// The files that follow a file in its block: a stream that does not
    /// decode at one place does not decode after it.
    fn tied(&mut self, index: u64, ctx: &OpContext) -> Result<Vec<u64>> {
        let archive = &self.load(ctx)?.archive;
        let block_of = |index: usize| {
            archive
                .stream_map
                .file_block_index
                .get(index)
                .copied()
                .flatten()
        };
        let index = index as usize;
        let Some(block) = block_of(index) else {
            return Ok(Vec::new());
        };
        Ok(archive
            .files
            .iter()
            .enumerate()
            .skip(index + 1)
            .filter(|(other, file)| {
                file.has_stream && !file.is_anti_item && block_of(*other) == Some(block)
            })
            .map(|(other, _)| other as u64)
            .collect())
    }

    /// A block whose files all stay is copied as it is compressed, and
    /// encrypted. A block that loses a file is decoded, and what stays of
    /// it is compressed again.
    fn rewrite(
        &mut self,
        changes: Changes,
        output: &mut dyn Output,
        ctx: &OpContext,
    ) -> Result<()> {
        self.load(ctx)?;
        let loaded = self.loaded.as_ref().expect("the archive is loaded");
        let archive = &loaded.archive;
        let files = &archive.files;
        let mut options = CreateOptions {
            format: ArchiveFormat::SevenZ,
            ..changes.options
        };
        // What is added to an archive that has a password is locked with
        // it, and a list that was encrypted stays so.
        let locked = loaded.locked || archive.blocks.iter().any(is_encrypted);
        if let (None, true, Some((password, _))) =
            (&options.password, locked, self.passwords.known())
        {
            options.password = Some(password);
            options.encrypt_names = loaded.locked;
        }
        let mut creator = SevenZCreator::new(Stoppable::new(output, ctx), &options)?;
        let stays =
            |index: usize| !changes.removed.contains(&(index as u64)) && !files[index].is_anti_item;
        let renamed = |index: usize| {
            let mut entry = files[index].clone();
            if let Some(name) = changes.renamed.get(&(index as u64)) {
                name.trim_end_matches('/').clone_into(&mut entry.name);
            }
            entry
        };
        let mut members: Vec<Vec<usize>> = vec![Vec::new(); archive.blocks.len()];
        for (index, file) in files.iter().enumerate() {
            if !file.has_stream {
                if stays(index) {
                    creator.bare(renamed(index))?;
                }
                continue;
            }
            archive
                .stream_map
                .file_block_index
                .get(index)
                .copied()
                .flatten()
                .and_then(|block| members.get_mut(block))
                .ok_or_else(|| damaged("a file without a block"))?
                .push(index);
        }
        let packed = packed_sizes(archive);
        let mut reader = self.source.reader()?;
        let mut lock: Option<Lock> = None;
        for (block, members) in members.iter().enumerate() {
            ctx.checkpoint()?;
            let staying: Vec<usize> = members
                .iter()
                .copied()
                .filter(|index| stays(*index))
                .collect();
            let Some(first) = staying.first() else {
                continue;
            };
            ctx.progress.entry(&files[*first].name);
            let entries: Vec<ArchiveEntry> = staying.iter().map(|index| renamed(*index)).collect();
            // The writer copies the sums of the files, and makes up none.
            let whole =
                staying.len() == members.len() && staying.iter().all(|index| files[*index].has_crc);
            if whole {
                let mut names = entries.into_iter().map(|entry| entry.name);
                creator
                    .archive
                    .push_raw_block(archive, block, &mut reader, |file| {
                        names.next().unwrap_or_else(|| file.name.clone())
                    })
                    .map_err(copy_error)?;
            } else {
                self.again(&mut creator, block, &staying, entries, &mut lock, ctx)?;
            }
            ctx.progress.advance(packed[block]);
        }
        creator.add_all(changes.added, ctx)?;
        Box::new(creator).finish()
    }
}

/// The methods a 7z can be created with: the ones 7-Zip itself reads.
pub const METHODS: &[Method] = &[
    Method::Lzma2,
    Method::Lzma,
    Method::Ppmd,
    Method::Bzip2,
    Method::Deflate,
    Method::Store,
];

/// How the data of an archive is compressed.
struct Recipe {
    method: Method,
    level: Level,
    dictionary: Option<u32>,
}

impl Recipe {
    fn new(options: &CreateOptions) -> Result<Self> {
        let method = plan::method(options);
        if !METHODS.contains(&method) {
            return Err(unsupported_method(method.name()));
        }
        Ok(Self {
            method,
            level: options.level,
            dictionary: plan::dictionary(options),
        })
    }

    /// The dictionary for a block of `bytes`: one larger than the input
    /// costs memory and finds nothing more.
    fn dictionary(&self, bytes: u64) -> u32 {
        let chosen = plan::lzma_options(self.level, self.dictionary).dict_size;
        let fit = bytes
            .clamp(DICTIONARY_MIN, DICTIONARY_MAX)
            .next_power_of_two();
        chosen.min(fit as u32)
    }

    /// Memory an encoder of a block of `bytes` takes on one thread.
    fn memory(&self, bytes: u64) -> u64 {
        match self.method {
            Method::Lzma | Method::Lzma2 => {
                let options = plan::lzma_options(self.level, Some(self.dictionary(bytes)));
                u64::from(options.get_memory_usage()) * KIB
            }
            Method::Ppmd => plan::ppmd_memory(self.level),
            Method::Bzip2 => 8 * MIB,
            _ => MIB,
        }
    }

    /// The coder for a block of `bytes` that may take this budget.
    fn coder(&self, bytes: u64, budget: Budget) -> EncoderConfiguration {
        let preset = plan::lzma_preset(self.level);
        match self.method {
            Method::Lzma2 => {
                let dictionary = self.dictionary(bytes);
                let planned = plan::lzma(&plan::lzma_options(self.level, Some(dictionary)), budget);
                let mut options = if planned.workers > 1 && bytes > planned.chunk {
                    Lzma2Options::from_level_mt(preset, planned.workers, planned.chunk)
                } else {
                    Lzma2Options::from_level(preset)
                };
                options.set_dictionary_size(dictionary);
                options.into()
            }
            Method::Lzma => {
                let mut options = LzmaOptions::from_level(preset);
                options.set_dictionary_size(self.dictionary(bytes));
                options.into()
            }
            Method::Ppmd => PpmdOptions::from_order_memory_size(
                plan::ppmd_order(self.level),
                plan::ppmd_memory(self.level) as u32,
            )
            .into(),
            Method::Bzip2 => Bzip2Options::from_level(plan::bzip2_level(self.level)).into(),
            Method::Deflate => DeflateOptions::from_level(plan::deflate_level(self.level)).into(),
            _ => EncoderConfiguration::new(EncoderMethod::COPY),
        }
    }

    /// The most a solid block holds.
    fn solid_block(&self) -> u64 {
        let dictionary = u64::from(plan::lzma_options(self.level, self.dictionary).dict_size);
        (dictionary * SOLID_DICTIONARIES).clamp(SOLID_BLOCK_MIN, SOLID_BLOCK_MAX)
    }
}

/// The encryption of an archive. The key is derived from the password and
/// the salt, at a cost meant to be felt: one salt for the archive derives it
/// once, and every block still gets a start of its own.
struct Lock {
    password: SzPassword,
    salt: [u8; 16],
}

impl Lock {
    fn new(password: &Password) -> Self {
        let fresh = AesEncoderOptions::new(sz_password(password));
        Self {
            salt: fresh.salt,
            password: fresh.password,
        }
    }

    fn coder(&self) -> EncoderConfiguration {
        let mut options = AesEncoderOptions::new(self.password.clone());
        options.salt = self.salt;
        options.num_cycles_power = KEY_CYCLES_POWER;
        options.into()
    }
}

/// The coders of a block, the one next to the output first: the data is
/// compressed, then encrypted.
fn coders(
    recipe: &Recipe,
    lock: Option<&Lock>,
    bytes: u64,
    budget: Budget,
) -> Vec<EncoderConfiguration> {
    lock.map(Lock::coder)
        .into_iter()
        .chain([recipe.coder(bytes, budget)])
        .collect()
}

fn nt_time(time: Option<Timestamp>) -> Option<NtTime> {
    time.and_then(to_windows_time).map(NtTime::new)
}

/// The record of an entry in the list of an archive.
fn archive_entry(meta: &EntryMeta) -> ArchiveEntry {
    let name = meta.path.trim_end_matches('/');
    let folder = meta.is_dir();
    let mut entry = if folder {
        ArchiveEntry::new_directory(name)
    } else {
        ArchiveEntry::new_file(name)
    };
    if let Some(time) = nt_time(meta.modified) {
        entry.has_last_modified_date = true;
        entry.last_modified_date = time;
    }
    if let Some(time) = nt_time(meta.created) {
        entry.has_creation_date = true;
        entry.creation_date = time;
    }
    if let Some(time) = nt_time(meta.accessed) {
        entry.has_access_date = true;
        entry.access_date = time;
    }
    let kind = if folder {
        ATTRIBUTE_DIRECTORY
    } else {
        ATTRIBUTE_ARCHIVE
    };
    let attributes = meta
        .windows_attributes
        .map_or(kind, |attributes| attributes & ATTRIBUTES)
        | if folder { ATTRIBUTE_DIRECTORY } else { 0 };
    let mode = match (meta.unix_mode, meta.kind) {
        (Some(mode), _) => Some(mode & MODE_PERMISSIONS),
        // The mode is what marks a link as one.
        (None, EntryKind::Symlink) => Some(0o777),
        (None, _) => None,
    };
    let file_type = match meta.kind {
        EntryKind::Directory => MODE_FOLDER,
        EntryKind::Symlink => MODE_LINK,
        EntryKind::File | EntryKind::Hardlink => MODE_FILE,
    };
    entry.has_windows_attributes = true;
    entry.windows_attributes = match mode {
        Some(mode) => attributes | UNIX_EXTENSION | ((file_type | mode) << 16),
        None => attributes,
    };
    entry
}

/// A file opened when the encoder first asks for its data and closed at its
/// end: a solid block of ten thousand files holds one of them open.
struct Lazy<'a> {
    source: &'a dyn DataSource,
    ctx: &'a OpContext,
    reader: Option<BufReader<Box<dyn Read + Send + 'a>>>,
    done: bool,
}

impl Read for Lazy<'_> {
    fn read(&mut self, buffer: &mut [u8]) -> io::Result<usize> {
        if self.done {
            return Ok(0);
        }
        self.ctx.checkpoint().map_err(Error::into_io)?;
        let reader = if let Some(reader) = &mut self.reader {
            reader
        } else {
            let opened = self.source.open().map_err(Error::into_io)?;
            self.reader
                .insert(BufReader::with_capacity(READ_BUFFER, opened))
        };
        let read = reader.read(buffer)?;
        if read == 0 {
            self.done = true;
            self.reader = None;
        }
        Ok(read)
    }
}

/// The data of one entry given to [`ArchiveWriter::add`], with a look at
/// the cancellation on every read.
struct Checked<'a> {
    data: &'a mut dyn Read,
    ctx: &'a OpContext,
}

impl Read for Checked<'_> {
    fn read(&mut self, buffer: &mut [u8]) -> io::Result<usize> {
        self.ctx.checkpoint().map_err(Error::into_io)?;
        self.data.read(buffer)
    }
}

/// An entry with content, on its way into a block.
struct Member {
    entry: ArchiveEntry,
    data: Box<dyn DataSource>,
    size: u64,
}

/// The files of one block.
struct Job {
    members: Vec<Member>,
    bytes: u64,
}

impl Job {
    fn open<'a>(&'a self, ctx: &'a OpContext) -> (Vec<ArchiveEntry>, Vec<Tallied<Lazy<'a>>>) {
        self.members
            .iter()
            .map(|member| {
                let reader = Lazy {
                    source: member.data.as_ref(),
                    ctx,
                    reader: None,
                    done: false,
                };
                (member.entry.clone(), Tallied::new(reader))
            })
            .unzip()
    }
}

/// The key that brings alike files together: the extension, then the name.
fn sort_key(name: &str) -> (String, String) {
    let lower = name.to_lowercase();
    let file = lower.rsplit('/').next().unwrap_or(&lower);
    let extension = match file.rfind('.') {
        Some(dot) if dot > 0 => file[dot + 1..].to_owned(),
        _ => String::new(),
    };
    (extension, lower)
}

/// Splits the files into blocks of at most `limit` bytes; zero gives every
/// file a block of its own.
fn group(members: Vec<Member>, limit: u64) -> Vec<Job> {
    let mut jobs: Vec<Job> = Vec::new();
    for member in members {
        match jobs.last_mut() {
            Some(job) if limit > 0 && job.bytes.saturating_add(member.size) <= limit => {
                job.bytes += member.size;
                job.members.push(member);
            }
            _ => jobs.push(Job {
                bytes: member.size,
                members: vec![member],
            }),
        }
    }
    jobs
}

/// A 7z archive being written.
pub struct SevenZCreator<W: Write + Seek + Send> {
    archive: SzWriter<W>,
    recipe: Recipe,
    solid: bool,
    lock: Option<Lock>,
    encrypt_names: bool,
}

impl<W: Write + Seek + Send> SevenZCreator<W> {
    pub fn new(output: W, options: &CreateOptions) -> Result<Self> {
        let lock = options
            .password
            .as_ref()
            .filter(|password| !password.is_empty())
            .map(Lock::new);
        Ok(Self {
            archive: SzWriter::new(output).map_err(write_error)?,
            recipe: Recipe::new(options)?,
            solid: options.solid,
            encrypt_names: options.encrypt_names && lock.is_some(),
            lock,
        })
    }

    /// Stores an entry that has no content.
    fn bare(&mut self, entry: ArchiveEntry) -> Result<()> {
        self.archive
            .push_archive_entry::<io::Empty>(entry, None)
            .map_err(write_error)?;
        Ok(())
    }

    /// Writes a block into the archive as it is compressed; the encoder
    /// takes the threads.
    fn stream(&mut self, job: &Job, ctx: &OpContext) -> Result<()> {
        self.archive.set_content_methods(coders(
            &self.recipe,
            self.lock.as_ref(),
            job.bytes,
            ctx.budget,
        ));
        let (entries, readers) = job.open(ctx);
        self.archive
            .push_archive_entries(entries, readers)
            .map_err(write_error)?;
        Ok(())
    }

    /// Compresses the blocks of a wave in memory, one on each thread, and
    /// appends them in their order.
    fn flush(
        &mut self,
        wave: &mut Vec<Job>,
        pool: &rayon::ThreadPool,
        ctx: &OpContext,
    ) -> Result<()> {
        if wave.is_empty() {
            return Ok(());
        }
        let recipe = &self.recipe;
        let lock = self.lock.as_ref();
        let alone = Budget {
            threads: 1,
            memory: ctx.budget.memory,
        };
        let prepared: Vec<Result<PreparedBlock>> = pool.install(|| {
            wave.par_iter()
                .map(|job| {
                    let methods = Arc::new(coders(recipe, lock, job.bytes, alone));
                    let (entries, readers) = job.open(ctx);
                    prepare_block(methods, entries, readers).map_err(write_error)
                })
                .collect()
        });
        wave.clear();
        for block in prepared {
            self.archive
                .push_prepared_block(block?)
                .map_err(write_error)?;
        }
        Ok(())
    }

    /// Turns an item into a member of a block; an item without content is
    /// stored at once.
    fn member(&mut self, item: Item) -> Result<Option<Member>> {
        let entry = archive_entry(&item.meta);
        match (item.meta.kind, item.data) {
            (EntryKind::Hardlink, _) => {
                Err(Error::new("hardlinkUnsupported").with("name", &item.meta.path))
            }
            // The target of a link is its content, as 7-Zip stores it.
            (EntryKind::Symlink, _) => {
                let target = item.meta.link_target.unwrap_or_default().into_bytes();
                Ok(Some(Member {
                    entry,
                    size: target.len() as u64,
                    data: Box::new(target),
                }))
            }
            (EntryKind::File, Some(data)) if item.meta.size != Some(0) => Ok(Some(Member {
                entry,
                data,
                // A file of unknown size is taken for a large one.
                size: item.meta.size.unwrap_or(u64::MAX),
            })),
            _ => self.bare(entry).map(|()| None),
        }
    }
}

impl<W: Write + Seek + Send> ArchiveWriter for SevenZCreator<W> {
    fn add(&mut self, meta: &EntryMeta, data: &mut dyn Read, ctx: &OpContext) -> Result<()> {
        ctx.checkpoint()?;
        let entry = archive_entry(meta);
        let size = match meta.kind {
            EntryKind::Hardlink => {
                return Err(Error::new("hardlinkUnsupported").with("name", &meta.path));
            }
            EntryKind::Directory => return self.bare(entry),
            EntryKind::File if meta.size == Some(0) => return self.bare(entry),
            EntryKind::File => meta.size.unwrap_or(u64::MAX),
            EntryKind::Symlink => meta.link_target.as_ref().map_or(0, String::len) as u64,
        };
        self.archive.set_content_methods(coders(
            &self.recipe,
            self.lock.as_ref(),
            size,
            ctx.budget,
        ));
        let pushed = if meta.kind == EntryKind::Symlink {
            let target = meta.link_target.as_deref().unwrap_or_default();
            self.archive
                .push_archive_entry(entry, Some(target.as_bytes()))
        } else {
            self.archive
                .push_archive_entry(entry, Some(Checked { data, ctx }))
        };
        pushed.map_err(write_error)?;
        Ok(())
    }

    fn add_all(&mut self, items: Vec<Item>, ctx: &OpContext) -> Result<()> {
        let mut members = Vec::with_capacity(items.len());
        for item in items {
            ctx.checkpoint()?;
            members.extend(self.member(item)?);
        }
        let threads = ctx.budget.threads.max(1);
        // A wave is held in memory compressed, and while it is compressed
        // every thread holds a block of it: a block is small when a thread
        // may hold it.
        let wave_bytes = (ctx.budget.memory / 4).max(WAVE_BYTES_MIN);
        let small = if threads > 1 {
            wave_bytes / threads as u64
        } else {
            0
        };
        let limit = if self.solid {
            members.sort_by_cached_key(|member| sort_key(&member.entry.name));
            let limit = self.recipe.solid_block();
            // Only the encoder of LZMA2 takes threads itself: the blocks of
            // the other methods are kept small enough to go side by side.
            if self.recipe.method == Method::Lzma2 || threads == 1 {
                limit
            } else {
                limit.min(small.max(MIB))
            }
        } else {
            0
        };
        let jobs = group(members, limit);
        let pool = if threads > 1 {
            let each = self.recipe.memory(small).saturating_add(small).max(1);
            let fit = usize::try_from(ctx.budget.memory / each).unwrap_or(usize::MAX);
            Some(support::pool(threads.min(fit))?)
        } else {
            None
        };
        let mut wave: Vec<Job> = Vec::new();
        let mut held = 0u64;
        for job in jobs {
            ctx.checkpoint()?;
            let Some(pool) = pool.as_ref().filter(|_| job.bytes <= small) else {
                if let Some(pool) = &pool {
                    self.flush(&mut wave, pool, ctx)?;
                    held = 0;
                }
                self.stream(&job, ctx)?;
                continue;
            };
            if held + job.bytes > wave_bytes || wave.len() >= WAVE_BLOCKS {
                self.flush(&mut wave, pool, ctx)?;
                held = 0;
            }
            held += job.bytes;
            wave.push(job);
        }
        match &pool {
            Some(pool) => self.flush(&mut wave, pool, ctx),
            None => Ok(()),
        }
    }

    fn finish(mut self: Box<Self>) -> Result<()> {
        // The list is encrypted with the coder the writer finds among its
        // methods: a fresh one, with a start of its own.
        if let Some(lock) = self.lock.as_ref().filter(|_| self.encrypt_names) {
            self.archive.set_content_methods(vec![lock.coder()]);
        }
        self.archive.set_encrypt_header(self.encrypt_names);
        let mut output = self.archive.finish()?;
        output.flush()?;
        Ok(())
    }
}

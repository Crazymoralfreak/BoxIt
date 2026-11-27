//! ZIP and ZIP64.
//!
//! Entries of a ZIP are compressed apart from each other, and the format
//! keeps its list at the end of the file. Both make it the format that gains
//! the most from threads: the list is read without touching the data, and
//! every thread decodes entries of its own through its own view of the file.
//! Writing compresses small files on all threads and appends the results in
//! order.

use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet};
use std::io::{self, Read, Seek, SeekFrom, Write};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

use boxit_core::{
    ArchiveFormat, ArchiveReader, ArchiveWriter, Capabilities, Changes, CreateOptions, DataSource,
    Entry, EntryKind, EntryMeta, EntrySink, Error, Item, Method, OpContext, Output, Password,
    Result, Selection, Timestamp,
};
use rayon::prelude::*;
use zip::read::{ArchiveOffset, Config, ZipArchiveMetadata};
use zip::result::ZipError;
use zip::write::{FullFileOptions, SimpleFileOptions, ZipFileBuilder};
use zip::{
    AesMode, CompressionMethod, DateTime, PreparedZipFile, System, ZipArchive, ZipReadOptions,
    ZipWriter,
};

use crate::bytes::{le16, le32, le64};
use crate::names::NameDecoder;
use crate::source::{Source, SourceReader};
use crate::support::{
    self, copy, damaged, failed, from_local, from_windows_time, read_link_target, to_local,
    to_windows_time, unsupported_method, Civil, Passwords, Stoppable, Watched,
};
use crate::{plan, zip_scan, OpenOptions};

/// The flag of an entry whose name is UTF-8.
const FLAG_UTF8: u16 = 0x0800;
/// Signature and size of the fixed part of a record of the central directory.
const CENTRAL_SIGNATURE: [u8; 4] = [b'P', b'K', 1, 2];
const CENTRAL_FIXED: usize = 46;
/// Extra fields that hold times: of Info-ZIP, in seconds, and of NTFS.
const EXTRA_UNIX_TIME: u16 = 0x5455;
const EXTRA_NTFS: u16 = 0x000A;

/// `FILE_ATTRIBUTE_DIRECTORY` of Windows and DOS.
const ATTRIBUTE_DIRECTORY: u32 = 0x10;

/// The most input one batch of files compressed in memory may hold, and the
/// most files: a batch is written only when all of it is compressed.
const BATCH_FILES: usize = 4096;
const BATCH_BYTES_MIN: u64 = 32 * 1024 * 1024;

fn convert(error: ZipError) -> Error {
    match error {
        ZipError::Io(error) => error.into(),
        ZipError::UnsupportedArchive(ZipError::PASSWORD_REQUIRED) => Error::new("passwordRequired"),
        ZipError::UnsupportedArchive(what) => Error::new("unsupportedArchive").detail(what),
        ZipError::InvalidPassword => Error::new("wrongPassword"),
        ZipError::CompressionMethodNotSupported(id) => {
            unsupported_method(CompressionMethod::name_from_u16(id))
        }
        other => damaged(other),
    }
}

fn method_name(method: CompressionMethod) -> String {
    match method {
        CompressionMethod::Stored => String::from(Method::Store.name()),
        CompressionMethod::Deflated => String::from(Method::Deflate.name()),
        CompressionMethod::Deflate64 => String::from(Method::Deflate64.name()),
        CompressionMethod::Bzip2 => String::from(Method::Bzip2.name()),
        CompressionMethod::Zstd => String::from(Method::Zstd.name()),
        CompressionMethod::Lzma => String::from(Method::Lzma.name()),
        CompressionMethod::Xz => String::from(Method::Xz.name()),
        CompressionMethod::Ppmd => String::from(Method::Ppmd.name()),
        other => other.to_string(),
    }
}

/// Times an entry keeps in its extra fields, more exact than the DOS time
/// of the header and free of its time zone.
#[derive(Default)]
struct ExtraTimes {
    modified: Option<Timestamp>,
    accessed: Option<Timestamp>,
    created: Option<Timestamp>,
}

/// Finds the times in the extra fields of an entry.
fn parse_extra_times(mut extra: &[u8]) -> ExtraTimes {
    let mut times = ExtraTimes::default();
    let mut unix: Option<Timestamp> = None;
    while let (Some(id), Some(size)) = (le16(extra, 0), le16(extra, 2)) {
        let Some(body) = extra.get(4..4 + usize::from(size)) else {
            break;
        };
        match id {
            EXTRA_UNIX_TIME => {
                if body.first().is_some_and(|flags| flags & 1 != 0) {
                    unix = le32(body, 1).map(|seconds| Timestamp::from_seconds(i64::from(seconds)));
                }
            }
            EXTRA_NTFS => {
                // Four reserved bytes, then attributes: a tag, a size, data.
                let mut attributes = body.get(4..).unwrap_or_default();
                while let (Some(tag), Some(size)) = (le16(attributes, 0), le16(attributes, 2)) {
                    let Some(data) = attributes.get(4..4 + usize::from(size)) else {
                        break;
                    };
                    if tag == 1 && data.len() >= 24 {
                        times.modified = le64(data, 0).and_then(from_windows_time);
                        times.accessed = le64(data, 8).and_then(from_windows_time);
                        times.created = le64(data, 16).and_then(from_windows_time);
                    }
                    attributes = &attributes[4 + usize::from(size)..];
                }
            }
            _ => {}
        }
        extra = &extra[4 + usize::from(size)..];
    }
    if times.modified.is_none() {
        times.modified = unix;
    }
    times
}

/// Reads the extra fields of the record of the central directory at `offset`.
fn read_central_extra(reader: &mut SourceReader, offset: u64) -> io::Result<Vec<u8>> {
    reader.seek(SeekFrom::Start(offset))?;
    let mut fixed = [0u8; CENTRAL_FIXED];
    reader.read_exact(&mut fixed)?;
    if fixed[..4] != CENTRAL_SIGNATURE {
        return Ok(Vec::new());
    }
    let name = u64::from(u16::from_le_bytes([fixed[28], fixed[29]]));
    let extra = usize::from(u16::from_le_bytes([fixed[30], fixed[31]]));
    reader.seek(SeekFrom::Current(name as i64))?;
    let mut bytes = vec![0u8; extra];
    reader.read_exact(&mut bytes)?;
    Ok(bytes)
}

fn dos_time(time: DateTime) -> Option<Timestamp> {
    from_local(Civil {
        year: i32::from(time.year()),
        month: u32::from(time.month()),
        day: u32::from(time.day()),
        hour: u32::from(time.hour()),
        minute: u32::from(time.minute()),
        second: u32::from(time.second()),
    })
}

struct Index {
    metadata: Arc<ZipArchiveMetadata>,
    entries: Vec<Entry>,
    comment: Option<String>,
}

/// A view of an archive of its own, over the list that was read once:
/// every thread that reads the archive takes one.
fn view(source: &Source, metadata: &Arc<ZipArchiveMetadata>) -> Result<ZipArchive<SourceReader>> {
    let reader = source.reader()?;
    // SAFETY: the function is unsafe only as a warning that the reader and
    // the list must belong together, and they do: the list was read from
    // this very source.
    Ok(unsafe { ZipArchive::unsafe_new_with_metadata(reader, Arc::clone(metadata)) })
}

/// A ZIP archive opened for reading.
pub struct ZipReader {
    source: Source,
    options: OpenOptions,
    passwords: Passwords,
    index: Option<Index>,
    /// The list was written anew from the headers of the entries and
    /// follows the file in memory: see [`ZipReader::salvage`].
    rebuilt: bool,
}

impl ZipReader {
    pub fn new(source: Source, options: OpenOptions) -> Self {
        let passwords = Passwords::new(
            source.path().to_string_lossy().into_owned(),
            options.password.clone(),
        );
        Self {
            source,
            options,
            passwords,
            index: None,
            rebuilt: false,
        }
    }

    /// Opens an archive that may have lost its list. One whose list reads
    /// is opened as it is. For any other the file is searched for the
    /// headers of its entries, and a list is made of them; the names of
    /// the entries that are cut short come with it. `true` tells that the
    /// list was made anew.
    pub fn salvage(
        source: Source,
        options: OpenOptions,
        ctx: &OpContext,
    ) -> Result<(Self, bool, Vec<String>)> {
        let mut reader = Self::new(source.clone(), options.clone());
        let error = match reader.index() {
            Ok(_) => return Ok((reader, false, Vec::new())),
            Err(error) if matches!(error.code.as_str(), "damaged" | "truncated") => error,
            Err(error) => return Err(error),
        };
        let scanned = zip_scan::scan(&source, ctx)?;
        if scanned.found.is_empty() {
            return Err(error);
        }
        let decoder = NameDecoder::new(
            options.encoding,
            scanned
                .found
                .iter()
                .map(|local| local.name.as_slice())
                .chain(scanned.lost.iter().map(Vec::as_slice)),
            options.hint.as_deref(),
        );
        let lost = scanned
            .lost
            .iter()
            .map(|name| decoder.decode(name, false).into_owned())
            .collect();
        let list = zip_scan::directory(&scanned.found, source.size());
        let mut reader = Self::new(source.with_tail(list), options);
        reader.rebuilt = true;
        reader.index()?;
        Ok((reader, true, lost))
    }

    /// Reads the list of the archive on first use.
    fn index(&mut self) -> Result<&Index> {
        if self.index.is_none() {
            self.index = Some(self.load()?);
        }
        Ok(self.index.as_ref().expect("the index was just loaded"))
    }

    fn load(&self) -> Result<Index> {
        let config = Config {
            // A list made anew counts from the start of the file.
            archive_offset: if self.rebuilt {
                ArchiveOffset::Known(0)
            } else {
                ArchiveOffset::Detect
            },
        };
        let archive = ZipArchive::with_config(config, self.source.reader()?).map_err(convert)?;
        let decoder = NameDecoder::new(
            self.options.encoding,
            archive.file_names_raw(),
            self.options.hint.as_deref(),
        );
        let mut central = self.source.reader()?;
        let mut entries = Vec::with_capacity(archive.len());
        for index in 0..archive.len() {
            let file = archive.by_index_data(index).map_err(convert)?;
            let utf8 = file.flags().as_u16() & FLAG_UTF8 != 0;
            let mut path = decoder.decode(file.name_raw(), utf8).into_owned();
            // Programs of DOS and Windows may leave their own separator in.
            if matches!(
                file.system(),
                System::Dos | System::WindowsNTFS | System::VFAT
            ) && path.contains('\\')
            {
                path = path.replace('\\', "/");
            }
            let kind = if file.is_dir() {
                EntryKind::Directory
            } else if file.is_symlink() {
                EntryKind::Symlink
            } else {
                EntryKind::File
            };
            // The times of the extra fields are a refinement: an entry whose
            // record cannot be read again still has the time of its header.
            let extra = read_central_extra(&mut central, file.central_header_start())
                .map_or_else(|_| ExtraTimes::default(), |extra| parse_extra_times(&extra));

            let attributes = file.external_attributes() & 0xFF;
            let comment = file.comment();
            entries.push(Entry {
                index: index as u64,
                meta: EntryMeta {
                    path,
                    kind,
                    link_target: None,
                    size: Some(file.size()),
                    modified: extra
                        .modified
                        .or_else(|| file.last_modified().and_then(dos_time)),
                    created: extra.created,
                    accessed: extra.accessed,
                    unix_mode: file.unix_mode().map(|mode| mode & 0o7777),
                    windows_attributes: (attributes != 0).then_some(attributes),
                },
                packed_size: Some(file.compressed_size()),
                crc32: Some(file.crc32()),
                encrypted: file.encrypted(),
                method: Some(method_name(file.compression()).into()),
                comment: (!comment.is_empty()).then(|| comment.to_owned()),
            });
        }
        let comment = archive.comment();
        Ok(Index {
            comment: (!comment.is_empty()).then(|| decoder.decode(comment, false).into_owned()),
            metadata: archive.metadata(),
            entries,
        })
    }
}

/// Hands one entry to the sink, asking for a password as long as the one
/// at hand is wrong.
fn deliver(
    archive: &mut ZipArchive<SourceReader>,
    entry: &Entry,
    passwords: &Passwords,
    sink: &dyn EntrySink,
    ctx: &OpContext,
) -> Result<()> {
    ctx.checkpoint()?;
    if entry.meta.kind == EntryKind::Directory {
        return sink.accept(entry, &mut io::empty());
    }
    let mut rejected = None;
    loop {
        let password = if entry.encrypted {
            Some(passwords.get(ctx, Some(&entry.meta.path), rejected)?)
        } else {
            None
        };
        let options = ZipReadOptions::new().password(
            password
                .as_ref()
                .map(|(password, _)| password.expose().as_bytes()),
        );
        let mut file = match archive.by_index_with_options(entry.index as usize, options) {
            Ok(file) => file,
            Err(ZipError::InvalidPassword) => {
                rejected = password.map(|(_, generation)| generation);
                continue;
            }
            // A header that cannot be read is the damage of its entry.
            Err(error) => return Err(failed(entry, convert(error))),
        };
        let mut data = Watched::new(&mut file);
        let result = if entry.meta.kind == EntryKind::Symlink {
            read_link_target(&mut data)
                .map_err(Error::from)
                .and_then(|target| {
                    let mut link = entry.clone();
                    link.meta.link_target = Some(target);
                    link.meta.size = Some(0);
                    sink.accept(&link, &mut io::empty())
                })
        } else {
            sink.accept(entry, &mut data)
        };
        return result.map_err(|error| failed(entry, data.settle(entry, error)));
    }
}

impl ArchiveReader for ZipReader {
    fn capabilities(&self) -> Capabilities {
        Capabilities {
            random_access: true,
            solid: false,
            parallel: true,
            has_index: true,
        }
    }

    fn entries(&mut self, _ctx: &OpContext) -> Result<Vec<Entry>> {
        Ok(self.index()?.entries.clone())
    }

    fn extract(&mut self, pick: &Selection, sink: &dyn EntrySink, ctx: &OpContext) -> Result<()> {
        self.index()?;
        let index = self.index.as_ref().expect("the index is loaded");
        let picked: Vec<&Entry> = index
            .entries
            .iter()
            .filter(|entry| pick.contains(entry))
            .collect();
        let source = &self.source;
        let passwords = &self.passwords;
        let open = || view(source, &index.metadata);
        let threads = ctx.budget.threads.min(picked.len());
        if threads <= 1 {
            let mut archive = open()?;
            return picked
                .into_iter()
                .try_for_each(|entry| deliver(&mut archive, entry, passwords, sink, ctx));
        }
        support::pool(threads)?.install(|| {
            picked
                .par_iter()
                .try_for_each_init(open, |archive, entry| match archive {
                    Ok(archive) => deliver(archive, entry, passwords, sink, ctx),
                    Err(error) => Err(error.clone()),
                })
        })
    }

    fn comment(&mut self, _ctx: &OpContext) -> Result<Option<String>> {
        Ok(self.index()?.comment.clone())
    }

    /// The entries that stay are copied as they are compressed, and
    /// encrypted: nothing of them is decoded.
    fn rewrite(
        &mut self,
        changes: Changes,
        output: &mut dyn Output,
        ctx: &OpContext,
    ) -> Result<()> {
        self.index()?;
        let index = self.index.as_ref().expect("the index is loaded");
        let Changes {
            removed,
            renamed,
            added,
            options,
        } = changes;
        let mut options = CreateOptions {
            format: ArchiveFormat::Zip,
            ..options
        };
        // What is added to an archive that has a password is locked with it.
        let locked = index.entries.iter().any(|entry| entry.encrypted);
        if let (None, true, Some((password, _))) =
            (&options.password, locked, self.passwords.known())
        {
            options.password = Some(password);
        }
        let failed = AtomicBool::new(false);
        let output = Stoppable::new(output, ctx).until(&failed);
        let mut creator = ZipCreator::new(output, &options)?;
        let copied = Copy {
            source: &self.source,
            index,
            removed: &removed,
            renamed: &renamed,
            // The comment of the archive stays unless another is given.
            comment: options.comment.is_none(),
        }
        .run(&mut creator.zip, ctx)
        .and_then(|()| creator.add_all(added, ctx));
        if copied.is_err() {
            // The writer closes its archive as it is dropped, and says so
            // aloud when it cannot: it is not stopped twice.
            failed.store(true, Ordering::Relaxed);
        }
        copied?;
        Box::new(creator).finish()
    }
}

/// The entries of an archive on their way into the one written from it.
struct Copy<'a> {
    source: &'a Source,
    index: &'a Index,
    removed: &'a BTreeSet<u64>,
    renamed: &'a BTreeMap<u64, String>,
    comment: bool,
}

impl Copy<'_> {
    /// The new name of an entry, as it is written.
    fn renamed(&self, entry: &Entry) -> Option<String> {
        self.renamed.get(&entry.index).map(|name| {
            let name = name.trim_end_matches('/');
            if entry.meta.is_dir() {
                format!("{name}/")
            } else {
                name.to_owned()
            }
        })
    }

    fn staying(&self) -> impl Iterator<Item = &Entry> {
        self.index
            .entries
            .iter()
            .filter(|entry| !self.removed.contains(&entry.index))
    }

    fn run<W: Write + Seek>(&self, zip: &mut ZipWriter<W>, ctx: &OpContext) -> Result<()> {
        let mut archive = view(self.source, &self.index.metadata)?;
        if self.comment {
            zip.set_raw_comment(archive.comment().into())
                .map_err(convert)?;
        }
        // An archive may hold several entries of one name, and one that
        // is written cannot: the last of them stays, the one every program
        // shows.
        let mut last: HashMap<Vec<u8>, u64> = HashMap::new();
        for entry in self.staying() {
            let name = match self.renamed(entry) {
                Some(name) => name.into_bytes(),
                None => archive
                    .by_index_data(entry.index as usize)
                    .map_err(convert)?
                    .name_raw()
                    .to_vec(),
            };
            last.insert(name, entry.index);
        }
        let last: HashSet<u64> = last.into_values().collect();
        for entry in self.staying().filter(|entry| last.contains(&entry.index)) {
            ctx.checkpoint()?;
            ctx.progress.entry(&entry.meta.path);
            let file = archive
                .by_index_raw(entry.index as usize)
                .map_err(|error| failed(entry, convert(error)))?;
            match self.renamed(entry) {
                Some(name) => zip.raw_copy_file_rename(file, name),
                None => zip.raw_copy_file(file),
            }
            .map_err(convert)?;
            ctx.progress.advance(entry.packed_size.unwrap_or(0));
        }
        Ok(())
    }
}

/// The methods a ZIP can be created with.
pub const METHODS: &[Method] = &[
    Method::Deflate,
    Method::Store,
    Method::Bzip2,
    Method::Zstd,
    Method::Xz,
    Method::Ppmd,
];

/// The method and the level of the `zip` crate for the options of a user.
fn compression(options: &CreateOptions) -> Result<(CompressionMethod, Option<i64>)> {
    let level = options.level;
    Ok(match plan::method(options) {
        Method::Store => (CompressionMethod::Stored, None),
        Method::Deflate => (
            CompressionMethod::Deflated,
            Some(i64::from(plan::deflate_level(level))),
        ),
        Method::Bzip2 => (
            CompressionMethod::Bzip2,
            Some(i64::from(plan::bzip2_level(level))),
        ),
        Method::Zstd => (
            CompressionMethod::Zstd,
            Some(i64::from(plan::zstd_level(level))),
        ),
        // These take the settings their encoders start with.
        Method::Xz => (CompressionMethod::Xz, None),
        Method::Ppmd => (CompressionMethod::Ppmd, None),
        // Read, and not written.
        method @ (Method::Deflate64 | Method::Lzma | Method::Lzma2) => {
            return Err(unsupported_method(method.name()));
        }
    })
}

/// The options of an entry: its method, times and attributes.
pub(crate) fn entry_options<'k>(
    meta: &EntryMeta,
    compression: (CompressionMethod, Option<i64>),
    password: Option<&'k Password>,
) -> Result<FullFileOptions<'k, 'static>> {
    let (method, level) = compression;
    let mut options = SimpleFileOptions::default()
        .compression_method(method)
        .compression_level(level);
    if let Some(civil) = meta.modified.and_then(to_local) {
        // The header keeps local time in steps of two seconds, from 1980.
        if let Ok(time) = DateTime::from_date_and_time(
            civil.year.clamp(0, i32::from(u16::MAX)) as u16,
            civil.month as u8,
            civil.day as u8,
            civil.hour as u8,
            civil.minute as u8,
            civil.second as u8,
        ) {
            options = options.last_modified_time(time);
        }
    }
    if let Some(mode) = meta.unix_mode {
        options = options.unix_permissions(mode).system(System::Unix);
    } else if let Some(attributes) = meta.windows_attributes {
        let directory = if meta.is_dir() {
            ATTRIBUTE_DIRECTORY
        } else {
            0
        };
        options = options
            .external_attributes((attributes & 0xFF) | directory)
            .system(System::Dos);
    }
    if let Some(password) = password {
        if !meta.is_dir() {
            options = options.with_aes_encryption(AesMode::Aes256, password.expose());
        }
    }
    let mut options = options.into_full_options();
    if let Some(modified) = meta.modified {
        // The exact time, free of the time zone: in seconds for the
        // programs of Unix and in the ticks of Windows for the rest.
        if let Ok(seconds) = u32::try_from(modified.seconds) {
            options = options
                .with_extended_timestamp(Some(seconds), None, None)
                .map_err(convert)?;
        }
        if let Some(ticks) = to_windows_time(modified) {
            let other = |time: Option<Timestamp>| time.and_then(to_windows_time).unwrap_or(ticks);
            options = options
                .with_ntfs_times(ticks, other(meta.accessed), other(meta.created))
                .map_err(convert)?;
        }
    }
    Ok(options)
}

fn link_unsupported(meta: &EntryMeta) -> Error {
    Error::new("hardlinkUnsupported").with("name", &meta.path)
}

/// Compresses a file in memory, on the thread that calls.
fn prepare(
    meta: &EntryMeta,
    data: &dyn DataSource,
    compression: (CompressionMethod, Option<i64>),
    ctx: &OpContext,
) -> Result<PreparedZipFile> {
    let options = entry_options(meta, compression, None)?;
    let mut builder = ZipFileBuilder::new(&meta.path, options).map_err(convert)?;
    copy(&mut data.open()?, &mut builder, ctx)?;
    builder.finish().map_err(convert)
}

/// A ZIP archive being written.
pub struct ZipCreator<W: Write + Seek + Send> {
    zip: ZipWriter<W>,
    compression: (CompressionMethod, Option<i64>),
    password: Option<Password>,
}

impl<W: Write + Seek + Send> ZipCreator<W> {
    pub fn new(output: W, options: &CreateOptions) -> Result<Self> {
        let mut zip = ZipWriter::new(output).set_auto_large_file();
        if let Some(comment) = &options.comment {
            zip.set_comment(comment.as_str())
                .map_err(|_| Error::new("commentTooLong"))?;
        }
        Ok(Self {
            zip,
            compression: compression(options)?,
            password: options
                .password
                .clone()
                .filter(|password| !password.is_empty()),
        })
    }

    /// Writes the files of a batch: compressed on all threads, appended in
    /// their order.
    fn flush(
        &mut self,
        batch: &mut Vec<Item>,
        pool: &rayon::ThreadPool,
        ctx: &OpContext,
    ) -> Result<()> {
        if batch.is_empty() {
            return Ok(());
        }
        let compression = self.compression;
        let prepared: Vec<Result<PreparedZipFile>> = pool.install(|| {
            batch
                .par_iter()
                .map(|item| {
                    let data = item.data.as_deref().expect("a batch holds files only");
                    prepare(&item.meta, data, compression, ctx)
                })
                .collect()
        });
        batch.clear();
        for file in prepared {
            self.zip.add_prepared_file(file?).map_err(convert)?;
        }
        Ok(())
    }
}

impl<W: Write + Seek + Send> ArchiveWriter for ZipCreator<W> {
    fn add(&mut self, meta: &EntryMeta, data: &mut dyn Read, ctx: &OpContext) -> Result<()> {
        ctx.checkpoint()?;
        let options = entry_options(meta, self.compression, self.password.as_ref())?;
        match meta.kind {
            EntryKind::Directory => self
                .zip
                .add_directory(meta.path.as_str(), options)
                .map_err(convert),
            EntryKind::Symlink => {
                let target = meta.link_target.as_deref().unwrap_or_default();
                self.zip
                    .add_symlink(&meta.path, target, options)
                    .map_err(convert)
            }
            EntryKind::Hardlink => Err(link_unsupported(meta)),
            EntryKind::File => {
                self.zip.start_file(&meta.path, options).map_err(convert)?;
                copy(data, &mut self.zip, ctx)
            }
        }
    }

    fn add_all(&mut self, items: Vec<Item>, ctx: &OpContext) -> Result<()> {
        let threads = ctx.budget.threads;
        // An encrypted entry is compressed by the writer itself, in order.
        if threads <= 1 || self.password.is_some() {
            for item in items {
                match &item.data {
                    Some(data) => self.add(&item.meta, &mut data.open()?, ctx)?,
                    None => self.add(&item.meta, &mut io::empty(), ctx)?,
                }
            }
            return Ok(());
        }
        let pool = support::pool(threads)?;
        // A batch is held in memory compressed, and while it is compressed
        // every thread holds a file of it.
        let batch_bytes = (ctx.budget.memory / 4).max(BATCH_BYTES_MIN);
        let single = batch_bytes / threads as u64;
        let mut batch: Vec<Item> = Vec::new();
        let mut held = 0u64;
        for item in items {
            ctx.checkpoint()?;
            let size = item.meta.size.unwrap_or(u64::MAX);
            let small = item.meta.is_file() && item.data.is_some() && size <= single;
            if small {
                if held + size > batch_bytes || batch.len() >= BATCH_FILES {
                    self.flush(&mut batch, &pool, ctx)?;
                    held = 0;
                }
                held += size;
                batch.push(item);
                continue;
            }
            // Folders, links and files too large to hold: written in place.
            self.flush(&mut batch, &pool, ctx)?;
            held = 0;
            match &item.data {
                Some(data) => self.add(&item.meta, &mut data.open()?, ctx)?,
                None => self.add(&item.meta, &mut io::empty(), ctx)?,
            }
        }
        self.flush(&mut batch, &pool, ctx)
    }

    fn finish(self: Box<Self>) -> Result<()> {
        let mut output = self.zip.finish().map_err(convert)?;
        output.flush()?;
        Ok(())
    }
}

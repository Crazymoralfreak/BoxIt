//! TAR, plain or inside a compressed stream.
//!
//! A tar is a sequence: headers and data follow one another, and nothing
//! tells where an entry is but the entries before it. A compressed tar is
//! therefore read in one pass, front to back, and listing it means
//! decompressing all of it. A plain tar is listed by jumping from header to
//! header.

use std::borrow::Cow;
use std::io::{self, Read};

use boxit_core::{
    ArchiveReader, ArchiveWriter, Capabilities, Entry, EntryKind, EntryMeta, EntrySink, Error,
    OpContext, Result, Selection, Stream, Timestamp,
};
use tar::{Archive, Builder, EntryType, Header, UstarHeader};

use crate::names::{NameDecoder, NameEncoding};
use crate::source::Source;
use crate::stream::{self, Encoder};
use crate::support::{decode_error, Watched};
use crate::OpenOptions;

/// Size of the buffer between a decoder and the tar reader, which asks for
/// 512 bytes at a time.
const DECODED_BUFFER: usize = 256 * 1024;

const DEFAULT_FILE_MODE: u32 = 0o644;
const DEFAULT_FOLDER_MODE: u32 = 0o755;

/// The name of a compression as the list of an archive shows it.
pub(crate) fn stream_name(stream: Stream) -> &'static str {
    match stream {
        Stream::Gzip => "gzip",
        Stream::Bzip2 => "bzip2",
        Stream::Xz => "xz",
        Stream::Zstd => "zstd",
        Stream::Lz4 => "lz4",
    }
}

/// Reads the names of one tar.
///
/// A name of an extended header is UTF-8 by the standard, and so is nearly
/// every name of a plain header written this century. What is not UTF-8 is
/// in the code page of the system the archive was made on, as the tar of
/// Windows writes it: the first such name settles the code page for the
/// rest of the archive, unless the user has named one.
struct Names<'a> {
    choice: NameEncoding,
    hint: Option<&'a str>,
    settled: Option<NameDecoder>,
}

impl Names<'_> {
    fn read(&mut self, raw: &[u8]) -> String {
        if let Ok(name) = std::str::from_utf8(raw) {
            return name.to_owned();
        }
        let (choice, hint) = (self.choice, self.hint);
        self.settled
            .get_or_insert_with(|| NameDecoder::new(choice, [raw], hint))
            .decode(raw, false)
            .into_owned()
    }
}

/// A time of the extended header: seconds with a decimal fraction.
fn pax_time(value: &str) -> Option<Timestamp> {
    let (whole, fraction) = value.split_once('.').unwrap_or((value, ""));
    let seconds: i64 = whole.parse().ok()?;
    let mut nanos = 0u32;
    let mut scale = 100_000_000u32;
    for digit in fraction.bytes().take(9) {
        nanos += u32::from(digit.checked_sub(b'0').filter(|digit| *digit <= 9)?) * scale;
        scale /= 10;
    }
    Some(Timestamp { seconds, nanos })
}

/// What an entry of a tar is for us; `None` for the kinds that have no
/// place on the disk of the user: devices, pipes.
fn describe<R: Read>(
    index: u64,
    entry: &mut tar::Entry<'_, R>,
    method: Option<&'static str>,
    names: &mut Names<'_>,
) -> Result<Option<Entry>> {
    let header = entry.header();
    let kind = match header.entry_type() {
        EntryType::Regular | EntryType::Continuous | EntryType::GNUSparse => EntryKind::File,
        EntryType::Directory => EntryKind::Directory,
        EntryType::Symlink => EntryKind::Symlink,
        EntryType::Link => EntryKind::Hardlink,
        _ => return Ok(None),
    };
    let mut modified = header
        .mtime()
        .ok()
        .map(|seconds| Timestamp::from_seconds(i64::try_from(seconds).unwrap_or(i64::MAX)));
    let unix_mode = header.mode().ok().map(|mode| mode & 0o7777);
    let path = names.read(&entry.path_bytes());
    let link_target = entry.link_name_bytes().map(|target| names.read(&target));
    let size = entry.size();
    let mut accessed = None;
    let mut created = None;
    if let Some(extensions) = entry.pax_extensions().map_err(decode_error)? {
        for extension in extensions.flatten() {
            let (Ok(key), Ok(value)) = (extension.key(), extension.value()) else {
                continue;
            };
            match key {
                "mtime" => modified = pax_time(value).or(modified),
                "atime" => accessed = pax_time(value),
                "LIBARCHIVE.creationtime" => created = pax_time(value),
                _ => {}
            }
        }
    }
    let is_file = kind == EntryKind::File;
    Ok(Some(Entry {
        index,
        meta: EntryMeta {
            path,
            kind,
            link_target: if is_file || kind == EntryKind::Directory {
                None
            } else {
                link_target
            },
            size: Some(if is_file { size } else { 0 }),
            modified,
            created,
            accessed,
            unix_mode,
            windows_attributes: None,
        },
        packed_size: None,
        crc32: None,
        encrypted: false,
        method: method.map(Cow::Borrowed),
        comment: None,
    }))
}

/// Walks the entries of a tar and hands `each` every entry we know how to
/// represent. `end` is the size of a plain tar: an entry whose data would
/// lie past it is cut short, which a reader that jumps over data would
/// otherwise never notice.
fn walk<R: Read>(
    entries: tar::Entries<'_, R>,
    method: Option<&'static str>,
    end: Option<u64>,
    mut names: Names<'_>,
    ctx: &OpContext,
    mut each: impl FnMut(Entry, &mut tar::Entry<'_, R>) -> Result<()>,
) -> Result<()> {
    let mut index = 0u64;
    for entry in entries {
        ctx.checkpoint()?;
        let mut entry = entry.map_err(decode_error)?;
        if let Some(end) = end {
            // A sparse file takes less room than its size says.
            let stored = if entry.header().entry_type() == EntryType::GNUSparse {
                entry.header().entry_size().unwrap_or(0)
            } else {
                entry.size()
            };
            if entry.raw_file_position().saturating_add(stored) > end {
                let name = names.read(&entry.path_bytes());
                return Err(Error::new("truncated").with("name", name));
            }
        }
        if let Some(described) = describe(index, &mut entry, method, &mut names)? {
            index += 1;
            each(described, &mut entry)?;
        }
    }
    Ok(())
}

/// A tar archive opened for reading.
pub struct TarReader {
    source: Source,
    stream: Option<Stream>,
    options: OpenOptions,
    /// The list, once it has been read.
    listed: Option<Vec<Entry>>,
}

impl TarReader {
    pub fn new(source: Source, stream: Option<Stream>, options: OpenOptions) -> Self {
        Self {
            source,
            stream,
            options,
            listed: None,
        }
    }

    /// Reads the archive from its start. A plain tar skips the data of the
    /// entries `each` does not read without reading it.
    fn pass(
        &self,
        ctx: &OpContext,
        each: impl FnMut(Entry, &mut dyn Read) -> Result<()>,
    ) -> Result<()> {
        let mut each = each;
        let input = self.source.reader()?;
        let names = Names {
            choice: self.options.encoding,
            hint: self.options.hint.as_deref(),
            settled: None,
        };
        match self.stream {
            None => {
                let mut archive = Archive::new(input);
                let entries = archive.entries_with_seek().map_err(decode_error)?;
                let end = Some(self.source.size());
                walk(entries, None, end, names, ctx, |entry, data| {
                    each(entry, data)
                })
            }
            Some(stream) => {
                let decoder = stream::decoder(stream, input, ctx.budget.memory)?;
                let mut archive =
                    Archive::new(io::BufReader::with_capacity(DECODED_BUFFER, decoder));
                let entries = archive.entries().map_err(decode_error)?;
                let method = Some(stream_name(stream));
                walk(entries, method, None, names, ctx, |entry, data| {
                    each(entry, data)
                })?;
                // A tar ends before its stream does: the checksum of the
                // stream and its last bytes are still ahead.
                let mut rest = archive.into_inner();
                let mut block = vec![0u8; DECODED_BUFFER];
                loop {
                    ctx.checkpoint()?;
                    if rest.read(&mut block).map_err(decode_error)? == 0 {
                        return Ok(());
                    }
                }
            }
        }
    }
}

impl ArchiveReader for TarReader {
    fn capabilities(&self) -> Capabilities {
        Capabilities {
            random_access: false,
            solid: self.stream.is_some(),
            parallel: false,
            // The list of a compressed tar costs a pass over all of it.
            has_index: self.stream.is_none(),
        }
    }

    fn entries(&mut self, ctx: &OpContext) -> Result<Vec<Entry>> {
        if let Some(listed) = &self.listed {
            return Ok(listed.clone());
        }
        // A compressed tar is read to its end for its list: whoever waits
        // for it sees the entries being found.
        let watched = self.stream.is_some();
        let mut listed = Vec::new();
        self.pass(ctx, |entry, _| {
            if watched {
                ctx.progress.entry(&entry.meta.path);
            }
            listed.push(entry);
            Ok(())
        })?;
        self.listed = Some(listed.clone());
        Ok(listed)
    }

    fn extract(&mut self, pick: &Selection, sink: &dyn EntrySink, ctx: &OpContext) -> Result<()> {
        let mut listed = Vec::new();
        self.pass(ctx, |entry, data| {
            let result = if !pick.contains(&entry) {
                Ok(())
            } else if entry.meta.is_file() {
                let mut data = Watched::new(Whole {
                    data,
                    left: entry.meta.size.unwrap_or(0),
                });
                sink.accept(&entry, &mut data)
                    .map_err(|error| data.settle(&entry, error))
            } else {
                sink.accept(&entry, &mut io::empty())
            };
            listed.push(entry);
            result
        })?;
        // The pass saw every entry: the list comes for free.
        self.listed = Some(listed);
        Ok(())
    }
}

/// The data of an entry that must be as long as its header says. An archive
/// that ends in the middle of a file reads as a shorter file otherwise.
struct Whole<'a> {
    data: &'a mut dyn Read,
    left: u64,
}

impl Read for Whole<'_> {
    fn read(&mut self, buffer: &mut [u8]) -> io::Result<usize> {
        let read = self.data.read(buffer)?;
        if read == 0 && self.left > 0 && !buffer.is_empty() {
            return Err(io::ErrorKind::UnexpectedEof.into());
        }
        self.left = self.left.saturating_sub(read as u64);
        Ok(read)
    }
}

/// Reads exactly as many bytes as the header of an entry promised: a file
/// that shrank while it was read is filled with zeros, as `tar` does, and
/// one that grew is cut. Anything else would shift every entry after it.
struct Exact<'a> {
    data: &'a mut dyn Read,
    left: u64,
    ended: bool,
}

impl Read for Exact<'_> {
    fn read(&mut self, buffer: &mut [u8]) -> io::Result<usize> {
        let want = buffer
            .len()
            .min(usize::try_from(self.left).unwrap_or(usize::MAX));
        if want == 0 {
            return Ok(0);
        }
        let read = if self.ended {
            0
        } else {
            self.data.read(&mut buffer[..want])?
        };
        let count = if read == 0 {
            self.ended = true;
            buffer[..want].fill(0);
            want
        } else {
            read
        };
        self.left -= count as u64;
        Ok(count)
    }
}

/// A tar archive being written, into a compressed stream or a plain file.
pub struct TarCreator<'a> {
    builder: Builder<Box<dyn Encoder + 'a>>,
}

impl<'a> TarCreator<'a> {
    pub fn new(output: Box<dyn Encoder + 'a>) -> Self {
        Self {
            builder: Builder::new(output),
        }
    }
}

/// The first bytes of a name that fit into `room`, cut between characters.
fn shortened(name: &str, room: usize) -> &[u8] {
    let mut end = name.len().min(room);
    while !name.is_char_boundary(end) {
        end -= 1;
    }
    &name.as_bytes()[..end]
}

/// Puts a path into the fields of a header: whole into the name, or split
/// at a slash between the prefix and the name. `false` when it fits neither
/// way; the header then holds the start of the path.
fn set_path(header: &mut UstarHeader, path: &str) -> bool {
    let bytes = path.as_bytes();
    let (name_room, prefix_room) = (header.name.len(), header.prefix.len());
    if bytes.len() <= name_room {
        header.name[..bytes.len()].copy_from_slice(bytes);
        return true;
    }
    // A slash that leaves a prefix and a name of the sizes the fields take.
    // The slash itself is not stored, and neither part may be empty.
    let split = bytes
        .iter()
        .enumerate()
        .filter(|(at, byte)| {
            **byte == b'/' && *at > 0 && *at <= prefix_room && *at + 1 < bytes.len()
        })
        .map(|(at, _)| at)
        .find(|at| bytes.len() - at - 1 <= name_room);
    if let Some(at) = split {
        header.prefix[..at].copy_from_slice(&bytes[..at]);
        header.name[..bytes.len() - at - 1].copy_from_slice(&bytes[at + 1..]);
        return true;
    }
    let start = shortened(path, name_room);
    header.name[..start.len()].copy_from_slice(start);
    false
}

impl TarCreator<'_> {
    /// Writes the header of an entry, with an extended header before it for
    /// what the fields of the header cannot hold.
    ///
    /// The header is the one of POSIX (`ustar`), and the extended one (`pax`)
    /// carries a path or a link target that is too long or has characters
    /// beyond ASCII. Those are in UTF-8 by the standard, where the fields
    /// of a header are read in whatever code page the reader assumes: the
    /// tar of Windows takes them for the code page of the system.
    fn entry(
        &mut self,
        meta: &EntryMeta,
        kind: EntryType,
        size: u64,
        data: &mut dyn Read,
    ) -> Result<()> {
        let mut header = Header::new_ustar();
        let folder = meta.is_dir();
        header.set_entry_type(kind);
        header.set_mode(meta.unix_mode.unwrap_or(if folder {
            DEFAULT_FOLDER_MODE
        } else {
            DEFAULT_FILE_MODE
        }));
        header.set_uid(0);
        header.set_gid(0);
        let modified = meta.modified.map_or(0, |time| time.seconds.max(0));
        header.set_mtime(modified as u64);
        header.set_size(size);

        let path = if folder {
            Cow::Owned(format!("{}/", meta.path.trim_end_matches('/')))
        } else {
            Cow::Borrowed(meta.path.as_str())
        };
        let target = match meta.kind {
            EntryKind::Symlink | EntryKind::Hardlink => meta.link_target.as_deref(),
            EntryKind::File | EntryKind::Directory => None,
        };
        let mut extended: Vec<(&str, &[u8])> = Vec::new();
        if let Some(fields) = header.as_ustar_mut() {
            if !set_path(fields, &path) || !path.is_ascii() {
                extended.push(("path", path.as_bytes()));
                // The name field is what a reader uses when it does not
                // understand the extended header. Bytes that are not ASCII
                // are not a name in the code page of Windows, and its tar
                // then skips the entry as unreadable.
                if !path.is_ascii() {
                    let ascii: String = path
                        .chars()
                        .map(|character| if character.is_ascii() { character } else { '_' })
                        .collect();
                    let _ = set_path(fields, &ascii);
                }
            }
            if let Some(target) = target {
                let start = shortened(target, fields.linkname.len());
                fields.linkname[..start.len()].copy_from_slice(start);
                if start.len() < target.len() || !target.is_ascii() {
                    extended.push(("linkpath", target.as_bytes()));
                }
            }
        }
        self.builder.append_pax_extensions(extended)?;
        header.set_cksum();
        self.builder.append(&header, data)?;
        Ok(())
    }
}

impl ArchiveWriter for TarCreator<'_> {
    fn add(&mut self, meta: &EntryMeta, data: &mut dyn Read, ctx: &OpContext) -> Result<()> {
        ctx.checkpoint()?;
        match meta.kind {
            EntryKind::File => {
                let Some(size) = meta.size else {
                    // The header goes before the data and holds its size:
                    // content of unknown length is read whole first.
                    let mut content = Vec::new();
                    data.read_to_end(&mut content)?;
                    let size = content.len() as u64;
                    return self.entry(meta, EntryType::Regular, size, &mut content.as_slice());
                };
                let mut exact = Exact {
                    data,
                    left: size,
                    ended: false,
                };
                self.entry(meta, EntryType::Regular, size, &mut exact)
            }
            EntryKind::Directory => self.entry(meta, EntryType::Directory, 0, &mut io::empty()),
            EntryKind::Symlink => self.entry(meta, EntryType::Symlink, 0, &mut io::empty()),
            EntryKind::Hardlink => self.entry(meta, EntryType::Link, 0, &mut io::empty()),
        }
    }

    fn finish(self: Box<Self>) -> Result<()> {
        // Taking the output back writes the blocks that end a tar.
        let output = self.builder.into_inner()?;
        output.finish()?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use encoding_rs::WINDOWS_1251;

    use super::*;
    use crate::stream::Plain;

    fn written(entries: &[EntryMeta]) -> Vec<u8> {
        let mut bytes = Vec::new();
        let mut creator = Box::new(TarCreator::new(Box::new(Plain(&mut bytes))));
        let ctx = OpContext::silent();
        for meta in entries {
            let size = meta.size.unwrap_or(0) as usize;
            creator
                .add(meta, &mut vec![b'x'; size].as_slice(), &ctx)
                .unwrap();
        }
        creator.finish().unwrap();
        bytes
    }

    fn listed(bytes: Vec<u8>, options: OpenOptions) -> Vec<Entry> {
        TarReader::new(Source::memory("a.tar", bytes), None, options)
            .entries(&OpContext::silent())
            .unwrap()
    }

    /// The kinds of the headers of a tar, in their order.
    fn kinds(bytes: &[u8]) -> Vec<EntryType> {
        let mut archive = Archive::new(bytes);
        archive.set_ignore_zeros(false);
        archive
            .entries()
            .unwrap()
            .raw(true)
            .map(|entry| entry.unwrap().header().entry_type())
            .collect()
    }

    #[test]
    fn a_path_goes_into_the_name_or_is_split_at_a_slash() {
        let name_of = |header: &UstarHeader| {
            let text = |field: &[u8]| {
                let end = field
                    .iter()
                    .position(|byte| *byte == 0)
                    .unwrap_or(field.len());
                String::from_utf8_lossy(&field[..end]).into_owned()
            };
            (text(&header.prefix), text(&header.name))
        };
        let fields = |path: &str| {
            let mut header = Header::new_ustar();
            let fits = set_path(header.as_ustar_mut().unwrap(), path);
            let (prefix, name) = name_of(header.as_ustar_mut().unwrap());
            (fits, prefix, name)
        };
        assert_eq!(
            fields("docs/readme.txt"),
            (true, String::new(), String::from("docs/readme.txt"))
        );
        // Exactly the size of the field: no room for the zero, none needed.
        let exact = "a".repeat(100);
        assert_eq!(fields(&exact), (true, String::new(), exact));
        let folder = "f".repeat(80);
        let file = "n".repeat(60);
        assert_eq!(
            fields(&format!("{folder}/{file}")),
            (true, folder.clone(), file.clone())
        );
        // A name of one part longer than its field fits nowhere.
        let long = "n".repeat(101);
        let (fits, prefix, name) = fields(&long);
        assert!(!fits);
        assert_eq!((prefix.as_str(), name.len()), ("", 100));
        // Nor does a path whose folders are longer than theirs.
        let (fits, _, _) = fields(&format!("{}/{file}", "f".repeat(156)));
        assert!(!fits);
        // The start that is kept is cut between characters.
        let (fits, _, name) = fields(&"я".repeat(60));
        assert!(!fits);
        assert_eq!(name, "я".repeat(50));
    }

    #[test]
    fn plain_names_take_a_header_and_the_others_take_two() {
        let short = written(&[EntryMeta::file("docs/readme.txt", 3)]);
        assert_eq!(kinds(&short), [EntryType::Regular]);
        assert_eq!(&short[257..263], b"ustar\0");

        let split = format!("{}/{}", "f".repeat(120), "n".repeat(90));
        assert_eq!(
            kinds(&written(&[EntryMeta::file(split, 3)])),
            [EntryType::Regular]
        );

        for path in [
            "docs/пример.txt".to_owned(),
            format!("docs/{}", "n".repeat(150)),
            format!("{}/file", "f".repeat(200)),
        ] {
            let bytes = written(&[EntryMeta::file(path.clone(), 3)]);
            assert_eq!(
                kinds(&bytes),
                [EntryType::XHeader, EntryType::Regular],
                "{path}"
            );
        }
    }

    #[test]
    fn names_of_any_length_and_alphabet_come_back() {
        let deep = (0..40)
            .map(|level| format!("папка {level}"))
            .collect::<Vec<_>>()
            .join("/");
        let entries = [
            EntryMeta::directory(deep.clone()),
            EntryMeta::file(
                format!("{deep}/файл с очень длинным именем {}.txt", "я".repeat(120)),
                5,
            ),
            EntryMeta::file(format!("{}/{}", "f".repeat(155), "n".repeat(100)), 1),
            EntryMeta::symlink("link", format!("{deep}/цель")),
            EntryMeta::symlink(format!("{}/link", "l".repeat(130)), "t".repeat(180)),
            EntryMeta {
                kind: EntryKind::Hardlink,
                link_target: Some(format!("{}/{}", "f".repeat(155), "n".repeat(100))),
                ..EntryMeta::file("again", 0)
            },
        ];
        let got = listed(written(&entries), OpenOptions::default());
        assert_eq!(got.len(), entries.len());
        for (got, wanted) in got.iter().zip(&entries) {
            assert_eq!(
                got.meta.path.trim_end_matches('/'),
                wanted.path.trim_end_matches('/')
            );
            assert_eq!(got.meta.kind, wanted.kind, "{}", wanted.path);
            assert_eq!(got.meta.link_target, wanted.link_target, "{}", wanted.path);
            assert_eq!(got.meta.size, wanted.size, "{}", wanted.path);
        }
    }

    #[test]
    fn names_in_a_code_page_of_windows_are_read() {
        // What the tar of a Russian Windows writes: the name in the code
        // page of the system, in a header that does not say so.
        let (name, _, _) = WINDOWS_1251.encode("Отчёт за квартал/Итоговая таблица.txt");
        let (second, _, _) = WINDOWS_1251.encode("Отчёт за квартал/Приложение.txt");
        let mut builder = Builder::new(Vec::new());
        for raw in [&name, &second] {
            let mut header = Header::new_ustar();
            header.as_ustar_mut().unwrap().name[..raw.len()].copy_from_slice(raw);
            header.set_entry_type(EntryType::Regular);
            header.set_mode(0o644);
            header.set_size(0);
            header.set_cksum();
            builder.append(&header, io::empty()).unwrap();
        }
        let bytes = builder.into_inner().unwrap();

        let hinted = OpenOptions {
            hint: Some(String::from("ru")),
            ..OpenOptions::default()
        };
        let got = listed(bytes.clone(), hinted);
        assert_eq!(got[0].meta.path, "Отчёт за квартал/Итоговая таблица.txt");
        assert_eq!(got[1].meta.path, "Отчёт за квартал/Приложение.txt");

        // The user names another code page, and is obeyed.
        let chosen = OpenOptions {
            encoding: NameEncoding::from_label("ibm866").unwrap(),
            ..OpenOptions::default()
        };
        let got = listed(bytes, chosen);
        assert_ne!(got[0].meta.path, "Отчёт за квартал/Итоговая таблица.txt");
        assert!(!got[0].meta.path.contains('\u{fffd}'));
    }
}

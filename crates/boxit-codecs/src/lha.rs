//! LHA, also called LZH: the archive of LHarc and LHA, common in Japan and
//! on the Amiga.
//!
//! Entries follow one another, each behind its header, and each compressed
//! on its own. The headers grew through four levels, and the newer ones
//! carry what the oldest had no room for in extensions: a folder apart from
//! the name, the permissions of Unix, times of every kind. The names are in
//! the code page of the machine that wrote them, more often than not the
//! Japanese one.

use std::borrow::Cow;
use std::io::{self, Read, Seek};

use boxit_core::{
    ArchiveReader, Capabilities, Entry, EntryKind, EntryMeta, EntrySink, Error, OpContext, Result,
    Selection, Timestamp,
};
use chrono::{Datelike, Timelike};
use delharc::decode::{Decoder, LhaDecodeError};
use delharc::header::ext::{EXT_HEADER_FILENAME, EXT_HEADER_PATH, EXT_HEADER_UNIX_PERM};
use delharc::header::TimestampResult;
use delharc::{LhaDecodeReader, LhaError, LhaHeader};

use crate::bytes::le16;
use crate::names::NameDecoder;
use crate::source::{Source, SourceReader};
use crate::support::{decode_error, from_local, unsupported_method, Civil, Watched};
use crate::OpenOptions;

/// What separates the folders of a path in an extension of a header.
const PATH_SEPARATOR: u8 = 0xFF;
/// The identifier of Unix among the systems an archive may come from.
const OS_UNIX: u8 = b'U';
/// The bits of a mode of Unix that say a file is a symbolic link.
const KIND_MASK: u16 = 0o170_000;
const KIND_LINK: u16 = 0o120_000;
/// What separates the name of a link from its target.
const LINK_SEPARATOR: u8 = b'|';
/// The method of an entry that is a folder or a link, without content.
const METHOD_FOLDER: [u8; 5] = *b"-lhd-";

fn convert(error: LhaDecodeError<SourceReader>) -> Error {
    decode_error(io::Error::from(LhaError::from(error)))
}

/// An entry as its header describes it, its name not yet read as text.
struct Raw {
    path: Vec<u8>,
    target: Option<Vec<u8>>,
    kind: EntryKind,
    /// The name may have the separator of DOS between its folders.
    foreign: bool,
    entry: Entry,
}

/// The permissions of Unix, where the header has them.
fn unix_mode(header: &LhaHeader) -> Option<u16> {
    for extra in header.iter_extra() {
        if let [EXT_HEADER_UNIX_PERM, data @ ..] = extra {
            return le16(data, 0);
        }
    }
    // The oldest header of Unix keeps them after the system and a version,
    // behind the time.
    if header.level == 0 && header.extended_area.first() == Some(&OS_UNIX) {
        return le16(&header.extended_area, 6);
    }
    None
}

/// The path of an entry as the header has it, with `/` between folders.
fn raw_path(header: &LhaHeader) -> Vec<u8> {
    let mut folder: &[u8] = &[];
    let mut name: &[u8] = &[];
    for extra in header.iter_extra() {
        match extra {
            [EXT_HEADER_FILENAME, data @ ..] => name = data,
            [EXT_HEADER_PATH, data @ ..] => folder = data,
            _ => {}
        }
    }
    if name.is_empty() {
        name = &header.filename;
    }
    // The Amiga keeps a comment after the name, behind a zero.
    let cut = |text: &[u8]| -> Vec<u8> {
        let end = text
            .iter()
            .position(|byte| *byte == 0)
            .unwrap_or(text.len());
        text[..end].to_vec()
    };
    let mut path: Vec<u8> = cut(folder)
        .into_iter()
        .map(|byte| if byte == PATH_SEPARATOR { b'/' } else { byte })
        .collect();
    if !path.is_empty() && path.last() != Some(&b'/') {
        path.push(b'/');
    }
    path.extend(cut(name));
    path
}

fn modified(header: &LhaHeader) -> Option<Timestamp> {
    match header.parse_last_modified() {
        TimestampResult::None => None,
        TimestampResult::Utc(moment) => Some(Timestamp {
            seconds: moment.timestamp(),
            nanos: moment.timestamp_subsec_nanos(),
        }),
        // The time of DOS: of the clock on the wall where the file was.
        TimestampResult::Naive(moment) => from_local(Civil {
            year: moment.year(),
            month: moment.month(),
            day: moment.day(),
            hour: moment.hour(),
            minute: moment.minute(),
            second: moment.second(),
        }),
    }
}

fn describe(index: u64, header: &LhaHeader) -> Raw {
    let mode = unix_mode(header);
    let mut path = raw_path(header);
    let mut target = None;
    let link = mode.is_some_and(|mode| mode & KIND_MASK == KIND_LINK);
    let kind = if link {
        // A link is stored as "name|target".
        if let Some(at) = path.iter().position(|byte| *byte == LINK_SEPARATOR) {
            target = Some(path.split_off(at + 1));
            path.pop();
        }
        EntryKind::Symlink
    } else if header.is_directory() || header.compression == METHOD_FOLDER {
        EntryKind::Directory
    } else {
        EntryKind::File
    };
    let is_file = kind == EntryKind::File;
    let method = String::from_utf8_lossy(&header.compression)
        .trim_matches('-')
        .to_owned();
    let attributes = u32::from(header.msdos_attrs.bits());
    Raw {
        path,
        target,
        kind,
        foreign: header.os_type != OS_UNIX,
        entry: Entry {
            index,
            meta: EntryMeta {
                size: Some(if is_file { header.original_size } else { 0 }),
                modified: modified(header),
                unix_mode: mode.map(|mode| u32::from(mode & 0o7777)),
                windows_attributes: (attributes != 0).then_some(attributes),
                ..EntryMeta::default()
            },
            packed_size: is_file.then_some(header.compressed_size),
            method: is_file.then_some(Cow::Owned(method)),
            comment: header.parse_comment().map(Cow::into_owned),
            ..Entry::default()
        },
    }
}

/// The content of an entry, with its checksum looked at when it ends.
struct Checked<'a> {
    decoder: &'a mut LhaDecodeReader<SourceReader>,
    done: bool,
}

impl Read for Checked<'_> {
    fn read(&mut self, buffer: &mut [u8]) -> io::Result<usize> {
        let read = self.decoder.read(buffer)?;
        if read == 0 && !buffer.is_empty() && !self.done {
            self.done = true;
            if !self.decoder.crc_is_ok() {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    "the checksum of the file does not match",
                ));
            }
        }
        Ok(read)
    }
}

/// An LHA archive opened for reading.
pub struct LhaReader {
    source: Source,
    options: OpenOptions,
    /// The list, once it has been read.
    listed: Option<Vec<Entry>>,
}

impl LhaReader {
    pub fn new(source: Source, options: OpenOptions) -> Self {
        Self {
            source,
            options,
            listed: None,
        }
    }

    /// Goes through the archive from its start, header by header. The
    /// content of an entry `each` does not read is jumped over.
    fn pass(
        &self,
        ctx: &OpContext,
        mut each: impl FnMut(u64, &mut LhaDecodeReader<SourceReader>) -> Result<()>,
    ) -> Result<()> {
        let size = self.source.size();
        let mut decoder = LhaDecodeReader::default();
        let mut more = decoder.begin_new(self.source.reader()?).map_err(convert)?;
        let mut index = 0u64;
        while more {
            ctx.checkpoint()?;
            // A jump past the end of the file would look like the end of
            // the archive.
            let packed = decoder
                .get_decoder()
                .map_or(0, |inner| inner.get_ref().limit());
            let position = match decoder.get_mut() {
                Some(reader) => reader.stream_position()?,
                None => 0,
            };
            if position.saturating_add(packed) > size {
                return Err(Error::new("truncated"));
            }
            each(index, &mut decoder)?;
            index += 1;
            more = decoder.seek_next_file().map_err(convert)?;
        }
        Ok(())
    }

    fn list(&self, ctx: &OpContext) -> Result<Vec<Entry>> {
        let mut raw = Vec::new();
        self.pass(ctx, |index, decoder| {
            raw.push(describe(index, decoder.header()));
            Ok(())
        })?;
        let names = NameDecoder::new(
            self.options.encoding,
            raw.iter().map(|entry| entry.path.as_slice()),
            self.options.hint.as_deref(),
        );
        Ok(raw
            .into_iter()
            .map(|raw| {
                let text = |bytes: &[u8]| {
                    let text = names.decode(bytes, false);
                    if raw.foreign && text.contains('\\') {
                        text.replace('\\', "/")
                    } else {
                        text.into_owned()
                    }
                };
                let mut entry = raw.entry;
                entry.meta.path = text(&raw.path);
                entry.meta.kind = raw.kind;
                entry.meta.link_target = raw.target.as_deref().map(text);
                entry
            })
            .collect())
    }
}

impl ArchiveReader for LhaReader {
    fn capabilities(&self) -> Capabilities {
        Capabilities {
            random_access: false,
            solid: false,
            parallel: false,
            has_index: true,
        }
    }

    fn entries(&mut self, ctx: &OpContext) -> Result<Vec<Entry>> {
        if self.listed.is_none() {
            self.listed = Some(self.list(ctx)?);
        }
        Ok(self.listed.clone().unwrap_or_default())
    }

    fn extract(&mut self, pick: &Selection, sink: &dyn EntrySink, ctx: &OpContext) -> Result<()> {
        // The names are read as text with all of them at hand.
        let listed = self.entries(ctx)?;
        self.pass(ctx, |index, decoder| {
            let Some(entry) = listed.get(index as usize) else {
                return Err(Error::new("archiveChanged").with("path", self.source.path().display()));
            };
            if !pick.contains(entry) {
                return Ok(());
            }
            if !entry.meta.is_file() {
                return sink.accept(entry, &mut io::empty());
            }
            if !decoder.is_decoder_supported() {
                return Err(unsupported_method(
                    entry.method.as_deref().unwrap_or_default(),
                ));
            }
            let mut data = Watched::new(Checked {
                decoder,
                done: false,
            });
            sink.accept(entry, &mut data)
                .map_err(|error| data.settle(entry, error))
        })
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use encoding_rs::SHIFT_JIS;

    use super::*;
    use crate::tests::Collector;

    /// The checksum of LHA: CRC-16 of the polynomial 0xA001.
    fn crc16(data: &[u8]) -> u16 {
        let mut crc = 0u16;
        for byte in data {
            crc ^= u16::from(*byte);
            for _ in 0..8 {
                crc = if crc & 1 == 1 {
                    (crc >> 1) ^ 0xA001
                } else {
                    crc >> 1
                };
            }
        }
        crc
    }

    /// The date and the time of DOS for noon of 2023-11-14.
    const DOS_TIME: u32 = ((2023 - 1980) << 25) | (11 << 21) | (14 << 16) | (12 << 11);

    /// An entry with a header of the oldest level, the content stored.
    pub(crate) fn level0(name: &[u8], data: &[u8]) -> Vec<u8> {
        let mut body = Vec::new();
        body.extend(b"-lh0-");
        body.extend((data.len() as u32).to_le_bytes());
        body.extend((data.len() as u32).to_le_bytes());
        body.extend(DOS_TIME.to_le_bytes());
        body.extend([0x20, 0]);
        body.push(name.len() as u8);
        body.extend(name);
        body.extend(crc16(data).to_le_bytes());
        let sum = body.iter().fold(0u8, |sum, byte| sum.wrapping_add(*byte));
        let mut bytes = vec![body.len() as u8, sum];
        bytes.extend(body);
        bytes.extend(data);
        bytes
    }

    /// An entry with a header of the second level: the name, the folder
    /// and the permissions of Unix in extensions, the time in seconds.
    fn level2(folder: &[&[u8]], name: &[u8], mode: u16, method: &[u8], data: &[u8]) -> Vec<u8> {
        let mut extras: Vec<Vec<u8>> = Vec::new();
        extras.push([&[EXT_HEADER_FILENAME][..], name].concat());
        if !folder.is_empty() {
            let mut path = vec![EXT_HEADER_PATH];
            for part in folder {
                path.extend(*part);
                path.push(PATH_SEPARATOR);
            }
            extras.push(path);
        }
        extras.push([&[EXT_HEADER_UNIX_PERM][..], &mode.to_le_bytes()].concat());
        // The checksum of the header, filled in at the end.
        extras.push(vec![0x00, 0, 0]);

        let mut bytes = vec![0u8; 2];
        bytes.extend(method);
        bytes.extend((data.len() as u32).to_le_bytes());
        bytes.extend((data.len() as u32).to_le_bytes());
        bytes.extend(1_700_000_000u32.to_le_bytes());
        bytes.extend([0x20, 2]);
        bytes.extend(crc16(data).to_le_bytes());
        bytes.push(OS_UNIX);
        for extra in &extras {
            bytes.extend((extra.len() as u16 + 2).to_le_bytes());
            bytes.extend(extra);
        }
        bytes.extend(0u16.to_le_bytes());
        let length = bytes.len() as u16;
        bytes[..2].copy_from_slice(&length.to_le_bytes());
        let crc = crc16(&bytes);
        let at = bytes.len() - 4;
        bytes[at..at + 2].copy_from_slice(&crc.to_le_bytes());
        bytes.extend(data);
        bytes
    }

    fn read(bytes: Vec<u8>, options: OpenOptions) -> Result<Vec<(Entry, Vec<u8>)>> {
        let ctx = OpContext::silent();
        let mut reader = LhaReader::new(Source::memory("a.lzh", bytes), options);
        let listed = reader.entries(&ctx)?;
        let sink = Collector::default();
        reader.extract(&Selection::All, &sink, &ctx)?;
        let got = sink.in_order();
        assert_eq!(
            listed,
            got.iter().map(|got| got.entry.clone()).collect::<Vec<_>>()
        );
        Ok(got.into_iter().map(|got| (got.entry, got.data)).collect())
    }

    pub(crate) fn archive(entries: &[Vec<u8>]) -> Vec<u8> {
        let mut bytes = entries.concat();
        bytes.push(0);
        bytes
    }

    #[test]
    fn the_oldest_headers_are_read() {
        let bytes = archive(&[
            level0(b"README.TXT", b"read me"),
            level0(b"DOCS\\NOTES.TXT", b"notes"),
            level0(b"EMPTY.BIN", b""),
        ]);
        let got = read(bytes, OpenOptions::default()).unwrap();
        let summary: Vec<_> = got
            .iter()
            .map(|(entry, data)| (entry.meta.path.as_str(), data.as_slice()))
            .collect();
        assert_eq!(
            summary,
            [
                ("README.TXT", &b"read me"[..]),
                ("DOCS/NOTES.TXT", b"notes"),
                ("EMPTY.BIN", b""),
            ]
        );
        let entry = &got[0].0;
        assert_eq!(entry.method.as_deref(), Some("lh0"));
        assert_eq!(entry.meta.size, Some(7));
        assert_eq!(entry.packed_size, Some(7));
        assert_eq!(entry.meta.windows_attributes, Some(0x20));
        assert_eq!(
            entry.meta.modified,
            from_local(Civil {
                year: 2023,
                month: 11,
                day: 14,
                hour: 12,
                minute: 0,
                second: 0
            })
        );
    }

    #[test]
    fn folders_links_and_permissions_come_from_the_extensions() {
        let bytes = archive(&[
            level2(&[b"docs"], b"", 0o040_755, b"-lhd-", b""),
            level2(
                &[b"docs", b"deep"],
                b"note.md",
                0o100_640,
                b"-lh0-",
                b"# Notes\n",
            ),
            level2(&[b"docs"], b"link|deep/note.md", 0o120_777, b"-lhd-", b""),
            level2(&[], b"back\\slash", 0o100_644, b"-lh0-", b"x"),
        ]);
        let got = read(bytes, OpenOptions::default()).unwrap();
        let summary: Vec<_> = got
            .iter()
            .map(|(entry, data)| {
                (
                    entry.meta.path.as_str(),
                    entry.meta.kind,
                    entry.meta.unix_mode,
                    entry.meta.link_target.as_deref(),
                    data.as_slice(),
                )
            })
            .collect();
        assert_eq!(
            summary,
            [
                ("docs/", EntryKind::Directory, Some(0o755), None, &b""[..]),
                (
                    "docs/deep/note.md",
                    EntryKind::File,
                    Some(0o640),
                    None,
                    b"# Notes\n"
                ),
                (
                    "docs/link",
                    EntryKind::Symlink,
                    Some(0o777),
                    Some("deep/note.md"),
                    b""
                ),
                // On Unix a backslash is a character of a name.
                ("back\\slash", EntryKind::File, Some(0o644), None, b"x"),
            ]
        );
        assert_eq!(
            got[1].0.meta.modified,
            Some(Timestamp::from_seconds(1_700_000_000))
        );
        assert_eq!(got[0].0.method, None);
    }

    #[test]
    fn japanese_names_are_read() {
        let (first, _, _) = SHIFT_JIS.encode("新しいフォルダー\\報告書.txt");
        let (second, _, _) = SHIFT_JIS.encode("写真\\東京タワー.jpg");
        // The second byte of a character of this name is a backslash.
        let (third, _, _) = SHIFT_JIS.encode("表示\\能力.txt");
        let bytes = archive(&[
            level0(&first, b"1"),
            level0(&second, b"2"),
            level0(&third, b"3"),
        ]);
        let hinted = OpenOptions {
            hint: Some(String::from("jp")),
            ..OpenOptions::default()
        };
        let got = read(bytes, hinted).unwrap();
        let names: Vec<_> = got
            .iter()
            .map(|(entry, _)| entry.meta.path.as_str())
            .collect();
        assert_eq!(
            names,
            [
                "新しいフォルダー/報告書.txt",
                "写真/東京タワー.jpg",
                "表示/能力.txt"
            ]
        );
    }

    #[test]
    fn only_what_is_picked_is_read() {
        let bytes = archive(&[
            level0(b"A.TXT", b"first"),
            level0(b"B.TXT", b"second"),
            level0(b"C.TXT", b"third"),
        ]);
        let ctx = OpContext::silent();
        let mut reader = LhaReader::new(Source::memory("a.lzh", bytes), OpenOptions::default());
        let sink = Collector::default();
        reader
            .extract(&Selection::paths(["B.TXT"]), &sink, &ctx)
            .unwrap();
        let got = sink.in_order();
        assert_eq!(got.len(), 1);
        assert_eq!(got[0].data, b"second");
        assert_eq!(got[0].entry.index, 1);
    }

    #[test]
    fn what_is_broken_is_said_to_be_broken() {
        let whole = archive(&[level0(b"A.TXT", b"first file"), level0(b"B.TXT", b"second")]);
        let read = |bytes: Vec<u8>| read(bytes, OpenOptions::default()).unwrap_err();
        // The content of the first file is changed: its checksum tells.
        let mut changed = whole.clone();
        changed[30] ^= 0xFF;
        let error = read(changed);
        assert_eq!(error.code, "damagedEntry");
        assert_eq!(error.params["name"], "A.TXT");
        // A header whose sum does not match, and one cut in the middle.
        let mut header = whole.clone();
        header[10] ^= 0x01;
        assert_eq!(read(header).code, "damaged");
        assert_eq!(read(whole[..12].to_vec()).code, "truncated");
        // The archive ends inside the content of the last file.
        assert_eq!(read(whole[..whole.len() - 3].to_vec()).code, "truncated");
        // A method nobody knows.
        let mut method = level2(&[], b"x", 0o100_644, b"-lh9-", b"data");
        method.push(0);
        let error = read(method);
        assert_eq!(error.code, "unsupportedMethod");
        assert_eq!(error.params["method"], "lh9");
    }
}

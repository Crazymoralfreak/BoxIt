//! ISO: the image of an optical disc.
//!
//! A disc may carry several file systems over the same files, each made for
//! different readers. ISO 9660 is the oldest, with short names in capitals;
//! Rock Ridge adds to its records what Unix knows of a file: a long name,
//! permissions, links; Joliet is a second tree of folders with names in
//! Unicode; UDF is the file system of DVDs and of what came after. The
//! richest one a disc has is the one that is read: UDF, then Rock Ridge,
//! then Joliet, then the bare ISO 9660.
//!
//! The files lie in the image as they are, so they are read where they lie.

use std::collections::HashSet;

use boxit_core::{Entry, EntryKind, EntryMeta, Error, OpContext, Result, Timestamp};
use chrono::NaiveDate;

use crate::bytes::{le16, le32};
use crate::names::NameDecoder;
use crate::source::{Source, SourceReader};
use crate::stored::{cut_short, read_at, Extent, Located, StoredReader};
use crate::support::{damaged, MAX_LINK_TARGET};
use crate::{udf, OpenOptions};

/// The size of a sector of a disc.
const SECTOR: u64 = 2048;
/// The sector the descriptors of the volume start at, after the area left
/// to the system, and the most of them that are looked at.
const FIRST_DESCRIPTOR: u64 = 16;
const DESCRIPTORS: u64 = 64;
/// What the descriptors of ISO 9660 are marked with.
const MARK: &[u8] = b"CD001";
const PRIMARY: u8 = 1;
const SUPPLEMENTARY: u8 = 2;
/// Where the record of the root folder is in a descriptor.
const ROOT_RECORD: usize = 156;

/// The shortest record of a folder: one with a name of one byte.
const RECORD: usize = 34;
const FLAG_FOLDER: u8 = 0x02;
/// The record is of the other fork of a file, which nothing here has a
/// place for.
const FLAG_ASSOCIATED: u8 = 0x04;
/// The file goes on in the next record.
const FLAG_MORE: u8 = 0x80;

/// The kinds of a file in a mode of Unix.
const KIND_MASK: u32 = 0o170_000;
const KIND_FOLDER: u32 = 0o040_000;
const KIND_FILE: u32 = 0o100_000;
const KIND_LINK: u32 = 0o120_000;

/// How many times the fields of Rock Ridge of one record may go on
/// elsewhere, and the most that is read of them there.
const MAX_HOPS: usize = 32;
const MAX_AREA: u64 = 64 * 1024;
/// The longest name Rock Ridge may put together.
const MAX_NAME: usize = 64 * 1024;

/// The image and what an operation on it needs.
pub(crate) struct Disc<'a> {
    pub reader: SourceReader,
    pub size: u64,
    pub ctx: &'a OpContext,
}

impl Disc<'_> {
    pub(crate) fn read(&mut self, offset: u64, length: u64) -> Result<Vec<u8>> {
        read_at(&mut self.reader, self.size, offset, length)
    }
}

/// A name as a file system has it: bytes of some code page, or text.
pub(crate) enum Name {
    Bytes(Vec<u8>),
    Text(String),
}

/// A file, a folder or a link of a disc, before its path is put together.
pub(crate) struct Node {
    /// The folder it is in, by its place in the list; none for the root.
    pub parent: Option<usize>,
    pub name: Name,
    pub kind: EntryKind,
    pub target: Option<Name>,
    pub size: u64,
    pub modified: Option<Timestamp>,
    pub accessed: Option<Timestamp>,
    pub created: Option<Timestamp>,
    pub mode: Option<u32>,
    pub extents: Vec<Extent>,
}

/// A moment of the calendar, `zone` quarters of an hour east of Greenwich.
fn moment(date: (i32, u32, u32), time: (u32, u32, u32), zone: i8) -> Option<Timestamp> {
    let local = NaiveDate::from_ymd_opt(date.0, date.1, date.2)?.and_hms_opt(
        time.0,
        time.1,
        time.2.min(59),
    )?;
    let seconds = local.and_utc().timestamp() - i64::from(zone) * 900;
    Some(Timestamp::from_seconds(seconds))
}

/// The time of a record: seven bytes, the year counted from 1900.
fn short_time(bytes: &[u8]) -> Option<Timestamp> {
    let [year, month, day, hour, minute, second, zone] = *bytes.first_chunk::<7>()?;
    if month == 0 {
        return None;
    }
    moment(
        (1900 + i32::from(year), u32::from(month), u32::from(day)),
        (u32::from(hour), u32::from(minute), u32::from(second)),
        zone as i8,
    )
}

/// The time of a descriptor: sixteen digits and the zone.
fn long_time(bytes: &[u8]) -> Option<Timestamp> {
    let (digits, zone) = bytes.first_chunk::<17>()?.split_at(16);
    let number = |from: usize, to: usize| -> Option<u32> {
        std::str::from_utf8(&digits[from..to]).ok()?.parse().ok()
    };
    let year = number(0, 4)?;
    if year == 0 {
        return None;
    }
    moment(
        (year as i32, number(4, 6)?, number(6, 8)?),
        (number(8, 10)?, number(10, 12)?, number(12, 14)?),
        zone[0] as i8,
    )
}

/// A record of a folder: a file or a folder in it.
#[derive(Clone, Debug)]
struct Record {
    /// Where its content is in the image.
    offset: u64,
    size: u64,
    flags: u8,
    name: Vec<u8>,
    modified: Option<Timestamp>,
    /// What the system that wrote the disc added: the fields of Rock Ridge.
    system_use: Vec<u8>,
}

impl Record {
    /// Reads a record from all of its bytes.
    fn parse(bytes: &[u8], block: u64) -> Option<Self> {
        if bytes.len() < RECORD {
            return None;
        }
        let name_length = usize::from(bytes[32]);
        let name = bytes.get(33..33 + name_length)?;
        // The name is padded to an even length of the record so far.
        let after = 33 + name_length + usize::from(name_length % 2 == 0);
        let start = u64::from(le32(bytes, 2)?) + u64::from(bytes[1]);
        Some(Self {
            offset: start.checked_mul(block)?,
            size: u64::from(le32(bytes, 10)?),
            flags: bytes[25],
            name: name.to_vec(),
            modified: short_time(&bytes[18..25]),
            system_use: bytes.get(after..).unwrap_or_default().to_vec(),
        })
    }

    /// The folder itself or the one above it: the first two records of
    /// every folder.
    fn is_dot(&self) -> bool {
        matches!(self.name.as_slice(), [0 | 1])
    }
}

/// One of the trees of folders of a disc.
struct Volume {
    root: Record,
    /// The size of the blocks the records count in.
    block: u64,
    /// The names are UTF-16.
    joliet: bool,
}

impl Volume {
    fn parse(sector: &[u8], joliet: bool) -> Option<Self> {
        let block = match le16(sector, 128)? {
            0 => SECTOR,
            block => u64::from(block),
        };
        let root = sector.get(ROOT_RECORD..ROOT_RECORD + RECORD)?;
        Some(Self {
            root: Record::parse(root, block)?,
            block,
            joliet,
        })
    }
}

/// What the descriptors of a disc tell of it.
#[derive(Default)]
struct Found {
    primary: Option<Volume>,
    joliet: Option<Volume>,
    /// A descriptor says there is UDF on the disc.
    udf: bool,
}

fn descriptors(disc: &mut Disc) -> Result<Found> {
    let mut found = Found::default();
    for index in 0..DESCRIPTORS {
        let offset = (FIRST_DESCRIPTOR + index) * SECTOR;
        if offset + SECTOR > disc.size {
            break;
        }
        let sector = disc.read(offset, SECTOR)?;
        match &sector[1..6] {
            mark if mark == MARK => match sector[0] {
                PRIMARY if found.primary.is_none() => found.primary = Volume::parse(&sector, false),
                // Joliet says which of the sets of Unicode its names are of.
                SUPPLEMENTARY
                    if found.joliet.is_none()
                        && matches!(&sector[88..91], b"%/@" | b"%/C" | b"%/E") =>
                {
                    found.joliet = Volume::parse(&sector, true);
                }
                _ => {}
            },
            b"NSR02" | b"NSR03" => found.udf = true,
            // What else stands among the descriptors of a disc.
            b"BEA01" | b"TEA01" | b"BOOT2" | b"CDW02" => {}
            _ => break,
        }
    }
    Ok(found)
}

/// What Rock Ridge adds to a record.
#[derive(Default)]
struct Ridge {
    name: Vec<u8>,
    mode: Option<u32>,
    target: Option<Vec<u8>>,
    /// The last part of the target goes on in the next one.
    joined: bool,
    modified: Option<Timestamp>,
    accessed: Option<Timestamp>,
    created: Option<Timestamp>,
    /// The folder was moved here from where it is too deep; it shows where
    /// it belongs.
    relocated: bool,
    /// The block of the folder this record stands in for.
    child: Option<u64>,
    /// Something of Rock Ridge was among the fields.
    marked: bool,
}

impl Ridge {
    fn name(&mut self, data: &[u8]) {
        // The flags say when the name is of the folder itself or of the
        // one above, which have no name to give.
        if let Some((flags, name)) = data.split_first() {
            if flags & 0x06 == 0 && self.name.len() + name.len() <= MAX_NAME {
                self.name.extend(name);
            }
        }
    }

    /// The target of a link: its parts, any of which may go on in the next.
    fn link(&mut self, data: &[u8]) {
        let target = self.target.get_or_insert_with(Vec::new);
        let mut parts = data.get(1..).unwrap_or_default();
        while let [flags, length, rest @ ..] = parts {
            let Some(text) = rest.get(..usize::from(*length)) else {
                break;
            };
            if target.len() as u64 > MAX_LINK_TARGET {
                break;
            }
            if !self.joined && !target.is_empty() && target.last() != Some(&b'/') {
                target.push(b'/');
            }
            match flags & 0x0E {
                0x02 => target.push(b'.'),
                0x04 => target.extend(b".."),
                0x08 => {
                    target.clear();
                    target.push(b'/');
                }
                _ => target.extend(text),
            }
            self.joined = flags & 1 != 0;
            parts = &rest[text.len()..];
        }
    }

    fn times(&mut self, data: &[u8]) {
        let Some((flags, mut stamps)) = data.split_first() else {
            return;
        };
        let long = flags & 0x80 != 0;
        let width = if long { 17 } else { 7 };
        for bit in 0..7 {
            if flags & (1 << bit) == 0 {
                continue;
            }
            let Some(stamp) = stamps.get(..width) else {
                return;
            };
            stamps = &stamps[width..];
            let time = if long {
                long_time(stamp)
            } else {
                short_time(stamp)
            };
            match bit {
                0 => self.created = time,
                1 => self.modified = time,
                2 => self.accessed = time,
                _ => {}
            }
        }
    }
}

/// Reads the fields of Rock Ridge of a record; `area` is where they start.
fn rock_ridge(disc: &mut Disc, block: u64, area: &[u8]) -> Result<Ridge> {
    let mut ridge = Ridge::default();
    let mut area = area.to_vec();
    for _ in 0..=MAX_HOPS {
        let mut next = None;
        let mut at = 0;
        while let Some(head) = area.get(at..at + 4) {
            let length = usize::from(head[2]);
            let Some(data) = area.get(at + 4..at + length.max(4)) else {
                break;
            };
            if length < 4 {
                break;
            }
            at += length;
            match &head[..2] {
                // The fields go on somewhere else on the disc.
                b"CE" => {
                    if let (Some(place), Some(offset), Some(length)) =
                        (le32(data, 0), le32(data, 8), le32(data, 16))
                    {
                        let start = u64::from(place) * block + u64::from(offset);
                        next = Some((start, u64::from(length)));
                    }
                    continue;
                }
                b"ST" => break,
                b"NM" => ridge.name(data),
                b"PX" => ridge.mode = le32(data, 0),
                b"SL" => ridge.link(data),
                b"TF" => ridge.times(data),
                b"RE" => ridge.relocated = true,
                b"CL" => ridge.child = le32(data, 0).map(u64::from),
                b"RR" | b"ER" => {}
                // The fields of other protocols.
                _ => continue,
            }
            ridge.marked = true;
        }
        let Some((start, length)) = next else {
            return Ok(ridge);
        };
        if length > MAX_AREA {
            break;
        }
        area = disc.read(start, length)?;
    }
    Err(damaged("the fields of Rock Ridge never end"))
}

/// The name of plain ISO 9660 without the version after it and without the
/// dot that ends a name with no extension.
fn plain_name(name: &[u8]) -> &[u8] {
    let name = match name.iter().rposition(|byte| *byte == b';') {
        Some(at) if name[at + 1..].iter().all(u8::is_ascii_digit) => &name[..at],
        _ => name,
    };
    name.strip_suffix(b".").unwrap_or(name)
}

/// The name of Joliet: UTF-16 with the high byte first.
fn joliet_name(name: &[u8]) -> String {
    let units: Vec<u16> = name
        .as_chunks::<2>()
        .0
        .iter()
        .map(|pair| u16::from_be_bytes(*pair))
        .collect();
    let text = String::from_utf16_lossy(&units);
    match text.rfind(';') {
        Some(at) if text[at + 1..].bytes().all(|byte| byte.is_ascii_digit()) => {
            text[..at].to_owned()
        }
        _ => text,
    }
}

/// A file, a folder or a link as a folder lists it.
struct Item {
    node: Node,
    /// Where the records of a folder are, and how many bytes of them.
    inside: Option<(u64, u64)>,
}

/// Goes through one of the trees of ISO 9660.
struct Walker<'d, 'a> {
    disc: &'d mut Disc<'a>,
    block: u64,
    joliet: bool,
    /// How many bytes to leave out before the fields of Rock Ridge; none
    /// when the disc has no Rock Ridge.
    ridge: Option<usize>,
}

impl Walker<'_, '_> {
    /// The first record of a folder, which is of the folder itself.
    fn first(&mut self, offset: u64) -> Result<Option<Record>> {
        if offset >= self.disc.size {
            return Err(cut_short());
        }
        let sector = self
            .disc
            .read(offset, SECTOR.min(self.disc.size - offset))?;
        let length = usize::from(*sector.first().unwrap_or(&0));
        Ok(sector
            .get(..length)
            .and_then(|bytes| Record::parse(bytes, self.block)))
    }

    /// What a record stands for; nothing for what has no place in a folder
    /// of the user.
    fn item(&mut self, record: &Record) -> Result<Option<Item>> {
        let ridge = match self.ridge {
            Some(skip) => {
                let area = record.system_use.get(skip..).unwrap_or_default();
                rock_ridge(self.disc, self.block, area)?
            }
            None => Ridge::default(),
        };
        if ridge.relocated {
            return Ok(None);
        }
        let mut inside = (record.flags & FLAG_FOLDER != 0).then_some((record.offset, record.size));
        if let Some(child) = ridge.child {
            // The folder is kept elsewhere; its own first record tells how
            // long it is.
            let offset = child.checked_mul(self.block).ok_or_else(cut_short)?;
            let size = self.first(offset)?.map_or(0, |first| first.size);
            inside = Some((offset, size));
        }
        let kind = match ridge.mode.map(|mode| mode & KIND_MASK) {
            _ if inside.is_some() => EntryKind::Directory,
            Some(KIND_LINK) => EntryKind::Symlink,
            Some(KIND_FILE | KIND_FOLDER | 0) | None => EntryKind::File,
            // Devices, pipes and sockets.
            Some(_) => return Ok(None),
        };
        let name = if !ridge.name.is_empty() {
            Name::Bytes(ridge.name)
        } else if self.joliet {
            Name::Text(joliet_name(&record.name))
        } else {
            Name::Bytes(plain_name(&record.name).to_vec())
        };
        let is_file = kind == EntryKind::File;
        let extents = if is_file && record.size > 0 {
            vec![Extent {
                offset: record.offset,
                length: record.size,
            }]
        } else {
            Vec::new()
        };
        Ok(Some(Item {
            node: Node {
                parent: None,
                name,
                kind,
                target: match kind {
                    EntryKind::Symlink => Some(Name::Bytes(ridge.target.unwrap_or_default())),
                    _ => None,
                },
                size: if is_file { record.size } else { 0 },
                modified: ridge.modified.or(record.modified),
                accessed: ridge.accessed,
                created: ridge.created,
                mode: ridge.mode.map(|mode| mode & 0o7777),
                extents,
            },
            inside,
        }))
    }

    /// The files and the folders of a folder.
    fn folder(&mut self, offset: u64, size: u64) -> Result<Vec<Item>> {
        let end = offset
            .checked_add(size)
            .filter(|end| *end <= self.disc.size)
            .ok_or_else(cut_short)?;
        let mut items = Vec::new();
        // A file too large for one record, its pieces gathered so far.
        let mut open: Option<Item> = None;
        let mut at = offset;
        while at < end {
            self.disc.ctx.checkpoint()?;
            let sector = self.disc.read(at, SECTOR.min(end - at))?;
            at += sector.len() as u64;
            let mut within = 0;
            // A record does not cross a sector: zeros fill what is left.
            while let Some(&length) = sector.get(within).filter(|length| **length != 0) {
                let record = sector
                    .get(within..within + usize::from(length))
                    .and_then(|bytes| Record::parse(bytes, self.block))
                    .ok_or_else(|| damaged("a record of a folder that makes no sense"))?;
                within += usize::from(length);
                if record.is_dot() || record.flags & FLAG_ASSOCIATED != 0 {
                    continue;
                }
                let more = record.flags & FLAG_MORE != 0;
                if let Some(mut item) = open.take() {
                    item.node.size += record.size;
                    item.node.extents.push(Extent {
                        offset: record.offset,
                        length: record.size,
                    });
                    if more {
                        open = Some(item);
                    } else {
                        items.push(item);
                    }
                    continue;
                }
                let Some(item) = self.item(&record)? else {
                    continue;
                };
                if more && item.node.kind == EntryKind::File {
                    open = Some(item);
                } else {
                    items.push(item);
                }
            }
        }
        items.extend(open);
        Ok(items)
    }

    fn walk(&mut self, root: &Record) -> Result<Vec<Node>> {
        let mut nodes = Vec::new();
        // A folder is gone into once, whatever the records say of where
        // folders are.
        let mut seen = HashSet::from([root.offset]);
        let mut waiting = vec![(root.offset, root.size, None)];
        while let Some((offset, size, parent)) = waiting.pop() {
            let mut inner = Vec::new();
            for mut item in self.folder(offset, size)? {
                if let Some((offset, size)) = item.inside {
                    if seen.insert(offset) {
                        inner.push((offset, size, Some(nodes.len())));
                    }
                }
                item.node.parent = parent;
                nodes.push(item.node);
            }
            waiting.extend(inner.into_iter().rev());
        }
        Ok(nodes)
    }
}

/// Whether the disc has Rock Ridge, and how many bytes come before its
/// fields in every record: the first record of the root says both.
fn ridge_of(disc: &mut Disc, volume: &Volume) -> Result<Option<usize>> {
    let mut walker = Walker {
        disc,
        block: volume.block,
        joliet: false,
        ridge: None,
    };
    let Some(first) = walker.first(volume.root.offset)? else {
        return Ok(None);
    };
    // The mark of the protocol the fields are written by; the discs of one
    // family of drives have fourteen bytes of their own before it.
    for start in [0, 14] {
        if let Some([b'S', b'P', 7, 1, 0xBE, 0xEF, skip]) = first.system_use.get(start..start + 7) {
            let ridge = rock_ridge(walker.disc, volume.block, &first.system_use[start..])?;
            return Ok(ridge.marked.then_some(usize::from(*skip)));
        }
    }
    Ok(None)
}

/// Puts the paths together and reads the names as text.
fn finish(nodes: Vec<Node>, options: &OpenOptions, ctx: &OpContext) -> Result<Vec<Located>> {
    let names = NameDecoder::new(
        options.encoding,
        nodes.iter().filter_map(|node| match &node.name {
            Name::Bytes(bytes) => Some(bytes.as_slice()),
            Name::Text(_) => None,
        }),
        options.hint.as_deref(),
    );
    let text = |name: Name| match name {
        Name::Bytes(bytes) => names.decode(&bytes, false).into_owned(),
        Name::Text(text) => text,
    };
    let mut located: Vec<Located> = Vec::with_capacity(nodes.len());
    // A path repeats the names of all the folders above it: folders put one
    // into another make paths that take far more than the disc itself.
    let mut spent = 0u64;
    for (index, node) in nodes.into_iter().enumerate() {
        let name = text(node.name);
        let path = match node.parent.and_then(|parent| located.get(parent)) {
            Some(parent) => format!("{}/{name}", parent.entry.meta.path),
            None => name,
        };
        spent += path.len() as u64;
        if spent > ctx.budget.memory {
            return Err(Error::new("notEnoughMemory"));
        }
        located.push(Located {
            entry: Entry {
                index: index as u64,
                meta: EntryMeta {
                    path,
                    kind: node.kind,
                    link_target: node.target.map(text),
                    size: Some(node.size),
                    modified: node.modified,
                    created: node.created,
                    accessed: node.accessed,
                    unix_mode: node.mode,
                    windows_attributes: None,
                },
                ..Entry::default()
            },
            extents: node.extents,
        });
    }
    Ok(located)
}

fn list(source: &Source, options: &OpenOptions, ctx: &OpContext) -> Result<Vec<Located>> {
    let mut disc = Disc {
        reader: source.reader()?,
        size: source.size(),
        ctx,
    };
    let found = descriptors(&mut disc)?;
    // A disc of UDF alone need not say so among its descriptors.
    if found.udf || found.primary.is_none() {
        match udf::list(&mut disc) {
            Ok(Some(nodes)) => return finish(nodes, options, ctx),
            Err(error) if found.primary.is_none() || error.is_cancelled() => return Err(error),
            // The older file system of the disc may still be read.
            Ok(None) | Err(_) => {}
        }
    }
    let Some(primary) = found.primary else {
        return Err(damaged("no file system of a disc was found"));
    };
    let ridge = ridge_of(&mut disc, &primary)?;
    let volume = match (ridge, found.joliet) {
        (None, Some(joliet)) => joliet,
        _ => primary,
    };
    let mut walker = Walker {
        disc: &mut disc,
        block: volume.block,
        joliet: volume.joliet,
        ridge,
    };
    finish(walker.walk(&volume.root)?, options, ctx)
}

/// The image of a disc opened for reading.
pub fn reader(source: Source, options: OpenOptions) -> StoredReader {
    StoredReader::new(source, options, list)
}

#[cfg(test)]
pub(crate) mod tests {
    use boxit_core::{ArchiveReader, Budget, Selection};

    use super::*;
    use crate::names::NameEncoding;
    use crate::tests::{gunzip, pattern, Collector};

    /// What an image holds: the path, whether it is a folder, the content.
    type Listing = Vec<(String, EntryKind, Vec<u8>)>;

    fn read_with(bytes: Vec<u8>, options: OpenOptions) -> Result<Vec<(Entry, Vec<u8>)>> {
        let ctx = OpContext::silent();
        let mut reader = reader(Source::memory("disc.iso", bytes), options);
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

    fn read(bytes: Vec<u8>) -> Result<Vec<(Entry, Vec<u8>)>> {
        read_with(bytes, OpenOptions::default())
    }

    fn listing(got: &[(Entry, Vec<u8>)]) -> Listing {
        let mut listing: Listing = got
            .iter()
            .map(|(entry, data)| (entry.meta.path.clone(), entry.meta.kind, data.clone()))
            .collect();
        listing.sort_by(|one, other| one.0.cmp(&other.0));
        listing
    }

    const LONG: &str =
        "a name that is longer than the sixty-four characters a name of Joliet may have.txt";

    /// The tree the image writer of Windows was given for the fixtures.
    fn planted() -> Listing {
        let file = |path: &str, data: &[u8]| (path.to_owned(), EntryKind::File, data.to_vec());
        let folder = |path: &str| (path.to_owned(), EntryKind::Directory, Vec::new());
        let mut listing = vec![
            file("big.bin", &pattern(70_000)),
            folder("deep"),
            folder("deep/a"),
            folder("deep/a/b"),
            file("deep/a/b/leaf.txt", b"leaf"),
            folder("docs"),
            file(&format!("docs/{LONG}"), b"long"),
            file("docs/привет мир.txt", "привет".as_bytes()),
            folder("empty"),
            file("empty.txt", b""),
            file("readme.txt", b"read me\n"),
        ];
        listing.sort_by(|one, other| one.0.cmp(&other.0));
        listing
    }

    #[test]
    fn the_images_windows_writes_are_read() {
        let fixtures: [(&str, &[u8]); 7] = [
            ("joliet", include_bytes!("../fixtures/joliet.iso.gz")),
            ("bridge", include_bytes!("../fixtures/bridge.iso.gz")),
            ("udf-102", include_bytes!("../fixtures/udf-102.iso.gz")),
            ("udf-150", include_bytes!("../fixtures/udf-150.iso.gz")),
            ("udf-200", include_bytes!("../fixtures/udf-200.iso.gz")),
            ("udf-201", include_bytes!("../fixtures/udf-201.iso.gz")),
            ("udf-250", include_bytes!("../fixtures/udf-250.iso.gz")),
        ];
        for (name, packed) in fixtures {
            let got = read(gunzip(packed)).unwrap_or_else(|error| panic!("{name}: {error:?}"));
            let (got, wanted) = (listing(&got), planted());
            let names = |listing: &Listing| -> Vec<String> {
                listing.iter().map(|(path, _, _)| path.clone()).collect()
            };
            assert_eq!(names(&got), names(&wanted), "{name}");
            assert!(got == wanted, "{name}: kinds and contents");
        }
    }

    #[test]
    fn more_paths_than_the_memory_holds_are_refused() {
        // The paths of the fixtures take a few hundred bytes.
        let tight = OpContext {
            budget: Budget {
                threads: 1,
                memory: 100,
            },
            ..OpContext::silent()
        };
        let fixtures: [(&str, &[u8]); 2] = [
            ("bridge", include_bytes!("../fixtures/bridge.iso.gz")),
            ("udf-250", include_bytes!("../fixtures/udf-250.iso.gz")),
        ];
        for (name, packed) in fixtures {
            let source = Source::memory("disc.iso", gunzip(packed));
            let mut reader = reader(source, OpenOptions::default());
            let error = reader.entries(&tight).unwrap_err();
            assert_eq!(error.code, "notEnoughMemory", "{name}");
        }
    }

    #[test]
    fn the_times_of_the_images_are_read() {
        let moment = Some(Timestamp::from_seconds(1_700_000_000));
        let fixtures: [(&str, &[u8]); 3] = [
            ("joliet", include_bytes!("../fixtures/joliet.iso.gz")),
            ("udf-102", include_bytes!("../fixtures/udf-102.iso.gz")),
            ("udf-250", include_bytes!("../fixtures/udf-250.iso.gz")),
        ];
        for (name, packed) in fixtures {
            let got = read(gunzip(packed)).unwrap();
            let (entry, _) = got
                .iter()
                .find(|(entry, _)| entry.meta.path == "readme.txt")
                .unwrap();
            assert_eq!(entry.meta.modified, moment, "{name}");
            assert_eq!(entry.meta.size, Some(8), "{name}");
        }
    }

    // What follows builds images by hand: every sector is where the test
    // puts it.

    fn both(value: u32) -> Vec<u8> {
        [value.to_le_bytes(), value.to_be_bytes()].concat()
    }

    /// A record of a folder. The time is 2023-11-14 22:13:20, an hour east
    /// of Greenwich.
    fn record(name: &[u8], sector: u32, size: u32, flags: u8, system_use: &[u8]) -> Vec<u8> {
        let mut bytes = vec![0u8, 0];
        bytes.extend(both(sector));
        bytes.extend(both(size));
        bytes.extend([123, 11, 14, 23, 13, 20, 4]);
        bytes.extend([flags, 0, 0, 1, 0, 0, 1]);
        bytes.push(name.len() as u8);
        bytes.extend(name);
        if name.len().is_multiple_of(2) {
            bytes.push(0);
        }
        bytes.extend(system_use);
        if bytes.len() % 2 == 1 {
            bytes.push(0);
        }
        bytes[0] = bytes.len() as u8;
        bytes
    }

    /// The first two records of a folder at `sector`, and whatever else.
    fn folder(sector: u32, own: &[u8], records: &[Vec<u8>]) -> Vec<u8> {
        let mut bytes = record(&[0], sector, SECTOR as u32, FLAG_FOLDER, own);
        bytes.extend(record(&[1], sector, SECTOR as u32, FLAG_FOLDER, &[]));
        for record in records {
            bytes.extend(record);
        }
        assert!(bytes.len() <= SECTOR as usize);
        bytes.resize(SECTOR as usize, 0);
        bytes
    }

    /// A descriptor of a volume whose root folder is at `root`.
    fn descriptor(kind: u8, root: u32, joliet: bool) -> Vec<u8> {
        let mut bytes = vec![0u8; SECTOR as usize];
        bytes[0] = kind;
        bytes[1..6].copy_from_slice(MARK);
        bytes[6] = 1;
        if joliet {
            bytes[88..91].copy_from_slice(b"%/E");
        }
        bytes[128..132].copy_from_slice(&[0, 8, 8, 0]);
        let record = record(&[0], root, SECTOR as u32, FLAG_FOLDER, &[]);
        bytes[ROOT_RECORD..ROOT_RECORD + RECORD].copy_from_slice(&record);
        bytes
    }

    /// An image with these sectors, each at its number.
    fn image(sectors: &[(u32, Vec<u8>)]) -> Vec<u8> {
        let last = sectors.iter().map(|(number, _)| *number).max().unwrap_or(0);
        let mut bytes = vec![0u8; (last as usize + 1) * SECTOR as usize];
        for (number, content) in sectors {
            let at = *number as usize * SECTOR as usize;
            bytes[at..at + content.len()].copy_from_slice(content);
        }
        bytes
    }

    /// A field of Rock Ridge.
    fn field(mark: &[u8], data: &[u8]) -> Vec<u8> {
        let mut bytes = mark.to_vec();
        bytes.extend([4 + data.len() as u8, 1]);
        bytes.extend(data);
        bytes
    }

    fn mode(mode: u32) -> Vec<u8> {
        field(b"PX", &[both(mode), both(1), both(0), both(0)].concat())
    }

    fn named(name: &[u8]) -> Vec<u8> {
        field(b"NM", &[&[0], name].concat())
    }

    #[test]
    fn plain_names_lose_their_versions_and_long_files_are_put_together() {
        let bytes = image(&[
            (16, descriptor(PRIMARY, 18, false)),
            (17, descriptor(255, 18, false)),
            (
                18,
                folder(
                    18,
                    &[],
                    &[
                        record(b"FILE.TXT;1", 20, 5, 0, &[]),
                        record(b"NOEXT.;1", 20, 0, 0, &[]),
                        record(b"SUB", 19, SECTOR as u32, FLAG_FOLDER, &[]),
                        // The other fork of a file is left out.
                        record(b"FILE.TXT;1", 21, 4, FLAG_ASSOCIATED, &[]),
                    ],
                ),
            ),
            (
                19,
                folder(
                    19,
                    &[],
                    &[
                        record(b"BIG.BIN;1", 22, 2048, FLAG_MORE, &[]),
                        record(b"BIG.BIN;1", 21, 4, FLAG_MORE, &[]),
                        record(b"BIG.BIN;1", 23, 3, 0, &[]),
                        record(b"V;2;17", 20, 2, 0, &[]),
                    ],
                ),
            ),
            (20, b"hello".to_vec()),
            (21, b"fork".to_vec()),
            (22, vec![b'x'; 2048]),
            (23, b"end".to_vec()),
        ]);
        let got = read(bytes).unwrap();
        let big = [vec![b'x'; 2048], b"fork".to_vec(), b"end".to_vec()].concat();
        assert_eq!(
            listing(&got),
            [
                (String::from("FILE.TXT"), EntryKind::File, b"hello".to_vec()),
                (String::from("NOEXT"), EntryKind::File, Vec::new()),
                (String::from("SUB"), EntryKind::Directory, Vec::new()),
                (String::from("SUB/BIG.BIN"), EntryKind::File, big),
                (String::from("SUB/V;2"), EntryKind::File, b"he".to_vec()),
            ]
        );
        let file = &got[0].0;
        assert_eq!(file.meta.size, Some(5));
        assert_eq!(
            file.meta.modified,
            Some(Timestamp::from_seconds(1_700_000_000))
        );
        assert_eq!(file.meta.unix_mode, None);
        // The folders come before what is in them.
        assert_eq!(got[2].0.meta.path, "SUB");
        assert_eq!(got[3].0.meta.size, Some(2055));
    }

    /// The fields that say a disc has Rock Ridge.
    fn ridge_marks() -> Vec<u8> {
        let mut bytes = field(b"SP", &[0xBE, 0xEF, 0]);
        bytes.extend(mode(0o040_755));
        bytes.extend(field(
            b"ER",
            &[
                10, 0, 0, 1, b'R', b'R', b'I', b'P', b'_', b'1', b'9', b'9', b'1', b'A',
            ],
        ));
        bytes
    }

    fn ridge_image(joliet: bool) -> Vec<u8> {
        // A name in two fields, the first of which says it goes on.
        let mut read_me = field(b"NM", &[&[1], &b"Read"[..]].concat());
        read_me.extend(named(b"Me.md"));
        read_me.extend(mode(0o100_640));
        // Modified and accessed, in the long form: 2023-11-14 22:13:20 and
        // a second later, in Greenwich.
        read_me.extend(field(
            b"TF",
            &[
                &[0x80 | 0x06],
                &b"2023111422132000\0"[..],
                &b"2023111422132100\0"[..],
            ]
            .concat(),
        ));
        // "../docs/ab": the folder above, a name, a name in two parts.
        let mut link = named(b"link");
        link.extend(mode(0o120_777));
        link.extend(field(
            b"SL",
            &[
                0, 4, 0, 0, 4, b'd', b'o', b'c', b's', 1, 1, b'a', 0, 1, b'b',
            ],
        ));
        let mut docs = named(b"docs");
        docs.extend(mode(0o040_755));
        let mut device = named(b"tty");
        device.extend(mode(0o020_644));
        // The name is kept somewhere else on the disc.
        let long = "a name too long for the record it belongs to".repeat(3);
        let elsewhere = named(long.as_bytes());
        let mut far = mode(0o100_644);
        far.extend(field(
            b"CE",
            &[both(25), both(8), both(elsewhere.len() as u32)].concat(),
        ));
        // A folder too deep for ISO 9660 is kept in a folder of the root
        // and shows where it belongs.
        let mut stand_in = named(b"deep");
        stand_in.extend(mode(0o040_755));
        stand_in.extend(field(b"CL", &both(22)));
        let mut moved = named(b"deep");
        moved.extend(mode(0o040_755));
        moved.extend(field(b"RE", &[]));
        let mut absolute = named(b"root");
        absolute.extend(mode(0o120_777));
        absolute.extend(field(b"SL", &[0, 8, 0, 0, 3, b'e', b't', b'c']));

        let mut sectors = vec![
            (16, descriptor(PRIMARY, 18, false)),
            (
                18,
                folder(
                    18,
                    &ridge_marks(),
                    &[
                        record(b"README.MD;1", 24, 7, 0, &read_me),
                        record(b"LINK.;1", 24, 0, 0, &link),
                        record(b"DOCS", 19, SECTOR as u32, FLAG_FOLDER, &docs),
                        record(b"TTY.;1", 24, 0, 0, &device),
                        record(b"LONG.;1", 24, 4, 0, &far),
                        record(b"RR_MOVED", 21, SECTOR as u32, FLAG_FOLDER, &[]),
                        record(b"ROOT.;1", 24, 0, 0, &absolute),
                    ],
                ),
            ),
            (
                19,
                folder(19, &[], &[record(b"DEEP.;1", 24, 0, 0, &stand_in)]),
            ),
            (
                21,
                folder(
                    21,
                    &[],
                    &[record(b"DEEP", 22, SECTOR as u32, FLAG_FOLDER, &moved)],
                ),
            ),
            (
                22,
                folder(
                    22,
                    &[],
                    &[record(b"LEAF.TXT;1", 24, 4, 0, &named(b"leaf.txt"))],
                ),
            ),
            (24, b"read me".to_vec()),
            (25, [&[0u8; 8][..], &elsewhere].concat()),
        ];
        if joliet {
            let name: Vec<u8> = "joliet.txt"
                .encode_utf16()
                .flat_map(u16::to_be_bytes)
                .collect();
            sectors.push((17, descriptor(SUPPLEMENTARY, 26, true)));
            sectors.push((26, folder(26, &[], &[record(&name, 24, 7, 0, &[])])));
        }
        image(&sectors)
    }

    #[test]
    fn rock_ridge_gives_names_links_and_permissions() {
        for joliet in [false, true] {
            let got = read(ridge_image(joliet)).unwrap();
            let long = "a name too long for the record it belongs to".repeat(3);
            let paths: Vec<&str> = got
                .iter()
                .map(|(entry, _)| entry.meta.path.as_str())
                .collect();
            assert_eq!(
                paths,
                [
                    "ReadMe.md",
                    "link",
                    "docs",
                    long.as_str(),
                    "RR_MOVED",
                    "root",
                    "docs/deep",
                    "docs/deep/leaf.txt"
                ]
            );
            let (read_me, data) = &got[0];
            assert_eq!(data, b"read me");
            assert_eq!(read_me.meta.unix_mode, Some(0o640));
            assert_eq!(
                read_me.meta.modified,
                Some(Timestamp::from_seconds(1_700_000_000))
            );
            assert_eq!(
                read_me.meta.accessed,
                Some(Timestamp::from_seconds(1_700_000_001))
            );
            let link = &got[1].0.meta;
            assert_eq!(link.kind, EntryKind::Symlink);
            assert_eq!(link.link_target.as_deref(), Some("../docs/ab"));
            assert_eq!(link.size, Some(0));
            assert_eq!(got[5].0.meta.link_target.as_deref(), Some("/etc"));
            assert_eq!(got[2].0.meta.kind, EntryKind::Directory);
            assert_eq!(got[2].0.meta.unix_mode, Some(0o755));
            assert_eq!(got[3].1, b"read");
            assert_eq!(got[6].0.meta.kind, EntryKind::Directory);
            assert_eq!(got[7].1, b"read");
        }
    }

    #[test]
    fn joliet_gives_names_in_unicode_where_there_is_no_rock_ridge() {
        let name =
            |text: &str| -> Vec<u8> { text.encode_utf16().flat_map(u16::to_be_bytes).collect() };
        let bytes = image(&[
            (16, descriptor(PRIMARY, 18, false)),
            (17, descriptor(SUPPLEMENTARY, 19, true)),
            (
                18,
                folder(18, &[], &[record(b"PRIVET.TXT;1", 20, 6, 0, &[])]),
            ),
            (
                19,
                folder(
                    19,
                    &[],
                    &[
                        record(&name("Привет, мир.txt;1"), 20, 6, 0, &[]),
                        record(&name("Папка"), 21, SECTOR as u32, FLAG_FOLDER, &[]),
                    ],
                ),
            ),
            (20, b"hello!".to_vec()),
            (
                21,
                folder(21, &[], &[record(&name("𝄞.txt"), 20, 2, 0, &[])]),
            ),
        ]);
        let got = read(bytes.clone()).unwrap();
        assert_eq!(
            listing(&got),
            [
                (String::from("Папка"), EntryKind::Directory, Vec::new()),
                (String::from("Папка/𝄞.txt"), EntryKind::File, b"he".to_vec()),
                (
                    String::from("Привет, мир.txt"),
                    EntryKind::File,
                    b"hello!".to_vec()
                ),
            ]
        );
        // Without the second tree the names are those of the first, read
        // in the code page the user says.
        let mut plain = bytes;
        let at = 17 * SECTOR as usize;
        plain[at] = 255;
        let cyrillic = [0x8F, 0x90, 0x88, 0x82, 0x85, 0x92, b'.', b'T', b'X', b'T'];
        let name_at = 18 * SECTOR as usize + 2 * RECORD + 33;
        plain[name_at..name_at + 10].copy_from_slice(&cyrillic);
        plain[name_at + 10..name_at + 12].copy_from_slice(b";1");
        let options = OpenOptions {
            encoding: NameEncoding::from_label("ibm866").unwrap(),
            ..OpenOptions::default()
        };
        let got = read_with(plain, options).unwrap();
        assert_eq!(got[0].0.meta.path, "ПРИВЕТ.TXT");
    }

    #[test]
    fn what_is_broken_is_said_to_be_broken() {
        let whole = ridge_image(false);
        let code = |bytes: Vec<u8>| read(bytes).unwrap_err().code;
        // No descriptor at all, and nothing at all.
        assert_eq!(code(vec![0u8; 40 * SECTOR as usize]), "damaged");
        assert_eq!(code(Vec::new()), "damaged");
        // The root folder is past the end of the image.
        assert_eq!(code(whole[..18 * SECTOR as usize].to_vec()), "truncated");
        // A folder is, and a file: the list is read, the file is not.
        let plain = |records: &[Vec<u8>]| {
            image(&[
                (16, descriptor(PRIMARY, 18, false)),
                (18, folder(18, &[], records)),
            ])
        };
        let lost = plain(&[record(b"SUB", 100, SECTOR as u32, FLAG_FOLDER, &[])]);
        assert_eq!(code(lost), "truncated");
        let error = read(plain(&[record(b"FILE.TXT;1", 100, 7, 0, &[])])).unwrap_err();
        assert_eq!(error.code, "damagedEntry");
        assert_eq!(error.params["name"], "FILE.TXT");
        // A record that runs over the end of its sector: the sector is
        // full of records, and the last one says it is longer than it is.
        let root = 18 * SECTOR as usize;
        let mut crossing = plain(&vec![record(b"A", 18, 1, 0, &[]); 58]);
        assert_eq!(read(crossing.clone()).unwrap().len(), 58);
        crossing[root + 59 * RECORD] = 60;
        assert_eq!(code(crossing), "damaged");
        // A record too short to be one.
        let mut short = plain(&[record(b"A", 18, 1, 0, &[])]);
        short[root + 2 * RECORD] = 20;
        assert_eq!(code(short), "damaged");
        // The fields of Rock Ridge go on in themselves, for ever.
        let mut endless = whole.clone();
        let far = 25 * SECTOR as usize + 8;
        let again = field(b"CE", &[both(25), both(8), both(28)].concat());
        endless[far..far + again.len()].copy_from_slice(&again);
        assert_eq!(code(endless), "damaged");
        // A folder inside itself is listed once.
        let inside = plain(&[
            record(b"SELF", 18, SECTOR as u32, FLAG_FOLDER, &[]),
            record(b"FILE.TXT;1", 16, 1, 0, &[]),
        ]);
        let got = read(inside).unwrap();
        let paths: Vec<&str> = got
            .iter()
            .map(|(entry, _)| entry.meta.path.as_str())
            .collect();
        assert_eq!(paths, ["SELF", "FILE.TXT"]);
    }

    #[test]
    fn a_damaged_udf_gives_way_to_what_else_the_disc_has() {
        let bridge = gunzip(include_bytes!("../fixtures/bridge.iso.gz"));
        let anchor = (256 * SECTOR) as usize;
        // The descriptors of the volume cannot be read: the older file
        // system of the disc is, with its shorter names.
        let mut lost = bridge.clone();
        let first = le32(&lost, anchor + 20).unwrap() as usize * SECTOR as usize;
        lost[first..first + 8 * SECTOR as usize].fill(0);
        let got = read(lost).unwrap();
        assert!(got.iter().any(|(entry, _)| entry.meta.path == "readme.txt"));
        assert_eq!(got.len(), planted().len());
        // A disc with nothing else has nothing to give.
        let mut alone = gunzip(include_bytes!("../fixtures/udf-201.iso.gz"));
        let first = le32(&alone, anchor + 20).unwrap() as usize * SECTOR as usize;
        alone[first..first + 8 * SECTOR as usize].fill(0);
        assert_eq!(read(alone).unwrap_err().code, "damaged");
        // Without its anchor it is no disc at all.
        let mut adrift = gunzip(include_bytes!("../fixtures/udf-201.iso.gz"));
        adrift[anchor..anchor + 16].fill(0);
        assert_eq!(read(adrift).unwrap_err().code, "damaged");
        // The image ends early: the list is read as far as it lies, the
        // files past the end are not.
        let whole = gunzip(include_bytes!("../fixtures/udf-250.iso.gz"));
        for cut in [whole.len() / 2, 300 * SECTOR as usize] {
            let error = read(whole[..cut].to_vec()).unwrap_err();
            assert!(
                ["truncated", "damagedEntry", "damaged"].contains(&error.code.as_str()),
                "{error:?}"
            );
        }
    }
}

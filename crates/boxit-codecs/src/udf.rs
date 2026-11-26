//! UDF, the file system of DVDs, of Blu-ray discs and of the images the
//! newer systems write.
//!
//! Everything on such a disc is found by following descriptors, each with a
//! tag that says what it is: the anchor at sector 256 leads to the
//! descriptors of the volume, those to its partitions and to the root
//! folder, a folder to the entries of its files, an entry to where the
//! content of a file lies. The blocks are counted inside a partition, and
//! the newest revisions keep the folders in a partition of their own that
//! is itself a file of the first.

use std::collections::HashSet;
use std::io::{BufReader, Read};

use boxit_core::{EntryKind, Error, Result, Timestamp};
use chrono::{Datelike, NaiveDate, Timelike};

use crate::bytes::{le16, le32, le64};
use crate::iso::{Disc, Name, Node};
use crate::stored::{Extent, Pieces};
use crate::support::{damaged, from_local, Civil, MAX_LINK_TARGET};

/// The sector the anchor of a volume is at, in blocks of any size.
const ANCHOR: u64 = 256;
/// The sizes a block may have, the one of optical discs first.
const BLOCKS: [u64; 4] = [2048, 512, 4096, 1024];

const TAG_ANCHOR: u16 = 2;
const TAG_POINTER: u16 = 3;
const TAG_PARTITION: u16 = 5;
const TAG_LOGICAL_VOLUME: u16 = 6;
const TAG_TERMINATOR: u16 = 8;
const TAG_FILE_SET: u16 = 256;
const TAG_NAME: u16 = 257;
const TAG_MORE_EXTENTS: u16 = 258;
const TAG_FILE: u16 = 261;
const TAG_EXTENDED_FILE: u16 = 266;

/// The kinds of a file an entry may be of.
const KIND_FOLDER: u8 = 4;
const KIND_FILE: u8 = 5;
const KIND_LINK: u8 = 12;
/// A file recorded for playing back as it was recorded: a file all the
/// same.
const KIND_REAL_TIME: u8 = 249;

/// A name of a folder that is of a deleted file, or of the folder above.
const NAME_DELETED: u8 = 0x04;
const NAME_PARENT: u8 = 0x08;
/// The part of a record of a name that is always there.
const NAME_HEAD: usize = 38;

/// The most descriptors of a volume that are looked at, and how many times
/// a list may go on somewhere else.
const MAX_DESCRIPTORS: u64 = 256;
const MAX_HOPS: usize = 4096;
/// The longest table of partitions that is read.
const MAX_MAPS: u64 = 64 * 1024;

/// Whether these bytes start with the tag of a descriptor of this kind:
/// the kind, and the sum of the bytes of the tag.
fn tagged(bytes: &[u8], kind: u16) -> bool {
    let Some(tag) = bytes.first_chunk::<16>() else {
        return false;
    };
    let sum = tag
        .iter()
        .enumerate()
        .filter(|(at, _)| *at != 4)
        .fold(0u8, |sum, (_, byte)| sum.wrapping_add(*byte));
    le16(tag, 0) == Some(kind) && sum == tag[4]
}

/// A moment as UDF writes it: twelve bytes, with the zone in minutes.
fn time(bytes: &[u8], at: usize) -> Option<Timestamp> {
    let stamp = bytes.get(at..at + 12)?;
    let zone = le16(stamp, 0)?;
    let year = le16(stamp, 2)? as i16;
    if year == 0 && stamp[4] == 0 {
        return None;
    }
    let local = NaiveDate::from_ymd_opt(i32::from(year), u32::from(stamp[4]), u32::from(stamp[5]))?
        .and_hms_opt(
            u32::from(stamp[6]),
            u32::from(stamp[7]),
            u32::from(stamp[8]).min(59),
        )?;
    let nanos = u32::from(stamp[9]) * 10_000_000
        + u32::from(stamp[10]) * 100_000
        + u32::from(stamp[11]) * 1000;
    // Twelve bits with a sign; the lowest number of all says the zone is
    // not known, and the time is of the clock on the wall.
    let minutes = i64::from((zone << 4) as i16 >> 4);
    if zone >> 12 != 1 || minutes == -2047 {
        return from_local(Civil {
            year: local.year(),
            month: local.month(),
            day: local.day(),
            hour: local.hour(),
            minute: local.minute(),
            second: local.second(),
        });
    }
    Some(Timestamp {
        seconds: local.and_utc().timestamp() - minutes * 60,
        nanos: nanos.min(999_999_999),
    })
}

/// The text of UDF: a byte that says how wide its characters are, then the
/// characters.
fn text(bytes: &[u8]) -> String {
    match bytes.split_first() {
        Some((16 | 255, wide)) => {
            let units: Vec<u16> = wide
                .as_chunks::<2>()
                .0
                .iter()
                .map(|pair| u16::from_be_bytes(*pair))
                .collect();
            String::from_utf16_lossy(&units)
        }
        Some((_, narrow)) => narrow.iter().map(|byte| char::from(*byte)).collect(),
        None => String::new(),
    }
}

/// The part of a list of pieces that starts `skip` bytes in and is `length`
/// bytes long; nothing when the pieces are shorter.
fn part(pieces: &[Extent], mut skip: u64, mut length: u64) -> Option<Vec<Extent>> {
    let mut out = Vec::new();
    for piece in pieces {
        if length == 0 {
            break;
        }
        if skip >= piece.length {
            skip -= piece.length;
            continue;
        }
        let take = (piece.length - skip).min(length);
        out.push(if piece.is_hole() {
            Extent::hole(take)
        } else {
            Extent {
                offset: piece.offset + skip,
                length: take,
            }
        });
        skip = 0;
        length -= take;
    }
    (length == 0).then_some(out)
}

/// How the blocks of a partition are found in the image.
enum Map {
    /// One run of blocks, from this byte and this many bytes long.
    Plain { start: u64, length: u64 },
    /// The blocks are the content of a file.
    File(Vec<Extent>),
    /// A kind of partition that is not read.
    Unread(&'static str),
}

/// What an entry of a file says.
struct File {
    kind: u8,
    size: u64,
    modified: Option<Timestamp>,
    accessed: Option<Timestamp>,
    created: Option<Timestamp>,
    extents: Vec<Extent>,
}

/// A name in a folder and where the entry of its file is.
struct Named {
    name: String,
    partition: u16,
    block: u32,
}

struct Volume<'d, 'a> {
    disc: &'d mut Disc<'a>,
    block: u64,
    maps: Vec<Map>,
}

impl Volume<'_, '_> {
    /// Where `length` bytes from a block of a partition are in the image.
    fn pieces(&self, partition: u16, block: u32, length: u64) -> Result<Vec<Extent>> {
        let skip = u64::from(block) * self.block;
        let found = match self.maps.get(usize::from(partition)) {
            Some(Map::Plain {
                start,
                length: whole,
            }) => part(
                &[Extent {
                    offset: *start,
                    length: *whole,
                }],
                skip,
                length,
            ),
            Some(Map::File(pieces)) => part(pieces, skip, length),
            Some(Map::Unread(what)) => return Err(Error::new("unsupportedArchive").detail(what)),
            None => None,
        };
        found.ok_or_else(|| damaged("UDF: a block outside of its partition"))
    }

    /// Reads one block of a partition.
    fn read(&mut self, partition: u16, block: u32) -> Result<Vec<u8>> {
        let pieces = self.pieces(partition, block, self.block)?;
        let mut bytes = vec![0u8; self.block as usize];
        Pieces::new(&mut self.disc.reader, &pieces).read_exact(&mut bytes)?;
        Ok(bytes)
    }

    /// Where the content of a file is, from the list in its entry. `form`
    /// is the shape of an item of the list and `home` the partition of the
    /// entry, which the shortest shape leaves out.
    fn extents(&mut self, home: u16, form: u16, area: &[u8], size: u64) -> Result<Vec<Extent>> {
        let width = [8, 16, 20][usize::from(form)];
        let mut area = area.to_vec();
        let mut out = Vec::new();
        let mut left = size;
        for _ in 0..=MAX_HOPS {
            let mut next = None;
            for item in area.chunks_exact(width) {
                let word = le32(item, 0).unwrap_or(0);
                let length = u64::from(word & 0x3FFF_FFFF);
                let (block, partition) = match form {
                    0 => (le32(item, 4), Some(home)),
                    1 => (le32(item, 4), le16(item, 8)),
                    _ => (le32(item, 12), le16(item, 16)),
                };
                let (Some(block), Some(partition)) = (block, partition) else {
                    break;
                };
                if length == 0 {
                    break;
                }
                match word >> 30 {
                    // The list goes on in a block of its own.
                    3 => {
                        next = Some((partition, block));
                        break;
                    }
                    0 => {
                        let take = length.min(left);
                        out.extend(self.pieces(partition, block, take)?);
                        left -= take;
                    }
                    // Room that was set aside and never written, or not
                    // even set aside: zeros.
                    _ => {
                        let take = length.min(left);
                        out.push(Extent::hole(take));
                        left -= take;
                    }
                }
                if left == 0 {
                    return Ok(out);
                }
            }
            let Some((partition, block)) = next else {
                // A file that ends before its size is sparse to its end.
                if left > 0 {
                    out.push(Extent::hole(left));
                }
                return Ok(out);
            };
            let bytes = self.read(partition, block)?;
            if !tagged(&bytes, TAG_MORE_EXTENTS) {
                return Err(damaged("UDF: a list of extents goes on nowhere"));
            }
            let length = le32(&bytes, 20).unwrap_or(0) as usize;
            area = bytes
                .get(24..24 + length)
                .ok_or_else(|| damaged("UDF: a list of extents longer than its block"))?
                .to_vec();
        }
        Err(damaged("UDF: a list of extents never ends"))
    }

    /// Reads the entry of a file.
    fn file(&mut self, partition: u16, block: u32) -> Result<File> {
        let bytes = self.read(partition, block)?;
        let extended = if tagged(&bytes, TAG_EXTENDED_FILE) {
            true
        } else if tagged(&bytes, TAG_FILE) {
            false
        } else {
            return Err(damaged("UDF: an entry of a file is not one"));
        };
        // The older entry is shorter by the time of creation and a few
        // fields after it.
        let (times, lengths) = if extended { (80, 208) } else { (72, 168) };
        let field = |at: usize| le32(&bytes, at).map(|value| value as usize);
        let (Some(flags), Some(size), Some(attributes), Some(listed)) = (
            le16(&bytes, 34),
            le64(&bytes, 56),
            field(lengths),
            field(lengths + 4),
        ) else {
            return Err(damaged("UDF: an entry of a file is cut short"));
        };
        let start = lengths + 8 + attributes;
        let area = start
            .checked_add(listed)
            .and_then(|end| bytes.get(start..end))
            .ok_or_else(|| damaged("UDF: an entry of a file larger than its block"))?;
        let extents = match flags & 7 {
            // The content is in the entry itself.
            3 => {
                if size > listed as u64 {
                    return Err(damaged("UDF: a file larger than the entry that holds it"));
                }
                let place = self.pieces(partition, block, self.block)?;
                place
                    .first()
                    .filter(|piece| !piece.is_hole() && size > 0)
                    .map(|piece| Extent {
                        offset: piece.offset + start as u64,
                        length: size,
                    })
                    .into_iter()
                    .collect()
            }
            form @ 0..=2 => self.extents(partition, form, area, size)?,
            _ => return Err(damaged("UDF: a list of extents of an unknown shape")),
        };
        Ok(File {
            kind: bytes[27],
            size,
            accessed: time(&bytes, times),
            modified: time(&bytes, times + 12),
            created: extended.then(|| time(&bytes, times + 24)).flatten(),
            extents,
        })
    }

    /// The names in a folder, without the deleted ones and the folder
    /// above.
    fn folder(&mut self, folder: &File) -> Result<Vec<Named>> {
        let ctx = self.disc.ctx;
        let pieces = Pieces::new(&mut self.disc.reader, &folder.extents);
        let mut reader = BufReader::new(pieces).take(folder.size);
        let mut names = Vec::new();
        loop {
            ctx.checkpoint()?;
            let mut head = [0u8; NAME_HEAD];
            let mut got = 0;
            while got < head.len() {
                match reader.read(&mut head[got..])? {
                    0 => break,
                    count => got += count,
                }
            }
            if got == 0 {
                return Ok(names);
            }
            if got < head.len() || !tagged(&head, TAG_NAME) {
                return Err(damaged("UDF: a folder holds what is not a name"));
            }
            let (kind, length) = (head[18], usize::from(head[19]));
            let before = usize::from(le16(&head, 36).unwrap_or(0));
            // A record is padded to a multiple of four bytes.
            let whole = (NAME_HEAD + before + length).next_multiple_of(4);
            let mut rest = vec![0u8; whole - NAME_HEAD];
            reader.read_exact(&mut rest)?;
            if kind & (NAME_DELETED | NAME_PARENT) != 0 {
                continue;
            }
            if let (Some(block), Some(partition)) = (le32(&head, 24), le16(&head, 28)) {
                names.push(Named {
                    name: text(&rest[before..before + length]),
                    partition,
                    block,
                });
            }
        }
    }

    /// The target of a link: the parts of a path, each with its kind.
    fn link(&mut self, link: &File) -> Result<String> {
        if link.size > MAX_LINK_TARGET {
            return Err(damaged("the target of a link is too long"));
        }
        let mut bytes = vec![0u8; link.size as usize];
        Pieces::new(&mut self.disc.reader, &link.extents).read_exact(&mut bytes)?;
        let mut target = String::new();
        let mut parts = bytes.as_slice();
        while let [kind, length, _, _, rest @ ..] = parts {
            let Some(name) = rest.get(..usize::from(*length)) else {
                break;
            };
            if !target.is_empty() && !target.ends_with('/') {
                target.push('/');
            }
            match kind {
                1 | 2 => target = String::from("/"),
                3 => target.push_str(".."),
                4 => target.push('.'),
                _ => target.push_str(&text(name)),
            }
            parts = &rest[name.len()..];
        }
        Ok(target)
    }
}

/// Finds the anchor of the volume: the size of a block, and the anchor.
fn anchor(disc: &mut Disc) -> Result<Option<(u64, Vec<u8>)>> {
    for block in BLOCKS {
        let offset = ANCHOR * block;
        if offset + block > disc.size {
            continue;
        }
        let bytes = disc.read(offset, block)?;
        if tagged(&bytes, TAG_ANCHOR) {
            return Ok(Some((block, bytes)));
        }
    }
    Ok(None)
}

/// What the descriptors of the volume say.
#[derive(Default)]
struct Described {
    /// The partitions: the number, the first block and the count of them.
    partitions: Vec<(u16, u64, u64)>,
    /// The descriptor of the logical volume, with its table of partitions.
    logical: Option<Vec<u8>>,
}

fn describe(disc: &mut Disc, block: u64, anchor: &[u8]) -> Result<Described> {
    let mut described = Described::default();
    let (Some(length), Some(first)) = (le32(anchor, 16), le32(anchor, 20)) else {
        return Ok(described);
    };
    let mut at = u64::from(first);
    let mut end = at + u64::from(length) / block;
    for _ in 0..MAX_DESCRIPTORS {
        if at >= end {
            break;
        }
        disc.ctx.checkpoint()?;
        let bytes = disc.read(at * block, block)?;
        at += 1;
        if tagged(&bytes, TAG_PARTITION) {
            if let (Some(number), Some(start), Some(count)) =
                (le16(&bytes, 22), le32(&bytes, 188), le32(&bytes, 192))
            {
                // A descriptor written later stands for the one before.
                described
                    .partitions
                    .retain(|(known, _, _)| *known != number);
                described
                    .partitions
                    .push((number, u64::from(start), u64::from(count)));
            }
        } else if tagged(&bytes, TAG_LOGICAL_VOLUME) {
            // The table of partitions may be longer than the block.
            let table = u64::from(le32(&bytes, 264).unwrap_or(0));
            let whole = 440 + table;
            described.logical = Some(if whole > block && whole <= MAX_MAPS {
                disc.read((at - 1) * block, whole)?
            } else {
                bytes
            });
        } else if tagged(&bytes, TAG_POINTER) {
            // The descriptors go on somewhere else.
            if let (Some(length), Some(first)) = (le32(&bytes, 20), le32(&bytes, 24)) {
                at = u64::from(first);
                end = at + u64::from(length) / block;
            }
        } else if tagged(&bytes, TAG_TERMINATOR) {
            break;
        }
    }
    Ok(described)
}

/// The files of the UDF of a disc; nothing when the disc has none.
pub(crate) fn list(disc: &mut Disc) -> Result<Option<Vec<Node>>> {
    let Some((block, anchor)) = anchor(disc)? else {
        return Ok(None);
    };
    let described = describe(disc, block, &anchor)?;
    let Some(logical) = described.logical else {
        return Err(damaged("UDF: the volume is not described"));
    };
    if le32(&logical, 212).map(u64::from) != Some(block) {
        return Err(Error::new("unsupportedArchive").detail("UDF: blocks of two sizes"));
    }
    let plain = |number: u16| {
        described
            .partitions
            .iter()
            .find(|(known, _, _)| *known == number)
            .map(|(_, start, count)| Map::Plain {
                start: start * block,
                length: count * block,
            })
    };

    // The table of partitions: where each one a file may name really is.
    let count = le32(&logical, 268).unwrap_or(0);
    let table_end = 440 + le32(&logical, 264).unwrap_or(0) as usize;
    let mut table = logical
        .get(440..table_end.min(logical.len()))
        .unwrap_or_default();
    let mut maps = Vec::new();
    // The number of the plain partition each item of the table stands for.
    let mut numbers = Vec::new();
    // The partitions that are files: the place in the list, the partition
    // the file is in, and the blocks of its entry and of the copy of it.
    let mut files = Vec::new();
    for _ in 0..count {
        let [kind, length, ..] = *table else {
            break;
        };
        let Some(map) = table
            .get(..usize::from(length))
            .filter(|map| map.len() >= 6)
        else {
            break;
        };
        table = &table[map.len()..];
        let mut number = None;
        let unread = match kind {
            1 => {
                number = le16(map, 4);
                None
            }
            2 => {
                let (name, of) = (map.get(5..28).unwrap_or_default(), le16(map, 38));
                if name.starts_with(b"*UDF Sparable Partition") {
                    number = of;
                    None
                } else if name.starts_with(b"*UDF Metadata Partition") {
                    if let (Some(of), Some(entry), Some(copy)) = (of, le32(map, 40), le32(map, 44))
                    {
                        files.push((maps.len(), of, [entry, copy]));
                    }
                    Some("UDF: the partition of the folders is not found")
                } else if name.starts_with(b"*UDF Virtual Partition") {
                    Some("UDF: a disc written piece by piece")
                } else {
                    Some("UDF: a partition of an unknown kind")
                }
            }
            _ => Some("UDF: a partition of an unknown kind"),
        };
        let map = match (unread, number.and_then(plain)) {
            (Some(what), _) => Map::Unread(what),
            (None, Some(map)) => map,
            (None, None) => {
                number = None;
                Map::Unread("UDF: a partition that is not described")
            }
        };
        numbers.push(number);
        maps.push(map);
    }

    let mut volume = Volume { disc, block, maps };
    for (place, number, entries) in files {
        // The file is in the plain partition of the same number.
        let home = numbers.iter().position(|known| *known == Some(number));
        let Some(home) = home.and_then(|home| u16::try_from(home).ok()) else {
            continue;
        };
        let found = entries
            .into_iter()
            .find_map(|entry| volume.file(home, entry).ok());
        if let Some(file) = found {
            volume.maps[place] = Map::File(file.extents);
        }
    }

    // The set of files, and in it the root folder.
    let (Some(set_block), Some(set_partition)) = (le32(&logical, 252), le16(&logical, 256)) else {
        return Err(damaged("UDF: the volume is cut short"));
    };
    let set = volume.read(set_partition, set_block)?;
    if !tagged(&set, TAG_FILE_SET) {
        return Err(damaged("UDF: the set of files is not where it should be"));
    }
    let (Some(root_block), Some(root_partition)) = (le32(&set, 404), le16(&set, 408)) else {
        return Err(damaged("UDF: the set of files is cut short"));
    };
    let root = volume.file(root_partition, root_block)?;
    if root.kind != KIND_FOLDER {
        return Err(damaged("UDF: the root is not a folder"));
    }

    let mut nodes = Vec::new();
    // A folder is gone into once, whatever the names say of where folders
    // are.
    let mut seen = HashSet::from([(root_partition, root_block)]);
    let mut waiting = vec![(root, None)];
    while let Some((folder, parent)) = waiting.pop() {
        let mut inner = Vec::new();
        for named in volume.folder(&folder)? {
            volume.disc.ctx.checkpoint()?;
            let mut file = volume.file(named.partition, named.block)?;
            let mut target = None;
            let kind = match file.kind {
                KIND_FOLDER => {
                    if !seen.insert((named.partition, named.block)) {
                        continue;
                    }
                    EntryKind::Directory
                }
                KIND_LINK => {
                    target = Some(Name::Text(volume.link(&file)?));
                    EntryKind::Symlink
                }
                0 | KIND_FILE | KIND_REAL_TIME => EntryKind::File,
                // Devices, pipes, and the files the volume keeps for
                // itself.
                _ => continue,
            };
            let is_file = kind == EntryKind::File;
            nodes.push(Node {
                parent,
                name: Name::Text(named.name),
                kind,
                target,
                size: if is_file { file.size } else { 0 },
                modified: file.modified,
                accessed: file.accessed,
                created: file.created,
                mode: None,
                extents: if is_file {
                    std::mem::take(&mut file.extents)
                } else {
                    Vec::new()
                },
            });
            if kind == EntryKind::Directory {
                inner.push((file, Some(nodes.len() - 1)));
            }
        }
        waiting.extend(inner.into_iter().rev());
    }
    Ok(Some(nodes))
}

//! The entries of a ZIP found without its list.
//!
//! A ZIP keeps the list of its entries at its end, and an archive that was
//! cut short or damaged there opens in no program. Every entry also starts
//! with a header of its own that repeats what the list says of it; the file
//! is searched for those headers, and a list is written anew from them. The
//! new list is not written into the file: it is held in memory and read as
//! if it followed the last byte of the archive (see `Source::with_tail`).
//!
//! What the headers do not repeat is lost: the attributes of the system the
//! archive was made on, and the comments.

use std::io::{self, Read, Seek, SeekFrom};

use boxit_core::{OpContext, Result};
use memchr::memmem;

use crate::bytes::{le16, le32, le64};
use crate::source::{Source, SourceReader};

/// The two letters every record of a ZIP starts with, and the two bytes
/// after them that tell the records apart.
const MARK: &[u8] = b"PK";
const LOCAL: [u8; 2] = [3, 4];
const CENTRAL: [u8; 2] = [1, 2];
const END: [u8; 2] = [5, 6];
const DESCRIPTOR: [u8; 2] = [7, 8];
const END64: [u8; 2] = [6, 6];
const LOCATOR64: [u8; 2] = [6, 7];

/// Size of the fixed part of the header of an entry.
const LOCAL_FIXED: usize = 30;
/// The sizes of the entry are not in its header: they follow its data.
const FLAG_DESCRIPTOR: u16 = 0x0008;
/// The extra field with the sizes that do not fit into 32 bits.
const EXTRA_ZIP64: u16 = 0x0001;
/// The extra field with the times of Unix; the list keeps one of them.
const EXTRA_UNIX_TIME: u16 = 0x5455;
/// The extra field that tells how an entry is encrypted with AES.
const EXTRA_AES: u16 = 0x9901;
/// A value that does not fit into its field and is in the ZIP64 field.
const WIDE32: u32 = u32::MAX;
const WIDE16: u16 = u16::MAX;
/// The version of the format that has ZIP64, and the one before it.
const VERSION_ZIP64: u16 = 45;
const VERSION_PLAIN: u16 = 20;
/// `FILE_ATTRIBUTE_DIRECTORY` of Windows and DOS.
const ATTRIBUTE_DIRECTORY: u32 = 0x10;

/// The methods an entry may name. A header found in the middle of damaged
/// data that names another one is no header.
const METHODS: [u16; 17] = [0, 1, 2, 3, 4, 5, 6, 8, 9, 12, 14, 20, 93, 95, 96, 98, 99];

/// Size of the block the file is searched in.
const SCAN_BLOCK: usize = 1024 * 1024;
/// The most entries whose ends are looked for at once.
const WAITING: usize = 16;

/// An entry of a ZIP as its own header describes it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct Local {
    /// Where the header starts.
    pub offset: u64,
    version: u16,
    flags: u16,
    method: u16,
    time: u16,
    date: u16,
    crc: u32,
    packed: u64,
    size: u64,
    pub name: Vec<u8>,
    extra: Vec<u8>,
    /// Where the data starts, and where the entry ends: after its data and
    /// the sizes that may follow them.
    data: u64,
    end: u64,
    /// The header has the ZIP64 field: the sizes after the data are wide.
    wide: bool,
}

impl Local {
    /// The sizes of the entry are to be found after its data.
    fn open_ended(&self) -> bool {
        self.flags & FLAG_DESCRIPTOR != 0 && self.packed == 0
    }
}

/// What the headers of the entries tell of an archive.
#[derive(Debug, Default)]
pub(crate) struct Scanned {
    /// The entries that are whole, in the order of the file.
    pub found: Vec<Local>,
    /// The names of the entries that are cut short.
    pub lost: Vec<Vec<u8>>,
}

enum Parsed {
    Entry(Local),
    /// A header whose data goes past the end of the file.
    Cut(Vec<u8>),
    No,
}

/// Reads as much of the block as the file has.
fn fill(reader: &mut SourceReader, block: &mut [u8]) -> io::Result<usize> {
    let mut count = 0;
    while count < block.len() {
        match reader.read(&mut block[count..]) {
            Ok(0) => break,
            Ok(read) => count += read,
            Err(error) if error.kind() == io::ErrorKind::Interrupted => {}
            Err(error) => return Err(error),
        }
    }
    Ok(count)
}

/// The bytes at a place of the file, as many of the `length` as it has.
fn bytes_at(reader: &mut SourceReader, place: u64, length: usize) -> io::Result<Vec<u8>> {
    reader.seek(SeekFrom::Start(place))?;
    let mut bytes = vec![0u8; length];
    let count = fill(reader, &mut bytes)?;
    bytes.truncate(count);
    Ok(bytes)
}

/// The fields of the extra data of an entry: their kinds and contents. A
/// field that goes past the end of the data ends the walk.
fn fields(mut extra: &[u8]) -> impl Iterator<Item = (u16, &[u8])> {
    std::iter::from_fn(move || {
        let id = le16(extra, 0)?;
        let size = usize::from(le16(extra, 2)?);
        let body = extra.get(4..4 + size)?;
        extra = &extra[4 + size..];
        Some((id, body))
    })
}

/// Reads the header of an entry at `offset`, if what stands there is one.
fn parse(reader: &mut SourceReader, offset: u64, limit: u64) -> io::Result<Parsed> {
    let fixed = bytes_at(reader, offset, LOCAL_FIXED)?;
    let (
        Some(version),
        Some(flags),
        Some(method),
        Some(time),
        Some(date),
        Some(crc),
        Some(packed),
        Some(size),
        Some(name_length),
        Some(extra_length),
    ) = (
        le16(&fixed, 4),
        le16(&fixed, 6),
        le16(&fixed, 8),
        le16(&fixed, 10),
        le16(&fixed, 12),
        le32(&fixed, 14),
        le32(&fixed, 18),
        le32(&fixed, 22),
        le16(&fixed, 26),
        le16(&fixed, 28),
    )
    else {
        return Ok(Parsed::No);
    };
    let (name_length, extra_length) = (usize::from(name_length), usize::from(extra_length));
    if name_length == 0 || !METHODS.contains(&method) {
        return Ok(Parsed::No);
    }
    let data = offset + (LOCAL_FIXED + name_length + extra_length) as u64;
    if data > limit {
        return Ok(Parsed::No);
    }
    let mut name = bytes_at(
        reader,
        offset + LOCAL_FIXED as u64,
        name_length + extra_length,
    )?;
    if name.len() < name_length + extra_length {
        return Ok(Parsed::No);
    }
    let extra = name.split_off(name_length);
    // No name has the bytes that are no letters in any encoding.
    if name.iter().any(|byte| *byte < 0x20) {
        return Ok(Parsed::No);
    }
    let (mut packed, mut size) = (u64::from(packed), u64::from(size));
    let mut wide = false;
    if let Some((_, mut body)) = fields(&extra).find(|(id, _)| *id == EXTRA_ZIP64) {
        wide = true;
        // The field holds the values that did not fit, in this order.
        if size == u64::from(WIDE32) {
            size = le64(body, 0).unwrap_or(size);
            body = body.get(8..).unwrap_or_default();
        }
        if packed == u64::from(WIDE32) {
            packed = le64(body, 0).unwrap_or(packed);
        }
    }
    let Some(end) = data.checked_add(packed) else {
        return Ok(Parsed::No);
    };
    if end > limit {
        return Ok(Parsed::Cut(name));
    }
    Ok(Parsed::Entry(Local {
        offset,
        version,
        flags,
        method,
        time,
        date,
        crc,
        packed,
        size,
        name,
        extra,
        data,
        end,
        wide,
    }))
}

/// The entries whose ends are looked for, the outer ones first: an entry
/// stored without compression may hold a ZIP of its own.
struct Search<'a> {
    probe: &'a mut SourceReader,
    limit: u64,
    waiting: Vec<Local>,
    scanned: Scanned,
}

/// The sizes that close an entry that waits for them.
struct Closing {
    /// Which of the waiting entries.
    index: usize,
    crc: u32,
    packed: u64,
    size: u64,
    end: u64,
}

impl Search<'_> {
    /// The sizes of an entry that may stand at `place`, after their mark
    /// when `marked`: they are the sizes of the entry whose data ends
    /// right there, and say so themselves.
    fn sizes(&mut self, place: u64, marked: bool) -> io::Result<Option<Closing>> {
        if self.waiting.is_empty() {
            return Ok(None);
        }
        let start = place + if marked { 4 } else { 0 };
        let sizes = bytes_at(self.probe, start, 20)?;
        let Some(crc) = le32(&sizes, 0) else {
            return Ok(None);
        };
        let narrow = le32(&sizes, 4)
            .zip(le32(&sizes, 8))
            .map(|(packed, size)| (u64::from(packed), u64::from(size), start + 12));
        let wide = le64(&sizes, 4)
            .zip(le64(&sizes, 12))
            .map(|(packed, size)| (packed, size, start + 20));
        // The innermost entry first.
        for index in (0..self.waiting.len()).rev() {
            let local = &self.waiting[index];
            let Some(length) = place.checked_sub(local.data) else {
                continue;
            };
            let order = if local.wide {
                [wide, narrow]
            } else {
                [narrow, wide]
            };
            let fits = order
                .into_iter()
                .flatten()
                .find(|(packed, _, end)| *packed == length && *end <= self.limit);
            if let Some((packed, size, end)) = fits {
                return Ok(Some(Closing {
                    index,
                    crc,
                    packed,
                    size,
                    end,
                }));
            }
        }
        Ok(None)
    }

    /// An entry that waited has its sizes.
    fn close(&mut self, closing: &Closing) {
        let mut local = self.waiting.remove(closing.index);
        // What was found inside it is its data.
        self.waiting.truncate(closing.index);
        let before = self
            .scanned
            .found
            .partition_point(|found| found.offset < local.offset);
        self.scanned.found.truncate(before);
        local.crc = closing.crc;
        local.packed = closing.packed;
        local.size = closing.size;
        local.end = closing.end;
        self.scanned.found.push(local);
    }

    /// The mark of the sizes of an entry stands at `place`.
    fn settle(&mut self, place: u64) -> io::Result<()> {
        if let Some(closing) = self.sizes(place, true)? {
            self.close(&closing);
        }
        Ok(())
    }

    /// A record starts at `place`: sizes without their mark may stand right
    /// before it, as some programs write them.
    fn settle_bare(&mut self, place: u64) -> io::Result<()> {
        for length in [12, 20] {
            let Some(start) = place.checked_sub(length) else {
                continue;
            };
            if let Some(closing) = self.sizes(start, false)? {
                if closing.end == place {
                    self.close(&closing);
                    break;
                }
            }
        }
        Ok(())
    }
    /// An entry waits for its end.
    fn wait(&mut self, local: Local) {
        if self.waiting.len() == WAITING {
            self.scanned.lost.push(self.waiting.remove(0).name);
        }
        self.waiting.push(local);
    }
}

/// Searches the whole file for the headers of entries.
pub(crate) fn scan(source: &Source, ctx: &OpContext) -> Result<Scanned> {
    let limit = source.size();
    let mut reader = source.reader()?;
    let mut probe = source.reader()?;
    let mut search = Search {
        probe: &mut probe,
        limit,
        waiting: Vec::new(),
        scanned: Scanned::default(),
    };
    let finder = memmem::Finder::new(MARK);
    let mut block = vec![0u8; SCAN_BLOCK];
    let mut position = 0u64;
    'blocks: while position < limit {
        ctx.checkpoint()?;
        reader.seek(SeekFrom::Start(position))?;
        let count = fill(&mut reader, &mut block)?;
        let last = position + count as u64 >= limit;
        let mut at = 0usize;
        while let Some(hit) = finder.find(&block[at..count]) {
            let index = at + hit;
            if index + 4 > count {
                if last {
                    break 'blocks;
                }
                // The mark goes on in the next block.
                position += index as u64;
                continue 'blocks;
            }
            at = index + MARK.len();
            let place = position + index as u64;
            let kind = [block[index + 2], block[index + 3]];
            if kind == LOCAL {
                match parse(search.probe, place, limit)? {
                    Parsed::Entry(local) => {
                        search.settle_bare(place)?;
                        // The data of an entry is not searched, unless
                        // nothing tells where it ends.
                        let next = if local.open_ended() {
                            local.data
                        } else {
                            local.end
                        };
                        if local.open_ended() {
                            search.wait(local);
                        } else {
                            search.scanned.found.push(local);
                        }
                        match usize::try_from(next - position) {
                            Ok(within) if within < count => at = within,
                            _ => {
                                position = next;
                                continue 'blocks;
                            }
                        }
                    }
                    Parsed::Cut(name) => search.scanned.lost.push(name),
                    Parsed::No => {}
                }
            } else if kind == DESCRIPTOR {
                search.settle(place)?;
            } else if [CENTRAL, END, END64, LOCATOR64].contains(&kind) {
                search.settle_bare(place)?;
            }
        }
        if last {
            break;
        }
        // The last byte of the block may be the first of a mark.
        position += count.saturating_sub(1).max(1) as u64;
    }
    let Search {
        waiting,
        mut scanned,
        ..
    } = search;
    scanned
        .lost
        .extend(waiting.into_iter().map(|local| local.name));
    Ok(scanned)
}

fn put16(bytes: &mut Vec<u8>, value: u16) {
    bytes.extend_from_slice(&value.to_le_bytes());
}

fn put32(bytes: &mut Vec<u8>, value: u32) {
    bytes.extend_from_slice(&value.to_le_bytes());
}

fn put64(bytes: &mut Vec<u8>, value: u64) {
    bytes.extend_from_slice(&value.to_le_bytes());
}

/// A value for a field of 32 bits, or the mark that it is in the wide one.
fn narrow(value: u64) -> u32 {
    u32::try_from(value)
        .ok()
        .filter(|value| *value != WIDE32)
        .unwrap_or(WIDE32)
}

fn put_field(bytes: &mut Vec<u8>, id: u16, body: &[u8]) {
    put16(bytes, id);
    put16(bytes, body.len() as u16);
    bytes.extend_from_slice(body);
}

/// The extra data of an entry for the list: what its header has, with the
/// wide values of the list in place of the ones of the header.
fn list_extra(local: &Local) -> Vec<u8> {
    let mut wide = Vec::new();
    for value in [local.size, local.packed, local.offset] {
        if narrow(value) == WIDE32 {
            put64(&mut wide, value);
        }
    }
    let build = |all: bool| {
        let mut extra = Vec::new();
        if !wide.is_empty() {
            put_field(&mut extra, EXTRA_ZIP64, &wide);
        }
        for (id, body) in fields(&local.extra) {
            match id {
                EXTRA_ZIP64 => {}
                // The list keeps the time of the last change only.
                EXTRA_UNIX_TIME if all => {
                    let kept = if body.first().is_some_and(|flags| flags & 1 != 0) {
                        body.get(..5)
                    } else {
                        body.get(..1)
                    };
                    if let Some(kept) = kept {
                        put_field(&mut extra, id, kept);
                    }
                }
                EXTRA_AES => put_field(&mut extra, id, body),
                _ if all => put_field(&mut extra, id, body),
                _ => {}
            }
        }
        extra
    };
    let extra = build(true);
    if extra.len() > usize::from(WIDE16) {
        // Too much for a record of the list: what reading needs stays.
        return build(false);
    }
    extra
}

/// The list of an archive that holds the entries found: what would stand
/// after the last of them, were the archive whole. `start` is where the
/// list is put: the size of the file.
pub(crate) fn directory(found: &[Local], start: u64) -> Vec<u8> {
    let mut list = Vec::new();
    for local in found {
        let extra = list_extra(local);
        let wide = narrow(local.size) == WIDE32
            || narrow(local.packed) == WIDE32
            || narrow(local.offset) == WIDE32;
        let folder = local.name.last().is_some_and(|byte| b"/\\".contains(byte));
        list.extend_from_slice(MARK);
        list.extend_from_slice(&CENTRAL);
        put16(&mut list, if wide { VERSION_ZIP64 } else { VERSION_PLAIN });
        put16(&mut list, local.version);
        put16(&mut list, local.flags);
        put16(&mut list, local.method);
        put16(&mut list, local.time);
        put16(&mut list, local.date);
        put32(&mut list, local.crc);
        put32(&mut list, narrow(local.packed));
        put32(&mut list, narrow(local.size));
        put16(&mut list, local.name.len() as u16);
        put16(&mut list, extra.len() as u16);
        // No comment, the first disk, no attributes of the content.
        put16(&mut list, 0);
        put16(&mut list, 0);
        put16(&mut list, 0);
        put32(&mut list, if folder { ATTRIBUTE_DIRECTORY } else { 0 });
        put32(&mut list, narrow(local.offset));
        list.extend_from_slice(&local.name);
        list.extend_from_slice(&extra);
    }
    let count = found.len() as u64;
    let size = list.len() as u64;
    let many = count >= u64::from(WIDE16);
    if many || narrow(size) == WIDE32 || narrow(start) == WIDE32 {
        let record = start + size;
        list.extend_from_slice(MARK);
        list.extend_from_slice(&END64);
        // Size of what follows of the record, the versions, two disks.
        put64(&mut list, 44);
        put16(&mut list, VERSION_ZIP64);
        put16(&mut list, VERSION_ZIP64);
        put32(&mut list, 0);
        put32(&mut list, 0);
        put64(&mut list, count);
        put64(&mut list, count);
        put64(&mut list, size);
        put64(&mut list, start);
        list.extend_from_slice(MARK);
        list.extend_from_slice(&LOCATOR64);
        put32(&mut list, 0);
        put64(&mut list, record);
        put32(&mut list, 1);
    }
    let count = if many { WIDE16 } else { count as u16 };
    list.extend_from_slice(MARK);
    list.extend_from_slice(&END);
    put16(&mut list, 0);
    put16(&mut list, 0);
    put16(&mut list, count);
    put16(&mut list, count);
    put32(&mut list, narrow(size));
    put32(&mut list, narrow(start));
    put16(&mut list, 0);
    list
}

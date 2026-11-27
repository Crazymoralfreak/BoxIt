//! Images of Windows: WIM, and ESD, its solid kind.
//!
//! An image file keeps every piece of data once, under its SHA-1: a table
//! says where each piece lies. The list of files of an image is one of the
//! pieces; a file in it names its data by the SHA-1. A piece is stored as
//! it is or compressed in chunks, each chunk by itself, with XPRESS, LZX or
//! LZMS. An ESD compresses many pieces together as one, in chunks of many
//! megabytes: to reach a file there, the chunks it lies in are unpacked
//! whole.
//!
//! A file may hold several images, as an installation disc holds the
//! editions of Windows. They are shown as the folders `1`, `2` and so on;
//! a file with one image shows what is in it.

use std::borrow::Cow;
use std::collections::{HashMap, HashSet};
use std::io::{self, Read, Seek, SeekFrom};

use boxit_core::{
    ArchiveReader, Capabilities, Entry, EntryKind, EntryMeta, EntrySink, Error, OpContext, Result,
    Selection,
};
use sha1::{Digest, Sha1};

use crate::bytes::{le16, le32, le64, part};
use crate::lzx::Lzx;
use crate::source::{Source, SourceReader};
use crate::stored::{cut_short, read_at};
use crate::support::{
    damaged, decode_error, failed, failed_named, from_windows_time, unsupported_method, Watched,
};
use crate::{lzms, xpress};

const SIGNATURE: &[u8; 8] = b"MSWIM\0\0\0";
const HEADER: usize = 208;
const VERSION: u32 = 0x1_0D00;
/// The version of a file that compresses pieces together.
const VERSION_SOLID: u32 = 0xE00;

// The flags of the header.
const FLAG_COMPRESSED: u32 = 0x2;
const FLAG_SPANNED: u32 = 0x8;
const FLAG_XPRESS: u32 = 0x2_0000;
const FLAG_LZX: u32 = 0x4_0000;
const FLAG_LZMS: u32 = 0x8_0000;
const FLAG_XPRESS_2: u32 = 0x20_0000;

// Where things are in the header.
const AT_VERSION: usize = 12;
const AT_FLAGS: usize = 16;
const AT_CHUNK: usize = 20;
const AT_PARTS: usize = 42;
const AT_IMAGES: usize = 44;
const AT_TABLE: usize = 48;

// The flags of a piece.
const PIECE_LIST: u8 = 0x02;
const PIECE_COMPRESSED: u8 = 0x04;
const PIECE_SOLID: u8 = 0x10;
/// The size a record of the table has when it stands for pieces compressed
/// together and not for a piece.
const SOLID_MARK: u64 = 0x1_0000_0000;
/// What pieces compressed together start with: their size, the size of a
/// chunk and the compression.
const SOLID_HEADER: u64 = 16;

/// The size of a record of the table of pieces.
const RECORD: usize = 50;
const HASH: usize = 20;
type Hash = [u8; HASH];
const NO_HASH: Hash = [0; HASH];

/// The size of what a file of a list has before its name.
const DENTRY: usize = 102;
/// The same for one more stream of a file.
const STREAM: usize = 38;
const ATTRIBUTE_DIRECTORY: u32 = 0x10;
const ATTRIBUTE_REPARSE: u32 = 0x400;
const ATTRIBUTE_ENCRYPTED: u32 = 0x4000;
const TAG_SYMLINK: u32 = 0xA000_000C;
const TAG_JUNCTION: u32 = 0xA000_0003;
/// The most a reparse point holds.
const MAX_REPARSE: u64 = 16 * 1024;
/// What Windows puts before the full path a link leads to.
const NT_PREFIX: &str = "\\??\\";

fn broken_list() -> Error {
    damaged("the list of an image runs past its end")
}

fn no_memory() -> Error {
    Error::new("notEnoughMemory")
}

fn align(length: usize) -> Option<usize> {
    Some(length.checked_add(7)? & !7)
}

fn utf16(bytes: &[u8]) -> String {
    let units: Vec<u16> = bytes
        .as_chunks::<2>()
        .0
        .iter()
        .map(|pair| u16::from_le_bytes(*pair))
        .collect();
    String::from_utf16_lossy(&units)
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Packing {
    Stored,
    Xpress,
    Lzx,
    Lzms,
}

impl Packing {
    fn name(self) -> &'static str {
        match self {
            Self::Stored => "Store",
            Self::Xpress => "XPRESS",
            Self::Lzx => "LZX",
            Self::Lzms => "LZMS",
        }
    }
}

/// Where a piece is, as the header and the table say it.
struct Place {
    stored: u64,
    flags: u8,
    offset: u64,
    size: u64,
}

impl Place {
    fn parse(bytes: &[u8], at: usize) -> Option<Self> {
        let first = le64(bytes, at)?;
        Some(Self {
            stored: first & 0x00FF_FFFF_FFFF_FFFF,
            flags: (first >> 56) as u8,
            offset: le64(bytes, at + 8)?,
            size: le64(bytes, at + 16)?,
        })
    }
}

/// Data of the file that is read as one thing: a piece, or pieces
/// compressed together.
#[derive(Clone, Copy, Debug)]
struct Resource {
    offset: u64,
    /// Its size in the file.
    stored: u64,
    /// Its size when unpacked.
    size: u64,
    packing: Packing,
    /// The size of a chunk when unpacked.
    chunk: u32,
    solid: bool,
}

/// A piece: where in a resource it is.
#[derive(Clone, Copy, Debug)]
struct Blob {
    resource: u32,
    offset: u64,
    size: u64,
    hash: Hash,
}

/// What an entry has for content.
#[derive(Clone, Copy, Debug)]
enum Content {
    /// Nothing: a folder, an empty file, a link whose target is known.
    Nothing,
    Data(Blob),
    /// A link whose target lies among pieces compressed together: it is
    /// read when the link is extracted, not when the list is shown.
    Link(Blob, u32),
    /// A file encrypted by the file system: its data is of use to that
    /// file system alone.
    Encrypted,
    /// A file whose data the image does not have.
    Missing,
}

/// Reads the resources of an image file.
struct Unpacker<'a> {
    reader: SourceReader,
    /// Where the reader is, when that is known.
    cursor: Option<u64>,
    file: u64,
    ctx: &'a OpContext,
    /// The resource the table of chunks is of.
    aimed: Option<u32>,
    resource: Resource,
    /// Where every chunk starts in the file, and where the last one ends.
    starts: Vec<u64>,
    /// The chunk in `block`.
    held: Option<u64>,
    block: Vec<u8>,
    packed: Vec<u8>,
    lzx: Option<(u32, Box<Lzx>)>,
}

impl<'a> Unpacker<'a> {
    fn new(source: &Source, ctx: &'a OpContext) -> Result<Self> {
        Ok(Self {
            reader: source.reader()?,
            cursor: None,
            file: source.size(),
            ctx,
            aimed: None,
            resource: Resource {
                offset: 0,
                stored: 0,
                size: 0,
                packing: Packing::Stored,
                chunk: 0,
                solid: false,
            },
            starts: Vec::new(),
            held: None,
            block: Vec::new(),
            packed: Vec::new(),
            lzx: None,
        })
    }

    /// Bytes of the file as they are.
    fn bytes(&mut self, offset: u64, length: u64) -> Result<Vec<u8>> {
        self.cursor = None;
        read_at(&mut self.reader, self.file, offset, length)
    }

    /// Reads at `offset` of the file into all of `buffer`.
    fn fill(&mut self, offset: u64, buffer: &mut [u8]) -> Result<()> {
        // Unknown again until the bytes are read.
        if self.cursor.take() != Some(offset) {
            self.reader.seek(SeekFrom::Start(offset))?;
        }
        self.reader.read_exact(buffer)?;
        self.cursor = Some(offset + buffer.len() as u64);
        Ok(())
    }

    /// Makes ready to read a resource: `which` is its number.
    fn aim(&mut self, which: u32, resource: Resource) -> Result<()> {
        if self.aimed == Some(which) {
            return Ok(());
        }
        self.aimed = None;
        self.held = None;
        self.starts.clear();
        let end = resource
            .offset
            .checked_add(resource.stored)
            .filter(|end| *end <= self.file)
            .ok_or_else(cut_short)?;
        if resource.packing == Packing::Stored {
            if resource.stored != resource.size {
                return Err(damaged("a piece that is not compressed has two sizes"));
            }
        } else {
            let chunk = u64::from(resource.chunk);
            if chunk == 0 {
                return Err(damaged("chunks of no size"));
            }
            // A chunk is held packed and unpacked.
            let memory = self.ctx.budget.memory;
            if chunk.min(resource.size).saturating_mul(2) > memory {
                return Err(no_memory());
            }
            let count = resource.size.div_ceil(chunk);
            // Pieces compressed together say the size of every chunk; a
            // piece alone says where every chunk but the first starts.
            let (head, width, listed) = if resource.solid {
                (SOLID_HEADER, 4, count)
            } else if resource.size > u64::from(u32::MAX) {
                (0, 8, count.saturating_sub(1))
            } else {
                (0, 4, count.saturating_sub(1))
            };
            let table = listed
                .checked_mul(width)
                .and_then(|table| table.checked_add(head))
                .filter(|table| *table <= resource.stored)
                .ok_or_else(|| damaged("a table of chunks larger than what it is of"))?;
            if table.saturating_mul(2) > memory {
                return Err(no_memory());
            }
            let raw = self.bytes(resource.offset + head, table - head)?;
            let number = |at: u64| {
                let at = at as usize;
                match width {
                    8 => le64(&raw, at * 8),
                    _ => le32(&raw, at * 4).map(u64::from),
                }
            };
            let data = resource.offset + table;
            let room = end - data;
            self.starts.reserve(count as usize + 1);
            self.starts.push(data);
            let mut at = 0u64;
            for line in 0..listed {
                let told = number(line).unwrap_or(u64::MAX);
                // The sizes add up; the starts are told from the first.
                let next = if resource.solid {
                    at.checked_add(told)
                } else {
                    Some(told)
                };
                at = next
                    .filter(|next| *next >= at && *next <= room)
                    .ok_or_else(|| damaged("chunks that are not one after another"))?;
                self.starts.push(data + at);
            }
            if !resource.solid && count > 0 {
                self.starts.push(end);
            }
        }
        self.resource = resource;
        self.aimed = Some(which);
        Ok(())
    }

    /// Unpacks a chunk of the resource aimed at into `block`.
    fn fetch(&mut self, index: u64) -> Result<()> {
        if self.held == Some(index) {
            return Ok(());
        }
        self.ctx.checkpoint()?;
        self.held = None;
        let resource = self.resource;
        let chunk = u64::from(resource.chunk);
        let size = (resource.size - index * chunk).min(chunk) as usize;
        let place = usize::try_from(index).ok();
        let bounds = place.and_then(|place| self.starts.get(place..place + 2));
        let Some(&[from, to]) = bounds else {
            return Err(damaged("a chunk that is not in the table"));
        };
        let packed = (to - from) as usize;
        let mut block = std::mem::take(&mut self.block);
        block.resize(size, 0);
        let unpacked = if packed == size {
            // What compression would not make smaller is kept as it is.
            self.fill(from, &mut block)
        } else if packed == 0 || packed > size {
            Err(damaged("a chunk larger packed than unpacked"))
        } else {
            let mut data = std::mem::take(&mut self.packed);
            data.resize(packed, 0);
            let result = self
                .fill(from, &mut data)
                .and_then(|()| self.unpack(&data, &mut block));
            self.packed = data;
            result
        };
        self.block = block;
        unpacked?;
        self.held = Some(index);
        Ok(())
    }

    fn unpack(&mut self, data: &[u8], block: &mut [u8]) -> Result<()> {
        let chunk = self.resource.chunk;
        match self.resource.packing {
            Packing::Stored => Ok(()),
            Packing::Xpress => xpress::decode(data, block).map_err(damaged),
            Packing::Lzms => lzms::decode(data, block).map_err(damaged),
            Packing::Lzx => {
                if !matches!(&self.lzx, Some((size, _)) if *size == chunk) {
                    let lzx = Lzx::image(chunk)
                        .ok_or_else(|| damaged("LZX with chunks of a size it does not have"))?;
                    self.lzx = Some((chunk, Box::new(lzx)));
                }
                match &mut self.lzx {
                    Some((_, lzx)) => lzx.decode(data, block).map_err(damaged),
                    None => Ok(()),
                }
            }
        }
    }

    /// Reads at `position` of the resource aimed at. Gives what is at hand:
    /// no more than the rest of a chunk.
    fn read(&mut self, position: u64, buffer: &mut [u8]) -> Result<usize> {
        let resource = self.resource;
        if position >= resource.size || buffer.is_empty() {
            return Ok(0);
        }
        if resource.packing == Packing::Stored {
            let left = resource.size - position;
            let count = buffer
                .len()
                .min(usize::try_from(left).unwrap_or(usize::MAX));
            self.fill(resource.offset + position, &mut buffer[..count])?;
            return Ok(count);
        }
        let chunk = u64::from(resource.chunk);
        self.fetch(position / chunk)?;
        let within = (position % chunk) as usize;
        let count = buffer.len().min(self.block.len() - within);
        buffer[..count].copy_from_slice(&self.block[within..within + count]);
        Ok(count)
    }

    /// A whole piece, checked against its SHA-1. `limit` is the most
    /// memory it may take.
    fn whole(&mut self, resources: &[Resource], blob: Blob, limit: u64) -> Result<Vec<u8>> {
        if blob.size > limit {
            return Err(no_memory());
        }
        let resource = *resources
            .get(blob.resource as usize)
            .ok_or_else(|| damaged("a piece of no resource"))?;
        self.aim(blob.resource, resource)?;
        let mut data = Vec::new();
        BlobData::new(self, blob)
            .read_to_end(&mut data)
            .map_err(decode_error)?;
        Ok(data)
    }
}

/// The content of a piece, checked against its SHA-1 as it is read.
struct BlobData<'u, 'a> {
    unpacker: &'u mut Unpacker<'a>,
    position: u64,
    left: u64,
    sum: Sha1,
    hash: Hash,
}

impl<'u, 'a> BlobData<'u, 'a> {
    /// The unpacker is aimed at the resource of the piece.
    fn new(unpacker: &'u mut Unpacker<'a>, blob: Blob) -> Self {
        Self {
            unpacker,
            position: blob.offset,
            left: blob.size,
            sum: Sha1::new(),
            hash: blob.hash,
        }
    }
}

impl Read for BlobData<'_, '_> {
    fn read(&mut self, buffer: &mut [u8]) -> io::Result<usize> {
        if self.left == 0 || buffer.is_empty() {
            return Ok(0);
        }
        let want = buffer
            .len()
            .min(usize::try_from(self.left).unwrap_or(usize::MAX));
        let count = self
            .unpacker
            .read(self.position, &mut buffer[..want])
            .map_err(Error::into_io)?;
        if count == 0 {
            return Err(io::ErrorKind::UnexpectedEof.into());
        }
        self.sum.update(&buffer[..count]);
        self.position += count as u64;
        self.left -= count as u64;
        // The last bytes are not given before the whole is known to be
        // what the image says it is.
        if self.left == 0 && self.sum.finalize_reset().as_slice() != self.hash {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "the SHA-1 of the data does not match",
            ));
        }
        Ok(count)
    }
}

/// A file or a folder of the list of an image.
struct Dentry {
    attributes: u32,
    /// Where the list of what a folder holds starts.
    children: u64,
    created: u64,
    accessed: u64,
    written: u64,
    tag: u32,
    name: String,
    /// The reparse point and the content, by their SHA-1.
    link: Hash,
    data: Hash,
}

/// The file at `at` of a list, and where the one after it is. `None` at
/// the end of a folder.
fn dentry(list: &[u8], at: usize) -> Result<Option<(Dentry, usize)>> {
    let length = le64(list, at).ok_or_else(broken_list)?;
    if length <= 8 {
        return Ok(None);
    }
    let length = usize::try_from(length).map_err(|_| broken_list())?;
    let record = part(list, at, length).ok_or_else(broken_list)?;
    if length < DENTRY {
        return Err(damaged("a file of a list shorter than any is"));
    }
    let number = |at: usize| le64(record, at).unwrap_or(0);
    let attributes = le32(record, 8).unwrap_or(0);
    let streams = le16(record, 96).unwrap_or(0);
    let name_length = usize::from(le16(record, 100).unwrap_or(0));
    let name = part(record, DENTRY, name_length).ok_or_else(broken_list)?;

    // The streams without a name, in their order. The one the file itself
    // names counts only when it is one: with more streams after it, it is
    // most often left empty.
    let mut unnamed: Vec<Hash> = Vec::new();
    let own: Hash = part(record, 64, HASH)
        .and_then(|hash| hash.try_into().ok())
        .unwrap_or(NO_HASH);
    if own != NO_HASH {
        unnamed.push(own);
    }
    let mut next = align(length)
        .and_then(|length| at.checked_add(length))
        .ok_or_else(broken_list)?;
    for _ in 0..streams {
        let length = le64(list, next).ok_or_else(broken_list)?;
        let length = usize::try_from(length).map_err(|_| broken_list())?;
        let stream = part(list, next, length).ok_or_else(broken_list)?;
        if length < STREAM {
            return Err(damaged("a stream of a file shorter than any is"));
        }
        if le16(stream, 36) == Some(0) {
            let hash = part(stream, 16, HASH).and_then(|hash| hash.try_into().ok());
            unnamed.push(hash.unwrap_or(NO_HASH));
        }
        next = align(length)
            .and_then(|length| next.checked_add(length))
            .ok_or_else(broken_list)?;
    }
    // A reparse point comes first, the content after it.
    let reparse = attributes & ATTRIBUTE_REPARSE != 0;
    let mut unnamed = unnamed.into_iter();
    let link = if reparse { unnamed.next() } else { None };
    let data = unnamed.next();
    Ok(Some((
        Dentry {
            attributes,
            children: number(16),
            created: number(40),
            accessed: number(48),
            written: number(56),
            tag: if reparse {
                le32(record, 88).unwrap_or(0)
            } else {
                0
            },
            name: utf16(name),
            link: link.unwrap_or(NO_HASH),
            data: data.unwrap_or(NO_HASH),
        },
        next,
    )))
}

/// Where a link leads, from the data of its reparse point as an image
/// keeps it: without the eight bytes it starts with on a disk.
fn target(tag: u32, data: &[u8]) -> Option<String> {
    // A symbolic link has flags after the places of its names.
    let head = if tag == TAG_SYMLINK { 12 } else { 8 };
    let names = data.get(head..)?;
    let name = |at: usize| {
        let offset = usize::from(le16(data, at)?);
        let length = usize::from(le16(data, at + 2)?);
        part(names, offset, length).map(utf16)
    };
    // The name the system follows, and the one it shows.
    let followed = name(0)?;
    let followed = followed.strip_prefix(NT_PREFIX).unwrap_or(&followed);
    let text = if followed.is_empty() {
        name(4)?
    } else {
        followed.to_owned()
    };
    Some(text.replace('\\', "/"))
}

/// The list of an image file, and where the content of everything is.
struct Loaded {
    entries: Vec<Entry>,
    /// By entry.
    contents: Vec<Content>,
    resources: Vec<Resource>,
    solid: bool,
}

/// What is known of an image file before the lists of its images.
struct Pieces {
    resources: Vec<Resource>,
    blobs: HashMap<Hash, Blob>,
    /// The lists of the images.
    lists: Vec<Blob>,
}

/// Reads the header and the table of pieces.
fn pieces(unpacker: &mut Unpacker) -> Result<Pieces> {
    let head = unpacker.bytes(0, HEADER as u64)?;
    if !head.starts_with(SIGNATURE) {
        return Err(damaged("no signature of an image of Windows"));
    }
    let field = |at: usize| le32(&head, at).unwrap_or(0);
    let unsupported = |detail: &str| Error::new("unsupportedArchive").detail(detail);
    let version = field(AT_VERSION);
    if version != VERSION && version != VERSION_SOLID {
        return Err(unsupported("an image of a version that is not read"));
    }
    let flags = field(AT_FLAGS);
    if flags & FLAG_SPANNED != 0 || le16(&head, AT_PARTS) != Some(1) {
        return Err(unsupported("a part of an image split into files"));
    }
    let packing = if flags & FLAG_COMPRESSED == 0 {
        Packing::Stored
    } else if flags & FLAG_LZMS != 0 {
        Packing::Lzms
    } else if flags & FLAG_LZX != 0 {
        Packing::Lzx
    } else if flags & (FLAG_XPRESS | FLAG_XPRESS_2) != 0 {
        Packing::Xpress
    } else {
        return Err(damaged("an image compressed in no known way"));
    };
    let chunk = field(AT_CHUNK);
    let alone = |place: &Place| -> Result<Resource> {
        let compressed = place.flags & PIECE_COMPRESSED != 0;
        if compressed && packing == Packing::Stored {
            return Err(damaged("a compressed piece in an image that has none"));
        }
        Ok(Resource {
            offset: place.offset,
            stored: place.stored,
            size: place.size,
            packing: if compressed { packing } else { Packing::Stored },
            chunk,
            solid: false,
        })
    };

    // The table is a piece itself, without a SHA-1 of its own.
    let place = Place::parse(&head, AT_TABLE).ok_or_else(cut_short)?;
    let memory = unpacker.ctx.budget.memory;
    if place.size > memory {
        return Err(no_memory());
    }
    let resource = alone(&place)?;
    unpacker.aim(u32::MAX, resource)?;
    let mut table = vec![0u8; place.size as usize];
    let mut at = 0;
    while at < table.len() {
        match unpacker.read(at as u64, &mut table[at..])? {
            0 => return Err(cut_short()),
            count => at += count,
        }
    }
    unpacker.aimed = None;

    let mut resources: Vec<Resource> = Vec::new();
    let mut blobs = HashMap::with_capacity(table.len() / RECORD);
    let mut lists = Vec::new();
    // The resources of pieces compressed together that the records after
    // them count their places through, and whether more may join them.
    let mut together: Vec<u32> = Vec::new();
    let mut joining = false;
    for record in table.as_chunks::<RECORD>().0 {
        let Some(place) = Place::parse(record, 0) else {
            continue;
        };
        let hash: Hash = record[RECORD - HASH..].try_into().unwrap_or(NO_HASH);
        let number = u32::try_from(resources.len()).map_err(|_| no_memory())?;
        let blob = if version == VERSION_SOLID && place.flags & PIECE_SOLID != 0 {
            if place.size == SOLID_MARK {
                if !joining {
                    together.clear();
                    joining = true;
                }
                let head = unpacker.bytes(place.offset, SOLID_HEADER)?;
                let packing = match le32(&head, 12) {
                    Some(1) => Packing::Xpress,
                    Some(2) => Packing::Lzx,
                    Some(3) => Packing::Lzms,
                    other => {
                        let number = other.unwrap_or(0);
                        return Err(unsupported_method(format!("WIM:{number}")));
                    }
                };
                resources.push(Resource {
                    offset: place.offset,
                    stored: place.stored,
                    size: le64(&head, 0).unwrap_or(0),
                    packing,
                    chunk: le32(&head, 8).unwrap_or(0),
                    solid: true,
                });
                together.push(number);
                continue;
            }
            joining = false;
            // The place is counted through all of them as through one.
            let mut offset = place.offset;
            let found = together.iter().find(|number| {
                let size = resources[**number as usize].size;
                let fits = offset
                    .checked_add(place.stored)
                    .is_some_and(|end| end <= size);
                if !fits {
                    offset = offset.saturating_sub(size);
                }
                fits
            });
            let Some(resource) = found else {
                return Err(damaged("a piece outside of what it is compressed with"));
            };
            Blob {
                resource: *resource,
                offset,
                size: place.stored,
                hash,
            }
        } else {
            together.clear();
            joining = false;
            resources.push(alone(&place)?);
            Blob {
                resource: number,
                offset: 0,
                size: place.size,
                hash,
            }
        };
        if place.flags & PIECE_LIST != 0 {
            lists.push(blob);
        } else {
            blobs.entry(hash).or_insert(blob);
        }
    }
    // The images are the first lists, as many as the header says.
    let images = field(AT_IMAGES) as usize;
    if lists.len() < images {
        return Err(damaged("fewer lists of images than images"));
    }
    lists.truncate(images);
    Ok(Pieces {
        resources,
        blobs,
        lists,
    })
}

/// Goes through the list of an image.
struct Lister<'p, 'u, 'a> {
    pieces: &'p Pieces,
    unpacker: &'u mut Unpacker<'a>,
    entries: Vec<Entry>,
    contents: Vec<Content>,
    /// The bytes the paths take: a list may give short names long paths
    /// without end.
    spent: u64,
}

impl Lister<'_, '_, '_> {
    fn push(&mut self, path: String, item: &Dentry) -> Result<()> {
        self.spent += path.len() as u64;
        if self.spent > self.unpacker.ctx.budget.memory {
            return Err(no_memory());
        }
        let folder = item.attributes & ATTRIBUTE_DIRECTORY != 0;
        let link = matches!(item.tag, TAG_SYMLINK | TAG_JUNCTION);
        let mut entry = Entry {
            index: self.entries.len() as u64,
            ..Entry::default()
        };
        let mut content = Content::Nothing;
        let mut meta = if link {
            let mut target_of = None;
            match self.pieces.blobs.get(&item.link) {
                Some(blob) if self.pieces.resources[blob.resource as usize].solid => {
                    content = Content::Link(*blob, item.tag);
                }
                Some(blob) => {
                    let data = self
                        .unpacker
                        .whole(&self.pieces.resources, *blob, MAX_REPARSE)
                        .map_err(|error| failed_named(&path, false, error))?;
                    target_of = target(item.tag, &data);
                }
                None => {}
            }
            EntryMeta {
                kind: EntryKind::Symlink,
                link_target: target_of,
                size: Some(0),
                ..EntryMeta::default()
            }
        } else if folder {
            EntryMeta::directory("")
        } else {
            let blob = self.pieces.blobs.get(&item.data);
            let size = match blob {
                Some(blob) => Some(blob.size),
                None if item.data == NO_HASH => Some(0),
                None => None,
            };
            if let Some(blob) = blob {
                let resource = &self.pieces.resources[blob.resource as usize];
                entry.method = Some(Cow::Borrowed(resource.packing.name()));
                if !resource.solid {
                    entry.packed_size = Some(resource.stored);
                }
            }
            content = match blob {
                _ if item.attributes & ATTRIBUTE_ENCRYPTED != 0 => Content::Encrypted,
                Some(blob) if blob.size > 0 => Content::Data(*blob),
                Some(_) => Content::Nothing,
                None if item.data == NO_HASH => Content::Nothing,
                None => Content::Missing,
            };
            EntryMeta {
                size,
                ..EntryMeta::default()
            }
        };
        meta.path = path;
        meta.modified = from_windows_time(item.written);
        meta.created = from_windows_time(item.created);
        meta.accessed = from_windows_time(item.accessed);
        meta.windows_attributes = Some(item.attributes);
        entry.meta = meta;
        self.entries.push(entry);
        self.contents.push(content);
        Ok(())
    }

    /// Lists an image. `folder` is where its files are shown: nowhere for
    /// the only image of a file.
    fn image(&mut self, list: &[u8], folder: Option<String>) -> Result<()> {
        // Before the files come the descriptions of who may do what.
        let security = le32(list, 0).ok_or_else(broken_list)? as usize;
        let root = align(security.max(8)).ok_or_else(broken_list)?;
        let Some((top, _)) = dentry(list, root)? else {
            return Err(damaged("an image without a folder to start from"));
        };
        if let Some(folder) = &folder {
            let top = Dentry {
                attributes: top.attributes | ATTRIBUTE_DIRECTORY,
                tag: 0,
                ..top
            };
            self.push(folder.clone(), &top)?;
        }
        let place = |children: u64| usize::try_from(children).map_err(|_| broken_list());
        // The folders being listed, the innermost last: where the next
        // file of each is, and the path of the folder.
        let mut open: Vec<(usize, String)> = Vec::new();
        // A folder listed once is not listed again, whatever leads to it.
        let mut seen: HashSet<usize> = HashSet::new();
        if top.children != 0 && seen.insert(place(top.children)?) {
            open.push((place(top.children)?, folder.unwrap_or_default()));
        }
        while let Some(inner) = open.len().checked_sub(1) {
            self.unpacker.ctx.checkpoint()?;
            let Some((item, next)) = dentry(list, open[inner].0)? else {
                open.pop();
                continue;
            };
            open[inner].0 = next;
            // What has no name cannot be shown; the system gives none.
            if item.name.is_empty() {
                continue;
            }
            let path = match open[inner].1.as_str() {
                "" => item.name.clone(),
                folder => format!("{folder}/{}", item.name),
            };
            let enters = item.attributes & ATTRIBUTE_DIRECTORY != 0
                && !matches!(item.tag, TAG_SYMLINK | TAG_JUNCTION)
                && item.children != 0
                && seen.insert(place(item.children)?);
            let entered = enters.then(|| path.clone());
            self.push(path, &item)?;
            if let Some(path) = entered {
                open.push((place(item.children)?, path));
            }
        }
        Ok(())
    }
}

fn load(source: &Source, ctx: &OpContext) -> Result<Loaded> {
    let mut unpacker = Unpacker::new(source, ctx)?;
    let pieces = pieces(&mut unpacker)?;
    let mut lister = Lister {
        pieces: &pieces,
        unpacker: &mut unpacker,
        entries: Vec::new(),
        contents: Vec::new(),
        spent: 0,
    };
    let many = pieces.lists.len() > 1;
    for (number, blob) in pieces.lists.iter().enumerate() {
        ctx.checkpoint()?;
        let list = lister
            .unpacker
            .whole(&pieces.resources, *blob, ctx.budget.memory)?;
        lister.image(&list, many.then(|| (number + 1).to_string()))?;
    }
    let Lister {
        entries, contents, ..
    } = lister;
    let solid = pieces.resources.iter().any(|resource| resource.solid);
    Ok(Loaded {
        entries,
        contents,
        resources: pieces.resources,
        solid,
    })
}

/// An image file of Windows opened for reading.
pub struct WimReader {
    source: Source,
    loaded: Option<Loaded>,
}

impl WimReader {
    pub fn new(source: Source) -> Self {
        Self {
            source,
            loaded: None,
        }
    }

    fn load(&mut self, ctx: &OpContext) -> Result<&Loaded> {
        if self.loaded.is_none() {
            self.loaded = Some(load(&self.source, ctx)?);
        }
        self.loaded
            .as_ref()
            .ok_or_else(|| Error::new("internal").detail("the list of an image is gone"))
    }
}

impl ArchiveReader for WimReader {
    fn capabilities(&self) -> Capabilities {
        let solid = self.loaded.as_ref().is_some_and(|loaded| loaded.solid);
        Capabilities {
            random_access: !solid,
            solid,
            parallel: false,
            has_index: true,
        }
    }

    fn entries(&mut self, ctx: &OpContext) -> Result<Vec<Entry>> {
        Ok(self.load(ctx)?.entries.clone())
    }

    fn extract(&mut self, pick: &Selection, sink: &dyn EntrySink, ctx: &OpContext) -> Result<()> {
        self.load(ctx)?;
        let (source, Some(loaded)) = (&self.source, &self.loaded) else {
            return Ok(());
        };
        // What has no data first, as the list has it; then the data in the
        // order it lies in the file, so that chunks shared by files are
        // unpacked once.
        let place = |index: usize| match loaded.contents[index] {
            Content::Data(blob) | Content::Link(blob, _) => {
                let resource = &loaded.resources[blob.resource as usize];
                (true, resource.offset, blob.offset, index)
            }
            Content::Nothing | Content::Encrypted | Content::Missing => (false, 0, 0, index),
        };
        let mut wanted: Vec<usize> = (0..loaded.entries.len())
            .filter(|index| pick.contains(&loaded.entries[*index]))
            .collect();
        wanted.sort_by_key(|index| place(*index));
        let mut unpacker = Unpacker::new(source, ctx)?;
        for index in wanted {
            ctx.checkpoint()?;
            let entry = &loaded.entries[index];
            match loaded.contents[index] {
                Content::Nothing => sink.accept(entry, &mut io::empty())?,
                Content::Encrypted => return Err(unsupported_method("EFS")),
                Content::Missing => {
                    return Err(Error::new("damagedEntry")
                        .with("name", &entry.meta.path)
                        .detail("the image does not hold the data of the file"));
                }
                Content::Link(blob, tag) => {
                    let data = unpacker
                        .whole(&loaded.resources, blob, MAX_REPARSE)
                        .map_err(|error| failed(entry, error))?;
                    let mut link = entry.clone();
                    link.meta.link_target = target(tag, &data);
                    sink.accept(&link, &mut io::empty())?;
                }
                Content::Data(blob) => {
                    unpacker
                        .aim(blob.resource, loaded.resources[blob.resource as usize])
                        .map_err(|error| failed(entry, error))?;
                    let mut data = Watched::new(BlobData::new(&mut unpacker, blob));
                    sink.accept(entry, &mut data)
                        .map_err(|error| data.settle(entry, error))?;
                }
            }
        }
        Ok(())
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use boxit_core::{Budget, Timestamp};
    use proptest::prelude::*;

    use super::*;
    use crate::lzx::tests::{sample, Kind, Packer};
    use crate::tests::{code, lines, pattern, Collector, Got};
    use crate::xpress::tests::{pack as xpress, Item};

    /// The time of everything in a test image, 2023-11-14 22:13:20 UTC, in
    /// the ticks of Windows.
    const TIME: u64 = (1_700_000_000 + 11_644_473_600) * 10_000_000;
    const SECOND: u64 = 10_000_000;
    const ATTRIBUTE_ARCHIVE: u32 = 0x20;
    /// The size of what says where a piece is.
    const PLACE: usize = 24;

    /// Pieces compressed together: the format, the size of a chunk, the
    /// data and its chunks.
    type Resource<'a> = (u32, u32, &'a [u8], Vec<Vec<u8>>);
    /// A format with its flag and its name, and two pieces of data with
    /// their chunks.
    type Case<'a> = (u32, &'a str, &'a [u8], Vec<Vec<u8>>, &'a [u8], Vec<Vec<u8>>);
    /// A reparse point that is no link: a file kept compressed on its disk.
    const TAG_OTHER: u32 = 0x8000_0017;

    pub(crate) fn sha(data: &[u8]) -> Hash {
        Sha1::digest(data).as_slice().try_into().unwrap()
    }

    /// A file or a folder of a test image.
    #[derive(Clone, Default)]
    pub(crate) struct Node {
        name: String,
        attributes: u32,
        tag: u32,
        hash: Hash,
        /// More streams: their names and what they hold.
        streams: Vec<(String, Hash)>,
        children: Vec<Node>,
    }

    pub(crate) fn file(name: &str, hash: Hash) -> Node {
        Node {
            name: name.to_owned(),
            attributes: ATTRIBUTE_ARCHIVE,
            hash,
            ..Node::default()
        }
    }

    pub(crate) fn folder(name: &str, children: Vec<Node>) -> Node {
        Node {
            name: name.to_owned(),
            attributes: ATTRIBUTE_DIRECTORY,
            children,
            ..Node::default()
        }
    }

    fn link(name: &str, tag: u32, hash: Hash) -> Node {
        Node {
            name: name.to_owned(),
            attributes: ATTRIBUTE_REPARSE,
            tag,
            hash,
            ..Node::default()
        }
    }

    fn wide(text: &str) -> Vec<u8> {
        text.encode_utf16().flat_map(u16::to_le_bytes).collect()
    }

    fn pad(bytes: &mut Vec<u8>) {
        bytes.resize(bytes.len().next_multiple_of(8), 0);
    }

    fn dentry_of(node: &Node, children: u64) -> Vec<u8> {
        let name = wide(&node.name);
        let mut record = vec![0u8; DENTRY];
        record[8..12].copy_from_slice(&node.attributes.to_le_bytes());
        record[12..16].copy_from_slice(&u32::MAX.to_le_bytes());
        record[16..24].copy_from_slice(&children.to_le_bytes());
        record[40..48].copy_from_slice(&(TIME + SECOND).to_le_bytes());
        record[48..56].copy_from_slice(&(TIME + 2 * SECOND).to_le_bytes());
        record[56..64].copy_from_slice(&TIME.to_le_bytes());
        record[64..84].copy_from_slice(&node.hash);
        if node.attributes & ATTRIBUTE_REPARSE != 0 {
            record[88..92].copy_from_slice(&node.tag.to_le_bytes());
        }
        record[96..98].copy_from_slice(&(node.streams.len() as u16).to_le_bytes());
        record[100..102].copy_from_slice(&(name.len() as u16).to_le_bytes());
        if !name.is_empty() {
            record.extend(&name);
            record.extend([0, 0]);
        }
        pad(&mut record);
        let length = record.len() as u64;
        record[..8].copy_from_slice(&length.to_le_bytes());
        for (name, hash) in &node.streams {
            let name = wide(name);
            let mut stream = vec![0u8; STREAM];
            stream[16..36].copy_from_slice(hash);
            stream[36..38].copy_from_slice(&(name.len() as u16).to_le_bytes());
            if !name.is_empty() {
                stream.extend(&name);
                stream.extend([0, 0]);
            }
            pad(&mut stream);
            let length = stream.len() as u64;
            stream[..8].copy_from_slice(&length.to_le_bytes());
            record.extend(stream);
        }
        record
    }

    /// Writes what the folders hold, the innermost first: a folder says
    /// where the list of what it holds is.
    fn lay(out: &mut Vec<u8>, nodes: &[Node]) -> u64 {
        if nodes.is_empty() {
            return 0;
        }
        let inner: Vec<u64> = nodes.iter().map(|node| lay(out, &node.children)).collect();
        let at = out.len() as u64;
        for (node, children) in nodes.iter().zip(inner) {
            out.extend(dentry_of(node, children));
        }
        out.extend([0u8; 8]);
        at
    }

    /// The list of an image: what `root` holds.
    pub(crate) fn list(root: &Node) -> Vec<u8> {
        // No descriptions of who may do what.
        let mut out = vec![8, 0, 0, 0, 0, 0, 0, 0];
        out.extend(dentry_of(root, 0));
        out.extend([0u8; 8]);
        let children = lay(&mut out, &root.children);
        out[8 + 16..8 + 24].copy_from_slice(&children.to_le_bytes());
        out
    }

    fn place(stored: u64, flags: u8, offset: u64, size: u64) -> Vec<u8> {
        let mut bytes = (stored | u64::from(flags) << 56).to_le_bytes().to_vec();
        bytes.extend(offset.to_le_bytes());
        bytes.extend(size.to_le_bytes());
        bytes
    }

    /// Writes an image file piece by piece.
    pub(crate) struct Builder {
        bytes: Vec<u8>,
        table: Vec<u8>,
        flags: u32,
        chunk: u32,
        version: u32,
        images: u32,
        parts: u16,
    }

    impl Builder {
        /// An image file compressed as `flags` say, in chunks of `chunk`.
        pub(crate) fn new(flags: u32, chunk: u32) -> Self {
            Self {
                bytes: vec![0u8; HEADER],
                table: Vec::new(),
                flags,
                chunk,
                version: VERSION,
                images: 0,
                parts: 1,
            }
        }

        fn record(&mut self, stored: u64, flags: u8, offset: u64, size: u64, hash: Hash) {
            self.table.extend(place(stored, flags, offset, size));
            self.table.extend(1u16.to_le_bytes());
            self.table.extend(1u32.to_le_bytes());
            self.table.extend(hash);
        }

        /// A piece: as it is, or in chunks compressed already.
        fn piece(&mut self, data: &[u8], chunks: Option<&[Vec<u8>]>, flags: u8) -> Hash {
            let offset = self.bytes.len() as u64;
            let mut flags = flags;
            match chunks {
                None => self.bytes.extend(data),
                Some(chunks) => {
                    flags |= PIECE_COMPRESSED;
                    let mut at = 0u32;
                    for chunk in &chunks[..chunks.len() - 1] {
                        at += chunk.len() as u32;
                        self.bytes.extend(at.to_le_bytes());
                    }
                    self.bytes.extend(chunks.concat());
                }
            }
            let stored = self.bytes.len() as u64 - offset;
            let hash = sha(data);
            self.record(stored, flags, offset, data.len() as u64, hash);
            hash
        }

        pub(crate) fn plain(&mut self, data: &[u8]) -> Hash {
            self.piece(data, None, 0)
        }

        pub(crate) fn packed(&mut self, data: &[u8], chunks: &[Vec<u8>]) -> Hash {
            self.piece(data, Some(chunks), 0)
        }

        /// Pieces compressed together. Each of `resources` is the format,
        /// the size of a chunk, the data and its chunks; the pieces are
        /// what the data of all of them is cut into at `cuts`.
        fn together(&mut self, resources: &[Resource], cuts: &[usize]) -> Vec<Hash> {
            let flags = PIECE_SOLID | PIECE_COMPRESSED;
            let mut all = Vec::new();
            for (format, chunk, data, chunks) in resources {
                let offset = self.bytes.len() as u64;
                self.bytes.extend((data.len() as u64).to_le_bytes());
                self.bytes.extend(chunk.to_le_bytes());
                self.bytes.extend(format.to_le_bytes());
                for chunk in chunks {
                    self.bytes.extend((chunk.len() as u32).to_le_bytes());
                }
                self.bytes.extend(chunks.concat());
                let stored = self.bytes.len() as u64 - offset;
                self.record(stored, flags, offset, SOLID_MARK, NO_HASH);
                all.extend(*data);
            }
            let mut from = 0;
            let mut hashes = Vec::new();
            for to in cuts.iter().copied().chain([all.len()]) {
                let piece = &all[from..to];
                let hash = sha(piece);
                self.record(
                    piece.len() as u64,
                    flags,
                    from as u64,
                    piece.len() as u64,
                    hash,
                );
                hashes.push(hash);
                from = to;
            }
            hashes
        }

        /// An image: what `root` holds.
        pub(crate) fn image(&mut self, root: &Node) {
            self.piece(&list(root), None, PIECE_LIST);
            self.images += 1;
        }

        pub(crate) fn finish(mut self) -> Vec<u8> {
            let offset = self.bytes.len() as u64;
            let table = std::mem::take(&mut self.table);
            self.bytes.extend(&table);
            let head = &mut self.bytes[..HEADER];
            head[..8].copy_from_slice(SIGNATURE);
            head[8..12].copy_from_slice(&(HEADER as u32).to_le_bytes());
            head[AT_VERSION..AT_VERSION + 4].copy_from_slice(&self.version.to_le_bytes());
            head[AT_FLAGS..AT_FLAGS + 4].copy_from_slice(&self.flags.to_le_bytes());
            head[AT_CHUNK..AT_CHUNK + 4].copy_from_slice(&self.chunk.to_le_bytes());
            head[40..42].copy_from_slice(&1u16.to_le_bytes());
            head[AT_PARTS..AT_PARTS + 2].copy_from_slice(&self.parts.to_le_bytes());
            head[AT_IMAGES..AT_IMAGES + 4].copy_from_slice(&self.images.to_le_bytes());
            let size = table.len() as u64;
            head[AT_TABLE..AT_TABLE + PLACE]
                .copy_from_slice(&place(size, PIECE_LIST, offset, size));
            self.bytes
        }
    }

    /// `data` with zeros after it up to `size`, and the same as a chunk
    /// of XPRESS: the zeros make it smaller than it was.
    fn squeezed(data: &[u8], size: usize) -> (Vec<u8>, Vec<u8>) {
        let mut plain = data.to_vec();
        plain.resize(size, 0);
        let mut items: Vec<Item> = data.iter().map(|byte| Item::Byte(*byte)).collect();
        items.push(Item::Byte(0));
        items.push(Item::Match {
            distance: 1,
            length: size - data.len() - 1,
        });
        let packed = xpress(&items);
        assert!(packed.len() < size);
        (plain, packed)
    }

    /// The chunks of LZX of `data`; one that would not get smaller is
    /// kept as it is, as a writer of images keeps it.
    fn lzx_chunks(data: &[u8], chunk: usize) -> Vec<Vec<u8>> {
        let blocks = [(Kind::Verbatim, 20_000), (Kind::Aligned, 9_000)];
        data.chunks(chunk)
            .map(|piece| {
                let packed = Packer::image(chunk as u32)
                    .pack(piece, &blocks)
                    .swap_remove(0);
                if packed.len() < piece.len() {
                    packed
                } else {
                    piece.to_vec()
                }
            })
            .collect()
    }

    /// The data of the reparse point of a link, as an image keeps it.
    fn reparse(tag: u32, followed: &str, shown: &str) -> Vec<u8> {
        let (followed, shown) = (wide(followed), wide(shown));
        let mut data = Vec::new();
        data.extend(0u16.to_le_bytes());
        data.extend((followed.len() as u16).to_le_bytes());
        data.extend((followed.len() as u16 + 2).to_le_bytes());
        data.extend((shown.len() as u16).to_le_bytes());
        if tag == TAG_SYMLINK {
            data.extend(1u32.to_le_bytes());
        }
        data.extend(&followed);
        data.extend([0, 0]);
        data.extend(&shown);
        data.extend([0, 0]);
        data
    }

    fn open(bytes: Vec<u8>) -> WimReader {
        WimReader::new(Source::memory("x.wim", bytes))
    }

    /// Everything of an image file, in the order of its list.
    pub(crate) fn read(bytes: Vec<u8>) -> Result<Vec<Got>> {
        let mut reader = open(bytes);
        let ctx = OpContext::silent();
        let listed = reader.entries(&ctx)?;
        let collector = Collector::default();
        reader.extract(&Selection::All, &collector, &ctx)?;
        let mut got = collector.in_order();
        got.sort_by_key(|got| got.entry.index);
        assert_eq!(got.len(), listed.len());
        for (got, listed) in got.iter().zip(&listed) {
            assert_eq!(got.entry.meta.path, listed.meta.path);
            assert_eq!(got.entry.index, listed.index);
        }
        Ok(got)
    }

    fn paths(got: &[Got]) -> Vec<&str> {
        got.iter().map(|got| got.entry.meta.path.as_str()).collect()
    }

    fn by_path<'g>(got: &'g [Got], path: &str) -> &'g Got {
        got.iter()
            .find(|got| got.entry.meta.path == path)
            .unwrap_or_else(|| panic!("no {path}"))
    }

    #[test]
    fn an_image_is_listed_and_its_files_are_read() {
        let big = pattern(70_000);
        let mut builder = Builder::new(0, 0);
        let readme = builder.plain(b"read me\n");
        let hello = builder.plain("привет".as_bytes());
        let large = builder.plain(&big);
        builder.image(&folder(
            "",
            vec![
                file("readme.txt", readme),
                folder(
                    "docs",
                    vec![
                        file("привет мир.txt", hello),
                        folder("empty", vec![]),
                        file("empty.txt", NO_HASH),
                    ],
                ),
                file("big.bin", large),
                // The same data under another name is kept once.
                file("copy.txt", readme),
            ],
        ));
        let bytes = builder.finish();
        let got = read(bytes.clone()).unwrap();
        assert_eq!(
            paths(&got),
            [
                "readme.txt",
                "docs",
                "docs/привет мир.txt",
                "docs/empty",
                "docs/empty.txt",
                "big.bin",
                "copy.txt",
            ]
        );
        let kinds: Vec<EntryKind> = got.iter().map(|got| got.entry.meta.kind).collect();
        let (file, folder) = (EntryKind::File, EntryKind::Directory);
        assert_eq!(kinds, [file, folder, file, folder, file, file, file]);
        assert_eq!(got[0].data, b"read me\n");
        assert_eq!(got[2].data, "привет".as_bytes());
        assert_eq!(got[4].data, b"");
        assert!(got[5].data == big);
        assert_eq!(got[6].data, b"read me\n");

        let first = &got[0].entry;
        let moment = Timestamp::from_seconds(1_700_000_000);
        assert_eq!(first.meta.modified, Some(moment));
        assert_eq!(
            first.meta.created,
            Some(Timestamp::from_seconds(1_700_000_001))
        );
        assert_eq!(
            first.meta.accessed,
            Some(Timestamp::from_seconds(1_700_000_002))
        );
        assert_eq!(first.meta.size, Some(8));
        assert_eq!(first.meta.windows_attributes, Some(ATTRIBUTE_ARCHIVE));
        assert_eq!(first.method.as_deref(), Some("Store"));
        assert_eq!(first.packed_size, Some(8));
        assert_eq!(got[1].entry.meta.modified, Some(moment));
        assert_eq!(got[4].entry.meta.size, Some(0));
        assert_eq!(got[4].entry.method, None);

        // A part of it, and in an order that is not the one of the file.
        let mut reader = open(bytes);
        let ctx = OpContext::silent();
        assert!(reader.capabilities().random_access);
        let collector = Collector::default();
        let pick = Selection::indexes(vec![6, 2]);
        reader.extract(&pick, &collector, &ctx).unwrap();
        let got = collector.in_order();
        assert_eq!(paths(&got), ["copy.txt", "docs/привет мир.txt"]);
        assert_eq!(got[0].data, b"read me\n");
        assert!(!reader.capabilities().solid);
    }

    #[test]
    fn every_compression_is_read() {
        let text = lines(700);
        let even = pattern(32768);
        let two = [&even[..], &text[..]].concat();
        let noise = sample(40_000, 7)
            .iter()
            .enumerate()
            .map(|(at, byte)| byte.wrapping_mul(31) ^ (at as u8))
            .collect::<Vec<u8>>();
        let raw: Vec<Vec<u8>> = noise.chunks(32768).map(<[u8]>::to_vec).collect();

        let xpress_chunks = |name: &str| -> Vec<u8> {
            match name {
                "text" => include_bytes!("../fixtures/text.xpress").to_vec(),
                _ => include_bytes!("../fixtures/pattern.xpress").to_vec(),
            }
        };
        let lzms_chunks = |name: &str| -> Vec<u8> {
            match name {
                "text" => include_bytes!("../fixtures/text.lzms").to_vec(),
                _ => include_bytes!("../fixtures/pattern.lzms").to_vec(),
            }
        };
        // What the test writes of LZX gets smaller only with data made
        // for it.
        let (mixed, more) = (sample(30_000, 3), sample(60_000, 9));
        let cases: [Case; 3] = [
            (
                FLAG_XPRESS,
                "XPRESS",
                &text,
                vec![xpress_chunks("text")],
                &two,
                vec![xpress_chunks("even"), xpress_chunks("text")],
            ),
            (
                FLAG_LZMS,
                "LZMS",
                &text,
                vec![lzms_chunks("text")],
                &two,
                vec![lzms_chunks("even"), lzms_chunks("text")],
            ),
            (
                FLAG_LZX,
                "LZX",
                &mixed,
                lzx_chunks(&mixed, 32768),
                &more,
                lzx_chunks(&more, 32768),
            ),
        ];
        for (flag, name, text, one_chunk, two, two_chunks) in cases {
            assert_eq!((one_chunk.len(), two_chunks.len()), (1, 2), "{name}");
            let smaller = |chunk: &Vec<u8>| chunk.len() < 32768.min(text.len());
            assert!(one_chunk.iter().chain(&two_chunks).all(smaller), "{name}");
            let mut builder = Builder::new(FLAG_COMPRESSED | flag, 32768);
            let short = builder.packed(text, &one_chunk);
            let long = builder.packed(two, &two_chunks);
            // Chunks that compression would not make smaller.
            let kept = builder.packed(&noise, &raw);
            // A piece that is not compressed at all.
            let plain = builder.plain(b"plain");
            // The list is compressed too, with what the test can write.
            let root = folder(
                "",
                vec![
                    file("short.txt", short),
                    file("long.bin", long),
                    file("noise.bin", kept),
                    file("plain.txt", plain),
                ],
            );
            if flag == FLAG_XPRESS {
                let (list, packed) = squeezed(&list(&root), 20_000);
                builder.piece(&list, Some(&[packed]), PIECE_LIST);
                builder.images += 1;
            } else {
                builder.image(&root);
            }
            let got = read(builder.finish()).unwrap_or_else(|error| panic!("{name}: {error:?}"));
            assert_eq!(
                paths(&got),
                ["short.txt", "long.bin", "noise.bin", "plain.txt"],
                "{name}"
            );
            assert!(got[0].data == text, "{name}");
            assert!(got[1].data == two, "{name}");
            assert!(got[2].data == noise, "{name}");
            assert_eq!(got[3].data, b"plain", "{name}");
            assert_eq!(got[0].entry.method.as_deref(), Some(name));
            assert_eq!(got[0].entry.packed_size, Some(one_chunk[0].len() as u64));
            assert_eq!(got[3].entry.method.as_deref(), Some("Store"));
            let table = 4 + two_chunks.iter().map(Vec::len).sum::<usize>();
            assert_eq!(got[1].entry.packed_size, Some(table as u64), "{name}");
        }
    }

    #[test]
    fn pieces_compressed_together_are_read() {
        let text = lines(30_000);
        let program = code(400_000);
        let large = [&text[..], &program[..], &text[..]].concat();
        let (first, second) = (text.len(), text.len() + program.len());
        let target = reparse(TAG_SYMLINK, "..\\docs\\a.txt", "..\\docs\\a.txt");
        let (small, small_packed) = squeezed(&target, 40_000);

        let mut builder = Builder::new(FLAG_COMPRESSED | FLAG_LZMS, 1 << 22);
        builder.version = VERSION_SOLID;
        let hashes = builder.together(
            &[
                (
                    3,
                    1 << 22,
                    &large,
                    vec![include_bytes!("../fixtures/large.lzms").to_vec()],
                ),
                (1, 1 << 16, &small, vec![small_packed]),
            ],
            &[
                first,
                second,
                second + 1000,
                large.len(),
                large.len() + target.len(),
            ],
        );
        // A piece by itself among them.
        let alone = builder.plain(b"alone");
        builder.image(&folder(
            "",
            vec![
                folder(
                    "docs",
                    vec![file("a.txt", hashes[0]), file("tail.txt", hashes[3])],
                ),
                file("program.bin", hashes[1]),
                file("head.txt", hashes[2]),
                link("link.txt", TAG_SYMLINK, hashes[4]),
                file("zeros.bin", hashes[5]),
                file("alone.txt", alone),
            ],
        ));
        let bytes = builder.finish();

        let mut reader = open(bytes.clone());
        let ctx = OpContext::silent();
        let listed = reader.entries(&ctx).unwrap();
        assert!(reader.capabilities().solid);
        assert!(!reader.capabilities().random_access);
        // The target of the link is not unpacked for the list.
        let listed_link = &listed[5];
        assert_eq!(listed_link.meta.kind, EntryKind::Symlink);
        assert_eq!(listed_link.meta.link_target, None);
        assert_eq!(listed[1].method.as_deref(), Some("LZMS"));
        assert_eq!(listed[1].packed_size, None);
        assert_eq!(listed[1].meta.size, Some(text.len() as u64));

        let got = read(bytes.clone()).unwrap();
        assert_eq!(
            paths(&got),
            [
                "docs",
                "docs/a.txt",
                "docs/tail.txt",
                "program.bin",
                "head.txt",
                "link.txt",
                "zeros.bin",
                "alone.txt",
            ]
        );
        assert!(got[1].data == text);
        assert!(got[2].data == text[1000..]);
        assert!(got[3].data == program);
        assert!(got[4].data == text[..1000]);
        assert_eq!(
            got[5].entry.meta.link_target.as_deref(),
            Some("../docs/a.txt")
        );
        assert!(got[6].data == vec![0u8; 40_000 - target.len()]);
        assert_eq!(got[6].entry.method.as_deref(), Some("XPRESS"));
        assert_eq!(got[7].data, b"alone");

        // One file from the middle.
        let collector = Collector::default();
        let pick = Selection::paths(["head.txt"]);
        reader.extract(&pick, &collector, &ctx).unwrap();
        let got = collector.in_order();
        assert_eq!(paths(&got), ["head.txt"]);
        assert!(got[0].data == text[..1000]);

        // The same file read as one that knows nothing of pieces kept
        // together: the records of them are records of no data.
        let mut plain = bytes;
        plain[AT_VERSION..AT_VERSION + 4].copy_from_slice(&VERSION.to_le_bytes());
        assert!(read(plain).is_err());
    }

    #[test]
    fn the_images_of_a_file_are_folders() {
        let mut builder = Builder::new(0, 0);
        let one = builder.plain(b"one");
        let two = builder.plain(b"two");
        builder.image(&folder("", vec![file("a.txt", one)]));
        builder.image(&folder(
            "",
            vec![folder("sub", vec![file("b.txt", two)]), file("a.txt", one)],
        ));
        let got = read(builder.finish()).unwrap();
        assert_eq!(
            paths(&got),
            ["1", "1/a.txt", "2", "2/sub", "2/sub/b.txt", "2/a.txt"]
        );
        assert_eq!(got[0].entry.meta.kind, EntryKind::Directory);
        assert_eq!(
            got[0].entry.meta.modified,
            Some(Timestamp::from_seconds(1_700_000_000))
        );
        assert_eq!(got[1].data, b"one");
        assert_eq!(got[4].data, b"two");
        assert_eq!(got[5].data, b"one");

        // More lists than the header counts: the first ones are the images.
        let mut builder = Builder::new(0, 0);
        builder.image(&folder("", vec![file("a.txt", NO_HASH)]));
        builder.image(&folder("", vec![file("b.txt", NO_HASH)]));
        builder.images = 1;
        assert_eq!(paths(&read(builder.finish()).unwrap()), ["a.txt"]);
        // And fewer.
        let mut builder = Builder::new(0, 0);
        builder.image(&folder("", vec![]));
        builder.images = 2;
        assert_eq!(read(builder.finish()).unwrap_err().code, "damaged");
        // No image at all is an empty archive.
        assert_eq!(read(Builder::new(0, 0).finish()).unwrap().len(), 0);
    }

    #[test]
    fn links_and_streams_are_told_apart() {
        let mut builder = Builder::new(0, 0);
        let content = builder.plain(b"content");
        let other = builder.plain(b"another stream");
        let near = builder.plain(&reparse(TAG_SYMLINK, "..\\docs\\a.txt", "shown.txt"));
        let far = builder.plain(&reparse(
            TAG_JUNCTION,
            "\\??\\C:\\Users\\Public",
            "C:\\Users\\Public",
        ));
        let shown = builder.plain(&reparse(TAG_JUNCTION, "", "D:\\shown"));
        let cut = builder.plain(&[1, 2, 3]);
        let packed_on_disk = builder.plain(&[0u8; 16]);
        let unnamed = |hash: Hash| (String::new(), hash);
        builder.image(&folder(
            "",
            vec![
                link("near.txt", TAG_SYMLINK, near),
                Node {
                    attributes: ATTRIBUTE_REPARSE | ATTRIBUTE_DIRECTORY,
                    // What a junction holds is not what it leads to.
                    children: vec![file("inside.txt", content)],
                    ..link("far", TAG_JUNCTION, far)
                },
                link("shown", TAG_JUNCTION, shown),
                link("cut", TAG_SYMLINK, cut),
                link("nowhere", TAG_SYMLINK, NO_HASH),
                // The file leaves its own stream empty and names two more.
                Node {
                    streams: vec![
                        (String::from("notes"), other),
                        unnamed(content),
                        unnamed(other),
                    ],
                    ..file("streams.txt", NO_HASH)
                },
                // Its own stream counts when it holds something.
                Node {
                    streams: vec![(String::from("notes"), other)],
                    ..file("own.txt", content)
                },
                // A reparse point that is no link: a file as any other,
                // its content after the data of the point.
                Node {
                    streams: vec![unnamed(packed_on_disk), unnamed(content)],
                    ..link("other.txt", TAG_OTHER, NO_HASH)
                },
                link("bare.txt", TAG_OTHER, packed_on_disk),
            ],
        ));
        let got = read(builder.finish()).unwrap();
        assert_eq!(
            paths(&got),
            [
                "near.txt",
                "far",
                "shown",
                "cut",
                "nowhere",
                "streams.txt",
                "own.txt",
                "other.txt",
                "bare.txt",
            ]
        );
        let target = |path: &str| {
            let entry = &by_path(&got, path).entry;
            assert_eq!(entry.meta.kind, EntryKind::Symlink, "{path}");
            entry.meta.link_target.clone()
        };
        assert_eq!(target("near.txt").as_deref(), Some("../docs/a.txt"));
        assert_eq!(target("far").as_deref(), Some("C:/Users/Public"));
        assert_eq!(target("shown").as_deref(), Some("D:/shown"));
        assert_eq!(target("cut"), None);
        assert_eq!(target("nowhere"), None);
        assert_eq!(by_path(&got, "streams.txt").data, b"content");
        assert_eq!(by_path(&got, "own.txt").data, b"content");
        let other = by_path(&got, "other.txt");
        assert_eq!(other.entry.meta.kind, EntryKind::File);
        assert_eq!(other.data, b"content");
        assert_eq!(by_path(&got, "bare.txt").data, b"");
    }

    #[test]
    fn what_is_broken_is_said_to_be_broken() {
        let code_of = |bytes: Vec<u8>| read(bytes).unwrap_err().code;
        let big = pattern(5000);
        let build = |change: &dyn Fn(&mut Builder)| {
            let mut builder = Builder::new(0, 0);
            let small = builder.plain(b"small");
            let large = builder.plain(&big);
            builder.image(&folder(
                "",
                vec![file("small.txt", small), file("big.bin", large)],
            ));
            change(&mut builder);
            builder.finish()
        };
        let whole = build(&|_| {});
        assert_eq!(read(whole.clone()).unwrap().len(), 2);

        // Not an image, and not all of the header of one.
        let mut other = whole.clone();
        other[0] = b'X';
        assert_eq!(code_of(other), "damaged");
        assert_eq!(code_of(whole[..HEADER - 1].to_vec()), "truncated");
        assert_eq!(code_of(Vec::new()), "truncated");
        // What is not read: other versions, and images split into files.
        assert_eq!(
            code_of(build(&|builder| builder.version = 0x1_0C00)),
            "unsupportedArchive"
        );
        assert_eq!(
            code_of(build(&|builder| builder.parts = 2)),
            "unsupportedArchive"
        );
        assert_eq!(
            code_of(build(&|builder| builder.flags = FLAG_SPANNED)),
            "unsupportedArchive"
        );
        // Compressed, and nothing says how.
        assert_eq!(
            code_of(build(&|builder| builder.flags = FLAG_COMPRESSED)),
            "damaged"
        );
        // The table of pieces is past the end of the file.
        assert_eq!(code_of(whole[..whole.len() - 1].to_vec()), "truncated");

        // A byte of a file is another one: the file is named.
        let mut flipped = whole.clone();
        flipped[HEADER + 5 + 100] ^= 1;
        let error = read(flipped).unwrap_err();
        assert_eq!(error.code, "damagedEntry");
        assert_eq!(error.params["name"], "big.bin");
        // A byte of the list is: nothing of it is believed.
        let mut flipped = whole.clone();
        flipped[HEADER + 5 + 5000 + 40] ^= 1;
        assert_eq!(code_of(flipped), "damaged");

        // A file whose data is not in the image is listed, and not read.
        let missing = {
            let mut builder = Builder::new(0, 0);
            builder.image(&folder("", vec![file("lost.txt", sha(b"lost"))]));
            builder.finish()
        };
        let mut reader = open(missing);
        let ctx = OpContext::silent();
        let listed = reader.entries(&ctx).unwrap();
        assert_eq!(listed[0].meta.size, None);
        let error = reader
            .extract(&Selection::All, &Collector::default(), &ctx)
            .unwrap_err();
        assert_eq!(error.code, "damagedEntry");
        assert_eq!(error.params["name"], "lost.txt");

        // A file the file system encrypted.
        let locked = {
            let mut builder = Builder::new(0, 0);
            let data = builder.plain(b"sealed");
            builder.image(&folder(
                "",
                vec![Node {
                    attributes: ATTRIBUTE_ENCRYPTED,
                    ..file("sealed.txt", data)
                }],
            ));
            builder.finish()
        };
        assert_eq!(code_of(locked), "unsupportedMethod");
    }

    /// An image whose list is these bytes.
    fn with_list(list: &[u8]) -> Vec<u8> {
        let mut builder = Builder::new(0, 0);
        builder.piece(list, None, PIECE_LIST);
        builder.images = 1;
        builder.finish()
    }

    #[test]
    fn a_broken_list_is_not_followed() {
        let root = folder(
            "",
            vec![
                folder("a", vec![file("inner.txt", NO_HASH)]),
                file("b.txt", NO_HASH),
            ],
        );
        let whole = list(&root);
        assert_eq!(
            paths(&read(with_list(&whole)).unwrap()),
            ["a", "a/inner.txt", "b.txt"]
        );
        let code_of = |list: &[u8]| read(with_list(list)).unwrap_err().code;
        // The list of the innermost folder comes first, after the root:
        // the root, the end of its level, then `inner.txt`.
        let root_length = DENTRY.next_multiple_of(8);
        let inner = 8 + root_length + 8;
        // A file longer than the list, and one shorter than any.
        let mut long = whole.clone();
        long[inner..inner + 8].copy_from_slice(&100_000u64.to_le_bytes());
        assert_eq!(code_of(&long), "damaged");
        let mut short = whole.clone();
        short[inner..inner + 8].copy_from_slice(&50u64.to_le_bytes());
        assert_eq!(code_of(&short), "damaged");
        // A name that runs out of its file.
        let mut name = whole.clone();
        name[inner + 100..inner + 102].copy_from_slice(&500u16.to_le_bytes());
        assert_eq!(code_of(&name), "damaged");
        // Streams that are not there.
        let mut streams = whole.clone();
        let last = whole.len() - 8 - (DENTRY + 12).next_multiple_of(8);
        streams[last + 96..last + 98].copy_from_slice(&3u16.to_le_bytes());
        assert_eq!(code_of(&streams), "damaged");
        // Nothing where the root should be, and no list at all.
        assert_eq!(code_of(&whole[..8]), "damaged");
        assert_eq!(code_of(&[]), "damaged");
        assert_eq!(
            code_of(&[8, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0]),
            "damaged"
        );
        // The descriptions before the root are said to be longer than the
        // list.
        let mut far = whole.clone();
        far[..4].copy_from_slice(&u32::MAX.to_le_bytes());
        assert_eq!(code_of(&far), "damaged");

        // A folder that holds itself is listed once: `a` is said to hold
        // the level it is on.
        let mut inside = whole.clone();
        let level =
            whole.len() - 8 - (DENTRY + 12).next_multiple_of(8) - (DENTRY + 4).next_multiple_of(8);
        let at = level as u64;
        inside[level + 16..level + 24].copy_from_slice(&at.to_le_bytes());
        assert_eq!(paths(&read(with_list(&inside)).unwrap()), ["a", "b.txt"]);

        // Short names in deep folders make long paths: the memory they may
        // take is counted.
        let mut deep = folder("x".repeat(200).as_str(), vec![]);
        for _ in 0..60 {
            deep = folder("x".repeat(200).as_str(), vec![deep]);
        }
        let bytes = with_list(&list(&folder("", vec![deep])));
        let mut reader = open(bytes.clone());
        assert_eq!(reader.entries(&OpContext::silent()).unwrap().len(), 61);
        let tight = OpContext {
            budget: Budget {
                threads: 1,
                memory: 100_000,
            },
            ..OpContext::silent()
        };
        let mut reader = open(bytes);
        assert_eq!(reader.entries(&tight).unwrap_err().code, "notEnoughMemory");
    }

    #[test]
    fn broken_chunks_name_their_file() {
        let text = lines(700);
        let even = pattern(32768);
        let two = [&even[..], &text[..]].concat();
        let chunks = vec![
            include_bytes!("../fixtures/pattern.xpress").to_vec(),
            include_bytes!("../fixtures/text.xpress").to_vec(),
        ];
        let build = |chunk: u32, change: &dyn Fn(&mut Vec<u8>)| {
            let mut builder = Builder::new(FLAG_COMPRESSED | FLAG_XPRESS, chunk);
            let hash = builder.packed(&two, &chunks);
            change(&mut builder.bytes);
            builder.image(&folder("", vec![file("two.bin", hash)]));
            builder.finish()
        };
        assert_eq!(read(build(32768, &|_| {})).unwrap().len(), 1);
        let failure = |bytes: Vec<u8>| {
            let error = read(bytes).unwrap_err();
            if error.code == "damagedEntry" {
                assert_eq!(error.params["name"], "two.bin");
            }
            error.code
        };
        // The second chunk starts before the first ends, or after the end
        // of the piece.
        let table = |value: u32| {
            move |bytes: &mut Vec<u8>| {
                bytes[HEADER..HEADER + 4].copy_from_slice(&value.to_le_bytes());
            }
        };
        assert_eq!(failure(build(32768, &table(u32::MAX))), "damagedEntry");
        assert_eq!(failure(build(32768, &table(100_000))), "damagedEntry");
        // It starts elsewhere: the first chunk is not what it was.
        assert_eq!(failure(build(32768, &table(700))), "damagedEntry");
        // A byte of a chunk is another one.
        let flip = |bytes: &mut Vec<u8>| bytes[HEADER + 4 + 600] ^= 0x10;
        assert_eq!(failure(build(32768, &flip)), "damagedEntry");
        // Chunks of no size, and of a size that leaves no table.
        assert_eq!(failure(build(0, &|_| {})), "damagedEntry");
        assert_eq!(failure(build(1 << 20, &|_| {})), "damagedEntry");
        // The piece is cut off.
        let cut = |bytes: &mut Vec<u8>| bytes.truncate(HEADER + 2000);
        assert!(read(build(32768, &cut)).is_err());

        // A chunk larger than the memory there is for it.
        let bytes = build(32768, &|_| {});
        let tight = OpContext {
            budget: Budget {
                threads: 1,
                memory: 40_000,
            },
            ..OpContext::silent()
        };
        let mut reader = open(bytes);
        reader.entries(&tight).unwrap();
        let error = reader
            .extract(&Selection::All, &Collector::default(), &tight)
            .unwrap_err();
        assert_eq!(error.code, "notEnoughMemory");

        // LZX has no chunks of every size.
        let mut builder = Builder::new(FLAG_COMPRESSED | FLAG_LZX, 1 << 23);
        let hash = builder.packed(&text, &lzx_chunks(&text, 32768));
        builder.image(&folder("", vec![file("text.txt", hash)]));
        assert_eq!(read(builder.finish()).unwrap_err().code, "damagedEntry");
    }

    #[test]
    fn broken_pieces_kept_together_are_told() {
        let target = reparse(TAG_SYMLINK, "a.txt", "a.txt");
        let (small, packed) = squeezed(&target, 40_000);
        let build = |format: u32, cuts: &[usize], change: &dyn Fn(&mut Builder)| {
            let mut builder = Builder::new(FLAG_COMPRESSED | FLAG_LZMS, 1 << 16);
            builder.version = VERSION_SOLID;
            let hashes = builder.together(&[(format, 1 << 16, &small, vec![packed.clone()])], cuts);
            change(&mut builder);
            builder.image(&folder(
                "",
                vec![
                    link("link", TAG_SYMLINK, hashes[0]),
                    file("zeros.bin", hashes[1]),
                ],
            ));
            builder.finish()
        };
        let cuts = [target.len()];
        assert_eq!(read(build(1, &cuts, &|_| {})).unwrap().len(), 2);
        // A compression that is none.
        let error = read(build(9, &cuts, &|_| {})).unwrap_err();
        assert_eq!(error.code, "unsupportedMethod");
        // Not the compression it was written with.
        let error = read(build(3, &cuts, &|_| {})).unwrap_err();
        assert_eq!(error.code, "damagedEntry");
        // A piece that is said to lie past the end of what it is in.
        let outside = |builder: &mut Builder| {
            let record = builder.table.len() - RECORD;
            builder.table[record + 8..record + 16].copy_from_slice(&1_000_000u64.to_le_bytes());
        };
        assert_eq!(read(build(1, &cuts, &outside)).unwrap_err().code, "damaged");
        // The sizes of the chunks say more than there is.
        let sizes = |builder: &mut Builder| {
            let at = HEADER + SOLID_HEADER as usize;
            builder.bytes[at..at + 4].copy_from_slice(&1_000_000u32.to_le_bytes());
        };
        let error = read(build(1, &cuts, &sizes)).unwrap_err();
        assert_eq!(error.code, "damagedEntry");
    }

    proptest! {
        #![proptest_config(ProptestConfig::with_cases(64))]

        #[test]
        fn no_change_to_an_image_makes_it_panic(
            changes in proptest::collection::vec((any::<prop::sample::Index>(), any::<u8>()), 1..6),
            solid in any::<bool>(),
        ) {
            let target = reparse(TAG_SYMLINK, "a.txt", "a.txt");
            let (small, packed) = squeezed(&target, 9_000);
            let mut builder = Builder::new(FLAG_COMPRESSED | FLAG_XPRESS, 1 << 15);
            let hashes = if solid {
                builder.version = VERSION_SOLID;
                builder.together(&[(1, 1 << 15, &small, vec![packed])], &[target.len()])
            } else {
                vec![builder.plain(&target), builder.packed(&small, &[packed])]
            };
            builder.image(&folder(
                "",
                vec![
                    link("link", TAG_SYMLINK, hashes[0]),
                    folder("sub", vec![file("zeros.bin", hashes[1])]),
                ],
            ));
            let mut bytes = builder.finish();
            for (at, value) in changes {
                let at = at.index(bytes.len());
                bytes[at] = value;
            }
            let _ = read(bytes);
        }
    }
}

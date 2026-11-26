//! CAB, the cabinet of Windows: the archive of its installers and updates.
//!
//! The files of a cabinet are gathered into folders, and a folder is
//! compressed as one stream cut into blocks of 32 KiB: a file is reached by
//! decoding its folder up to it. The methods are MSZIP, which is Deflate
//! that remembers the block before, LZX and the rare Quantum.

use std::borrow::Cow;
use std::io::{self, BufRead, BufReader, Read, Seek, SeekFrom};

use boxit_core::{
    ArchiveReader, Capabilities, Entry, EntryMeta, EntrySink, Error, Method, OpContext, Result,
    Selection, Timestamp,
};
use flate2::{Decompress, FlushDecompress};

use crate::bytes::{le16, le32};
use crate::lzx::{Lzx, FRAME};
use crate::names::NameDecoder;
use crate::source::{Source, SourceReader};
use crate::stored::cut_short;
use crate::support::{damaged, from_local, unsupported_method, Civil, Watched};
use crate::OpenOptions;

const SIGNATURE: &[u8; 4] = b"MSCF";
/// The header of a cabinet without what its flags add.
const HEADER: usize = 36;
const FOLDER_HEADER: usize = 8;
const FILE_HEADER: usize = 16;
const BLOCK_HEADER: usize = 8;

/// The cabinet is one of a set and something comes before or after it.
const FLAG_PREVIOUS: u16 = 1;
const FLAG_NEXT: u16 = 2;
/// Room is set aside in the headers for whoever made the cabinet.
const FLAG_RESERVE: u16 = 4;

/// The folder of a file whose content starts or ends in another cabinet.
const FOLDER_CONTINUED: u16 = 0xFFFD;

const METHOD_MASK: u16 = 0x000F;
const METHOD_STORED: u16 = 0;
const METHOD_MSZIP: u16 = 1;
const METHOD_QUANTUM: u16 = 2;
const METHOD_LZX: u16 = 3;

/// The name of a file is UTF-8, not of a code page.
const ATTRIBUTE_UTF_NAME: u16 = 0x80;
/// Read-only, hidden, system and archive: what Windows knows of the bits.
const ATTRIBUTES_KEPT: u16 = 0x27;

/// The longest name of a file or of a neighbour of the cabinet, with the
/// zero that ends it. The format says 256; nothing is lost by reading more.
const MAX_NAME: u64 = 1024;
/// The most a block may take compressed: LZX may grow what it cannot
/// compress.
const MAX_PACKED: usize = FRAME + 6144;
/// What a block of MSZIP starts with.
const MSZIP_MARK: &[u8; 2] = b"CK";

/// How a folder is compressed.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Packing {
    Stored,
    MsZip,
    Quantum,
    /// With a window of this power of two.
    Lzx(u32),
}

impl Packing {
    fn of(kind: u16) -> Result<Self> {
        Ok(match kind & METHOD_MASK {
            METHOD_STORED => Self::Stored,
            METHOD_MSZIP => Self::MsZip,
            METHOD_QUANTUM => Self::Quantum,
            METHOD_LZX => Self::Lzx(u32::from(kind >> 8) & 0x1F),
            other => return Err(unsupported_method(format!("CAB {other}"))),
        })
    }

    fn name(self) -> Cow<'static, str> {
        match self {
            Self::Stored => Cow::Borrowed(Method::Store.name()),
            Self::MsZip => Cow::Borrowed("MSZIP"),
            Self::Quantum => Cow::Borrowed("Quantum"),
            Self::Lzx(order) => Cow::Owned(format!("LZX:{order}")),
        }
    }
}

#[derive(Clone, Copy, Debug)]
struct Folder {
    /// Where its first block is in the file.
    first: u64,
    blocks: u16,
    packing: Packing,
}

/// Where the content of a file is.
#[derive(Clone, Copy, Debug)]
struct Place {
    folder: usize,
    /// From the start of the folder as it is when decompressed.
    offset: u64,
    size: u64,
}

struct Loaded {
    entries: Vec<Entry>,
    places: Vec<Place>,
    folders: Vec<Folder>,
    /// The room set aside in the header of every block.
    reserve: usize,
}

/// The checksum of a block: its words folded together, what is left of the
/// last one read the other way round.
fn checksum(data: &[u8], seed: u32) -> u32 {
    let (words, rest) = data.as_chunks::<4>();
    let mut sum = seed;
    for word in words {
        sum ^= u32::from_le_bytes(*word);
    }
    let rest = rest
        .iter()
        .fold(0u32, |rest, byte| rest << 8 | u32::from(*byte));
    sum ^ rest
}

/// The date and the time of DOS, of the clock on the wall where the file
/// was.
fn dos_time(date: u16, time: u16) -> Option<Timestamp> {
    if date == 0 {
        return None;
    }
    from_local(Civil {
        year: 1980 + i32::from(date >> 9),
        month: u32::from(date >> 5) & 0xF,
        day: u32::from(date) & 0x1F,
        hour: u32::from(time >> 11),
        minute: u32::from(time >> 5) & 0x3F,
        second: (u32::from(time) & 0x1F) * 2,
    })
}

/// Reads text that ends with a zero, without the zero.
fn text(reader: &mut impl BufRead) -> Result<Vec<u8>> {
    let mut bytes = Vec::new();
    reader.by_ref().take(MAX_NAME).read_until(0, &mut bytes)?;
    match bytes.pop() {
        Some(0) => Ok(bytes),
        Some(_) if bytes.len() as u64 + 1 == MAX_NAME => Err(damaged("a name without an end")),
        _ => Err(cut_short()),
    }
}

fn load(source: &Source, options: &OpenOptions, ctx: &OpContext) -> Result<Loaded> {
    let mut reader = BufReader::new(source.reader()?);
    let mut head = [0u8; HEADER];
    reader.read_exact(&mut head)?;
    if &head[..4] != SIGNATURE {
        return Err(damaged("not a cabinet"));
    }
    let number = |at: usize| u32::from(le16(&head, at).unwrap_or(0));
    let files_at = u64::from(le32(&head, 16).unwrap_or(0));
    let (folder_count, file_count) = (number(26) as usize, number(28) as usize);
    let flags = number(30) as u16;

    let mut folder_reserve = 0usize;
    let mut reserve = 0usize;
    if flags & FLAG_RESERVE != 0 {
        let mut sizes = [0u8; 4];
        reader.read_exact(&mut sizes)?;
        folder_reserve = usize::from(sizes[2]);
        reserve = usize::from(sizes[3]);
        let skipped = i64::from(le16(&sizes, 0).unwrap_or(0));
        reader.seek_relative(skipped)?;
    }
    for flag in [FLAG_PREVIOUS, FLAG_NEXT] {
        if flags & flag != 0 {
            // The name of the neighbour and of its disk.
            text(&mut reader)?;
            text(&mut reader)?;
        }
    }

    let mut folders = Vec::with_capacity(folder_count);
    let mut record = vec![0u8; FOLDER_HEADER + folder_reserve];
    for _ in 0..folder_count {
        reader.read_exact(&mut record)?;
        folders.push(Folder {
            first: u64::from(le32(&record, 0).unwrap_or(0)),
            blocks: le16(&record, 4).unwrap_or(0),
            packing: Packing::of(le16(&record, 6).unwrap_or(0))?,
        });
    }

    reader.seek(SeekFrom::Start(files_at))?;
    let mut places = Vec::with_capacity(file_count);
    let mut raw = Vec::with_capacity(file_count);
    for index in 0..file_count {
        ctx.checkpoint()?;
        let mut record = [0u8; FILE_HEADER];
        reader.read_exact(&mut record)?;
        let name = text(&mut reader)?;
        let size = u64::from(le32(&record, 0).unwrap_or(0));
        let folder = le16(&record, 8).unwrap_or(0);
        if folder >= FOLDER_CONTINUED {
            // Its content is spread over the cabinets of a set.
            return Err(
                Error::new("unsupportedArchive").detail("a file continues in another cabinet")
            );
        }
        let folder = usize::from(folder);
        let Some(packing) = folders.get(folder).map(|folder| folder.packing) else {
            return Err(damaged("a file of a folder the cabinet does not have"));
        };
        let attributes = le16(&record, 14).unwrap_or(0);
        let kept = u32::from(attributes & ATTRIBUTES_KEPT);
        places.push(Place {
            folder,
            offset: u64::from(le32(&record, 4).unwrap_or(0)),
            size,
        });
        let entry = Entry {
            index: index as u64,
            meta: EntryMeta {
                size: Some(size),
                modified: dos_time(
                    le16(&record, 10).unwrap_or(0),
                    le16(&record, 12).unwrap_or(0),
                ),
                windows_attributes: (kept != 0).then_some(kept),
                ..EntryMeta::file("", size)
            },
            method: Some(packing.name()),
            ..Entry::default()
        };
        raw.push((name, attributes & ATTRIBUTE_UTF_NAME != 0, entry));
    }

    let names = NameDecoder::new(
        options.encoding,
        raw.iter()
            .filter(|(_, utf8, _)| !utf8)
            .map(|(name, _, _)| name.as_slice()),
        options.hint.as_deref(),
    );
    let entries = raw
        .into_iter()
        .map(|(name, utf8, mut entry)| {
            entry.meta.path = names.decode(&name, utf8).replace('\\', "/");
            entry
        })
        .collect();
    Ok(Loaded {
        entries,
        places,
        folders,
        reserve,
    })
}

/// What turns the blocks of a folder back into its data.
enum Unpacker {
    Stored,
    /// Deflate; every block after the first starts from the one before.
    MsZip {
        inflate: Box<Decompress>,
        fresh: bool,
    },
    Lzx(Box<Lzx>),
}

/// The data of a folder, decompressed block after block.
struct FolderData<'a> {
    reader: SourceReader,
    /// How many blocks have not been read.
    blocks: u16,
    reserve: usize,
    unpacker: Unpacker,
    /// The block at hand, and how much of it has been given.
    block: Vec<u8>,
    given: usize,
    /// Where in the folder what is given next is.
    position: u64,
    packed: Vec<u8>,
    spare: Vec<u8>,
    ctx: &'a OpContext,
}

impl<'a> FolderData<'a> {
    fn open(source: &Source, folder: &Folder, reserve: usize, ctx: &'a OpContext) -> Result<Self> {
        let unpacker = match folder.packing {
            Packing::Stored => Unpacker::Stored,
            Packing::MsZip => Unpacker::MsZip {
                inflate: Box::new(Decompress::new(false)),
                fresh: true,
            },
            Packing::Lzx(order) => match Lzx::cabinet(order) {
                Some(lzx) => Unpacker::Lzx(Box::new(lzx)),
                None => return Err(damaged(format!("LZX with a window of 2^{order}"))),
            },
            Packing::Quantum => return Err(unsupported_method(folder.packing.name())),
        };
        let mut reader = source.reader()?;
        if folder.first > source.size() {
            return Err(cut_short());
        }
        reader.seek(SeekFrom::Start(folder.first))?;
        Ok(Self {
            reader,
            blocks: folder.blocks,
            reserve,
            unpacker,
            block: Vec::new(),
            given: 0,
            position: 0,
            packed: Vec::new(),
            spare: Vec::new(),
            ctx,
        })
    }

    /// Reads and decompresses the next block. `false` when the folder has
    /// no more.
    fn fill(&mut self) -> Result<bool> {
        if self.blocks == 0 {
            return Ok(false);
        }
        self.ctx.checkpoint()?;
        self.blocks -= 1;
        let mut head = vec![0u8; BLOCK_HEADER + self.reserve];
        self.reader.read_exact(&mut head)?;
        let sum = le32(&head, 0).unwrap_or(0);
        let packed = usize::from(le16(&head, 4).unwrap_or(0));
        let size = usize::from(le16(&head, 6).unwrap_or(0));
        if size == 0 {
            return Err(
                Error::new("unsupportedArchive").detail("a block continues in another cabinet")
            );
        }
        if size > FRAME || packed > MAX_PACKED {
            return Err(damaged("a block larger than a cabinet has"));
        }
        self.packed.resize(packed, 0);
        self.reader.read_exact(&mut self.packed)?;
        // No checksum is written as zero.
        if sum != 0 && checksum(&head[4..], checksum(&self.packed, 0)) != sum {
            return Err(damaged("the checksum of a block does not match"));
        }

        let mut next = std::mem::take(&mut self.spare);
        next.clear();
        next.resize(size, 0);
        match &mut self.unpacker {
            Unpacker::Stored => {
                if packed != size {
                    return Err(damaged("a stored block of two sizes"));
                }
                next.copy_from_slice(&self.packed);
            }
            Unpacker::MsZip { inflate, fresh } => {
                let Some(deflated) = self.packed.strip_prefix(MSZIP_MARK) else {
                    return Err(damaged("a block of MSZIP without its mark"));
                };
                if !*fresh {
                    inflate.reset(false);
                    inflate
                        .set_dictionary(&self.block)
                        .map_err(|error| damaged(format!("MSZIP: {error}")))?;
                }
                *fresh = false;
                let before = inflate.total_out();
                inflate
                    .decompress(deflated, &mut next, FlushDecompress::Finish)
                    .map_err(|error| damaged(format!("MSZIP: {error}")))?;
                if inflate.total_out() - before != size as u64 {
                    return Err(damaged("MSZIP: a block shorter than it says"));
                }
            }
            Unpacker::Lzx(lzx) => lzx.decode(&self.packed, &mut next).map_err(damaged)?,
        }
        self.spare = std::mem::replace(&mut self.block, next);
        self.given = 0;
        Ok(true)
    }

    /// Goes on to `offset` of the folder, which is not behind.
    fn skip_to(&mut self, offset: u64) -> Result<()> {
        while self.position < offset {
            if self.given == self.block.len() && !self.fill()? {
                return Err(cut_short());
            }
            let step = ((self.block.len() - self.given) as u64).min(offset - self.position);
            self.given += step as usize;
            self.position += step;
        }
        Ok(())
    }

    fn read(&mut self, buffer: &mut [u8]) -> Result<usize> {
        if self.given == self.block.len() && !self.fill()? {
            return Ok(0);
        }
        let count = buffer.len().min(self.block.len() - self.given);
        buffer[..count].copy_from_slice(&self.block[self.given..self.given + count]);
        self.given += count;
        self.position += count as u64;
        Ok(count)
    }
}

/// The content of one file of a folder.
struct FileData<'s, 'a> {
    folder: &'s mut FolderData<'a>,
    start: u64,
    left: u64,
}

impl Read for FileData<'_, '_> {
    fn read(&mut self, buffer: &mut [u8]) -> io::Result<usize> {
        if self.left == 0 || buffer.is_empty() {
            return Ok(0);
        }
        self.folder.skip_to(self.start).map_err(Error::into_io)?;
        let want = buffer
            .len()
            .min(usize::try_from(self.left).unwrap_or(usize::MAX));
        let count = self
            .folder
            .read(&mut buffer[..want])
            .map_err(Error::into_io)?;
        if count == 0 {
            return Err(io::ErrorKind::UnexpectedEof.into());
        }
        self.left -= count as u64;
        Ok(count)
    }
}

/// A cabinet opened for reading.
pub struct CabReader {
    source: Source,
    options: OpenOptions,
    loaded: Option<Loaded>,
}

impl CabReader {
    pub fn new(source: Source, options: OpenOptions) -> Self {
        Self {
            source,
            options,
            loaded: None,
        }
    }

    fn load(&mut self, ctx: &OpContext) -> Result<&Loaded> {
        if self.loaded.is_none() {
            self.loaded = Some(load(&self.source, &self.options, ctx)?);
        }
        self.loaded
            .as_ref()
            .ok_or_else(|| Error::new("internal").detail("the list of a cabinet is gone"))
    }
}

impl ArchiveReader for CabReader {
    fn capabilities(&self) -> Capabilities {
        Capabilities {
            random_access: false,
            solid: true,
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
        // Folder by folder, and within a folder in the order of the data:
        // one pass over a folder gives all of its files.
        let mut wanted: Vec<usize> = (0..loaded.entries.len())
            .filter(|index| pick.contains(&loaded.entries[*index]))
            .collect();
        wanted.sort_by_key(|index| (loaded.places[*index].folder, loaded.places[*index].offset));
        let mut open: Option<(usize, FolderData)> = None;
        for index in wanted {
            ctx.checkpoint()?;
            let (entry, place) = (&loaded.entries[index], loaded.places[index]);
            if place.size == 0 {
                sink.accept(entry, &mut io::empty())?;
                continue;
            }
            // A file that starts before the end of the one before it is
            // read by going through the folder once more.
            let usable = open.as_ref().is_some_and(|(folder, data)| {
                *folder == place.folder && data.position <= place.offset
            });
            if !usable {
                let folder = &loaded.folders[place.folder];
                open = Some((
                    place.folder,
                    FolderData::open(source, folder, loaded.reserve, ctx)?,
                ));
            }
            let Some((_, folder)) = open.as_mut() else {
                continue;
            };
            let mut data = Watched::new(FileData {
                folder,
                start: place.offset,
                left: place.size,
            });
            sink.accept(entry, &mut data)
                .map_err(|error| data.settle(entry, error))?;
        }
        Ok(())
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use flate2::{Compress, Compression, FlushCompress, Status};

    use super::*;
    use crate::lzx::tests::{sample, Kind, Packer};
    use crate::names::NameEncoding;
    use crate::tests::Collector;

    /// A file of a test cabinet: its name, its attributes and its content.
    pub(crate) type Item<'a> = (&'a [u8], u16, &'a [u8]);

    pub(crate) fn item<'a>(name: &'a [u8], attributes: u16, content: &'a [u8]) -> Item<'a> {
        (name, attributes, content)
    }

    /// A folder of a test cabinet: how it is compressed and its files.
    pub(crate) struct Group<'a> {
        pub(crate) kind: u16,
        pub(crate) files: Vec<Item<'a>>,
    }

    pub(crate) const STORED: u16 = METHOD_STORED;
    pub(crate) const MSZIP: u16 = METHOD_MSZIP;
    pub(crate) const LZX_16: u16 = METHOD_LZX | 16 << 8;

    /// The date and the time of DOS for 2023-11-14 12:30:10.
    const DATE: u16 = ((2023 - 1980) << 9) + (11 << 5) + 14;
    const TIME: u16 = (12 << 11) + (30 << 5) + 5;

    /// The blocks of a folder: compressed, and how long when not.
    fn blocks(kind: u16, data: &[u8]) -> Vec<(Vec<u8>, usize)> {
        let pieces = data.chunks(FRAME);
        match kind & METHOD_MASK {
            METHOD_MSZIP => {
                let mut before: &[u8] = &[];
                pieces
                    .map(|piece| {
                        let mut deflate = Compress::new(Compression::default(), false);
                        if !before.is_empty() {
                            deflate.set_dictionary(before).unwrap();
                        }
                        let mut packed = Vec::with_capacity(piece.len() + 1024);
                        packed.extend(MSZIP_MARK);
                        let status = deflate
                            .compress_vec(piece, &mut packed, FlushCompress::Finish)
                            .unwrap();
                        assert_eq!(status, Status::StreamEnd);
                        before = piece;
                        (packed, piece.len())
                    })
                    .collect()
            }
            METHOD_LZX => {
                let layout = [
                    (Kind::Verbatim, 40_000),
                    (Kind::Uncompressed, 999),
                    (Kind::Aligned, 32768),
                ];
                let packed = Packer::cabinet(u32::from(kind >> 8), 0).pack(data, &layout);
                packed
                    .into_iter()
                    .zip(pieces)
                    .map(|(packed, piece)| (packed, piece.len()))
                    .collect()
            }
            _ => pieces.map(|piece| (piece.to_vec(), piece.len())).collect(),
        }
    }

    /// A cabinet of these folders. `reserve` is the room set aside in the
    /// header, in every folder and in every block; no room at all when all
    /// three are zero.
    pub(crate) fn cabinet(groups: &[Group], reserve: [u8; 3]) -> Vec<u8> {
        let reserved = reserve != [0; 3];
        let mut head = vec![0u8; HEADER];
        head[..4].copy_from_slice(SIGNATURE);
        head[24..26].copy_from_slice(&[3, 1]);
        head[26..28].copy_from_slice(&(groups.len() as u16).to_le_bytes());
        let count: usize = groups.iter().map(|group| group.files.len()).sum();
        head[28..30].copy_from_slice(&(count as u16).to_le_bytes());
        if reserved {
            head[30] = FLAG_RESERVE as u8;
            head.extend(u16::from(reserve[0]).to_le_bytes());
            head.extend([reserve[1], reserve[2]]);
            head.extend(vec![0xEE; usize::from(reserve[0])]);
        }

        let mut files = Vec::new();
        let mut data = Vec::new();
        let mut starts = Vec::new();
        for (index, group) in groups.iter().enumerate() {
            let mut whole = Vec::new();
            for (name, attributes, content) in &group.files {
                files.extend((content.len() as u32).to_le_bytes());
                files.extend((whole.len() as u32).to_le_bytes());
                files.extend((index as u16).to_le_bytes());
                files.extend(DATE.to_le_bytes());
                files.extend(TIME.to_le_bytes());
                files.extend(attributes.to_le_bytes());
                files.extend(*name);
                files.push(0);
                whole.extend(*content);
            }
            let blocks = blocks(group.kind, &whole);
            starts.push((data.len(), blocks.len()));
            for (packed, size) in blocks {
                let mut block = vec![0u8; 4];
                block.extend((packed.len() as u16).to_le_bytes());
                block.extend((size as u16).to_le_bytes());
                block.extend(vec![0xDD; usize::from(reserve[2])]);
                let sum = checksum(&block[4..], checksum(&packed, 0));
                block[..4].copy_from_slice(&sum.to_le_bytes());
                data.extend(block);
                data.extend(packed);
            }
        }

        let folder_size = FOLDER_HEADER + usize::from(reserve[1]);
        let files_at = head.len() + groups.len() * folder_size;
        let data_at = files_at + files.len();
        head[16..20].copy_from_slice(&(files_at as u32).to_le_bytes());
        let mut bytes = head;
        for (group, (start, count)) in groups.iter().zip(starts) {
            bytes.extend(((data_at + start) as u32).to_le_bytes());
            bytes.extend((count as u16).to_le_bytes());
            bytes.extend(group.kind.to_le_bytes());
            bytes.extend(vec![0xFF; usize::from(reserve[1])]);
        }
        bytes.extend(files);
        bytes.extend(data);
        let size = bytes.len() as u32;
        bytes[8..12].copy_from_slice(&size.to_le_bytes());
        bytes
    }

    fn read_with(bytes: Vec<u8>, options: OpenOptions) -> Result<Vec<(Entry, Vec<u8>)>> {
        let ctx = OpContext::silent();
        let mut reader = CabReader::new(Source::memory("a.cab", bytes), options);
        let listed = reader.entries(&ctx)?;
        let sink = Collector::default();
        reader.extract(&Selection::All, &sink, &ctx)?;
        let mut got = sink.in_order();
        got.sort_by_key(|got| got.entry.index);
        assert_eq!(
            listed,
            got.iter().map(|got| got.entry.clone()).collect::<Vec<_>>()
        );
        Ok(got.into_iter().map(|got| (got.entry, got.data)).collect())
    }

    fn read(bytes: Vec<u8>) -> Result<Vec<(Entry, Vec<u8>)>> {
        read_with(bytes, OpenOptions::default())
    }

    #[test]
    fn every_method_gives_the_files_back() {
        let large = sample(100_000, 11);
        let small = sample(3000, 12);
        for kind in [STORED, MSZIP, LZX_16, METHOD_LZX | 21 << 8] {
            for reserve in [[0, 0, 0], [20, 4, 3]] {
                let bytes = cabinet(
                    &[
                        Group {
                            kind,
                            files: vec![
                                item(b"docs\\read me.txt", 0x20, &small),
                                item(b"empty.bin", 0, b""),
                                item(b"large.bin", 0x21, &large),
                                item(b"again.bin", 0, &small),
                            ],
                        },
                        Group {
                            kind: STORED,
                            files: vec![item(b"last.txt", 0x02, b"the last one")],
                        },
                    ],
                    reserve,
                );
                let got = read(bytes).unwrap();
                let summary: Vec<_> = got
                    .iter()
                    .map(|(entry, data)| (entry.meta.path.as_str(), data.as_slice()))
                    .collect();
                assert!(
                    summary
                        == [
                            ("docs/read me.txt", small.as_slice()),
                            ("empty.bin", b""),
                            ("large.bin", large.as_slice()),
                            ("again.bin", small.as_slice()),
                            ("last.txt", b"the last one"),
                        ],
                    "method {kind:#x}, reserve {reserve:?}"
                );
                let entry = &got[2].0;
                assert_eq!(entry.meta.size, Some(100_000));
                assert_eq!(entry.meta.windows_attributes, Some(0x21));
                assert_eq!(got[1].0.meta.windows_attributes, None);
                assert_eq!(
                    entry.meta.modified,
                    from_local(Civil {
                        year: 2023,
                        month: 11,
                        day: 14,
                        hour: 12,
                        minute: 30,
                        second: 10
                    })
                );
                let method = match kind {
                    STORED => "Store",
                    MSZIP => "MSZIP",
                    LZX_16 => "LZX:16",
                    _ => "LZX:21",
                };
                assert_eq!(entry.method.as_deref(), Some(method));
                assert_eq!(got[4].0.method.as_deref(), Some("Store"));
            }
        }
    }

    #[test]
    fn only_what_is_picked_is_read_in_whatever_order_it_lies() {
        let first = sample(50_000, 1);
        let second = sample(40_000, 2);
        let mut bytes = cabinet(
            &[Group {
                kind: MSZIP,
                files: vec![
                    item(b"first", 0, &first),
                    item(b"second", 0, &second),
                    item(b"third", 0, b"third"),
                ],
            }],
            [0; 3],
        );
        let ctx = OpContext::silent();
        let mut reader = CabReader::new(
            Source::memory("a.cab", bytes.clone()),
            OpenOptions::default(),
        );
        let sink = Collector::default();
        let pick = Selection::Paths(vec![String::from("third"), String::from("second")]);
        reader.extract(&pick, &sink, &ctx).unwrap();
        let got = sink.in_order();
        assert_eq!(got.len(), 2);
        assert_eq!(got[0].entry.meta.path, "second");
        assert!(got[0].data == second);
        assert_eq!(got[1].data, b"third");

        // Two files that share their bytes: the second one starts where
        // the first did.
        let files_at = le32(&bytes, 16).unwrap() as usize;
        let second_at = files_at + FILE_HEADER + b"first\0".len();
        bytes[second_at..second_at + 4].copy_from_slice(&50_000u32.to_le_bytes());
        bytes[second_at + 4..second_at + 8].copy_from_slice(&0u32.to_le_bytes());
        let got = read(bytes).unwrap();
        assert!(got[0].1 == first);
        assert!(got[1].1 == first);
        assert_eq!(got[2].1, b"third");
    }

    #[test]
    fn names_are_read_in_their_encoding() {
        // "привет.txt" in the code page of DOS and in UTF-8.
        let dos = [0xAF, 0xE0, 0xA8, 0xA2, 0xA5, 0xE2, b'.', b't', b'x', b't'];
        let utf8 = "папка\\привет.txt".as_bytes();
        let bytes = cabinet(
            &[Group {
                kind: STORED,
                files: vec![item(&dos, 0, b"1"), item(utf8, ATTRIBUTE_UTF_NAME, b"2")],
            }],
            [0; 3],
        );
        let options = OpenOptions {
            encoding: NameEncoding::from_label("ibm866").unwrap(),
            ..OpenOptions::default()
        };
        let got = read_with(bytes, options).unwrap();
        assert_eq!(got[0].0.meta.path, "привет.txt");
        assert_eq!(got[1].0.meta.path, "папка/привет.txt");
    }

    #[test]
    fn the_checksum_is_what_the_format_says() {
        assert_eq!(checksum(&[], 7), 7);
        assert_eq!(checksum(&[1, 2, 3, 4], 0), 0x0403_0201);
        assert_eq!(checksum(&[1, 2, 3, 4, 5], 0), 0x0403_0201 ^ 5);
        assert_eq!(checksum(&[1, 2, 3, 4, 5, 6], 0), 0x0403_0201 ^ 0x0506);
        assert_eq!(
            checksum(&[1, 2, 3, 4, 5, 6, 7], 1),
            0x0403_0200 ^ 0x0005_0607
        );
    }

    #[test]
    fn what_is_broken_is_said_to_be_broken() {
        let content = sample(70_000, 4);
        let group = |kind| Group {
            kind,
            files: vec![item(b"a.bin", 0, &content), item(b"b.bin", 0, b"second")],
        };
        let whole = cabinet(&[group(MSZIP)], [0; 3]);
        let code = |bytes: Vec<u8>| read(bytes).unwrap_err().code;

        assert_eq!(code(b"MSCF".to_vec()), "truncated");
        assert_eq!(code(Vec::new()), "truncated");
        let mut marked = whole.clone();
        marked[0] = b'X';
        assert_eq!(code(marked), "damaged");
        // The list is cut, then the data.
        assert_eq!(code(whole[..50].to_vec()), "truncated");
        let error = read(whole[..whole.len() - 10].to_vec()).unwrap_err();
        assert_eq!(error.code, "damagedEntry");
        assert_eq!(error.params["name"], "a.bin");
        // A byte of the data is changed: the checksum tells.
        let mut flipped = whole.clone();
        let at = flipped.len() - 200;
        flipped[at] ^= 0x40;
        let error = read(flipped).unwrap_err();
        assert_eq!(error.code, "damagedEntry");
        assert!(error.params["detail"].contains("checksum"), "{error:?}");
        // The same without a checksum: the decoder tells, or the sizes.
        let mut blind = whole.clone();
        let first = le32(&blind, HEADER).unwrap() as usize;
        blind[first..first + 4].fill(0);
        blind[first + BLOCK_HEADER + 40] ^= 0x55;
        blind[first + BLOCK_HEADER + 41] ^= 0xAA;
        assert_eq!(code(blind), "damagedEntry");
        // A block of MSZIP that does not start as one.
        let mut unmarked = whole.clone();
        unmarked[first..first + 4].fill(0);
        unmarked[first + BLOCK_HEADER] = b'X';
        let error = read(unmarked).unwrap_err();
        assert!(error.params["detail"].contains("mark"), "{error:?}");
        // A file of a folder there is not, and one from another cabinet.
        let files_at = le32(&whole, 16).unwrap() as usize;
        let mut lost = whole.clone();
        lost[files_at + 8] = 9;
        assert_eq!(code(lost), "damaged");
        let mut continued = whole.clone();
        continued[files_at + 8..files_at + 10].copy_from_slice(&0xFFFDu16.to_le_bytes());
        assert_eq!(code(continued), "unsupportedArchive");
        // A file that ends after its folder does.
        let mut long = whole.clone();
        long[files_at..files_at + 4].copy_from_slice(&80_000u32.to_le_bytes());
        assert_eq!(code(long), "damagedEntry");
        // Methods there are no decoders for.
        let mut quantum = whole.clone();
        quantum[HEADER + 6] = METHOD_QUANTUM as u8;
        let error = read(quantum).unwrap_err();
        assert_eq!(error.code, "unsupportedMethod");
        assert_eq!(error.params["method"], "Quantum");
        let mut unknown = whole.clone();
        unknown[HEADER + 6] = 9;
        assert_eq!(code(unknown), "unsupportedMethod");
        let mut window = cabinet(&[group(LZX_16)], [0; 3]);
        window[HEADER + 7] = 30;
        assert_eq!(code(window), "damaged");
    }
}

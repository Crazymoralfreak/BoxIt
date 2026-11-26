//! Entries whose content lies in the archive file as it is, in one piece or
//! in several: the members of an `ar`, of a `cpio`, the files of a disc
//! image. Their archives are read by jumping to where the content is.

use std::io::{self, Read, Seek, SeekFrom};

use boxit_core::{
    ArchiveReader, Capabilities, Entry, EntrySink, Error, OpContext, Result, Selection,
};

use crate::source::{Source, SourceReader};
use crate::support::Watched;
use crate::OpenOptions;

/// A piece of the archive file.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct Extent {
    pub offset: u64,
    pub length: u64,
}

/// The offset of a piece that is not in the file at all: a part of a
/// sparse file, which reads as zeros.
const NOWHERE: u64 = u64::MAX;

impl Extent {
    /// A piece of `length` zeros that the archive does not store.
    pub(crate) fn hole(length: u64) -> Self {
        Self {
            offset: NOWHERE,
            length,
        }
    }

    pub(crate) fn is_hole(&self) -> bool {
        self.offset == NOWHERE
    }
}

/// An entry and where its content is. An entry without content has no
/// pieces.
#[derive(Clone, Debug)]
pub(crate) struct Located {
    pub entry: Entry,
    pub extents: Vec<Extent>,
}

/// The pieces of an entry read one after another. A file that ends before
/// a piece does is cut short, and says so.
pub(crate) struct Pieces<'a> {
    reader: &'a mut SourceReader,
    extents: &'a [Extent],
    next: usize,
    left: u64,
    /// The piece being read is one of zeros.
    hole: bool,
}

impl<'a> Pieces<'a> {
    pub(crate) fn new(reader: &'a mut SourceReader, extents: &'a [Extent]) -> Self {
        Self {
            reader,
            extents,
            next: 0,
            left: 0,
            hole: false,
        }
    }
}

impl Read for Pieces<'_> {
    fn read(&mut self, buffer: &mut [u8]) -> io::Result<usize> {
        if buffer.is_empty() {
            return Ok(0);
        }
        while self.left == 0 {
            let Some(extent) = self.extents.get(self.next) else {
                return Ok(0);
            };
            self.next += 1;
            self.hole = extent.is_hole();
            if !self.hole {
                self.reader.seek(SeekFrom::Start(extent.offset))?;
            }
            self.left = extent.length;
        }
        let want = buffer
            .len()
            .min(usize::try_from(self.left).unwrap_or(usize::MAX));
        if self.hole {
            buffer[..want].fill(0);
            self.left -= want as u64;
            return Ok(want);
        }
        let read = self.reader.read(&mut buffer[..want])?;
        if read == 0 {
            return Err(io::ErrorKind::UnexpectedEof.into());
        }
        self.left -= read as u64;
        Ok(read)
    }
}

/// Hands the picked entries to the sink, in the order of the archive.
pub(crate) fn hand_over(
    source: &Source,
    located: &[Located],
    pick: &Selection,
    sink: &dyn EntrySink,
    ctx: &OpContext,
) -> Result<()> {
    let mut reader = source.reader()?;
    for item in located {
        if !pick.contains(&item.entry) {
            continue;
        }
        ctx.checkpoint()?;
        if item.entry.meta.is_file() {
            let mut data = Watched::new(Pieces::new(&mut reader, &item.extents));
            sink.accept(&item.entry, &mut data)
                .map_err(|error| data.settle(&item.entry, error))?;
        } else {
            sink.accept(&item.entry, &mut io::empty())?;
        }
    }
    Ok(())
}

/// Reads the list of an archive of one of these formats.
pub(crate) type Lister = fn(&Source, &OpenOptions, &OpContext) -> Result<Vec<Located>>;

/// An archive whose entries lie in it as they are, opened for reading.
pub struct StoredReader {
    source: Source,
    options: OpenOptions,
    list: Lister,
    /// The list, once it has been read.
    located: Option<Vec<Located>>,
}

impl StoredReader {
    pub(crate) fn new(source: Source, options: OpenOptions, list: Lister) -> Self {
        Self {
            source,
            options,
            list,
            located: None,
        }
    }

    /// Reads the list of the archive on first use.
    fn load(&mut self, ctx: &OpContext) -> Result<&[Located]> {
        if self.located.is_none() {
            self.located = Some((self.list)(&self.source, &self.options, ctx)?);
        }
        Ok(self.located.as_deref().unwrap_or_default())
    }
}

impl ArchiveReader for StoredReader {
    fn capabilities(&self) -> Capabilities {
        Capabilities {
            random_access: true,
            solid: false,
            parallel: false,
            has_index: true,
        }
    }

    fn entries(&mut self, ctx: &OpContext) -> Result<Vec<Entry>> {
        Ok(self
            .load(ctx)?
            .iter()
            .map(|item| item.entry.clone())
            .collect())
    }

    fn extract(&mut self, pick: &Selection, sink: &dyn EntrySink, ctx: &OpContext) -> Result<()> {
        self.load(ctx)?;
        let located = self.located.as_deref().unwrap_or_default();
        hand_over(&self.source, located, pick, sink, ctx)
    }
}

/// The archive ends before what it says it holds.
pub(crate) fn cut_short() -> Error {
    Error::new("truncated")
}

/// Reads `length` bytes at `offset` of an archive of `size` bytes. The
/// length is checked against the file before any memory is taken for it:
/// it comes from the archive.
pub(crate) fn read_at(
    reader: &mut SourceReader,
    size: u64,
    offset: u64,
    length: u64,
) -> Result<Vec<u8>> {
    let end = offset.checked_add(length).ok_or_else(cut_short)?;
    if end > size {
        return Err(cut_short());
    }
    let length = usize::try_from(length).map_err(|_| Error::new("notEnoughMemory"))?;
    reader.seek(SeekFrom::Start(offset))?;
    let mut bytes = vec![0u8; length];
    reader.read_exact(&mut bytes)?;
    Ok(bytes)
}

#[cfg(test)]
mod tests {
    use boxit_core::EntryMeta;

    use super::*;

    /// Reads what it is given and keeps nothing.
    struct Reads;

    impl EntrySink for Reads {
        fn accept(&self, _entry: &Entry, data: &mut dyn Read) -> Result<()> {
            io::copy(data, &mut io::sink())?;
            Ok(())
        }
    }

    #[test]
    fn pieces_are_read_one_after_another() {
        let source = Source::memory("x", b"0123456789abcdef".to_vec());
        let mut reader = source.reader().unwrap();
        let extents = [
            Extent {
                offset: 10,
                length: 3,
            },
            Extent {
                offset: 4,
                length: 0,
            },
            Extent {
                offset: 2,
                length: 4,
            },
        ];
        let mut content = String::new();
        Pieces::new(&mut reader, &extents)
            .read_to_string(&mut content)
            .unwrap();
        assert_eq!(content, "abc2345");
        // A piece the file does not store reads as zeros.
        let sparse = [extents[0], Extent::hole(3), extents[2]];
        let mut content = Vec::new();
        Pieces::new(&mut reader, &sparse)
            .read_to_end(&mut content)
            .unwrap();
        assert_eq!(content, b"abc\0\0\x002345");
        let mut nothing = Vec::new();
        Pieces::new(&mut reader, &[])
            .read_to_end(&mut nothing)
            .unwrap();
        assert_eq!(nothing, b"");
    }

    #[test]
    fn a_piece_past_the_end_of_the_file_is_an_error() {
        let source = Source::memory("x", b"0123456789".to_vec());
        let mut reader = source.reader().unwrap();
        let extents = [Extent {
            offset: 8,
            length: 5,
        }];
        let mut content = Vec::new();
        let error = Pieces::new(&mut reader, &extents)
            .read_to_end(&mut content)
            .unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::UnexpectedEof);

        let located = [Located {
            entry: Entry {
                meta: EntryMeta::file("cut.bin", 5),
                ..Entry::default()
            },
            extents: extents.to_vec(),
        }];
        let error = hand_over(
            &source,
            &located,
            &Selection::All,
            &Reads,
            &OpContext::silent(),
        )
        .unwrap_err();
        assert_eq!(error.code, "damagedEntry");
        assert_eq!(error.params["name"], "cut.bin");
    }

    #[test]
    fn a_block_is_read_only_if_the_file_holds_it() {
        let source = Source::memory("x", b"0123456789".to_vec());
        let mut reader = source.reader().unwrap();
        assert_eq!(read_at(&mut reader, 10, 7, 3).unwrap(), b"789");
        assert_eq!(
            read_at(&mut reader, 10, 7, 4).unwrap_err().code,
            "truncated"
        );
        assert_eq!(
            read_at(&mut reader, 10, u64::MAX, 2).unwrap_err().code,
            "truncated"
        );
        assert_eq!(
            read_at(&mut reader, 10, 0, u64::MAX).unwrap_err().code,
            "truncated"
        );
    }
}

//! The bytes of an archive file, shared by the threads that read it.
//!
//! Formats with an index (ZIP, 7z) jump around the file, and several threads
//! decode different entries at once. Mapping the file into memory gives each
//! of them a cursor of its own over the same pages, with no system call per
//! read and no copy; the system reads ahead and caches as it sees fit. A file
//! that cannot be mapped is read through ordinary handles, one per reader.
//!
//! An archive cut into volumes (see [`crate::volumes`]) is read as the one
//! run of bytes its volumes make, whichever of them was opened: no format
//! has to know that its file is several.

use std::fs::File;
use std::io::{self, BufReader, Cursor, Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};
use std::sync::Arc;

use boxit_core::Result;
use memmap2::Mmap;

use crate::volumes;

/// Size of the buffer of a reader that goes through a file handle.
const FILE_BUFFER: usize = 256 * 1024;

/// Where the bytes of an archive are held.
enum Held {
    File(Mmap),
    Memory(Vec<u8>),
}

/// The bytes of an archive in memory: a mapped file, or a block of memory.
/// Clones share them.
#[derive(Clone)]
pub struct Mapped(Arc<Held>);

impl AsRef<[u8]> for Mapped {
    fn as_ref(&self) -> &[u8] {
        match &*self.0 {
            Held::File(map) => map,
            Held::Memory(bytes) => bytes,
        }
    }
}

/// One of the volumes of an archive.
#[derive(Clone)]
struct Volume {
    path: PathBuf,
    /// Where the volume starts in the archive.
    start: u64,
    size: u64,
    /// Bytes at the start of the file that are not part of the archive:
    /// the program a self-extracting archive is glued to.
    skip: u64,
    map: Option<Mapped>,
}

/// An archive opened for reading: a file, the volumes of one, or bytes in
/// memory.
#[derive(Clone)]
pub struct Source {
    path: PathBuf,
    /// The name of the archive: of the file, or of what its volumes make.
    name: String,
    size: u64,
    map: Option<Mapped>,
    /// The volumes, when the archive is cut into them.
    volumes: Option<Arc<Vec<Volume>>>,
}

/// Maps a file into memory, when the system lets it.
fn map(file: &File, size: u64) -> Option<Mapped> {
    if size == 0 {
        return None;
    }
    // SAFETY: the mapping is read-only and lives as long as the `Arc`
    // that every reader holds. What the language cannot rule out is
    // another program changing the file while it is mapped: Windows
    // refuses to truncate a mapped file, and on other systems a
    // truncation would end this process with a bus error rather than
    // corrupt memory. An archive being rewritten while it is read is
    // broken input under any way of reading it.
    let map = unsafe { Mmap::map(file) }.ok()?;
    Some(Mapped(Arc::new(Held::File(map))))
}

fn file_name(path: &Path) -> String {
    path.file_name()
        .map_or_else(String::new, |name| name.to_string_lossy().into_owned())
}

impl Source {
    /// An archive that is not a file: one found inside another archive, one
    /// that came from somewhere else. `name` is what a file would be called;
    /// it shows in messages and tells `x.tar.gz` from `x.gz`.
    pub fn memory(name: &str, bytes: Vec<u8>) -> Self {
        let path = PathBuf::from(name);
        Self {
            name: file_name(&path),
            path,
            size: bytes.len() as u64,
            map: Some(Mapped(Arc::new(Held::Memory(bytes)))),
            volumes: None,
        }
    }

    /// Opens an archive file. A file that is a volume by its name brings
    /// the other volumes with it: the archive is all of them, from the
    /// first, whichever was named.
    pub fn open(path: &Path) -> Result<Self> {
        let file = File::open(path)?;
        if let Some((name, paths)) = volumes::of(path)? {
            let mut volumes = Vec::with_capacity(paths.len());
            let mut size = 0u64;
            for path in paths {
                let file = File::open(&path)?;
                let length = file.metadata()?.len();
                volumes.push(Volume {
                    map: map(&file, length),
                    path,
                    start: size,
                    size: length,
                    skip: 0,
                });
                size += length;
            }
            return Ok(Self {
                path: volumes[0].path.clone(),
                name,
                size,
                map: None,
                volumes: Some(Arc::new(volumes)),
            });
        }
        let size = file.metadata()?.len();
        Ok(Self {
            path: path.to_path_buf(),
            name: file_name(path),
            size,
            map: map(&file, size),
            volumes: None,
        })
    }

    /// The archive with more bytes after its end, which are held in memory
    /// and written nowhere: the list of a ZIP that lost its own.
    pub(crate) fn with_tail(&self, tail: Vec<u8>) -> Self {
        let mut volumes = match &self.volumes {
            Some(volumes) => volumes.as_ref().clone(),
            None => vec![Volume {
                path: self.path.clone(),
                start: 0,
                size: self.size,
                skip: 0,
                map: self.map.clone(),
            }],
        };
        let size = tail.len() as u64;
        volumes.push(Volume {
            path: PathBuf::new(),
            start: self.size,
            size,
            skip: 0,
            map: Some(Mapped(Arc::new(Held::Memory(tail)))),
        });
        Self {
            path: self.path.clone(),
            name: self.name.clone(),
            size: self.size + size,
            map: None,
            volumes: Some(Arc::new(volumes)),
        }
    }

    /// The file of the archive; the first of them when it is cut into
    /// volumes.
    pub fn path(&self) -> &Path {
        &self.path
    }

    /// The number of files the archive is in: one, unless it is cut into
    /// volumes.
    pub fn volumes(&self) -> usize {
        self.volumes.as_ref().map_or(1, |volumes| volumes.len())
    }

    /// The archive that starts `start` bytes into this file. What stands
    /// before that is a program the archive was glued to.
    pub(crate) fn suffix(&self, start: u64) -> Result<Self> {
        if start == 0 || start >= self.size || self.volumes.is_some() {
            return Err(boxit_core::Error::new("internal").detail("suffix"));
        }
        let size = self.size - start;
        Ok(Self {
            path: self.path.clone(),
            name: self.name.clone(),
            size,
            map: None,
            volumes: Some(Arc::new(vec![Volume {
                path: self.path.clone(),
                start: 0,
                size,
                skip: start,
                map: self.map.clone(),
            }])),
        })
    }

    /// The bytes of an archive that is not a file: there is nothing at its
    /// path to open.
    #[cfg(feature = "rar")]
    pub(crate) fn held(&self) -> Option<&[u8]> {
        match self.map.as_ref().map(|map| &*map.0) {
            Some(Held::Memory(bytes)) => Some(bytes),
            _ => None,
        }
    }

    /// Size of the archive in bytes: of all its volumes when it has them.
    pub fn size(&self) -> u64 {
        self.size
    }

    /// The name of the archive, for messages and for telling `x.tar.gz`
    /// from `x.gz`: of the file, or of what its volumes make.
    pub fn name(&self) -> String {
        self.name.clone()
    }

    /// The first bytes of the file, at most `length` of them.
    pub fn head(&self, length: usize) -> Result<Vec<u8>> {
        let mut head = Vec::with_capacity(length.min(self.size as usize));
        self.reader()?.take(length as u64).read_to_end(&mut head)?;
        Ok(head)
    }

    /// A reader of the file with its own position, starting at the beginning.
    /// Every thread takes one of its own.
    pub fn reader(&self) -> Result<SourceReader> {
        if let Some(volumes) = &self.volumes {
            return Ok(SourceReader::Volumes(Joined {
                volumes: volumes.clone(),
                size: self.size,
                position: 0,
                open: None,
            }));
        }
        Ok(match &self.map {
            Some(map) => SourceReader::Mapped(Cursor::new(map.clone())),
            None => SourceReader::File(BufReader::with_capacity(
                FILE_BUFFER,
                File::open(&self.path)?,
            )),
        })
    }
}

/// The volumes of an archive read as one file.
pub struct Joined {
    volumes: Arc<Vec<Volume>>,
    size: u64,
    position: u64,
    /// The volume that is read through a handle because it could not be
    /// mapped: which one, the handle, and where the handle stands.
    open: Option<(usize, File, u64)>,
}

impl Joined {
    /// Reads from one volume that is not mapped.
    fn read_file(&mut self, index: usize, offset: u64, buffer: &mut [u8]) -> io::Result<usize> {
        let (_, file, at) = match self.open.take() {
            Some(open) if open.0 == index => self.open.insert(open),
            _ => self
                .open
                .insert((index, File::open(&self.volumes[index].path)?, 0)),
        };
        if *at != offset {
            file.seek(SeekFrom::Start(offset))?;
            *at = offset;
        }
        let count = file.read(buffer)?;
        if count == 0 {
            // The volume was as long as this when the archive was opened.
            return Err(io::ErrorKind::UnexpectedEof.into());
        }
        *at += count as u64;
        Ok(count)
    }
}

impl Read for Joined {
    fn read(&mut self, buffer: &mut [u8]) -> io::Result<usize> {
        if self.position >= self.size || buffer.is_empty() {
            return Ok(0);
        }
        // The volume the position is in; a volume without bytes is none.
        let index = self
            .volumes
            .partition_point(|volume| volume.start + volume.size <= self.position);
        let volume = &self.volumes[index];
        let offset = self.position - volume.start + volume.skip;
        let count =
            (volume.size - (self.position - volume.start)).min(buffer.len() as u64) as usize;
        let count = match &volume.map {
            Some(map) => {
                let offset = offset as usize;
                buffer[..count].copy_from_slice(&map.as_ref()[offset..offset + count]);
                count
            }
            None => self.read_file(index, offset, &mut buffer[..count])?,
        };
        self.position += count as u64;
        Ok(count)
    }
}

impl Seek for Joined {
    fn seek(&mut self, position: SeekFrom) -> io::Result<u64> {
        let (base, offset) = match position {
            SeekFrom::Start(offset) => {
                self.position = offset;
                return Ok(offset);
            }
            SeekFrom::End(offset) => (self.size, offset),
            SeekFrom::Current(offset) => (self.position, offset),
        };
        match base.checked_add_signed(offset) {
            Some(position) => {
                self.position = position;
                Ok(position)
            }
            None => Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "a position before the start of the archive",
            )),
        }
    }
}

/// A reader of an archive file: a cursor over the mapping, a buffered file
/// handle, or the volumes of the archive one after another.
pub enum SourceReader {
    Mapped(Cursor<Mapped>),
    File(BufReader<File>),
    Volumes(Joined),
}

impl Read for SourceReader {
    fn read(&mut self, buffer: &mut [u8]) -> io::Result<usize> {
        match self {
            Self::Mapped(cursor) => cursor.read(buffer),
            Self::File(file) => file.read(buffer),
            Self::Volumes(joined) => joined.read(buffer),
        }
    }
}

impl Seek for SourceReader {
    fn seek(&mut self, position: SeekFrom) -> io::Result<u64> {
        match self {
            Self::Mapped(cursor) => cursor.seek(position),
            Self::File(file) => file.seek(position),
            Self::Volumes(joined) => joined.seek(position),
        }
    }
}

#[cfg(test)]
mod tests {
    use std::fs;
    use std::thread;

    use super::*;

    #[test]
    fn readers_have_positions_of_their_own() {
        let folder = tempfile::tempdir().unwrap();
        let path = folder.path().join("data.bin");
        let data: Vec<u8> = (0..100_000u32).flat_map(u32::to_le_bytes).collect();
        fs::write(&path, &data).unwrap();
        let source = Source::open(&path).unwrap();
        assert_eq!(source.size(), data.len() as u64);
        assert_eq!(source.name(), "data.bin");
        assert_eq!(source.head(8).unwrap(), data[..8]);
        thread::scope(|scope| {
            for part in 0..4u64 {
                let source = source.clone();
                let data = &data;
                scope.spawn(move || {
                    let mut reader = source.reader().unwrap();
                    let start = part * 50_000;
                    reader.seek(SeekFrom::Start(start)).unwrap();
                    let mut read = vec![0u8; 10_000];
                    reader.read_exact(&mut read).unwrap();
                    assert_eq!(read, data[start as usize..start as usize + 10_000]);
                });
            }
        });
    }

    #[test]
    fn volumes_read_as_the_file_they_were_cut_from() {
        let folder = tempfile::tempdir().unwrap();
        let data: Vec<u8> = (0..50_000u32).flat_map(u32::to_le_bytes).collect();
        // Volumes of every size, one of a single byte and one of none.
        let cuts = [
            1usize, 4096, 4097, 70_000, 70_000, 150_000, 150_000, 200_000,
        ];
        let mut start = 0;
        for (number, end) in cuts.into_iter().enumerate() {
            let name = volumes::name("data.tar.gz", number as u64 + 1);
            fs::write(folder.path().join(name), &data[start..end]).unwrap();
            start = end;
        }
        for opened in ["data.tar.gz.001", "data.tar.gz.004", "data.tar.gz.008"] {
            let source = Source::open(&folder.path().join(opened)).unwrap();
            assert_eq!(source.volumes(), cuts.len());
            assert_eq!(source.size(), data.len() as u64);
            // The name of the archive, without the number of a volume.
            assert_eq!(source.name(), "data.tar.gz");
            assert_eq!(source.path(), folder.path().join("data.tar.gz.001"));
            assert_eq!(source.head(10).unwrap(), data[..10]);

            let mut reader = source.reader().unwrap();
            let mut whole = Vec::new();
            reader.read_to_end(&mut whole).unwrap();
            assert!(whole == data, "{opened}");
            // Across the ends of volumes, from every side.
            for (position, length) in [
                (SeekFrom::Start(0), 3usize),
                (SeekFrom::Start(69_990), 20),
                (SeekFrom::Start(4090), 20),
                (SeekFrom::End(-50_001), 50_001),
                (SeekFrom::Current(-150_010), 100),
                (SeekFrom::Start(149_990), 50_010),
            ] {
                let at = reader.seek(position).unwrap() as usize;
                let mut read = vec![0u8; length];
                reader.read_exact(&mut read).unwrap();
                assert!(read == data[at..at + length], "{opened}: {position:?}");
            }
            // Past the end there is nothing, and before the start no place.
            reader.seek(SeekFrom::End(10)).unwrap();
            assert_eq!(reader.read(&mut [0u8; 8]).unwrap(), 0);
            assert!(reader.seek(SeekFrom::Current(-300_000)).is_err());
        }

        // A volume alone is an archive of one volume.
        let alone = folder.path().join("alone.zip.001");
        fs::write(&alone, b"0123456789").unwrap();
        let source = Source::open(&alone).unwrap();
        assert_eq!((source.volumes(), source.size()), (1, 10));
        assert_eq!(source.name(), "alone.zip");
        // A volume that is not there, and one before it that is not.
        let error = Source::open(&folder.path().join("no.zip.001"))
            .err()
            .unwrap();
        assert_eq!(error.code, "notFound");
        fs::write(folder.path().join("gap.zip.003"), b"x").unwrap();
        let error = Source::open(&folder.path().join("gap.zip.003"))
            .err()
            .unwrap();
        assert_eq!(error.code, "volumeMissing");
        assert_eq!(error.params["name"], "gap.zip.001");
    }

    #[test]
    fn a_tail_in_memory_follows_the_bytes_of_the_file() {
        let folder = tempfile::tempdir().unwrap();
        let path = folder.path().join("data.bin");
        fs::write(&path, b"0123456789").unwrap();
        let source = Source::open(&path).unwrap().with_tail(b"abc".to_vec());
        assert_eq!(source.size(), 13);
        assert_eq!(source.path(), path);
        let mut whole = String::new();
        source.reader().unwrap().read_to_string(&mut whole).unwrap();
        assert_eq!(whole, "0123456789abc");
        let mut reader = source.reader().unwrap();
        reader.seek(SeekFrom::End(-5)).unwrap();
        let mut end = String::new();
        reader.read_to_string(&mut end).unwrap();
        assert_eq!(end, "89abc");

        // After bytes in memory, after volumes, and after nothing at all.
        let memory = Source::memory("a.zip", b"xy".to_vec()).with_tail(b"z".to_vec());
        assert_eq!(memory.head(8).unwrap(), b"xyz");
        for (number, part) in [b"ab", b"cd"].into_iter().enumerate() {
            let name = volumes::name("cut.zip", number as u64 + 1);
            fs::write(folder.path().join(name), part).unwrap();
        }
        let cut = Source::open(&folder.path().join("cut.zip.002")).unwrap();
        assert_eq!(cut.with_tail(b"ef".to_vec()).head(8).unwrap(), b"abcdef");
        let empty = folder.path().join("empty");
        fs::write(&empty, b"").unwrap();
        let empty = Source::open(&empty).unwrap().with_tail(b"tail".to_vec());
        assert_eq!(empty.head(8).unwrap(), b"tail");
    }

    #[test]
    fn bytes_in_memory_read_like_a_file() {
        let source = Source::memory("inner.tar.gz", b"0123456789".to_vec());
        assert_eq!(source.size(), 10);
        assert_eq!(source.name(), "inner.tar.gz");
        assert_eq!(source.head(4).unwrap(), b"0123");
        let mut reader = source.clone().reader().unwrap();
        reader.seek(SeekFrom::End(-3)).unwrap();
        let mut tail = String::new();
        reader.read_to_string(&mut tail).unwrap();
        assert_eq!(tail, "789");
        // Without a byte it is still not a file to look for on the disk.
        let empty = Source::memory("empty.zip", Vec::new());
        assert_eq!(empty.head(4).unwrap(), b"");
    }

    #[test]
    fn an_empty_file_is_read_as_empty() {
        let folder = tempfile::tempdir().unwrap();
        let path = folder.path().join("empty");
        fs::write(&path, b"").unwrap();
        let source = Source::open(&path).unwrap();
        assert_eq!(source.size(), 0);
        assert_eq!(source.head(16).unwrap(), b"");
    }

    #[test]
    fn a_missing_file_is_reported_as_such() {
        let error = Source::open(Path::new("no/such/file.zip")).err().unwrap();
        assert_eq!(error.code, "notFound");
    }
}

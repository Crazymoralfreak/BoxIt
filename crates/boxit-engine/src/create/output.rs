//! What an archive is written into when it is not one plain file: the
//! volumes it is cut into, and the end of the program that unpacks it.
//!
//! Both look like a file to the format that writes the archive: it writes,
//! and goes back to fill in what it did not know, as it would in a file.

use std::fs::{self, File, OpenOptions};
use std::io::{self, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};

use boxit_codecs::volumes;
use boxit_core::{Error, Result};

use crate::fsx;

/// How an archive lies on the disk.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub enum Layout {
    /// One file.
    #[default]
    Whole,
    /// Cut into volumes of this many bytes: `name.001`, `name.002` and so
    /// on, which put end to end are the archive.
    Volumes(u64),
    /// Behind a program that unpacks it where no archiver is installed.
    /// The path is of the module: the program without an archive.
    SelfExtracting(PathBuf),
}

/// The name a volume has while the archive is written.
fn staged(folder: &Path, index: u64) -> PathBuf {
    folder.join(format!("{:05}", index + 1))
}

/// The volumes of an archive being written, as one file. They are written
/// in a folder of their own and put in their places when all are complete.
pub struct Cut {
    folder: PathBuf,
    /// Bytes in a volume.
    size: u64,
    position: u64,
    /// Bytes of the archive so far.
    length: u64,
    /// Volumes that have a file.
    made: u64,
    /// The volume being written: which one, its file, and where the file
    /// stands.
    open: Option<(u64, File, u64)>,
}

impl Cut {
    pub fn new(folder: PathBuf, size: u64) -> Result<Self> {
        if size < volumes::SMALLEST {
            return Err(Error::new("volumeTooSmall").with("size", volumes::SMALLEST));
        }
        Ok(Self {
            folder,
            size,
            position: 0,
            length: 0,
            made: 0,
            open: None,
        })
    }

    /// The file of a volume, made if it was not there, with the volumes
    /// before it.
    fn volume(&mut self, index: u64) -> io::Result<&mut (u64, File, u64)> {
        if index >= volumes::MOST {
            return Err(Error::new("tooManyVolumes")
                .with("count", volumes::MOST)
                .into_io());
        }
        match self.open.take() {
            Some(open) if open.0 == index => return Ok(self.open.insert(open)),
            _ => {}
        }
        // A format that leaves room to come back to goes past the end of
        // a volume before anything is written in it.
        while self.made < index {
            let skipped = File::create_new(staged(&self.folder, self.made))?;
            skipped.set_len(self.size)?;
            self.made += 1;
        }
        let path = staged(&self.folder, index);
        let file = if index == self.made {
            self.made += 1;
            File::create_new(path)?
        } else {
            OpenOptions::new().write(true).open(path)?
        };
        Ok(self.open.insert((index, file, 0)))
    }

    /// Makes sure the volumes are on the disk, and tells how many there
    /// are and how many bytes in all.
    pub fn finish(mut self) -> io::Result<(u64, u64)> {
        self.open = None;
        for index in 0..self.made {
            OpenOptions::new()
                .write(true)
                .open(staged(&self.folder, index))?
                .sync_all()?;
        }
        Ok((self.made, self.length))
    }
}

impl Write for Cut {
    fn write(&mut self, buffer: &[u8]) -> io::Result<usize> {
        if buffer.is_empty() {
            return Ok(0);
        }
        let offset = self.position % self.size;
        let room = (self.size - offset).min(buffer.len() as u64) as usize;
        let (_, file, at) = self.volume(self.position / self.size)?;
        if *at != offset {
            file.seek(SeekFrom::Start(offset))?;
            *at = offset;
        }
        let count = file.write(&buffer[..room])?;
        *at += count as u64;
        self.position += count as u64;
        self.length = self.length.max(self.position);
        Ok(count)
    }

    fn flush(&mut self) -> io::Result<()> {
        // Nothing is kept from the files.
        Ok(())
    }
}

/// A position counted from the start, the end or the current place.
fn moved(position: SeekFrom, current: u64, end: u64) -> io::Result<u64> {
    let (base, offset) = match position {
        SeekFrom::Start(offset) => return Ok(offset),
        SeekFrom::End(offset) => (end, offset),
        SeekFrom::Current(offset) => (current, offset),
    };
    base.checked_add_signed(offset).ok_or_else(|| {
        io::Error::new(
            io::ErrorKind::InvalidInput,
            "a position before the start of the archive",
        )
    })
}

impl Seek for Cut {
    fn seek(&mut self, position: SeekFrom) -> io::Result<u64> {
        self.position = moved(position, self.position, self.length)?;
        Ok(self.position)
    }
}

/// The volumes that stand where an archive is to be written: `name.001`
/// and the ones after it, for as long as their numbers have no gap.
pub fn standing(destination: &Path) -> Vec<PathBuf> {
    (1..=volumes::MOST)
        .map(|number| volumes::path(destination, number))
        .take_while(|volume| volume.is_file())
        .collect()
}

/// Moves files, and moves the ones it moved back when one cannot be.
fn move_all(moves: &[(PathBuf, PathBuf)]) -> io::Result<()> {
    for (done, (from, to)) in moves.iter().enumerate() {
        if let Err(error) = fs::rename(from, to) {
            undo(&moves[..done]);
            return Err(error);
        }
    }
    Ok(())
}

/// Takes moved files back to where they were.
fn undo(moves: &[(PathBuf, PathBuf)]) {
    for (from, to) in moves.iter().rev() {
        if let Err(error) = fs::rename(to, from) {
            tracing::warn!(file = %fsx::display(to), "a file was not put back: {error}");
        }
    }
}

/// Puts the volumes written in `folder` where the archive goes. The volumes
/// that stood there are taken away first and come back if the new ones
/// cannot be put in: either the new archive stands there whole, or the old.
pub fn place(folder: &Path, count: u64, destination: &Path) -> io::Result<()> {
    let old: Vec<(PathBuf, PathBuf)> = standing(destination)
        .into_iter()
        .enumerate()
        .map(|(index, volume)| (volume, folder.join(format!("old-{:05}", index + 1))))
        .collect();
    let new: Vec<(PathBuf, PathBuf)> = (0..count)
        .map(|index| (staged(folder, index), volumes::path(destination, index + 1)))
        .collect();
    move_all(&old)?;
    if let Err(error) = move_all(&new) {
        undo(&old);
        return Err(error);
    }
    Ok(())
}

/// The end of a file as a file of its own: what is written lands after
/// what the file held, and the positions are counted from there.
pub struct Tail {
    file: File,
    /// Where the archive starts in the file.
    start: u64,
}

impl Tail {
    /// The file stands at its end: the archive starts there.
    pub fn new(mut file: File) -> io::Result<Self> {
        let start = file.stream_position()?;
        Ok(Self { file, start })
    }

    pub fn into_file(self) -> File {
        self.file
    }
}

impl Write for Tail {
    fn write(&mut self, buffer: &[u8]) -> io::Result<usize> {
        self.file.write(buffer)
    }

    fn flush(&mut self) -> io::Result<()> {
        self.file.flush()
    }
}

impl Seek for Tail {
    fn seek(&mut self, position: SeekFrom) -> io::Result<u64> {
        let place = match position {
            SeekFrom::Start(offset) => offset,
            SeekFrom::Current(_) | SeekFrom::End(_) => {
                let current = self.file.stream_position()? - self.start;
                let end = self.file.metadata()?.len() - self.start;
                moved(position, current, end)?
            }
        };
        self.file.seek(SeekFrom::Start(self.start + place))?;
        Ok(place)
    }
}

#[cfg(test)]
mod tests {
    use std::io::Read;

    use tempfile::TempDir;

    use super::*;

    const SIZE: u64 = volumes::SMALLEST;

    /// The volumes written in a folder, put end to end.
    fn joined(folder: &Path, count: u64) -> Vec<u8> {
        (0..count)
            .flat_map(|index| fs::read(staged(folder, index)).unwrap())
            .collect()
    }

    #[test]
    fn a_cut_is_written_like_a_file() {
        let folder = TempDir::new().unwrap();
        let mut cut = Cut::new(folder.path().to_path_buf(), SIZE).unwrap();
        let mut model = Vec::new();
        let size = SIZE as usize;
        // Room left at the start, as a format leaves for what it learns at
        // the end; then data over the ends of volumes.
        cut.seek(SeekFrom::Start(32)).unwrap();
        model.resize(32, 0);
        let data: Vec<u8> = (0..size * 3 + 100).map(|index| index as u8).collect();
        cut.write_all(&data).unwrap();
        model.extend_from_slice(&data);
        assert_eq!(cut.stream_position().unwrap(), model.len() as u64);
        // Back to the start, and over the end of the first volume.
        for (at, patch) in [(0usize, &b"start"[..]), (size - 2, &b"over the end"[..])] {
            cut.seek(SeekFrom::Start(at as u64)).unwrap();
            cut.write_all(patch).unwrap();
            model[at..at + patch.len()].copy_from_slice(patch);
        }
        // From the end, and on from there.
        assert_eq!(cut.seek(SeekFrom::End(-4)).unwrap(), model.len() as u64 - 4);
        cut.write_all(b"the very end").unwrap();
        let end = model.len() - 4;
        model.truncate(end);
        model.extend_from_slice(b"the very end");
        assert!(cut
            .seek(SeekFrom::Current(-(model.len() as i64) - 1))
            .is_err());

        let (count, length) = cut.finish().unwrap();
        assert_eq!((count, length), (4, model.len() as u64));
        assert!(joined(folder.path(), count) == model);
    }

    #[test]
    fn volumes_that_were_passed_over_are_there() {
        let folder = TempDir::new().unwrap();
        let mut cut = Cut::new(folder.path().to_path_buf(), SIZE).unwrap();
        cut.seek(SeekFrom::Start(SIZE * 2 + 5)).unwrap();
        cut.write_all(b"far").unwrap();
        let (count, length) = cut.finish().unwrap();
        assert_eq!((count, length), (3, SIZE * 2 + 8));
        let bytes = joined(folder.path(), count);
        assert_eq!(bytes.len() as u64, length);
        assert!(bytes[..bytes.len() - 3].iter().all(|byte| *byte == 0));
        assert_eq!(&bytes[bytes.len() - 3..], b"far");

        // A volume is not smaller than it is worth cutting for.
        let error = Cut::new(folder.path().to_path_buf(), SIZE - 1)
            .err()
            .unwrap();
        assert_eq!(error.code, "volumeTooSmall");
    }

    #[test]
    fn the_end_of_a_file_is_written_as_a_file_of_its_own() {
        let folder = TempDir::new().unwrap();
        let path = folder.path().join("program.exe");
        let mut file = OpenOptions::new()
            .read(true)
            .write(true)
            .create_new(true)
            .open(&path)
            .unwrap();
        file.write_all(b"the program").unwrap();
        let mut tail = Tail::new(file).unwrap();
        tail.write_all(b"....archive").unwrap();
        // The positions are of the archive, not of the file.
        assert_eq!(tail.stream_position().unwrap(), 11);
        assert_eq!(tail.seek(SeekFrom::Start(0)).unwrap(), 0);
        tail.write_all(b"head").unwrap();
        assert_eq!(tail.seek(SeekFrom::End(0)).unwrap(), 11);
        tail.write_all(b" end").unwrap();
        assert_eq!(tail.seek(SeekFrom::Current(-15)).unwrap(), 0);
        // Nothing of the archive is before its start.
        assert!(tail.seek(SeekFrom::Current(-1)).is_err());

        let mut file = tail.into_file();
        file.seek(SeekFrom::Start(0)).unwrap();
        let mut written = String::new();
        file.read_to_string(&mut written).unwrap();
        assert_eq!(written, "the programheadarchive end");
    }

    #[test]
    fn volumes_take_their_places_or_the_old_ones_stay() {
        let folder = TempDir::new().unwrap();
        let staging = folder.path().join("staging");
        let destination = folder.path().join("backup.7z");
        for number in 1..=3 {
            fs::write(volumes::path(&destination, number), format!("old {number}")).unwrap();
        }
        // The fourth is not one of them: a gap in the numbers.
        fs::write(volumes::path(&destination, 5), "other").unwrap();
        fs::create_dir(&staging).unwrap();
        for index in 0..2 {
            fs::write(staged(&staging, index), format!("new {index}")).unwrap();
        }
        // A volume that is not there: nothing changes.
        assert!(place(&staging, 3, &destination).is_err());
        for number in 1..=3 {
            let volume = fs::read_to_string(volumes::path(&destination, number)).unwrap();
            assert_eq!(volume, format!("old {number}"));
        }
        assert_eq!(fs::read_to_string(staged(&staging, 0)).unwrap(), "new 0");

        place(&staging, 2, &destination).unwrap();
        assert_eq!(standing(&destination).len(), 2);
        for number in 1..=2 {
            let volume = fs::read_to_string(volumes::path(&destination, number)).unwrap();
            assert_eq!(volume, format!("new {}", number - 1));
        }
        assert!(!volumes::path(&destination, 3).exists());
        assert!(volumes::path(&destination, 5).exists());
    }
}

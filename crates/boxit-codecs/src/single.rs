//! One compressed file: `.gz`, `.bz2`, `.xz`, `.zst`, `.lz4`.
//!
//! These are not containers. The file holds the content of one file and, at
//! most, its name; it is shown as an archive of one entry, named after the
//! file itself without the extension of the compression.

use std::io::Read;

use boxit_core::{
    ArchiveFormat, ArchiveReader, ArchiveWriter, Capabilities, Entry, EntryMeta, EntrySink, Error,
    OpContext, Result, Selection, Stream, Timestamp,
};
use flate2::read::GzDecoder;

use crate::source::Source;
use crate::stream::{self, Encoder};
use crate::support::{copy, Watched};
use crate::tar_archive::stream_name;

/// The name of the entry when the file has nothing but an extension.
const NAMELESS: &str = "data";

/// The name of a file without the extension of its compression.
fn inner_name(name: &str, stream: Stream) -> String {
    let stem = ArchiveFormat::of_stream(stream, false)
        .extensions()
        .iter()
        .find_map(|extension| {
            let at = name.len().checked_sub(extension.len() + 1)?;
            let tail = name.get(at..)?;
            (tail.starts_with('.') && tail[1..].eq_ignore_ascii_case(extension)).then_some(at)
        })
        .map_or(name, |at| &name[..at]);
    if stem.is_empty() {
        String::from(NAMELESS)
    } else {
        stem.to_owned()
    }
}

/// One compressed file opened for reading.
pub struct SingleReader {
    source: Source,
    stream: Stream,
}

impl SingleReader {
    pub fn new(source: Source, stream: Stream) -> Self {
        Self { source, stream }
    }

    /// What gzip keeps about the file it compressed: its name and its time.
    fn gzip_header(&self) -> (Option<String>, Option<Timestamp>) {
        let Ok(reader) = self.source.reader() else {
            return (None, None);
        };
        let decoder = GzDecoder::new(reader);
        let Some(header) = decoder.header() else {
            return (None, None);
        };
        let name = header
            .filename()
            .map(|name| String::from_utf8_lossy(name).into_owned())
            // A name, not a path: what leads to it is of another machine.
            .and_then(|name| name.rsplit(['/', '\\']).next().map(str::to_owned))
            .filter(|name| !name.is_empty());
        let modified =
            (header.mtime() != 0).then(|| Timestamp::from_seconds(i64::from(header.mtime())));
        (name, modified)
    }

    fn entry(&self) -> Entry {
        let (name, modified) = if self.stream == Stream::Gzip {
            self.gzip_header()
        } else {
            (None, None)
        };
        Entry {
            index: 0,
            meta: EntryMeta {
                path: name.unwrap_or_else(|| inner_name(&self.source.name(), self.stream)),
                // The size is known only once the content is decompressed.
                size: None,
                modified,
                ..EntryMeta::default()
            },
            packed_size: Some(self.source.size()),
            method: Some(stream_name(self.stream).into()),
            ..Entry::default()
        }
    }
}

impl ArchiveReader for SingleReader {
    fn capabilities(&self) -> Capabilities {
        Capabilities {
            random_access: false,
            solid: false,
            parallel: false,
            has_index: true,
        }
    }

    fn entries(&mut self, _ctx: &OpContext) -> Result<Vec<Entry>> {
        Ok(vec![self.entry()])
    }

    fn extract(&mut self, pick: &Selection, sink: &dyn EntrySink, ctx: &OpContext) -> Result<()> {
        let entry = self.entry();
        if !pick.contains(&entry) {
            return Ok(());
        }
        ctx.checkpoint()?;
        let decoder = stream::decoder(self.stream, self.source.reader()?, ctx.budget.memory)?;
        let mut data = Watched::new(decoder);
        sink.accept(&entry, &mut data)
            .map_err(|error| data.settle(&entry, error))
    }
}

/// One compressed file being written.
pub struct SingleCreator<'a> {
    output: Box<dyn Encoder + 'a>,
    written: bool,
}

impl<'a> SingleCreator<'a> {
    pub fn new(output: Box<dyn Encoder + 'a>) -> Self {
        Self {
            output,
            written: false,
        }
    }
}

impl ArchiveWriter for SingleCreator<'_> {
    fn add(&mut self, meta: &EntryMeta, data: &mut dyn Read, ctx: &OpContext) -> Result<()> {
        if self.written || !meta.is_file() {
            return Err(Error::new("singleFileOnly"));
        }
        self.written = true;
        copy(data, &mut self.output, ctx)
    }

    fn finish(self: Box<Self>) -> Result<()> {
        self.output.finish()?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_entry_is_named_after_the_file() {
        assert_eq!(inner_name("notes.txt.gz", Stream::Gzip), "notes.txt");
        assert_eq!(inner_name("NOTES.TXT.GZ", Stream::Gzip), "NOTES.TXT");
        assert_eq!(inner_name("dump.sql.zstd", Stream::Zstd), "dump.sql");
        assert_eq!(inner_name("a.bz2", Stream::Bzip2), "a");
        // The extension of another compression is part of the name.
        assert_eq!(inner_name("a.gz", Stream::Xz), "a.gz");
        assert_eq!(inner_name("backup", Stream::Lz4), "backup");
        assert_eq!(inner_name(".gz", Stream::Gzip), "data");
        assert_eq!(inner_name("İ.gz", Stream::Gzip), "İ");
    }
}

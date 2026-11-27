//! What every fuzz target does with the bytes it is given: reads them as an
//! archive the way BoxIt would, to the list and through the content.
//!
//! An error is the expected answer to most inputs. What the fuzzer looks
//! for is everything else: a panic, an overflow, a read out of bounds, an
//! allocation the input should not be able to ask for, a loop that does not
//! end.

use std::io::{self, Read};
use std::sync::atomic::{AtomicU64, Ordering};

use boxit_codecs::source::Source;
use boxit_codecs::{open_source, reader, OpenOptions};
use boxit_core::{
    ArchiveFormat, ArchiveReader, Budget, Entry, EntrySink, Error, OpContext, Password, Result,
    Selection,
};

/// The most that is read of one entry, and of all the entries of one input:
/// enough to go through every decoder, too little for an input made to
/// unpack into gigabytes to stall the fuzzer.
const ENTRY_LIMIT: u64 = 1024 * 1024;
const TOTAL_LIMIT: u64 = 16 * 1024 * 1024;

/// Memory the decoders may take. An input that asks for more must be
/// refused, not obeyed.
const MEMORY: u64 = 64 * 1024 * 1024;

/// The password every input is tried with, so the code behind encryption
/// is reached.
const PASSWORD: &str = "fuzz";

/// Reads the content it is given and keeps nothing.
struct Sink {
    left: AtomicU64,
}

impl EntrySink for Sink {
    fn accept(&self, _entry: &Entry, data: &mut dyn Read) -> Result<()> {
        let left = self.left.load(Ordering::Relaxed);
        if left == 0 {
            // Enough was read: the rest of the archive is more of the same.
            return Err(Error::cancelled());
        }
        let read = io::copy(&mut data.take(ENTRY_LIMIT.min(left)), &mut io::sink())?;
        self.left.fetch_sub(read.min(left), Ordering::Relaxed);
        Ok(())
    }
}

fn options() -> OpenOptions {
    OpenOptions {
        password: Some(Password::from(PASSWORD)),
        ..OpenOptions::default()
    }
}

/// Lists an archive and reads its content.
fn exercise(mut reader: Box<dyn ArchiveReader>) {
    // One thread: a finding must be the same on every run of its input.
    let ctx = OpContext {
        budget: Budget {
            threads: 1,
            memory: MEMORY,
        },
        ..OpContext::silent()
    };
    let _ = reader.entries(&ctx);
    let _ = reader.comment(&ctx);
    let sink = Sink {
        left: AtomicU64::new(TOTAL_LIMIT),
    };
    let _ = reader.extract(&Selection::All, &sink, &ctx);
}

/// Reads the bytes as an archive of a given format.
pub fn read_as(format: ArchiveFormat, data: &[u8]) {
    let source = Source::memory("fuzz", data.to_vec());
    if let Ok(reader) = reader(format, source, options()) {
        exercise(reader);
    }
}

/// Reads the bytes as whatever they look like: the way a file the user
/// opens is read, the recognition of its format included.
pub fn read_any(data: &[u8]) {
    if let Ok(opened) = open_source(Source::memory("fuzz", data.to_vec()), options()) {
        exercise(opened.reader);
    }
}

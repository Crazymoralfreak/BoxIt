//! The archive formats of BoxIt: reading and writing, behind the contract of
//! `boxit-core`. This is the only crate of the core that is allowed `unsafe`,
//! and it keeps it to three places: mapping an archive file into memory,
//! sharing the list of a ZIP between the threads that read it, and calling
//! UnRAR, the library in C++ that reads RAR.

use std::io::{Read, Seek, Write};
use std::path::{Path, PathBuf};

use boxit_core::format::starts_tar;
use boxit_core::{
    sniff, ArchiveFormat, ArchiveReader, ArchiveWriter, Budget, CreateOptions, Error, Method,
    OpContext, Password, Result, Signature, Stream, SNIFF_LENGTH,
};

use crate::cab::CabReader;
use crate::lha::LhaReader;
use crate::names::NameEncoding;
#[cfg(feature = "rar")]
use crate::rar::RarReader;
use crate::sevenz::{SevenZCreator, SevenZReader};
use crate::single::{SingleCreator, SingleReader};
use crate::source::Source;
use crate::stream::{Encoder, Plain, Settings};
use crate::tar_archive::{TarCreator, TarReader};
use crate::wim::WimReader;
use crate::zip_archive::{ZipCreator, ZipReader};

pub mod ar;
mod bytes;
pub mod cab;
pub mod chunked;
pub mod corpus;
pub mod cpio;
mod huffman;
pub mod iso;
pub mod lha;
mod lzms;
mod lzx;
pub mod names;
pub mod plan;
#[cfg(feature = "rar")]
pub mod rar;
pub mod sevenz;
pub mod single;
pub mod source;
pub mod stored;
pub mod stream;
mod support;
pub mod tar_archive;
mod udf;
pub mod volumes;
pub mod wim;
mod xpress;
pub mod zip_archive;
mod zip_scan;

// A failed step of a test names its format; the name is a constant.
#[cfg(test)]
#[allow(clippy::expect_fun_call)]
mod tests;

/// Size of a block of tar: enough of a decompressed stream to tell whether
/// it holds one.
const TAR_BLOCK: usize = 512;

/// What the user knows about an archive before it is opened.
#[derive(Clone, Debug, Default)]
pub struct OpenOptions {
    pub password: Option<Password>,
    /// How names that do not say they are UTF-8 are read.
    pub encoding: NameEncoding,
    /// A top-level domain such as `ru` or `jp`: where the user is from. It
    /// settles the guess between code pages that fit the names equally well.
    pub hint: Option<String>,
}

/// An archive opened for reading.
pub struct Opened {
    /// What the file turned out to be, whatever its name says.
    pub format: ArchiveFormat,
    pub reader: Box<dyn ArchiveReader>,
    /// Size of the archive: of all its volumes when it is cut into them.
    pub size: u64,
    /// The number of files the archive is in.
    pub volumes: usize,
    /// The file of the archive; the first one when it is in several.
    pub path: PathBuf,
}

/// Whether a compressed stream holds a tar archive: its first block tells,
/// and the name of the file where the block cannot.
fn holds_tar(source: &Source, stream: Stream, name: &str) -> bool {
    let named = ArchiveFormat::from_name(name).is_some_and(ArchiveFormat::is_tar);
    let mut block = Vec::with_capacity(TAR_BLOCK);
    let decoded = source
        .reader()
        .ok()
        // A block of a tar is asked for: a stream that needs more memory
        // than any modest budget for it is told by its name instead.
        .and_then(|reader| stream::decoder(stream, reader, Budget::DEFAULT_MEMORY).ok())
        .and_then(|decoder| decoder.take(TAR_BLOCK as u64).read_to_end(&mut block).ok());
    match decoded {
        // A tar without entries is nothing but zeros.
        Some(_) => starts_tar(&block) || (named && block.iter().all(|byte| *byte == 0)),
        None => named,
    }
}

/// Recognizes the format of an archive file by its content.
fn recognize(source: &Source) -> Result<ArchiveFormat> {
    let head = source.head(SNIFF_LENGTH)?;
    let name = source.name();
    let named = ArchiveFormat::from_name(&name);
    Ok(match sniff(&head) {
        Some(Signature::Stream(stream)) => {
            ArchiveFormat::of_stream(stream, holds_tar(source, stream, &name))
        }
        Some(signature) => signature.format(&name),
        // A ZIP keeps its list at its end: one that has something else at
        // its start, such as a program that unpacks it, is still a ZIP.
        None if named == Some(ArchiveFormat::Zip) => ArchiveFormat::Zip,
        // A tar without entries is nothing but zeros.
        None if named == Some(ArchiveFormat::Tar)
            && !head.is_empty()
            && head.iter().all(|byte| *byte == 0) =>
        {
            ArchiveFormat::Tar
        }
        None => return Err(Error::new("unknownFormat").with("name", name)),
    })
}

/// A reader of `source` as an archive of a known format. Nothing is read
/// until the list or the content is asked for.
pub fn reader(
    format: ArchiveFormat,
    source: Source,
    options: OpenOptions,
) -> Result<Box<dyn ArchiveReader>> {
    Ok(match format {
        ArchiveFormat::Zip => Box::new(ZipReader::new(source, options)),
        ArchiveFormat::SevenZ => Box::new(SevenZReader::new(source, options)),
        ArchiveFormat::Ar => Box::new(ar::reader(source, options)),
        ArchiveFormat::Cab => Box::new(CabReader::new(source, options)),
        ArchiveFormat::Cpio => Box::new(cpio::reader(source, options)),
        ArchiveFormat::Iso => Box::new(iso::reader(source, options)),
        ArchiveFormat::Lha => Box::new(LhaReader::new(source, options)),
        #[cfg(feature = "rar")]
        ArchiveFormat::Rar => Box::new(RarReader::new(source, options)),
        ArchiveFormat::Wim => Box::new(WimReader::new(source)),
        format if format.is_tar() => Box::new(TarReader::new(source, format.stream(), options)),
        format => match format.stream() {
            Some(stream) if format.is_single_file() => Box::new(SingleReader::new(source, stream)),
            _ => return Err(Error::new("unsupportedFormat").with("format", format.id())),
        },
    })
}

/// Opens `source` as whatever its content says it is.
pub fn open_source(source: Source, options: OpenOptions) -> Result<Opened> {
    let format = recognize(&source)?;
    let (size, volumes) = (source.size(), source.volumes());
    let path = source.path().to_path_buf();
    let reader = reader(format, source, options)?;
    Ok(Opened {
        format,
        reader,
        size,
        volumes,
        path,
    })
}

/// Opens an archive file. Nothing but its first bytes is read until the
/// list or the content is asked for.
pub fn open(path: &Path, options: OpenOptions) -> Result<Opened> {
    open_source(Source::open(path)?, options)
}

/// A damaged archive opened to save what is left of it.
pub struct Salvaged {
    pub opened: Opened,
    /// The list of the archive did not read: it was made anew from what
    /// the entries say of themselves.
    pub rebuilt: bool,
    /// The names of the entries that are cut short and are in no list.
    pub lost: Vec<String>,
}

/// Whether what is left of a damaged archive of a format can be written
/// as an archive again.
pub fn can_repair(format: ArchiveFormat) -> bool {
    matches!(format, ArchiveFormat::Zip | ArchiveFormat::SevenZ)
}

/// Opens an archive that may be damaged, to be written anew without what
/// does not read (see [`ArchiveReader::rewrite`]). A ZIP that lost its
/// list gets one made from the headers of its entries; a 7z has to have
/// its list, for its entries have no headers of their own.
pub fn salvage(path: &Path, options: OpenOptions, ctx: &OpContext) -> Result<Salvaged> {
    let source = Source::open(path)?;
    let format = recognize(&source)?;
    let (size, volumes) = (source.size(), source.volumes());
    let path = source.path().to_path_buf();
    let (reader, rebuilt, lost): (Box<dyn ArchiveReader>, _, _) = match format {
        ArchiveFormat::Zip => {
            let (reader, rebuilt, lost) = ZipReader::salvage(source, options, ctx)?;
            (Box::new(reader), rebuilt, lost)
        }
        ArchiveFormat::SevenZ => (
            Box::new(SevenZReader::new(source, options)),
            false,
            Vec::new(),
        ),
        format => return Err(Error::new("cannotRepair").with("format", format.id())),
    };
    Ok(Salvaged {
        opened: Opened {
            format,
            reader,
            size,
            volumes,
            path,
        },
        rebuilt,
        lost,
    })
}

/// The methods a format can be created with, the usual one first; empty for
/// a format that has no choice.
pub fn methods(format: ArchiveFormat) -> &'static [Method] {
    match format {
        ArchiveFormat::Zip => zip_archive::METHODS,
        ArchiveFormat::SevenZ => sevenz::METHODS,
        _ => &[],
    }
}

/// Whether archives of a format can be encrypted.
pub fn can_encrypt(format: ArchiveFormat) -> bool {
    matches!(format, ArchiveFormat::Zip | ArchiveFormat::SevenZ)
}

/// Starts an archive in `output`. The budget is what the encoders of the
/// whole stream may take; the formats that compress entry by entry follow
/// the budget of each call.
pub fn create<'a>(
    output: impl Write + Seek + Send + 'a,
    options: &CreateOptions,
    budget: Budget,
) -> Result<Box<dyn ArchiveWriter + 'a>> {
    let format = options.format;
    if !format.can_write() {
        return Err(Error::new("cannotCreate").with("format", format.id()));
    }
    let locked = options
        .password
        .as_ref()
        .is_some_and(|password| !password.is_empty());
    if locked && !can_encrypt(format) {
        return Err(Error::new("encryptionUnsupported").with("format", format.id()));
    }
    Ok(match format {
        ArchiveFormat::Zip => Box::new(ZipCreator::new(output, options)?),
        ArchiveFormat::SevenZ => Box::new(SevenZCreator::new(output, options)?),
        _ => {
            let encoder: Box<dyn Encoder + 'a> = match format.stream() {
                Some(stream) => {
                    let settings = Settings {
                        level: options.level,
                        dictionary: plan::dictionary(options),
                        budget,
                    };
                    stream::encoder(stream, output, &settings)?
                }
                None => Box::new(Plain(output)),
            };
            if format.is_tar() {
                Box::new(TarCreator::new(encoder))
            } else {
                Box::new(SingleCreator::new(encoder))
            }
        }
    })
}

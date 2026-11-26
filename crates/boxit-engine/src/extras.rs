//! The extra operations: convert, compare, search, and a size estimate.
//!
//! A conversion streams each file from the archive that is read into the
//! archive that is written. Nothing is unpacked onto the disk.

use std::io::{self, BufWriter, Read, Write};
use std::path::PathBuf;
use std::sync::{Arc, Mutex, PoisonError};
use std::thread;

use boxit_codecs::OpenOptions;
use boxit_core::{
    Budget, CreateOptions, Entry, EntrySink, Error, Item, OpContext, Result, Selection,
};
use globset::GlobBuilder;
use regex::Regex;
use serde::Serialize;

use crate::archive::Archive;
use crate::create::{self, OUTPUT_BUFFER};
use crate::fsx;

/// Where a conversion reads and where it writes.
pub struct ConvertRequest {
    pub source: PathBuf,
    pub destination: PathBuf,
    /// How the new archive is compressed. Its format is the format of the
    /// new file.
    pub options: CreateOptions,
    pub overwrite: bool,
}

/// What a conversion wrote. The same numbers a creation reports.
pub use crate::create::Created;

/// Copies an archive into another format. The source is not changed.
pub fn convert(
    request: &ConvertRequest,
    open: OpenOptions,
    ctx: &OpContext,
) -> Result<Created> {
    if !request.options.format.can_write() {
        return Err(Error::new("cannotCreate").with("format", request.options.format.id()));
    }
    let source = fsx::long(&request.source)?;
    let destination = fsx::long(&request.destination)?;
    if fsx::same_path(&source, &destination) {
        return Err(Error::new("sameAsSource"));
    }
    let mut archive = Archive::open(&source, open.clone())?;
    let entries = archive.entries(ctx)?;
    drop(archive);
    let mut failures: Vec<Arc<Mutex<Option<Error>>>> = Vec::new();
    let mut items = Vec::with_capacity(entries.len());
    let mut files = 0u64;
    let mut folders = 0u64;
    let mut bytes = 0u64;
    for entry in &entries {
        if entry.meta.is_dir() {
            folders += 1;
            items.push(Item::bare(entry.meta.clone()));
            continue;
        }
        if !entry.meta.is_file() {
            items.push(Item::bare(entry.meta.clone()));
            continue;
        }
        files += 1;
        bytes += entry.meta.size.unwrap_or(0);
        let failed = Arc::new(Mutex::new(None));
        failures.push(Arc::clone(&failed));
        items.push(Item::new(
            entry.meta.clone(),
            FromArchive {
                path: source.clone(),
                index: entry.index,
                options: open.clone(),
                failed,
                ctx: ctx.clone(),
            },
        ));
    }
    if items.is_empty() {
        return Err(Error::new("nothingToAdd"));
    }
    ctx.progress.total(Some(bytes), Some(items.len() as u64));
    let mut partial = create::stage(&destination, &destination, request.overwrite)?;
    let failed_io = |error| Error::from(error).with("path", fsx::display(&destination));
    let mut file = std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&partial.path)
        .map_err(failed_io)?;
    {
        let mut output = BufWriter::with_capacity(OUTPUT_BUFFER, &mut file);
        let mut writer = boxit_codecs::create(&mut output, &request.options, ctx.budget)?;
        writer.add_all(items, ctx)?;
        writer.finish()?;
        ctx.checkpoint()?;
        output.flush().map_err(failed_io)?;
    }
    for failed in &failures {
        if let Some(error) = failed.lock().unwrap_or_else(PoisonError::into_inner).take() {
            return Err(error);
        }
    }
    file.sync_all().map_err(failed_io)?;
    let packed = file.metadata().map_err(failed_io)?.len();
    drop(file);
    std::fs::rename(&partial.path, &destination).map_err(failed_io)?;
    partial.placed = true;
    Ok(Created {
        path: fsx::display(&destination),
        files,
        folders,
        links: 0,
        bytes,
        packed,
        volumes: 1,
    })
}

/// One file of the source, read when the writer asks for it.
struct FromArchive {
    path: PathBuf,
    index: u64,
    options: OpenOptions,
    failed: Arc<Mutex<Option<Error>>>,
    ctx: OpContext,
}

impl boxit_core::DataSource for FromArchive {
    fn open(&self) -> Result<Box<dyn Read + Send + '_>> {
        let (reader, writer) = io::pipe().map_err(Error::from)?;
        let path = self.path.clone();
        let index = self.index;
        let options = self.options.clone();
        let failed = Arc::clone(&self.failed);
        let ctx = self.ctx.clone();
        thread::spawn(move || {
            let outcome = (|| -> Result<()> {
                let mut opened = boxit_codecs::open(&path, options)?;
                let sink = Pump {
                    index,
                    writer: Mutex::new(writer),
                };
                opened
                    .reader
                    .extract(&Selection::indexes(vec![index]), &sink, &ctx)?;
                Ok(())
            })();
            if let Err(error) = outcome {
                *failed.lock().unwrap_or_else(PoisonError::into_inner) = Some(error);
            }
        });
        Ok(Box::new(reader))
    }
}

struct Pump {
    index: u64,
    writer: Mutex<io::PipeWriter>,
}

impl EntrySink for Pump {
    fn accept(&self, entry: &Entry, data: &mut dyn Read) -> Result<()> {
        if entry.index != self.index {
            return Ok(());
        }
        let mut writer = self.writer.lock().unwrap_or_else(PoisonError::into_inner);
        io::copy(data, &mut *writer).map(|_| ()).map_err(Error::from)
    }
}

/// One file that is not the same in the two archives.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Different {
    pub path: String,
    pub left_size: Option<u64>,
    pub right_size: Option<u64>,
    pub left_crc32: Option<u32>,
    pub right_crc32: Option<u32>,
}

/// What comparing two archives found.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Compared {
    /// Paths that are only in the first archive.
    pub only_left: Vec<String>,
    /// Paths that are only in the second archive.
    pub only_right: Vec<String>,
    pub different: Vec<Different>,
}

/// Compares two archives by path, size and CRC. Folders are compared by path.
fn files(entries: &[Entry]) -> std::collections::BTreeMap<String, &Entry> {
    let mut map = std::collections::BTreeMap::new();
    for entry in entries {
        if entry.meta.is_dir() {
            continue;
        }
        map.insert(entry.meta.path.trim_end_matches('/').to_owned(), entry);
    }
    map
}

pub fn compare(left: &[Entry], right: &[Entry]) -> Compared {
    let left_map = files(left);
    let right_map = files(right);
    let mut compared = Compared {
        only_left: Vec::new(),
        only_right: Vec::new(),
        different: Vec::new(),
    };
    for (path, entry) in &left_map {
        match right_map.get(path) {
            None => compared.only_left.push(path.clone()),
            Some(other) if entry.meta.size != other.meta.size || entry.crc32 != other.crc32 => {
                compared.different.push(Different {
                    path: path.clone(),
                    left_size: entry.meta.size,
                    right_size: other.meta.size,
                    left_crc32: entry.crc32,
                    right_crc32: other.crc32,
                });
            }
            Some(_) => {}
        }
    }
    for path in right_map.keys() {
        if !left_map.contains_key(path) {
            compared.only_right.push(path.clone());
        }
    }
    compared
}

/// Whether `name` is what `needle` asks for.
///
/// A needle that starts with `re:` is a regular expression. One that
/// contains `*` or `?` is a mask. Anything else is a piece of the name,
/// whatever the case of the letters. A pattern that cannot be read matches
/// nothing.
pub fn name_matches(name: &str, needle: &str) -> bool {
    let needle = needle.trim();
    if needle.is_empty() {
        return true;
    }
    if let Some(pattern) = needle.strip_prefix("re:") {
        return Regex::new(pattern).is_ok_and(|regex| regex.is_match(name));
    }
    if needle.contains('*') || needle.contains('?') {
        return GlobBuilder::new(needle)
            .case_insensitive(true)
            .build()
            .is_ok_and(|glob| glob.compile_matcher().is_match(name));
    }
    name.to_lowercase().contains(&needle.to_lowercase())
}

/// The entries whose path `needle` asks for. See [`name_matches`].
pub fn find<'a>(entries: &'a [Entry], needle: &str) -> Result<Vec<&'a Entry>> {
    let needle = needle.trim();
    if let Some(pattern) = needle.strip_prefix("re:") {
        Regex::new(pattern).map_err(|error| Error::new("badPattern").detail(error.to_string()))?;
    } else if needle.contains('*') || needle.contains('?') {
        GlobBuilder::new(needle)
            .case_insensitive(true)
            .build()
            .map_err(|error| Error::new("badPattern").detail(error.to_string()))?;
    }
    Ok(entries
        .iter()
        .filter(|entry| name_matches(&entry.meta.path, needle))
        .collect())
}

/// What compressing a sample says about the whole set.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Estimate {
    /// Bytes of the files that were actually compressed.
    pub sample_bytes: u64,
    /// Size of the archive those files made.
    pub sample_packed: u64,
    /// Bytes of every file that was asked about.
    pub total_bytes: u64,
    /// The size the whole set would take, at the ratio of the sample.
    pub packed: u64,
    /// A smaller and a larger size the result may land between.
    pub low: u64,
    pub high: u64,
}

/// Compresses a sample of `sources` with `options` and extends the ratio to
/// all of them. The range is wider when the sample is a small part of the set.
pub fn estimate(sources: &[PathBuf], options: &CreateOptions, ctx: &OpContext) -> Result<Estimate> {
    let gathered = create::gather(sources, &[], ctx)?;
    let total_bytes = gathered.bytes;
    if total_bytes == 0 {
        return Err(Error::new("nothingToAdd"));
    }
    let budget = total_bytes.clamp(64 * 1024, 4 * 1024 * 1024);
    let mut sample = Vec::new();
    let mut sample_bytes = 0u64;
    for item in gathered.items {
        let size = item.meta.size.unwrap_or(0);
        let file = item.meta.is_file();
        if file && sample_bytes > 0 && sample_bytes + size > budget {
            continue;
        }
        if file {
            sample_bytes += size;
        }
        sample.push(item);
        if sample_bytes >= budget {
            break;
        }
    }
    if sample_bytes == 0 {
        return Err(Error::new("nothingToAdd"));
    }
    let mut output = BufWriter::new(std::io::Cursor::new(Vec::new()));
    let mut writer = boxit_codecs::create(&mut output, options, Budget::default())?;
    writer.add_all(sample, ctx)?;
    writer.finish()?;
    let sample_packed = output.get_ref().get_ref().len() as u64;
    let packed = (u128::from(total_bytes) * u128::from(sample_packed) / u128::from(sample_bytes)) as u64;
    // A small sample says less about the rest.
    let slack = if sample_bytes * 5 >= total_bytes { 8 } else { 20 };
    let low = packed.saturating_mul(100 - slack) / 100;
    let high = packed.saturating_mul(100 + slack) / 100;
    Ok(Estimate {
        sample_bytes,
        sample_packed,
        total_bytes,
        packed,
        low,
        high,
    })
}

#[cfg(test)]
mod tests;

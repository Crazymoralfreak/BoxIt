//! Creation of an archive from files and folders: all of it, or nothing.
//!
//! The archive is written under a temporary name next to its destination
//! and renamed when its last byte is on the disk. A failure, a cancellation
//! or a file that cannot be read half way through leaves no archive at all,
//! and an archive that was to be replaced stays as it was.
//!
//! The folders are read on all the threads of the operation before anything
//! is written, so the progress knows its total from the start and the format
//! gets the whole set at once: that is what lets it compress files side by
//! side and order them for a solid block.

use std::collections::HashSet;
use std::env;
use std::fs::{self, DirEntry, File, Metadata, OpenOptions};
use std::io::{self, BufWriter, Read, Seek, Write};
use std::path::{Path, PathBuf};
use std::sync::Arc;

use boxit_codecs::volumes;
use boxit_core::{
    ArchiveFormat, CreateOptions, DataSource, EntryKind, EntryMeta, Error, Item, OpContext,
    Progress, Result,
};
use rayon::prelude::*;
use rayon::ThreadPoolBuilder;
use serde::Serialize;

use crate::extract::STAGING_PREFIX;
use crate::fsx::{self, Existing};
use crate::guarded::guarded;

pub use self::output::Layout;
use self::output::{Cut, Tail};

mod output;

/// Size of the buffer between the format and the archive file.
pub(crate) const OUTPUT_BUFFER: usize = 1024 * 1024;

pub struct CreateRequest {
    /// The files and folders to store. Each goes into the archive under its
    /// own name; a folder brings everything inside it.
    pub sources: Vec<PathBuf>,
    /// The archive file. An archive cut into volumes is named as it would
    /// be whole: its volumes add their numbers to the name.
    pub destination: PathBuf,
    pub options: CreateOptions,
    /// Replace the file that stands at the destination.
    pub overwrite: bool,
    pub layout: Layout,
}

impl CreateRequest {
    /// An archive in one file, which replaces nothing.
    pub fn new(sources: Vec<PathBuf>, destination: PathBuf, options: CreateOptions) -> Self {
        Self {
            sources,
            destination,
            options,
            overwrite: false,
            layout: Layout::Whole,
        }
    }
}

/// What a creation did.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Created {
    /// The archive file; the first of its volumes when it is cut into them.
    pub path: String,
    pub files: u64,
    pub folders: u64,
    pub links: u64,
    /// Bytes of content stored.
    pub bytes: u64,
    /// Size of the archive: of all the files it is in.
    pub packed: u64,
    /// The number of files the archive is in.
    pub volumes: u64,
}

/// The formats an archive that unpacks itself can be of.
pub fn unpacks_itself(format: ArchiveFormat) -> bool {
    matches!(format, ArchiveFormat::Zip | ArchiveFormat::SevenZ)
}

/// The name of the file of the module: the program that unpacks the
/// archive put after it.
pub const MODULE: &str = "boxit-sfx.exe";

/// Where the module is looked for: next to the program that runs, or in
/// the resources folder an installer puts beside it.
pub fn module() -> PathBuf {
    let beside = |folder: &Path| {
        let direct = folder.join(MODULE);
        if direct.is_file() {
            return Some(direct);
        }
        let nested = folder.join("resources").join(MODULE);
        nested.is_file().then_some(nested)
    };
    env::current_exe()
        .ok()
        .and_then(|program| program.parent().and_then(beside))
        .unwrap_or_else(|| PathBuf::from(MODULE))
}

/// An error of the disk, with the place it happened at.
fn at(error: io::Error, path: &Path) -> Error {
    Error::from(error).with("path", fsx::display(path))
}

/// A file on the disk as the content of an entry. It is opened when the
/// format gets to it and not before: a tree of a million files does not
/// hold a million handles.
struct DiskFile {
    path: PathBuf,
    name: String,
    progress: Arc<dyn Progress>,
}

struct Counted<'a> {
    file: File,
    source: &'a DiskFile,
}

impl Read for Counted<'_> {
    fn read(&mut self, buffer: &mut [u8]) -> io::Result<usize> {
        match self.file.read(buffer) {
            Ok(count) => {
                self.source.progress.advance(count as u64);
                Ok(count)
            }
            Err(error) if error.kind() == io::ErrorKind::Interrupted => Err(error),
            // The format passes the error on; the name of the file goes
            // with it.
            Err(error) => Err(at(error, &self.source.path).into_io()),
        }
    }
}

impl DataSource for DiskFile {
    fn open(&self) -> Result<Box<dyn Read + Send + '_>> {
        self.progress.entry(&self.name);
        let file = fsx::open_sequential(&self.path).map_err(|error| at(error, &self.path))?;
        Ok(Box::new(Counted { file, source: self }))
    }
}

/// Something found on the disk, to be stored.
struct Found {
    meta: EntryMeta,
    path: PathBuf,
}

struct Walk<'a> {
    /// Paths that are not stored: the archive being written.
    skip: &'a [PathBuf],
    ctx: &'a OpContext,
}

impl Walk<'_> {
    /// What stands at `path`, to be stored as `name`, with everything
    /// inside it.
    fn visit(&self, path: &Path, name: String, metadata: &Metadata) -> Result<Vec<Found>> {
        self.ctx.checkpoint()?;
        if self
            .skip
            .iter()
            .any(|skipped| fsx::same_path(skipped, path))
        {
            return Ok(Vec::new());
        }
        let meta = fsx::describe(path, name, metadata).map_err(|error| at(error, path))?;
        let mut found = if meta.is_dir() {
            self.inside(path, &format!("{}/", meta.path))?
        } else {
            Vec::new()
        };
        found.push(Found {
            meta,
            path: path.to_path_buf(),
        });
        Ok(found)
    }

    /// The content of a folder, its parts read side by side.
    fn inside(&self, folder: &Path, prefix: &str) -> Result<Vec<Found>> {
        let children = fs::read_dir(folder)
            .and_then(Iterator::collect::<io::Result<Vec<DirEntry>>>)
            .map_err(|error| at(error, folder))?;
        children
            .par_iter()
            .map(|child| {
                let path = child.path();
                // What the listing of the folder already knows of the file.
                let metadata = child.metadata().map_err(|error| at(error, &path))?;
                let name = format!("{prefix}{}", child.file_name().to_string_lossy());
                self.visit(&path, name, &metadata)
            })
            .try_reduce(Vec::new, |mut all, mut part| {
                all.append(&mut part);
                Ok(all)
            })
    }

    /// Everything the sources bring, in the order of the archive: a folder
    /// before its content, names by the alphabet.
    fn all(&self, sources: &[PathBuf]) -> Result<Vec<Found>> {
        let mut taken = HashSet::new();
        let mut found = Vec::new();
        for source in sources {
            let path = fsx::long(source)?;
            let metadata = fs::symlink_metadata(&path).map_err(|error| at(error, &path))?;
            let Some(name) = path.file_name() else {
                // A whole disk: its content, without a folder around it.
                found.append(&mut self.inside(&path, "")?);
                continue;
            };
            let name = name.to_string_lossy().into_owned();
            if !taken.insert(name.to_lowercase()) {
                return Err(Error::new("duplicateName").with("name", name));
            }
            found.append(&mut self.visit(&path, name, &metadata)?);
        }
        found.sort_by(|one, other| one.meta.path.split('/').cmp(other.meta.path.split('/')));
        Ok(found)
    }
}

/// The archive under its temporary name. Dropping it removes the file
/// unless it reached its place.
pub(crate) struct Partial {
    pub path: PathBuf,
    pub placed: bool,
}

impl Drop for Partial {
    fn drop(&mut self) {
        if !self.placed {
            if let Err(error) = fsx::remove_any(&self.path) {
                tracing::warn!(file = %fsx::display(&self.path), "a temporary file was left behind: {error}");
            }
        }
    }
}

/// Where the archive goes, checked: the destination in its long form, the
/// file that will stand there first, and the temporary name next to it.
fn output(request: &CreateRequest) -> Result<(PathBuf, PathBuf, Partial)> {
    let destination = fsx::long(&request.destination)?;
    let first = match request.layout {
        Layout::Volumes(_) => volumes::path(&destination, 1),
        Layout::Whole | Layout::SelfExtracting(_) => destination.clone(),
    };
    let partial = stage(&destination, &first, request.overwrite)?;
    Ok((destination, first, partial))
}

/// A temporary name next to `destination` for an archive to be written
/// under, once the place is found fit: `first` is the file that will
/// stand there first, and nothing stands there unless it may be replaced.
pub(crate) fn stage(destination: &Path, first: &Path, overwrite: bool) -> Result<Partial> {
    let shown = || fsx::display(first);
    match fsx::existing(first) {
        Existing::Folder => return Err(Error::new("existingFolder").with("path", shown())),
        Existing::File | Existing::Link if !overwrite => {
            return Err(Error::new("exists").with("path", shown()));
        }
        Existing::Nothing | Existing::File | Existing::Link => {}
    }
    let folder = destination
        .parent()
        .filter(|_| destination.file_name().is_some())
        .ok_or_else(|| Error::new("badOutput").with("path", shown()))?;
    fs::create_dir_all(folder).map_err(|error| at(error, folder))?;
    Ok(Partial {
        path: folder.join(format!("{STAGING_PREFIX}{}.part", fsx::unique_suffix())),
        placed: false,
    })
}

/// Writes the archive into what stands for its file, and gives that back
/// with everything the format wrote in it.
fn write<W: Write + Seek + Send>(
    output: W,
    items: Vec<Item>,
    options: &CreateOptions,
    ctx: &OpContext,
) -> Result<W> {
    let mut output = BufWriter::with_capacity(OUTPUT_BUFFER, output);
    let mut writer = boxit_codecs::create(&mut output, options, ctx.budget)?;
    writer.add_all(items, ctx)?;
    writer.finish()?;
    ctx.checkpoint()?;
    output
        .into_inner()
        .map_err(|error| Error::from(error.into_error()))
}

/// What a walk of the disk found, ready to be stored.
pub(crate) struct Gathered {
    pub items: Vec<Item>,
    pub files: u64,
    pub folders: u64,
    pub links: u64,
    pub bytes: u64,
}

/// The sources as entries, skipping the paths in `skip`. An empty result is
/// not an error: the caller decides whether nothing to add is a failure.
pub(crate) fn gather(sources: &[PathBuf], skip: &[PathBuf], ctx: &OpContext) -> Result<Gathered> {
    let walk = Walk { skip, ctx };
    let found = ThreadPoolBuilder::new()
        .num_threads(ctx.budget.threads)
        .build()
        .map_err(|error| Error::new("internal").detail(error))?
        .install(|| walk.all(sources))?;
    let mut gathered = Gathered {
        items: Vec::with_capacity(found.len()),
        files: 0,
        folders: 0,
        links: 0,
        bytes: 0,
    };
    for Found { meta, path } in found {
        match meta.kind {
            EntryKind::File => {
                gathered.files += 1;
                gathered.bytes += meta.size.unwrap_or(0);
                let data = DiskFile {
                    path,
                    name: meta.path.clone(),
                    progress: ctx.progress.clone(),
                };
                gathered.items.push(Item::new(meta, data));
            }
            EntryKind::Directory => {
                gathered.folders += 1;
                gathered.items.push(Item::bare(meta));
            }
            EntryKind::Symlink | EntryKind::Hardlink => {
                gathered.links += 1;
                gathered.items.push(Item::bare(meta));
            }
        }
    }
    Ok(gathered)
}

/// Creates an archive of the sources. Either the whole archive appears at
/// the destination, or nothing changes there.
pub fn create(request: &CreateRequest, ctx: &OpContext) -> Result<Created> {
    if request.sources.is_empty() {
        return Err(Error::new("nothingToAdd"));
    }
    if matches!(request.layout, Layout::SelfExtracting(_))
        && !unpacks_itself(request.options.format)
    {
        return Err(Error::new("cannotUnpackItself").with("format", request.options.format.id()));
    }
    let (destination, first, mut partial) = output(request)?;
    let mut ctx = ctx.clone();
    if request.options.threads > 0 {
        ctx.budget.threads = request.options.threads;
    }
    let ctx = &ctx;
    let mut created = Created {
        path: fsx::display(&first),
        volumes: 1,
        ..Created::default()
    };
    // A panic in a format library becomes an error, and dropping `partial`
    // removes what was written.
    guarded(|| {
        // The archive is not stored in itself, nor the one it replaces.
        let mut skip = vec![destination.clone(), partial.path.clone()];
        if matches!(request.layout, Layout::Volumes(_)) {
            skip.append(&mut output::standing(&destination));
        }
        let gathered = gather(&request.sources, &skip, ctx)?;
        if gathered.items.is_empty() {
            return Err(Error::new("nothingToAdd"));
        }
        created.files = gathered.files;
        created.folders = gathered.folders;
        created.links = gathered.links;
        created.bytes = gathered.bytes;
        let items = gathered.items;
        ctx.progress
            .total(Some(created.bytes), Some(items.len() as u64));

        let failed = |error: io::Error| at(error, &first);
        let options = &request.options;
        if let Layout::Volumes(size) = request.layout {
            fs::create_dir(&partial.path).map_err(failed)?;
            let cut = Cut::new(partial.path.clone(), size)?;
            let cut = write(cut, items, options, ctx)
                .map_err(|error| error.with("path", fsx::display(&first)))?;
            (created.volumes, created.packed) = cut.finish().map_err(failed)?;
            output::place(&partial.path, created.volumes, &destination).map_err(failed)?;
            // The volumes are in their places; what is left is the folder
            // they were written in, with the volumes they replaced.
            partial.placed = true;
            if let Err(error) = fsx::remove_any(&partial.path) {
                tracing::warn!(folder = %fsx::display(&partial.path), "a temporary folder was left behind: {error}");
            }
            return Ok(());
        }
        let mut file = OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&partial.path)
            .map_err(failed)?;
        let file = if let Layout::SelfExtracting(module) = &request.layout {
            let mut module = fsx::open_sequential(module)
                .map_err(|error| Error::new("moduleMissing").detail(error))?;
            io::copy(&mut module, &mut file).map_err(failed)?;
            let tail = Tail::new(file).map_err(failed)?;
            write(tail, items, options, ctx)?.into_file()
        } else {
            write(file, items, options, ctx)?
        };
        // The rename must not come before the data: after a power failure
        // an archive with the right name and no content is worse than none.
        file.sync_all().map_err(failed)?;
        created.packed = file.metadata()?.len();
        drop(file);
        fs::rename(&partial.path, &destination).map_err(failed)?;
        partial.placed = true;
        Ok(())
    })?;
    Ok(created)
}

#[cfg(test)]
mod tests;

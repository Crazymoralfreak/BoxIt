//! Changing an archive that already exists: add, remove, rename.
//!
//! ZIP and 7z are written into a new file by copying what stays as it is
//! already compressed. A compressed tar is packed again from its content.
//! Either the new archive appears whole, or the one that was there stays.

use std::collections::{BTreeMap, BTreeSet, HashSet};
use std::fs::{self, OpenOptions as FileOptions};
use std::io::{BufWriter, Write};
use std::path::{Path, PathBuf};

use boxit_codecs::OpenOptions;
use boxit_core::{
    Changes, ConflictPolicy, CreateOptions, Entry, Error, ExtractOptions, OpContext, Result,
    Selection, SymlinkPolicy,
};
use serde::Serialize;

use crate::archive::Archive;
use crate::create::{self, CreateRequest, Layout, OUTPUT_BUFFER};
use crate::fsx;

pub struct UpdateRequest {
    /// The archive to change.
    pub archive: PathBuf,
    /// Where the changed archive goes. The same file as `archive` replaces
    /// it only when the new one is complete.
    pub destination: PathBuf,
    /// Entries to leave out, by their place in the archive.
    pub removed: BTreeSet<u64>,
    /// New paths of entries, by their place in the archive.
    pub renamed: BTreeMap<u64, String>,
    /// Files and folders to add. Each is stored under its own name.
    pub added: Vec<PathBuf>,
    /// Entries replaced by a file from the disk, keeping the name they had.
    pub replaced: Vec<(u64, PathBuf)>,
}

/// What a change of an archive did.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Updated {
    pub path: String,
    pub format: boxit_core::ArchiveFormat,
    /// The whole archive was packed again, because its format cannot copy
    /// a compressed entry into a new file.
    pub repacked: bool,
    pub files: u64,
    pub folders: u64,
    pub bytes: u64,
    pub packed: u64,
}

/// Writes the changed archive. The one that was read is replaced only
/// after the new one is complete, and only when the destination is that file.
pub fn update(request: &UpdateRequest, options: OpenOptions, ctx: &OpContext) -> Result<Updated> {
    if request.removed.is_empty()
        && request.renamed.is_empty()
        && request.added.is_empty()
        && request.replaced.is_empty()
    {
        return Err(Error::new("nothingToAdd"));
    }
    let source = fsx::long(&request.archive)?;
    let destination = fsx::long(&request.destination)?;
    let mut archive =
        Archive::open(&source, options.clone()).map_err(|error| match error.code.as_str() {
            "notFound" | "accessDenied" => error.with("path", fsx::display(&source)),
            _ => error,
        })?;
    let format = archive.format();
    if !format.can_write() {
        return Err(Error::new("cannotModify").with("format", format.id()));
    }
    let entries = archive.entries(ctx)?;
    let removed = with_children(&entries, &request.removed);
    let gathered = create::gather(&request.added, &[source.clone(), destination.clone()], ctx)?;
    let mut changes = Changes::none(format);
    changes.removed = removed;
    changes.renamed.clone_from(&request.renamed);
    changes.added = gathered.items;
    let mut replaced_bytes = 0u64;
    for (index, disk) in &request.replaced {
        let Some(previous) = entries.iter().find(|entry| entry.index == *index) else {
            continue;
        };
        let mut brought = create::gather(std::slice::from_ref(disk), &[], ctx)?;
        if let Some(item) = brought.items.first_mut() {
            let path = previous.meta.path.clone();
            item.meta.path.clone_from(&path);
        }
        replaced_bytes += brought.bytes;
        changes.removed.insert(*index);
        changes.added.append(&mut brought.items);
    }
    changes.options = CreateOptions::new(format);

    let mut partial = create::stage(&destination, &destination, true)?;
    let failed = |error| Error::from(error).with("path", fsx::display(&destination));
    let mut file = FileOptions::new()
        .write(true)
        .create_new(true)
        .open(&partial.path)
        .map_err(failed)?;
    let mut output = BufWriter::with_capacity(OUTPUT_BUFFER, &mut file);
    let copied = archive.rewrite(changes, &mut output, ctx);
    if let Err(error) = copied {
        drop(output);
        drop(file);
        drop(partial);
        if error.code == "cannotModify" {
            drop(archive);
            return repack(
                request,
                &source,
                &destination,
                &entries,
                format,
                options,
                ctx,
            );
        }
        return Err(error);
    }
    ctx.checkpoint()?;
    output.flush().map_err(failed)?;
    drop(output);
    file.sync_all().map_err(failed)?;
    let packed = file.metadata().map_err(failed)?.len();
    drop(file);
    // The archive that was read may be the one replaced: it has to be
    // closed before the new file takes its name.
    drop(archive);
    fs::rename(&partial.path, &destination).map_err(failed)?;
    partial.placed = true;
    let kept = entries
        .iter()
        .filter(|entry| !request.removed.contains(&entry.index));
    Ok(Updated {
        path: fsx::display(&destination),
        format,
        repacked: false,
        files: kept.clone().filter(|entry| entry.meta.is_file()).count() as u64 + gathered.files,
        folders: kept.filter(|entry| entry.meta.is_dir()).count() as u64 + gathered.folders,
        bytes: entries
            .iter()
            .filter(|entry| !request.removed.contains(&entry.index))
            .filter_map(|entry| entry.meta.size)
            .sum::<u64>()
            + gathered.bytes
            + replaced_bytes,
        packed,
    })
}

/// A removed folder takes its content with it.
fn with_children(entries: &[Entry], removed: &BTreeSet<u64>) -> BTreeSet<u64> {
    let mut roots = HashSet::new();
    for entry in entries {
        if removed.contains(&entry.index) {
            roots.insert(entry.meta.path.trim_end_matches(['/', '\\']).to_owned());
        }
    }
    let mut all = removed.clone();
    for entry in entries {
        let path = entry.meta.path.trim_end_matches(['/', '\\']);
        let inside = roots.iter().any(|root| {
            path == root
                || path
                    .strip_prefix(root)
                    .is_some_and(|rest| rest.starts_with('/'))
        });
        if inside {
            all.insert(entry.index);
        }
    }
    all
}

/// Packs the archive again from what it holds. Used for a compressed tar,
/// which has no way to copy one entry as it is compressed.
fn repack(
    request: &UpdateRequest,
    source: &Path,
    destination: &Path,
    entries: &[Entry],
    format: boxit_core::ArchiveFormat,
    options: OpenOptions,
    ctx: &OpContext,
) -> Result<Updated> {
    let removed = with_children(entries, &request.removed);
    let kept: Vec<u64> = entries
        .iter()
        .filter(|entry| !removed.contains(&entry.index))
        .map(|entry| entry.index)
        .collect();
    if kept.is_empty() && request.added.is_empty() {
        return Err(Error::new("nothingToAdd"));
    }
    let folder = destination
        .parent()
        .unwrap_or(Path::new("."))
        .join(format!(".boxit-repack-{}", fsx::unique_suffix()));
    fs::create_dir_all(&folder)
        .map_err(|error| Error::from(error).with("path", fsx::display(&folder)))?;
    let cleanup = TempFolder(folder.clone());
    if !kept.is_empty() {
        let mut archive = Archive::open(source, options)?;
        archive.extract(
            &folder,
            Selection::indexes(kept),
            ExtractOptions {
                conflicts: ConflictPolicy::Overwrite,
                symlinks: SymlinkPolicy::Skip,
                ..ExtractOptions::default()
            },
            ctx,
        )?;
        drop(archive);
        for (index, name) in &request.renamed {
            let Some(entry) = entries.iter().find(|entry| entry.index == *index) else {
                continue;
            };
            let from = under(&folder, &entry.meta.path);
            let to = under(&folder, name);
            if let Some(parent) = to.parent() {
                fs::create_dir_all(parent)
                    .map_err(|error| Error::from(error).with("path", fsx::display(parent)))?;
            }
            if from != to && from.exists() {
                fs::rename(&from, &to)
                    .map_err(|error| Error::from(error).with("path", fsx::display(&to)))?;
            }
        }
    }
    let mut sources: Vec<PathBuf> = fs::read_dir(&folder)
        .map_err(|error| Error::from(error).with("path", fsx::display(&folder)))?
        .map(|entry| entry.map(|entry| entry.path()))
        .collect::<std::io::Result<_>>()
        .map_err(|error| Error::from(error).with("path", fsx::display(&folder)))?;
    sources.extend(request.added.iter().cloned());
    let created = create::create(
        &CreateRequest {
            sources,
            destination: destination.to_path_buf(),
            options: CreateOptions::new(format),
            overwrite: true,
            layout: Layout::Whole,
        },
        ctx,
    )?;
    let _ = cleanup;
    Ok(Updated {
        path: created.path,
        format,
        repacked: true,
        files: created.files,
        folders: created.folders,
        bytes: created.bytes,
        packed: created.packed,
    })
}

/// A folder removed when this value is dropped, however the change ends.
struct TempFolder(PathBuf);

impl Drop for TempFolder {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

fn under(root: &Path, name: &str) -> PathBuf {
    name.split(['/', '\\'])
        .filter(|part| !part.is_empty())
        .fold(root.to_path_buf(), |path, part| path.join(part))
}

#[cfg(test)]
mod tests;

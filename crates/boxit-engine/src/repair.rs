//! What is left of a damaged archive, written as an archive again.
//!
//! The damaged archive is read through, and every entry that reads goes
//! into a new archive as it is compressed: nothing is unpacked to the disk
//! and nothing is compressed again, but the files of a 7z block that lost
//! its end. The archive that was damaged stays as it is.
//!
//! A ZIP that lost the list at its end gets one made from the headers of
//! its entries. A 7z has no such headers: one that lost its list has
//! nothing to be read by.

use std::fs::{self, OpenOptions as FileOptions};
use std::io::{BufWriter, Write};
use std::path::{Path, PathBuf};

use boxit_codecs::OpenOptions;
use boxit_core::{ArchiveFormat, Changes, Error, OpContext, Result, Selection};
use serde::Serialize;

use crate::archive::{Archive, Purpose, TestOptions, FAILED_LISTED};
use crate::create::{self, OUTPUT_BUFFER};
use crate::fsx;
use crate::guarded::guarded;

/// Where a repair reads and where it writes.
pub struct RepairRequest {
    /// The damaged archive.
    pub source: PathBuf,
    /// The archive to write; another file than the damaged one.
    pub destination: PathBuf,
    /// Replace the file that stands at the destination.
    pub overwrite: bool,
}

/// An entry of the damaged archive that is not in the new one.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct Lost {
    pub path: String,
    pub reason: Error,
}

/// What a repair saved.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Repaired {
    /// The new archive.
    pub path: String,
    pub format: ArchiveFormat,
    /// What the new archive holds.
    pub files: u64,
    pub folders: u64,
    /// Bytes of the content of its files.
    pub bytes: u64,
    /// Size of the new archive.
    pub packed: u64,
    /// The list of the damaged archive did not read and was made anew
    /// from the headers of its entries.
    pub rebuilt: bool,
    /// Files that are locked with a password nobody gave: they are in
    /// the new archive as they were, damaged or not.
    pub unchecked: u64,
    /// The entries that did not read, the first [`FAILED_LISTED`] of them.
    pub lost: Vec<Lost>,
    pub lost_total: u64,
}

/// The archive a repair offers beside the damaged one: `photos.repaired.zip`
/// beside `photos.zip`. A name the formats do not know keeps a `.zip`.
pub fn beside(source: &Path) -> PathBuf {
    let name = source
        .file_name()
        .map(|name| name.to_string_lossy().into_owned())
        .filter(|name| !name.is_empty())
        .unwrap_or_else(|| String::from("archive"));
    let lower = name.to_lowercase();
    let cut = ArchiveFormat::ALL
        .into_iter()
        .flat_map(ArchiveFormat::extensions)
        .filter(|extension| {
            lower
                .strip_suffix(*extension)
                .is_some_and(|rest| rest.ends_with('.'))
        })
        .map(|extension| extension.len() + 1)
        .max()
        .unwrap_or(0);
    let (stem, extension) = if cut == 0 {
        (name.as_str(), "zip")
    } else {
        (&name[..name.len() - cut], &name[name.len() - cut + 1..])
    };
    let stem = stem.trim_end_matches(['.', ' ']);
    let stem = if stem.is_empty() { "archive" } else { stem };
    let file = format!("{stem}.repaired.{extension}");
    match source.parent() {
        Some(parent) if !parent.as_os_str().is_empty() => parent.join(file),
        Some(_) | None => PathBuf::from(file),
    }
}

/// Writes what reads of a damaged archive into a new one. Either the whole
/// new archive appears at the destination, or nothing changes there.
pub fn repair(request: &RepairRequest, options: OpenOptions, ctx: &OpContext) -> Result<Repaired> {
    let source = fsx::long(&request.source)?;
    let destination = fsx::long(&request.destination)?;
    if fsx::same_path(&source, &destination) {
        return Err(Error::new("repairOverItself"));
    }
    let salvaged = guarded(|| boxit_codecs::salvage(&source, options, ctx)).map_err(|error| {
        match error.code.as_str() {
            "notFound" | "accessDenied" => error.with("path", fsx::display(&source)),
            _ => error,
        }
    })?;
    let rebuilt = salvaged.rebuilt;
    let mut lost: Vec<Lost> = Vec::new();
    let mut lost_total = salvaged.lost.len() as u64;
    for path in salvaged.lost.into_iter().take(FAILED_LISTED) {
        let reason = Error::new("cutShort").with("name", &path);
        lost.push(Lost { path, reason });
    }
    let mut archive = Archive::opened(salvaged.opened);
    let format = archive.format();
    // The entries of a 7z are known by its list only.
    let entries = archive.entries(ctx).map_err(|error| {
        if matches!(error.code.as_str(), "damaged" | "truncated") {
            Error::new("listLost").with("format", format.id())
        } else {
            error
        }
    })?;
    // The content is read to be checked, and what is whole is copied as
    // it is compressed.
    let content: u64 = entries.iter().filter_map(|entry| entry.meta.size).sum();
    let packed: u64 = entries.iter().filter_map(|entry| entry.packed_size).sum();
    ctx.progress.total(
        Some(content.saturating_add(packed)),
        Some(entries.len() as u64),
    );
    let survey = archive.survey(
        &Selection::All,
        TestOptions::default(),
        Purpose::Repair,
        ctx,
    )?;
    lost_total += survey.tested.failed_total;
    let room = FAILED_LISTED.saturating_sub(lost.len());
    lost.extend(
        survey
            .tested
            .failed
            .into_iter()
            .take(room)
            .map(|failed| Lost {
                path: failed.path,
                reason: failed.reason,
            }),
    );
    let mut files = 0u64;
    let mut folders = 0u64;
    let mut bytes = 0u64;
    let mut unchecked_files = 0u64;
    for entry in entries
        .iter()
        .filter(|entry| !survey.failed.contains(&entry.index))
    {
        if entry.meta.is_file() {
            files += 1;
            bytes += entry.meta.size.unwrap_or(0);
            if survey.unchecked.contains(&entry.index) {
                unchecked_files += 1;
            }
        } else if entry.meta.is_dir() {
            folders += 1;
        }
    }
    // Folders of a lost file are not a recovery.
    if files == 0 {
        return Err(Error::new("nothingRecovered"));
    }
    let mut repaired = Repaired {
        path: fsx::display(&destination),
        format,
        files,
        folders,
        bytes,
        packed: 0,
        rebuilt,
        unchecked: unchecked_files,
        lost,
        lost_total,
    };

    let mut partial = create::stage(&destination, &destination, request.overwrite)?;
    let failed = |error| Error::from(error).with("path", fsx::display(&destination));
    let mut file = FileOptions::new()
        .write(true)
        .create_new(true)
        .open(&partial.path)
        .map_err(failed)?;
    let mut output = BufWriter::with_capacity(OUTPUT_BUFFER, &mut file);
    let mut changes = Changes::none(format);
    changes.removed = survey.failed;
    archive.rewrite(changes, &mut output, ctx)?;
    ctx.checkpoint()?;
    output.flush().map_err(failed)?;
    drop(output);
    // The rename must not come before the data: see `create`.
    file.sync_all().map_err(failed)?;
    repaired.packed = file.metadata().map_err(failed)?.len();
    drop(file);
    fs::rename(&partial.path, &destination).map_err(failed)?;
    partial.placed = true;
    Ok(repaired)
}

#[cfg(test)]
mod tests;

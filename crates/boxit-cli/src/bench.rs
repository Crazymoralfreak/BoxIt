//! `bench`: how fast this machine creates and extracts archives.
//!
//! The measurement is the real thing: the files of the benchmark corpus
//! are written to a disk, packed by the engine into an archive on that disk
//! and extracted from it, the way the window and the other commands do it.
//! The speed of the formats alone, with no disk in the way, is what the
//! criterion benchmarks of `boxit-codecs` measure.
//!
//! The corpus is generated, the same bytes on every machine, so numbers of
//! two machines, or of BoxIt and another archiver given the same files
//! (`--corpus` writes them out), can be set side by side.

use std::env;
use std::fs;
use std::path::{Path, PathBuf, MAIN_SEPARATOR_STR};
use std::time::Instant;

use boxit_codecs::corpus::{total_size, Scale, Set};
use boxit_codecs::OpenOptions;
use boxit_core::{
    ArchiveFormat, CreateOptions, Error, ExtractOptions, Level, Limits, OpContext, Result,
    Selection,
};
use boxit_engine::archive::Archive;
use boxit_engine::create::{self, CreateRequest, Layout};
use boxit_engine::text::text;
use serde::Serialize;

use crate::args::BenchArgs;
use crate::commands::{absolute, context};
use crate::screen::Screen;
use crate::{show, Host, EXIT_OK};

/// One measurement: a format on a set of files.
#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct Row {
    set: &'static str,
    format: ArchiveFormat,
    files: u64,
    /// Bytes of content.
    bytes: u64,
    /// Size of the archive.
    packed: u64,
    /// The best time of creating the archive.
    create_seconds: f64,
    /// The best time of extracting it.
    extract_seconds: f64,
}

/// A set of the corpus written out for another archiver.
#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct Written {
    set: &'static str,
    path: String,
    files: u64,
    bytes: u64,
}

fn at(error: std::io::Error, path: &Path) -> Error {
    Error::from(error).with("path", path.display())
}

/// The sets asked for, each once, in the order they were named.
fn sets(arguments: &BenchArgs) -> Vec<Set> {
    let mut sets = Vec::new();
    for set in &arguments.sets {
        if !sets.contains(set) {
            sets.push(*set);
        }
    }
    sets
}

fn write_corpus(arguments: &BenchArgs, folder: &Path, scale: Scale, host: &Host) -> Result<i32> {
    let folder = absolute(folder)?;
    let mut written = Vec::new();
    for set in sets(arguments) {
        let bytes = set
            .write_tree(scale, &folder)
            .map_err(|error| at(error, &folder))?;
        written.push(Written {
            set: set.name(),
            path: folder.join(set.name()).display().to_string(),
            files: set.files(scale).len() as u64,
            bytes,
        });
    }
    if arguments.json {
        show::print_json(&written);
    } else {
        for set in &written {
            show::say(text(
                &host.language,
                "cli.corpusWritten",
                &[
                    ("path", &set.path),
                    ("files", &set.files.to_string()),
                    ("size", &show::size(&host.language, set.bytes)),
                ],
            ));
        }
    }
    Ok(EXIT_OK)
}

/// Creates an archive of `sources` and extracts it, `runs` times each, and
/// keeps the best times: the other runs were slowed down by something else
/// the machine was doing.
fn measure(
    format: ArchiveFormat,
    level: Level,
    sources: Vec<PathBuf>,
    work: &Path,
    runs: u32,
    ctx: &OpContext,
) -> Result<(u64, f64, f64)> {
    let archive = work.join(format!("bench.{}", format.id()));
    let request = CreateRequest {
        sources,
        destination: archive.clone(),
        options: CreateOptions {
            level,
            ..CreateOptions::new(format)
        },
        overwrite: true,
        layout: Layout::Whole,
    };
    let mut packed = 0;
    let mut create_seconds = f64::INFINITY;
    for _ in 0..runs {
        let started = Instant::now();
        packed = create::create(&request, ctx)?.packed;
        create_seconds = create_seconds.min(started.elapsed().as_secs_f64());
    }

    let unpacked = work.join("unpacked");
    // The corpus has files of zeros in it: what protects from an archive
    // bomb has nothing to protect from here.
    let options = ExtractOptions {
        limits: Limits::none(),
        ..ExtractOptions::default()
    };
    let mut extract_seconds = f64::INFINITY;
    for _ in 0..runs {
        let started = Instant::now();
        let mut opened = Archive::open(&archive, OpenOptions::default())?;
        opened.extract(&unpacked, Selection::All, options.clone(), ctx)?;
        extract_seconds = extract_seconds.min(started.elapsed().as_secs_f64());
        drop(opened);
        fs::remove_dir_all(&unpacked).map_err(|error| at(error, &unpacked))?;
    }
    fs::remove_file(&archive).map_err(|error| at(error, &archive))?;
    Ok((packed, create_seconds, extract_seconds))
}

fn header(language: &str) -> String {
    let word = |key: &str| text(language, key, &[]);
    format!(
        "  {:<9}  {:>12}  {:>8}  {:>14}  {:>14}",
        word("cli.bench.format"),
        word("cli.bench.packed"),
        word("cli.bench.ratio"),
        word("cli.bench.create"),
        word("cli.bench.extract"),
    )
}

fn line(language: &str, row: &Row) -> String {
    format!(
        "  {:<9}  {:>12}  {:>7}%  {:>14}  {:>14}",
        row.format.id(),
        show::size(language, row.packed),
        show::percent(row.packed, row.bytes),
        show::speed(language, row.bytes, row.create_seconds),
        show::speed(language, row.bytes, row.extract_seconds),
    )
}

pub(crate) fn run(arguments: &BenchArgs, host: &Host) -> Result<i32> {
    let language = host.language.as_str();
    let scale = if arguments.full {
        Scale::Full
    } else {
        Scale::Small
    };
    if let Some(folder) = &arguments.corpus {
        return write_corpus(arguments, folder, scale, host);
    }

    let base = match &arguments.folder {
        Some(folder) => absolute(folder)?,
        None => env::temp_dir(),
    };
    fs::create_dir_all(&base).map_err(|error| at(error, &base))?;
    // Removed with everything in it when the command ends, however it ends.
    let work = tempfile::Builder::new()
        .prefix("boxit-bench-")
        .tempdir_in(&base)
        .map_err(|error| at(error, &base))?;
    let screen = Screen::new(host, false);
    let ctx = context(&screen, arguments.threads);

    let mut rows = Vec::new();
    for set in sets(arguments) {
        let files = set.files(scale);
        let bytes = total_size(&files);
        let data = work.path().join("data");
        set.write_tree(scale, &data)
            .map_err(|error| at(error, &data))?;
        if !arguments.json {
            show::say(text(
                language,
                "cli.bench.data",
                &[
                    ("set", set.name()),
                    ("files", &files.len().to_string()),
                    ("size", &show::size(language, bytes)),
                    ("threads", &ctx.budget.threads.to_string()),
                ],
            ));
            show::say(header(language));
        }
        for format in &arguments.formats {
            // A format that compresses one file is measured on the set
            // that is one file.
            let sources = match files.as_slice() {
                [only] if format.is_single_file() => {
                    vec![data.join(only.path.replace('/', MAIN_SEPARATOR_STR))]
                }
                _ if format.is_single_file() => continue,
                _ => vec![data.join(set.name())],
            };
            let (packed, create_seconds, extract_seconds) = measure(
                *format,
                arguments.level.level(),
                sources,
                work.path(),
                arguments.runs,
                &ctx,
            )?;
            let row = Row {
                set: set.name(),
                format: *format,
                files: files.len() as u64,
                bytes,
                packed,
                create_seconds,
                extract_seconds,
            };
            if !arguments.json {
                show::say(line(language, &row));
            }
            rows.push(row);
        }
        fs::remove_dir_all(&data).map_err(|error| at(error, &data))?;
    }
    if arguments.json {
        show::print_json(&rows);
    }
    Ok(EXIT_OK)
}

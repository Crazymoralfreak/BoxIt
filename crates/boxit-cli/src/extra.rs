//! convert, diff, find, estimate and batch.

use std::path::Path;

use boxit_codecs::OpenOptions;
use boxit_core::{CreateOptions, ExtractOptions, Password, Result};
use boxit_engine::archive::Archive;
use boxit_engine::extras::{self, ConvertRequest};
use boxit_engine::text::text;

use crate::args::{BatchArgs, ConvertArgs, DiffArgs, EstimateArgs, FindArgs};
use crate::commands::{absolute, context, screen};
use crate::{show, Host, EXIT_OK};

fn read_options(password: Option<&str>) -> OpenOptions {
    OpenOptions {
        password: password
            .filter(|password| !password.is_empty())
            .map(Password::from),
        ..OpenOptions::default()
    }
}

pub(crate) fn convert(arguments: &ConvertArgs, host: &Host) -> Result<i32> {
    let language = host.language.as_str();
    let screen = screen(host, &arguments.common);
    let ctx = context(&screen, arguments.common.threads);
    let destination = absolute(&arguments.destination)?;
    let name = destination
        .file_name()
        .map_or_else(String::new, |name| name.to_string_lossy().into_owned());
    let format = arguments
        .format
        .or_else(|| boxit_core::ArchiveFormat::from_name(&name))
        .ok_or_else(|| boxit_core::Error::new("formatNotTold").with("name", &name))?;
    let created = {
        let _watch = screen.watch();
        extras::convert(
            &ConvertRequest {
                source: absolute(&arguments.source)?,
                destination,
                options: CreateOptions {
                    level: arguments.level.level(),
                    ..CreateOptions::new(format)
                },
                overwrite: arguments.overwrite,
            },
            read_options(arguments.password.as_deref()),
            &ctx,
        )?
    };
    if arguments.common.json {
        show::print_json(&created);
    } else {
        show::say(text(
            language,
            "cli.converted",
            &[
                ("path", &created.path),
                ("files", &created.files.to_string()),
                ("size", &show::size(language, created.bytes)),
                ("packed", &show::size(language, created.packed)),
            ],
        ));
    }
    Ok(EXIT_OK)
}

pub(crate) fn diff(arguments: &DiffArgs, host: &Host) -> Result<i32> {
    let language = host.language.as_str();
    let screen = screen(host, &arguments.common);
    let ctx = context(&screen, 0);
    let mut left = Archive::open(&absolute(&arguments.left)?, OpenOptions::default())?;
    let mut right = Archive::open(&absolute(&arguments.right)?, OpenOptions::default())?;
    let compared = extras::compare(&left.entries(&ctx)?, &right.entries(&ctx)?);
    if arguments.common.json {
        show::print_json(&compared);
    } else {
        show::say(text(
            language,
            "cli.compared",
            &[
                ("left", &compared.only_left.len().to_string()),
                ("right", &compared.only_right.len().to_string()),
                ("different", &compared.different.len().to_string()),
            ],
        ));
        for path in compared.only_left.iter().chain(&compared.only_right) {
            show::say(show::printable(path));
        }
        for different in &compared.different {
            show::say(show::printable(&different.path));
        }
    }
    Ok(EXIT_OK)
}

pub(crate) fn find(arguments: &FindArgs, host: &Host) -> Result<i32> {
    let language = host.language.as_str();
    let screen = screen(host, &arguments.common);
    let ctx = context(&screen, 0);
    let mut archive = Archive::open(&absolute(&arguments.archive)?, OpenOptions::default())?;
    let entries = archive.entries(&ctx)?;
    let found = extras::find(&entries, &arguments.pattern)?;
    if arguments.common.json {
        let paths: Vec<&str> = found.iter().map(|entry| entry.meta.path.as_str()).collect();
        show::print_json(&paths);
    } else {
        for entry in &found {
            show::say(show::printable(&entry.meta.path));
        }
        show::say(text(
            language,
            "cli.found",
            &[("count", &found.len().to_string())],
        ));
    }
    Ok(EXIT_OK)
}

pub(crate) fn estimate(arguments: &EstimateArgs, host: &Host) -> Result<i32> {
    let language = host.language.as_str();
    let screen = screen(host, &arguments.common);
    let ctx = context(&screen, arguments.common.threads);
    let sources = arguments
        .sources
        .iter()
        .map(|path| absolute(path))
        .collect::<Result<Vec<_>>>()?;
    let options = CreateOptions {
        level: arguments.level.level(),
        ..CreateOptions::new(arguments.format)
    };
    let estimate = extras::estimate(&sources, &options, &ctx)?;
    if arguments.common.json {
        show::print_json(&estimate);
    } else {
        show::say(text(
            language,
            "cli.estimated",
            &[
                ("packed", &show::size(language, estimate.packed)),
                ("low", &show::size(language, estimate.low)),
                ("high", &show::size(language, estimate.high)),
            ],
        ));
    }
    Ok(EXIT_OK)
}

pub(crate) fn batch(arguments: &BatchArgs, host: &Host) -> Result<i32> {
    let screen = screen(host, &arguments.common);
    let ctx = context(&screen, arguments.common.threads);
    let output = arguments
        .output
        .as_ref()
        .map(|path| absolute(path))
        .transpose()?;
    if arguments.action == "extract" && output.is_none() {
        return Err(boxit_core::Error::new("badDestination").with("path", ""));
    }
    for archive in &arguments.archives {
        let path = absolute(archive)?;
        let mut opened = Archive::open(&path, OpenOptions::default())?;
        let _watch = screen.watch();
        if arguments.action == "test" {
            opened.test(
                &boxit_core::Selection::All,
                boxit_engine::archive::TestOptions::default(),
                &ctx,
            )?;
        } else {
            let folder = output
                .as_deref()
                .unwrap_or(Path::new("."))
                .join(path.file_stem().unwrap_or_default());
            opened.extract(
                &folder,
                boxit_core::Selection::All,
                ExtractOptions::default(),
                &ctx,
            )?;
        }
    }
    Ok(EXIT_OK)
}

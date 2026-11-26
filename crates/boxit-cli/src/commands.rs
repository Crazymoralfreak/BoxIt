//! The commands that work with an archive: list, extract, create, test, repair.

use std::io::{self, IsTerminal};
use std::path::{self, Path, PathBuf};
use std::sync::Arc;

use boxit_core::{
    ArchiveFormat, Control, CreateOptions, Error, ExtractOptions, Limits, OpContext, Password,
    Result, Selection,
};
use boxit_engine::archive::{Archive, TestOptions};
use boxit_engine::budget;
use boxit_engine::create::{self, CreateRequest, Layout};
use boxit_engine::extract::Outcome;
use boxit_engine::passwords;
use boxit_engine::repair::{self, RepairRequest};
use boxit_engine::text::{error_text, text};
use serde::Serialize;

use crate::args::{
    CommonArgs, CreateArgs, ExtractArgs, ListArgs, MethodArg, PasswordArgs, RepairArgs, TestArgs,
};
use crate::screen::Screen;
use crate::{interrupt, show, Host, EXIT_FAILED, EXIT_OK, EXIT_PARTIAL};

/// How many of the entries an extraction left out are named; the rest are
/// counted. `--json` has them all.
const LEFT_OUT_SHOWN: usize = 20;

/// What the standard output of `extract --json` holds.
#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct Extracted<'a> {
    /// The folder the content went into.
    destination: String,
    #[serde(flatten)]
    outcome: &'a Outcome,
}

/// A screen for a command: the progress is drawn for a person who has not
/// asked for silence, and only on a terminal.
pub(crate) fn screen(host: &Host, common: &CommonArgs) -> Arc<Screen> {
    let live = !common.quiet && !common.json && io::stderr().is_terminal();
    Screen::new(host, live)
}

/// The context of an operation of the command line: Ctrl+C stops it, the
/// screen shows it and asks its questions.
pub(crate) fn context(screen: &Arc<Screen>, threads: usize) -> OpContext {
    let control = Control::new();
    interrupt::watch(&control);
    OpContext {
        control,
        progress: screen.clone(),
        interaction: screen.clone(),
        budget: budget::system(threads),
    }
}

/// A path of the command line as a full path: the engine works with those.
pub(crate) fn absolute(path: &Path) -> Result<PathBuf> {
    path::absolute(path).map_err(|error| Error::from(error).with("path", path.display()))
}

/// The entries the paths of the command line name. The paths are given
/// with either slash.
fn selection(paths: &[String]) -> Selection {
    if paths.is_empty() {
        Selection::All
    } else {
        Selection::paths(paths.iter().map(|path| path.replace('\\', "/")))
    }
}

pub(crate) fn list(arguments: &ListArgs, host: &Host) -> Result<i32> {
    let screen = Screen::new(host, false);
    let ctx = context(&screen, 0);
    let mut archive = Archive::open(
        &absolute(&arguments.read.archive)?,
        arguments.read.options(),
    )?;
    let listing = archive.list(&ctx)?;
    if arguments.json {
        show::print_json(&listing);
    } else {
        show::listing(&host.language, &listing);
    }
    Ok(EXIT_OK)
}

pub(crate) fn extract(arguments: &ExtractArgs, host: &Host) -> Result<i32> {
    let language = host.language.as_str();
    let screen = screen(host, &arguments.common);
    let ctx = context(&screen, arguments.common.threads);
    let source = absolute(&arguments.read.archive)?;
    let destination = absolute(arguments.output.as_deref().unwrap_or(Path::new(".")))?;
    tracing::info!(archive = %source.display(), destination = %destination.display(), "extract");

    let mut archive = Archive::open(&source, arguments.read.options())?;
    let options = ExtractOptions {
        conflicts: arguments.conflicts.policy(),
        symlinks: arguments.symlinks.policy(),
        limits: if arguments.no_limits {
            Limits::none()
        } else {
            Limits::default()
        },
    };
    let outcome = {
        let _watch = screen.watch();
        archive.extract(&destination, selection(&arguments.paths), options, &ctx)?
    };
    if arguments.common.json {
        show::print_json(&Extracted {
            destination: destination.display().to_string(),
            outcome: &outcome,
        });
    } else {
        show::say(text(
            language,
            "cli.extracted",
            &[
                ("path", &destination.display().to_string()),
                ("files", &outcome.files.to_string()),
                ("folders", &outcome.folders.to_string()),
                ("links", &outcome.links.to_string()),
                ("size", &show::size(language, outcome.bytes)),
            ],
        ));
        if outcome.skipped_total > 0 {
            show::warn(text(
                language,
                "cli.leftOut",
                &[("count", &outcome.skipped_total.to_string())],
            ));
            for skipped in outcome.skipped.iter().take(LEFT_OUT_SHOWN) {
                let reason = error_text(language, &skipped.reason);
                show::warn(format!("  {}", show::printable(&reason)));
            }
            let more = outcome.skipped_total - outcome.skipped.len().min(LEFT_OUT_SHOWN) as u64;
            if more > 0 {
                show::warn(text(
                    language,
                    "cli.leftOutMore",
                    &[("count", &more.to_string())],
                ));
            }
        }
    }
    Ok(if outcome.skipped_total > 0 {
        EXIT_PARTIAL
    } else {
        EXIT_OK
    })
}

pub(crate) fn test(arguments: &TestArgs, host: &Host) -> Result<i32> {
    let language = host.language.as_str();
    let screen = screen(host, &arguments.common);
    let ctx = context(&screen, arguments.common.threads);
    let source = absolute(&arguments.read.archive)?;
    tracing::info!(archive = %source.display(), "test");

    let mut archive = Archive::open(&source, arguments.read.options())?;
    let tested = {
        let _watch = screen.watch();
        let options = TestOptions {
            sums: arguments.sums,
        };
        archive.test(&selection(&arguments.paths), options, &ctx)?
    };
    if arguments.common.json {
        show::print_json(&tested);
    } else {
        // The sums are printed the way `sha256sum` prints them, and can be
        // checked by it once the files are extracted.
        for sum in tested.sums.iter().flatten() {
            show::say(format!("{}  {}", sum.sha256, show::printable(&sum.path)));
        }
        if let Some(sha256) = &tested.archive_sha256 {
            show::say(text(language, "cli.archiveSum", &[("sha256", sha256)]));
        }
        let counts = [
            ("files", tested.files.to_string()),
            ("size", show::size(language, tested.bytes)),
            ("count", tested.failed_total.to_string()),
        ];
        let counts: Vec<(&str, &str)> = counts
            .iter()
            .map(|(key, value)| (*key, value.as_str()))
            .collect();
        if tested.failed_total == 0 {
            show::say(text(language, "cli.tested", &counts));
        } else {
            for failed in &tested.failed {
                let reason = error_text(language, &failed.reason);
                show::warn(format!("  {}", show::printable(&reason)));
            }
            let more = tested.failed_total - tested.failed.len() as u64;
            if more > 0 {
                show::warn(text(
                    language,
                    "cli.leftOutMore",
                    &[("count", &more.to_string())],
                ));
            }
            show::warn(text(language, "cli.testedDamaged", &counts));
            if !tested.complete {
                show::warn(text(language, "cli.testedPartly", &[]));
            }
        }
    }
    Ok(if tested.failed_total > 0 {
        EXIT_FAILED
    } else {
        EXIT_OK
    })
}

/// Prints a new password and nothing else: it is not written to the log.
pub(crate) fn password(arguments: &PasswordArgs) -> Result<i32> {
    let drawn = passwords::generate(arguments.length, arguments.symbols)?;
    show::say(drawn.expose());
    Ok(EXIT_OK)
}

pub(crate) fn repair(arguments: &RepairArgs, host: &Host) -> Result<i32> {
    let language = host.language.as_str();
    let screen = screen(host, &arguments.common);
    let ctx = context(&screen, arguments.common.threads);
    let source = absolute(&arguments.read.archive)?;
    let destination = match &arguments.output {
        Some(output) => absolute(output)?,
        None => repair::beside(&source),
    };
    tracing::info!(archive = %source.display(), destination = %destination.display(), "repair");

    let request = RepairRequest {
        source,
        destination,
        overwrite: arguments.overwrite,
    };
    let repaired = {
        let _watch = screen.watch();
        repair::repair(&request, arguments.read.options(), &ctx)?
    };
    if arguments.common.json {
        show::print_json(&repaired);
    } else {
        show::say(text(
            language,
            "cli.repaired",
            &[
                ("path", &repaired.path),
                ("files", &repaired.files.to_string()),
                ("folders", &repaired.folders.to_string()),
                ("size", &show::size(language, repaired.bytes)),
            ],
        ));
        if repaired.rebuilt {
            show::say(text(language, "cli.repairedRebuilt", &[]));
        }
        if repaired.unchecked > 0 {
            let count = repaired.unchecked.to_string();
            show::say(text(
                language,
                "cli.repairedUnchecked",
                &[("count", &count)],
            ));
        }
        if repaired.lost_total > 0 {
            for lost in &repaired.lost {
                let reason = error_text(language, &lost.reason);
                show::warn(format!("  {}", show::printable(&reason)));
            }
            let more = repaired.lost_total - repaired.lost.len() as u64;
            if more > 0 {
                show::warn(text(
                    language,
                    "cli.leftOutMore",
                    &[("count", &more.to_string())],
                ));
            }
            let count = repaired.lost_total.to_string();
            show::warn(text(language, "cli.repairedLost", &[("count", &count)]));
        }
    }
    Ok(if repaired.lost_total > 0 {
        EXIT_PARTIAL
    } else {
        EXIT_OK
    })
}

/// The password of a new archive: given, or typed twice in the terminal.
fn new_password(
    arguments: &CreateArgs,
    screen: &Screen,
    language: &str,
) -> Result<Option<Password>> {
    if !arguments.ask_password {
        return Ok(arguments
            .password
            .as_deref()
            .filter(|password| !password.is_empty())
            .map(Password::from));
    }
    let ask = |key: &str| {
        screen
            .ask_password(&text(language, key, &[]))
            .ok_or_else(|| Error::new("passwordRequired"))
    };
    let password = ask("cli.passwordNew")?;
    if ask("cli.passwordAgain")? != password {
        return Err(Error::new("passwordMismatch"));
    }
    Ok(Some(password))
}

pub(crate) fn create(arguments: &CreateArgs, host: &Host) -> Result<i32> {
    let language = host.language.as_str();
    let screen = screen(host, &arguments.common);
    let destination = absolute(&arguments.archive)?;
    let name = destination
        .file_name()
        .map_or_else(String::new, |name| name.to_string_lossy().into_owned());
    let format = arguments
        .format
        .or_else(|| ArchiveFormat::from_name(&name).filter(|format| format.can_write()))
        .ok_or_else(|| Error::new("formatNotTold").with("name", &name))?;
    let options = CreateOptions {
        level: arguments.level.level(),
        method: arguments.method.map(MethodArg::method),
        solid: !arguments.no_solid,
        dictionary: arguments.dictionary,
        comment: arguments.comment.clone(),
        password: new_password(arguments, &screen, language)?,
        encrypt_names: arguments.encrypt_names,
        ..CreateOptions::new(format)
    };
    let request = CreateRequest {
        sources: arguments
            .sources
            .iter()
            .map(|source| absolute(source))
            .collect::<Result<_>>()?,
        destination,
        options,
        overwrite: arguments.overwrite,
        layout: arguments.volume.map_or(Layout::Whole, Layout::Volumes),
    };
    tracing::info!(archive = %request.destination.display(), format = format.id(), "create");

    let ctx = context(&screen, arguments.common.threads);
    let created = {
        let _watch = screen.watch();
        create::create(&request, &ctx)?
    };
    if arguments.common.json {
        show::print_json(&created);
    } else {
        show::say(text(
            language,
            "cli.created",
            &[
                ("path", &created.path),
                ("files", &created.files.to_string()),
                ("folders", &created.folders.to_string()),
                ("links", &created.links.to_string()),
                ("size", &show::size(language, created.bytes)),
                ("packed", &show::size(language, created.packed)),
                ("ratio", &show::percent(created.packed, created.bytes)),
            ],
        ));
        if created.volumes > 1 {
            let count = created.volumes.to_string();
            show::say(text(language, "cli.volumes", &[("count", &count)]));
        }
    }
    Ok(EXIT_OK)
}

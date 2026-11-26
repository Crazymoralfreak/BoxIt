//! The command line of BoxIt:
//!
//! ```text
//! boxit-cli list photos.7z
//! boxit-cli extract photos.7z -o photos
//! boxit-cli create backup.tar.zst documents projects --level maximum
//! boxit-cli test backup.tar.zst
//! boxit-cli repair broken.zip -o fixed.zip
//! ```
//!
//! The same commands are built into `BoxIt.exe`. They use the engine the
//! window uses: an extraction is the same all-or-nothing extraction, with
//! the same checks of the names and the same limits.
//!
//! What a command prints for people is in the language of the program and
//! may change between versions; a script asks for `--json` and reads the
//! exit code.

#![forbid(unsafe_code)]

mod args;
mod bench;
mod commands;
mod interrupt;
mod screen;
mod show;

use std::ffi::OsString;

use boxit_core::Error;
use boxit_engine::text;
use clap::{CommandFactory, Parser, Subcommand};
use serde_json::json;

pub use args::{
    BenchArgs, CommonArgs, ConflictArg, CreateArgs, ExtractArgs, LevelArg, ListArgs, MethodArg,
    PasswordArgs, ReadArgs, RepairArgs, SymlinkArg, TestArgs,
};

/// Everything went well.
pub const EXIT_OK: i32 = 0;
/// The command failed; nothing was left half done.
pub const EXIT_FAILED: i32 = 1;
/// The command line could not be understood.
pub const EXIT_USAGE: i32 = 2;
/// The command finished, but some entries were left out.
pub const EXIT_PARTIAL: i32 = 3;
/// The command was stopped with Ctrl+C.
pub const EXIT_CANCELLED: i32 = 130;

const AFTER_HELP: &str = "\
Exit codes:
    0  done
    1  failed: nothing is left half done
    2  the command line is not understood
    3  done, but some entries were left out
  130  stopped with Ctrl+C";

#[derive(Debug, Parser)]
#[command(
    name = "BoxIt",
    version,
    about = "BoxIt: create, list, test, repair and extract archives",
    after_help = AFTER_HELP
)]
pub struct Cli {
    #[command(subcommand)]
    pub command: Command,
}

#[derive(Debug, Subcommand)]
pub enum Command {
    /// Show what is inside an archive
    List(ListArgs),
    /// Extract an archive into a folder: all of it, or nothing
    Extract(ExtractArgs),
    /// Create an archive from files and folders
    Create(CreateArgs),
    /// Read every file of an archive and check it, writing nothing
    Test(TestArgs),
    /// Write what still reads of a damaged archive into a new one
    Repair(RepairArgs),
    /// Measure how fast this machine creates and extracts archives
    Bench(BenchArgs),
    /// Draw a password from the system generator
    Password(PasswordArgs),
}

/// Where the command line runs.
#[derive(Clone, Debug)]
pub struct Host {
    /// Language of the messages, a code the dictionaries know.
    pub language: String,
    /// There is a person at the terminal to ask for a password or a choice.
    pub prompts: bool,
}

/// Whether the first argument names a command. Anything else given to
/// `BoxIt.exe` is a file to open in the window.
pub fn is_command(argument: &str) -> bool {
    matches!(argument, "help" | "-h" | "--help" | "-V" | "--version")
        || Cli::command()
            .get_subcommands()
            .any(|command| command.get_name() == argument)
}

/// Tells a failure: to a person in their language, to a script as JSON.
fn report(error: &Error, json: bool, host: &Host) {
    let message = text::error_text(&host.language, error);
    if json {
        show::print_json(&json!({
            "error": {
                "code": error.code,
                "params": error.params,
                "message": message,
            }
        }));
    } else {
        show::warn(text::text(
            &host.language,
            "cli.error",
            &[("message", &message)],
        ));
    }
}

/// Runs a parsed command and returns the exit code of the process.
pub fn execute(cli: Cli, host: &Host) -> i32 {
    let (name, json, outcome) = match &cli.command {
        Command::List(arguments) => ("list", arguments.json, commands::list(arguments, host)),
        Command::Extract(arguments) => (
            "extract",
            arguments.common.json,
            commands::extract(arguments, host),
        ),
        Command::Create(arguments) => (
            "create",
            arguments.common.json,
            commands::create(arguments, host),
        ),
        Command::Test(arguments) => (
            "test",
            arguments.common.json,
            commands::test(arguments, host),
        ),
        Command::Repair(arguments) => (
            "repair",
            arguments.common.json,
            commands::repair(arguments, host),
        ),
        Command::Bench(arguments) => ("bench", arguments.json, bench::run(arguments, host)),
        Command::Password(arguments) => ("password", false, commands::password(arguments)),
    };
    match outcome {
        Ok(code) => code,
        Err(error) => {
            tracing::warn!(command = name, "the command failed: {error}");
            report(&error, json, host);
            if error.is_cancelled() {
                EXIT_CANCELLED
            } else {
                EXIT_FAILED
            }
        }
    }
}

/// Parses the command line (the program name first) and runs it.
pub fn run<I, T>(arguments: I, host: &Host) -> i32
where
    I: IntoIterator<Item = T>,
    T: Into<OsString> + Clone,
{
    match Cli::try_parse_from(arguments) {
        Ok(cli) => execute(cli, host),
        Err(error) => {
            // Help and version requests arrive here too and are not failures.
            let code = if error.use_stderr() {
                EXIT_USAGE
            } else {
                EXIT_OK
            };
            let _ = error.print();
            code
        }
    }
}

#[cfg(test)]
mod tests;

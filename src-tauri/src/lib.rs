//! BoxIt: a desktop archiver.
//!
//! One program has two faces. Started without arguments, or with files to
//! open, it shows its window. Started with a command (`list`, `extract`,
//! `create`, `test`, `bench`) it works in the terminal and exits: the same
//! engine without the interface.
//!
//! This crate is the window: it owns the web view and hands the commands of
//! the interface to the engine. Everything that works with archives lives in
//! the crates under `crates/`, which know nothing about the window.

pub mod app;
pub mod commands;
pub mod console;
pub mod dialogs;
#[cfg(windows)]
mod modern;
pub mod protocol;
pub mod secrets;
pub mod shell;
pub mod shell_run;
pub mod update;

use std::env;
use std::ffi::OsString;
use std::io::{self, IsTerminal};
use std::sync::Arc;
use std::time::Instant;

use boxit_cli::Host;
use boxit_engine::{logging, stored_language, Engine, Paths};

/// Exit code of a start that failed before anything could run.
const EXIT_FAILED: i32 = 1;

/// Runs a command of the command line and returns its exit code. The data
/// folder is only read: a window may be open next to the command.
fn run_console(arguments: Vec<OsString>) -> i32 {
    let attached = console::attach();
    let paths = Paths::detect();
    let _log = logging::init(&paths.logs(), false);
    let host = Host {
        language: stored_language(&paths),
        // A terminal that was attached to does not wait for this program,
        // and what is typed there goes to the terminal as well.
        prompts: !attached && io::stdin().is_terminal() && io::stderr().is_terminal(),
    };
    let code = boxit_cli::run(arguments, &host);
    if attached {
        // The terminal has already printed its prompt: leave it a clean line.
        println!();
    }
    code
}

fn run_window(started: Instant) -> i32 {
    let engine = match Engine::open(Paths::detect()) {
        Ok(engine) => Arc::new(engine),
        Err(error) => {
            eprintln!("error: {error}");
            return EXIT_FAILED;
        }
    };
    let _log = logging::init(&engine.paths.logs(), false);
    // What BoxIt was started with: an archive to open, files to pack.
    let opened = app::paths_among(env::args_os(), env::current_dir().ok().as_deref());
    let built = tauri::Builder::default()
        // Has to be the first plugin: a second copy hands over to the running one and exits.
        .plugin(tauri_plugin_single_instance::init(
            |handle, arguments, folder| {
                app::second_instance(handle, &arguments, &folder);
            },
        ))
        .plugin(tauri_plugin_notification::init())
        .plugin(tauri_plugin_dialog::init())
        .plugin(tauri_plugin_drag::init())
        // Asks nothing by itself: the address in `tauri.conf.json` is read
        // only when the user presses "Check for updates".
        .plugin(tauri_plugin_updater::Builder::new().build())
        .register_asynchronous_uri_scheme_protocol(
            protocol::SCHEME,
            |context, request, responder| {
                protocol::handle(context.app_handle(), &request, responder);
            },
        )
        .setup(move |setup| {
            app::setup(setup.handle(), engine, started, opened);
            Ok(())
        })
        .invoke_handler(tauri::generate_handler![
            commands::app_info,
            commands::window_show,
            commands::opened_files,
            commands::settings_get,
            commands::settings_set,
            commands::shell_register,
            commands::shell_unregister,
            commands::formats_list,
            commands::name_encodings,
            commands::archive_open,
            commands::archive_cancel_open,
            commands::archive_close,
            commands::archive_folder,
            commands::archive_rows,
            commands::archive_summary,
            commands::archive_set_password,
            commands::recognize,
            commands::extract_destination,
            commands::extract_start,
            commands::test_start,
            commands::repair_start,
            commands::repair_suggest,
            commands::update_start,
            commands::files_list,
            commands::file_stamp,
            commands::open_path,
            commands::batch_test,
            commands::batch_extract,
            commands::create_start,
            commands::create_memory,
            commands::create_suggest,
            commands::password_generate,
            commands::password_names,
            commands::password_save,
            commands::password_forget,
            commands::password_named,
            commands::password_remember,
            commands::password_recall,
            commands::password_forget_archive,
            commands::path_state,
            commands::drag_out,
            commands::jobs_list,
            commands::job_cancel,
            commands::job_pause,
            commands::job_answer,
            commands::jobs_clear,
            commands::update_status,
            commands::update_check,
            commands::update_install,
            commands::update_page,
            dialogs::pick_archive,
            dialogs::pick_sources,
            dialogs::pick_folder,
            dialogs::pick_save,
            dialogs::confirm,
            dialogs::reveal_path,
            dialogs::reveal_logs,
        ])
        .build(tauri::generate_context!());
    match built {
        Ok(built) => {
            built.run(|_, _| {});
            0
        }
        Err(error) => {
            tracing::error!("BoxIt could not start: {error}");
            EXIT_FAILED
        }
    }
}

/// Starts BoxIt in the mode its arguments ask for and returns the exit code
/// of the process. The arguments are looked at before anything of the
/// window is started: a command never opens one.
pub fn run() -> i32 {
    // The cold start is counted from here to the window on the screen.
    let started = Instant::now();
    // An argument that is not Unicode is a file name, never a command.
    let first = env::args_os().nth(1);
    if first.as_deref().and_then(|first| first.to_str()) == Some("--shell") {
        return shell_run::run();
    }
    let command = first
        .as_deref()
        .and_then(|first| first.to_str())
        .is_some_and(boxit_cli::is_command);
    if command {
        run_console(env::args_os().collect())
    } else {
        run_window(started)
    }
}

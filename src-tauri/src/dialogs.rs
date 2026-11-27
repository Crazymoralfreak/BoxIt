//! The dialogs of the system: choosing files and folders, a question with
//! two answers, a folder shown in the file manager. They are the ones of
//! the operating system, so they look and behave the way its user expects.

#[cfg(windows)]
use std::os::windows::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::Command;

use tauri_plugin_dialog::{
    DialogExt, FileDialogBuilder, FilePath, MessageDialogButtons, MessageDialogKind,
};

use boxit_core::{ArchiveFormat, Error, Result};
use boxit_engine::formats;
use boxit_engine::fsx::{self, Existing};
use boxit_engine::text::text;

use crate::app::Core;
use crate::commands::CoreState;

fn label(core: &Core, key: &str) -> String {
    text(&core.lang(), key, &[])
}

fn shown(path: FilePath) -> Option<String> {
    path.into_path().ok().map(|path| fsx::display(&path))
}

/// A dialog that starts in `folder` when there is such a folder.
fn starting_in<R: tauri::Runtime>(
    dialog: FileDialogBuilder<R>,
    folder: Option<&Path>,
) -> FileDialogBuilder<R> {
    match folder {
        Some(folder) if folder.is_dir() => dialog.set_directory(folder),
        _ => dialog,
    }
}

/// Asks for an archive to open. Nothing when the dialog was closed.
#[tauri::command(async)]
pub fn pick_archive(core: CoreState<'_>) -> Option<String> {
    let extensions: Vec<&str> = formats::formats()
        .iter()
        .flat_map(|format| format.extensions.iter().copied())
        .collect();
    core.app()
        .dialog()
        .file()
        .set_title(label(&core, "native.dialog.openArchive"))
        .add_filter(label(&core, "native.dialog.archives"), &extensions)
        .add_filter(label(&core, "native.dialog.allFiles"), &["*"])
        .blocking_pick_file()
        .and_then(shown)
}

/// Asks for the files, or the folders, to put into a new archive.
#[tauri::command(async)]
pub fn pick_sources(core: CoreState<'_>, folders: bool) -> Vec<String> {
    let dialog = core.app().dialog().file();
    let chosen = if folders {
        dialog
            .set_title(label(&core, "native.dialog.addFolders"))
            .blocking_pick_folders()
    } else {
        dialog
            .set_title(label(&core, "native.dialog.addFiles"))
            .blocking_pick_files()
    };
    chosen
        .unwrap_or_default()
        .into_iter()
        .filter_map(shown)
        .collect()
}

/// Asks for a folder, starting in `start` when there is such a folder.
#[tauri::command(async)]
pub fn pick_folder(core: CoreState<'_>, start: Option<String>) -> Option<String> {
    let dialog = core
        .app()
        .dialog()
        .file()
        .set_title(label(&core, "native.dialog.chooseFolder"));
    starting_in(dialog, start.as_deref().map(Path::new))
        .blocking_pick_folder()
        .and_then(shown)
}

/// Asks where to save a new archive of `format`, starting with the name
/// and the folder of `path`.
#[tauri::command(async)]
pub fn pick_save(core: CoreState<'_>, path: String, format: ArchiveFormat) -> Option<String> {
    let proposed = PathBuf::from(&path);
    let mut dialog = core
        .app()
        .dialog()
        .file()
        .set_title(label(&core, "native.dialog.saveArchive"))
        .add_filter(format.id(), format.extensions());
    if let Some(name) = proposed.file_name() {
        dialog = dialog.set_file_name(name.to_string_lossy());
    }
    starting_in(dialog, proposed.parent())
        .blocking_save_file()
        .and_then(shown)
}

/// Asks a question with two answers in a dialog of the system. `true` for
/// the answer `accept`.
#[tauri::command(async)]
pub fn confirm(core: CoreState<'_>, message: String, accept: String) -> bool {
    let cancel = label(&core, "native.dialog.cancel");
    core.app()
        .dialog()
        .message(message)
        .title("BoxIt")
        .kind(MessageDialogKind::Warning)
        .buttons(MessageDialogButtons::OkCancelCustom(accept, cancel))
        .blocking_show()
}

/// The program of the system that shows a folder, and its arguments for
/// `path`: a file is shown selected where the file manager can do that.
fn revealing(path: &Path, folder: bool) -> Command {
    #[cfg(windows)]
    {
        let mut explorer = Command::new("explorer.exe");
        if folder {
            explorer.arg(path);
        } else {
            // Explorer reads this argument by its own rules, so it is
            // passed as written.
            explorer.raw_arg(format!("/select,\"{}\"", path.display()));
        }
        explorer
    }
    #[cfg(target_os = "macos")]
    {
        let mut open = Command::new("open");
        if !folder {
            open.arg("-R");
        }
        open.arg(path);
        open
    }
    #[cfg(not(any(windows, target_os = "macos")))]
    {
        let mut open = Command::new("xdg-open");
        open.arg(if folder {
            path
        } else {
            path.parent().unwrap_or(path)
        });
        open
    }
}

/// Shows a file selected in the file manager, or opens a folder.
#[tauri::command(async)]
pub fn reveal_path(path: String) -> Result<()> {
    let target = PathBuf::from(&path);
    let folder = match fsx::existing(&target) {
        Existing::Nothing => return Err(Error::new("notFound").with("path", path)),
        Existing::Folder => true,
        Existing::File | Existing::Link => false,
    };
    revealing(&target, folder)
        .spawn()
        .map(|_| ())
        .map_err(|error| Error::from(error).with("path", path))
}

/// Opens the folder with the log files.
#[tauri::command(async)]
pub fn reveal_logs(core: CoreState<'_>) -> Result<()> {
    reveal_path(fsx::display(&core.engine.paths.logs()))
}

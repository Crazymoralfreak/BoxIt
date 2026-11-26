//! What the menu of Explorer asks for, done without the window.
//!
//! `BoxIt.exe --shell extract-here archive.zip` and the other actions run
//! here and exit. A password, which this path cannot ask for, opens the
//! window on the archive instead.

use boxit_codecs::OpenOptions;
use boxit_core::{ArchiveFormat, CreateOptions, ExtractOptions, OpContext, Result, Selection};
use boxit_engine::archive::Archive;
use boxit_engine::create::{self, CreateRequest, Layout};
use boxit_engine::settings::ExtractPlace;
use boxit_engine::tasks;
use boxit_engine::{logging, Engine, Paths};
use std::env;
use std::ffi::OsString;
use std::path::{Path, PathBuf};

use crate::shell;

/// Runs one action of the menu and returns the exit code.
pub fn run() -> i32 {
    let paths = Paths::detect();
    let _log = logging::init(&paths.logs(), false);
    let mut args = env::args_os().skip(2);
    let Some(action) = args.next() else {
        return 2;
    };
    let files: Vec<PathBuf> = args.map(PathBuf::from).collect();
    let action = action.to_string_lossy();
    let outcome = match action.as_ref() {
        "register" => shell::register(&shell::program()),
        "unregister" => shell::unregister(),
        "extract-here" => extract(&files, ExtractPlace::Beside),
        "extract-to" => extract(&files, ExtractPlace::Named),
        "add" => add(&files),
        "email" => email(&files),
        _ => return 2,
    };
    match outcome {
        Ok(()) => 0,
        Err(error) => {
            tracing::warn!(action = action.as_ref(), "the menu action failed: {error}");
            if error.code == "passwordRequired" || error.code == "wrongPassword" {
                if let Some(file) = files.first() {
                    let _ = std::process::Command::new(shell::program())
                        .arg(file)
                        .spawn();
                }
            }
            1
        }
    }
}

fn context() -> OpContext {
    OpContext::silent()
}

fn extract(files: &[PathBuf], place: ExtractPlace) -> Result<()> {
    let archive = files
        .first()
        .ok_or_else(|| boxit_core::Error::new("nothingMatched"))?;
    let destination = tasks::destination_for(archive, place, "");
    let mut opened = Archive::open(archive, OpenOptions::default())?;
    opened.extract(
        &destination,
        Selection::All,
        ExtractOptions::default(),
        &context(),
    )?;
    Ok(())
}

fn add(files: &[PathBuf]) -> Result<()> {
    let destination = tasks::archive_for(files, ArchiveFormat::Zip)
        .ok_or_else(|| boxit_core::Error::new("nothingToAdd"))?;
    create::create(
        &CreateRequest {
            sources: files.to_vec(),
            destination,
            options: CreateOptions::new(ArchiveFormat::Zip),
            overwrite: false,
            layout: Layout::Whole,
        },
        &context(),
    )?;
    Ok(())
}

fn email(files: &[PathBuf]) -> Result<()> {
    let engine = Engine::open(Paths::detect())?;
    let destination = engine
        .paths
        .temp()
        .join(format!("mail-{}.zip", boxit_engine::fsx::unique_suffix()));
    create::create(
        &CreateRequest {
            sources: files.to_vec(),
            destination: destination.clone(),
            options: CreateOptions::new(ArchiveFormat::Zip),
            overwrite: true,
            layout: Layout::Whole,
        },
        &context(),
    )?;
    #[cfg(windows)]
    {
        send_mail(&destination)
    }
    #[cfg(not(windows))]
    {
        let _ = destination;
        Err(boxit_core::Error::new("shellFailed"))
    }
}

#[cfg(windows)]
fn send_mail(archive: &Path) -> Result<()> {
    use std::os::windows::ffi::OsStrExt;
    // Simple MAPI. The wide entry point takes UTF-16 paths.
    #[repr(C)]
    struct FileDesc {
        reserved: u32,
        flags: u32,
        position: u32,
        path: *const u16,
        name: *const u16,
        kind: *mut std::ffi::c_void,
    }
    #[repr(C)]
    struct Message {
        reserved: u32,
        subject: *const u16,
        note: *const u16,
        kind: *const u16,
        date: *const u16,
        conversation: *const u16,
        flags: u32,
        originator: *mut std::ffi::c_void,
        recip_count: u32,
        recips: *mut std::ffi::c_void,
        file_count: u32,
        files: *mut FileDesc,
    }
    type SendMail = unsafe extern "system" fn(usize, isize, *const Message, u32, u32) -> u32;
    let module = unsafe {
        windows::Win32::System::LibraryLoader::LoadLibraryW(windows::core::w!("mapi32.dll"))
    }
    .map_err(|error| boxit_core::Error::new("shellFailed").detail(error.to_string()))?;
    let symbol = unsafe {
        windows::Win32::System::LibraryLoader::GetProcAddress(
            module,
            windows::core::s!("MAPISendMailW"),
        )
    };
    let Some(symbol) = symbol else {
        return Err(boxit_core::Error::new("shellFailed"));
    };
    let send: SendMail = unsafe { std::mem::transmute(symbol) };
    let path: Vec<u16> = archive
        .as_os_str()
        .encode_wide()
        .chain(std::iter::once(0))
        .collect();
    let name: Vec<u16> = archive
        .file_name()
        .unwrap_or_default()
        .encode_wide()
        .chain(std::iter::once(0))
        .collect();
    let subject: Vec<u16> = "BoxIt\0".encode_utf16().collect();
    let mut file = FileDesc {
        reserved: 0,
        flags: 0,
        position: u32::MAX,
        path: path.as_ptr(),
        name: name.as_ptr(),
        kind: std::ptr::null_mut(),
    };
    let message = Message {
        reserved: 0,
        subject: subject.as_ptr(),
        note: std::ptr::null(),
        kind: std::ptr::null(),
        date: std::ptr::null(),
        conversation: std::ptr::null(),
        flags: 0,
        originator: std::ptr::null_mut(),
        recip_count: 0,
        recips: std::ptr::null_mut(),
        file_count: 1,
        files: std::ptr::addr_of_mut!(file),
    };
    // MAPI_DIALOG asks the user who to send it to.
    let code = unsafe { send(0, 0, std::ptr::from_ref(&message), 8, 0) };
    // 0 is success, 1 is the user cancelling the dialog.
    if code == 0 || code == 1 {
        Ok(())
    } else {
        Err(boxit_core::Error::new("shellFailed").detail(format!("MAPI {code}")))
    }
}

/// The arguments after the program name, so a test can build the list.
#[allow(dead_code)]
pub fn rest(arguments: impl IntoIterator<Item = OsString>) -> Vec<OsString> {
    arguments.into_iter().skip(1).collect()
}

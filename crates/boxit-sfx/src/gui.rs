//! The dialogs of the self-extractor: a folder, a password, a progress bar.
//!
//! `IProgressDialog` is called from the threads that unpack. Microsoft
//! documents those calls as legal from any thread, and the crate's pointer
//! is wrapped so the type system agrees.

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Mutex, PoisonError};

use boxit_core::{Control, Progress};
use windows::core::{IUnknown, HSTRING};
use windows::Win32::Foundation::ERROR_SUCCESS;
use windows::Win32::Security::Credentials::{
    CredUIPromptForCredentialsW, CREDUI_FLAGS_ALWAYS_SHOW_UI, CREDUI_FLAGS_DO_NOT_PERSIST,
    CREDUI_FLAGS_EXCLUDE_CERTIFICATES, CREDUI_FLAGS_GENERIC_CREDENTIALS,
};
use windows::Win32::System::Com::{
    CoCreateInstance, CoInitializeEx, CoTaskMemFree, CLSCTX_INPROC_SERVER, COINIT_APARTMENTTHREADED,
};
use windows::Win32::UI::Shell::{
    FileOpenDialog, IFileOpenDialog, IProgressDialog, FOS_FORCEFILESYSTEM, FOS_PICKFOLDERS,
    PROGDLG_AUTOTIME, PROGDLG_NORMAL, SIGDN_FILESYSPATH,
};
use windows::Win32::UI::WindowsAndMessaging::{MessageBoxW, MB_ICONINFORMATION, MB_OK};

/// The progress dialog, callable from the unpacking threads.
struct Shown(IProgressDialog);
// SAFETY: `SetProgress64`, `SetLine` and `HasUserCancelled` are documented
// to be called from any thread. The pointer is created once and then only
// used through this wrapper.
unsafe impl Send for Shown {}
unsafe impl Sync for Shown {}

fn apartment() {
    // A second call reports that the apartment is already there.
    let _ = unsafe { CoInitializeEx(None, COINIT_APARTMENTTHREADED) };
}

/// Asks for a folder. `None` when the dialog is cancelled or cannot be shown.
pub fn folder(title: &str) -> Option<std::path::PathBuf> {
    apartment();
    unsafe {
        let dialog: IFileOpenDialog =
            CoCreateInstance(&FileOpenDialog, None::<&IUnknown>, CLSCTX_INPROC_SERVER).ok()?;
        dialog
            .SetOptions(FOS_PICKFOLDERS | FOS_FORCEFILESYSTEM)
            .ok()?;
        dialog.SetTitle(&HSTRING::from(title)).ok()?;
        dialog.Show(None).ok()?;
        let item = dialog.GetResult().ok()?;
        let name = item.GetDisplayName(SIGDN_FILESYSPATH).ok()?;
        let text = name.to_string().unwrap_or_default();
        CoTaskMemFree(Some(name.0.cast()));
        Some(std::path::PathBuf::from(text))
    }
}

/// Asks for a password. `None` when the dialog is cancelled.
pub fn password(name: &str) -> Option<String> {
    let mut user = [0u16; 2];
    let mut secret = [0u16; 256];
    let result = unsafe {
        CredUIPromptForCredentialsW(
            None,
            &HSTRING::from(name),
            None,
            0,
            &mut user,
            &mut secret,
            None,
            CREDUI_FLAGS_GENERIC_CREDENTIALS
                | CREDUI_FLAGS_ALWAYS_SHOW_UI
                | CREDUI_FLAGS_DO_NOT_PERSIST
                | CREDUI_FLAGS_EXCLUDE_CERTIFICATES,
        )
    };
    let text = String::from_utf16_lossy(secret.split(|unit| *unit == 0).next().unwrap_or_default());
    secret.fill(0);
    if result == ERROR_SUCCESS && !text.is_empty() {
        Some(text)
    } else {
        None
    }
}

pub fn tell(message: &str) {
    let _ = unsafe {
        MessageBoxW(
            None,
            &HSTRING::from(message),
            &HSTRING::from("BoxIt"),
            MB_OK | MB_ICONINFORMATION,
        )
    };
}

/// The progress dialog. Dropping it closes the dialog.
pub struct Bar {
    control: Control,
    total: AtomicU64,
    done: AtomicU64,
    dialog: Mutex<Option<Shown>>,
}

impl Bar {
    pub fn start(language: &str, control: Control) -> Self {
        apartment();
        let dialog = unsafe {
            CoCreateInstance::<_, IProgressDialog>(
                &windows::Win32::UI::Shell::CLSID_ProgressDialog,
                None::<&IUnknown>,
                CLSCTX_INPROC_SERVER,
            )
            .ok()
        };
        if let Some(dialog) = &dialog {
            let title = HSTRING::from(boxit_engine::text::text(language, "sfx.working", &[]));
            unsafe {
                let _ = dialog.SetTitle(&title);
                let _ = dialog.StartProgressDialog(
                    None,
                    None::<&IUnknown>,
                    PROGDLG_NORMAL | PROGDLG_AUTOTIME,
                    None,
                );
            }
        }
        Self {
            control,
            total: AtomicU64::new(0),
            done: AtomicU64::new(0),
            dialog: Mutex::new(dialog.map(Shown)),
        }
    }
}

impl Drop for Bar {
    fn drop(&mut self) {
        if let Some(dialog) = self.dialog.get_mut().ok().and_then(Option::take) {
            let _ = unsafe { dialog.0.StopProgressDialog() };
        }
    }
}

impl Progress for Bar {
    fn total(&self, bytes: Option<u64>, _entries: Option<u64>) {
        self.total.store(bytes.unwrap_or(0), Ordering::Relaxed);
    }

    fn entry(&self, path: &str) {
        if let Some(dialog) = self
            .dialog
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .as_ref()
        {
            let _ = unsafe { dialog.0.SetLine(1, &HSTRING::from(path), false, None) };
        }
    }

    fn advance(&self, bytes: u64) {
        let done = self.done.fetch_add(bytes, Ordering::Relaxed) + bytes;
        let total = self.total.load(Ordering::Relaxed).max(done);
        if let Some(dialog) = self
            .dialog
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .as_ref()
        {
            let _ = unsafe { dialog.0.SetProgress64(done, total) };
            if unsafe { dialog.0.HasUserCancelled() }.as_bool() {
                self.control.cancel();
            }
        }
    }
}

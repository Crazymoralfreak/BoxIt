//! COM server Explorer loads for the Windows 11 menu.
//!
//! The class id is `A7C3E4B2-1D58-4F0A-9B6E-3C8D5F1A2E70`, the same value the
//! package manifest registers.
#![allow(clippy::inline_always, clippy::ref_as_ptr)]

use std::ffi::OsString;
use std::os::windows::ffi::OsStringExt;
use std::path::PathBuf;
use std::process::Command;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Mutex;

use windows::core::{implement, Interface, BOOL, GUID, HRESULT, PCWSTR, PWSTR};
use windows::Win32::Foundation::{
    CLASS_E_CLASSNOTAVAILABLE, CLASS_E_NOAGGREGATION, E_FAIL, E_INVALIDARG, E_NOTIMPL, E_POINTER,
    HMODULE, S_FALSE, S_OK,
};
use windows::Win32::Globalization::GetUserDefaultUILanguage;
use windows::Win32::System::Com::{
    CoTaskMemAlloc, CoTaskMemFree, IBindCtx, IClassFactory, IClassFactory_Impl,
};
use windows::Win32::System::LibraryLoader::{
    GetModuleFileNameW, GetModuleHandleExW, GET_MODULE_HANDLE_EX_FLAG_FROM_ADDRESS,
    GET_MODULE_HANDLE_EX_FLAG_UNCHANGED_REFCOUNT,
};
use windows::Win32::UI::Shell::{
    IEnumExplorerCommand, IEnumExplorerCommand_Impl, IExplorerCommand, IExplorerCommand_Impl,
    IObjectWithSelection, IObjectWithSelection_Impl, IShellItemArray, ECF_HASSUBCOMMANDS,
    ECS_ENABLED, SIGDN_FILESYSPATH,
};

use crate::text::{actions_for, Action};

const CLSID: GUID = GUID::from_u128(0xA7C3E4B2_1D58_4F0A_9B6E_3C8D5F1A2E70);

fn failed<T>() -> windows::core::Result<T> {
    Err(E_FAIL.into())
}

fn caught<T>(body: impl FnOnce() -> windows::core::Result<T>) -> windows::core::Result<T> {
    match std::panic::catch_unwind(std::panic::AssertUnwindSafe(body)) {
        Ok(result) => result,
        Err(_) => failed(),
    }
}

fn task_string(text: &str) -> windows::core::Result<PWSTR> {
    let wide: Vec<u16> = text.encode_utf16().chain(std::iter::once(0)).collect();
    let memory = unsafe { CoTaskMemAlloc(wide.len() * 2) };
    if memory.is_null() {
        return failed();
    }
    unsafe { std::ptr::copy_nonoverlapping(wide.as_ptr(), memory.cast(), wide.len()) };
    Ok(PWSTR(memory.cast()))
}

fn from_wide(text: PWSTR) -> String {
    if text.is_null() {
        return String::new();
    }
    let mut length = 0usize;
    unsafe {
        while *text.0.add(length) != 0 {
            length += 1;
        }
        String::from_utf16_lossy(std::slice::from_raw_parts(text.0, length))
    }
}

fn paths(items: windows::core::Ref<'_, IShellItemArray>) -> Vec<PathBuf> {
    let Some(items) = items.as_ref() else {
        return Vec::new();
    };
    let count = unsafe { items.GetCount() }.unwrap_or(0);
    let mut files = Vec::with_capacity(count as usize);
    for index in 0..count {
        let Ok(item) = (unsafe { items.GetItemAt(index) }) else {
            continue;
        };
        let Ok(name) = (unsafe { item.GetDisplayName(SIGDN_FILESYSPATH) }) else {
            continue;
        };
        let path = from_wide(name);
        unsafe { CoTaskMemFree(Some(name.0.cast())) };
        if !path.is_empty() {
            files.push(PathBuf::from(path));
        }
    }
    files
}

fn remember(slot: &Mutex<Vec<PathBuf>>, items: windows::core::Ref<'_, IShellItemArray>) {
    let found = paths(items);
    if found.is_empty() {
        return;
    }
    if let Ok(mut guard) = slot.lock() {
        *guard = found;
    }
}

fn program() -> Option<PathBuf> {
    let mut module = HMODULE::default();
    unsafe {
        // The address is a location inside this library, which is what
        // `GET_MODULE_HANDLE_EX_FLAG_FROM_ADDRESS` asks for.
        GetModuleHandleExW(
            GET_MODULE_HANDLE_EX_FLAG_FROM_ADDRESS | GET_MODULE_HANDLE_EX_FLAG_UNCHANGED_REFCOUNT,
            PCWSTR((program as *const ()).cast::<u16>()),
            &raw mut module,
        )
        .ok()?;
        let mut buffer = vec![0u16; 32768];
        let length = GetModuleFileNameW(Some(module), &mut buffer);
        if length == 0 {
            return None;
        }
        let path = PathBuf::from(OsString::from_wide(&buffer[..length as usize]));
        Some(path.parent()?.join("BoxIt.exe"))
    }
}

fn ui_language() -> &'static str {
    match unsafe { GetUserDefaultUILanguage() } & 0x3ff {
        0x07 => "de",
        0x0a => "es",
        0x0c => "fr",
        0x11 => "ja",
        0x12 => "ko",
        0x15 => "pl",
        0x16 => "pt",
        0x19 => "ru",
        0x1f => "tr",
        0x04 => "zh",
        _ => "en",
    }
}

fn launch(action: Action, files: &[PathBuf]) -> windows::core::Result<()> {
    if files.is_empty() {
        return Err(E_INVALIDARG.into());
    }
    let Some(exe) = program() else {
        return failed();
    };
    let mut command = Command::new(exe);
    if let Some(flag) = action.flag() {
        command.arg("--shell").arg(flag);
    }
    command.args(files);
    command.spawn().map(|_| ()).map_err(|_| E_FAIL.into())
}

#[implement(IExplorerCommand, IObjectWithSelection)]
struct Root {
    files: Mutex<Vec<PathBuf>>,
}

impl IExplorerCommand_Impl for Root_Impl {
    fn GetTitle(
        &self,
        items: windows::core::Ref<'_, IShellItemArray>,
    ) -> windows::core::Result<PWSTR> {
        caught(|| {
            remember(&self.files, items);
            task_string("BoxIt")
        })
    }

    fn GetIcon(
        &self,
        items: windows::core::Ref<'_, IShellItemArray>,
    ) -> windows::core::Result<PWSTR> {
        Self::icon(items)
    }

    fn GetToolTip(
        &self,
        _: windows::core::Ref<'_, IShellItemArray>,
    ) -> windows::core::Result<PWSTR> {
        Err(E_NOTIMPL.into())
    }

    fn GetCanonicalName(&self) -> windows::core::Result<GUID> {
        Ok(CLSID)
    }

    fn GetState(
        &self,
        items: windows::core::Ref<'_, IShellItemArray>,
        _: BOOL,
    ) -> windows::core::Result<u32> {
        caught(|| {
            remember(&self.files, items);
            Ok(ECS_ENABLED.0 as u32)
        })
    }

    fn Invoke(
        &self,
        items: windows::core::Ref<'_, IShellItemArray>,
        _: windows::core::Ref<'_, IBindCtx>,
    ) -> windows::core::Result<()> {
        caught(|| launch(Action::Open, &paths(items)))
    }

    fn GetFlags(&self) -> windows::core::Result<u32> {
        Ok(ECF_HASSUBCOMMANDS.0 as u32)
    }

    fn EnumSubCommands(&self) -> windows::core::Result<IEnumExplorerCommand> {
        caught(|| {
            let files = self
                .files
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .clone();
            let list: IEnumExplorerCommand = Commands {
                actions: actions_for(&files).to_vec(),
                index: AtomicUsize::new(0),
            }
            .into();
            Ok(list)
        })
    }
}

impl IObjectWithSelection_Impl for Root_Impl {
    fn SetSelection(
        &self,
        items: windows::core::Ref<'_, IShellItemArray>,
    ) -> windows::core::Result<()> {
        caught(|| {
            remember(&self.files, items);
            Ok(())
        })
    }

    fn GetSelection(
        &self,
        _: *const GUID,
        _: *mut *mut core::ffi::c_void,
    ) -> windows::core::Result<()> {
        Err(E_NOTIMPL.into())
    }
}

#[implement(IExplorerCommand)]
struct Child {
    action: Action,
}

impl IExplorerCommand_Impl for Child_Impl {
    fn GetTitle(&self, _: windows::core::Ref<'_, IShellItemArray>) -> windows::core::Result<PWSTR> {
        caught(|| task_string(&self.action.title(ui_language())))
    }

    fn GetIcon(
        &self,
        items: windows::core::Ref<'_, IShellItemArray>,
    ) -> windows::core::Result<PWSTR> {
        Root_Impl::icon(items)
    }

    fn GetToolTip(
        &self,
        _: windows::core::Ref<'_, IShellItemArray>,
    ) -> windows::core::Result<PWSTR> {
        Err(E_NOTIMPL.into())
    }

    fn GetCanonicalName(&self) -> windows::core::Result<GUID> {
        Ok(self.action.guid())
    }

    fn GetState(
        &self,
        _: windows::core::Ref<'_, IShellItemArray>,
        _: BOOL,
    ) -> windows::core::Result<u32> {
        Ok(ECS_ENABLED.0 as u32)
    }

    fn Invoke(
        &self,
        items: windows::core::Ref<'_, IShellItemArray>,
        _: windows::core::Ref<'_, IBindCtx>,
    ) -> windows::core::Result<()> {
        caught(|| launch(self.action, &paths(items)))
    }

    fn GetFlags(&self) -> windows::core::Result<u32> {
        Ok(0)
    }

    fn EnumSubCommands(&self) -> windows::core::Result<IEnumExplorerCommand> {
        Err(E_NOTIMPL.into())
    }
}

impl Root_Impl {
    fn icon(_: windows::core::Ref<'_, IShellItemArray>) -> windows::core::Result<PWSTR> {
        caught(|| {
            let Some(exe) = program() else {
                return failed();
            };
            task_string(&format!("{},0", exe.to_string_lossy()))
        })
    }
}

impl Action {
    fn guid(self) -> GUID {
        let tail = match self {
            Self::Open => 0x71,
            Self::ExtractTo => 0x72,
            Self::ExtractHere => 0x73,
            Self::Add => 0x74,
            Self::Email => 0x75,
        };
        GUID::from_u128(0xA7C3E4B2_1D58_4F0A_9B6E_3C8D5F1A2E00 | tail)
    }
}

#[implement(IEnumExplorerCommand)]
struct Commands {
    actions: Vec<Action>,
    index: AtomicUsize,
}

impl IEnumExplorerCommand_Impl for Commands_Impl {
    fn Next(
        &self,
        count: u32,
        commands: *mut Option<IExplorerCommand>,
        fetched: *mut u32,
    ) -> HRESULT {
        if count > 0 && commands.is_null() {
            return E_POINTER;
        }
        let mut wrote = 0u32;
        while wrote < count {
            let index = self.index.fetch_add(1, Ordering::Relaxed);
            if index >= self.actions.len() {
                self.index.store(self.actions.len(), Ordering::Relaxed);
                break;
            }
            let command: IExplorerCommand = Child {
                action: self.actions[index],
            }
            .into();
            unsafe { commands.add(wrote as usize).write(Some(command)) };
            wrote += 1;
        }
        if !fetched.is_null() {
            unsafe { fetched.write(wrote) };
        }
        if wrote == count {
            S_OK
        } else {
            S_FALSE
        }
    }

    fn Skip(&self, count: u32) -> windows::core::Result<()> {
        self.index.fetch_add(count as usize, Ordering::Relaxed);
        Ok(())
    }

    fn Reset(&self) -> windows::core::Result<()> {
        self.index.store(0, Ordering::Relaxed);
        Ok(())
    }

    fn Clone(&self) -> windows::core::Result<IEnumExplorerCommand> {
        Ok(Commands {
            actions: self.actions.clone(),
            index: AtomicUsize::new(self.index.load(Ordering::Relaxed)),
        }
        .into())
    }
}

#[implement(IClassFactory)]
struct Factory;

impl IClassFactory_Impl for Factory_Impl {
    fn CreateInstance(
        &self,
        outer: windows::core::Ref<'_, windows::core::IUnknown>,
        riid: *const GUID,
        object: *mut *mut core::ffi::c_void,
    ) -> windows::core::Result<()> {
        if object.is_null() || riid.is_null() {
            return Err(E_POINTER.into());
        }
        unsafe { object.write(std::ptr::null_mut()) };
        if !outer.is_null() {
            return Err(CLASS_E_NOAGGREGATION.into());
        }
        let command: IExplorerCommand = Root {
            files: Mutex::new(Vec::new()),
        }
        .into();
        unsafe { command.query(riid, object).ok() }
    }

    fn LockServer(&self, _: BOOL) -> windows::core::Result<()> {
        Ok(())
    }
}

/// Explorer asks for the menu class.
///
/// # Safety
/// `class`, `interface` and `object` follow `DllGetClassObject`: the
/// identifiers are readable and `object` is writable.
#[no_mangle]
pub unsafe extern "system" fn DllGetClassObject(
    class: *const GUID,
    interface: *const GUID,
    object: *mut *mut core::ffi::c_void,
) -> HRESULT {
    if class.is_null() || interface.is_null() || object.is_null() {
        return E_POINTER;
    }
    unsafe { object.write(std::ptr::null_mut()) };
    if unsafe { *class } != CLSID {
        return CLASS_E_CLASSNOTAVAILABLE;
    }
    let factory: IClassFactory = Factory.into();
    unsafe { factory.query(interface, object) }
}

/// Explorer may unload the library when this returns `S_OK`. It stays loaded:
/// unloading under the shell is how a menu takes Explorer down.
#[no_mangle]
pub extern "system" fn DllCanUnloadNow() -> HRESULT {
    S_FALSE
}

//! The classic menu of Explorer and the file associations, in the registry
//! of the current user. Nothing is written for the whole machine, and the
//! user still chooses the default application in the settings of Windows.
//!
//! The cascade is `ExtendedSubCommandsKey`. A file dialog does not load the
//! per-user CommandStore, so `SubCommands` draws the name BoxIt with an empty
//! submenu. The commands live under `BoxIt.Menu`, next to the file types that
//! dialog already reads.
//!
//! The Windows 11 menu is a separate package (`boxit_shell.dll`). It is
//! registered for the current user next to the classic menu. The same
//! registration is what the installer runs and what the settings of a
//! portable copy run.

use std::path::{Path, PathBuf};

use boxit_core::{ArchiveFormat, Result};

/// Registers BoxIt for the current user: associations and the cascade menu.
pub fn register(program: &Path) -> Result<()> {
    #[cfg(windows)]
    {
        registry::write(program)?;
        crate::modern::install(program)
    }
    #[cfg(not(windows))]
    {
        let _ = program;
        Err(boxit_core::Error::new("shellFailed"))
    }
}

/// Removes what [`register`] wrote.
pub fn unregister() -> Result<()> {
    #[cfg(windows)]
    {
        let classic = registry::remove();
        let modern = crate::modern::remove();
        classic?;
        modern
    }
    #[cfg(not(windows))]
    {
        Err(boxit_core::Error::new("shellFailed"))
    }
}

/// The program that is running, for the command lines stored in the registry.
pub fn program() -> PathBuf {
    std::env::current_exe().unwrap_or_else(|_| PathBuf::from("BoxIt.exe"))
}

#[cfg(windows)]
mod registry {
    use std::path::Path;

    use boxit_core::{Error, Result};
    use boxit_engine::text::{self, system_language};
    use winreg::enums::{HKEY_CURRENT_USER, KEY_SET_VALUE};
    use winreg::RegKey;

    use super::extensions;

    fn quote(program: &Path) -> String {
        format!("\"{}\"", program.display())
    }

    fn classes() -> Result<RegKey> {
        RegKey::predef(HKEY_CURRENT_USER)
            .open_subkey("Software\\Classes")
            .map_err(|error| Error::new("shellFailed").detail(error.to_string()))
    }

    fn put(key: &RegKey, path: &str, name: &str, value: &str) -> Result<()> {
        let (created, _) = key
            .create_subkey(path)
            .map_err(|error| Error::new("shellFailed").detail(error.to_string()))?;
        created
            .set_value(name, &value)
            .map_err(|error| Error::new("shellFailed").detail(error.to_string()))
    }

    /// One line of a cascade. No action opens the window on the file.
    struct Entry {
        /// Key name. The menu shows these in the order of the names.
        id: &'static str,
        /// Dictionary key of the label.
        label: &'static str,
        /// Argument of `--shell`. None opens the window on the file.
        action: Option<&'static str>,
    }

    const ARCHIVE_MENU: &[Entry] = &[
        Entry {
            id: "1open",
            label: "shell.open",
            action: None,
        },
        Entry {
            id: "2extractTo",
            label: "shell.extractTo",
            action: Some("extract-to"),
        },
        Entry {
            id: "3extractHere",
            label: "shell.extractHere",
            action: Some("extract-here"),
        },
        Entry {
            id: "4add",
            label: "shell.add",
            action: Some("add"),
        },
        Entry {
            id: "5email",
            label: "shell.email",
            action: Some("email"),
        },
    ];

    const FOLDER_MENU: &[Entry] = &[
        Entry {
            id: "1add",
            label: "shell.add",
            action: Some("add"),
        },
        Entry {
            id: "2email",
            label: "shell.email",
            action: Some("email"),
        },
    ];

    fn line(exe: &str, action: Option<&str>) -> String {
        match action {
            Some(action) => format!("{exe} --shell {action} \"%1\""),
            None => format!("{exe} \"%1\""),
        }
    }

    fn cascade(classes: &RegKey, name: &str, icon: &str, exe: &str, items: &[Entry]) -> Result<()> {
        let language = system_language();
        for item in items {
            let path = format!("{name}\\shell\\{}", item.id);
            let label = text::text(language, item.label, &[]);
            put(classes, &path, "", &label)?;
            put(classes, &path, "MUIVerb", &label)?;
            put(classes, &path, "Icon", icon)?;
            put(
                classes,
                &format!("{path}\\command"),
                "",
                &line(exe, item.action),
            )?;
        }
        Ok(())
    }

    fn attach(classes: &RegKey, path: &str, commands: &str, icon: &str) -> Result<()> {
        put(classes, path, "", "BoxIt")?;
        put(classes, path, "MUIVerb", "BoxIt")?;
        put(classes, path, "Icon", icon)?;
        put(classes, path, "ExtendedSubCommandsKey", commands)?;
        if let Ok(key) = classes.open_subkey_with_flags(path, KEY_SET_VALUE) {
            let _ = key.delete_value("SubCommands");
        }
        Ok(())
    }

    fn forget_store() {
        let hkcu = RegKey::predef(HKEY_CURRENT_USER);
        let store = "Software\\Microsoft\\Windows\\CurrentVersion\\Explorer\\CommandStore\\shell";
        for id in [
            "BoxIt.extractHere",
            "BoxIt.extractTo",
            "BoxIt.add",
            "BoxIt.email",
        ] {
            let _ = hkcu.delete_subkey_all(format!("{store}\\{id}"));
        }
    }

    pub(super) fn write(program: &Path) -> Result<()> {
        let classes = classes()?;
        let exe = quote(program);
        let icon = format!("{},0", program.display());
        put(&classes, "BoxIt.Archive", "", "BoxIt archive")?;
        put(
            &classes,
            "BoxIt.Archive\\shell\\open\\command",
            "",
            &format!("{exe} \"%1\""),
        )?;
        cascade(&classes, "BoxIt.Menu", &icon, &exe, ARCHIVE_MENU)?;
        cascade(&classes, "BoxIt.Folder", &icon, &exe, FOLDER_MENU)?;
        for extension in extensions() {
            attach(
                &classes,
                &format!("SystemFileAssociations\\.{extension}\\shell\\BoxIt"),
                "BoxIt.Menu",
                &icon,
            )?;
            put(
                &classes,
                &format!(".{extension}\\OpenWithProgids"),
                "BoxIt.Archive",
                "",
            )?;
        }
        attach(&classes, "Directory\\shell\\BoxIt", "BoxIt.Folder", &icon)?;
        forget_store();
        Ok(())
    }

    pub(super) fn remove() -> Result<()> {
        let hkcu = RegKey::predef(HKEY_CURRENT_USER);
        let _ = hkcu.delete_subkey_all("Software\\Classes\\BoxIt.Archive");
        let _ = hkcu.delete_subkey_all("Software\\Classes\\BoxIt.Menu");
        let _ = hkcu.delete_subkey_all("Software\\Classes\\BoxIt.Folder");
        let _ = hkcu.delete_subkey_all("Software\\Classes\\Directory\\shell\\BoxIt");
        forget_store();
        let classes = classes()?;
        for extension in extensions() {
            let _ = classes.delete_subkey_all(format!(
                "SystemFileAssociations\\.{extension}\\shell\\BoxIt"
            ));
            if let Ok(progids) = classes
                .open_subkey_with_flags(format!(".{extension}\\OpenWithProgids"), KEY_SET_VALUE)
            {
                let _ = progids.delete_value("BoxIt.Archive");
            }
        }
        Ok(())
    }
}

fn extensions() -> Vec<&'static str> {
    ArchiveFormat::ALL
        .into_iter()
        .flat_map(|format| format.extensions().iter().copied())
        .collect()
}

//! Where BoxIt keeps its data.
//!
//! An installed copy stores its settings, cache and logs in the folder the
//! system gives applications for that: `%APPDATA%\BoxIt` on Windows,
//! `~/Library/Application Support/BoxIt` on macOS, `$XDG_DATA_HOME/BoxIt` (or
//! `~/.local/share/BoxIt`) elsewhere. A portable copy (an empty file named
//! `portable`, or a `data` folder, next to the program) keeps everything in
//! `data` beside the program and leaves no trace on the computer.
//!
//! Passwords are the one exception: they are kept by the operating system
//! (Credential Manager, Keychain, Secret Service), never in this folder.

use std::env;
use std::ffi::OsString;
use std::fs;
use std::path::{Path, PathBuf};

use boxit_core::Result;

const APP_FOLDER: &str = "BoxIt";
const PORTABLE_MARKER: &str = "portable";
const PORTABLE_DATA: &str = "data";

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Paths {
    pub root: PathBuf,
    pub portable: bool,
}

/// The folder for the data of applications, from the environment of the
/// process. `variable` reads a variable, so tests can pass their own.
fn system_folder(variable: impl Fn(&str) -> Option<OsString>) -> Option<PathBuf> {
    let set = |name: &str| {
        variable(name)
            .filter(|value| !value.is_empty())
            .map(PathBuf::from)
    };
    if cfg!(windows) {
        set("APPDATA")
    } else if cfg!(target_os = "macos") {
        set("HOME").map(|home| home.join("Library").join("Application Support"))
    } else {
        set("XDG_DATA_HOME").or_else(|| set("HOME").map(|home| home.join(".local").join("share")))
    }
}

impl Paths {
    /// Finds the data folder of this copy of the application.
    pub fn detect() -> Self {
        let beside = env::current_exe()
            .ok()
            .and_then(|program| program.parent().map(Path::to_path_buf));
        Self::locate(beside.as_deref(), |name| env::var_os(name))
    }

    /// The data folder for a program in `program_folder` and the given
    /// environment.
    pub fn locate(
        program_folder: Option<&Path>,
        variable: impl Fn(&str) -> Option<OsString>,
    ) -> Self {
        if let Some(folder) = program_folder {
            if folder.join(PORTABLE_MARKER).is_file() || folder.join(PORTABLE_DATA).is_dir() {
                return Self {
                    root: folder.join(PORTABLE_DATA),
                    portable: true,
                };
            }
        }
        let base = system_folder(variable).unwrap_or_else(env::temp_dir);
        Self {
            root: base.join(APP_FOLDER),
            portable: false,
        }
    }

    /// A data folder at a fixed place, for tests and tools.
    pub fn at(root: impl Into<PathBuf>) -> Self {
        Self {
            root: root.into(),
            portable: false,
        }
    }

    pub fn settings(&self) -> PathBuf {
        self.root.join("settings.json")
    }

    /// Everything that can be thrown away and built again.
    pub fn cache(&self) -> PathBuf {
        self.root.join("cache")
    }

    /// Files extracted for a look or an edit; emptied at every start.
    pub fn temp(&self) -> PathBuf {
        self.cache().join("temp")
    }

    pub fn logs(&self) -> PathBuf {
        self.root.join("logs")
    }

    /// Creates the folders the application writes into.
    pub fn ensure(&self) -> Result<()> {
        for folder in [self.root.clone(), self.cache(), self.temp(), self.logs()] {
            fs::create_dir_all(folder)?;
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn no_environment(_: &str) -> Option<OsString> {
        None
    }

    #[test]
    fn files_live_under_the_root() {
        let paths = Paths::at("/data");
        for path in [paths.settings(), paths.cache(), paths.temp(), paths.logs()] {
            assert!(path.starts_with("/data"), "{path:?}");
        }
    }

    #[test]
    fn ensure_creates_folders() {
        let folder = tempfile::tempdir().unwrap();
        let paths = Paths::at(folder.path().join("nested"));
        paths.ensure().unwrap();
        assert!(paths.temp().is_dir());
        assert!(paths.logs().is_dir());
    }

    #[test]
    fn a_marker_beside_the_program_makes_the_copy_portable() {
        let folder = tempfile::tempdir().unwrap();
        fs::write(folder.path().join(PORTABLE_MARKER), b"").unwrap();
        let paths = Paths::locate(Some(folder.path()), no_environment);
        assert!(paths.portable);
        assert_eq!(paths.root, folder.path().join(PORTABLE_DATA));
    }

    #[test]
    fn a_data_folder_beside_the_program_makes_the_copy_portable() {
        let folder = tempfile::tempdir().unwrap();
        fs::create_dir(folder.path().join(PORTABLE_DATA)).unwrap();
        let paths = Paths::locate(Some(folder.path()), no_environment);
        assert!(paths.portable);
    }

    #[test]
    fn a_folder_named_portable_is_not_a_marker() {
        let folder = tempfile::tempdir().unwrap();
        fs::create_dir(folder.path().join(PORTABLE_MARKER)).unwrap();
        let paths = Paths::locate(Some(folder.path()), no_environment);
        assert!(!paths.portable);
    }

    #[test]
    fn an_installed_copy_uses_the_folder_of_the_system() {
        let folder = tempfile::tempdir().unwrap();
        let home = tempfile::tempdir().unwrap();
        let variable = |name: &str| match name {
            "APPDATA" | "XDG_DATA_HOME" | "HOME" => Some(home.path().as_os_str().to_owned()),
            _ => None,
        };
        let paths = Paths::locate(Some(folder.path()), variable);
        assert!(!paths.portable);
        assert!(paths.root.starts_with(home.path()), "{:?}", paths.root);
        assert!(paths.root.ends_with(APP_FOLDER));
    }

    #[test]
    fn an_empty_variable_counts_as_unset() {
        let paths = Paths::locate(None, |_| Some(OsString::new()));
        assert!(paths.root.starts_with(env::temp_dir()));
    }
}

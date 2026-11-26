//! The engine of BoxIt: everything the window and the command line share
//! that touches the disk. Operations on archives, the settings and the data
//! folder live here; the formats themselves are in `boxit-codecs`.

#![forbid(unsafe_code)]

pub mod archive;
pub mod browse;
pub mod budget;
pub mod create;
pub mod extract;
pub mod formats;
pub mod fsx;
pub mod guarded;
pub mod jobs;
pub mod logging;
pub mod passwords;
pub mod paths;
pub mod preview;
pub mod repair;
pub mod sessions;
pub mod settings;
pub mod store;
pub mod tasks;
#[cfg(test)]
mod testing;
pub mod text;
pub mod version;

use std::fs;
use std::sync::{PoisonError, RwLock};

use boxit_core::Result;

use crate::jobs::Jobs;
use crate::sessions::Sessions;

pub use paths::Paths;
pub use settings::{Settings, Theme};

/// One copy of BoxIt at work: its data folder, its settings, the archives
/// it has open and the operations it runs.
pub struct Engine {
    pub paths: Paths,
    settings: RwLock<Settings>,
    pub sessions: Sessions,
    pub jobs: Jobs,
}

impl Engine {
    /// Opens the data folder: creates what is missing, reads the settings
    /// and removes the temporary files a previous run left behind.
    pub fn open(paths: Paths) -> Result<Self> {
        // What was extracted for a look during the last run is of no use now.
        let _ = fs::remove_dir_all(paths.temp());
        paths.ensure()?;
        let settings = store::load::<Settings>(&paths.settings()).normalized();
        Ok(Self {
            paths,
            settings: RwLock::new(settings),
            sessions: Sessions::new(),
            jobs: Jobs::new(),
        })
    }

    pub fn settings(&self) -> Settings {
        self.settings
            .read()
            .unwrap_or_else(PoisonError::into_inner)
            .clone()
    }

    /// Stores new settings. Returns what was stored.
    pub fn set_settings(&self, new: Settings) -> Result<Settings> {
        let new = new.normalized();
        let mut current = self
            .settings
            .write()
            .unwrap_or_else(PoisonError::into_inner);
        if *current != new {
            store::save(&self.paths.settings(), &new)?;
            *current = new.clone();
        }
        Ok(new)
    }

    /// Changes the settings in place and stores them.
    pub fn update_settings(&self, change: impl FnOnce(&mut Settings)) -> Result<Settings> {
        let mut new = self.settings();
        change(&mut new);
        self.set_settings(new)
    }

    /// Language of the texts: the one of the settings, or the one of the
    /// operating system.
    pub fn language(&self) -> String {
        language_of(&self.settings())
    }
}

fn language_of(settings: &Settings) -> String {
    if settings.language.is_empty() {
        text::system_language().to_owned()
    } else {
        settings.language.clone()
    }
}

/// Language of the texts for a program that does not own the data folder:
/// the command line, which may run next to an open window and must leave
/// the files of that window alone.
pub fn stored_language(paths: &Paths) -> String {
    language_of(&store::load::<Settings>(&paths.settings()).normalized())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn settings_survive_a_restart() {
        let folder = tempfile::tempdir().unwrap();
        let paths = Paths::at(folder.path().join("data"));
        let engine = Engine::open(paths.clone()).unwrap();
        assert_eq!(engine.settings(), Settings::default());
        let saved = engine
            .update_settings(|settings| {
                settings.theme = Theme::Dark;
                settings.language = String::from("ru");
            })
            .unwrap();
        assert_eq!(saved.theme, Theme::Dark);
        drop(engine);

        let engine = Engine::open(paths).unwrap();
        assert_eq!(engine.settings(), saved);
        assert_eq!(engine.language(), "ru");
    }

    #[test]
    fn the_language_is_read_without_opening_the_data_folder() {
        let folder = tempfile::tempdir().unwrap();
        let paths = Paths::at(folder.path().join("data"));
        // Nothing is stored yet, and nothing is created by the question.
        assert!(text::known_language(&stored_language(&paths)));
        assert!(!paths.root.exists());

        let engine = Engine::open(paths.clone()).unwrap();
        engine
            .update_settings(|settings| settings.language = String::from("ru"))
            .unwrap();
        let left = paths.temp().join("open in the window.txt");
        fs::write(&left, b"x").unwrap();
        assert_eq!(stored_language(&paths), "ru");
        assert!(left.exists());
    }

    #[test]
    fn temporary_files_of_the_last_run_are_removed() {
        let folder = tempfile::tempdir().unwrap();
        let paths = Paths::at(folder.path());
        paths.ensure().unwrap();
        let left = paths.temp().join("left.txt");
        fs::write(&left, b"x").unwrap();
        let engine = Engine::open(paths).unwrap();
        assert!(!left.exists());
        assert!(engine.paths.temp().is_dir());
    }

    #[test]
    fn an_empty_language_follows_the_system() {
        let folder = tempfile::tempdir().unwrap();
        let engine = Engine::open(Paths::at(folder.path())).unwrap();
        assert!(text::known_language(&engine.language()));
    }
}

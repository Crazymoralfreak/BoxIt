//! Application settings shared by the window and the command line.

use std::collections::BTreeMap;

use boxit_core::{ArchiveFormat, ConflictPolicy, Level, SymlinkPolicy};
use serde::{Deserialize, Serialize};

use crate::text::known_language;

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum Theme {
    /// Follow the theme of the operating system.
    #[default]
    System,
    Light,
    Dark,
}

/// Where the content of an archive goes.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum ExtractPlace {
    /// A new folder named after the archive, next to it.
    #[default]
    Named,
    /// The folder the archive is in.
    Beside,
    /// A folder the user chooses.
    Chosen,
}

/// What the dialog of an extraction starts with: the choices of the last
/// time.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct ExtractChoices {
    pub place: ExtractPlace,
    /// The folder chosen the last time.
    pub folder: String,
    pub conflicts: ConflictPolicy,
    pub symlinks: SymlinkPolicy,
}

/// The keys that work on the list. A value is `F3` or `Alt+F9`.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct Keys {
    pub view: String,
    pub edit: String,
    pub copy: String,
    pub extract: String,
}

impl Default for Keys {
    fn default() -> Self {
        Self {
            view: String::from("F3"),
            edit: String::from("F4"),
            copy: String::from("F5"),
            extract: String::from("Alt+F9"),
        }
    }
}

/// What the dialog of a new archive starts with.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct CreateChoices {
    pub format: ArchiveFormat,
    pub level: Level,
}

impl Default for CreateChoices {
    fn default() -> Self {
        Self {
            format: ArchiveFormat::Zip,
            level: Level::default(),
        }
    }
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct Settings {
    /// Interface language code; empty follows the operating system.
    pub language: String,
    pub theme: Theme,
    /// The pane with a look at the chosen file is open.
    pub preview: bool,
    pub extract: ExtractChoices,
    pub create: CreateChoices,
    /// The archives opened lately, the latest first.
    pub recent: Vec<String>,
    /// Names of passwords kept in the credential store of the system. The
    /// passwords themselves are not in the settings.
    pub saved_passwords: Vec<String>,
    pub keys: Keys,
    /// Icon name for a format id. An unknown name is dropped.
    pub icons: BTreeMap<String, String>,
}

/// How many archives opened lately are remembered.
pub const RECENT_KEPT: usize = 12;

/// Icon names the window knows how to draw.
const FORMAT_ICONS: &[&str] = &["box", "file", "folder", "extract", "test", "lock", "repair"];

impl Settings {
    /// Puts an archive at the head of the ones opened lately.
    pub fn opened(&mut self, archive: &str) {
        self.recent.retain(|known| known != archive);
        self.recent.insert(0, archive.to_owned());
        self.recent.truncate(RECENT_KEPT);
    }

    /// Settings as they are stored: values that mean nothing are replaced by
    /// the ones they fall back to, so a file edited by hand cannot put the
    /// application into a state the interface has no control for.
    #[must_use]
    pub fn normalized(mut self) -> Self {
        let language = self.language.trim().to_lowercase();
        self.language = if known_language(&language) {
            language
        } else {
            String::new()
        };
        if !self.create.format.can_write() {
            self.create.format = CreateChoices::default().format;
        }
        let mut seen = Vec::new();
        self.recent.retain(|archive| {
            let fresh = !archive.is_empty() && !seen.contains(archive);
            if fresh {
                seen.push(archive.clone());
            }
            fresh
        });
        self.recent.truncate(RECENT_KEPT);
        crate::passwords::normalize_names(&mut self.saved_passwords);
        if self.keys.view.trim().is_empty() {
            self.keys.view = Keys::default().view;
        }
        if self.keys.edit.trim().is_empty() {
            self.keys.edit = Keys::default().edit;
        }
        if self.keys.copy.trim().is_empty() {
            self.keys.copy = Keys::default().copy;
        }
        if self.keys.extract.trim().is_empty() {
            self.keys.extract = Keys::default().extract;
        }
        self.icons.retain(|format, icon| {
            ArchiveFormat::from_id(format).is_some() && FORMAT_ICONS.contains(&icon.as_str())
        });
        self
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_unknown_language_follows_the_system() {
        let settings = Settings {
            language: String::from("xx"),
            ..Settings::default()
        };
        assert_eq!(settings.normalized().language, "");
        let settings = Settings {
            language: String::from(" RU "),
            ..Settings::default()
        };
        assert_eq!(settings.normalized().language, "ru");
    }

    #[test]
    fn fields_missing_in_the_file_take_defaults() {
        let settings: Settings = serde_json::from_str(r#"{"theme":"dark"}"#).unwrap();
        assert_eq!(settings.theme, Theme::Dark);
        assert_eq!(settings.language, "");
        assert!(!settings.preview);
        assert_eq!(settings.extract.place, ExtractPlace::Named);
        assert_eq!(settings.create.format, ArchiveFormat::Zip);

        let settings: Settings =
            serde_json::from_str(r#"{"extract":{"place":"chosen","conflicts":"rename"}}"#).unwrap();
        assert_eq!(settings.extract.place, ExtractPlace::Chosen);
        assert_eq!(settings.extract.conflicts, ConflictPolicy::Rename);
        assert_eq!(settings.extract.symlinks, SymlinkPolicy::Skip);
    }

    #[test]
    fn an_archive_is_not_created_in_a_format_that_is_only_read() {
        let settings: Settings =
            serde_json::from_str(r#"{"create":{"format":"rar","level":"ultra"}}"#).unwrap();
        let settings = settings.normalized();
        assert_eq!(settings.create.format, ArchiveFormat::Zip);
        assert_eq!(settings.create.level, Level::Ultra);
        let settings: Settings = serde_json::from_str(r#"{"create":{"format":"7z"}}"#).unwrap();
        assert_eq!(settings.normalized().create.format, ArchiveFormat::SevenZ);
    }

    #[test]
    fn the_archives_opened_lately_are_few_and_the_latest_is_first() {
        let mut settings = Settings::default();
        for number in 0..20 {
            settings.opened(&format!("{number}.zip"));
        }
        settings.opened("5.zip");
        assert_eq!(settings.recent.len(), RECENT_KEPT);
        assert_eq!(settings.recent[..3], ["5.zip", "19.zip", "18.zip"]);
        assert_eq!(settings.clone().normalized(), settings);

        let settings: Settings =
            serde_json::from_str(r#"{"recent":["a.zip","","b.zip","a.zip"]}"#).unwrap();
        assert_eq!(settings.normalized().recent, ["a.zip", "b.zip"]);
    }
}

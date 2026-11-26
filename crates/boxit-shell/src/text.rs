//! Labels and the choice of commands. Kept free of COM so a test can check
//! them without Explorer.

use std::path::Path;

/// One line of the BoxIt submenu.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Action {
    Open,
    ExtractTo,
    ExtractHere,
    Add,
    Email,
}

impl Action {
    /// Argument of `--shell`. None opens the window on the file.
    pub const fn flag(self) -> Option<&'static str> {
        match self {
            Self::Open => None,
            Self::ExtractTo => Some("extract-to"),
            Self::ExtractHere => Some("extract-here"),
            Self::Add => Some("add"),
            Self::Email => Some("email"),
        }
    }

    /// The dictionary key, `shell.<name>`.
    #[cfg(test)]
    const fn key(self) -> &'static str {
        match self {
            Self::Open => "open",
            Self::ExtractTo => "extractTo",
            Self::ExtractHere => "extractHere",
            Self::Add => "add",
            Self::Email => "email",
        }
    }

    pub fn title(self, russian: bool) -> &'static str {
        match (russian, self) {
            (true, Self::Open) => "Открыть в BoxIt",
            (false, Self::Open) => "Open in BoxIt",
            (true, Self::ExtractTo) => "Распаковать в папку",
            (false, Self::ExtractTo) => "Extract to a folder",
            (true, Self::ExtractHere) => "Распаковать здесь",
            (false, Self::ExtractHere) => "Extract here",
            (true, Self::Add) => "Добавить в архив",
            (false, Self::Add) => "Add to an archive",
            (true, Self::Email) => "Сжать и отправить",
            (false, Self::Email) => "Compress and email",
        }
    }
}

/// Shown for an archive: open it, or unpack it.
const ARCHIVE: &[Action] = &[Action::Open, Action::ExtractTo, Action::ExtractHere];

/// Shown for a file or a folder that is not an archive.
const PLAIN: &[Action] = &[Action::Add, Action::Email];

/// Extensions of one segment, the same set `ArchiveFormat` publishes.
const EXTENSIONS: &[&str] = &[
    "zip", "zipx", "jar", "war", "ear", "apk", "ipa", "xpi", "epub", "7z", "tar", "gtar", "ustar",
    "tgz", "taz", "tbz2", "tbz", "tb2", "txz", "tzst", "tlz4", "gz", "gzip", "bz2", "bzip2", "xz",
    "zst", "zstd", "lz4", "rar", "cab", "iso", "ar", "a", "deb", "lib", "cpio", "lha", "lzh",
    "wim", "swm", "esd",
];

/// Whether the file name is one of the archive types BoxIt opens.
pub fn is_archive(path: &Path) -> bool {
    let Some(name) = path.file_name().and_then(|name| name.to_str()) else {
        return false;
    };
    let name = name.to_lowercase();
    EXTENSIONS.iter().any(|extension| {
        name.strip_suffix(extension)
            .is_some_and(|rest| rest.ends_with('.'))
    })
}

/// Commands for this selection. An empty selection is treated as an archive
/// so a menu built before the selection arrives still has the unpack lines.
pub fn actions_for(files: &[std::path::PathBuf]) -> &'static [Action] {
    if files.is_empty() || files.iter().all(|path| is_archive(path)) {
        ARCHIVE
    } else {
        PLAIN
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn extensions_match_the_formats() {
        let mut ours: Vec<&str> = EXTENSIONS.to_vec();
        ours.sort_unstable();
        ours.dedup();
        let mut formats: Vec<&str> = boxit_core::ArchiveFormat::ALL
            .into_iter()
            .flat_map(|format| format.extensions().iter().copied())
            .filter(|extension| !extension.contains('.'))
            .collect();
        formats.sort_unstable();
        formats.dedup();
        assert_eq!(ours, formats);
    }

    #[test]
    fn titles_match_the_dictionaries() {
        let russian: serde_json::Value =
            serde_json::from_str(include_str!("../../../src/i18n/ru.json")).expect("ru");
        let english: serde_json::Value =
            serde_json::from_str(include_str!("../../../src/i18n/en.json")).expect("en");
        for action in [
            Action::Open,
            Action::ExtractTo,
            Action::ExtractHere,
            Action::Add,
            Action::Email,
        ] {
            assert_eq!(
                action.title(true),
                russian["shell"][action.key()].as_str().unwrap()
            );
            assert_eq!(
                action.title(false),
                english["shell"][action.key()].as_str().unwrap()
            );
        }
    }

    #[test]
    fn an_archive_offers_unpack_and_a_folder_offers_packing() {
        assert!(is_archive(Path::new("Playwrite_BR_Guides.rar")));
        assert!(is_archive(Path::new("notes.tar.gz")));
        assert!(!is_archive(Path::new("notes.txt")));
        assert_eq!(actions_for(&[Path::new("a.rar").to_path_buf()]), ARCHIVE);
        assert_eq!(actions_for(&[Path::new("photos").to_path_buf()]), PLAIN);
    }
}

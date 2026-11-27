//! Small JSON files of the data folder: settings and the like.
//!
//! A file is replaced in one step (written beside itself, then renamed), so a
//! crash or a power cut leaves either the old content or the new one, never a
//! half.

use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};

use boxit_core::{Error, Result};
use serde::de::DeserializeOwned;
use serde::Serialize;

fn beside(path: &Path, suffix: &str) -> PathBuf {
    let mut name = path.file_name().unwrap_or_default().to_os_string();
    name.push(suffix);
    path.with_file_name(name)
}

/// Reads a file of the data folder. A missing file gives the default value.
/// A file that cannot be understood is set aside as `<name>.bad`, so the next
/// save does not destroy what the user may want to repair by hand, and the
/// default value is used.
pub fn load<T: DeserializeOwned + Default>(path: &Path) -> T {
    let Ok(text) = fs::read_to_string(path) else {
        return T::default();
    };
    match serde_json::from_str(text.trim_start_matches('\u{feff}')) {
        Ok(value) => value,
        Err(error) => {
            tracing::warn!(file = %path.display(), "the file is damaged and was set aside: {error}");
            let _ = fs::rename(path, beside(path, ".bad"));
            T::default()
        }
    }
}

/// Writes a file of the data folder, replacing the old one in one step.
pub fn save<T: Serialize>(path: &Path, value: &T) -> Result<()> {
    let text = serde_json::to_string_pretty(value)
        .map_err(|error| Error::new("internal").detail(error))?;
    if let Some(folder) = path.parent() {
        fs::create_dir_all(folder)?;
    }
    let draft = beside(path, ".new");
    let written = fs::File::create(&draft).and_then(|mut file| {
        file.write_all(text.as_bytes())?;
        file.sync_all()
    });
    if let Err(error) = written.and_then(|()| fs::rename(&draft, path)) {
        let _ = fs::remove_file(&draft);
        return Err(Error::from(error).with("path", path.display()));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use serde::Deserialize;

    use super::*;

    #[derive(Debug, Default, PartialEq, Serialize, Deserialize)]
    struct Sample {
        name: String,
        count: u32,
    }

    #[test]
    fn a_value_comes_back_as_it_was_saved() {
        let folder = tempfile::tempdir().unwrap();
        let path = folder.path().join("nested").join("sample.json");
        let value = Sample {
            name: String::from("архив"),
            count: 3,
        };
        save(&path, &value).unwrap();
        assert_eq!(load::<Sample>(&path), value);
        assert!(!beside(&path, ".new").exists());
    }

    #[test]
    fn a_missing_file_gives_the_default() {
        let folder = tempfile::tempdir().unwrap();
        assert_eq!(
            load::<Sample>(&folder.path().join("none.json")),
            Sample::default()
        );
    }

    #[test]
    fn a_damaged_file_is_set_aside() {
        let folder = tempfile::tempdir().unwrap();
        let path = folder.path().join("sample.json");
        fs::write(&path, "{ not json").unwrap();
        assert_eq!(load::<Sample>(&path), Sample::default());
        assert!(!path.exists());
        assert_eq!(
            fs::read_to_string(beside(&path, ".bad")).unwrap(),
            "{ not json"
        );
    }

    #[test]
    fn a_byte_order_mark_is_accepted() {
        let folder = tempfile::tempdir().unwrap();
        let path = folder.path().join("sample.json");
        fs::write(&path, "\u{feff}{\"name\":\"a\",\"count\":1}").unwrap();
        assert_eq!(load::<Sample>(&path).count, 1);
    }
}

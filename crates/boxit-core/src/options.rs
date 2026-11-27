//! Choices the user makes for an operation.

use std::fmt;

use serde::{Deserialize, Serialize};
use zeroize::Zeroizing;

use crate::format::ArchiveFormat;

/// A password. Its memory is wiped when it is dropped and it never appears
/// in logs or debug output.
#[derive(Clone, PartialEq, Eq)]
pub struct Password(Zeroizing<String>);

impl Password {
    pub fn new(text: impl Into<String>) -> Self {
        Self(Zeroizing::new(text.into()))
    }

    /// The text of the password, for the code that derives a key from it.
    pub fn expose(&self) -> &str {
        &self.0
    }

    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }
}

impl fmt::Debug for Password {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("Password(***)")
    }
}

impl From<&str> for Password {
    fn from(text: &str) -> Self {
        Self::new(text)
    }
}

/// What to do when a file to extract already exists.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum ConflictPolicy {
    /// Ask through [`crate::archive::Interaction`]; without anyone to ask,
    /// the existing file is kept.
    #[default]
    Ask,
    /// Keep the existing file.
    Skip,
    Overwrite,
    /// Extract under a free name: `name (2).txt`.
    Rename,
}

/// What to do with symbolic links found in an archive.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum SymlinkPolicy {
    /// Leave them out.
    #[default]
    Skip,
    /// Create the links whose targets stay inside the extracted folder.
    Extract,
    /// Ask about each through [`crate::archive::Interaction`].
    Ask,
}

/// Limits that protect the disk from an archive made to fill it.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct Limits {
    /// The largest file an archive may unpack, in bytes.
    pub max_file_size: u64,
    /// The largest ratio of unpacked to packed size of a whole archive.
    pub max_ratio: u64,
    /// The ratio is not judged until this many bytes are unpacked: a tiny
    /// archive of a file of zeros is not an attack.
    pub ratio_floor: u64,
    /// Refuse to start when the content does not fit on the disk.
    pub check_free_space: bool,
}

impl Limits {
    pub const DEFAULT_MAX_FILE_SIZE: u64 = 100 * 1024 * 1024 * 1024;
    pub const DEFAULT_MAX_RATIO: u64 = 10_000;
    pub const DEFAULT_RATIO_FLOOR: u64 = 64 * 1024 * 1024;

    /// No limits at all, for the user who knows what the archive is.
    pub fn none() -> Self {
        Self {
            max_file_size: u64::MAX,
            max_ratio: u64::MAX,
            ratio_floor: 0,
            check_free_space: false,
        }
    }
}

impl Default for Limits {
    fn default() -> Self {
        Self {
            max_file_size: Self::DEFAULT_MAX_FILE_SIZE,
            max_ratio: Self::DEFAULT_MAX_RATIO,
            ratio_floor: Self::DEFAULT_RATIO_FLOOR,
            check_free_space: true,
        }
    }
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct ExtractOptions {
    pub conflicts: ConflictPolicy,
    pub symlinks: SymlinkPolicy,
    pub limits: Limits,
}

/// How hard to compress. Every format maps the five levels to its own scale.
#[derive(
    Clone, Copy, Debug, Default, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize,
)]
#[serde(rename_all = "camelCase")]
pub enum Level {
    /// No compression.
    Store,
    Fast,
    #[default]
    Normal,
    Maximum,
    Ultra,
}

impl Level {
    pub const ALL: [Self; 5] = [
        Self::Store,
        Self::Fast,
        Self::Normal,
        Self::Maximum,
        Self::Ultra,
    ];
}

/// A compression method, where a format offers a choice.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum Method {
    Store,
    Deflate,
    Deflate64,
    Bzip2,
    Lzma,
    Lzma2,
    Ppmd,
    Zstd,
    Xz,
}

impl Method {
    /// The name shown in the list of an archive.
    pub fn name(self) -> &'static str {
        match self {
            Self::Store => "Store",
            Self::Deflate => "Deflate",
            Self::Deflate64 => "Deflate64",
            Self::Bzip2 => "BZip2",
            Self::Lzma => "LZMA",
            Self::Lzma2 => "LZMA2",
            Self::Ppmd => "PPMd",
            Self::Zstd => "Zstandard",
            Self::Xz => "XZ",
        }
    }
}

/// The smallest and the largest dictionary a user can ask for.
pub const MIN_DICTIONARY: u64 = 64 * 1024;
pub const MAX_DICTIONARY: u64 = 1024 * 1024 * 1024;

/// How to create an archive.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CreateOptions {
    pub format: ArchiveFormat,
    pub level: Level,
    /// `None` takes the usual method of the format.
    pub method: Option<Method>,
    /// Compress files as one stream (7z): better ratio, slower access to a
    /// single file.
    pub solid: bool,
    /// Size of the dictionary in bytes; `None` follows the level.
    pub dictionary: Option<u64>,
    pub comment: Option<String>,
    pub password: Option<Password>,
    /// Hide the names of the files as well as their content (7z).
    pub encrypt_names: bool,
    /// Worker threads to use; 0 takes one per processor.
    pub threads: usize,
}

impl CreateOptions {
    pub fn new(format: ArchiveFormat) -> Self {
        Self {
            format,
            level: Level::default(),
            method: None,
            solid: true,
            dictionary: None,
            comment: None,
            password: None,
            encrypt_names: false,
            threads: 0,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_password_does_not_show_in_debug_output() {
        let password = Password::new("hunter2");
        assert_eq!(format!("{password:?}"), "Password(***)");
        assert_eq!(password.expose(), "hunter2");
        let options = CreateOptions {
            password: Some(password),
            ..CreateOptions::new(ArchiveFormat::Zip)
        };
        assert!(!format!("{options:?}").contains("hunter2"));
    }

    #[test]
    fn default_limits_follow_the_requirements() {
        let limits = Limits::default();
        assert_eq!(limits.max_file_size, 100 << 30);
        assert_eq!(limits.max_ratio, 10_000);
        assert!(limits.check_free_space);
        let parsed: Limits = serde_json::from_str(r#"{"maxFileSize":5}"#).unwrap();
        assert_eq!(parsed.max_file_size, 5);
        assert_eq!(parsed.max_ratio, 10_000);
    }

    #[test]
    fn levels_are_ordered_from_fastest_to_strongest() {
        assert!(Level::ALL.windows(2).all(|pair| pair[0] < pair[1]));
        assert_eq!(Level::default(), Level::Normal);
    }
}

//! What an archive holds: files, folders and links, with their attributes.

use std::borrow::Cow;

use serde::{Deserialize, Serialize};

/// A moment as seconds and nanoseconds since 1970-01-01 UTC. Archives store
/// times with very different precision; this keeps whatever they have.
#[derive(
    Clone, Copy, Debug, Default, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize,
)]
pub struct Timestamp {
    pub seconds: i64,
    pub nanos: u32,
}

impl Timestamp {
    pub fn from_seconds(seconds: i64) -> Self {
        Self { seconds, nanos: 0 }
    }
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum EntryKind {
    #[default]
    File,
    Directory,
    /// A name that points to another path; `link_target` holds the path.
    Symlink,
    /// Another name of a file stored earlier in the same archive;
    /// `link_target` holds the path of that entry.
    Hardlink,
}

/// What is known about a file before its data: enough to store it in an
/// archive or to create it on the disk.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct EntryMeta {
    /// Path inside the archive, as the archive stores it, with `/` between
    /// the parts. It is not safe to use on the disk as it is.
    pub path: String,
    pub kind: EntryKind,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub link_target: Option<String>,
    /// Size of the data; some streaming formats do not know it in advance.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub size: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub modified: Option<Timestamp>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub created: Option<Timestamp>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub accessed: Option<Timestamp>,
    /// Permission bits of Unix, without the file type.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub unix_mode: Option<u32>,
    /// `FILE_ATTRIBUTE_*` bits of Windows.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub windows_attributes: Option<u32>,
}

impl EntryMeta {
    pub fn file(path: impl Into<String>, size: u64) -> Self {
        Self {
            path: path.into(),
            size: Some(size),
            ..Self::default()
        }
    }

    pub fn directory(path: impl Into<String>) -> Self {
        Self {
            path: path.into(),
            kind: EntryKind::Directory,
            size: Some(0),
            ..Self::default()
        }
    }

    pub fn symlink(path: impl Into<String>, target: impl Into<String>) -> Self {
        Self {
            path: path.into(),
            kind: EntryKind::Symlink,
            link_target: Some(target.into()),
            size: Some(0),
            ..Self::default()
        }
    }

    pub fn is_dir(&self) -> bool {
        self.kind == EntryKind::Directory
    }

    pub fn is_file(&self) -> bool {
        self.kind == EntryKind::File
    }

    /// The last part of the path.
    pub fn name(&self) -> &str {
        let trimmed = self.path.trim_end_matches('/');
        trimmed.rsplit('/').next().unwrap_or(trimmed)
    }
}

/// An entry of an existing archive.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct Entry {
    /// Position of the entry in the archive; names may repeat, this does not.
    pub index: u64,
    #[serde(flatten)]
    pub meta: EntryMeta,
    /// Bytes the data takes in the archive. Entries of a solid block share
    /// their compressed bytes, so only the first one of a block has a size.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub packed_size: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub crc32: Option<u32>,
    pub encrypted: bool,
    /// Name of the compression method, such as `Deflate` or `LZMA2`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub method: Option<Cow<'static, str>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub comment: Option<String>,
}

/// Which entries an operation applies to.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub enum Selection {
    #[default]
    All,
    /// Entries by their position in the archive, sorted.
    Indexes(Vec<u64>),
    /// Entries with one of these paths, and everything inside folders with
    /// these paths.
    Paths(Vec<String>),
}

impl Selection {
    pub fn indexes(mut indexes: Vec<u64>) -> Self {
        indexes.sort_unstable();
        indexes.dedup();
        Self::Indexes(indexes)
    }

    pub fn paths(paths: impl IntoIterator<Item = impl Into<String>>) -> Self {
        Self::Paths(
            paths
                .into_iter()
                .map(|path| path.into().trim_end_matches('/').to_owned())
                .collect(),
        )
    }

    pub fn is_all(&self) -> bool {
        matches!(self, Self::All)
    }

    pub fn contains(&self, entry: &Entry) -> bool {
        match self {
            Self::All => true,
            Self::Indexes(indexes) => indexes.binary_search(&entry.index).is_ok(),
            Self::Paths(paths) => {
                let path = entry.meta.path.trim_end_matches('/');
                paths.iter().any(|picked| {
                    path.strip_prefix(picked.as_str())
                        .is_some_and(|rest| rest.is_empty() || rest.starts_with('/'))
                })
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn entry(index: u64, path: &str) -> Entry {
        Entry {
            index,
            meta: EntryMeta::file(path, 1),
            ..Entry::default()
        }
    }

    #[test]
    fn the_name_is_the_last_part_of_the_path() {
        assert_eq!(EntryMeta::file("a/b/c.txt", 0).name(), "c.txt");
        assert_eq!(EntryMeta::directory("a/b/").name(), "b");
        assert_eq!(EntryMeta::file("c.txt", 0).name(), "c.txt");
        assert_eq!(EntryMeta::file("", 0).name(), "");
    }

    #[test]
    fn a_selection_by_index_ignores_order_and_repeats() {
        let selection = Selection::indexes(vec![5, 1, 5, 3]);
        assert_eq!(selection, Selection::Indexes(vec![1, 3, 5]));
        assert!(selection.contains(&entry(3, "x")));
        assert!(!selection.contains(&entry(2, "x")));
        assert!(Selection::All.contains(&entry(2, "x")));
    }

    #[test]
    fn a_selected_folder_brings_its_content_and_nothing_else() {
        let selection = Selection::paths(["docs/", "a.txt"]);
        assert!(selection.contains(&entry(0, "docs")));
        assert!(selection.contains(&entry(0, "docs/")));
        assert!(selection.contains(&entry(0, "docs/x/y.txt")));
        assert!(selection.contains(&entry(0, "a.txt")));
        assert!(!selection.contains(&entry(0, "docs2/y.txt")));
        assert!(!selection.contains(&entry(0, "a.txt.bak")));
        assert!(!selection.contains(&entry(0, "b/a.txt")));
    }

    #[test]
    fn an_entry_is_one_flat_object_in_json() {
        let json = serde_json::to_value(entry(7, "a/b.txt")).unwrap();
        assert_eq!(json["index"], 7);
        assert_eq!(json["path"], "a/b.txt");
        assert_eq!(json["kind"], "file");
        assert_eq!(json["size"], 1);
        assert!(json.get("crc32").is_none());
        let back: Entry = serde_json::from_value(json).unwrap();
        assert_eq!(back, entry(7, "a/b.txt"));
    }
}

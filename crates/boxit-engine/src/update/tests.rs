use std::fs;
use std::path::Path;

use boxit_codecs::OpenOptions;
use boxit_core::{ArchiveFormat, CreateOptions, ExtractOptions, Level, OpContext, Selection};
use tempfile::TempDir;

use super::{update, UpdateRequest};
use crate::archive::Archive;
use crate::create::{create, CreateRequest, Layout};
use crate::testing::{plant, tree};

fn stored(format: ArchiveFormat) -> CreateOptions {
    CreateOptions {
        level: Level::Store,
        ..CreateOptions::new(format)
    }
}

fn packed(root: &Path, name: &str, options: CreateOptions) -> std::path::PathBuf {
    let folder = root.join("set");
    plant(
        &folder,
        &[
            ("docs/left.txt", "left stays"),
            ("docs/right.txt", "right stays"),
            ("note.txt", "a note"),
        ],
    );
    let destination = root.join(name);
    create(
        &CreateRequest {
            sources: vec![folder],
            destination: destination.clone(),
            options,
            overwrite: false,
            layout: Layout::Whole,
        },
        &OpContext::silent(),
    )
    .unwrap();
    destination
}

fn index_of(path: &Path, name: &str) -> u64 {
    let entries = Archive::open(path, OpenOptions::default())
        .unwrap()
        .entries(&OpContext::silent())
        .unwrap();
    entries
        .iter()
        .find(|entry| entry.meta.path.trim_end_matches('/') == name)
        .unwrap_or_else(|| panic!("{name} is missing"))
        .index
}

fn names(path: &Path) -> Vec<String> {
    let mut archive = Archive::open(path, OpenOptions::default()).unwrap();
    let mut listed = archive.entries(&OpContext::silent()).unwrap();
    listed.sort_by(|one, other| one.meta.path.cmp(&other.meta.path));
    listed
        .into_iter()
        .map(|entry| entry.meta.path)
        .filter(|path| !path.ends_with('/'))
        .collect()
}

#[test]
fn a_file_is_left_out_and_another_is_renamed_without_repacking_the_rest() {
    let root = TempDir::new().unwrap();
    let archive = packed(root.path(), "notes.zip", stored(ArchiveFormat::Zip));
    let before = fs::metadata(&archive).unwrap().len();
    let removed = index_of(&archive, "set/note.txt");
    let right = index_of(&archive, "set/docs/right.txt");
    let mut renamed = std::collections::BTreeMap::new();
    renamed.insert(right, String::from("set/docs/moved.txt"));
    let updated = update(
        &UpdateRequest {
            archive: archive.clone(),
            destination: archive.clone(),
            removed: [removed].into(),
            renamed,
            added: Vec::new(),
            replaced: Vec::new(),
        },
        OpenOptions::default(),
        &OpContext::silent(),
    )
    .unwrap();
    assert!(!updated.repacked);
    assert!(updated.packed > 0);
    assert_eq!(
        names(&archive),
        ["set/docs/left.txt", "set/docs/moved.txt"]
    );
    // The file that was there is the new archive, not a second copy.
    assert!(fs::metadata(&archive).unwrap().len() < before + 64);
    let out = root.path().join("out");
    Archive::open(&archive, OpenOptions::default())
        .unwrap()
        .extract(
            &out,
            Selection::All,
            ExtractOptions::default(),
            &OpContext::silent(),
        )
        .unwrap();
    let mut expected = tree(&root.path().join("set"));
    expected.remove("docs/right.txt");
    expected.remove("note.txt");
    expected.insert(String::from("docs/moved.txt"), String::from("right stays"));
    assert_eq!(tree(&out.join("set")), expected);
}

#[test]
fn a_file_is_added_to_an_archive() {
    let root = TempDir::new().unwrap();
    let archive = packed(root.path(), "notes.zip", stored(ArchiveFormat::Zip));
    let extra = root.path().join("extra.txt");
    fs::write(&extra, "more").unwrap();
    update(
        &UpdateRequest {
            archive: archive.clone(),
            destination: archive.clone(),
            removed: std::collections::BTreeSet::new(),
            renamed: std::collections::BTreeMap::new(),
            added: vec![extra],
            replaced: Vec::new(),
        },
        OpenOptions::default(),
        &OpContext::silent(),
    )
    .unwrap();
    let listed = names(&archive);
    assert!(listed.iter().any(|name| name == "extra.txt"), "{listed:?}");
}

#[test]
fn a_compressed_tar_is_packed_again() {
    let root = TempDir::new().unwrap();
    let archive = packed(
        root.path(),
        "notes.tar.gz",
        CreateOptions::new(ArchiveFormat::TarGz),
    );
    let removed = index_of(&archive, "set/note.txt");
    let updated = update(
        &UpdateRequest {
            archive: archive.clone(),
            destination: archive.clone(),
            removed: [removed].into(),
            renamed: std::collections::BTreeMap::new(),
            added: Vec::new(),
            replaced: Vec::new(),
        },
        OpenOptions::default(),
        &OpContext::silent(),
    )
    .unwrap();
    assert!(updated.repacked);
    assert_eq!(
        names(&archive),
        ["set/docs/left.txt", "set/docs/right.txt"]
    );
}

#[test]
fn a_format_that_is_only_read_is_not_changed() {
    let root = TempDir::new().unwrap();
    // A tar is written, then the test above covers tar.gz. Here an empty
    // change is refused before the format is asked.
    let archive = packed(root.path(), "notes.zip", stored(ArchiveFormat::Zip));
    let error = update(
        &UpdateRequest {
            archive,
            destination: root.path().join("other.zip"),
            removed: std::collections::BTreeSet::new(),
            renamed: std::collections::BTreeMap::new(),
            added: Vec::new(),
            replaced: Vec::new(),
        },
        OpenOptions::default(),
        &OpContext::silent(),
    )
    .unwrap_err();
    assert_eq!(error.code, "nothingToAdd");
}

use std::fs;
use std::path::Path;

use boxit_codecs::OpenOptions;
use boxit_core::{ArchiveFormat, CreateOptions, Entry, Level, OpContext};
use tempfile::TempDir;

use super::{compare, convert, estimate, find, name_matches, ConvertRequest};
use crate::archive::Archive;
use crate::create::{create, CreateRequest, Layout};
use crate::testing::plant;

fn packed(root: &Path, name: &str, format: ArchiveFormat) -> std::path::PathBuf {
    let folder = root.join(format!("src-{name}"));
    plant(
        &folder,
        &[
            ("docs/left.txt", "left"),
            ("docs/right.txt", "right"),
            ("note.txt", "a note"),
        ],
    );
    let destination = root.join(name);
    create(
        &CreateRequest {
            sources: vec![folder],
            destination: destination.clone(),
            options: CreateOptions {
                level: Level::Store,
                ..CreateOptions::new(format)
            },
            overwrite: false,
            layout: Layout::Whole,
        },
        &OpContext::silent(),
    )
    .unwrap();
    destination
}

fn listed(path: &Path) -> Vec<Entry> {
    Archive::open(path, OpenOptions::default())
        .unwrap()
        .entries(&OpContext::silent())
        .unwrap()
}

#[test]
fn an_archive_is_converted_without_a_folder_of_files() {
    let root = TempDir::new().unwrap();
    let zip = packed(root.path(), "notes.zip", ArchiveFormat::Zip);
    let seven = root.path().join("notes.7z");
    let created = convert(
        &ConvertRequest {
            source: zip,
            destination: seven.clone(),
            options: CreateOptions {
                level: Level::Store,
                ..CreateOptions::new(ArchiveFormat::SevenZ)
            },
            overwrite: false,
        },
        OpenOptions::default(),
        &OpContext::silent(),
    )
    .unwrap();
    assert_eq!(created.files, 3);
    let names: Vec<_> = listed(&seven)
        .into_iter()
        .map(|entry| entry.meta.path)
        .filter(|path| !path.ends_with('/'))
        .collect();
    assert!(
        names.iter().any(|name| name.ends_with("note.txt")),
        "{names:?}"
    );
    // The conversion did not leave a folder of unpacked files beside the archive.
    assert!(fs::read_dir(root.path()).unwrap().all(|entry| {
        let path = entry.unwrap().path();
        path.is_file()
            || path
                .file_name()
                .is_some_and(|name| name.to_string_lossy().starts_with("src-"))
    }));
}

#[test]
fn two_archives_are_compared_by_name_size_and_crc() {
    let root = TempDir::new().unwrap();
    let left = listed(&packed(root.path(), "a.zip", ArchiveFormat::Zip));
    let mut right = left.clone();
    right.retain(|entry| !entry.meta.path.ends_with("note.txt"));
    let compared = compare(&left, &right);
    assert!(compared
        .only_left
        .iter()
        .any(|path| path.ends_with("note.txt")));
    assert_eq!(compared.only_right, Vec::<String>::new());
    assert_eq!(compared.different, Vec::<super::Different>::new());
    assert!(name_matches("Docs/Left.TXT", "*.txt"));
    assert!(name_matches("note.txt", "re:^note"));
    assert!(name_matches("note.txt", "NOTE"));
    let found = find(&left, "*.txt").unwrap();
    assert!(found.len() >= 3);
    assert_eq!(find(&left, "re:(").unwrap_err().code, "badPattern");
}

#[test]
fn a_sample_estimates_the_whole_set() {
    let root = TempDir::new().unwrap();
    let folder = root.path().join("docs");
    plant(
        &folder,
        &[
            ("a.txt", "alpha alpha alpha"),
            ("b.txt", "beta beta beta beta"),
        ],
    );
    let estimate = estimate(
        &[folder],
        &CreateOptions::new(ArchiveFormat::Zip),
        &OpContext::silent(),
    )
    .unwrap();
    assert!(estimate.sample_bytes > 0);
    assert!(estimate.packed > 0);
    assert!(estimate.low <= estimate.packed && estimate.packed <= estimate.high);
}

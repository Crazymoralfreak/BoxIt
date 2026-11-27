use std::fs;
use std::path::{Path, PathBuf};

use boxit_codecs::OpenOptions;
use boxit_core::{
    ArchiveFormat, CreateOptions, ExtractOptions, Level, OpContext, Password, Selection,
};
use tempfile::TempDir;

use super::{beside, repair, RepairRequest};
use crate::archive::Archive;
use crate::create::{create, CreateRequest, Layout};
use crate::fsx;
use crate::testing::{plant, tree};

fn stored(format: ArchiveFormat) -> CreateOptions {
    CreateOptions {
        level: Level::Store,
        ..CreateOptions::new(format)
    }
}

/// Stores a small folder and returns the archive and the folder.
fn packed(root: &Path, name: &str, options: CreateOptions) -> (PathBuf, PathBuf) {
    let folder = root.join("set");
    plant(
        &folder,
        &[
            ("docs/left.txt", "the left file stays"),
            ("docs/right.txt", "BREAK-THIS-FILE-MARKER and the rest"),
            ("docs/empty/", ""),
            ("note.txt", "a short note that is whole"),
        ],
    );
    let destination = root.join(name);
    let request = CreateRequest {
        sources: vec![folder.clone()],
        destination: destination.clone(),
        options,
        overwrite: false,
        layout: Layout::Whole,
    };
    create(&request, &OpContext::silent()).unwrap();
    (destination, folder)
}

fn request(source: &Path, destination: &Path) -> RepairRequest {
    RepairRequest {
        source: source.to_path_buf(),
        destination: destination.to_path_buf(),
        overwrite: false,
    }
}

fn unpacked(archive: &Path, folder: &Path) {
    Archive::open(archive, OpenOptions::default())
        .unwrap()
        .extract(
            folder,
            Selection::All,
            ExtractOptions::default(),
            &OpContext::silent(),
        )
        .unwrap();
}

#[test]
fn the_offered_name_keeps_the_extension() {
    assert_eq!(
        beside(Path::new("C:/archives/photos.zip")),
        PathBuf::from("C:/archives/photos.repaired.zip")
    );
    assert_eq!(
        beside(Path::new("backup.tar.gz")),
        PathBuf::from("backup.repaired.tar.gz")
    );
    assert_eq!(
        beside(Path::new("notes")),
        PathBuf::from("notes.repaired.zip")
    );
}

#[test]
fn a_zip_that_lost_its_list_is_repaired_from_the_headers() {
    let root = TempDir::new().unwrap();
    let (archive, folder) = packed(root.path(), "broken.zip", stored(ArchiveFormat::Zip));
    let original = fs::read(&archive).unwrap();
    let mut bytes = original.clone();
    let end = bytes
        .windows(4)
        .rposition(|window| window == b"PK\x05\x06")
        .unwrap();
    bytes.truncate(end);
    fs::write(&archive, &bytes).unwrap();

    let destination = root.path().join("fixed.zip");
    let repaired = repair(
        &request(&archive, &destination),
        OpenOptions::default(),
        &OpContext::silent(),
    )
    .unwrap();
    assert!(repaired.rebuilt, "{repaired:?}");
    assert_eq!(repaired.lost_total, 0);
    assert_eq!(repaired.files, 3);
    // The damaged archive is left as it was found.
    assert_eq!(fs::read(&archive).unwrap(), bytes);
    assert_ne!(bytes, original);

    let out = root.path().join("out");
    unpacked(&destination, &out);
    assert_eq!(tree(&out.join("set")), tree(&folder));
}

#[test]
fn a_damaged_file_is_left_out_and_the_rest_are_kept() {
    let root = TempDir::new().unwrap();
    let (archive, folder) = packed(root.path(), "torn.zip", stored(ArchiveFormat::Zip));
    let mut bytes = fs::read(&archive).unwrap();
    let mark = b"BREAK-THIS-FILE-MARKER";
    let place = bytes
        .windows(mark.len())
        .position(|window| window == mark)
        .unwrap();
    bytes[place] ^= 0x01;
    fs::write(&archive, &bytes).unwrap();

    let destination = beside(&archive);
    let repaired = repair(
        &request(&archive, &destination),
        OpenOptions::default(),
        &OpContext::silent(),
    )
    .unwrap();
    assert!(!repaired.rebuilt);
    assert!(repaired.lost_total >= 1, "{repaired:?}");
    assert!(repaired
        .lost
        .iter()
        .any(|lost| lost.path.ends_with("right.txt")));

    let out = root.path().join("out");
    unpacked(&destination, &out);
    let mut expected = tree(&folder);
    expected.remove("docs/right.txt");
    assert_eq!(tree(&out.join("set")), expected);
    assert_eq!(fs::read(&archive).unwrap(), bytes);
}

#[test]
fn a_whole_archive_is_written_again() {
    let root = TempDir::new().unwrap();
    for (format, name) in [
        (ArchiveFormat::Zip, "a.zip"),
        (ArchiveFormat::SevenZ, "a.7z"),
    ] {
        let (archive, folder) = packed(root.path(), name, stored(format));
        let destination = root.path().join(format!("fixed.{name}"));
        let repaired = repair(
            &request(&archive, &destination),
            OpenOptions::default(),
            &OpContext::silent(),
        )
        .unwrap();
        assert_eq!(repaired.lost_total, 0, "{name}");
        assert!(!repaired.rebuilt, "{name}");
        let out = root.path().join(format!("out-{name}"));
        unpacked(&destination, &out);
        assert_eq!(tree(&out.join("set")), tree(&folder), "{name}");
    }
}

#[test]
fn a_repair_never_replaces_the_archive_it_reads() {
    let root = TempDir::new().unwrap();
    let (archive, _) = packed(root.path(), "same.zip", stored(ArchiveFormat::Zip));
    let before = fs::read(&archive).unwrap();
    let error = repair(
        &request(&archive, &archive),
        OpenOptions::default(),
        &OpContext::silent(),
    )
    .unwrap_err();
    assert_eq!(error.code, "repairOverItself");
    assert_eq!(fs::read(&archive).unwrap(), before);
    assert_eq!(fsx::existing(&beside(&archive)), fsx::Existing::Nothing);
}

#[test]
fn a_format_that_cannot_be_repaired_says_so() {
    let root = TempDir::new().unwrap();
    let (archive, _) = packed(root.path(), "a.tar", stored(ArchiveFormat::Tar));
    let destination = beside(&archive);
    let error = repair(
        &request(&archive, &destination),
        OpenOptions::default(),
        &OpContext::silent(),
    )
    .unwrap_err();
    assert_eq!(error.code, "cannotRepair");
    assert_eq!(fsx::existing(&destination), fsx::Existing::Nothing);
}

#[test]
fn a_locked_archive_is_copied_without_its_password() {
    let root = TempDir::new().unwrap();
    let options = CreateOptions {
        password: Some(Password::from("open sesame")),
        ..stored(ArchiveFormat::Zip)
    };
    let (archive, folder) = packed(root.path(), "locked.zip", options);
    let destination = root.path().join("fixed.zip");
    let repaired = repair(
        &request(&archive, &destination),
        OpenOptions::default(),
        &OpContext::silent(),
    )
    .unwrap();
    assert_eq!(repaired.unchecked, repaired.files);
    assert_eq!(repaired.lost_total, 0);

    let knows = OpenOptions {
        password: Some(Password::from("open sesame")),
        ..OpenOptions::default()
    };
    let out = root.path().join("out");
    Archive::open(&destination, knows)
        .unwrap()
        .extract(
            &out,
            Selection::All,
            ExtractOptions::default(),
            &OpContext::silent(),
        )
        .unwrap();
    assert_eq!(tree(&out.join("set")), tree(&folder));
}

#[test]
fn nothing_is_written_when_no_file_can_be_read() {
    let root = TempDir::new().unwrap();
    let (archive, _) = packed(root.path(), "gone.zip", stored(ArchiveFormat::Zip));
    let mut bytes = fs::read(&archive).unwrap();
    for mark in [
        b"the left file stays".as_slice(),
        b"BREAK-THIS-FILE-MARKER",
        b"a short note",
    ] {
        let place = bytes
            .windows(mark.len())
            .position(|window| window == mark)
            .unwrap();
        bytes[place] ^= 0x01;
    }
    fs::write(&archive, &bytes).unwrap();
    let destination = beside(&archive);
    let error = repair(
        &request(&archive, &destination),
        OpenOptions::default(),
        &OpContext::silent(),
    )
    .unwrap_err();
    assert_eq!(error.code, "nothingRecovered");
    assert_eq!(fsx::existing(&destination), fsx::Existing::Nothing);
    assert_eq!(fs::read(&archive).unwrap(), bytes);
}

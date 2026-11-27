use std::fs;
use std::sync::atomic::{AtomicUsize, Ordering as AtomicOrdering};
use std::sync::Arc;

use boxit_core::{CreateOptions, Interaction, Level, Password, PasswordRequest};
use tempfile::TempDir;

use super::*;
use crate::create::{create, CreateRequest, Layout};
use crate::testing::{lines, plant, tree, Watch};

fn options(format: ArchiveFormat) -> CreateOptions {
    CreateOptions {
        level: Level::Fast,
        ..CreateOptions::new(format)
    }
}

/// Stores a small folder as `name` and returns the path of the archive.
fn packed(root: &Path, name: &str, options: CreateOptions) -> PathBuf {
    let noise = lines(4000, "");
    plant(
        &root.join("photos"),
        &[
            ("2024/a.txt", "alpha"),
            ("2024/b.txt", "beta"),
            ("2025/c.txt", &noise),
            ("empty/", ""),
        ],
    );
    let destination = root.join(name);
    let request = CreateRequest {
        sources: vec![root.join("photos")],
        destination: destination.clone(),
        options,
        overwrite: false,
        layout: Layout::Whole,
    };
    create(&request, &OpContext::silent()).unwrap();
    destination
}

fn open(path: &Path) -> Archive {
    Archive::open(path, OpenOptions::default()).unwrap()
}

/// Gives one password to whoever asks, and counts the questions.
struct Knows(&'static str, AtomicUsize);

impl Interaction for Knows {
    fn password(&self, _request: &PasswordRequest) -> Option<Password> {
        self.1.fetch_add(1, AtomicOrdering::SeqCst);
        Some(Password::from(self.0))
    }
}

#[test]
fn a_listing_sums_the_archive_up() {
    let root = TempDir::new().unwrap();
    let silent = OpContext::silent();
    for (format, name, solid) in [
        (ArchiveFormat::Zip, "a.zip", false),
        (ArchiveFormat::SevenZ, "a.7z", true),
        (ArchiveFormat::TarGz, "a.tar.gz", true),
        (ArchiveFormat::Tar, "a.tar", false),
    ] {
        let path = packed(root.path(), name, options(format));
        let mut archive = open(&path);
        assert_eq!(archive.format(), format);
        let listing = archive.list(&silent).unwrap();
        assert_eq!(listing.format, format);
        assert_eq!(listing.path, fsx::display(&fsx::long(&path).unwrap()));
        assert_eq!(listing.size, fs::metadata(&path).unwrap().len());
        assert_eq!(listing.files, 3, "{name}");
        assert_eq!(listing.folders, 4, "{name}");
        assert_eq!(listing.entries.len(), 7, "{name}");
        let stored = tree(&root.path().join("photos"));
        assert_eq!(
            listing.bytes,
            stored.values().map(|text| text.len() as u64).sum::<u64>(),
            "{name}"
        );
        assert_eq!(listing.solid, solid, "{name}");
        assert!(!listing.encrypted, "{name}");
        assert_eq!(listing.comment, None);
        // The list is read once and asked for as often as needed.
        assert_eq!(archive.list(&silent).unwrap(), listing);
    }
}

#[test]
fn the_name_of_a_file_does_not_decide_its_format() {
    let root = TempDir::new().unwrap();
    let path = packed(root.path(), "holiday.zip", options(ArchiveFormat::SevenZ));
    assert_eq!(open(&path).format(), ArchiveFormat::SevenZ);

    let text = root.path().join("notes.txt");
    fs::write(&text, b"just some text").unwrap();
    let error = Archive::open(&text, OpenOptions::default()).err().unwrap();
    assert_eq!(error.code, "unknownFormat");

    let missing = root.path().join("missing.zip");
    let error = Archive::open(&missing, OpenOptions::default())
        .err()
        .unwrap();
    assert_eq!(error.code, "notFound");
    assert_eq!(error.params["path"], fsx::display(&missing));

    assert_eq!(recognize(&path), Some(ArchiveFormat::SevenZ));
    assert_eq!(recognize(&text), None);
    assert_eq!(recognize(&missing), None);
    assert_eq!(recognize(root.path()), None);
}

#[test]
fn a_comment_and_a_password_show_in_the_listing() {
    let root = TempDir::new().unwrap();
    let locked = CreateOptions {
        password: Some(Password::from("open sesame")),
        comment: Some(String::from("Летние фотографии")),
        ..options(ArchiveFormat::Zip)
    };
    let path = packed(root.path(), "locked.zip", locked);
    let listing = open(&path).list(&OpContext::silent()).unwrap();
    assert!(listing.encrypted);
    assert_eq!(listing.comment.as_deref(), Some("Летние фотографии"));

    // Without the password the content does not come out, and nothing is
    // left where it would have gone.
    let destination = root.path().join("out");
    let error = open(&path)
        .extract(
            &destination,
            Selection::All,
            ExtractOptions::default(),
            &OpContext::silent(),
        )
        .unwrap_err();
    assert_eq!(error.code, "passwordRequired");
    assert_eq!(fsx::existing(&destination), fsx::Existing::Nothing);

    let knows = Arc::new(Knows("open sesame", AtomicUsize::new(0)));
    let mut ctx = OpContext::silent();
    ctx.interaction = knows.clone();
    open(&path)
        .extract(
            &destination,
            Selection::All,
            ExtractOptions::default(),
            &ctx,
        )
        .unwrap();
    assert_eq!(
        tree(&destination.join("photos")),
        tree(&root.path().join("photos"))
    );
    assert_eq!(knows.1.load(AtomicOrdering::SeqCst), 1);
}

/// RAR is read by a library that takes an archive by the name of its file
/// and hands the content over through a callback: here it goes the whole
/// way, from a file on the disk to files on the disk.
#[cfg(feature = "rar")]
#[test]
fn rar_goes_through_its_library_to_the_disk() {
    let root = TempDir::new().unwrap();
    let silent = OpContext::silent();

    // Names outside ASCII: of the archive, of the folder it is in, and in
    // the archive.
    let folder = root.path().join("архивы ❤");
    fs::create_dir(&folder).unwrap();
    let path = folder.join("файл.rar");
    fs::write(
        &path,
        include_bytes!("../../../boxit-codecs/fixtures/rar5-unicode.rar"),
    )
    .unwrap();
    let mut archive = open(&path);
    assert_eq!(archive.format(), ArchiveFormat::Rar);
    let listing = archive.list(&silent).unwrap();
    assert_eq!((listing.files, listing.folders), (1, 0));
    assert!(!listing.encrypted);
    let tested = archive
        .test(&Selection::All, TestOptions::default(), &silent)
        .unwrap();
    assert_eq!((tested.files, tested.bytes), (1, 7));
    let destination = root.path().join("out");
    archive
        .extract(
            &destination,
            Selection::All,
            ExtractOptions::default(),
            &silent,
        )
        .unwrap();
    let unpacked = tree(&destination);
    assert_eq!(
        unpacked.into_iter().collect::<Vec<_>>(),
        [(
            String::from("unicodefilename❤\u{fe0f}.txt"),
            String::from("foobar\n")
        )]
    );

    // The names and the comment behind a password: asked for once, at the
    // list, and known from then on.
    let path = root.path().join("secret.rar");
    fs::write(
        &path,
        include_bytes!("../../../boxit-codecs/fixtures/rar5-secret.rar"),
    )
    .unwrap();
    let error = open(&path).list(&silent).unwrap_err();
    assert_eq!(error.code, "passwordRequired");
    let knows = Arc::new(Knows("password", AtomicUsize::new(0)));
    let mut ctx = OpContext::silent();
    ctx.interaction = knows.clone();
    let mut archive = open(&path);
    let listing = archive.list(&ctx).unwrap();
    assert!(listing.encrypted);
    assert_eq!(listing.comment.as_deref(), Some("abcdef12345\n"));
    let destination = root.path().join("secret");
    archive
        .extract(
            &destination,
            Selection::All,
            ExtractOptions::default(),
            &ctx,
        )
        .unwrap();
    assert_eq!(
        tree(&destination).into_iter().collect::<Vec<_>>(),
        [(
            String::from(".gitignore"),
            String::from("target\nCargo.lock\n")
        )]
    );
    assert_eq!(knows.1.load(AtomicOrdering::SeqCst), 1);

    // A wrong password of an old archive shows when the content does not
    // add up: nothing is left where the file would have gone.
    let path = root.path().join("locked.rar");
    fs::write(
        &path,
        include_bytes!("../../../boxit-codecs/fixtures/rar4-locked.rar"),
    )
    .unwrap();
    let wrong = OpenOptions {
        password: Some(Password::from("wrong")),
        ..OpenOptions::default()
    };
    let destination = root.path().join("locked");
    let error = Archive::open(&path, wrong)
        .unwrap()
        .extract(
            &destination,
            Selection::All,
            ExtractOptions::default(),
            &silent,
        )
        .unwrap_err();
    assert_eq!(error.code, "damagedOrWrongPassword");
    assert_eq!(fsx::existing(&destination), fsx::Existing::Nothing);

    // The first volume of several, without the others: the one that is
    // looked for next is named.
    let path = root.path().join("volume.part1.rar");
    fs::write(
        &path,
        include_bytes!("../../../boxit-codecs/fixtures/rar4-volume.rar"),
    )
    .unwrap();
    let error = open(&path).list(&silent).unwrap_err();
    assert_eq!(error.code, "volumeMissing");
    assert_eq!(error.params["name"], "volume.part2.rar");
}

#[test]
fn hidden_names_need_the_password_for_the_list() {
    let root = TempDir::new().unwrap();
    let locked = CreateOptions {
        password: Some(Password::from("open sesame")),
        encrypt_names: true,
        ..options(ArchiveFormat::SevenZ)
    };
    let path = packed(root.path(), "locked.7z", locked);
    let error = open(&path).list(&OpContext::silent()).unwrap_err();
    assert_eq!(error.code, "passwordRequired");

    let known = OpenOptions {
        password: Some(Password::from("open sesame")),
        ..OpenOptions::default()
    };
    let mut archive = Archive::open(&path, known).unwrap();
    let listing = archive.list(&OpContext::silent()).unwrap();
    assert_eq!(listing.files, 3);
    assert!(listing.encrypted);
    let tested = archive
        .test(
            &Selection::All,
            TestOptions::default(),
            &OpContext::silent(),
        )
        .unwrap();
    assert_eq!(tested.files, 3);
}

#[test]
fn a_test_reads_everything_and_writes_nothing() {
    let root = TempDir::new().unwrap();
    for (format, name) in [
        (ArchiveFormat::Zip, "t.zip"),
        (ArchiveFormat::SevenZ, "t.7z"),
        (ArchiveFormat::TarXz, "t.tar.xz"),
    ] {
        let path = packed(root.path(), name, options(format));
        let before: Vec<_> = tree(root.path()).into_keys().collect();
        let watch = Arc::new(Watch::default());
        let mut ctx = OpContext::silent();
        ctx.progress = watch.clone();
        let mut archive = open(&path);
        let listing = archive.list(&ctx).unwrap();
        let tested = archive
            .test(&Selection::All, TestOptions::default(), &ctx)
            .unwrap();
        assert_eq!(tested.files, 3, "{name}");
        assert_eq!(tested.bytes, listing.bytes, "{name}");
        assert_eq!(watch.advanced.load(AtomicOrdering::SeqCst), listing.bytes);
        if format != ArchiveFormat::TarXz {
            // An archive with a list knows the amount of work beforehand.
            assert_eq!(watch.total.load(AtomicOrdering::SeqCst), listing.bytes);
        }
        assert_eq!(tree(root.path()).into_keys().collect::<Vec<_>>(), before);

        // A part of the archive alone.
        let part = Selection::paths(["photos/2024"]);
        let tested = archive
            .test(&part, TestOptions::default(), &OpContext::silent())
            .unwrap();
        assert_eq!((tested.files, tested.bytes), (2, 9), "{name}");
        assert_eq!((tested.failed_total, tested.complete), (0, true), "{name}");
        assert_eq!(tested.sums, None, "{name}");
    }
}

#[test]
fn a_test_finds_the_damage() {
    let root = TempDir::new().unwrap();
    for (format, name) in [
        (ArchiveFormat::Zip, "d.zip"),
        (ArchiveFormat::SevenZ, "d.7z"),
        (ArchiveFormat::TarGz, "d.tar.gz"),
        (ArchiveFormat::TarZst, "d.tar.zst"),
    ] {
        // Stored as it is, the content of a file can be found and changed;
        // a compressed stream has its middle in the compressed data.
        let stored = CreateOptions {
            level: Level::Store,
            ..CreateOptions::new(format)
        };
        let path = packed(root.path(), name, stored);
        let mut bytes = fs::read(&path).unwrap();
        let place = if format.is_tar() {
            bytes.len() / 2
        } else {
            bytes
                .windows(8)
                .position(|window| window == b"7ff\n800\n")
                .unwrap()
        };
        bytes[place] ^= 0x01;
        fs::write(&path, &bytes).unwrap();
        let result = open(&path).test(
            &Selection::All,
            TestOptions::default(),
            &OpContext::silent(),
        );
        match result {
            // An archive with a list names the file and reads the others.
            Ok(tested) => {
                assert!(!format.is_tar(), "{name}");
                assert_eq!(tested.failed_total, 1, "{name}");
                assert_eq!(tested.failed[0].path, "photos/2025/c.txt", "{name}");
                assert_eq!(tested.failed[0].reason.code, "damagedEntry", "{name}");
                if tested.complete {
                    assert_eq!((tested.files, tested.bytes), (2, 9), "{name}");
                }
            }
            Err(error) => assert!(
                ["damaged", "damagedEntry"].contains(&error.code.as_str()),
                "{name}: {error}"
            ),
        }
        // An extraction of the same archive brings nothing.
        let destination = root.path().join("out");
        let result = open(&path).extract(
            &destination,
            Selection::All,
            ExtractOptions::default(),
            &OpContext::silent(),
        );
        assert!(result.is_err(), "{name}");
        assert_eq!(
            fsx::existing(&destination),
            fsx::Existing::Nothing,
            "{name}"
        );
    }
}

#[test]
fn every_damaged_file_is_named_and_the_rest_are_read() {
    let root = TempDir::new().unwrap();
    let noise = lines(4000, "");
    plant(
        &root.path().join("set"),
        &[
            ("one.txt", &noise),
            ("two.txt", "two is whole"),
            ("three.txt", &noise.replace("7ff", "7FF")),
            ("four.txt", "four is whole as well"),
        ],
    );
    let path = root.path().join("set.zip");
    let request = CreateRequest::new(
        vec![root.path().join("set")],
        path.clone(),
        CreateOptions {
            level: Level::Store,
            ..CreateOptions::new(ArchiveFormat::Zip)
        },
    );
    create(&request, &OpContext::silent()).unwrap();
    let mut bytes = fs::read(&path).unwrap();
    for mark in [&b"7ff\n800\n"[..], &b"7FF\n800\n"[..]] {
        let place = bytes.windows(8).position(|window| window == mark).unwrap();
        bytes[place + 4] ^= 0x01;
    }
    fs::write(&path, &bytes).unwrap();

    for sums in [false, true] {
        let tested = open(&path)
            .test(&Selection::All, TestOptions { sums }, &OpContext::silent())
            .unwrap();
        assert_eq!(tested.failed_total, 2);
        let mut failed: Vec<&str> = tested
            .failed
            .iter()
            .map(|failed| failed.path.as_str())
            .collect();
        failed.sort_unstable();
        assert_eq!(failed, ["set/one.txt", "set/three.txt"]);
        assert!(tested.complete);
        assert_eq!(tested.files, 2);
        assert_eq!(tested.bytes, 12 + 21);
        // The sums are of the files that are whole.
        let named: Option<Vec<&str>> = tested
            .sums
            .as_ref()
            .map(|sums| sums.iter().map(|sum| sum.path.as_str()).collect());
        assert_eq!(named, sums.then(|| vec!["set/four.txt", "set/two.txt"]));
    }
    // One of the two alone.
    let tested = open(&path)
        .test(
            &Selection::paths(["set/three.txt", "set/two.txt"]),
            TestOptions::default(),
            &OpContext::silent(),
        )
        .unwrap();
    assert_eq!((tested.files, tested.failed_total), (1, 1));
}

#[test]
fn the_sums_of_the_files_are_worked_out_without_writing_them() {
    let root = TempDir::new().unwrap();
    let noise = lines(4000, "");
    let sha = |bytes: &[u8]| hex(&Sha256::digest(bytes));
    for (format, name) in [
        (ArchiveFormat::Zip, "s.zip"),
        (ArchiveFormat::SevenZ, "s.7z"),
        (ArchiveFormat::TarZst, "s.tar.zst"),
        (ArchiveFormat::Tar, "s.tar"),
    ] {
        let path = packed(root.path(), name, options(format));
        let before: Vec<_> = tree(root.path()).into_keys().collect();
        let tested = open(&path)
            .test(
                &Selection::All,
                TestOptions { sums: true },
                &OpContext::silent(),
            )
            .unwrap();
        assert_eq!(tree(root.path()).into_keys().collect::<Vec<_>>(), before);
        assert_eq!(tested.files, 3, "{name}");
        assert_eq!(
            tested.archive_sha256,
            Some(sha(&fs::read(&path).unwrap())),
            "{name}"
        );
        let sums = tested.sums.unwrap();
        let wanted: Vec<Sum> = [
            ("photos/2024/a.txt", "alpha"),
            ("photos/2024/b.txt", "beta"),
            ("photos/2025/c.txt", noise.as_str()),
        ]
        .into_iter()
        .map(|(path, content)| Sum {
            path: path.to_owned(),
            size: content.len() as u64,
            crc32: crc32fast::hash(content.as_bytes()),
            sha256: sha(content.as_bytes()),
        })
        .collect();
        assert_eq!(sums, wanted, "{name}");
    }
    // What everybody knows the sum of.
    assert_eq!(
        sha(b"abc"),
        "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"
    );

    // The sum of an archive in volumes is the sum of the archive whole.
    let whole = packed(root.path(), "v.zip", options(ArchiveFormat::Zip));
    let mut request = CreateRequest::new(
        vec![root.path().join("photos")],
        root.path().join("cut").join("v.zip"),
        CreateOptions {
            level: Level::Store,
            ..CreateOptions::new(ArchiveFormat::Zip)
        },
    );
    request.layout = Layout::Volumes(boxit_codecs::volumes::SMALLEST);
    let created = create(&request, &OpContext::silent()).unwrap();
    let tested = open(Path::new(&created.path))
        .test(
            &Selection::All,
            TestOptions { sums: true },
            &OpContext::silent(),
        )
        .unwrap();
    let mut joined = Vec::new();
    for number in 1..=created.volumes {
        let volume = boxit_codecs::volumes::path(&root.path().join("cut").join("v.zip"), number);
        joined.extend(fs::read(volume).unwrap());
    }
    assert_eq!(tested.archive_sha256, Some(sha(&joined)));
    assert!(whole.is_file());
}

#[test]
fn a_selection_is_extracted_alone() {
    let root = TempDir::new().unwrap();
    let path = packed(root.path(), "s.7z", options(ArchiveFormat::SevenZ));
    let destination = root.path().join("out");
    let outcome = open(&path)
        .extract(
            &destination,
            Selection::paths(["photos/2024/b.txt", "photos/empty"]),
            ExtractOptions::default(),
            &OpContext::silent(),
        )
        .unwrap();
    assert_eq!(outcome.files, 1);
    assert_eq!(
        tree(&destination).into_keys().collect::<Vec<_>>(),
        [
            "photos/",
            "photos/2024/",
            "photos/2024/b.txt",
            "photos/empty/"
        ]
    );
}

#[test]
fn a_selection_that_picks_nothing_is_an_error() {
    let root = TempDir::new().unwrap();
    let silent = OpContext::silent();
    for (format, name) in [
        (ArchiveFormat::Zip, "n.zip"),
        (ArchiveFormat::SevenZ, "n.7z"),
        (ArchiveFormat::TarGz, "n.tar.gz"),
    ] {
        let path = packed(root.path(), name, options(format));
        let destination = root.path().join(format!("{name}.out"));
        for nothing in [
            Selection::paths(["photos/2026", "photos/2024/c.txt"]),
            Selection::indexes(vec![700]),
            Selection::indexes(Vec::new()),
        ] {
            let error = open(&path)
                .extract(
                    &destination,
                    nothing.clone(),
                    ExtractOptions::default(),
                    &silent,
                )
                .unwrap_err();
            assert_eq!(error.code, "nothingMatched", "{name}");
            // Not even an empty folder is left for it.
            assert_eq!(
                fsx::existing(&destination),
                fsx::Existing::Nothing,
                "{name}"
            );
            let error = open(&path)
                .test(&nothing, TestOptions::default(), &silent)
                .unwrap_err();
            assert_eq!(error.code, "nothingMatched", "{name}");
        }
        let error = open(&path)
            .test(
                &Selection::paths(["photos/2026"]),
                TestOptions::default(),
                &silent,
            )
            .unwrap_err();
        assert_eq!(error.params["name"], "photos/2026");
        // A folder without files is something.
        let empty = Selection::paths(["photos/empty"]);
        assert_eq!(
            open(&path)
                .test(&empty, TestOptions::default(), &silent)
                .unwrap(),
            Tested {
                complete: true,
                ..Tested::default()
            }
        );
    }
}

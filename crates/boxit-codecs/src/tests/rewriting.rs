//! Archives written anew: with changes, and from what is left of a
//! damaged one.

use std::fs::{self, File};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::atomic::Ordering;

use boxit_core::{
    ArchiveFormat, Changes, CreateOptions, Entry, Item, OpContext, Password, Result, Selection,
};
use tempfile::TempDir;
use zip::{CompressionMethod, ZipWriter};

use super::{
    answering, assert_same, context, fast, file, lines, pack, pattern, small_tree, tree, unpack,
    unpack_all, Collector, Tree, Unpacked,
};
use crate::zip_archive::entry_options;
use crate::{can_repair, open, salvage, OpenOptions};

fn index_of(entries: &[Entry], path: &str) -> u64 {
    entries
        .iter()
        .find(|entry| entry.meta.path.trim_end_matches('/') == path)
        .unwrap_or_else(|| panic!("{path} is not in the archive"))
        .index
}

fn known(password: Option<&str>) -> OpenOptions {
    OpenOptions {
        password: password.map(Password::from),
        ..OpenOptions::default()
    }
}

/// Writes the archive at `path` anew with the changes `change` makes.
fn rewritten(
    path: &Path,
    options: OpenOptions,
    change: impl FnOnce(&[Entry], &mut Changes),
    ctx: &OpContext,
) -> Result<PathBuf> {
    let mut opened = open(path, options)?;
    let entries = opened.reader.entries(ctx)?;
    let mut changes = Changes::none(opened.format);
    changes.options = fast(opened.format);
    change(&entries, &mut changes);
    let target = path.with_extension(format!("new.{}", opened.format.id()));
    let mut output = File::create(&target)?;
    opened.reader.rewrite(changes, &mut output, ctx)?;
    output.flush()?;
    Ok(target)
}

#[test]
fn an_archive_is_written_anew_with_its_changes() {
    let folder_of_all = TempDir::new().unwrap();
    let tree = tree();
    let ctx = context(4);
    for (format, solid, password, encrypt_names) in [
        (ArchiveFormat::Zip, false, None, false),
        (ArchiveFormat::Zip, false, Some("correct horse"), false),
        (ArchiveFormat::SevenZ, false, None, false),
        (ArchiveFormat::SevenZ, true, None, false),
        (ArchiveFormat::SevenZ, true, Some("correct horse"), false),
        (ArchiveFormat::SevenZ, true, Some("correct horse"), true),
    ] {
        let label = format!(
            "{} solid {solid} locked {} names {encrypt_names}",
            format.id(),
            password.is_some()
        );
        let options = CreateOptions {
            solid,
            password: password.map(Password::from),
            encrypt_names,
            comment: (format == ArchiveFormat::Zip).then(|| String::from("kept as it is")),
            ..fast(format)
        };
        let path = pack(&folder_of_all, &options, &tree, &ctx).expect(&label);

        // Nothing changes: the archive is the archive.
        let same = rewritten(&path, known(password), |_, _| {}, &ctx).expect(&label);
        let unpacked = unpack(&same, known(password), &Selection::All, &ctx).expect(&label);
        assert_same(&tree, &unpacked, &label);

        // A file and a folder leave, a file and a folder get other names,
        // and a file comes.
        let added = file("docs/new.txt", lines(500), 50);
        let target = rewritten(
            &path,
            known(password),
            |entries, changes| {
                changes.removed.insert(index_of(entries, "docs/readme.txt"));
                changes.removed.insert(index_of(entries, "docs"));
                changes.renamed.insert(
                    index_of(entries, "docs/notes.md"),
                    String::from("docs/remarks.md"),
                );
                changes
                    .renamed
                    .insert(index_of(entries, "empty folder"), String::from("hollow"));
                changes
                    .added
                    .push(Item::new(added.0.clone(), added.1.clone()));
            },
            &ctx,
        )
        .expect(&label);
        let mut expected: Tree = tree
            .iter()
            .filter(|(meta, _)| !["docs", "docs/readme.txt"].contains(&meta.path.as_str()))
            .cloned()
            .collect();
        for (meta, _) in &mut expected {
            match meta.path.as_str() {
                "docs/notes.md" => meta.path = String::from("docs/remarks.md"),
                "empty folder" => meta.path = String::from("hollow"),
                _ => {}
            }
        }
        expected.push(added);
        let unpacked = unpack(&target, known(password), &Selection::All, &ctx).expect(&label);
        assert_same(&expected, &unpacked, &label);

        // What was locked stays locked, and what came is locked with it.
        if password.is_some() {
            let open_content = unpacked
                .listed
                .iter()
                .filter(|entry| entry.meta.size.unwrap_or(0) > 0 && !entry.encrypted)
                .count();
            assert_eq!(open_content, 0, "{label}");
            let listed = open(&target, OpenOptions::default())
                .and_then(|mut opened| opened.reader.entries(&ctx));
            assert_eq!(listed.is_err(), encrypt_names, "{label}");
        }
        if format == ArchiveFormat::Zip {
            let mut opened = open(&target, known(password)).unwrap();
            assert_eq!(
                opened.reader.comment(&ctx).unwrap().as_deref(),
                Some("kept as it is"),
                "{label}"
            );
            // A copy is a copy: not a byte of a file was compressed again.
            let before = unpack(&path, known(password), &Selection::All, &ctx).unwrap();
            for entry in &unpacked.listed {
                let path = entry.meta.path.as_str();
                if let Some(old) = before.listed.iter().find(|old| old.meta.path == path) {
                    assert_eq!(entry.packed_size, old.packed_size, "{label}: {path}");
                    assert_eq!(entry.crc32, old.crc32, "{label}: {path}");
                }
            }
        }
    }
}

#[test]
fn a_format_that_is_not_written_anew_says_so() {
    let folder = TempDir::new().unwrap();
    let ctx = context(1);
    let path = pack(&folder, &fast(ArchiveFormat::TarGz), &small_tree(), &ctx).unwrap();
    let error = rewritten(&path, OpenOptions::default(), |_, _| {}, &ctx).unwrap_err();
    assert_eq!(error.code, "cannotModify");
    assert!(can_repair(ArchiveFormat::Zip) && can_repair(ArchiveFormat::SevenZ));
    assert!(!can_repair(ArchiveFormat::TarGz) && !can_repair(ArchiveFormat::Rar));
    let error = salvage(&path, OpenOptions::default(), &ctx).err().unwrap();
    assert_eq!(error.code, "cannotRepair");
    assert_eq!(error.params["format"], "tar.gz");
}

#[test]
fn a_cancelled_rewriting_stops() {
    let folder = TempDir::new().unwrap();
    let tree = tree();
    for format in [ArchiveFormat::Zip, ArchiveFormat::SevenZ] {
        let path = pack(&folder, &fast(format), &tree, &context(2)).unwrap();
        let ctx = context(2);
        ctx.control.cancel();
        let error = rewritten(&path, OpenOptions::default(), |_, _| {}, &ctx).unwrap_err();
        assert!(error.is_cancelled(), "{}: {error}", format.id());
    }
}

#[test]
fn the_files_of_a_block_are_tied_to_the_ones_before_them() {
    let folder = TempDir::new().unwrap();
    let ctx = context(2);
    let tree = small_tree();
    for (solid, tied) in [(true, true), (false, false)] {
        let options = CreateOptions {
            solid,
            ..fast(ArchiveFormat::SevenZ)
        };
        let path = pack(&folder, &options, &tree, &ctx).unwrap();
        let mut opened = open(&path, OpenOptions::default()).unwrap();
        let entries = opened.reader.entries(&ctx).unwrap();
        let with_data: Vec<u64> = entries
            .iter()
            .filter(|entry| entry.meta.size.unwrap_or(0) > 0)
            .map(|entry| entry.index)
            .collect();
        let after_first = opened.reader.tied(with_data[0], &ctx).unwrap();
        if tied {
            assert_eq!(after_first, with_data[1..], "solid");
        } else {
            assert!(after_first.is_empty(), "apart");
        }
        // A folder is tied to nothing, and nothing to the last file.
        let folder_index = index_of(&entries, "empty folder");
        assert!(opened.reader.tied(folder_index, &ctx).unwrap().is_empty());
        let last = *with_data.last().unwrap();
        assert!(opened.reader.tied(last, &ctx).unwrap().is_empty());
    }
    // The entries of a ZIP are apart from each other.
    let path = pack(&folder, &fast(ArchiveFormat::Zip), &tree, &ctx).unwrap();
    let mut opened = open(&path, OpenOptions::default()).unwrap();
    assert!(opened.reader.tied(0, &ctx).unwrap().is_empty());
}

/// Where the list of a ZIP starts: its last record tells.
fn list_start(zip: &[u8]) -> usize {
    let end = zip
        .windows(4)
        .rposition(|window| window == b"PK\x05\x06")
        .unwrap();
    u32::from_le_bytes(zip[end + 16..end + 20].try_into().unwrap()) as usize
}

/// A ZIP written the way a program that cannot go back writes it: the
/// sizes of every entry follow its data.
fn streamed_zip(tree: &Tree) -> Vec<u8> {
    let mut zip = ZipWriter::new_stream(Vec::new());
    for (index, (meta, data)) in tree.iter().enumerate() {
        let method = if index % 2 == 0 {
            CompressionMethod::Deflated
        } else {
            CompressionMethod::Stored
        };
        let options = entry_options(meta, (method, None), None).unwrap();
        if meta.is_dir() {
            zip.add_directory(meta.path.as_str(), options).unwrap();
        } else {
            zip.start_file(meta.path.as_str(), options).unwrap();
            zip.write_all(data).unwrap();
        }
    }
    zip.finish().unwrap().into_inner()
}

#[test]
fn a_zip_without_its_list_is_read_by_the_headers_of_its_entries() {
    let folder_of_all = TempDir::new().unwrap();
    let ctx = context(4);
    let tree = tree();

    let whole = pack(&folder_of_all, &fast(ArchiveFormat::Zip), &tree, &ctx).unwrap();
    let locked = pack(
        &folder_of_all,
        &CreateOptions {
            password: Some(Password::from("correct horse")),
            ..fast(ArchiveFormat::Zip)
        },
        &tree,
        &ctx,
    )
    .unwrap();
    // An archive inside an archive, stored as it is: its entries are the
    // data of an entry, and no entries of the outer one.
    let inner = fs::read(&whole).unwrap();
    let mut nested = small_tree();
    nested.insert(2, file("inner/archive.zip", inner, 30));
    nested.push(file("after.bin", pattern(70_000), 31));
    let cases: Vec<(&str, Vec<u8>, &Tree, Option<&str>)> = vec![
        ("written at once", fs::read(&whole).unwrap(), &tree, None),
        (
            "locked",
            fs::read(&locked).unwrap(),
            &tree,
            Some("correct horse"),
        ),
        ("streamed", streamed_zip(&tree), &tree, None),
        ("streamed, nested", streamed_zip(&nested), &nested, None),
    ];
    for (label, good, tree, password) in cases {
        let path = folder_of_all.path().join("broken.zip");
        // The archive as it is: nothing is made anew.
        fs::write(&path, &good).unwrap();
        let salvaged = salvage(&path, known(password), &ctx).expect(label);
        assert!(!salvaged.rebuilt && salvaged.lost.is_empty(), "{label}");
        drop(salvaged);

        // The list is gone, to its last byte and to its first.
        let start = list_start(&good);
        for cut in [good.len() - 10, start + 7, start] {
            fs::write(&path, &good[..cut]).unwrap();
            assert!(unpack_all(&path, &ctx).is_err(), "{label}: cut at {cut}");
            let mut salvaged = salvage(&path, known(password), &ctx).expect(label);
            assert!(salvaged.rebuilt, "{label}: cut at {cut}");
            assert!(salvaged.lost.is_empty(), "{label}: {:?}", salvaged.lost);
            assert_eq!(salvaged.opened.format, ArchiveFormat::Zip);
            let listed = salvaged.opened.reader.entries(&ctx).expect(label);
            assert_eq!(listed.len(), tree.len(), "{label}: cut at {cut}");
            let sink = Collector::default();
            salvaged
                .opened
                .reader
                .extract(&Selection::All, &sink, &ctx)
                .expect(label);
            let unpacked = Unpacked {
                format: ArchiveFormat::Zip,
                listed,
                got: sink.by_path(),
            };
            assert_same(tree, &unpacked, label);

            // And it is written as an archive any program opens.
            let mut output = File::create(folder_of_all.path().join("mended.zip")).unwrap();
            salvaged
                .opened
                .reader
                .rewrite(Changes::none(ArchiveFormat::Zip), &mut output, &ctx)
                .expect(label);
            // The files are let go: the next ones take their names.
            drop((output, salvaged));
            let mended = folder_of_all.path().join("mended.zip");
            let unpacked = unpack(&mended, known(password), &Selection::All, &ctx).expect(label);
            assert_same(tree, &unpacked, label);
        }

        // The archive ends in the middle of its last file: the file is
        // named as lost, and the ones before it are there.
        let (last, data) = tree.last().unwrap();
        assert!(last.is_file() && !data.is_empty(), "{label}");
        fs::write(&path, &good[..start - 1]).unwrap();
        let mut salvaged = salvage(&path, known(password), &ctx).expect(label);
        assert!(salvaged.rebuilt, "{label}");
        assert_eq!(salvaged.lost, [last.path.clone()], "{label}");
        let listed = salvaged.opened.reader.entries(&ctx).expect(label);
        assert_eq!(listed.len(), tree.len() - 1, "{label}");
        assert!(
            listed.iter().all(|entry| entry.meta.path != last.path),
            "{label}"
        );
    }

    // Nothing of an archive: there is nothing to make a list of.
    let path = folder_of_all.path().join("nothing.zip");
    fs::write(&path, pattern(5000)).unwrap();
    let error = salvage(&path, OpenOptions::default(), &ctx).err().unwrap();
    assert!(["damaged", "truncated"].contains(&error.code.as_str()));
}

#[test]
fn a_password_nobody_gives_does_not_stop_the_search_for_entries() {
    let folder = TempDir::new().unwrap();
    let ctx = context(2);
    let options = CreateOptions {
        password: Some(Password::from("correct horse")),
        ..fast(ArchiveFormat::Zip)
    };
    let path = pack(&folder, &options, &small_tree(), &ctx).unwrap();
    let good = fs::read(&path).unwrap();
    fs::write(&path, &good[..list_start(&good)]).unwrap();
    // The list is made without the password; the content waits for it.
    let (ctx, answers) = answering(vec!["correct horse"]);
    let mut salvaged = salvage(&path, OpenOptions::default(), &ctx).unwrap();
    assert_eq!(answers.asked.load(Ordering::SeqCst), 0);
    let listed = salvaged.opened.reader.entries(&ctx).unwrap();
    assert_eq!(listed.len(), small_tree().len());
    assert!(listed
        .iter()
        .all(|entry| entry.encrypted || entry.meta.size.unwrap_or(0) == 0));
    let sink = Collector::default();
    salvaged
        .opened
        .reader
        .extract(&Selection::All, &sink, &ctx)
        .unwrap();
    assert_eq!(answers.asked.load(Ordering::SeqCst), 1);
}

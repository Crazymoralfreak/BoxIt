//! The formats as a user meets them: what one writes, the reader gives back,
//! and what is broken is said to be broken.

use std::collections::BTreeMap;
use std::fmt::Write as _;
use std::fs::{self, File};
use std::io::{self, BufWriter, Read};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

use boxit_core::{
    ArchiveFormat, Budget, CreateOptions, Entry, EntryKind, EntryMeta, EntrySink, Error,
    Interaction, Item, Level, Method, OpContext, Password, PasswordRequest, Result, Selection,
    Timestamp,
};
use proptest::prelude::*;
use tempfile::TempDir;

use crate::ar::tests::gnu;
use crate::cab::tests::{cabinet, item, Group, MSZIP, STORED};
use crate::corpus::{Scale, Set};
use crate::cpio::tests::{file as cpio_file, newc};
use crate::lha::tests::{archive as lha, level0};
use crate::names::NameEncoding;
use crate::source::Source;
use crate::wim::tests::{file as wim_file, folder as wim_folder, Builder};
use crate::{can_encrypt, create, methods, open, open_source, reader, OpenOptions};

mod rewriting;

const CONTAINERS: [ArchiveFormat; 8] = [
    ArchiveFormat::Zip,
    ArchiveFormat::SevenZ,
    ArchiveFormat::Tar,
    ArchiveFormat::TarGz,
    ArchiveFormat::TarBz2,
    ArchiveFormat::TarXz,
    ArchiveFormat::TarZst,
    ArchiveFormat::TarLz4,
];

const SINGLES: [ArchiveFormat; 5] = [
    ArchiveFormat::Gz,
    ArchiveFormat::Bz2,
    ArchiveFormat::Xz,
    ArchiveFormat::Zst,
    ArchiveFormat::Lz4,
];

/// The most a test reads of one entry: a damaged archive may promise more
/// than any memory holds.
const READ_LIMIT: u64 = 64 * 1024 * 1024;

type Tree = Vec<(EntryMeta, Vec<u8>)>;

#[derive(Debug)]
pub(crate) struct Got {
    pub(crate) entry: Entry,
    pub(crate) data: Vec<u8>,
}

/// Keeps what an archive hands over.
#[derive(Default)]
pub(crate) struct Collector(Mutex<Vec<Got>>);

impl EntrySink for Collector {
    fn accept(&self, entry: &Entry, data: &mut dyn Read) -> Result<()> {
        let mut bytes = Vec::new();
        data.take(READ_LIMIT).read_to_end(&mut bytes)?;
        self.0.lock().unwrap().push(Got {
            entry: entry.clone(),
            data: bytes,
        });
        Ok(())
    }
}

impl Collector {
    /// What arrived, in the order it did.
    pub(crate) fn in_order(self) -> Vec<Got> {
        self.0.into_inner().unwrap()
    }

    /// What arrived, by path; folders without the slash some formats end
    /// their names with.
    fn by_path(self) -> BTreeMap<String, Got> {
        self.0
            .into_inner()
            .unwrap()
            .into_iter()
            .map(|got| (got.entry.meta.path.trim_end_matches('/').to_owned(), got))
            .collect()
    }
}

/// The bytes of the larger files of the fixtures, as the script that makes
/// the fixtures writes them.
pub(crate) fn pattern(size: usize) -> Vec<u8> {
    (0..size)
        .map(|at| ((at * 7 + (at >> 8) * 13) % 251) as u8)
        .collect()
}

/// The text of the fixtures, as the script that makes them writes it.
pub(crate) fn lines(count: u64) -> Vec<u8> {
    let mut text = String::new();
    for line in 0..count {
        let number = line * 2_654_435_761 % 1_000_003;
        writeln!(text, "{number:x} is the number of a line of this text").unwrap();
    }
    text.into_bytes()
}

/// Numbers that grow evenly, four bytes each, as the script that makes the
/// fixtures writes them.
pub(crate) fn table(count: u32) -> Vec<u8> {
    (0..count)
        .flat_map(|at| (1000 + 12 * at).to_le_bytes())
        .collect()
}

/// What the script that makes the fixtures writes for the code of a
/// program: instructions that name a place by its distance.
pub(crate) fn code(size: usize) -> Vec<u8> {
    let mut bytes = Vec::with_capacity(size + 32);
    let mut state: u64 = 12345;
    while bytes.len() < size {
        state = (state * 1_103_515_245 + 12345) & 0x7fff_ffff;
        let filler = 3 + state % 13;
        bytes.extend((0..filler).map(|_| (0x50 + (state >> 8) % 8) as u8));
        let target = 0x1000 * (1 + (state >> 20) % 40) as i32;
        let at = bytes.len() as i32;
        let instruction: &[u8] = match (state >> 16) % 7 {
            0 => &[0xE8],
            1 => &[0x48, 0x8B, 0x05],
            2 => &[0x48, 0x8D, 0x0D],
            3 => &[0x4C, 0x8D, 0x05],
            4 => &[0xFF, 0x15],
            5 => &[0xF0, 0x83, 0x05],
            _ => &[0xE9],
        };
        bytes.extend_from_slice(instruction);
        bytes.extend_from_slice(&(target - at).to_le_bytes());
    }
    bytes.truncate(size);
    bytes
}

/// A fixture that is kept compressed.
pub(crate) fn gunzip(bytes: &[u8]) -> Vec<u8> {
    let mut plain = Vec::new();
    flate2::read::GzDecoder::new(bytes)
        .read_to_end(&mut plain)
        .unwrap();
    plain
}

/// Answers the questions about a password from a list, and counts them.
pub(crate) struct Answers {
    pub(crate) asked: AtomicUsize,
    pub(crate) retries: AtomicUsize,
    given: Vec<&'static str>,
}

impl Interaction for Answers {
    fn password(&self, request: &PasswordRequest) -> Option<Password> {
        if request.retry {
            self.retries.fetch_add(1, Ordering::SeqCst);
        }
        let asked = self.asked.fetch_add(1, Ordering::SeqCst);
        self.given.get(asked).copied().map(Password::from)
    }
}

fn context(threads: usize) -> OpContext {
    OpContext {
        budget: Budget::with_threads(threads),
        ..OpContext::silent()
    }
}

pub(crate) fn answering(given: Vec<&'static str>) -> (OpContext, Arc<Answers>) {
    let answers = Arc::new(Answers {
        asked: AtomicUsize::new(0),
        retries: AtomicUsize::new(0),
        given,
    });
    let ctx = OpContext {
        interaction: answers.clone(),
        ..context(4)
    };
    (ctx, answers)
}

fn stamp(offset: i64) -> Timestamp {
    Timestamp {
        seconds: 1_700_000_000 + offset * 3,
        nanos: 123_456_700,
    }
}

fn file(path: &str, data: Vec<u8>, offset: i64) -> (EntryMeta, Vec<u8>) {
    let mut meta = EntryMeta::file(path, data.len() as u64);
    meta.modified = Some(stamp(offset));
    (meta, data)
}

fn folder(path: &str, offset: i64) -> (EntryMeta, Vec<u8>) {
    let mut meta = EntryMeta::directory(path);
    meta.modified = Some(stamp(offset));
    (meta, Vec::new())
}

/// A few files of every shape that matters: a folder with nothing in it, an
/// empty file, names outside ASCII, a path longer than a tar header holds.
fn small_tree() -> Tree {
    let deep = format!("{}/file.txt", ["a-rather-long-folder-name"; 6].join("/"));
    vec![
        folder("docs", 0),
        file(
            "docs/readme.txt",
            b"BoxIt packs and unpacks.\n".repeat(200),
            1,
        ),
        file(
            "docs/пример текста.txt",
            "Привет, архив!".as_bytes().to_vec(),
            2,
        ),
        file("empty.bin", Vec::new(), 3),
        folder("empty folder", 4),
        file(&deep, b"deep".to_vec(), 5),
        file("docs/notes.md", b"# Notes\n".to_vec(), 6),
    ]
}

/// The small tree and a part of the corpus: files of every kind.
fn tree() -> Tree {
    let mut tree = small_tree();
    for (index, spec) in Set::Mixed.files(Scale::Small).iter().take(30).enumerate() {
        tree.push(file(
            &format!("corpus/{}", spec.path),
            spec.bytes(),
            10 + index as i64,
        ));
    }
    tree
}

/// A tree with a file too large to be compressed in memory next to others.
fn large_tree() -> Tree {
    let text = Set::OneLarge.files(Scale::Small)[0].bytes();
    let repeats = 9 * 1024 * 1024 / text.len() + 1;
    let mut tree = tree();
    tree.insert(3, file("large/big.txt", text.repeat(repeats), 100));
    tree
}

fn items(tree: &Tree) -> Vec<Item> {
    tree.iter()
        .map(|(meta, data)| {
            if meta.is_file() {
                Item::new(meta.clone(), data.clone())
            } else {
                Item::bare(meta.clone())
            }
        })
        .collect()
}

fn fast(format: ArchiveFormat) -> CreateOptions {
    CreateOptions {
        level: Level::Fast,
        ..CreateOptions::new(format)
    }
}

fn pack(
    folder: &TempDir,
    options: &CreateOptions,
    tree: &Tree,
    ctx: &OpContext,
) -> Result<PathBuf> {
    static NUMBER: AtomicUsize = AtomicUsize::new(0);
    let number = NUMBER.fetch_add(1, Ordering::Relaxed);
    let path = folder
        .path()
        .join(format!("archive-{number}.{}", options.format.id()));
    let output = BufWriter::new(File::create(&path)?);
    let mut writer = create(output, options, ctx.budget)?;
    writer.add_all(items(tree), ctx)?;
    writer.finish()?;
    Ok(path)
}

#[derive(Debug)]
struct Unpacked {
    format: ArchiveFormat,
    listed: Vec<Entry>,
    got: BTreeMap<String, Got>,
}

fn unpack(
    path: &Path,
    options: OpenOptions,
    pick: &Selection,
    ctx: &OpContext,
) -> Result<Unpacked> {
    let mut opened = open(path, options)?;
    let listed = opened.reader.entries(ctx)?;
    let sink = Collector::default();
    opened.reader.extract(pick, &sink, ctx)?;
    Ok(Unpacked {
        format: opened.format,
        listed,
        got: sink.by_path(),
    })
}

fn unpack_all(path: &Path, ctx: &OpContext) -> Result<Unpacked> {
    unpack(path, OpenOptions::default(), &Selection::All, ctx)
}

/// Whether a format keeps times finer than a second.
fn keeps_fractions(format: ArchiveFormat) -> bool {
    matches!(format, ArchiveFormat::Zip | ArchiveFormat::SevenZ)
}

fn assert_same(tree: &Tree, unpacked: &Unpacked, label: &str) {
    let got = &unpacked.got;
    assert_eq!(got.len(), tree.len(), "{label}: number of entries");
    assert_eq!(
        unpacked.listed.len(),
        tree.len(),
        "{label}: length of the list"
    );
    for (meta, data) in tree {
        let path = meta.path.trim_end_matches('/');
        let found = got
            .get(path)
            .unwrap_or_else(|| panic!("{label}: {path} is missing"));
        assert_eq!(found.entry.meta.kind, meta.kind, "{label}: kind of {path}");
        assert!(found.data == *data, "{label}: content of {path}");
        let (wanted, kept) = (meta.modified.unwrap(), found.entry.meta.modified.unwrap());
        assert_eq!(kept.seconds, wanted.seconds, "{label}: time of {path}");
        if keeps_fractions(unpacked.format) {
            assert_eq!(kept.nanos, wanted.nanos, "{label}: time of {path}");
        }
        let listed = unpacked
            .listed
            .iter()
            .find(|entry| entry.meta.path.trim_end_matches('/') == path)
            .unwrap_or_else(|| panic!("{label}: {path} is not listed"));
        if meta.is_file() {
            assert_eq!(listed.meta.size, Some(data.len() as u64), "{label}: {path}");
        }
    }
}

#[test]
fn every_container_gives_back_what_it_took() {
    let folder = TempDir::new().unwrap();
    let tree = tree();
    for format in CONTAINERS {
        for threads in [1, 4] {
            for solid in [true, false] {
                if !solid && format != ArchiveFormat::SevenZ {
                    continue;
                }
                let label = format!("{} on {threads} threads, solid {solid}", format.id());
                let ctx = context(threads);
                let options = CreateOptions {
                    solid,
                    ..fast(format)
                };
                let path = pack(&folder, &options, &tree, &ctx).expect(&label);
                let unpacked = unpack_all(&path, &ctx).expect(&label);
                assert_eq!(unpacked.format, format, "{label}");
                assert_same(&tree, &unpacked, &label);
            }
        }
    }
}

#[test]
fn large_files_go_through_the_archive_next_to_small_ones() {
    let folder = TempDir::new().unwrap();
    let tree = large_tree();
    let ctx = context(4);
    for (format, solid) in [
        (ArchiveFormat::Zip, false),
        (ArchiveFormat::SevenZ, false),
        (ArchiveFormat::SevenZ, true),
    ] {
        let label = format!("{} solid {solid}", format.id());
        let options = CreateOptions {
            solid,
            ..fast(format)
        };
        let path = pack(&folder, &options, &tree, &ctx).expect(&label);
        let size = fs::metadata(&path).unwrap().len();
        let raw: u64 = tree.iter().map(|(_, data)| data.len() as u64).sum();
        // A third of the tree is noise; the rest compresses well.
        assert!(size < raw / 4 * 3, "{label}: {size} of {raw}");
        assert_same(&tree, &unpack_all(&path, &ctx).expect(&label), &label);
    }
}

#[test]
fn every_method_and_level_round_trips() {
    let folder = TempDir::new().unwrap();
    let tree = small_tree();
    let ctx = context(2);
    for format in [ArchiveFormat::Zip, ArchiveFormat::SevenZ] {
        for method in methods(format) {
            for level in [Level::Fast, Level::Ultra] {
                let label = format!("{} {method:?} {level:?}", format.id());
                let options = CreateOptions {
                    method: Some(*method),
                    level,
                    ..CreateOptions::new(format)
                };
                let path = pack(&folder, &options, &tree, &ctx).expect(&label);
                let unpacked = unpack_all(&path, &ctx).expect(&label);
                assert_same(&tree, &unpacked, &label);
                let readme = &unpacked.got["docs/readme.txt"].entry;
                assert_eq!(readme.method.as_deref(), Some(method.name()), "{label}");
                if *method != Method::Store && format == ArchiveFormat::Zip {
                    assert!(readme.packed_size.unwrap() < 5000, "{label}");
                }
            }
        }
    }
    // A level of no compression stores, whatever the method.
    for format in CONTAINERS {
        let options = CreateOptions {
            level: Level::Store,
            ..CreateOptions::new(format)
        };
        let path = pack(&folder, &options, &tree, &ctx).expect(format.id());
        assert_same(
            &tree,
            &unpack_all(&path, &ctx).expect(format.id()),
            format.id(),
        );
    }
}

#[test]
fn a_single_file_is_an_archive_of_one_entry() {
    let folder = TempDir::new().unwrap();
    let data = Set::OneLarge.files(Scale::Small)[0].bytes();
    for format in SINGLES {
        for threads in [1, 4] {
            let ctx = context(threads);
            let tree = vec![file("ignored name.txt", data.clone(), 0)];
            let path = pack(&folder, &fast(format), &tree, &ctx).expect(format.id());
            let unpacked = unpack_all(&path, &ctx).expect(format.id());
            assert_eq!(unpacked.format, format);
            assert_eq!(unpacked.got.len(), 1);
            // The entry is named after the archive, without its extension.
            let stem = path.file_stem().unwrap().to_string_lossy().into_owned();
            let got = &unpacked.got[&stem];
            assert!(got.data == data, "{}", format.id());
            assert_eq!(
                unpacked.listed[0].packed_size,
                Some(fs::metadata(&path).unwrap().len())
            );
        }
        // One file, and nothing but a file.
        let two = vec![file("a", b"a".to_vec(), 0), file("b", b"b".to_vec(), 0)];
        let error = pack(&folder, &fast(format), &two, &context(1)).unwrap_err();
        assert_eq!(error.code, "singleFileOnly");
        let error = pack(&folder, &fast(format), &vec![folder_entry()], &context(1)).unwrap_err();
        assert_eq!(error.code, "singleFileOnly");
    }
}

fn folder_entry() -> (EntryMeta, Vec<u8>) {
    folder("folder", 0)
}

#[test]
fn an_archive_without_entries_is_an_archive() {
    let folder = TempDir::new().unwrap();
    let ctx = context(2);
    for format in CONTAINERS {
        let path = pack(&folder, &fast(format), &Vec::new(), &ctx).expect(format.id());
        let unpacked = unpack_all(&path, &ctx).expect(format.id());
        assert_eq!(unpacked.format, format);
        assert!(unpacked.listed.is_empty(), "{}", format.id());
        assert!(unpacked.got.is_empty(), "{}", format.id());
    }
}

#[test]
fn a_selection_brings_what_was_selected_and_nothing_else() {
    let folder = TempDir::new().unwrap();
    let tree = tree();
    let ctx = context(4);
    for format in CONTAINERS {
        let path = pack(&folder, &fast(format), &tree, &ctx).expect(format.id());
        let pick = Selection::paths(["docs", "empty.bin"]);
        let unpacked = unpack(&path, OpenOptions::default(), &pick, &ctx).expect(format.id());
        let mut paths: Vec<&str> = unpacked.got.keys().map(String::as_str).collect();
        paths.sort_unstable();
        assert_eq!(
            paths,
            [
                "docs",
                "docs/notes.md",
                "docs/readme.txt",
                "docs/пример текста.txt",
                "empty.bin"
            ],
            "{}",
            format.id()
        );
        assert_eq!(unpacked.got["docs/notes.md"].data, b"# Notes\n");
        // By position in the archive, the last entry alone.
        let last = unpacked.listed.last().unwrap();
        let pick = Selection::indexes(vec![last.index]);
        let unpacked = unpack(&path, OpenOptions::default(), &pick, &ctx).expect(format.id());
        assert_eq!(unpacked.got.len(), 1, "{}", format.id());
        let (path, got) = unpacked.got.iter().next().unwrap();
        let (_, wanted) = tree
            .iter()
            .find(|(meta, _)| meta.path.trim_end_matches('/') == path)
            .unwrap();
        assert!(got.data == *wanted, "{}", format.id());
    }
}

#[test]
fn links_keep_their_targets() {
    let folder = TempDir::new().unwrap();
    let ctx = context(2);
    let mut link = EntryMeta::symlink("docs/latest", "readme.txt");
    link.modified = Some(stamp(7));
    let mut tree = small_tree();
    tree.push((link, Vec::new()));
    for format in CONTAINERS {
        let path = pack(&folder, &fast(format), &tree, &ctx).expect(format.id());
        let unpacked = unpack_all(&path, &ctx).expect(format.id());
        let got = &unpacked.got["docs/latest"];
        assert_eq!(got.entry.meta.kind, EntryKind::Symlink, "{}", format.id());
        assert_eq!(
            got.entry.meta.link_target.as_deref(),
            Some("readme.txt"),
            "{}",
            format.id()
        );
        assert_eq!(got.data, b"");
        assert_eq!(unpacked.got.len(), tree.len(), "{}", format.id());
    }
    // Another name of a file is something only tar can hold.
    let mut hard = EntryMeta::file("docs/again.txt", 0);
    hard.kind = EntryKind::Hardlink;
    hard.link_target = Some(String::from("docs/readme.txt"));
    hard.modified = Some(stamp(8));
    let mut tree = small_tree();
    tree.push((hard, Vec::new()));
    for format in CONTAINERS {
        let packed = pack(&folder, &fast(format), &tree, &ctx);
        if format.is_tar() {
            let unpacked = unpack_all(&packed.expect(format.id()), &ctx).expect(format.id());
            let got = &unpacked.got["docs/again.txt"].entry.meta;
            assert_eq!(got.kind, EntryKind::Hardlink);
            assert_eq!(got.link_target.as_deref(), Some("docs/readme.txt"));
        } else {
            assert_eq!(packed.unwrap_err().code, "hardlinkUnsupported");
        }
    }
}

#[test]
fn permissions_and_attributes_survive() {
    let folder = TempDir::new().unwrap();
    let ctx = context(1);
    let (mut script, data) = file("run.sh", b"#!/bin/sh\n".to_vec(), 1);
    script.unix_mode = Some(0o755);
    let (mut hidden, _) = file("hidden.txt", b"x".to_vec(), 2);
    hidden.windows_attributes = Some(0x22);
    for format in CONTAINERS {
        let tree = vec![
            (script.clone(), data.clone()),
            (hidden.clone(), b"x".to_vec()),
        ];
        let path = pack(&folder, &fast(format), &tree, &ctx).expect(format.id());
        let unpacked = unpack_all(&path, &ctx).expect(format.id());
        let mode = unpacked.got["run.sh"].entry.meta.unix_mode;
        assert_eq!(mode, Some(0o755), "{}", format.id());
        if !format.is_tar() {
            let attributes = unpacked.got["hidden.txt"].entry.meta.windows_attributes;
            assert_eq!(attributes, Some(0x22), "{}", format.id());
        }
    }
}

#[test]
fn an_encrypted_archive_opens_with_its_password_only() {
    let folder = TempDir::new().unwrap();
    let tree = tree();
    for (format, solid, encrypt_names) in [
        (ArchiveFormat::Zip, false, false),
        (ArchiveFormat::SevenZ, true, false),
        (ArchiveFormat::SevenZ, false, false),
        (ArchiveFormat::SevenZ, true, true),
    ] {
        let label = format!("{} solid {solid} names {encrypt_names}", format.id());
        let options = CreateOptions {
            password: Some(Password::from("correct horse")),
            solid,
            encrypt_names,
            ..fast(format)
        };
        let path = pack(&folder, &options, &tree, &context(4)).expect(&label);
        // Nobody to ask: the archive says what it needs. Its list is open
        // to all unless the names were to be hidden too.
        let listed = open(&path, OpenOptions::default())
            .and_then(|mut opened| opened.reader.entries(&context(4)));
        match listed {
            Ok(listed) => {
                assert!(!encrypt_names, "{label}");
                assert_eq!(listed.len(), tree.len(), "{label}");
            }
            Err(error) => {
                assert!(encrypt_names, "{label}");
                assert_eq!(error.code, "passwordRequired", "{label}");
            }
        }
        let error = unpack_all(&path, &context(4)).expect_err(&label);
        assert_eq!(error.code, "passwordRequired", "{label}");

        // The password known in advance.
        let known = OpenOptions {
            password: Some(Password::from("correct horse")),
            ..OpenOptions::default()
        };
        let unpacked = unpack(&path, known, &Selection::All, &context(4)).expect(&label);
        assert_same(&tree, &unpacked, &label);
        let open_content = unpacked
            .listed
            .iter()
            .filter(|entry| entry.meta.size.unwrap_or(0) > 0 && !entry.encrypted)
            .count();
        assert_eq!(open_content, 0, "{label}");

        // A wrong one first, then the right one: asked once more, for all
        // the threads together.
        let (ctx, answers) = answering(vec!["wrong", "correct horse"]);
        let unpacked = unpack_all(&path, &ctx).expect(&label);
        assert_same(&tree, &unpacked, &label);
        assert_eq!(answers.asked.load(Ordering::SeqCst), 2, "{label}");
        assert_eq!(answers.retries.load(Ordering::SeqCst), 1, "{label}");

        // Wrong, and nothing better to offer.
        let (ctx, _) = answering(vec!["wrong"]);
        let error = unpack_all(&path, &ctx).expect_err(&label);
        assert_eq!(error.code, "wrongPassword", "{label}");
    }
    for format in CONTAINERS {
        if !can_encrypt(format) {
            let options = CreateOptions {
                password: Some(Password::from("secret")),
                ..fast(format)
            };
            let error = pack(&folder, &options, &small_tree(), &context(1)).unwrap_err();
            assert_eq!(error.code, "encryptionUnsupported");
        }
    }
}

#[test]
fn damage_is_reported_and_never_passes_for_data() {
    let folder = TempDir::new().unwrap();
    let tree = tree();
    let ctx = context(4);
    let codes = ["damaged", "damagedEntry", "truncated"];
    for format in CONTAINERS {
        let path = pack(&folder, &fast(format), &tree, &ctx).expect(format.id());
        let good = fs::read(&path).unwrap();
        // A plain tar has no check of its content: only its shape can break.
        if format != ArchiveFormat::Tar {
            for place in [good.len() / 3, good.len() / 2] {
                let mut bad = good.clone();
                bad[place] ^= 0x5A;
                fs::write(&path, &bad).unwrap();
                let error = unpack_all(&path, &ctx)
                    .err()
                    .unwrap_or_else(|| panic!("{} with a flipped byte at {place}", format.id()));
                assert!(
                    codes.contains(&error.code.as_str()),
                    "{}: {error}",
                    format.id()
                );
            }
        }
        for cut in [good.len() / 2, good.len() - 20] {
            // A tar ends with blocks of zeros: one cut there lost nothing.
            if format == ArchiveFormat::Tar && cut > good.len() / 2 {
                continue;
            }
            fs::write(&path, &good[..cut]).unwrap();
            let error = unpack_all(&path, &ctx)
                .err()
                .unwrap_or_else(|| panic!("{} cut at {cut}", format.id()));
            assert!(
                codes.contains(&error.code.as_str()),
                "{}: {error}",
                format.id()
            );
        }
    }
}

#[test]
fn the_content_decides_what_a_file_is() {
    let folder = TempDir::new().unwrap();
    let tree = small_tree();
    let ctx = context(1);
    for (format, name) in [
        (ArchiveFormat::TarGz, "backup.gz"),
        (ArchiveFormat::TarZst, "backup.zip"),
        (ArchiveFormat::SevenZ, "backup.zip"),
        (ArchiveFormat::Zip, "backup.7z"),
        (ArchiveFormat::Tar, "backup.bin"),
    ] {
        let packed = pack(&folder, &fast(format), &tree, &ctx).unwrap();
        let renamed = folder.path().join(name);
        fs::rename(&packed, &renamed).unwrap();
        let unpacked = unpack_all(&renamed, &ctx).expect(name);
        assert_eq!(unpacked.format, format, "{name}");
        assert_same(&tree, &unpacked, name);
        fs::remove_file(&renamed).unwrap();
    }
    let text = folder.path().join("notes.txt");
    fs::write(&text, b"not an archive at all").unwrap();
    assert_eq!(
        open(&text, OpenOptions::default()).err().unwrap().code,
        "unknownFormat"
    );
    let missing = folder.path().join("missing.zip");
    assert_eq!(
        open(&missing, OpenOptions::default()).err().unwrap().code,
        "notFound"
    );
    // RAR is recognized in every build, and read where its library is
    // built in.
    let rar = folder.path().join("old.bin");
    fs::write(&rar, include_bytes!("../fixtures/rar4-stored.rar")).unwrap();
    let opened = open(&rar, OpenOptions::default());
    #[cfg(feature = "rar")]
    {
        let mut opened = opened.unwrap();
        assert_eq!(opened.format, ArchiveFormat::Rar);
        let entries = opened.reader.entries(&ctx).unwrap();
        assert_eq!(entries[0].meta.path, "VERSION");
    }
    #[cfg(not(feature = "rar"))]
    {
        let error = opened.err().unwrap();
        assert_eq!(error.code, "unsupportedFormat");
        assert_eq!(error.params["format"], "rar");
    }
}

#[test]
fn the_comment_of_a_zip_is_kept() {
    let folder = TempDir::new().unwrap();
    let ctx = context(1);
    let options = CreateOptions {
        comment: Some(String::from("Сборка от вторника")),
        ..fast(ArchiveFormat::Zip)
    };
    let path = pack(&folder, &options, &small_tree(), &ctx).unwrap();
    let mut opened = open(&path, OpenOptions::default()).unwrap();
    assert_eq!(
        opened.reader.comment(&ctx).unwrap().as_deref(),
        Some("Сборка от вторника")
    );
    let plain = pack(&folder, &fast(ArchiveFormat::Zip), &small_tree(), &ctx).unwrap();
    let mut opened = open(&plain, OpenOptions::default()).unwrap();
    assert_eq!(opened.reader.comment(&ctx).unwrap(), None);
    let options = CreateOptions {
        comment: Some("x".repeat(70_000)),
        ..fast(ArchiveFormat::Zip)
    };
    let error = pack(&folder, &options, &small_tree(), &ctx).unwrap_err();
    assert_eq!(error.code, "commentTooLong");
}

/// A ZIP of one stored file, written by hand: the only way to get a name
/// in the code page of an old program.
fn raw_zip(name: &[u8], data: &[u8]) -> Vec<u8> {
    let crc = crc32fast::hash(data);
    let size = data.len() as u32;
    let length = name.len() as u16;
    let mut zip = Vec::new();
    zip.extend_from_slice(b"PK\x03\x04");
    zip.extend_from_slice(&[20, 0, 0, 0, 0, 0]);
    // 12:00:00 on the first of January 2020, as DOS counts.
    zip.extend_from_slice(&[0x00, 0x60, 0x21, 0x50]);
    zip.extend_from_slice(&crc.to_le_bytes());
    zip.extend_from_slice(&size.to_le_bytes());
    zip.extend_from_slice(&size.to_le_bytes());
    zip.extend_from_slice(&length.to_le_bytes());
    zip.extend_from_slice(&[0, 0]);
    zip.extend_from_slice(name);
    zip.extend_from_slice(data);
    let central = zip.len() as u32;
    zip.extend_from_slice(b"PK\x01\x02");
    zip.extend_from_slice(&[20, 0, 20, 0, 0, 0, 0, 0]);
    zip.extend_from_slice(&[0x00, 0x60, 0x21, 0x50]);
    zip.extend_from_slice(&crc.to_le_bytes());
    zip.extend_from_slice(&size.to_le_bytes());
    zip.extend_from_slice(&size.to_le_bytes());
    zip.extend_from_slice(&length.to_le_bytes());
    zip.extend_from_slice(&[0; 12]);
    zip.extend_from_slice(&0u32.to_le_bytes());
    zip.extend_from_slice(name);
    let central_size = zip.len() as u32 - central;
    zip.extend_from_slice(b"PK\x05\x06");
    zip.extend_from_slice(&[0, 0, 0, 0, 1, 0, 1, 0]);
    zip.extend_from_slice(&central_size.to_le_bytes());
    zip.extend_from_slice(&central.to_le_bytes());
    zip.extend_from_slice(&[0, 0]);
    zip
}

#[test]
fn names_of_old_archives_are_read_in_their_code_page() {
    let folder = TempDir::new().unwrap();
    let ctx = context(1);
    let (name, _, _) = encoding_rs::IBM866.encode("Отчёт за квартал/Итоговая таблица.txt");
    let path = folder.path().join("old.zip");
    fs::write(&path, raw_zip(&name, b"content")).unwrap();

    let guessed = OpenOptions {
        hint: Some(String::from("ru")),
        ..OpenOptions::default()
    };
    let unpacked = unpack(&path, guessed, &Selection::All, &ctx).unwrap();
    let got = &unpacked.got["Отчёт за квартал/Итоговая таблица.txt"];
    assert_eq!(got.data, b"content");
    // The time of the header is local time: it is read as such.
    let modified = got.entry.meta.modified.unwrap();
    assert!((1_577_793_600..1_577_966_400).contains(&modified.seconds));

    // The user knows better than the guess.
    let chosen = OpenOptions {
        encoding: NameEncoding::from_label("windows-1251").unwrap(),
        ..OpenOptions::default()
    };
    let unpacked = unpack(&path, chosen, &Selection::All, &ctx).unwrap();
    assert!(!unpacked
        .got
        .contains_key("Отчёт за квартал/Итоговая таблица.txt"));
    assert_eq!(unpacked.got.len(), 1);
}

#[test]
fn an_archive_in_memory_opens_like_a_file() {
    let folder = TempDir::new().unwrap();
    let tree = small_tree();
    let ctx = context(2);
    for format in CONTAINERS {
        let path = pack(&folder, &fast(format), &tree, &ctx).unwrap();
        let bytes = fs::read(&path).unwrap();
        // The name is of no help: the content decides.
        let source = Source::memory("inner", bytes.clone());
        let mut opened = open_source(source, OpenOptions::default()).unwrap();
        assert_eq!(opened.format, format);
        let listed = opened.reader.entries(&ctx).unwrap();
        let sink = Collector::default();
        opened.reader.extract(&Selection::All, &sink, &ctx).unwrap();
        let unpacked = Unpacked {
            format,
            listed,
            got: sink.by_path(),
        };
        assert_same(&tree, &unpacked, format.id());

        // The format given, the same bytes are read without a guess.
        let mut known = reader(
            format,
            Source::memory("inner", bytes),
            OpenOptions::default(),
        )
        .unwrap();
        assert_eq!(known.entries(&ctx).unwrap().len(), tree.len());
    }
}

#[test]
fn an_archive_cut_into_volumes_reads_like_a_whole_one() {
    let folder = TempDir::new().unwrap();
    let tree = small_tree();
    let ctx = context(2);
    for format in CONTAINERS {
        let path = pack(&folder, &fast(format), &tree, &ctx).unwrap();
        let bytes = fs::read(&path).unwrap();
        let name = path.file_name().unwrap().to_str().unwrap().to_owned();
        // Seven volumes, the last one shorter.
        let size = bytes.len() / 7 + 1;
        let volumes = folder.path().join("volumes");
        fs::create_dir_all(&volumes).unwrap();
        let mut count = 0;
        for (index, part) in bytes.chunks(size).enumerate() {
            let volume = crate::volumes::name(&name, index as u64 + 1);
            fs::write(volumes.join(volume), part).unwrap();
            count += 1;
        }
        // Opened by its first volume and by one from the middle.
        for number in [1, 4] {
            let volume = volumes.join(crate::volumes::name(&name, number));
            let opened = open(&volume, OpenOptions::default()).unwrap();
            assert_eq!(opened.volumes, count, "{}", format.id());
            assert_eq!(opened.size, bytes.len() as u64, "{}", format.id());
            let unpacked = unpack_all(&volume, &ctx).expect(format.id());
            assert_eq!(unpacked.format, format);
            assert_same(&tree, &unpacked, format.id());
        }
        // Without its last volume the archive is one that was cut short.
        fs::remove_file(volumes.join(crate::volumes::name(&name, 7))).unwrap();
        let first = volumes.join(crate::volumes::name(&name, 1));
        assert!(unpack_all(&first, &ctx).is_err(), "{}", format.id());
        fs::remove_dir_all(&volumes).unwrap();
        fs::remove_file(&path).unwrap();
    }
}

#[test]
fn a_dictionary_larger_than_the_budget_is_an_error_of_memory() {
    let folder = TempDir::new().unwrap();
    let options = CreateOptions {
        dictionary: Some(32 << 20),
        ..CreateOptions::new(ArchiveFormat::TarXz)
    };
    let path = pack(&folder, &options, &small_tree(), &context(1)).unwrap();
    let mut tight = context(1);
    tight.budget.memory = 4 << 20;
    assert_eq!(
        unpack_all(&path, &tight).unwrap_err().code,
        "notEnoughMemory"
    );
    assert_same(
        &small_tree(),
        &unpack_all(&path, &context(1)).unwrap(),
        "tar.xz",
    );
}

#[test]
fn a_cancelled_operation_stops() {
    let folder = TempDir::new().unwrap();
    let tree = tree();
    for format in CONTAINERS {
        let ctx = context(4);
        let path = pack(&folder, &fast(format), &tree, &ctx).unwrap();
        ctx.control.cancel();
        assert!(
            unpack_all(&path, &ctx).unwrap_err().is_cancelled(),
            "{}",
            format.id()
        );
        assert!(
            pack(&folder, &fast(format), &tree, &ctx)
                .unwrap_err()
                .is_cancelled(),
            "{}",
            format.id()
        );
    }
}

/// An archive of a format that is read and never written, with the content
/// of one of its files.
struct Sample {
    label: &'static str,
    format: ArchiveFormat,
    bytes: Vec<u8>,
    content: Vec<u8>,
}

/// One archive, or more, of every format that is only read: made by the
/// tests of its reader, or by the program the format belongs to.
fn read_only() -> Vec<Sample> {
    let text = lines(200);
    let sample = |label, format, bytes, content: &[u8]| Sample {
        label,
        format,
        bytes,
        content: content.to_vec(),
    };
    let mut image = Builder::new(0, 0);
    let (first, second) = (image.plain(b"first"), image.plain(&text));
    image.image(&wim_folder(
        "",
        vec![
            wim_file("first.txt", first),
            wim_folder("docs", vec![wim_file("text.txt", second)]),
        ],
    ));
    let mut samples = vec![
        sample(
            "ar",
            ArchiveFormat::Ar,
            gnu(&[
                ("first.txt", b"first"),
                ("a name longer than sixteen.txt", &text),
            ]),
            &text,
        ),
        sample(
            "cpio",
            ArchiveFormat::Cpio,
            newc(&[
                cpio_file("first.txt", b"first"),
                cpio_file("docs/text.txt", &text),
            ]),
            &text,
        ),
        sample(
            "lha",
            ArchiveFormat::Lha,
            lha(&[
                level0(b"FIRST.TXT", b"first"),
                level0(b"DOCS\\TEXT.TXT", &text),
            ]),
            &text,
        ),
        sample(
            "cab",
            ArchiveFormat::Cab,
            cabinet(
                &[
                    Group {
                        kind: MSZIP,
                        files: vec![
                            item(b"first.txt", 0x20, b"first"),
                            item(b"docs\\text.txt", 0x20, &text),
                        ],
                    },
                    Group {
                        kind: STORED,
                        files: vec![item(b"last.txt", 0x20, b"last")],
                    },
                ],
                [0; 3],
            ),
            &text,
        ),
        sample(
            "iso",
            ArchiveFormat::Iso,
            gunzip(include_bytes!("../fixtures/bridge.iso.gz")),
            b"read me\n",
        ),
        sample(
            "udf",
            ArchiveFormat::Iso,
            gunzip(include_bytes!("../fixtures/udf-250.iso.gz")),
            b"read me\n",
        ),
        sample("wim", ArchiveFormat::Wim, image.finish(), &text),
    ];
    if cfg!(feature = "rar") {
        samples.push(sample(
            "rar 4",
            ArchiveFormat::Rar,
            include_bytes!("../fixtures/rar4-stored.rar").to_vec(),
            b"unrar-0.4.0",
        ));
        samples.push(sample(
            "rar 5",
            ArchiveFormat::Rar,
            include_bytes!("../fixtures/rar5-comment.rar").to_vec(),
            b"target\nCargo.lock\n",
        ));
    }
    samples
}

#[test]
fn the_formats_that_are_only_read_open_by_their_content() {
    let folder = TempDir::new().unwrap();
    let ctx = context(2);
    for sample in read_only() {
        let label = sample.label;
        assert!(!sample.format.can_write(), "{label}");
        // In memory, under a name that says nothing.
        let source = Source::memory("inner", sample.bytes.clone());
        let mut opened = open_source(source, OpenOptions::default()).expect(label);
        assert_eq!(opened.format, sample.format, "{label}");
        let listed = opened.reader.entries(&ctx).expect(label);
        let sink = Collector::default();
        opened
            .reader
            .extract(&Selection::All, &sink, &ctx)
            .expect(label);
        // A reader hands the entries over in the order that suits it.
        let mut got = sink.in_order();
        got.sort_by_key(|got| got.entry.index);
        let arrived: Vec<Entry> = got.iter().map(|got| got.entry.clone()).collect();
        assert_eq!(arrived, listed, "{label}");
        assert!(
            got.iter().any(|got| got.data == sample.content),
            "{label}: the content of a file"
        );
        for (index, entry) in listed.iter().enumerate() {
            assert_eq!(entry.index, index as u64, "{label}");
        }

        // In a file, under a name that says something else.
        let path = folder.path().join("backup.zip");
        fs::write(&path, &sample.bytes).unwrap();
        let mut opened = open(&path, OpenOptions::default()).expect(label);
        assert_eq!(opened.format, sample.format, "{label}");
        assert_eq!(opened.reader.entries(&ctx).expect(label), listed, "{label}");

        // One entry asked for, one entry given.
        let last = listed.last().unwrap();
        let sink = Collector::default();
        opened
            .reader
            .extract(&Selection::indexes(vec![last.index]), &sink, &ctx)
            .expect(label);
        let got = sink.in_order();
        assert_eq!(got.len(), 1, "{label}");
        assert_eq!(got[0].entry, *last, "{label}");
        drop(opened);
        fs::remove_file(&path).unwrap();

        // Nothing of it can be made.
        let mut output = io::Cursor::new(Vec::new());
        let made = create(&mut output, &fast(sample.format), ctx.budget);
        assert_eq!(made.err().unwrap().code, "cannotCreate", "{label}");
    }
}

#[test]
fn a_cancelled_reading_of_a_format_that_is_only_read_stops() {
    for sample in read_only() {
        let ctx = context(2);
        ctx.control.cancel();
        let source = Source::memory("inner", sample.bytes);
        let mut reader = reader(sample.format, source, OpenOptions::default()).unwrap();
        let outcome = reader
            .entries(&ctx)
            .and_then(|_| reader.extract(&Selection::All, &Collector::default(), &ctx));
        assert!(outcome.unwrap_err().is_cancelled(), "{}", sample.label);
    }
}

/// Names that make a tree: unique, and no file where a folder is.
fn random_tree() -> impl Strategy<Value = Tree> {
    let name = "[a-zA-Z0-9 _.-]{1,12}|[а-яё]{1,8}|[一-龥]{1,4}";
    let entry = (
        0u8..4,
        name,
        proptest::collection::vec(any::<u8>(), 0..3000),
        1usize..40,
    );
    proptest::collection::vec(entry, 0..24).prop_map(|entries| {
        let mut tree = Tree::new();
        for (index, (folder, name, data, repeats)) in entries.into_iter().enumerate() {
            let name = name.trim_matches([' ', '.']);
            let path = format!("folder {folder}/{index}-{name}.bin");
            tree.push(file(&path, data.repeat(repeats), index as i64));
        }
        tree
    })
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(12))]

    #[test]
    fn any_tree_round_trips(
        tree in random_tree(),
        threads in 1usize..5,
        level in 0usize..5,
        solid in any::<bool>(),
    ) {
        let folder = TempDir::new().unwrap();
        let ctx = context(threads);
        for format in CONTAINERS {
            let options = CreateOptions {
                level: Level::ALL[level],
                solid,
                ..CreateOptions::new(format)
            };
            let path = pack(&folder, &options, &tree, &ctx).unwrap();
            let unpacked = unpack_all(&path, &ctx).unwrap();
            assert_same(&tree, &unpacked, format.id());
        }
    }

    #[test]
    fn a_mangled_archive_never_panics(
        places in proptest::collection::vec((0.0f64..1.0, any::<u8>()), 1..6),
        cut in proptest::option::of(0.0f64..1.0),
    ) {
        let folder = TempDir::new().unwrap();
        let ctx = context(2);
        let tree = small_tree();
        for format in CONTAINERS {
            let path = pack(&folder, &fast(format), &tree, &ctx).unwrap();
            let mut bytes = fs::read(&path).unwrap();
            for (place, value) in &places {
                let length = bytes.len();
                bytes[((*place * length as f64) as usize).min(length - 1)] = *value;
            }
            if let Some(cut) = cut {
                bytes.truncate((cut * bytes.len() as f64) as usize);
            }
            fs::write(&path, &bytes).unwrap();
            // Whatever comes out, it comes out as an answer.
            let _: Result<Unpacked, Error> = unpack_all(&path, &ctx);
        }
    }
}

proptest! {
    // These are read out of memory and are small: more of them fit into
    // the time of a test.
    #![proptest_config(ProptestConfig::with_cases(160))]

    #[test]
    fn a_mangled_archive_of_a_format_that_is_only_read_never_panics(
        places in proptest::collection::vec((0.0f64..1.0, any::<u8>()), 1..6),
        // Most of what makes an archive is at its start.
        early in proptest::collection::vec((0usize..600, any::<u8>()), 0..4),
        cut in proptest::option::of(0.0f64..1.0),
        guess in any::<bool>(),
    ) {
        let mut ctx = context(2);
        ctx.budget.memory = 64 << 20;
        for sample in read_only() {
            let mut bytes = sample.bytes;
            let length = bytes.len();
            for (place, value) in &places {
                bytes[((*place * length as f64) as usize).min(length - 1)] = *value;
            }
            for (place, value) in &early {
                bytes[*place % length] ^= *value;
            }
            if let Some(cut) = cut {
                bytes.truncate((cut * length as f64) as usize);
            }
            let source = Source::memory("inner", bytes);
            // As the format it was, and as whatever it looks like now.
            let opened = if guess {
                open_source(source, OpenOptions::default()).map(|opened| opened.reader)
            } else {
                reader(sample.format, source, OpenOptions::default())
            };
            let Ok(mut reader) = opened else { continue };
            let _ = reader.comment(&ctx);
            if reader.entries(&ctx).is_ok() {
                let _ = reader.extract(&Selection::All, &Collector::default(), &ctx);
            }
        }
    }
}

/// A Windows program whose own bytes end at `end`, so an archive glued
/// after it starts there.
fn program(end: usize) -> Vec<u8> {
    let mut bytes = vec![0u8; end];
    bytes[0] = b'M';
    bytes[1] = b'Z';
    bytes[0x3C..0x40].copy_from_slice(&64u32.to_le_bytes());
    bytes[64..68].copy_from_slice(b"PE\0\0");
    bytes[70..72].copy_from_slice(&1u16.to_le_bytes());
    // The one section covers the bytes from 128 to `end`.
    bytes[104..108].copy_from_slice(&((end - 128) as u32).to_le_bytes());
    bytes[108..112].copy_from_slice(&128u32.to_le_bytes());
    bytes
}

#[test]
fn an_archive_glued_to_a_program_reads_as_the_archive() {
    let folder = TempDir::new().unwrap();
    let ctx = context(1);
    let tree = small_tree();
    for format in [ArchiveFormat::Zip, ArchiveFormat::SevenZ] {
        let archive = pack(&folder, &fast(format), &tree, &ctx).unwrap();
        let mut glued = program(640);
        glued.extend(fs::read(&archive).unwrap());
        let path = folder.path().join(format!("setup-{}.exe", format.id()));
        fs::write(&path, &glued).unwrap();
        let opened = open(&path, OpenOptions::default()).unwrap();
        assert_eq!(opened.format, format, "{}", format.id());
        assert_eq!(opened.size, glued.len() as u64, "{}", format.id());
        assert_same(&tree, &unpack_all(&path, &ctx).unwrap(), format.id());
    }
    let plain = folder.path().join("empty.exe");
    fs::write(&plain, program(640)).unwrap();
    match open(&plain, OpenOptions::default()) {
        Ok(_) => panic!("a program without an archive was opened"),
        Err(error) => assert_eq!(error.code, "unknownFormat"),
    }
}

use std::fmt::Write as _;
use std::hash::{DefaultHasher, Hash, Hasher};
use std::sync::atomic::Ordering;

use boxit_codecs::OpenOptions as ArchiveOptions;
use boxit_core::{ArchiveFormat, ExtractOptions, Level, Selection};
use tempfile::TempDir;

use super::*;
use crate::archive::Archive;
use crate::testing::{lines, plant, tree, Watch};

const FORMATS: [ArchiveFormat; 6] = [
    ArchiveFormat::Zip,
    ArchiveFormat::SevenZ,
    ArchiveFormat::Tar,
    ArchiveFormat::TarGz,
    ArchiveFormat::TarXz,
    ArchiveFormat::TarZst,
];

/// A folder to store: `work/project` with its files, and a file next to it.
fn sample(root: &Path) -> Vec<PathBuf> {
    // A megabyte of text that is not one line repeated.
    let long = lines(20_000, " is the number of a line that compresses well");
    plant(
        &root.join("work"),
        &[
            ("project/readme.txt", "read me"),
            ("project/docs/пример текста.txt", "привет"),
            ("project/docs/notes.md", "# Notes\n"),
            ("project/empty/", ""),
            ("project/long.txt", &long),
            ("lone.txt", "alone"),
        ],
    );
    vec![
        root.join("work").join("project"),
        root.join("work").join("lone.txt"),
    ]
}

fn request(sources: Vec<PathBuf>, destination: &Path, format: ArchiveFormat) -> CreateRequest {
    CreateRequest {
        sources,
        destination: destination.to_path_buf(),
        options: CreateOptions {
            level: Level::Fast,
            ..CreateOptions::new(format)
        },
        overwrite: false,
        layout: Layout::Whole,
    }
}

fn unpack(archive: &Path, destination: &Path) {
    Archive::open(archive, ArchiveOptions::default())
        .unwrap()
        .extract(
            destination,
            Selection::All,
            ExtractOptions::default(),
            &OpContext::silent(),
        )
        .unwrap();
}

/// The names in a folder that an operation of BoxIt left behind.
fn leftovers(folder: &Path) -> Vec<String> {
    fs::read_dir(folder)
        .unwrap()
        .map(|entry| entry.unwrap().file_name().to_string_lossy().into_owned())
        .filter(|name| name.starts_with(STAGING_PREFIX))
        .collect()
}

#[test]
fn an_archive_holds_what_the_folders_held() {
    let root = TempDir::new().unwrap();
    let sources = sample(root.path());
    let stored = tree(&root.path().join("work"));
    for format in FORMATS {
        let name = format!("out.{}", format.extensions()[0]);
        let destination = root.path().join("archives").join(name);
        let created = create(
            &request(sources.clone(), &destination, format),
            &OpContext::silent(),
        )
        .unwrap_or_else(|error| panic!("{}: {error}", format.id()));
        assert_eq!(created.files, 5, "{}", format.id());
        assert_eq!(created.folders, 3, "{}", format.id());
        assert_eq!(created.links, 0, "{}", format.id());
        assert_eq!(
            created.bytes,
            stored.values().map(|text| text.len() as u64).sum::<u64>(),
            "{}",
            format.id()
        );
        assert_eq!(created.packed, fs::metadata(&destination).unwrap().len());
        assert!(created.packed > 0);
        assert!(!created.path.starts_with(r"\\?\"));
        assert_eq!(leftovers(destination.parent().unwrap()), [""; 0]);

        let unpacked = root.path().join(format!("unpacked-{}", format.id()));
        unpack(&destination, &unpacked);
        assert_eq!(tree(&unpacked), stored, "{}", format.id());
    }
}

#[test]
fn the_order_of_an_archive_is_a_folder_and_then_its_content() {
    let root = TempDir::new().unwrap();
    let sources = sample(root.path());
    let destination = root.path().join("out.tar");
    create(
        &request(sources, &destination, ArchiveFormat::Tar),
        &OpContext::silent(),
    )
    .unwrap();
    let listing = Archive::open(&destination, ArchiveOptions::default())
        .unwrap()
        .list(&OpContext::silent())
        .unwrap();
    let paths: Vec<&str> = listing
        .entries
        .iter()
        .map(|entry| entry.meta.path.trim_end_matches('/'))
        .collect();
    assert_eq!(
        paths,
        [
            "lone.txt",
            "project",
            "project/docs",
            "project/docs/notes.md",
            "project/docs/пример текста.txt",
            "project/empty",
            "project/long.txt",
            "project/readme.txt",
        ]
    );
}

#[test]
fn the_times_of_the_files_are_stored() {
    let root = TempDir::new().unwrap();
    let sources = sample(root.path());
    let on_disk = fs::metadata(&sources[1]).unwrap().modified().unwrap();
    let destination = root.path().join("out.7z");
    create(
        &request(sources, &destination, ArchiveFormat::SevenZ),
        &OpContext::silent(),
    )
    .unwrap();
    let listing = Archive::open(&destination, ArchiveOptions::default())
        .unwrap()
        .list(&OpContext::silent())
        .unwrap();
    let lone = listing
        .entries
        .iter()
        .find(|entry| entry.meta.path == "lone.txt")
        .unwrap();
    // The format keeps hundreds of nanoseconds.
    let stored = lone.meta.modified.unwrap();
    let wanted = fsx::timestamp(on_disk);
    assert_eq!(stored.seconds, wanted.seconds);
    assert_eq!(stored.nanos / 100, wanted.nanos / 100);
}

#[test]
fn an_existing_archive_is_kept_unless_it_may_be_replaced() {
    let root = TempDir::new().unwrap();
    let sources = sample(root.path());
    let destination = root.path().join("out.zip");
    fs::write(&destination, b"something of value").unwrap();
    let mut request = request(sources, &destination, ArchiveFormat::Zip);
    let error = create(&request, &OpContext::silent()).unwrap_err();
    assert_eq!(error.code, "exists");
    assert_eq!(error.params["path"], fsx::display(&destination));
    assert_eq!(fs::read(&destination).unwrap(), b"something of value");

    request.overwrite = true;
    create(&request, &OpContext::silent()).unwrap();
    let unpacked = root.path().join("unpacked");
    unpack(&destination, &unpacked);
    assert_eq!(tree(&unpacked), tree(&root.path().join("work")));

    // A folder is never replaced by a file.
    request.destination = root.path().join("work");
    let error = create(&request, &OpContext::silent()).unwrap_err();
    assert_eq!(error.code, "existingFolder");
}

#[test]
fn a_cancelled_creation_leaves_nothing_behind() {
    let root = TempDir::new().unwrap();
    let sources = sample(root.path());
    for format in FORMATS {
        for existed in [false, true] {
            let destination = root.path().join("cancelled.bin");
            if existed {
                fs::write(&destination, b"the old archive").unwrap();
            }
            let mut ctx = OpContext::silent();
            ctx.progress = Arc::new(Watch::cancelling(&ctx.control, 100_000));
            let mut request = request(sources.clone(), &destination, format);
            request.overwrite = true;
            let error = create(&request, &ctx).unwrap_err();
            assert!(error.is_cancelled(), "{}: {error}", format.id());
            assert_eq!(leftovers(root.path()), [""; 0], "{}", format.id());
            if existed {
                assert_eq!(fs::read(&destination).unwrap(), b"the old archive");
                fs::remove_file(&destination).unwrap();
            } else {
                assert_eq!(fsx::existing(&destination), Existing::Nothing);
            }
        }
    }
}

#[test]
fn the_progress_adds_up_to_its_total() {
    let root = TempDir::new().unwrap();
    let sources = sample(root.path());
    for format in FORMATS {
        let destination = root
            .path()
            .join(format!("progress.{}", format.extensions()[0]));
        let watch = Arc::new(Watch::default());
        let mut ctx = OpContext::silent();
        ctx.progress = watch.clone();
        let created = create(&request(sources.clone(), &destination, format), &ctx).unwrap();
        assert_eq!(
            watch.total.load(Ordering::SeqCst),
            created.bytes,
            "{}",
            format.id()
        );
        assert_eq!(
            watch.advanced.load(Ordering::SeqCst),
            created.bytes,
            "{}",
            format.id()
        );
        assert_eq!(
            watch.entries.load(Ordering::SeqCst),
            created.files,
            "{}",
            format.id()
        );
    }
}

#[test]
fn an_archive_is_not_stored_in_itself() {
    let root = TempDir::new().unwrap();
    let sources = sample(root.path());
    let destination = sources[0].join("backup.zip");
    let mut request = request(vec![sources[0].clone()], &destination, ArchiveFormat::Zip);
    request.overwrite = true;
    for _ in 0..2 {
        // The second time the archive of the first is in the folder.
        create(&request, &OpContext::silent()).unwrap();
        let listing = Archive::open(&destination, ArchiveOptions::default())
            .unwrap()
            .list(&OpContext::silent())
            .unwrap();
        assert_eq!(listing.files, 4);
        assert!(listing
            .entries
            .iter()
            .all(|entry| !entry.meta.path.contains("backup")
                && !entry.meta.path.contains(STAGING_PREFIX)));
    }
}

/// The smallest volume there is.
const VOLUME: u64 = volumes::SMALLEST;

/// The sample with a file that does not compress to nothing: the archive
/// of any format takes several volumes.
fn bulky(root: &Path) -> Vec<PathBuf> {
    let sources = sample(root);
    let mut state = 0x2545_f491_4f6c_dd1du64;
    let mut noise = String::new();
    for _ in 0..40_000 {
        state ^= state << 13;
        state ^= state >> 7;
        state ^= state << 17;
        writeln!(noise, "{state:016x}").unwrap();
    }
    fs::write(sources[0].join("noise.txt"), noise).unwrap();
    sources
}

/// The volumes that stand for an archive, with their sizes.
fn volumes_of(destination: &Path) -> Vec<(PathBuf, u64)> {
    output::standing(destination)
        .into_iter()
        .map(|volume| {
            let size = fs::metadata(&volume).unwrap().len();
            (volume, size)
        })
        .collect()
}

/// What tells the content of each volume of an archive from any other.
fn prints_of(destination: &Path) -> Vec<(u64, u64)> {
    volumes_of(destination)
        .into_iter()
        .map(|(volume, size)| {
            let mut hasher = DefaultHasher::new();
            fs::read(volume).unwrap().hash(&mut hasher);
            (size, hasher.finish())
        })
        .collect()
}

#[test]
fn an_archive_cut_into_volumes_is_the_archive_again() {
    let root = TempDir::new().unwrap();
    let sources = bulky(root.path());
    let stored = tree(&root.path().join("work"));
    for format in FORMATS {
        let id = format.id();
        let folder = root.path().join(format!("cut-{id}"));
        let destination = folder.join(format!("out.{}", format.extensions()[0]));
        let mut request = request(sources.clone(), &destination, format);
        request.layout = Layout::Volumes(VOLUME);
        let created =
            create(&request, &OpContext::silent()).unwrap_or_else(|error| panic!("{id}: {error}"));

        let volumes = volumes_of(&destination);
        assert!(volumes.len() >= 3, "{id}: {}", volumes.len());
        assert_eq!(created.volumes, volumes.len() as u64, "{id}");
        assert_eq!(created.path, fsx::display(&volumes[0].0), "{id}");
        assert!(created.path.ends_with(".001"), "{id}");
        // Every volume is full but the last, which is not empty.
        let (last, full) = volumes.split_last().unwrap();
        assert!(full.iter().all(|(_, size)| *size == VOLUME), "{id}");
        assert!((1..=VOLUME).contains(&last.1), "{id}");
        assert_eq!(
            created.packed,
            volumes.iter().map(|(_, size)| size).sum::<u64>(),
            "{id}"
        );
        // Nothing but the volumes is in the folder.
        assert_eq!(
            fs::read_dir(&folder).unwrap().count(),
            volumes.len(),
            "{id}"
        );

        // Opened by any volume, the archive is the whole of it.
        for (number, (volume, _)) in volumes.iter().enumerate().take(2) {
            let mut archive = Archive::open(volume, ArchiveOptions::default()).unwrap();
            assert_eq!(archive.format(), format, "{id}");
            let listing = archive.list(&OpContext::silent()).unwrap();
            assert_eq!(listing.size, created.packed, "{id}");
            assert_eq!(listing.volumes, created.volumes, "{id}");
            assert_eq!(listing.path, created.path, "{id}");
            let unpacked = root.path().join(format!("unpacked-{id}-{number}"));
            unpack(volume, &unpacked);
            assert_eq!(tree(&unpacked), stored, "{id}");
        }
        // The volumes put end to end are an archive in one file.
        let whole = root
            .path()
            .join(format!("whole.{}", format.extensions()[0]));
        let mut joined = Vec::new();
        for (volume, _) in &volumes {
            joined.extend(fs::read(volume).unwrap());
        }
        fs::write(&whole, joined).unwrap();
        let unpacked = root.path().join(format!("unpacked-{id}-whole"));
        unpack(&whole, &unpacked);
        assert_eq!(tree(&unpacked), stored, "{id}");
    }
}

#[test]
fn new_volumes_take_the_place_of_all_the_old() {
    let root = TempDir::new().unwrap();
    let sources = bulky(root.path());
    // The archive goes into the folder it stores.
    let destination = sources[0].join("backup.zip");
    let mut request = request(vec![sources[0].clone()], &destination, ArchiveFormat::Zip);
    request.layout = Layout::Volumes(VOLUME);
    let first = create(&request, &OpContext::silent()).unwrap();
    assert!(first.volumes >= 3);
    let before = prints_of(&destination);

    // They are not replaced unless they may be.
    let error = create(&request, &OpContext::silent()).unwrap_err();
    assert_eq!(error.code, "exists");
    assert_eq!(
        error.params["path"],
        fsx::display(&volumes::path(&destination, 1))
    );
    // Nor when the new archive is not written to its end.
    request.overwrite = true;
    let mut ctx = OpContext::silent();
    ctx.progress = Arc::new(Watch::cancelling(&ctx.control, 100_000));
    assert!(create(&request, &ctx).unwrap_err().is_cancelled());
    assert_eq!(prints_of(&destination), before);
    assert_eq!(leftovers(&sources[0]), [""; 0]);

    // Larger volumes: fewer of them, and none of the old ones left.
    request.layout = Layout::Volumes(VOLUME * 4);
    let second = create(&request, &OpContext::silent()).unwrap();
    assert!(second.volumes < first.volumes);
    assert_eq!(volumes_of(&destination).len() as u64, second.volumes);
    assert_eq!(
        fsx::existing(&volumes::path(&destination, second.volumes + 1)),
        Existing::Nothing
    );
    assert_eq!(leftovers(&sources[0]), [""; 0]);
    // The old volumes were in the folder and are not in the archive.
    let listing = Archive::open(&volumes::path(&destination, 1), ArchiveOptions::default())
        .unwrap()
        .list(&OpContext::silent())
        .unwrap();
    assert_eq!(listing.files, 5);
    assert!(listing
        .entries
        .iter()
        .all(|entry| !entry.meta.path.contains("backup")));
}

#[test]
fn a_cancelled_cut_leaves_nothing_behind() {
    let root = TempDir::new().unwrap();
    let sources = bulky(root.path());
    for format in FORMATS {
        let destination = root.path().join("cancelled.bin");
        let mut ctx = OpContext::silent();
        ctx.progress = Arc::new(Watch::cancelling(&ctx.control, 400_000));
        let mut request = request(sources.clone(), &destination, format);
        request.layout = Layout::Volumes(VOLUME);
        let error = create(&request, &ctx).unwrap_err();
        assert!(error.is_cancelled(), "{}: {error}", format.id());
        assert_eq!(leftovers(root.path()), [""; 0], "{}", format.id());
        assert_eq!(volumes_of(&destination).len(), 0, "{}", format.id());
    }
}

#[test]
fn a_volume_has_a_size_worth_cutting_for() {
    let root = TempDir::new().unwrap();
    let sources = sample(root.path());
    let destination = root.path().join("out.zip");
    let mut request = request(sources, &destination, ArchiveFormat::Zip);
    request.layout = Layout::Volumes(VOLUME - 1);
    let error = create(&request, &OpContext::silent()).unwrap_err();
    assert_eq!(error.code, "volumeTooSmall");
    assert_eq!(error.params["size"], VOLUME.to_string());
    assert_eq!(leftovers(root.path()), [""; 0]);
    assert_eq!(volumes_of(&destination).len(), 0);
}

#[test]
fn what_cannot_be_stored_is_said_before_anything_is_written() {
    let root = TempDir::new().unwrap();
    let sources = sample(root.path());
    let destination = root.path().join("out.zip");
    let silent = OpContext::silent();

    let error = create(
        &request(Vec::new(), &destination, ArchiveFormat::Zip),
        &silent,
    )
    .unwrap_err();
    assert_eq!(error.code, "nothingToAdd");

    // Two sources that would get the same name in the archive.
    plant(&root.path().join("other"), &[("lone.txt", "another")]);
    let twice = vec![
        sources[1].clone(),
        root.path().join("other").join("lone.txt"),
    ];
    let error = create(&request(twice, &destination, ArchiveFormat::Zip), &silent).unwrap_err();
    assert_eq!(error.code, "duplicateName");
    assert_eq!(error.params["name"], "lone.txt");

    let missing = root.path().join("work").join("missing.txt");
    let error = create(
        &request(vec![missing.clone()], &destination, ArchiveFormat::Zip),
        &silent,
    )
    .unwrap_err();
    assert_eq!(error.code, "notFound");
    assert_eq!(error.params["path"], fsx::display(&missing));

    // A format of one file does not take a folder.
    let error = create(
        &request(sources.clone(), &destination, ArchiveFormat::Gz),
        &silent,
    )
    .unwrap_err();
    assert_eq!(error.code, "singleFileOnly");

    // A format that is only read.
    let error = create(&request(sources, &destination, ArchiveFormat::Rar), &silent).unwrap_err();
    assert_eq!(error.code, "cannotCreate");

    assert_eq!(fsx::existing(&destination), Existing::Nothing);
    assert_eq!(leftovers(root.path()), [""; 0]);
}

#[test]
fn one_file_makes_a_compressed_file() {
    let root = TempDir::new().unwrap();
    let sources = sample(root.path());
    let long = sources[0].join("long.txt");
    let destination = root.path().join("long.txt.zst");
    let created = create(
        &request(vec![long.clone()], &destination, ArchiveFormat::Zst),
        &OpContext::silent(),
    )
    .unwrap();
    assert_eq!(created.files, 1);
    assert!(created.packed < created.bytes / 4);
    let unpacked = root.path().join("unpacked");
    unpack(&destination, &unpacked);
    assert_eq!(
        fs::read(unpacked.join("long.txt")).unwrap(),
        fs::read(&long).unwrap()
    );
}

#[test]
fn a_link_is_stored_as_a_link() {
    let root = TempDir::new().unwrap();
    let sources = sample(root.path());
    let link = sources[0].join("latest.txt");
    if fsx::symlink("readme.txt", &link, false).is_err() {
        // Windows lets only administrators and developers create links.
        return;
    }
    let destination = root.path().join("out.tar.gz");
    let created = create(
        &request(vec![sources[0].clone()], &destination, ArchiveFormat::TarGz),
        &OpContext::silent(),
    )
    .unwrap();
    assert_eq!(created.links, 1);
    let listing = Archive::open(&destination, ArchiveOptions::default())
        .unwrap()
        .list(&OpContext::silent())
        .unwrap();
    let stored = listing
        .entries
        .iter()
        .find(|entry| entry.meta.path == "project/latest.txt")
        .unwrap();
    assert_eq!(stored.meta.kind, EntryKind::Symlink);
    assert_eq!(stored.meta.link_target.as_deref(), Some("readme.txt"));
}

#[test]
fn the_threads_of_the_options_overrule_the_budget() {
    let root = TempDir::new().unwrap();
    let sources = sample(root.path());
    let destination = root.path().join("out.7z");
    let mut request = request(sources, &destination, ArchiveFormat::SevenZ);
    request.options.threads = 1;
    create(&request, &OpContext::silent()).unwrap();
    let unpacked = root.path().join("unpacked");
    unpack(&destination, &unpacked);
    assert_eq!(tree(&unpacked), tree(&root.path().join("work")));
}

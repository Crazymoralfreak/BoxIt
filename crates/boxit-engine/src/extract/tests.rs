use std::collections::BTreeMap;
use std::sync::Arc;

use boxit_core::{Interaction, Limits, Timestamp};
use tempfile::TempDir;

use super::*;
use crate::testing::{tree, MemoryArchive, Trouble};

fn request(destination: &Path) -> ExtractRequest {
    ExtractRequest {
        destination: destination.to_path_buf(),
        base: String::new(),
        selection: Selection::All,
        options: ExtractOptions::default(),
        archive_size: 1 << 20,
    }
}

fn with_conflicts(destination: &Path, conflicts: ConflictPolicy) -> ExtractRequest {
    let mut request = request(destination);
    request.options.conflicts = conflicts;
    request
}

fn run(archive: &mut MemoryArchive, request: &ExtractRequest) -> Result<Outcome> {
    extract(archive, request, &OpContext::silent())
}

fn names(folder: &Path) -> Vec<String> {
    tree(folder).into_keys().collect()
}

fn reasons(outcome: &Outcome) -> Vec<(&str, &str)> {
    outcome
        .skipped
        .iter()
        .map(|skipped| (skipped.path.as_str(), skipped.reason.code.as_str()))
        .collect()
}

fn sample() -> MemoryArchive {
    MemoryArchive::new()
        .folder("docs/")
        .file("docs/a.txt", b"alpha")
        .file("docs/deep/b.txt", b"beta")
        .file("c.txt", b"gamma")
        .folder("empty")
}

#[test]
fn a_new_destination_appears_complete_and_nothing_else_is_left() {
    let root = TempDir::new().unwrap();
    let destination = root.path().join("new").join("out");
    let outcome = run(&mut sample(), &request(&destination)).unwrap();
    assert_eq!(
        names(&destination),
        [
            "c.txt",
            "docs/",
            "docs/a.txt",
            "docs/deep/",
            "docs/deep/b.txt",
            "empty/"
        ]
    );
    assert_eq!(tree(&destination)["docs/deep/b.txt"], "beta");
    assert_eq!((outcome.files, outcome.folders, outcome.bytes), (3, 2, 14));
    assert_eq!(outcome.skipped_total, 0);
    // No temporary folder next to the destination.
    assert_eq!(
        names(root.path())
            .iter()
            .filter(|name| name.contains(STAGING_PREFIX))
            .count(),
        0
    );
}

#[test]
fn an_existing_destination_gets_the_new_files_next_to_its_own() {
    let root = TempDir::new().unwrap();
    fs::create_dir_all(root.path().join("docs")).unwrap();
    fs::write(root.path().join("docs/mine.txt"), b"mine").unwrap();
    run(&mut sample(), &request(root.path())).unwrap();
    assert_eq!(
        names(root.path()),
        [
            "c.txt",
            "docs/",
            "docs/a.txt",
            "docs/deep/",
            "docs/deep/b.txt",
            "docs/mine.txt",
            "empty/"
        ]
    );
}

#[test]
fn only_the_selection_is_extracted() {
    let root = TempDir::new().unwrap();
    let mut request = request(&root.path().join("out"));
    request.selection = Selection::paths(["docs/deep"]);
    let outcome = run(&mut sample(), &request).unwrap();
    assert_eq!(
        names(&request.destination),
        ["docs/", "docs/deep/", "docs/deep/b.txt"]
    );
    assert_eq!(outcome.files, 1);
}

#[test]
fn what_is_taken_from_a_folder_comes_without_the_folders_above_it() {
    let root = TempDir::new().unwrap();
    let mut archive = sample()
        .file("./docs/deep/more/d.txt", b"delta")
        .entry(
            EntryMeta {
                kind: EntryKind::Hardlink,
                link_target: Some(String::from("docs/deep/b.txt")),
                ..EntryMeta::file("docs/deep/same.txt", 0)
            },
            b"",
        )
        .entry(
            EntryMeta {
                kind: EntryKind::Hardlink,
                // A file of the same name stands inside the folder, and it
                // is not the one this link is another name of.
                link_target: Some(String::from("b.txt")),
                ..EntryMeta::file("docs/deep/other.txt", 0)
            },
            b"",
        );
    let mut request = request(&root.path().join("out"));
    request.base = String::from("docs/deep/");
    request.selection = Selection::indexes(vec![2, 5, 6, 7]);
    let outcome = run(&mut archive, &request).unwrap();
    assert_eq!(
        tree(&request.destination),
        BTreeMap::from([
            (String::from("b.txt"), String::from("beta")),
            (String::from("more/"), String::new()),
            (String::from("more/d.txt"), String::from("delta")),
            (String::from("same.txt"), String::from("beta")),
        ])
    );
    assert_eq!(
        reasons(&outcome),
        [("docs/deep/other.txt", "linkTargetMissing")]
    );

    // An entry that is not in the folder has no place in the destination.
    let mut request = self::request(&root.path().join("apart"));
    request.base = String::from("docs");
    let outcome = run(&mut sample(), &request).unwrap();
    assert_eq!(
        names(&request.destination),
        ["a.txt", "deep/", "deep/b.txt"]
    );
    assert_eq!(
        reasons(&outcome),
        [("c.txt", "outsideFolder"), ("empty", "outsideFolder")]
    );

    // A folder that climbs is no folder to take anything from.
    let mut request = self::request(&root.path().join("never"));
    request.base = String::from("docs/../..");
    assert_eq!(
        run(&mut sample(), &request).unwrap_err().code,
        "pathTraversal"
    );
    assert!(!request.destination.exists());
}

#[test]
fn times_of_files_and_folders_come_from_the_archive() {
    let root = TempDir::new().unwrap();
    let destination = root.path().join("out");
    let stamp = |seconds| Some(Timestamp::from_seconds(seconds));
    let mut archive = MemoryArchive::new()
        .entry(
            EntryMeta {
                modified: stamp(1_300_000_000),
                ..EntryMeta::directory("dir")
            },
            b"",
        )
        .entry(
            EntryMeta {
                modified: stamp(1_400_000_000),
                ..EntryMeta::file("dir/file.txt", 1)
            },
            b"x",
        );
    run(&mut archive, &request(&destination)).unwrap();
    let modified = |path: &str| {
        fsx::timestamp(
            fs::metadata(destination.join(path))
                .unwrap()
                .modified()
                .unwrap(),
        )
        .seconds
    };
    assert_eq!(modified("dir/file.txt"), 1_400_000_000);
    // Writing the file into the folder did not disturb the time of the folder.
    assert_eq!(modified("dir"), 1_300_000_000);
}

#[test]
fn names_that_leave_the_destination_are_left_out_and_reported() {
    let root = TempDir::new().unwrap();
    let destination = root.path().join("safe").join("out");
    let mut archive = MemoryArchive::new()
        .file("../evil.txt", b"x")
        .file("a/../../evil.txt", b"x")
        .file("/abs/evil.txt", b"x")
        .file("good.txt", b"ok")
        .file("nul\0name", b"x")
        .file("./", b"x");
    let outcome = run(&mut archive, &request(&destination)).unwrap();
    assert_eq!(names(&destination), ["good.txt"]);
    assert_eq!(
        reasons(&outcome),
        [
            ("../evil.txt", "pathTraversal"),
            ("a/../../evil.txt", "pathTraversal"),
            ("/abs/evil.txt", "absolutePath"),
            ("nul\0name", "invalidName"),
            ("./", "invalidName"),
        ]
    );
    assert_eq!(outcome.skipped_total, 5);
    // Nothing appeared anywhere above the destination.
    assert_eq!(
        names(root.path()),
        ["safe/", "safe/out/", "safe/out/good.txt"]
    );
}

#[cfg(windows)]
#[test]
fn windows_tricks_in_names_are_defused() {
    let root = TempDir::new().unwrap();
    let destination = root.path().join("out");
    let mut archive = MemoryArchive::new()
        .file("..\\evil.txt", b"x")
        .file("C:\\evil.txt", b"x")
        .file("aux.txt", b"device")
        .file("file.txt:stream", b"stream")
        .file("dots...", b"dots");
    let outcome = run(&mut archive, &request(&destination)).unwrap();
    assert_eq!(names(&destination), ["_aux.txt", "dots", "file.txt_stream"]);
    assert_eq!(
        reasons(&outcome),
        [
            ("..\\evil.txt", "pathTraversal"),
            ("C:\\evil.txt", "absolutePath")
        ]
    );
}

#[test]
fn a_damaged_archive_leaves_no_trace_in_a_new_destination() {
    let root = TempDir::new().unwrap();
    let destination = root.path().join("made").join("for").join("this");
    let error = run(
        &mut sample().trouble_at(3, Trouble::Fail),
        &request(&destination),
    )
    .unwrap_err();
    assert_eq!(error.code, "damaged");
    // Not the destination, not its parents, not the temporary folder.
    assert_eq!(names(root.path()), Vec::<String>::new());
}

#[test]
fn a_failure_leaves_an_existing_destination_as_it_was() {
    let root = TempDir::new().unwrap();
    fs::write(root.path().join("c.txt"), b"old").unwrap();
    fs::create_dir(root.path().join("docs")).unwrap();
    fs::write(root.path().join("docs/a.txt"), b"old a").unwrap();
    let before = tree(root.path());
    for trouble in [Trouble::Fail, Trouble::Cancel, Trouble::Panic] {
        for parallel in [false, true] {
            let mut archive = sample().trouble_at(4, trouble);
            archive.parallel = parallel;
            let request = with_conflicts(root.path(), ConflictPolicy::Overwrite);
            let error = run(&mut archive, &request).unwrap_err();
            let expected = match trouble {
                Trouble::Fail => "damaged",
                Trouble::Cancel => "cancelled",
                Trouble::Panic => "internal",
            };
            assert_eq!(error.code, expected);
            assert_eq!(
                tree(root.path()),
                before,
                "{trouble:?}, parallel: {parallel}"
            );
        }
    }
}

#[test]
fn existing_files_follow_the_conflict_policy() {
    let prepare = || {
        let root = TempDir::new().unwrap();
        fs::write(root.path().join("c.txt"), b"old").unwrap();
        fs::write(root.path().join(".hidden"), b"old").unwrap();
        root
    };
    let archive = || sample().file(".hidden", b"new").file("c.txt", b"second");

    let root = prepare();
    let outcome = run(
        &mut archive(),
        &with_conflicts(root.path(), ConflictPolicy::Skip),
    )
    .unwrap();
    assert_eq!(tree(root.path())["c.txt"], "old");
    assert_eq!(tree(root.path())["docs/a.txt"], "alpha");
    assert_eq!(
        reasons(&outcome),
        [
            ("c.txt", "exists"),
            (".hidden", "exists"),
            ("c.txt", "exists")
        ]
    );

    // Nobody to ask keeps the existing files as well.
    let root = prepare();
    run(
        &mut archive(),
        &with_conflicts(root.path(), ConflictPolicy::Ask),
    )
    .unwrap();
    assert_eq!(tree(root.path())["c.txt"], "old");

    let root = prepare();
    let outcome = run(
        &mut archive(),
        &with_conflicts(root.path(), ConflictPolicy::Overwrite),
    )
    .unwrap();
    // The archive holds the name twice: the later entry wins.
    assert_eq!(tree(root.path())["c.txt"], "second");
    assert_eq!(tree(root.path())[".hidden"], "new");
    assert_eq!(outcome.skipped_total, 0);
    assert_eq!(
        names(root.path())
            .iter()
            .filter(|name| name.contains(STAGING_PREFIX))
            .count(),
        0
    );

    let root = prepare();
    run(
        &mut archive(),
        &with_conflicts(root.path(), ConflictPolicy::Rename),
    )
    .unwrap();
    let files = tree(root.path());
    assert_eq!(files["c.txt"], "old");
    assert_eq!(files["c (2).txt"], "gamma");
    assert_eq!(files["c (3).txt"], "second");
    assert_eq!(files[".hidden (2)"], "new");
}

struct Answers {
    answer: ConflictAnswer,
    asked: Mutex<Vec<String>>,
}

impl Interaction for Answers {
    fn conflict(&self, conflict: &Conflict) -> ConflictAnswer {
        assert_eq!(conflict.existing_size, 3);
        lock(&self.asked).push(conflict.entry.path.clone());
        self.answer
    }

    fn symlink(&self, _link: &EntryMeta) -> bool {
        true
    }
}

fn ask(answer: ConflictAnswer) -> (Result<Outcome>, BTreeMap<String, String>, Vec<String>) {
    let root = TempDir::new().unwrap();
    fs::write(root.path().join("a.txt"), b"old").unwrap();
    fs::write(root.path().join("b.txt"), b"old").unwrap();
    let answers = Arc::new(Answers {
        answer,
        asked: Mutex::new(Vec::new()),
    });
    let ctx = OpContext {
        interaction: answers.clone(),
        ..OpContext::silent()
    };
    let mut archive = MemoryArchive::new()
        .file("a.txt", b"new a")
        .file("b.txt", b"new b");
    let result = extract(
        &mut archive,
        &with_conflicts(root.path(), ConflictPolicy::Ask),
        &ctx,
    );
    let asked = lock(&answers.asked).clone();
    (result, tree(root.path()), asked)
}

#[test]
fn the_user_is_asked_about_each_conflict_or_once_for_all() {
    let (result, files, asked) = ask(ConflictAnswer::Overwrite);
    assert_eq!(result.unwrap().files, 2);
    assert_eq!(
        (files["a.txt"].as_str(), files["b.txt"].as_str()),
        ("new a", "new b")
    );
    assert_eq!(asked, ["a.txt", "b.txt"]);

    let (result, files, asked) = ask(ConflictAnswer::OverwriteAll);
    assert!(result.is_ok());
    assert_eq!(files["b.txt"], "new b");
    assert_eq!(asked, ["a.txt"]);

    let (result, files, asked) = ask(ConflictAnswer::SkipAll);
    assert_eq!(result.unwrap().skipped_total, 2);
    assert_eq!(files["a.txt"], "old");
    assert_eq!(asked, ["a.txt"]);

    let (result, files, _) = ask(ConflictAnswer::RenameAll);
    assert!(result.is_ok());
    assert_eq!(files["a (2).txt"], "new a");
    assert_eq!(files["b (2).txt"], "new b");

    let (result, files, asked) = ask(ConflictAnswer::Cancel);
    assert!(result.unwrap_err().is_cancelled());
    assert_eq!(files.len(), 2);
    assert_eq!(asked, ["a.txt"]);
}

#[test]
fn a_file_in_the_way_of_a_folder_blocks_what_goes_inside() {
    let root = TempDir::new().unwrap();
    fs::write(root.path().join("docs"), b"a file, not a folder").unwrap();
    let outcome = run(
        &mut sample(),
        &with_conflicts(root.path(), ConflictPolicy::Skip),
    )
    .unwrap();
    assert_eq!(tree(root.path())["docs"], "a file, not a folder");
    assert_eq!(
        reasons(&outcome),
        [
            ("docs/", "blockedByFile"),
            ("docs/a.txt", "blockedByFile"),
            ("docs/deep/b.txt", "blockedByFile")
        ]
    );
    // Replacing was asked for: the folder takes the place of the file.
    run(
        &mut sample(),
        &with_conflicts(root.path(), ConflictPolicy::Overwrite),
    )
    .unwrap();
    assert_eq!(tree(root.path())["docs/a.txt"], "alpha");
}

#[test]
fn a_folder_is_not_replaced_by_a_file() {
    let root = TempDir::new().unwrap();
    fs::create_dir(root.path().join("c.txt")).unwrap();
    fs::write(root.path().join("c.txt/keep"), b"keep").unwrap();
    let outcome = run(
        &mut sample(),
        &with_conflicts(root.path(), ConflictPolicy::Overwrite),
    )
    .unwrap();
    assert_eq!(tree(root.path())["c.txt/keep"], "keep");
    assert_eq!(reasons(&outcome), [("c.txt", "existingFolder")]);
}

#[test]
fn a_merge_that_fails_half_way_is_undone() {
    let root = TempDir::new().unwrap();
    let from = root.path().join("from");
    let to = root.path().join("to");
    for (path, text) in [
        ("from/a.txt", "new a"),
        ("from/sub/b.txt", "new b"),
        ("to/a.txt", "old a"),
        ("to/keep.txt", "keep"),
    ] {
        let path = root.path().join(path);
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(path, text).unwrap();
    }
    let before = tree(&to);
    let mut journal = Journal::default();
    merge(&from, &to, &root.path().join("backup"), &mut journal).unwrap();
    assert_eq!(tree(&to)["a.txt"], "new a");
    assert_eq!(tree(&to)["sub/b.txt"], "new b");
    assert_eq!(journal.set_aside, 1);
    journal.undo();
    assert_eq!(tree(&to), before);
}

#[test]
fn links_are_left_out_unless_asked_for() {
    let root = TempDir::new().unwrap();
    let destination = root.path().join("out");
    let mut archive = MemoryArchive::new()
        .file("a.txt", b"a")
        .symlink("link", "a.txt");
    let outcome = run(&mut archive, &request(&destination)).unwrap();
    assert_eq!(names(&destination), ["a.txt"]);
    assert_eq!(reasons(&outcome), [("link", "symlinkSkipped")]);
}

fn with_links(destination: &Path) -> ExtractRequest {
    let mut request = request(destination);
    request.options.symlinks = SymlinkPolicy::Extract;
    request
}

#[test]
fn a_link_out_of_the_destination_is_refused_and_nothing_goes_through_a_link() {
    let root = TempDir::new().unwrap();
    let outside = root.path().join("outside");
    fs::create_dir(&outside).unwrap();
    let destination = root.path().join("out");
    let mut archive = MemoryArchive::new()
        .folder("dir")
        .symlink("escape", "../outside")
        .file("escape/evil.txt", b"x")
        .symlink("dir/up", "../..")
        .symlink("abs", outside.to_str().unwrap())
        .symlink("inside", "dir")
        .file("inside/through.txt", b"x")
        .file("dir/ok.txt", b"ok");
    let outcome = run(&mut archive, &with_links(&destination)).unwrap();
    assert_eq!(names(&outside), Vec::<String>::new());
    let reasons = reasons(&outcome);
    assert!(reasons.contains(&("escape", "symlinkEscape")));
    assert!(reasons.contains(&("dir/up", "symlinkEscape")));
    assert!(reasons.contains(&("abs", "symlinkEscape")));
    assert_eq!(tree(&destination)["dir/ok.txt"], "ok");
    if outcome.links == 1 {
        // The link was made; the file addressed through it was not written.
        assert!(reasons.contains(&("inside/through.txt", "symlinkParent")));
        assert!(!destination.join("dir").join("through.txt").exists());
        assert_eq!(fsx::existing(&destination.join("inside")), Existing::Link);
    } else {
        // This Windows account may not create links.
        assert!(reasons.contains(&("inside", "symlinkNotPermitted")));
    }
}

#[test]
fn a_link_is_created_when_the_user_agrees() {
    let root = TempDir::new().unwrap();
    let destination = root.path().join("out");
    let ctx = OpContext {
        interaction: Arc::new(Answers {
            answer: ConflictAnswer::Skip,
            asked: Mutex::new(Vec::new()),
        }),
        ..OpContext::silent()
    };
    let mut request = request(&destination);
    request.options.symlinks = SymlinkPolicy::Ask;
    let mut archive = MemoryArchive::new()
        .file("a.txt", b"content")
        .symlink("link.txt", "a.txt");
    let outcome = extract(&mut archive, &request, &ctx).unwrap();
    if outcome.links == 1 {
        assert_eq!(fs::read(destination.join("link.txt")).unwrap(), b"content");
    } else {
        assert_eq!(reasons(&outcome), [("link.txt", "symlinkNotPermitted")]);
    }
}

#[test]
fn a_hard_link_is_another_name_of_an_extracted_file() {
    let root = TempDir::new().unwrap();
    let destination = root.path().join("out");
    let hardlink = |path: &str, target: &str| EntryMeta {
        kind: EntryKind::Hardlink,
        link_target: Some(target.to_owned()),
        ..EntryMeta::file(path, 0)
    };
    let mut archive = MemoryArchive::new()
        .file("dir/a.txt", b"shared")
        .entry(hardlink("b.txt", "dir/a.txt"), b"")
        .entry(hardlink("c.txt", "missing.txt"), b"")
        .entry(hardlink("d.txt", "../outside.txt"), b"");
    let outcome = run(&mut archive, &request(&destination)).unwrap();
    assert_eq!(tree(&destination)["b.txt"], "shared");
    assert_eq!(
        reasons(&outcome),
        [
            ("c.txt", "linkTargetMissing"),
            ("d.txt", "linkTargetMissing")
        ]
    );
    assert_eq!(outcome.files, 2);
}

#[test]
fn an_archive_that_unpacks_too_much_is_stopped_and_undone() {
    let root = TempDir::new().unwrap();
    let destination = root.path().join("out");
    let zeros = vec![0u8; 300_000];
    let bomb = || {
        MemoryArchive::new()
            .file("small.txt", b"x")
            .file("zeros-1.bin", &zeros)
            .file("zeros-2.bin", &zeros)
    };
    let mut request = request(&destination);
    request.archive_size = 100;
    request.options.limits = Limits {
        max_ratio: 1000,
        ratio_floor: 0,
        ..Limits::default()
    };
    // The sizes are honest here, but the archive has no index to read them
    // from: the limit is hit while writing.
    let error = run(&mut bomb(), &request).unwrap_err();
    assert_eq!(error.code, "bombSuspected");
    assert!(!destination.exists());

    // With an index the declared sizes stop it before anything is written.
    let mut indexed = bomb();
    indexed.has_index = true;
    assert_eq!(
        run(&mut indexed, &request).unwrap_err().code,
        "bombSuspected"
    );

    // Declared sizes that lie do not help: the bytes are counted.
    let mut lying = bomb();
    lying.has_index = true;
    for (meta, _) in &mut lying.items {
        meta.size = Some(1);
    }
    assert_eq!(run(&mut lying, &request).unwrap_err().code, "bombSuspected");
    assert!(!destination.exists());

    request.options.limits = Limits {
        max_file_size: 200_000,
        ..Limits::default()
    };
    let error = run(&mut bomb(), &request).unwrap_err();
    assert_eq!(error.code, "fileTooLarge");
    assert_eq!(error.params["name"], "zeros-1.bin");
    assert_eq!(names(root.path()), Vec::<String>::new());

    request.options.limits = Limits::none();
    assert_eq!(run(&mut bomb(), &request).unwrap().bytes, 600_001);
}

#[test]
fn a_declared_size_is_only_a_hint() {
    let root = TempDir::new().unwrap();
    let destination = root.path().join("out");
    let data = vec![7u8; 3_000_000];
    let mut archive = MemoryArchive::new()
        .entry(EntryMeta::file("more-than-declared.bin", 2_000_000), &data)
        .entry(EntryMeta::file("less-than-declared.bin", 8_000_000), &data)
        .entry(
            EntryMeta {
                size: None,
                ..EntryMeta::file("unknown.bin", 0)
            },
            &data,
        );
    let outcome = run(&mut archive, &request(&destination)).unwrap();
    assert_eq!(outcome.bytes, 9_000_000);
    for name in [
        "more-than-declared.bin",
        "less-than-declared.bin",
        "unknown.bin",
    ] {
        assert_eq!(fs::read(destination.join(name)).unwrap(), data, "{name}");
    }
}

#[test]
fn paths_far_longer_than_260_characters_are_extracted() {
    let root = TempDir::new().unwrap();
    let destination = root.path().join("out");
    let deep = ["a-folder-with-a-name-of-forty-characters"; 10].join("/");
    let mut archive = MemoryArchive::new().file(&format!("{deep}/file.txt"), b"deep");
    let outcome = run(&mut archive, &request(&destination)).unwrap();
    assert_eq!(outcome.files, 1);
    let extracted = fsx::long(&destination)
        .unwrap()
        .join(deep.replace('/', std::path::MAIN_SEPARATOR_STR));
    assert_eq!(fs::read(extracted.join("file.txt")).unwrap(), b"deep");
    // And into an existing destination, through the merge.
    let mut archive = MemoryArchive::new().file(&format!("{deep}/other.txt"), b"more");
    run(&mut archive, &request(&destination)).unwrap();
    assert_eq!(fs::read(extracted.join("other.txt")).unwrap(), b"more");
}

#[test]
fn entries_arriving_from_several_threads_all_land() {
    let root = TempDir::new().unwrap();
    fs::write(root.path().join("file-0007.txt"), b"old").unwrap();
    let mut archive = MemoryArchive::new();
    archive.parallel = true;
    archive.has_index = true;
    for number in 0..400 {
        archive = archive.file(
            &format!("d{}/file-{number:04}.txt", number % 7),
            format!("{number}").as_bytes(),
        );
        archive = archive.file(&format!("file-{number:04}.txt"), b"top");
    }
    let outcome = run(
        &mut archive,
        &with_conflicts(root.path(), ConflictPolicy::Rename),
    )
    .unwrap();
    assert_eq!(outcome.files, 800);
    let files = tree(root.path());
    assert_eq!(files.len(), 801 + 7);
    assert_eq!(files["d3/file-0010.txt"], "10");
    assert_eq!(files["file-0007.txt"], "old");
    assert_eq!(files["file-0007 (2).txt"], "top");
}

#[test]
fn a_file_is_not_a_destination() {
    let root = TempDir::new().unwrap();
    let file = root.path().join("file.txt");
    fs::write(&file, b"x").unwrap();
    assert_eq!(
        run(&mut sample(), &request(&file)).unwrap_err().code,
        "badDestination"
    );
    assert_eq!(fs::read(&file).unwrap(), b"x");
}

#[test]
fn the_report_lists_the_first_left_out_entries_and_counts_them_all() {
    let root = TempDir::new().unwrap();
    let mut archive = MemoryArchive::new();
    for number in 0..SKIPPED_LISTED + 20 {
        archive = archive.file(&format!("../{number}"), b"x");
    }
    let outcome = run(&mut archive, &request(&root.path().join("out"))).unwrap();
    assert_eq!(outcome.skipped.len(), SKIPPED_LISTED);
    assert_eq!(outcome.skipped_total, SKIPPED_LISTED as u64 + 20);
}

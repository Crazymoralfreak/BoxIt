use std::collections::BTreeMap;
use std::fmt::Write;
use std::fs;
use std::path::Path;

use boxit_codecs::names::NameEncoding;
use boxit_core::ArchiveFormat;
use clap::error::ErrorKind;
use tempfile::TempDir;

use super::*;
use crate::args::{dictionary, format, volume};

/// Runs a command the way a script would: nobody to ask, JSON to read.
fn boxit(arguments: &[&str]) -> i32 {
    let host = Host {
        language: String::from("en"),
        prompts: false,
    };
    let mut line = vec!["boxit"];
    line.extend_from_slice(arguments);
    line.push("--json");
    run(line, &host)
}

/// Runs a command the way a person would, in their language.
fn typed(language: &str, arguments: &[&str]) -> i32 {
    let host = Host {
        language: language.to_owned(),
        prompts: false,
    };
    let mut line = vec!["boxit"];
    line.extend_from_slice(arguments);
    run(line, &host)
}

fn text_of(path: &Path) -> &str {
    path.to_str().unwrap()
}

/// A folder `project` with files in it, under `root`.
fn plant(root: &Path) -> String {
    let project = root.join("project");
    fs::create_dir_all(project.join("docs").join("empty")).unwrap();
    fs::write(project.join("readme.txt"), "read me").unwrap();
    fs::write(project.join("docs").join("пример.txt"), "привет").unwrap();
    let mut long = String::new();
    for line in 0..20_000 {
        writeln!(long, "{line:x} is the number of a line of this text").unwrap();
    }
    fs::write(project.join("docs").join("long.txt"), long).unwrap();
    text_of(&project).to_owned()
}

fn collect(root: &Path, folder: &Path, out: &mut BTreeMap<String, Vec<u8>>) {
    for entry in fs::read_dir(folder).unwrap() {
        let path = entry.unwrap().path();
        let name = path
            .strip_prefix(root)
            .unwrap()
            .to_string_lossy()
            .replace('\\', "/");
        if path.is_dir() {
            out.insert(format!("{name}/"), Vec::new());
            collect(root, &path, out);
        } else {
            out.insert(name, fs::read(&path).unwrap());
        }
    }
}

/// Every file and folder under `folder`, by relative path.
fn tree(folder: &Path) -> BTreeMap<String, Vec<u8>> {
    let mut out = BTreeMap::new();
    collect(folder, folder, &mut out);
    out
}

#[test]
fn the_command_line_is_well_formed() {
    Cli::command().debug_assert();
}

#[test]
fn commands_are_told_from_files() {
    for command in [
        "list", "extract", "create", "test", "repair", "bench", "password", "--help", "-V",
    ] {
        assert!(is_command(command), "{command}");
    }
    for file in [
        "C:\\archives\\list.zip",
        "photos.7z",
        "List",
        "",
        "--portable",
    ] {
        assert!(!is_command(file), "{file}");
    }
}

#[test]
fn help_is_not_a_failure_and_nonsense_is() {
    let parse = |arguments: &[&str]| Cli::try_parse_from(arguments).map(|_| ()).unwrap_err();
    assert_eq!(parse(&["boxit", "--help"]).kind(), ErrorKind::DisplayHelp);
    assert!(!parse(&["boxit", "--version"]).use_stderr());
    assert!(parse(&["boxit"]).use_stderr());
    assert!(parse(&["boxit", "explode", "a.zip"]).use_stderr());
    assert!(parse(&["boxit", "create", "a.zip"]).use_stderr());
    assert!(parse(&["boxit", "create", "a.zip", "x", "--level", "11"]).use_stderr());
    assert!(parse(&["boxit", "create", "a.zip", "x", "-p", "1", "--ask-password"]).use_stderr());
    assert!(parse(&["boxit", "list", "a.zip", "--encoding", "klingon"]).use_stderr());
}

#[test]
fn values_are_read_the_way_people_write_them() {
    assert_eq!(format("7z"), Ok(ArchiveFormat::SevenZ));
    assert_eq!(format(".TGZ"), Ok(ArchiveFormat::TarGz));
    assert_eq!(format("tar.zst"), Ok(ArchiveFormat::TarZst));
    // Read, but not created.
    assert!(format("rar").unwrap_err().contains("tar.xz"));
    assert!(format("docx").is_err());

    assert_eq!(dictionary("64k"), Ok(64 << 10));
    assert_eq!(dictionary("16M"), Ok(16 << 20));
    assert_eq!(dictionary("1gb"), Ok(1 << 30));
    assert_eq!(dictionary("131072"), Ok(131_072));
    for wrong in ["", "k", "63k", "2g", "-1m", "1.5m", "99999999999999g"] {
        assert!(dictionary(wrong).is_err(), "{wrong}");
    }
    assert_eq!(volume("64k"), Ok(64 << 10));
    assert_eq!(volume("700M"), Ok(700 << 20));
    assert_eq!(volume("4g"), Ok(4 << 30));
    for wrong in ["", "m", "63k", "0", "-1m", "99999999999999g"] {
        assert!(volume(wrong).is_err(), "{wrong}");
    }

    let cli = Cli::try_parse_from([
        "boxit",
        "extract",
        "a.zip",
        "docs",
        "readme.txt",
        "-o",
        "out",
        "--encoding",
        "IBM866",
    ])
    .unwrap();
    let Command::Extract(arguments) = cli.command else {
        panic!("another command");
    };
    assert_eq!(arguments.paths, ["docs", "readme.txt"]);
    assert_eq!(
        arguments.read.encoding,
        NameEncoding::from_label("ibm866").unwrap()
    );
    assert_eq!(arguments.conflicts, ConflictArg::Ask);
    assert_eq!(arguments.symlinks, SymlinkArg::Skip);
}

#[test]
fn an_archive_goes_round_through_the_commands() {
    let root = TempDir::new().unwrap();
    let project = plant(root.path());
    let wanted = tree(root.path());
    for name in ["a.zip", "a.7z", "a.tar.gz", "a.tar.zst"] {
        let archive = root.path().join("archives").join(name);
        let archive = text_of(&archive);
        assert_eq!(boxit(&["create", archive, &project]), EXIT_OK, "{name}");
        assert_eq!(boxit(&["list", archive]), EXIT_OK, "{name}");
        assert_eq!(boxit(&["test", archive]), EXIT_OK, "{name}");
        let unpacked = root.path().join(format!("{name}.out"));
        assert_eq!(
            boxit(&["extract", archive, "-o", text_of(&unpacked)]),
            EXIT_OK,
            "{name}"
        );
        assert_eq!(tree(&unpacked), wanted, "{name}");
    }
}

#[test]
fn an_archive_is_cut_into_volumes_and_read_from_them() {
    let root = TempDir::new().unwrap();
    let project = plant(root.path());
    let wanted = tree(root.path());
    for name in ["a.zip", "a.7z", "a.tar"] {
        let archive = root.path().join("volumes").join(name);
        let volume = |number: u32| format!("{}.{number:03}", text_of(&archive));
        assert_eq!(
            boxit(&[
                "create",
                text_of(&archive),
                &project,
                "--volume",
                "64k",
                "--level",
                "store"
            ]),
            EXIT_OK,
            "{name}"
        );
        // The text is a megabyte: more than ten volumes, and no file under
        // the name of the archive itself.
        assert!(Path::new(&volume(10)).is_file(), "{name}");
        assert!(!archive.exists(), "{name}");
        assert_eq!(fs::metadata(volume(1)).unwrap().len(), 64 << 10, "{name}");
        // They are not replaced unasked.
        assert_ne!(
            boxit(&["create", text_of(&archive), &project, "-v", "64k"]),
            EXIT_OK,
            "{name}"
        );
        assert_eq!(typed("ru", &["list", &volume(1)]), EXIT_OK, "{name}");
        assert_eq!(boxit(&["test", &volume(3)]), EXIT_OK, "{name}");
        let unpacked = root.path().join(format!("{name}.out"));
        assert_eq!(
            boxit(&["extract", &volume(2), "-o", text_of(&unpacked)]),
            EXIT_OK,
            "{name}"
        );
        assert_eq!(tree(&unpacked), wanted, "{name}");
        // Without a volume the archive is not whole, and says so.
        fs::remove_file(volume(1)).unwrap();
        assert_ne!(boxit(&["list", &volume(2)]), EXIT_OK, "{name}");
    }
}

#[test]
fn the_commands_talk_to_people_in_both_languages() {
    let root = TempDir::new().unwrap();
    let project = plant(root.path());
    let wanted = tree(root.path());
    for language in ["en", "ru"] {
        let archive = root.path().join(format!("{language}.7z"));
        let archive = text_of(&archive);
        let unpacked = root.path().join(format!("{language}.out"));
        let unpacked = text_of(&unpacked);
        let create = ["create", archive, &project, "-p", "correct horse"];
        assert_eq!(typed(language, &create), EXIT_OK);
        let known = ["-p", "correct horse"];
        assert_eq!(
            typed(language, &[&["list", archive], &known[..]].concat()),
            EXIT_OK
        );
        assert_eq!(
            typed(language, &[&["test", archive], &known[..]].concat()),
            EXIT_OK
        );
        let extract = [&["extract", archive, "-o", unpacked], &known[..]].concat();
        assert_eq!(typed(language, &extract), EXIT_OK);
        assert_eq!(tree(Path::new(unpacked)), wanted);
        // The files are there now, and there is nobody to ask about them.
        assert_eq!(typed(language, &extract), EXIT_PARTIAL);
        let bench = ["bench", "-f", "zip", "--set", "mixed", "--runs", "1"];
        assert_eq!(typed(language, &bench), EXIT_OK);
    }
}

#[test]
fn an_existing_archive_is_kept_unless_told_otherwise() {
    let root = TempDir::new().unwrap();
    let project = plant(root.path());
    let archive = root.path().join("a.zip");
    let archive = text_of(&archive);
    assert_eq!(boxit(&["create", archive, &project]), EXIT_OK);
    let first = fs::read(archive).unwrap();
    assert_eq!(
        boxit(&["create", archive, &project, "--level", "store"]),
        EXIT_FAILED
    );
    assert_eq!(fs::read(archive).unwrap(), first);
    assert_eq!(
        boxit(&[
            "create",
            archive,
            &project,
            "--level",
            "store",
            "--overwrite"
        ]),
        EXIT_OK
    );
    assert!(fs::read(archive).unwrap().len() > first.len());
}

#[test]
fn the_format_comes_from_the_name_or_from_the_option() {
    let root = TempDir::new().unwrap();
    let project = plant(root.path());
    let archive = root.path().join("backup.bin");
    let archive = text_of(&archive);
    assert_eq!(boxit(&["create", archive, &project]), EXIT_FAILED);
    assert!(!Path::new(archive).exists());
    assert_eq!(boxit(&["create", archive, &project, "-f", "7z"]), EXIT_OK);
    // What the file is, is told by its content.
    assert_eq!(boxit(&["test", archive]), EXIT_OK);
    // A format BoxIt only reads is not one to create.
    let rar = root.path().join("backup.rar");
    assert_eq!(boxit(&["create", text_of(&rar), &project]), EXIT_FAILED);
}

#[test]
fn files_that_exist_are_kept_and_the_exit_code_says_so() {
    let root = TempDir::new().unwrap();
    let project = plant(root.path());
    let archive = root.path().join("a.zip");
    let archive = text_of(&archive);
    assert_eq!(boxit(&["create", archive, &project]), EXIT_OK);
    let unpacked = root.path().join("out");
    let readme = unpacked.join("project").join("readme.txt");
    assert_eq!(
        boxit(&["extract", archive, "-o", text_of(&unpacked)]),
        EXIT_OK
    );

    fs::write(&readme, "mine").unwrap();
    // Nobody to ask: what is on the disk stays.
    assert_eq!(
        boxit(&["extract", archive, "-o", text_of(&unpacked)]),
        EXIT_PARTIAL
    );
    assert_eq!(fs::read(&readme).unwrap(), b"mine");
    assert_eq!(
        boxit(&[
            "extract",
            archive,
            "-o",
            text_of(&unpacked),
            "--conflicts",
            "overwrite"
        ]),
        EXIT_OK
    );
    assert_eq!(fs::read(&readme).unwrap(), b"read me");
}

#[test]
fn a_part_of_an_archive_is_extracted_by_its_path() {
    let root = TempDir::new().unwrap();
    let project = plant(root.path());
    let archive = root.path().join("a.7z");
    let archive = text_of(&archive);
    assert_eq!(boxit(&["create", archive, &project]), EXIT_OK);

    let unpacked = root.path().join("out");
    // Either slash names the folder.
    assert_eq!(
        boxit(&[
            "extract",
            archive,
            "project\\docs",
            "-o",
            text_of(&unpacked)
        ]),
        EXIT_OK
    );
    let got = tree(&unpacked);
    assert!(
        got.contains_key("project/docs/long.txt"),
        "{:?}",
        got.keys()
    );
    assert!(!got.contains_key("project/readme.txt"), "{:?}", got.keys());

    let nothing = root.path().join("nothing");
    assert_eq!(
        boxit(&["extract", archive, "no/such/file", "-o", text_of(&nothing)]),
        EXIT_FAILED
    );
    assert!(!nothing.exists());
    assert_eq!(boxit(&["test", archive, "project/readme.txt"]), EXIT_OK);
    assert_eq!(boxit(&["test", archive, "no/such/file"]), EXIT_FAILED);
}

#[test]
fn a_locked_archive_takes_its_password_and_no_other() {
    let root = TempDir::new().unwrap();
    let project = plant(root.path());
    for (name, hidden) in [("a.zip", false), ("a.7z", false), ("hidden.7z", true)] {
        let archive = root.path().join(name);
        let archive = text_of(&archive);
        let mut create = vec!["create", archive, &project, "-p", "correct horse"];
        if hidden {
            create.push("--encrypt-names");
        }
        assert_eq!(boxit(&create), EXIT_OK, "{name}");

        assert_eq!(boxit(&["test", archive]), EXIT_FAILED, "{name}");
        assert_eq!(
            boxit(&["test", archive, "-p", "wrong"]),
            EXIT_FAILED,
            "{name}"
        );
        assert_eq!(
            boxit(&["test", archive, "-p", "correct horse"]),
            EXIT_OK,
            "{name}"
        );
        // The names are a secret only where the archive hides them.
        let listed = if hidden { EXIT_FAILED } else { EXIT_OK };
        assert_eq!(boxit(&["list", archive]), listed, "{name}");
        assert_eq!(
            boxit(&["list", archive, "-p", "correct horse"]),
            EXIT_OK,
            "{name}"
        );

        let unpacked = root.path().join(format!("{name}.out"));
        assert_eq!(
            boxit(&["extract", archive, "-o", text_of(&unpacked)]),
            EXIT_FAILED,
            "{name}"
        );
        assert!(!unpacked.exists(), "{name}");
    }
    // A password is asked for only where one can be typed.
    let asked = root.path().join("asked.zip");
    assert_eq!(
        boxit(&["create", text_of(&asked), &project, "--ask-password"]),
        EXIT_FAILED
    );
    // And a format that cannot encrypt does not pretend to.
    let tar = root.path().join("a.tar");
    assert_eq!(
        boxit(&["create", text_of(&tar), &project, "-p", "correct horse"]),
        EXIT_FAILED
    );
}

#[test]
fn damage_is_found_by_the_test() {
    let root = TempDir::new().unwrap();
    let project = plant(root.path());
    let archive = root.path().join("a.zip");
    assert_eq!(boxit(&["create", text_of(&archive), &project]), EXIT_OK);
    let mut bytes = fs::read(&archive).unwrap();
    let middle = bytes.len() / 2;
    for byte in &mut bytes[middle..middle + 64] {
        *byte ^= 0xFF;
    }
    fs::write(&archive, bytes).unwrap();
    assert_eq!(boxit(&["test", text_of(&archive)]), EXIT_FAILED);
    // For a person the damaged file is named, in either language.
    assert_eq!(typed("ru", &["test", text_of(&archive)]), EXIT_FAILED);
    assert_eq!(
        typed("en", &["test", text_of(&archive), "--sums"]),
        EXIT_FAILED
    );

    let whole = root.path().join("whole.7z");
    assert_eq!(boxit(&["create", text_of(&whole), &project]), EXIT_OK);
    assert_eq!(boxit(&["test", text_of(&whole), "--sums"]), EXIT_OK);
    assert_eq!(typed("ru", &["test", text_of(&whole), "--sums"]), EXIT_OK);

    let missing = root.path().join("missing.zip");
    assert_eq!(boxit(&["test", text_of(&missing)]), EXIT_FAILED);
    assert_eq!(boxit(&["list", text_of(&missing)]), EXIT_FAILED);
    let not_an_archive = root.path().join("project").join("readme.txt");
    assert_eq!(boxit(&["list", text_of(&not_an_archive)]), EXIT_FAILED);
}

#[test]
fn a_password_is_drawn_and_a_bad_length_is_refused() {
    assert_eq!(typed("en", &["password"]), EXIT_OK);
    assert_eq!(
        typed("en", &["password", "--length", "12", "--symbols"]),
        EXIT_OK
    );
    assert_eq!(typed("en", &["password", "--length", "3"]), EXIT_USAGE);
    assert_eq!(typed("en", &["password", "--length", "1000"]), EXIT_USAGE);
}

#[test]
fn a_damaged_archive_is_repaired_into_a_new_one() {
    let root = TempDir::new().unwrap();
    let project = plant(root.path());
    let archive = root.path().join("broken.zip");
    assert_eq!(
        boxit(&["create", text_of(&archive), &project, "--level", "store"]),
        EXIT_OK
    );
    let mut bytes = fs::read(&archive).unwrap();
    let end = bytes
        .windows(4)
        .rposition(|window| window == b"PK\x05\x06")
        .unwrap();
    bytes.truncate(end);
    fs::write(&archive, &bytes).unwrap();
    assert_eq!(boxit(&["test", text_of(&archive)]), EXIT_FAILED);

    let fixed = root.path().join("fixed.zip");
    assert_eq!(
        boxit(&["repair", text_of(&archive), "-o", text_of(&fixed)]),
        EXIT_OK
    );
    assert_eq!(fs::read(&archive).unwrap(), bytes);
    let unpacked = root.path().join("out");
    assert_eq!(
        boxit(&["extract", text_of(&fixed), "-o", text_of(&unpacked)]),
        EXIT_OK
    );
    assert_eq!(tree(&unpacked.join("project")), tree(Path::new(&project)));

    let tar = root.path().join("a.tar");
    assert_eq!(boxit(&["create", text_of(&tar), &project]), EXIT_OK);
    assert_eq!(boxit(&["repair", text_of(&tar)]), EXIT_FAILED);
}

#[test]
fn the_benchmark_measures_and_leaves_nothing_behind() {
    let root = TempDir::new().unwrap();
    let folder = root.path().join("bench");
    let code = boxit(&[
        "bench",
        "--formats",
        "zip,tar.zst,gz",
        "--set",
        "mixed,one-large",
        "--runs",
        "1",
        "--folder",
        text_of(&folder),
    ]);
    assert_eq!(code, EXIT_OK);
    assert_eq!(fs::read_dir(&folder).unwrap().count(), 0);
}

#[test]
fn the_corpus_is_written_out_for_other_archivers() {
    let root = TempDir::new().unwrap();
    let folder = root.path().join("corpus");
    assert_eq!(
        boxit(&["bench", "--corpus", text_of(&folder), "--set", "mixed"]),
        EXIT_OK
    );
    let files = tree(&folder);
    assert!(files.keys().all(|name| name.starts_with("mixed/")));
    assert_eq!(
        files.values().filter(|content| !content.is_empty()).count(),
        48
    );
}

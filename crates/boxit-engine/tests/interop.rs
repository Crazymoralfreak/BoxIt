//! BoxIt and the archivers people already have read each other's archives:
//! 7-Zip, and the `tar` of the system. The latter is GNU tar on Linux and
//! the one of libarchive on Windows and macOS, which reads and writes ZIP
//! and 7z as well.
//!
//! The formats that are only read are checked the same way against the
//! programs that write them: the tar of libarchive (cpio, ar, disc images),
//! `makecab` and DISM of Windows, 7-Zip and wimlib (images of Windows).
//!
//! The tests with 7-Zip need it on the PATH (`7z`, `7zz` or `7za`), and
//! the others their programs. On a machine without one of them the tests of
//! it check nothing and say so, unless `BOXIT_INTEROP` names the program
//! among those the machine is supposed to have, as the CI does: there a
//! missing program is a failure. The names are the ones [`missing`] is
//! called with, separated by commas.

use std::collections::BTreeMap;
use std::env;
use std::fmt::Write;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use boxit_codecs::OpenOptions;
use boxit_core::{
    ArchiveFormat, CreateOptions, ExtractOptions, Method, OpContext, Password, Selection,
};
use boxit_engine::archive::{Archive, TestOptions};
use boxit_engine::create::{create, CreateRequest, Layout};
use tempfile::TempDir;

const PASSWORD: &str = "correct horse";

/// The number of files [`plant`] makes.
const FILES: u64 = 6;

/// A temporary folder the system `tar` can archive. The usual temporary
/// folder on a CI machine, plus the deep names [`plant`] adds, is longer
/// than the paths that `tar` of Windows will open.
fn scratch() -> TempDir {
    #[cfg(windows)]
    {
        let short = Path::new(r"C:\b");
        fs::create_dir_all(short).unwrap();
        return tempfile::Builder::new()
            .prefix("t")
            .tempdir_in(short)
            .unwrap();
    }
    #[cfg(not(windows))]
    TempDir::new().unwrap()
}

/// Says that a program is missing: a failure where it is required.
fn missing(program: &str) {
    let required = env::var("BOXIT_INTEROP").unwrap_or_default();
    assert!(
        !required.split(',').any(|name| name.trim() == program),
        "{program} was not found, and BOXIT_INTEROP says it is here"
    );
    eprintln!("{program} was not found: nothing was checked");
}

/// Whether a program of this name can be started.
fn runs(program: &str, arguments: &[&str]) -> bool {
    Command::new(program)
        .args(arguments)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .is_ok()
}

/// The command of 7-Zip on this machine.
fn seven_zip() -> Option<&'static str> {
    let found = ["7z", "7zz", "7za"]
        .into_iter()
        .find(|name| runs(name, &[]));
    if found.is_none() {
        missing("7-zip");
    }
    found
}

/// The tar of libarchive on this machine: the `tar` of Windows and macOS,
/// a program of its own name on Linux.
fn bsdtar() -> Option<&'static str> {
    let found = ["bsdtar", "tar"].into_iter().find(|name| {
        Command::new(name)
            .arg("--version")
            .output()
            .is_ok_and(|output| String::from_utf8_lossy(&output.stdout).contains("bsdtar"))
    });
    if found.is_none() {
        missing("bsdtar");
    }
    found
}

/// What the `tar` of this machine can do.
struct SystemTar {
    /// The tar of libarchive: ZIP and 7z are archives to it like any other.
    libarchive: bool,
    /// It reads and writes zstd.
    zstd: bool,
}

fn system_tar() -> Option<SystemTar> {
    let Ok(output) = Command::new("tar").arg("--version").output() else {
        missing("tar");
        return None;
    };
    let version = String::from_utf8_lossy(&output.stdout).into_owned();
    let libarchive = version.contains("bsdtar");
    // GNU tar hands the compression to a program of that name.
    let zstd = if libarchive {
        version.contains("libzstd")
    } else {
        runs("zstd", &["--version"])
    };
    Some(SystemTar { libarchive, zstd })
}

/// Runs a program in `folder` and fails the test with its output if it
/// fails.
fn run(program: &str, folder: &Path, arguments: &[&str]) {
    let output = Command::new(program)
        .current_dir(folder)
        .args(arguments)
        .stdin(Stdio::null())
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{program} {arguments:?} failed ({}):\n{}\n{}",
        output.status,
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
}

/// A folder `project` under `root`: text in two alphabets, a megabyte that
/// compresses, bytes that do not, a file without content, an empty folder.
fn plant(root: &Path) -> PathBuf {
    let project = root.join("project");
    fs::create_dir_all(project.join("docs")).unwrap();
    fs::create_dir_all(project.join("empty")).unwrap();
    let mut text = String::new();
    for line in 0..20_000 {
        writeln!(text, "{line:x} is the number of a line of this text").unwrap();
    }
    // A sequence that no compression makes shorter, the same on every run.
    let mut state = 0x2545_F491_4F6C_DD1D_u64;
    let noise: Vec<u8> = (0..200_000)
        .map(|_| {
            state ^= state << 13;
            state ^= state >> 7;
            state ^= state << 17;
            (state >> 24) as u8
        })
        .collect();
    // A path longer than the hundred bytes the header of a tar has for it.
    let deep = project
        .join("docs")
        .join("a folder with a name of some fifty characters, or so")
        .join("and another folder with a name just as long inside");
    fs::create_dir_all(&deep).unwrap();
    fs::write(deep.join("deep.txt"), "deep").unwrap();
    fs::write(project.join("readme.txt"), "read me").unwrap();
    fs::write(project.join("docs").join("пример текста.txt"), "привет").unwrap();
    fs::write(project.join("docs").join("long.txt"), text).unwrap();
    fs::write(project.join("noise.bin"), noise).unwrap();
    fs::write(project.join("nothing.txt"), "").unwrap();
    project
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

/// Compares two trees by names first, so a failure shows what is missing
/// and not a megabyte of bytes.
fn assert_same(got: &BTreeMap<String, Vec<u8>>, wanted: &BTreeMap<String, Vec<u8>>, label: &str) {
    assert_eq!(
        got.keys().collect::<Vec<_>>(),
        wanted.keys().collect::<Vec<_>>(),
        "{label}: names"
    );
    for (name, content) in wanted {
        assert!(got[name] == *content, "{label}: content of {name}");
    }
}

/// The only file of a folder: what 7-Zip takes out of a compressed tar.
fn only_file(folder: &Path) -> PathBuf {
    let mut files = fs::read_dir(folder)
        .unwrap()
        .map(|entry| entry.unwrap().path());
    let file = files.next().expect("the folder is empty");
    assert!(files.next().is_none(), "more than one file in the folder");
    file
}

fn options(
    format: ArchiveFormat,
    method: Option<Method>,
    locked: bool,
    names: bool,
) -> CreateOptions {
    CreateOptions {
        method,
        password: locked.then(|| Password::from(PASSWORD)),
        encrypt_names: names,
        ..CreateOptions::new(format)
    }
}

#[test]
fn seven_zip_reads_what_boxit_writes() {
    let Some(seven_zip) = seven_zip() else {
        return;
    };
    let root = scratch();
    let project = plant(root.path());
    // The project alone is under the root yet.
    let wanted = tree(root.path());
    let cases = [
        ("plain.zip", options(ArchiveFormat::Zip, None, false, false)),
        (
            "stored.zip",
            options(ArchiveFormat::Zip, Some(Method::Store), false, false),
        ),
        (
            "bzip2.zip",
            options(ArchiveFormat::Zip, Some(Method::Bzip2), false, false),
        ),
        ("locked.zip", options(ArchiveFormat::Zip, None, true, false)),
        (
            "plain.7z",
            options(ArchiveFormat::SevenZ, None, false, false),
        ),
        (
            "apart.7z",
            CreateOptions {
                solid: false,
                ..options(ArchiveFormat::SevenZ, None, false, false)
            },
        ),
        (
            "lzma.7z",
            options(ArchiveFormat::SevenZ, Some(Method::Lzma), false, false),
        ),
        (
            "ppmd.7z",
            options(ArchiveFormat::SevenZ, Some(Method::Ppmd), false, false),
        ),
        (
            "bzip2.7z",
            options(ArchiveFormat::SevenZ, Some(Method::Bzip2), false, false),
        ),
        (
            "deflate.7z",
            options(ArchiveFormat::SevenZ, Some(Method::Deflate), false, false),
        ),
        (
            "locked.7z",
            options(ArchiveFormat::SevenZ, None, true, false),
        ),
        (
            "hidden.7z",
            options(ArchiveFormat::SevenZ, None, true, true),
        ),
        ("plain.tar", options(ArchiveFormat::Tar, None, false, false)),
        (
            "plain.tar.gz",
            options(ArchiveFormat::TarGz, None, false, false),
        ),
        (
            "plain.tar.bz2",
            options(ArchiveFormat::TarBz2, None, false, false),
        ),
        (
            "plain.tar.xz",
            options(ArchiveFormat::TarXz, None, false, false),
        ),
    ];
    let archives = root.path().join("archives");
    for (name, options) in cases {
        let format = options.format;
        let archive = archives.join(name);
        let request = CreateRequest {
            sources: vec![project.clone()],
            destination: archive.clone(),
            options,
            overwrite: false,
            layout: Layout::Whole,
        };
        create(&request, &OpContext::silent()).unwrap_or_else(|error| panic!("{name}: {error}"));

        let password = format!("-p{PASSWORD}");
        let archive_text = archive.to_string_lossy().into_owned();
        run(seven_zip, &archives, &["t", &password, &archive_text]);

        let mut inner = archive_text;
        if format.stream().is_some() {
            // The compression comes off first and leaves a tar.
            let step = root.path().join(format!("{name}.step"));
            let output = format!("-o{}", step.to_string_lossy());
            run(seven_zip, &archives, &["x", "-y", &output, &inner]);
            inner = only_file(&step).to_string_lossy().into_owned();
        }
        let unpacked = root.path().join(format!("{name}.out"));
        let output = format!("-o{}", unpacked.to_string_lossy());
        run(
            seven_zip,
            &archives,
            &["x", "-y", &password, &output, &inner],
        );
        assert_same(&tree(&unpacked), &wanted, name);
    }
}

#[test]
fn boxit_reads_what_seven_zip_writes() {
    let Some(seven_zip) = seven_zip() else {
        return;
    };
    let root = scratch();
    plant(root.path());
    // The project alone is under the root yet.
    let wanted = tree(root.path());
    let password = format!("-p{PASSWORD}");
    // The name of the archive, whether it is locked, and what 7-Zip is told.
    let cases: [(&str, bool, &[&str]); 16] = [
        ("deflate.zip", false, &["-tzip"]),
        ("stored.zip", false, &["-tzip", "-mx=0"]),
        ("deflate64.zip", false, &["-tzip", "-mm=Deflate64"]),
        ("bzip2.zip", false, &["-tzip", "-mm=BZip2"]),
        ("lzma.zip", false, &["-tzip", "-mm=LZMA"]),
        ("ppmd.zip", false, &["-tzip", "-mm=PPMd"]),
        ("aes.zip", true, &["-tzip", "-mem=AES256", &password]),
        (
            "zipcrypto.zip",
            true,
            &["-tzip", "-mem=ZipCrypto", &password],
        ),
        ("lzma2.7z", false, &["-t7z"]),
        ("apart.7z", false, &["-t7z", "-ms=off"]),
        ("lzma.7z", false, &["-t7z", "-m0=LZMA"]),
        ("ppmd.7z", false, &["-t7z", "-m0=PPMd"]),
        ("bzip2.7z", false, &["-t7z", "-m0=BZip2"]),
        ("locked.7z", true, &["-t7z", &password]),
        ("hidden.7z", true, &["-t7z", "-mhe=on", &password]),
        ("plain.tar", false, &["-ttar"]),
    ];
    let mut archives: Vec<(String, bool)> = Vec::new();
    for (name, locked, switches) in cases {
        let mut arguments = vec!["a"];
        arguments.extend_from_slice(switches);
        arguments.extend_from_slice(&[name, "project"]);
        run(seven_zip, root.path(), &arguments);
        archives.push((name.to_owned(), locked));
    }
    // A tar inside each of the compressions 7-Zip writes.
    for (name, kind) in [
        ("plain.tar.gz", "-tgzip"),
        ("plain.tar.bz2", "-tbzip2"),
        ("plain.tar.xz", "-txz"),
    ] {
        run(seven_zip, root.path(), &["a", kind, name, "plain.tar"]);
        archives.push((name.to_owned(), false));
    }

    for (name, locked) in archives {
        boxit_reads(root.path(), &name, locked, &wanted);
    }
}

#[test]
fn volumes_are_cut_the_way_seven_zip_cuts_them() {
    let Some(seven_zip) = seven_zip() else {
        return;
    };
    let root = scratch();
    let project = plant(root.path());
    // The project alone is under the root yet.
    let wanted = tree(root.path());

    // The volumes of BoxIt, read by 7-Zip from the first of them.
    let archives = root.path().join("archives");
    for (name, format) in [
        ("cut.zip", ArchiveFormat::Zip),
        ("cut.7z", ArchiveFormat::SevenZ),
        ("cut.tar", ArchiveFormat::Tar),
    ] {
        let request = CreateRequest {
            layout: Layout::Volumes(64 * 1024),
            ..CreateRequest::new(
                vec![project.clone()],
                archives.join(name),
                options(format, None, false, false),
            )
        };
        let created = create(&request, &OpContext::silent())
            .unwrap_or_else(|error| panic!("{name}: {error}"));
        assert!(created.volumes > 2, "{name}: {}", created.volumes);
        let first = format!("{name}.001");
        run(seven_zip, &archives, &["t", &first]);
        let unpacked = root.path().join(format!("{name}.out"));
        let output = format!("-o{}", unpacked.to_string_lossy());
        run(seven_zip, &archives, &["x", "-y", &output, &first]);
        assert_same(&tree(&unpacked), &wanted, name);
    }

    // The volumes of 7-Zip, read by BoxIt from the first and from another.
    for (name, kind) in [
        ("theirs.7z", "-t7z"),
        ("theirs.zip", "-tzip"),
        ("theirs.tar", "-ttar"),
    ] {
        run(
            seven_zip,
            root.path(),
            &["a", kind, "-v64k", name, "project"],
        );
        assert!(root.path().join(format!("{name}.003")).is_file(), "{name}");
        boxit_reads(root.path(), &format!("{name}.001"), false, &wanted);
        boxit_reads(root.path(), &format!("{name}.002"), false, &wanted);
    }
}

#[test]
fn the_tar_of_the_system_reads_what_boxit_writes() {
    let Some(tar) = system_tar() else {
        return;
    };
    let root = scratch();
    let project = plant(root.path());
    // The project alone is under the root yet.
    let wanted = tree(root.path());
    let mut cases = vec![
        ("plain.tar", options(ArchiveFormat::Tar, None, false, false)),
        (
            "plain.tar.gz",
            options(ArchiveFormat::TarGz, None, false, false),
        ),
        (
            "plain.tar.bz2",
            options(ArchiveFormat::TarBz2, None, false, false),
        ),
        (
            "plain.tar.xz",
            options(ArchiveFormat::TarXz, None, false, false),
        ),
    ];
    if tar.zstd {
        cases.push((
            "plain.tar.zst",
            options(ArchiveFormat::TarZst, None, false, false),
        ));
    }
    if tar.libarchive {
        cases.extend([
            ("plain.zip", options(ArchiveFormat::Zip, None, false, false)),
            (
                "stored.zip",
                options(ArchiveFormat::Zip, Some(Method::Store), false, false),
            ),
            (
                "plain.7z",
                options(ArchiveFormat::SevenZ, None, false, false),
            ),
            (
                "apart.7z",
                CreateOptions {
                    solid: false,
                    ..options(ArchiveFormat::SevenZ, None, false, false)
                },
            ),
        ]);
    }
    let archives = root.path().join("archives");
    for (name, options) in cases {
        let request = CreateRequest {
            sources: vec![project.clone()],
            destination: archives.join(name),
            options,
            overwrite: false,
            layout: Layout::Whole,
        };
        create(&request, &OpContext::silent()).unwrap_or_else(|error| panic!("{name}: {error}"));

        // Names relative to the folder: to GNU tar a colon in the name of
        // an archive means another machine.
        let unpacked = format!("{name}.out");
        fs::create_dir(archives.join(&unpacked)).unwrap();
        run("tar", &archives, &["-xf", name, "-C", &unpacked]);
        assert_same(&tree(&archives.join(&unpacked)), &wanted, name);
    }
}

#[test]
fn boxit_reads_what_the_tar_of_the_system_writes() {
    let Some(tar) = system_tar() else {
        return;
    };
    let root = scratch();
    plant(root.path());
    // The project alone is under the root yet.
    let wanted = tree(root.path());
    // The old headers have no word on the code page of their names, and
    // libarchive would write them in the one of the system, which need not
    // have the letters of the names at all.
    let utf8: &[&str] = if tar.libarchive {
        &["--options", "hdrcharset=UTF-8"]
    } else {
        &[]
    };
    let gnu = if tar.libarchive {
        "--format=gnutar"
    } else {
        "--format=gnu"
    };
    let mut cases: Vec<(&str, Vec<&str>)> = vec![
        ("default.tar", vec![]),
        ("pax.tar", vec!["--format=pax"]),
        ("ustar.tar", [&["--format=ustar"], utf8].concat()),
        ("gnu.tar", [&[gnu], utf8].concat()),
        ("plain.tar.gz", vec!["-z"]),
        ("plain.tar.bz2", vec!["-j"]),
        ("plain.tar.xz", vec!["-J"]),
    ];
    if tar.zstd {
        cases.push(("plain.tar.zst", vec!["--zstd"]));
    }
    if tar.libarchive {
        cases.push(("plain.zip", [&["--format=zip"], utf8].concat()));
        cases.push(("plain.7z", vec!["--format=7zip"]));
    }
    for (name, switches) in cases {
        let mut arguments = switches;
        arguments.extend_from_slice(&["-cf", name, "project"]);
        run("tar", root.path(), &arguments);
        boxit_reads(root.path(), name, false, &wanted);
    }
}

/// The tree without what an archive of files alone cannot hold: the folder
/// with nothing in it. `ascii` leaves out the names a program reads in a
/// code page that may not have their letters.
fn files_only(wanted: &BTreeMap<String, Vec<u8>>, ascii: bool) -> BTreeMap<String, Vec<u8>> {
    wanted
        .iter()
        .filter(|(name, _)| !name.ends_with("empty/") && (!ascii || name.is_ascii()))
        .map(|(name, content)| (name.clone(), content.clone()))
        .collect()
}

/// The number of files of a tree.
fn count_files(tree: &BTreeMap<String, Vec<u8>>) -> u64 {
    tree.keys().filter(|name| !name.ends_with('/')).count() as u64
}

#[test]
fn boxit_reads_what_makecab_writes() {
    if !cfg!(windows) {
        eprintln!("makecab is a program of Windows: nothing was checked");
        return;
    }
    let root = scratch();
    let project = plant(root.path());
    plant_program(&project);
    // The directives are read in the code page of the system, which need
    // not have the letters of every name.
    let wanted = files_only(&tree(root.path()), true);
    let files: Vec<String> = wanted
        .keys()
        .filter(|name| !name.ends_with('/'))
        .map(|name| name.replace('/', "\\"))
        .collect();
    // The name of the cabinet, and how it is compressed.
    let cases = [
        ("mszip.cab", "CompressionType=MSZIP"),
        (
            "lzx15.cab",
            "CompressionType=LZX\n.Set CompressionMemory=15",
        ),
        (
            "lzx18.cab",
            "CompressionType=LZX\n.Set CompressionMemory=18",
        ),
        (
            "lzx21.cab",
            "CompressionType=LZX\n.Set CompressionMemory=21",
        ),
        ("stored.cab", "Compress=off"),
    ];
    for (name, compression) in cases {
        let mut directives = format!(
            ".OPTION EXPLICIT\n\
             .Set CabinetNameTemplate={name}\n\
             .Set DiskDirectory1=.\n\
             .Set Cabinet=on\n\
             .Set MaxDiskSize=0\n\
             .Set RptFileName=nul\n\
             .Set InfFileName=nul\n\
             .Set {compression}\n"
        );
        for (index, file) in files.iter().enumerate() {
            // The files are spread over two folders of the cabinet.
            if index == 2 {
                directives.push_str(".New Folder\n");
            }
            writeln!(directives, "\"{file}\" \"{file}\"").unwrap();
        }
        let list = format!("{name}.ddf");
        fs::write(root.path().join(&list), directives).unwrap();
        run("makecab", root.path(), &["/F", &list]);
        boxit_reads_files(root.path(), name, false, &wanted);
    }
}

#[test]
fn boxit_reads_the_other_archives_libarchive_writes() {
    let Some(tar) = bsdtar() else {
        return;
    };
    let root = scratch();
    let project = plant(root.path());
    // The project alone is under the root yet.
    let wanted = tree(root.path());
    // The formats of cpio have no word on the code page of their names
    // either.
    let utf8 = ["--options", "hdrcharset=UTF-8"];
    let cases: [(&str, Vec<&str>); 4] = [
        ("odc.cpio", [&["--format=cpio"][..], &utf8].concat()),
        ("newc.cpio", [&["--format=newc"][..], &utf8].concat()),
        // Both sets of names of a disc: Rock Ridge and Joliet.
        ("both.iso", vec!["--format=iso9660"]),
        // Joliet alone: names as Windows reads them.
        (
            "joliet.iso",
            vec!["--format=iso9660", "--options", "!rockridge"],
        ),
    ];
    for (name, switches) in cases {
        let mut arguments = switches;
        arguments.extend_from_slice(&["-cf", name, "project"]);
        run(tar, root.path(), &arguments);
        boxit_reads(root.path(), name, false, &wanted);
    }

    // A library of object files holds files alone, without folders.
    let flat = ["readme.txt", "noise.bin", "nothing.txt"];
    let wanted: BTreeMap<String, Vec<u8>> = flat
        .iter()
        .map(|name| ((*name).to_owned(), fs::read(project.join(name)).unwrap()))
        .collect();
    for (name, format) in [("bsd.a", "--format=arbsd"), ("gnu.a", "--format=argnu")] {
        let archive = root.path().join(name);
        let mut arguments = vec![format, "-cf", archive.to_str().unwrap()];
        arguments.extend_from_slice(&flat);
        run(tar, &project, &arguments);
        boxit_reads_files(root.path(), name, false, &wanted);
    }
}

#[test]
fn boxit_reads_the_images_seven_zip_writes() {
    let Some(seven_zip) = seven_zip() else {
        return;
    };
    let root = scratch();
    plant(root.path());
    // The project alone is under the root yet.
    let wanted = tree(root.path());
    run(
        seven_zip,
        root.path(),
        &["a", "-twim", "plain.wim", "project"],
    );
    boxit_reads(root.path(), "plain.wim", false, &wanted);
}

/// A megabyte of the code of a program for the project: LZX and LZMS
/// rewrite the calls in it before they compress, and the calls have to come
/// back as they were.
fn plant_program(project: &Path) {
    let mut program = fs::read(env::current_exe().unwrap()).unwrap();
    program.truncate(3 << 20);
    fs::write(project.join("program.bin"), program).unwrap();
}

#[test]
fn boxit_reads_what_wimlib_writes() {
    if !runs("wimlib-imagex", &["--version"]) {
        missing("wimlib");
        return;
    }
    let root = scratch();
    let project = plant(root.path());
    plant_program(&project);
    // The image is of what is in the project.
    let wanted = tree(&project);
    // The name of the file, and how it is compressed.
    let cases: [(&str, &[&str]); 8] = [
        ("stored.wim", &["--compress=none"]),
        ("xpress.wim", &["--compress=XPRESS"]),
        ("xpress-4k.wim", &["--compress=XPRESS", "--chunk-size=4096"]),
        ("lzx.wim", &["--compress=LZX"]),
        ("lzx-best.wim", &["--compress=LZX:100"]),
        ("lzx-1m.wim", &["--compress=LZX", "--chunk-size=1048576"]),
        ("lzms.wim", &["--compress=LZMS"]),
        // Everything compressed together, as in an ESD.
        ("solid.esd", &["--solid"]),
    ];
    for (name, switches) in cases {
        let mut arguments = vec!["capture", "project", name, "project"];
        arguments.extend_from_slice(switches);
        run("wimlib-imagex", root.path(), &arguments);
        boxit_reads_files(root.path(), name, false, &wanted);
    }

    // Two images in one file are two folders.
    run(
        "wimlib-imagex",
        root.path(),
        &[
            "capture",
            "project/docs",
            "two.wim",
            "docs",
            "--compress=LZX",
        ],
    );
    run(
        "wimlib-imagex",
        root.path(),
        &["append", "project", "two.wim", "project"],
    );
    let mut both = BTreeMap::new();
    both.insert(String::from("1/"), Vec::new());
    both.insert(String::from("2/"), Vec::new());
    for (name, content) in tree(&project.join("docs")) {
        both.insert(format!("1/{name}"), content);
    }
    for (name, content) in &wanted {
        both.insert(format!("2/{name}"), content.clone());
    }
    boxit_reads_files(root.path(), "two.wim", false, &both);
}

#[test]
fn boxit_reads_what_dism_writes() {
    if !cfg!(windows) {
        eprintln!("DISM is a program of Windows: nothing was checked");
        return;
    }
    let root = scratch();
    let project = plant(root.path());
    plant_program(&project);
    // The image is of what is in the project.
    let wanted = tree(&project);
    let folder = format!("/CaptureDir:{}", project.to_string_lossy());
    // The name of the file, and how DISM names its compression: XPRESS is
    // the fast one, LZX the one that compresses most.
    for (name, compression) in [
        ("stored.wim", "/Compress:none"),
        ("xpress.wim", "/Compress:fast"),
        ("lzx.wim", "/Compress:max"),
    ] {
        let file = format!("/ImageFile:{}", root.path().join(name).to_string_lossy());
        let output = Command::new("dism")
            .args([
                "/Capture-Image",
                &file,
                &folder,
                "/Name:project",
                compression,
            ])
            .stdin(Stdio::null())
            .output();
        let done = output.as_ref().is_ok_and(|output| output.status.success());
        if !done {
            // DISM works for an administrator alone.
            if let Ok(output) = output {
                eprintln!("{}", String::from_utf8_lossy(&output.stdout));
            }
            missing("dism");
            return;
        }
        boxit_reads_files(root.path(), name, false, &wanted);
    }
}

/// BoxIt lists, tests and unpacks the archive `name` of the folder `root`
/// and gets the tree `wanted`.
fn boxit_reads(root: &Path, name: &str, locked: bool, wanted: &BTreeMap<String, Vec<u8>>) {
    assert_eq!(count_files(wanted), FILES);
    boxit_reads_files(root, name, locked, wanted);
}

/// See [`boxit_reads`]; for a tree of however many files.
fn boxit_reads_files(root: &Path, name: &str, locked: bool, wanted: &BTreeMap<String, Vec<u8>>) {
    let silent = OpContext::silent();
    let path = root.join(name);
    // Without its password a locked archive gives nothing away.
    if locked {
        let refused = Archive::open(&path, OpenOptions::default())
            .and_then(|mut archive| archive.test(&Selection::All, TestOptions::default(), &silent));
        assert_eq!(
            refused.unwrap_err().code,
            "passwordRequired",
            "{name} without a password"
        );
    }
    let known = OpenOptions {
        password: locked.then(|| Password::from(PASSWORD)),
        ..OpenOptions::default()
    };
    let mut archive = Archive::open(&path, known).unwrap_or_else(|error| panic!("{name}: {error}"));
    let listing = archive
        .list(&silent)
        .unwrap_or_else(|error| panic!("{name}: {error}"));
    assert_eq!(listing.files, count_files(wanted), "{name}");
    assert_eq!(listing.encrypted, locked, "{name}");
    archive
        .test(&Selection::All, TestOptions::default(), &silent)
        .unwrap_or_else(|error| panic!("{name}: {error}"));
    let unpacked = root.join(format!("{name}.out"));
    let outcome = archive
        .extract(
            &unpacked,
            Selection::All,
            ExtractOptions::default(),
            &silent,
        )
        .unwrap_or_else(|error| panic!("{name}: {error}"));
    assert_eq!(outcome.skipped_total, 0, "{name}: {:?}", outcome.skipped);
    assert_same(&tree(&unpacked), wanted, name);
}

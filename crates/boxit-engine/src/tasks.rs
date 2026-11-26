//! The operations of the window as jobs of the queue: what happens between
//! a press of a button and the report of what was done.

use std::fs;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, PoisonError};

use boxit_codecs::OpenOptions as ReadOptions;
use boxit_core::{
    ArchiveFormat, Conflict, ConflictAnswer, ConflictPolicy, CreateOptions, EntryMeta, Error,
    ExtractOptions, Interaction, Level, Method, OpContext, Password, PasswordRequest, Result,
    Selection,
};
use serde::Deserialize;
use serde_json::{json, Value};

use crate::archive::{open_options, Archive, TestOptions};
use crate::browse::NodeId;
use crate::budget;
use crate::create::{self, CreateRequest, Layout};
use crate::fsx;
use crate::jobs::{JobInfo, JobKind, JobState, Jobs, Ticket};
use crate::repair::{self, RepairRequest};
use crate::sessions::Session;
use crate::settings::ExtractPlace;
use crate::update::{self, UpdateRequest};

/// Passes the questions of an operation on, and notes the password it was
/// given: one that opened the archive is not asked for again.
struct Noting {
    asked: Arc<dyn Interaction>,
    given: Mutex<Option<Password>>,
}

impl Interaction for Noting {
    fn password(&self, request: &PasswordRequest) -> Option<Password> {
        let password = self.asked.password(request);
        if password.is_some() {
            self.given
                .lock()
                .unwrap_or_else(PoisonError::into_inner)
                .clone_from(&password);
        }
        password
    }

    fn conflict(&self, conflict: &Conflict) -> ConflictAnswer {
        self.asked.conflict(conflict)
    }

    fn symlink(&self, link: &EntryMeta) -> bool {
        self.asked.symlink(link)
    }
}

/// Runs an operation on an open archive with the context of a job. When
/// the operation succeeds, the password it was given is kept for the
/// archive.
fn with_archive<T>(
    ticket: &Arc<Ticket>,
    session: &Session,
    operation: impl FnOnce(&OpContext) -> Result<T>,
) -> Result<T> {
    let mut ctx = ticket.context(budget::system(0));
    let noting = Arc::new(Noting {
        asked: Arc::clone(&ctx.interaction),
        given: Mutex::new(None),
    });
    ctx.interaction = Arc::clone(&noting) as Arc<dyn Interaction>;
    let done = operation(&ctx)?;
    let given = noting
        .given
        .lock()
        .unwrap_or_else(PoisonError::into_inner)
        .take();
    if let Some(password) = given {
        session.remember(password);
    }
    Ok(done)
}

/// What to take out of an open archive, and where to.
pub struct ExtractTask {
    pub session: Arc<Session>,
    /// The folder of the archive the rows are shown in.
    pub base: NodeId,
    /// The rows to extract; none stands for the whole folder.
    pub rows: Vec<NodeId>,
    pub destination: PathBuf,
    pub options: ExtractOptions,
}

/// Queues an extraction. The result of the job is the outcome of the
/// extraction with the `destination` it went to.
pub fn extract(jobs: &Jobs, task: ExtractTask) -> u64 {
    let archive = fsx::display(task.session.path());
    let target = fsx::display(&task.destination);
    let shown = target.clone();
    jobs.spawn(JobKind::Extract, &archive, Some(&shown), move |ticket| {
        let ExtractTask {
            session,
            base,
            rows,
            destination,
            options,
        } = task;
        let picked = session.index.pick(base, &rows);
        let outcome = with_archive(ticket, &session, |ctx| {
            session.archive()?.extract_from(
                &picked.base,
                &destination,
                picked.selection,
                options,
                ctx,
            )
        })?;
        let mut result = json!(outcome);
        result["destination"] = Value::String(target);
        Ok(result)
    })
}

/// Queues a test of the rows of an open archive, or of all of it.
pub fn test(
    jobs: &Jobs,
    session: Arc<Session>,
    base: NodeId,
    rows: Vec<NodeId>,
    options: TestOptions,
) -> u64 {
    let archive = fsx::display(session.path());
    jobs.spawn(JobKind::Test, &archive, None, move |ticket| {
        let picked = session.index.pick(base, &rows);
        let tested = with_archive(ticket, &session, |ctx| {
            session.archive()?.test(&picked.selection, options, ctx)
        })?;
        Ok(json!(tested))
    })
}

/// Queues a repair of an archive into a new file. The password and the
/// encoding are the ones the archive was opened with.
pub fn repair(jobs: &Jobs, session: Arc<Session>, destination: PathBuf, overwrite: bool) -> u64 {
    let archive = fsx::display(session.path());
    let shown = fsx::display(&destination);
    jobs.spawn(JobKind::Repair, &archive, Some(&shown), move |ticket| {
        let request = RepairRequest {
            source: session.path().to_path_buf(),
            destination,
            overwrite,
        };
        let options = open_options(session.password(), session.encoding());
        let ctx = ticket.context(budget::system(0));
        Ok(json!(repair::repair(&request, options, &ctx)?))
    })
}

/// Queues a change of an archive. The caller has closed the session that
/// had the file open: the new file takes its place, and the window opens
/// it again when the job is done.
pub fn update(jobs: &Jobs, request: UpdateRequest, options: boxit_codecs::OpenOptions) -> u64 {
    let archive = fsx::display(&request.archive);
    jobs.spawn(JobKind::Update, &archive, None, move |ticket| {
        let ctx = ticket.context(budget::system(0));
        Ok(json!(update::update(&request, options, &ctx)?))
    })
}

/// Queues a test of every archive. Each archive is its own job.
pub fn batch_test(jobs: &Jobs, archives: Vec<PathBuf>) -> Vec<u64> {
    archives
        .into_iter()
        .map(|path| {
            let shown = fsx::display(&path);
            jobs.spawn(JobKind::Test, &shown, None, move |ticket| {
                let ctx = ticket.context(budget::system(0));
                let mut archive = Archive::open(&path, ReadOptions::default())?;
                Ok(json!(archive.test(
                    &Selection::All,
                    TestOptions::default(),
                    &ctx
                )?))
            })
        })
        .collect()
}

/// Queues an extraction of every archive into `destination`, each in a
/// folder of its name.
pub fn batch_extract(jobs: &Jobs, archives: Vec<PathBuf>, destination: PathBuf) -> Vec<u64> {
    archives
        .into_iter()
        .map(|path| {
            let shown = fsx::display(&path);
            let folder = destination.join(path.file_stem().unwrap_or_default());
            let target = fsx::display(&folder);
            jobs.spawn(JobKind::Extract, &shown, Some(&target), move |ticket| {
                let ctx = ticket.context(budget::system(0));
                let mut archive = Archive::open(&path, ReadOptions::default())?;
                let outcome =
                    archive.extract(&folder, Selection::All, ExtractOptions::default(), &ctx)?;
                let mut result = json!(outcome);
                result["destination"] = Value::String(fsx::display(&folder));
                Ok(result)
            })
        })
        .collect()
}

/// Queues the creation of an archive.
pub fn create(jobs: &Jobs, request: CreateRequest) -> u64 {
    let archive = fsx::display(&request.destination);
    jobs.spawn(JobKind::Create, &archive, None, move |ticket| {
        let ctx = ticket.context(budget::system(request.options.threads));
        Ok(json!(create::create(&request, &ctx)?))
    })
}

/// The memory creating an archive with these options takes on this
/// machine, and the memory the machine gives to one operation.
pub fn memory_to_create(options: &CreateOptions) -> (u64, u64) {
    let budget = budget::system(options.threads);
    (
        boxit_codecs::plan::memory_to_create(options, budget),
        budget.memory,
    )
}

/// A new archive as the dialog of the window describes it.
#[derive(Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CreateForm {
    /// The files and folders to store.
    #[serde(default)]
    pub sources: Vec<String>,
    /// The archive file.
    #[serde(default)]
    pub destination: String,
    pub format: ArchiveFormat,
    #[serde(default)]
    pub level: Level,
    #[serde(default)]
    pub method: Option<Method>,
    #[serde(default = "solid_by_default")]
    pub solid: bool,
    #[serde(default)]
    pub dictionary: Option<u64>,
    #[serde(default)]
    pub comment: Option<String>,
    #[serde(default)]
    pub password: Option<String>,
    #[serde(default)]
    pub encrypt_names: bool,
    /// Worker threads; 0 takes one per processor.
    #[serde(default)]
    pub threads: usize,
    /// Replace the file that stands at the destination.
    #[serde(default)]
    pub overwrite: bool,
    /// Cut the archive into volumes of this many bytes.
    #[serde(default)]
    pub volume: Option<u64>,
    /// Put the archive behind the program that unpacks it.
    #[serde(default)]
    pub self_extracting: bool,
}

fn solid_by_default() -> bool {
    CreateOptions::new(ArchiveFormat::SevenZ).solid
}

impl CreateForm {
    /// The options of the archive. What a field of the dialog was left
    /// empty for is not asked of the format.
    pub fn options(&self) -> CreateOptions {
        let filled = |text: &Option<String>| text.clone().filter(|text| !text.is_empty());
        CreateOptions {
            format: self.format,
            level: self.level,
            method: self.method,
            solid: self.solid,
            dictionary: self.dictionary,
            comment: filled(&self.comment),
            password: filled(&self.password).map(Password::new),
            encrypt_names: self.encrypt_names,
            threads: self.threads,
        }
    }

    pub fn request(&self) -> CreateRequest {
        CreateRequest {
            sources: self.sources.iter().map(PathBuf::from).collect(),
            destination: PathBuf::from(&self.destination),
            options: self.options(),
            overwrite: self.overwrite,
            layout: match self.volume {
                _ if self.self_extracting => Layout::SelfExtracting(create::module()),
                Some(size) => Layout::Volumes(size),
                None => Layout::Whole,
            },
        }
    }
}

/// The name of a file without the extension of an archive, when it has one.
fn without_extension(name: &str) -> &str {
    let lower = name.to_lowercase();
    ArchiveFormat::ALL
        .into_iter()
        .flat_map(ArchiveFormat::extensions)
        .filter(|extension| {
            lower
                .strip_suffix(*extension)
                .is_some_and(|rest| rest.ends_with('.'))
        })
        .map(|extension| extension.len() + 1)
        .max()
        // The extensions are ASCII: as many bytes in either case.
        .map_or(name, |length| &name[..name.len() - length])
}

/// The name of the folder an archive is extracted into when the folder is
/// named after it: the name of the file without its extension. A name that
/// has nothing to lose gets a mark, since the file itself stands there.
fn folder_name(archive: &Path) -> String {
    let name = archive
        .file_name()
        .map_or_default(|name| name.to_string_lossy().into_owned());
    // Windows gives a name that ends with a dot or a space to nobody.
    let stem = without_extension(&name).trim_end_matches(['.', ' ']);
    if stem.is_empty() || stem == name {
        format!("{name}~")
    } else {
        stem.to_owned()
    }
}

/// Where the content of an archive goes for a choice of the dialog.
/// `chosen` is the folder of [`ExtractPlace::Chosen`]; without one the
/// content goes where it does by default.
pub fn destination_for(archive: &Path, place: ExtractPlace, chosen: &str) -> PathBuf {
    let beside = archive.parent().map_or_default(Path::to_path_buf);
    match place {
        ExtractPlace::Beside => beside,
        ExtractPlace::Chosen if !chosen.trim().is_empty() => PathBuf::from(chosen.trim()),
        ExtractPlace::Chosen | ExtractPlace::Named => beside.join(folder_name(archive)),
    }
}

/// The name a new archive of `sources` is offered: after the only source,
/// or after the folder the sources share, next to them. One compressed
/// file keeps the whole name of what it holds.
pub fn archive_for(sources: &[PathBuf], format: ArchiveFormat) -> Option<PathBuf> {
    let first = sources.first()?;
    let folder = first.parent()?;
    let name_of = |path: &Path| {
        path.file_name()
            .map(|name| name.to_string_lossy().into_owned())
    };
    let name = if sources.len() > 1 {
        name_of(folder)
    } else if format.is_single_file() || first.is_dir() {
        name_of(first)
    } else {
        first
            .file_stem()
            .map(|stem| stem.to_string_lossy().into_owned())
    };
    let name = name
        .filter(|name| !name.is_empty())
        .unwrap_or_else(|| String::from("archive"));
    Some(folder.join(format!("{name}.{}", format.id())))
}

/// Rows on their way out of the window: the job that extracts them, and
/// the folder it extracts them into.
#[derive(Clone, Debug)]
pub struct Drag {
    pub job: u64,
    pub folder: PathBuf,
}

/// Queues the extraction of rows of an open archive into a new folder
/// inside `temp`, to be dragged out of the window.
pub fn drag_start(
    jobs: &Jobs,
    session: Arc<Session>,
    base: NodeId,
    rows: Vec<NodeId>,
    temp: &Path,
) -> Drag {
    let folder = temp.join(format!("drag-{}", fsx::unique_suffix()));
    let task = ExtractTask {
        session,
        base,
        rows,
        destination: folder.clone(),
        options: ExtractOptions {
            conflicts: ConflictPolicy::Overwrite,
            ..ExtractOptions::default()
        },
    };
    Drag {
        job: extract(jobs, task),
        folder,
    }
}

/// Waits for the rows of a drag to be extracted. Returns what stands in
/// its folder: the files and folders to drag.
pub fn drag_finish(jobs: &Jobs, drag: &Drag) -> Result<Vec<PathBuf>> {
    match jobs.wait(drag.job) {
        Some(info) if info.state == JobState::Done => {}
        Some(JobInfo {
            error: Some(error), ..
        }) => return Err(error),
        _ => return Err(Error::cancelled()),
    }
    let mut taken: Vec<PathBuf> = fs::read_dir(&drag.folder)
        .map_err(|error| Error::from(error).with("path", fsx::display(&drag.folder)))?
        .filter_map(|entry| entry.ok().map(|entry| entry.path()))
        .collect();
    taken.sort();
    Ok(taken)
}

#[cfg(test)]
mod tests {
    use std::fs;
    use std::thread;
    use std::time::Duration;

    use boxit_codecs::names::NameEncoding;
    use boxit_core::{ArchiveFormat, ConflictPolicy, Level};
    use tempfile::TempDir;

    use super::*;
    use crate::browse::{Sort, ROOT};
    use crate::jobs::{JobState, Question, Reply};
    use crate::sessions::Sessions;
    use crate::testing::{plant, tree};

    fn options(format: ArchiveFormat) -> CreateOptions {
        CreateOptions {
            level: Level::Fast,
            ..CreateOptions::new(format)
        }
    }

    /// Waits until the job shows a question and returns it.
    fn question(jobs: &Jobs, id: u64) -> Question {
        loop {
            let info = jobs.get(id).unwrap();
            assert!(!info.state.is_finished(), "{:?}", info.error);
            if let Some(question) = info.question {
                return question;
            }
            thread::sleep(Duration::from_millis(5));
        }
    }

    #[test]
    fn an_archive_is_created_browsed_extracted_and_tested_through_the_queue() {
        let root = TempDir::new().unwrap();
        plant(
            &root.path().join("photos"),
            &[
                ("2024/a.txt", "alpha"),
                ("2024/b.txt", "beta"),
                ("c.txt", "gamma"),
            ],
        );
        let jobs = Jobs::new();
        let archive = root.path().join("photos.7z");
        let request = CreateRequest {
            sources: vec![root.path().join("photos")],
            destination: archive.clone(),
            options: options(ArchiveFormat::SevenZ),
            overwrite: false,
            layout: Layout::Whole,
        };
        let info = jobs.wait(create(&jobs, request)).unwrap();
        assert_eq!(info.state, JobState::Done, "{:?}", info.error);
        let result = info.result.unwrap();
        assert_eq!(
            (result["files"].as_u64(), result["folders"].as_u64()),
            (Some(3), Some(2))
        );
        assert_eq!(info.bytes_done, 14);

        let sessions = Sessions::new();
        let session = sessions
            .open(&archive, None, NameEncoding::Auto, &OpContext::silent())
            .unwrap();
        let index = &session.index;
        let photos = index.order(ROOT, Sort::default())[0];
        let year = index.order(photos, Sort::default())[0];
        assert_eq!(index.path(year), "photos/2024");

        let destination = root.path().join("out");
        let extracted = extract(
            &jobs,
            ExtractTask {
                session: Arc::clone(&session),
                base: photos,
                rows: vec![year],
                destination: destination.clone(),
                options: ExtractOptions::default(),
            },
        );
        let info = jobs.wait(extracted).unwrap();
        assert_eq!(info.state, JobState::Done, "{:?}", info.error);
        let result = info.result.unwrap();
        assert_eq!(result["files"].as_u64(), Some(2));
        assert_eq!(result["skippedTotal"].as_u64(), Some(0));
        assert_eq!(result["destination"].as_str(), info.target.as_deref());
        assert_eq!(
            tree(&destination).into_keys().collect::<Vec<_>>(),
            ["2024/", "2024/a.txt", "2024/b.txt"]
        );

        let tested = test(
            &jobs,
            Arc::clone(&session),
            ROOT,
            Vec::new(),
            TestOptions::default(),
        );
        let info = jobs.wait(tested).unwrap();
        assert_eq!(info.state, JobState::Done, "{:?}", info.error);
        assert_eq!(info.result.unwrap()["files"].as_u64(), Some(3));
        assert_eq!((info.bytes_done, info.bytes_total), (14, Some(14)));
    }

    #[test]
    fn a_job_asks_for_the_password_once_and_the_archive_keeps_it() {
        let root = TempDir::new().unwrap();
        plant(&root.path().join("docs"), &[("a.txt", "alpha")]);
        let archive = root.path().join("locked.zip");
        let request = CreateRequest {
            sources: vec![root.path().join("docs")],
            destination: archive.clone(),
            options: CreateOptions {
                password: Some(Password::from("sesame")),
                ..options(ArchiveFormat::Zip)
            },
            overwrite: false,
            layout: Layout::Whole,
        };
        let jobs = Jobs::new();
        assert_eq!(
            jobs.wait(create(&jobs, request)).unwrap().state,
            JobState::Done
        );

        let sessions = Sessions::new();
        let session = sessions
            .open(&archive, None, NameEncoding::Auto, &OpContext::silent())
            .unwrap();
        assert!(session.index.info().encrypted);
        let task = |destination: &str| ExtractTask {
            session: Arc::clone(&session),
            base: ROOT,
            rows: Vec::new(),
            destination: root.path().join(destination),
            options: ExtractOptions {
                conflicts: ConflictPolicy::Overwrite,
                ..ExtractOptions::default()
            },
        };

        // A wrong password is asked for again, and giving up fails the job.
        let job = extract(&jobs, task("never"));
        let Question::Password { id, retry, .. } = question(&jobs, job) else {
            panic!("another question");
        };
        assert!(!retry);
        let wrong = Reply::Password {
            password: Some(String::from("guess")),
        };
        assert!(jobs.answer(job, id, wrong));
        let again = loop {
            match question(&jobs, job) {
                Question::Password {
                    id: next,
                    retry: true,
                    ..
                } if next != id => break next,
                _ => thread::sleep(Duration::from_millis(5)),
            }
        };
        assert!(jobs.answer(job, again, Reply::Password { password: None }));
        let info = jobs.wait(job).unwrap();
        assert_eq!(info.state, JobState::Failed);
        assert_eq!(info.error.unwrap().code, "wrongPassword");
        assert!(!root.path().join("never").exists());
        assert_eq!(session.password(), None);

        // The right one is kept: the next job does not ask.
        let job = extract(&jobs, task("out"));
        let Question::Password { id, .. } = question(&jobs, job) else {
            panic!("another question");
        };
        let right = Reply::Password {
            password: Some(String::from("sesame")),
        };
        assert!(jobs.answer(job, id, right));
        assert_eq!(jobs.wait(job).unwrap().state, JobState::Done);
        assert_eq!(session.password(), Some(Password::from("sesame")));
        assert_eq!(
            fs::read_to_string(root.path().join("out/docs/a.txt")).unwrap(),
            "alpha"
        );

        let job = test(
            &jobs,
            Arc::clone(&session),
            ROOT,
            Vec::new(),
            TestOptions::default(),
        );
        let info = jobs.wait(job).unwrap();
        assert_eq!(info.state, JobState::Done, "{:?}", info.error);
    }

    #[test]
    fn the_memory_of_a_creation_is_told_before_it_starts() {
        let mut options = CreateOptions::new(ArchiveFormat::SevenZ);
        options.threads = 2;
        options.dictionary = Some(1 << 20);
        let (small, budget) = memory_to_create(&options);
        options.dictionary = Some(64 << 20);
        let (large, _) = memory_to_create(&options);
        assert!(small > 0 && large > small);
        assert!(budget >= 64 << 20);
    }

    #[test]
    fn the_dialog_of_a_new_archive_is_read_as_a_request() {
        let form: CreateForm = serde_json::from_str(
            r#"{
                "sources": ["C:\\work\\a.txt"],
                "destination": "C:\\work\\a.7z",
                "format": "7z",
                "level": "ultra",
                "method": "lzma2",
                "solid": false,
                "dictionary": 1048576,
                "comment": "",
                "password": "sesame",
                "encryptNames": true,
                "threads": 3,
                "overwrite": true
            }"#,
        )
        .unwrap();
        let request = form.request();
        assert_eq!(request.sources, [PathBuf::from(r"C:\work\a.txt")]);
        assert_eq!(request.destination, PathBuf::from(r"C:\work\a.7z"));
        assert!(request.overwrite);
        assert_eq!(
            request.options,
            CreateOptions {
                format: ArchiveFormat::SevenZ,
                level: Level::Ultra,
                method: Some(Method::Lzma2),
                solid: false,
                dictionary: Some(1 << 20),
                comment: None,
                password: Some(Password::from("sesame")),
                encrypt_names: true,
                threads: 3,
            }
        );

        // What the dialog leaves out is what the format does by default.
        let form: CreateForm =
            serde_json::from_str(r#"{"format": "tar.gz", "password": ""}"#).unwrap();
        assert_eq!(form.options(), CreateOptions::new(ArchiveFormat::TarGz));
        assert!(serde_json::from_str::<CreateForm>(r#"{"format": "nope"}"#).is_err());
    }

    #[test]
    fn the_content_goes_where_the_dialog_says() {
        let archive = Path::new("work").join("Photos 2024.tar.gz");
        let to = |place, chosen| destination_for(&archive, place, chosen);
        assert_eq!(to(ExtractPlace::Beside, "x"), Path::new("work"));
        assert_eq!(
            to(ExtractPlace::Named, "x"),
            Path::new("work").join("Photos 2024")
        );
        assert_eq!(to(ExtractPlace::Chosen, " out "), Path::new("out"));
        // Nothing is chosen yet: the content goes where it does by default.
        assert_eq!(
            to(ExtractPlace::Chosen, " "),
            Path::new("work").join("Photos 2024")
        );

        let named = |name: &str| folder_name(Path::new(name));
        assert_eq!(named("a.ZIP"), "a");
        assert_eq!(named("a.b.7z"), "a.b");
        assert_eq!(named("backup.tgz"), "backup");
        assert_eq!(named("setup.exe"), "setup.exe~");
        assert_eq!(named("data"), "data~");
        assert_eq!(named("odd. .zip"), "odd");
        assert_eq!(named(".zip"), ".zip~");
        assert_eq!(named("архив.rar"), "архив");
    }

    #[test]
    fn a_new_archive_is_named_after_what_goes_into_it() {
        let root = TempDir::new().unwrap();
        plant(
            &root.path().join("work"),
            &[("report.final.txt", "x"), ("photos/a.jpg", "y")],
        );
        let work = root.path().join("work");
        let file = work.join("report.final.txt");
        let folder = work.join("photos");
        let offered = |sources: &[PathBuf], format| archive_for(sources, format).unwrap();
        assert_eq!(
            offered(std::slice::from_ref(&file), ArchiveFormat::Zip),
            work.join("report.final.zip")
        );
        assert_eq!(
            offered(std::slice::from_ref(&file), ArchiveFormat::Gz),
            work.join("report.final.txt.gz")
        );
        assert_eq!(
            offered(std::slice::from_ref(&folder), ArchiveFormat::TarZst),
            work.join("photos.tar.zst")
        );
        assert_eq!(
            offered(&[file, folder], ArchiveFormat::SevenZ),
            work.join("work.7z")
        );
        assert_eq!(archive_for(&[], ArchiveFormat::Zip), None);
    }

    #[test]
    fn what_is_dragged_out_waits_in_a_folder_of_its_own() {
        let root = TempDir::new().unwrap();
        plant(
            &root.path().join("photos"),
            &[("2024/a.txt", "alpha"), ("c.txt", "gamma")],
        );
        let jobs = Jobs::new();
        let archive = root.path().join("photos.zip");
        let request = CreateRequest {
            sources: vec![root.path().join("photos")],
            destination: archive.clone(),
            options: options(ArchiveFormat::Zip),
            overwrite: false,
            layout: Layout::Whole,
        };
        assert_eq!(
            jobs.wait(create(&jobs, request)).unwrap().state,
            JobState::Done
        );
        let sessions = Sessions::new();
        let session = sessions
            .open(&archive, None, NameEncoding::Auto, &OpContext::silent())
            .unwrap();
        let photos = session.index.order(ROOT, Sort::default())[0];
        let temp = root.path().join("temp");
        let drag = |session: &Arc<Session>| {
            let started = drag_start(&jobs, Arc::clone(session), photos, Vec::new(), &temp);
            drag_finish(&jobs, &started)
        };

        let taken = drag(&session).unwrap();
        let names: Vec<_> = taken
            .iter()
            .map(|path| path.file_name().unwrap().to_string_lossy().into_owned())
            .collect();
        assert_eq!(names, ["2024", "c.txt"]);
        assert_eq!(fs::read_to_string(&taken[1]).unwrap(), "gamma");
        assert!(taken[0].starts_with(&temp));

        // A second drag does not meet the first.
        let again = drag(&session).unwrap();
        assert_ne!(again[0].parent(), taken[0].parent());

        // The archive is gone: the error of the job is the error of the drag.
        fs::remove_file(&archive).unwrap();
        let error = drag(&session).unwrap_err();
        assert_eq!(error.code, "notFound");
    }
}

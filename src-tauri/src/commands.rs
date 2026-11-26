//! Commands of the interface, mirrored by `src/api/index.ts`. They all run on
//! the worker pool, so the main thread stays free for the window.

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::Instant;

use serde::Serialize;
use tauri::ipc::Response;
use tauri::{AppHandle, Emitter, State};

use boxit_core::{
    ArchiveFormat, Error, ExtractOptions, NoInteraction, OpContext, Password, Progress, Result,
};
use boxit_engine::archive::TestOptions;
use boxit_engine::browse::{Info, NodeId, Row, Sort, Summary};
use boxit_engine::formats::{self, FormatInfo};
use boxit_engine::fsx::{self, Existing};
use boxit_engine::jobs::{JobInfo, Reply};
use boxit_engine::passwords::{self, GENERATED_LENGTH};
use boxit_engine::repair;
use boxit_engine::settings::ExtractPlace;
use boxit_engine::tasks::{self, CreateForm, ExtractTask};
use boxit_engine::{archive, budget, Settings};

use crate::app::{AppInfo, Core};
use crate::secrets;
use crate::update::{self, UpdateStatus};

pub type CoreState<'a> = State<'a, Arc<Core>>;

/// How often the reading of a long list tells how far it is, in
/// milliseconds.
const OPENING_TICK: u64 = 100;

// The application

#[tauri::command(async)]
pub fn app_info(core: CoreState<'_>) -> AppInfo {
    core.info()
}

/// The interface has painted its first frame: the window may appear.
#[tauri::command(async)]
pub fn window_show(core: CoreState<'_>) {
    core.show_window();
}

/// Files and folders BoxIt was started with; handed out once.
#[tauri::command(async)]
pub fn opened_files(core: CoreState<'_>) -> Vec<String> {
    core.take_opened()
}

#[tauri::command(async)]
pub fn settings_get(core: CoreState<'_>) -> Settings {
    core.engine.settings()
}

#[tauri::command(async)]
pub fn settings_set(core: CoreState<'_>, settings: Settings) -> Result<Settings> {
    core.set_settings(settings)
}

/// What the formats offer to the dialogs.
#[tauri::command(async)]
pub fn formats_list() -> Vec<FormatInfo> {
    formats::formats()
}

/// The encodings offered for the names of an archive.
#[tauri::command(async)]
pub fn name_encodings() -> &'static [&'static str] {
    formats::name_encodings()
}

// Open archives

/// An archive as the interface gets it once its list is read.
#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Opened {
    pub id: u64,
    pub info: Info,
    /// The encoding the names were asked to be read in: a label, or `auto`.
    pub encoding: String,
    /// How many entries the archive has.
    pub entries: usize,
}

/// How far the reading of a list is.
#[derive(Clone, Copy, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct Opening {
    token: u64,
    entries: u64,
}

/// Counts the entries found while a list is read and tells the interface,
/// a few times a second.
struct Counting {
    app: AppHandle,
    token: u64,
    since: Instant,
    found: AtomicU64,
    /// When the interface was told last, in milliseconds after `since`.
    told: AtomicU64,
}

impl Progress for Counting {
    fn entry(&self, _path: &str) {
        let entries = self.found.fetch_add(1, Ordering::Relaxed) + 1;
        let now = self.since.elapsed().as_millis() as u64;
        let told = self.told.load(Ordering::Relaxed);
        let due = now.saturating_sub(told) >= OPENING_TICK;
        if due
            && self
                .told
                .compare_exchange(told, now, Ordering::Relaxed, Ordering::Relaxed)
                .is_ok()
        {
            let opening = Opening {
                token: self.token,
                entries,
            };
            let _ = self.app.emit("archive:opening", opening);
        }
    }
}

/// Opens an archive and reads its list. `token` is a number of the
/// interface for this reading: its progress arrives as `archive:opening`
/// events with it, and `archive_cancel_open` stops it.
#[tauri::command(async)]
pub fn archive_open(
    core: CoreState<'_>,
    path: String,
    password: Option<String>,
    encoding: Option<String>,
    token: u64,
) -> Result<Opened> {
    let encoding = formats::name_encoding(encoding.as_deref())?;
    let password = password
        .filter(|password| !password.is_empty())
        .map(Password::new);
    let ctx = OpContext {
        control: core.begin_opening(token),
        progress: Arc::new(Counting {
            app: core.app().clone(),
            token,
            since: Instant::now(),
            found: AtomicU64::new(0),
            told: AtomicU64::new(0),
        }),
        interaction: Arc::new(NoInteraction),
        budget: budget::system(0),
    };
    let opened = core
        .engine
        .sessions
        .open(Path::new(&path), password, encoding, &ctx);
    core.end_opening(token);
    let session = opened?;
    let shown = fsx::display(session.path());
    // An archive that opened stays open whether or not it could be noted.
    if let Err(error) = core
        .engine
        .update_settings(|settings| settings.opened(&shown))
    {
        tracing::warn!("the list of recent archives was not saved: {error}");
    }
    Ok(Opened {
        id: session.id,
        info: session.index.info().clone(),
        encoding: session.encoding().label(),
        entries: session.index.count(),
    })
}

/// Stops the reading of the list that was started under `token`.
#[tauri::command(async)]
pub fn archive_cancel_open(core: CoreState<'_>, token: u64) {
    core.cancel_opening(token);
}

#[tauri::command(async)]
pub fn archive_close(core: CoreState<'_>, id: u64) {
    core.engine.sessions.close(id);
}

/// The rows of a folder of an open archive in the order of the list, as
/// their numbers: four bytes each, the low byte first. A folder of a
/// hundred thousand files is a few hundred kilobytes that way, and the rows
/// themselves are asked for as they come into view.
#[tauri::command(async)]
pub fn archive_folder(
    core: CoreState<'_>,
    id: u64,
    folder: NodeId,
    sort: Sort,
    filter: String,
) -> Result<Response> {
    let session = core.engine.sessions.get(id)?;
    let index = &session.index;
    let order = index.matching(index.order(folder, sort), &filter);
    let mut bytes = Vec::with_capacity(order.len() * size_of::<NodeId>());
    for row in order {
        bytes.extend_from_slice(&row.to_le_bytes());
    }
    Ok(Response::new(bytes))
}

/// The rows with these numbers.
#[tauri::command(async)]
pub fn archive_rows(core: CoreState<'_>, id: u64, rows: Vec<NodeId>) -> Result<Vec<Row>> {
    Ok(core.engine.sessions.get(id)?.index.rows(&rows))
}

/// What a set of rows holds, with everything inside the folders among them.
#[tauri::command(async)]
pub fn archive_summary(core: CoreState<'_>, id: u64, rows: Vec<NodeId>) -> Result<Summary> {
    Ok(core.engine.sessions.get(id)?.index.summary(&rows))
}

/// Gives an open archive the password its content is read with.
#[tauri::command(async)]
pub fn archive_set_password(core: CoreState<'_>, id: u64, password: String) -> Result<()> {
    core.engine
        .sessions
        .get(id)?
        .remember(Password::new(password));
    Ok(())
}

/// The format of a file that is an archive BoxIt reads; nothing for any
/// other file and for a folder.
#[tauri::command(async)]
pub fn recognize(path: String) -> Option<ArchiveFormat> {
    archive::recognize(Path::new(&path))
}

// Operations

/// Where the content of an open archive goes for a choice of the dialog.
#[tauri::command(async)]
pub fn extract_destination(
    core: CoreState<'_>,
    id: u64,
    place: ExtractPlace,
    chosen: String,
) -> Result<String> {
    let session = core.engine.sessions.get(id)?;
    Ok(fsx::display(&tasks::destination_for(
        session.path(),
        place,
        &chosen,
    )))
}

/// Queues the extraction of rows of a folder of an open archive; no rows
/// stand for the whole folder. Returns the number of the job.
#[tauri::command(async)]
pub fn extract_start(
    core: CoreState<'_>,
    id: u64,
    base: NodeId,
    rows: Vec<NodeId>,
    destination: String,
    options: ExtractOptions,
) -> Result<u64> {
    let session = core.engine.sessions.get(id)?;
    let destination = destination.trim();
    if destination.is_empty() {
        return Err(Error::new("badDestination").with("path", destination));
    }
    let task = ExtractTask {
        session,
        base,
        rows,
        destination: PathBuf::from(destination),
        options,
    };
    core.admit(|| tasks::extract(&core.engine.jobs, task))
}

/// Queues a test of rows of an open archive, or of all of it. With `sums`
/// the test works out the sums of the files as well.
#[tauri::command(async)]
pub fn test_start(
    core: CoreState<'_>,
    id: u64,
    base: NodeId,
    rows: Vec<NodeId>,
    sums: bool,
) -> Result<u64> {
    let session = core.engine.sessions.get(id)?;
    let options = TestOptions { sums };
    core.admit(|| tasks::test(&core.engine.jobs, session, base, rows, options))
}

/// Queues a repair of an open archive into a new file.
#[tauri::command(async)]
pub fn repair_start(
    core: CoreState<'_>,
    id: u64,
    destination: String,
    overwrite: bool,
) -> Result<u64> {
    let session = core.engine.sessions.get(id)?;
    let destination = destination.trim();
    if destination.is_empty() {
        return Err(Error::new("badOutput").with("path", destination));
    }
    let destination = PathBuf::from(destination);
    core.admit(|| tasks::repair(&core.engine.jobs, session, destination, overwrite))
}

/// The archive a repair of an open one offers, beside it.
#[tauri::command(async)]
pub fn repair_suggest(core: CoreState<'_>, id: u64) -> Result<String> {
    let session = core.engine.sessions.get(id)?;
    Ok(fsx::display(&repair::beside(session.path())))
}

/// Queues the creation of an archive.
#[tauri::command(async)]
pub fn create_start(core: CoreState<'_>, form: CreateForm) -> Result<u64> {
    if form.sources.is_empty() {
        return Err(Error::new("nothingToAdd"));
    }
    if form.destination.trim().is_empty() {
        return Err(Error::new("badOutput").with("path", &form.destination));
    }
    core.admit(|| tasks::create(&core.engine.jobs, form.request()))
}

/// The memory an operation takes, and the memory it may take.
#[derive(Clone, Copy, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Memory {
    pub needed: u64,
    pub available: u64,
}

/// The memory creating an archive with these options takes on this machine.
#[tauri::command(async)]
pub fn create_memory(form: CreateForm) -> Memory {
    let (needed, available) = tasks::memory_to_create(&form.options());
    Memory { needed, available }
}

fn fitting_length(length: u32) -> u32 {
    if (passwords::SHORTEST..=passwords::LONGEST).contains(&length) {
        length
    } else {
        GENERATED_LENGTH
    }
}

/// Draws a password from the system generator.
#[tauri::command(async)]
pub fn password_generate(length: u32, symbols: bool) -> Result<String> {
    Ok(passwords::generate(fitting_length(length), symbols)?
        .expose()
        .to_owned())
}

/// The names of the passwords kept in the credential store.
#[tauri::command(async)]
pub fn password_names(core: CoreState<'_>) -> Vec<String> {
    core.engine.settings().saved_passwords
}

/// Saves a named password in the credential store.
#[tauri::command(async)]
pub fn password_save(core: CoreState<'_>, name: String, password: String) -> Result<Settings> {
    if password.is_empty() {
        return Err(Error::new("passwordRequired"));
    }
    secrets::save(&core.engine, &name, &Password::new(password))
}

/// Forgets a named password.
#[tauri::command(async)]
pub fn password_forget(core: CoreState<'_>, name: String) -> Result<Settings> {
    secrets::forget(&core.engine, &name)
}

/// The text of a named password, to be put into a field.
#[tauri::command(async)]
pub fn password_named(name: String) -> Result<Option<String>> {
    Ok(secrets::named(&name)?.map(|password| password.expose().to_owned()))
}

/// Remembers the password that opened an archive.
#[tauri::command(async)]
pub fn password_remember(path: String, password: String) -> Result<()> {
    if password.is_empty() {
        return Err(Error::new("passwordRequired"));
    }
    secrets::remember_archive(Path::new(path.trim()), &Password::new(password))
}

/// The password remembered for an archive, when there is one.
#[tauri::command(async)]
pub fn password_recall(path: String) -> Result<Option<String>> {
    Ok(secrets::recall_archive(Path::new(path.trim()))?
        .map(|password| password.expose().to_owned()))
}

/// Forgets the password remembered for an archive.
#[tauri::command(async)]
pub fn password_forget_archive(path: String) -> Result<()> {
    secrets::forget_archive(Path::new(path.trim()))
}

/// The name a new archive of these files and folders is offered.
#[tauri::command(async)]
pub fn create_suggest(sources: Vec<String>, format: ArchiveFormat) -> Option<String> {
    let sources: Vec<PathBuf> = sources.iter().map(PathBuf::from).collect();
    tasks::archive_for(&sources, format).map(|path| fsx::display(&path))
}

/// What stands at a path: `file`, `folder`, or `missing`.
#[tauri::command(async)]
pub fn path_state(path: String) -> &'static str {
    match fsx::existing(Path::new(&path)) {
        Existing::Nothing => "missing",
        Existing::Folder => "folder",
        Existing::File | Existing::Link => "file",
    }
}

/// Extracts rows of an open archive into a temporary folder to be dragged
/// out of the window. Returns what to drag.
#[tauri::command(async)]
pub fn drag_out(
    core: CoreState<'_>,
    id: u64,
    base: NodeId,
    rows: Vec<NodeId>,
) -> Result<Vec<String>> {
    let session = core.engine.sessions.get(id)?;
    let engine = &core.engine;
    let drag =
        core.admit(|| tasks::drag_start(&engine.jobs, session, base, rows, &engine.paths.temp()))?;
    let taken = tasks::drag_finish(&engine.jobs, &drag)?;
    Ok(taken.iter().map(|path| fsx::display(path)).collect())
}

// The queue

#[tauri::command(async)]
pub fn jobs_list(core: CoreState<'_>) -> Vec<JobInfo> {
    core.engine.jobs.list()
}

#[tauri::command(async)]
pub fn job_cancel(core: CoreState<'_>, id: u64) {
    core.engine.jobs.cancel(id);
}

#[tauri::command(async)]
pub fn job_pause(core: CoreState<'_>, id: u64, paused: bool) {
    core.engine.jobs.set_paused(id, paused);
}

/// Answers the question a job stopped at. `false` when the job does not
/// wait for this answer any more.
#[tauri::command(async)]
pub fn job_answer(core: CoreState<'_>, id: u64, question: u64, reply: Reply) -> bool {
    core.engine.jobs.answer(id, question, reply)
}

/// Forgets the jobs that have finished.
#[tauri::command(async)]
pub fn jobs_clear(core: CoreState<'_>) {
    core.engine.jobs.clear_finished();
}

// Updates

/// What is known about updates in this session.
#[tauri::command(async)]
pub fn update_status(core: CoreState<'_>) -> UpdateStatus {
    core.updates.status()
}

/// Looks for a newer version: the only request BoxIt sends to the network,
/// and only when the user asks for it. The answer arrives as
/// `update:status` events.
#[tauri::command(async)]
pub fn update_check(core: CoreState<'_>) {
    update::check_now(&core);
}

/// Downloads and installs the version the last check found.
#[tauri::command(async)]
pub fn update_install(core: CoreState<'_>) -> Result<()> {
    update::install_now(&core)
}

/// Opens the page of the version found in the browser of the system.
#[tauri::command(async)]
pub fn update_page(core: CoreState<'_>) -> Result<()> {
    update::open_page(&core)
}

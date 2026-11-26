//! The application core of the window: owns the engine and everything that
//! lives outside the web view.

use std::collections::HashMap;
use std::ffi::OsString;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError, Weak};
use std::thread;
use std::time::{Duration, Instant};

use serde::Serialize;
use tauri::window::{ProgressBarState, ProgressBarStatus};
use tauri::{AppHandle, Emitter, Manager};

use boxit_core::{Control, Error, Result};
use boxit_engine::fsx::{self, Existing};
use boxit_engine::jobs::{JobInfo, JobState};
use boxit_engine::{text, Engine, Settings};

use crate::update::Tracker;

/// Label of the main window in `tauri.conf.json`.
pub const MAIN_WINDOW: &str = "main";

/// The window is created hidden and shown by the interface after its first
/// paint, so it never flashes an empty page. Should the interface fail to
/// start, the window is shown anyway after this time: an empty window can be
/// closed, a missing one cannot.
const SHOW_FALLBACK: Duration = Duration::from_secs(4);

pub fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(PoisonError::into_inner)
}

#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct AppInfo {
    pub version: String,
    pub data_dir: String,
    /// Settings and logs are kept next to the program.
    pub portable: bool,
    pub system_language: String,
    /// Language of the interface: the setting, or the system language.
    pub language: String,
}

/// What the button of the window on the taskbar shows of the operations.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Taskbar {
    Idle,
    /// Percent of the work done.
    Working(u64),
    /// The amount of work is not known.
    Busy,
    /// Paused, or waiting for an answer.
    Waiting(u64),
}

impl Taskbar {
    fn of(info: &JobInfo) -> Self {
        let percent = match info.bytes_total {
            Some(total) if total > 0 => {
                Some((u128::from(info.bytes_done) * 100 / u128::from(total)).min(100) as u64)
            }
            _ => None,
        };
        match info.state {
            JobState::Paused => Self::Waiting(percent.unwrap_or(0)),
            JobState::Running if info.question.is_some() => Self::Waiting(percent.unwrap_or(0)),
            JobState::Running => percent.map_or(Self::Busy, Self::Working),
            JobState::Queued | JobState::Done | JobState::Failed | JobState::Cancelled => {
                Self::Idle
            }
        }
    }

    fn state(self) -> ProgressBarState {
        let (status, progress) = match self {
            Self::Idle => (ProgressBarStatus::None, None),
            Self::Working(percent) => (ProgressBarStatus::Normal, Some(percent)),
            Self::Busy => (ProgressBarStatus::Indeterminate, None),
            Self::Waiting(percent) => (ProgressBarStatus::Paused, Some(percent)),
        };
        ProgressBarState {
            status: Some(status),
            progress,
        }
    }
}

pub struct Core {
    app: AppHandle,
    pub engine: Arc<Engine>,
    shown: AtomicBool,
    /// When the process started, for the time the window took to appear.
    started: Instant,
    /// Archives whose lists are being read, by the number the interface
    /// gave to each, so it can stop the reading.
    opening: Mutex<HashMap<u64, Control>>,
    /// The look at a file that is being read: a new one stops it.
    preview: Mutex<Control>,
    /// Files and folders BoxIt was started with, until the interface takes
    /// them.
    opened: Mutex<Vec<String>>,
    pub updates: Tracker,
    /// Set once the installer of an update is on its way: no operation
    /// starts after that. An operation is added with this lock held, so
    /// none slips in between the look at the queue and the installer.
    installing: Mutex<bool>,
}

/// The files and folders among the arguments of a start, the name of the
/// program left out. A relative path is one in `folder`, the folder the
/// program was started in.
pub fn paths_among(
    arguments: impl IntoIterator<Item = OsString>,
    folder: Option<&Path>,
) -> Vec<String> {
    arguments
        .into_iter()
        .skip(1)
        .map(PathBuf::from)
        .map(|path| match folder {
            Some(folder) if path.is_relative() => folder.join(path),
            _ => path,
        })
        .filter(|path| fsx::existing(path) != Existing::Nothing)
        .map(|path| fsx::display(&path))
        .collect()
}

impl Core {
    pub fn app(&self) -> &AppHandle {
        &self.app
    }

    pub fn lang(&self) -> String {
        self.engine.language()
    }

    pub fn info(&self) -> AppInfo {
        AppInfo {
            version: env!("CARGO_PKG_VERSION").to_owned(),
            data_dir: self.engine.paths.root.display().to_string(),
            portable: self.engine.paths.portable,
            system_language: text::system_language().to_owned(),
            language: self.engine.language(),
        }
    }

    pub fn set_settings(&self, settings: Settings) -> Result<Settings> {
        self.engine.set_settings(settings)
    }

    /// Shows the main window and brings it to the front.
    pub fn show_window(&self) {
        if !self.shown.swap(true, Ordering::Relaxed) {
            // The cold start: from the start of the process to a window
            // with its content on the screen.
            tracing::info!(
                cold_start_ms = self.started.elapsed().as_millis() as u64,
                "the window is shown"
            );
        }
        if let Some(window) = self.app.get_webview_window(MAIN_WINDOW) {
            let _ = window.unminimize();
            let _ = window.show();
            let _ = window.set_focus();
        }
    }

    /// Starts watching the reading of a list under the number `token`.
    pub fn begin_opening(&self, token: u64) -> Control {
        let control = Control::new();
        lock(&self.opening).insert(token, control.clone());
        control
    }

    pub fn end_opening(&self, token: u64) {
        lock(&self.opening).remove(&token);
    }

    /// Stops the reading of the list that was started under `token`.
    pub fn cancel_opening(&self, token: u64) {
        if let Some(control) = lock(&self.opening).get(&token) {
            control.cancel();
        }
    }

    /// Stops the look that is being read and starts watching a new one.
    pub fn begin_preview(&self) -> Control {
        let control = Control::new();
        let previous = std::mem::replace(&mut *lock(&self.preview), control.clone());
        previous.cancel();
        control
    }

    /// What BoxIt was started with; handed out once.
    pub fn take_opened(&self) -> Vec<String> {
        std::mem::take(&mut *lock(&self.opened))
    }

    /// Adds an operation to the queue with `add`, unless an update is
    /// being installed: BoxIt is about to be replaced then.
    pub fn admit<T>(&self, add: impl FnOnce() -> T) -> Result<T> {
        let installing = lock(&self.installing);
        if *installing {
            return Err(Error::new("updateInstalling"));
        }
        Ok(add())
    }

    /// Closes the queue for the installer of an update. `false` while an
    /// operation is queued or running: the installer waits for it.
    pub fn begin_install(&self) -> bool {
        let mut installing = lock(&self.installing);
        if self.engine.jobs.is_busy() {
            return false;
        }
        *installing = true;
        true
    }

    /// The installer did not start: operations may be added again.
    pub fn cancel_install(&self) {
        *lock(&self.installing) = false;
    }
}

/// Tells the interface about every change of an operation, and shows the
/// running one on the taskbar.
fn watch_jobs(app: &AppHandle, engine: &Arc<Engine>) {
    let app = app.clone();
    // The engine owns the listener: holding the engine here would keep
    // both alive for ever.
    let owner: Weak<Engine> = Arc::downgrade(engine);
    let shown = Mutex::new(Taskbar::Idle);
    engine.jobs.set_listener(Arc::new(move |info: &JobInfo| {
        let _ = app.emit("jobs:changed", info);
        let taskbar = Taskbar::of(info);
        let busy = owner.upgrade().is_some_and(|engine| engine.jobs.is_busy());
        if taskbar == Taskbar::Idle && busy {
            // Another operation is at work or waits its turn: its own
            // changes say what to show.
            return;
        }
        let mut shown = lock(&shown);
        if *shown != taskbar {
            *shown = taskbar;
            if let Some(window) = app.get_webview_window(MAIN_WINDOW) {
                let _ = window.set_progress_bar(taskbar.state());
            }
        }
    }));
}

/// Builds the core and puts it into the app state. Called once, from `setup`.
pub fn setup(app: &AppHandle, engine: Arc<Engine>, started: Instant, opened: Vec<String>) {
    tracing::info!(
        version = env!("CARGO_PKG_VERSION"),
        data = %engine.paths.root.display(),
        portable = engine.paths.portable,
        "BoxIt started"
    );
    watch_jobs(app, &engine);
    let core = Arc::new(Core {
        app: app.clone(),
        engine,
        shown: AtomicBool::new(false),
        started,
        opening: Mutex::new(HashMap::new()),
        preview: Mutex::new(Control::new()),
        opened: Mutex::new(opened),
        updates: Tracker::new(),
        installing: Mutex::new(false),
    });
    app.manage(core.clone());
    thread::spawn(move || {
        thread::sleep(SHOW_FALLBACK);
        if !core.shown.load(Ordering::Relaxed) {
            tracing::warn!("the interface did not report its first paint; showing the window");
            core.show_window();
        }
    });
}

/// A second copy of BoxIt was started: the running one comes to the front
/// and opens what the second one was started with.
pub fn second_instance(app: &AppHandle, arguments: &[String], folder: &str) {
    let Some(core) = app.try_state::<Arc<Core>>() else {
        return;
    };
    core.show_window();
    let folder = Some(Path::new(folder)).filter(|folder| !folder.as_os_str().is_empty());
    let paths = paths_among(arguments.iter().map(OsString::from), folder);
    if !paths.is_empty() {
        let _ = app.emit("open:files", &paths);
    }
}

#[cfg(test)]
mod tests {
    use std::fs;

    use super::*;

    #[test]
    fn the_paths_of_a_start_are_the_arguments_that_exist() {
        let folder = std::env::temp_dir().join(format!("boxit-start-{}", fsx::unique_suffix()));
        fs::create_dir_all(folder.join("inner")).unwrap();
        fs::write(folder.join("a.zip"), b"").unwrap();
        let arguments =
            |list: &[&str]| -> Vec<OsString> { list.iter().map(OsString::from).collect() };

        let absolute = folder.join("a.zip");
        let found = paths_among(
            arguments(&["BoxIt.exe", "a.zip", "inner", "missing.zip", "--flag"])
                .into_iter()
                .chain([absolute.clone().into_os_string()]),
            Some(&folder),
        );
        assert_eq!(
            found,
            [
                fsx::display(&folder.join("a.zip")),
                fsx::display(&folder.join("inner")),
                fsx::display(&absolute),
            ]
        );
        // The program itself is not something to open.
        assert_eq!(
            paths_among([absolute.into_os_string()], None),
            Vec::<String>::new()
        );
        fs::remove_dir_all(&folder).unwrap();
    }

    #[test]
    fn the_taskbar_follows_the_operation() {
        let info = |state, done, total: Option<u64>| JobInfo {
            id: 1,
            kind: boxit_engine::jobs::JobKind::Extract,
            archive: String::new(),
            target: None,
            state,
            bytes_done: done,
            bytes_total: total,
            entries_done: 0,
            entries_total: None,
            current: String::new(),
            speed: None,
            eta_seconds: None,
            question: None,
            error: None,
            result: None,
        };
        assert_eq!(
            Taskbar::of(&info(JobState::Running, 50, Some(200))),
            Taskbar::Working(25)
        );
        assert_eq!(
            Taskbar::of(&info(JobState::Running, 5, None)),
            Taskbar::Busy
        );
        assert_eq!(
            Taskbar::of(&info(JobState::Paused, 300, Some(200))),
            Taskbar::Waiting(100)
        );
        assert_eq!(
            Taskbar::of(&info(JobState::Done, 200, Some(200))),
            Taskbar::Idle
        );
        assert_eq!(
            Taskbar::of(&info(JobState::Queued, 0, Some(200))),
            Taskbar::Idle
        );
    }
}

//! One queue for the long operations on archives: extraction, creation,
//! test.
//!
//! An operation already takes every processor, so the jobs run one at a
//! time, in the order they were added. A job reports its progress, can be
//! paused, resumed and cancelled, may stop to ask its user a question, and
//! announces every change through the listener: that is what the window
//! draws its list of operations and the progress on the taskbar from.

use std::collections::{HashMap, VecDeque};
use std::panic::{self, AssertUnwindSafe};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::mpsc::{self, RecvTimeoutError, Sender};
use std::sync::{Arc, Condvar, Mutex, MutexGuard, PoisonError};
use std::thread;
use std::time::{Duration, Instant};

use boxit_core::{
    Budget, Conflict, ConflictAnswer, Control, EntryMeta, Error, Interaction, OpContext, Password,
    PasswordRequest, Progress, Result,
};
use serde::{Deserialize, Serialize};
use serde_json::Value;

/// Finished jobs kept in the list.
const FINISHED_KEPT: usize = 50;

/// How often a running job announces its progress.
const TICK: Duration = Duration::from_millis(100);

/// How often a waiting job looks whether it was cancelled.
const POLL: Duration = Duration::from_millis(200);

/// The time left is not told before a job has worked this long: the first
/// moments of an operation say little about the rest of it.
const ETA_AFTER: Duration = Duration::from_secs(2);

/// How much the speed of the last tick weighs in the speed that is shown.
const SPEED_WEIGHT: f64 = 0.1;

/// The counter of a total that is not known.
const UNKNOWN: u64 = u64::MAX;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum JobKind {
    Extract,
    Create,
    Test,
    Repair,
    Update,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum JobState {
    Queued,
    Running,
    Paused,
    Done,
    Failed,
    Cancelled,
}

impl JobState {
    pub fn is_finished(self) -> bool {
        matches!(self, Self::Done | Self::Failed | Self::Cancelled)
    }
}

/// What a job stopped to ask. The number tells the answer which question
/// it is for.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
#[serde(
    tag = "kind",
    rename_all = "camelCase",
    rename_all_fields = "camelCase"
)]
pub enum Question {
    Password {
        id: u64,
        archive: String,
        /// The entry that needs the password, when it is one entry.
        entry: Option<String>,
        /// The password given before was wrong.
        retry: bool,
    },
    Conflict {
        id: u64,
        /// The file that is in the way.
        path: String,
        existing_size: u64,
        /// Seconds since 1970.
        existing_modified: Option<i64>,
        size: Option<u64>,
        modified: Option<i64>,
    },
    Symlink {
        id: u64,
        name: String,
        target: String,
    },
}

impl Question {
    fn id(&self) -> u64 {
        match self {
            Self::Password { id, .. } | Self::Conflict { id, .. } | Self::Symlink { id, .. } => *id,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum ConflictChoice {
    Skip,
    Overwrite,
    Rename,
    /// Stop the whole operation.
    Cancel,
}

/// The answer to a [`Question`].
#[derive(Clone, PartialEq, Eq, Deserialize)]
#[serde(
    tag = "kind",
    rename_all = "camelCase",
    rename_all_fields = "camelCase"
)]
pub enum Reply {
    /// `None` gives up on what needed the password.
    Password {
        password: Option<String>,
    },
    Conflict {
        choice: ConflictChoice,
        /// The same for every later conflict of the operation.
        #[serde(default)]
        all: bool,
    },
    Symlink {
        create: bool,
    },
}

impl Reply {
    /// Whether this is an answer to a question of that kind.
    fn fits(&self, question: &Question) -> bool {
        matches!(
            (self, question),
            (Self::Password { .. }, Question::Password { .. })
                | (Self::Conflict { .. }, Question::Conflict { .. })
                | (Self::Symlink { .. }, Question::Symlink { .. })
        )
    }
}

/// The public picture of a job.
#[derive(Clone, Debug, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct JobInfo {
    pub id: u64,
    pub kind: JobKind,
    /// The archive the job reads or writes.
    pub archive: String,
    /// The folder an extraction goes to.
    pub target: Option<String>,
    pub state: JobState,
    /// Bytes of content processed so far.
    pub bytes_done: u64,
    pub bytes_total: Option<u64>,
    pub entries_done: u64,
    pub entries_total: Option<u64>,
    /// What is being worked on right now.
    pub current: String,
    /// Bytes of content a second, lately.
    pub speed: Option<u64>,
    /// Seconds left, once enough work is done to tell.
    pub eta_seconds: Option<u64>,
    /// The job waits for an answer to this.
    pub question: Option<Question>,
    pub error: Option<Error>,
    /// What the job did, once it is done.
    pub result: Option<Value>,
}

/// Called on every change of a job.
pub type Listener = Arc<dyn Fn(&JobInfo) + Send + Sync>;

fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(PoisonError::into_inner)
}

struct Entry {
    info: JobInfo,
    control: Control,
    /// Where the answer to the question of the job goes.
    answer: Option<Sender<Reply>>,
}

#[derive(Default)]
struct State {
    jobs: HashMap<u64, Entry>,
    /// Every job in the order it was added.
    order: Vec<u64>,
    running: Option<u64>,
    waiting: VecDeque<u64>,
}

struct Shared {
    state: Mutex<State>,
    changed: Condvar,
    listener: Mutex<Option<Listener>>,
    next_job: AtomicU64,
    next_question: AtomicU64,
}

impl Shared {
    fn announce(&self, info: &JobInfo) {
        let listener = lock(&self.listener).clone();
        if let Some(listener) = listener {
            listener(info);
        }
    }

    /// Changes the picture of a job and announces it.
    fn update(&self, id: u64, change: impl FnOnce(&mut Entry)) {
        let info = {
            let mut state = lock(&self.state);
            let Some(entry) = state.jobs.get_mut(&id) else {
                return;
            };
            change(entry);
            entry.info.clone()
        };
        self.announce(&info);
    }
}

/// What a running job reports its progress through and asks its questions
/// with: the progress and the interaction of its operation.
pub struct Ticket {
    id: u64,
    shared: Arc<Shared>,
    control: Control,
    bytes: AtomicU64,
    entries: AtomicU64,
    total_bytes: AtomicU64,
    total_entries: AtomicU64,
    current: Mutex<String>,
    /// One question at a time, however many threads have one.
    asking: Mutex<()>,
}

impl Ticket {
    pub fn id(&self) -> u64 {
        self.id
    }

    pub fn control(&self) -> &Control {
        &self.control
    }

    /// The context of the operation of this job.
    pub fn context(self: &Arc<Self>, budget: Budget) -> OpContext {
        OpContext {
            control: self.control.clone(),
            progress: Arc::clone(self) as Arc<dyn Progress>,
            interaction: Arc::clone(self) as Arc<dyn Interaction>,
            budget,
        }
    }

    /// Shows a question and waits for its answer. `None` when the job is
    /// cancelled before anyone answers.
    fn ask(&self, question: impl FnOnce(u64) -> Question) -> Option<Reply> {
        let _turn = lock(&self.asking);
        if self.control.is_cancelled() {
            return None;
        }
        let question = question(self.shared.next_question.fetch_add(1, Ordering::Relaxed));
        let (sender, receiver) = mpsc::channel();
        self.shared.update(self.id, |entry| {
            entry.info.question = Some(question);
            entry.info.speed = None;
            entry.info.eta_seconds = None;
            entry.answer = Some(sender);
        });
        let reply = loop {
            match receiver.recv_timeout(POLL) {
                Ok(reply) => break Some(reply),
                Err(RecvTimeoutError::Timeout) if self.control.is_cancelled() => break None,
                Err(RecvTimeoutError::Timeout) => {}
                Err(RecvTimeoutError::Disconnected) => break None,
            }
        };
        self.shared.update(self.id, |entry| {
            entry.info.question = None;
            entry.answer = None;
        });
        reply
    }
}

impl Progress for Ticket {
    fn total(&self, bytes: Option<u64>, entries: Option<u64>) {
        self.total_bytes
            .store(bytes.unwrap_or(UNKNOWN), Ordering::Relaxed);
        self.total_entries
            .store(entries.unwrap_or(UNKNOWN), Ordering::Relaxed);
    }

    fn entry(&self, path: &str) {
        self.entries.fetch_add(1, Ordering::Relaxed);
        let mut current = lock(&self.current);
        current.clear();
        current.push_str(path);
    }

    fn advance(&self, bytes: u64) {
        self.bytes.fetch_add(bytes, Ordering::Relaxed);
    }
}

impl Interaction for Ticket {
    fn password(&self, request: &PasswordRequest) -> Option<Password> {
        let reply = self.ask(|id| Question::Password {
            id,
            archive: request.archive.clone(),
            entry: request.entry.clone(),
            retry: request.retry,
        });
        match reply {
            Some(Reply::Password {
                password: Some(password),
            }) => Some(Password::new(password)),
            _ => None,
        }
    }

    fn conflict(&self, conflict: &Conflict) -> ConflictAnswer {
        let reply = self.ask(|id| Question::Conflict {
            id,
            path: conflict.path.clone(),
            existing_size: conflict.existing_size,
            existing_modified: conflict.existing_modified.map(|time| time.seconds),
            size: conflict.entry.size,
            modified: conflict.entry.modified.map(|time| time.seconds),
        });
        match reply {
            Some(Reply::Conflict { choice, all }) => match (choice, all) {
                (ConflictChoice::Skip, false) => ConflictAnswer::Skip,
                (ConflictChoice::Skip, true) => ConflictAnswer::SkipAll,
                (ConflictChoice::Overwrite, false) => ConflictAnswer::Overwrite,
                (ConflictChoice::Overwrite, true) => ConflictAnswer::OverwriteAll,
                (ConflictChoice::Rename, false) => ConflictAnswer::Rename,
                (ConflictChoice::Rename, true) => ConflictAnswer::RenameAll,
                (ConflictChoice::Cancel, _) => ConflictAnswer::Cancel,
            },
            // Nobody answered: the job was cancelled.
            _ => ConflictAnswer::Cancel,
        }
    }

    fn symlink(&self, link: &EntryMeta) -> bool {
        let reply = self.ask(|id| Question::Symlink {
            id,
            name: link.path.clone(),
            target: link.link_target.clone().unwrap_or_default(),
        });
        matches!(reply, Some(Reply::Symlink { create: true }))
    }
}

/// The speed of a job, from one tick to the next.
struct Meter {
    started: Instant,
    /// Time the job spent working: not paused, not waiting for an answer.
    worked: Duration,
    at: Instant,
    bytes: u64,
    /// Bytes a second, smoothed.
    speed: f64,
}

impl Meter {
    fn new() -> Self {
        let now = Instant::now();
        Self {
            started: now,
            worked: Duration::ZERO,
            at: now,
            bytes: 0,
            speed: 0.0,
        }
    }

    /// Takes the count of bytes at this moment. `working` is false while
    /// the job stands still for a reason of its user.
    fn tick(&mut self, bytes: u64, working: bool) {
        let now = Instant::now();
        let passed = now.duration_since(self.at);
        if working && passed > Duration::ZERO {
            let lately = bytes.saturating_sub(self.bytes) as f64 / passed.as_secs_f64();
            self.speed = if self.worked == Duration::ZERO {
                lately
            } else {
                self.speed + (lately - self.speed) * SPEED_WEIGHT
            };
            self.worked += passed;
        }
        self.at = now;
        self.bytes = bytes;
    }

    fn speed(&self) -> Option<u64> {
        (self.speed >= 1.0).then_some(self.speed as u64)
    }

    fn eta(&self, done: u64, total: Option<u64>) -> Option<u64> {
        let left = total?.checked_sub(done)?;
        if left == 0 || self.worked < ETA_AFTER || self.speed < 1.0 {
            return None;
        }
        Some((left as f64 / self.speed).ceil() as u64)
    }
}

fn known(total: &AtomicU64) -> Option<u64> {
    Some(total.load(Ordering::Relaxed)).filter(|total| *total != UNKNOWN)
}

/// Brings the picture of a job up to its counters.
fn refresh(ticket: &Ticket, meter: &mut Meter) {
    let bytes = ticket.bytes.load(Ordering::Relaxed);
    let entries = ticket.entries.load(Ordering::Relaxed);
    let info = {
        let mut state = lock(&ticket.shared.state);
        let Some(entry) = state.jobs.get_mut(&ticket.id) else {
            return;
        };
        let working = entry.info.state == JobState::Running && entry.info.question.is_none();
        meter.tick(bytes, working);
        let before = entry.info.clone();
        let info = &mut entry.info;
        info.bytes_done = bytes;
        info.entries_done = entries;
        info.bytes_total = known(&ticket.total_bytes);
        info.entries_total = known(&ticket.total_entries);
        info.current.clone_from(&lock(&ticket.current));
        if working {
            info.speed = meter.speed();
            info.eta_seconds = meter.eta(bytes, info.bytes_total);
        } else {
            info.speed = None;
            info.eta_seconds = None;
        }
        if *info == before {
            return;
        }
        info.clone()
    };
    ticket.shared.announce(&info);
}

/// The job queue. Cloning gives another handle to the same queue.
#[derive(Clone)]
pub struct Jobs {
    shared: Arc<Shared>,
}

impl Default for Jobs {
    fn default() -> Self {
        Self::new()
    }
}

impl Jobs {
    pub fn new() -> Self {
        Self {
            shared: Arc::new(Shared {
                state: Mutex::new(State::default()),
                changed: Condvar::new(),
                listener: Mutex::new(None),
                next_job: AtomicU64::new(1),
                next_question: AtomicU64::new(1),
            }),
        }
    }

    /// Sets the function called on every change of a job.
    pub fn set_listener(&self, listener: Listener) {
        *lock(&self.shared.listener) = Some(listener);
    }

    /// Jobs in the order they were added.
    pub fn list(&self) -> Vec<JobInfo> {
        let state = lock(&self.shared.state);
        state
            .order
            .iter()
            .filter_map(|id| state.jobs.get(id).map(|entry| entry.info.clone()))
            .collect()
    }

    pub fn get(&self, id: u64) -> Option<JobInfo> {
        lock(&self.shared.state)
            .jobs
            .get(&id)
            .map(|entry| entry.info.clone())
    }

    /// Whether any job is queued, running or paused.
    pub fn is_busy(&self) -> bool {
        lock(&self.shared.state)
            .jobs
            .values()
            .any(|entry| !entry.info.state.is_finished())
    }

    /// Adds a job; it starts when the ones before it are finished.
    pub fn spawn<F>(&self, kind: JobKind, archive: &str, target: Option<&str>, work: F) -> u64
    where
        F: FnOnce(&Arc<Ticket>) -> Result<Value> + Send + 'static,
    {
        let id = self.shared.next_job.fetch_add(1, Ordering::Relaxed);
        let control = Control::new();
        let info = JobInfo {
            id,
            kind,
            archive: archive.to_owned(),
            target: target.map(str::to_owned),
            state: JobState::Queued,
            bytes_done: 0,
            bytes_total: None,
            entries_done: 0,
            entries_total: None,
            current: String::new(),
            speed: None,
            eta_seconds: None,
            question: None,
            error: None,
            result: None,
        };
        {
            let mut state = lock(&self.shared.state);
            state.jobs.insert(
                id,
                Entry {
                    info: info.clone(),
                    control: control.clone(),
                    answer: None,
                },
            );
            state.order.push(id);
            state.waiting.push_back(id);
            prune(&mut state);
        }
        self.shared.announce(&info);

        let ticket = Arc::new(Ticket {
            id,
            shared: Arc::clone(&self.shared),
            control,
            bytes: AtomicU64::new(0),
            entries: AtomicU64::new(0),
            total_bytes: AtomicU64::new(UNKNOWN),
            total_entries: AtomicU64::new(UNKNOWN),
            current: Mutex::new(String::new()),
            asking: Mutex::new(()),
        });
        let spawned = thread::Builder::new()
            .name(format!("boxit-job-{id}"))
            .spawn(move || run(&ticket, work));
        if let Err(error) = spawned {
            finish(&self.shared, id, Err(Error::new("internal").detail(error)));
        }
        id
    }

    /// Asks a job to stop. A queued job never starts.
    pub fn cancel(&self, id: u64) {
        if let Some(entry) = lock(&self.shared.state).jobs.get(&id) {
            entry.control.cancel();
        }
        self.shared.changed.notify_all();
    }

    /// Asks every unfinished job to stop.
    pub fn cancel_all(&self) {
        for entry in lock(&self.shared.state).jobs.values() {
            entry.control.cancel();
        }
        self.shared.changed.notify_all();
    }

    /// Pauses or resumes a running job at its next checkpoint.
    pub fn set_paused(&self, id: u64, paused: bool) {
        self.shared.update(id, |entry| {
            let next = match (entry.info.state, paused) {
                (JobState::Running, true) => JobState::Paused,
                (JobState::Paused, false) => JobState::Running,
                _ => return,
            };
            entry.control.set_paused(paused);
            entry.info.state = next;
            if paused {
                entry.info.speed = None;
                entry.info.eta_seconds = None;
            }
        });
    }

    /// Hands the answer to the question a job waits with. `false` when the
    /// job has no such question any more, or the answer is to another kind
    /// of question.
    pub fn answer(&self, id: u64, question: u64, reply: Reply) -> bool {
        let state = lock(&self.shared.state);
        let Some(entry) = state.jobs.get(&id) else {
            return false;
        };
        match (&entry.info.question, &entry.answer) {
            (Some(asked), Some(sender)) if asked.id() == question && reply.fits(asked) => {
                sender.send(reply).is_ok()
            }
            _ => false,
        }
    }

    /// Removes finished jobs from the list.
    pub fn clear_finished(&self) {
        let mut state = lock(&self.shared.state);
        let State { jobs, order, .. } = &mut *state;
        jobs.retain(|_, entry| !entry.info.state.is_finished());
        order.retain(|id| jobs.contains_key(id));
    }

    /// Blocks until the job finishes and returns its final picture.
    pub fn wait(&self, id: u64) -> Option<JobInfo> {
        let mut state = lock(&self.shared.state);
        loop {
            let info = state.jobs.get(&id).map(|entry| entry.info.clone())?;
            if info.state.is_finished() {
                return Some(info);
            }
            state = self
                .shared
                .changed
                .wait_timeout(state, POLL)
                .unwrap_or_else(PoisonError::into_inner)
                .0;
        }
    }
}

/// Drops the oldest finished jobs above the kept number.
fn prune(state: &mut State) {
    let State { jobs, order, .. } = state;
    let finished = |jobs: &HashMap<u64, Entry>, id: &u64| {
        jobs.get(id)
            .is_some_and(|entry| entry.info.state.is_finished())
    };
    let count = order.iter().filter(|id| finished(jobs, id)).count();
    let mut extra = count.saturating_sub(FINISHED_KEPT);
    order.retain(|id| {
        let dropped = extra > 0 && finished(jobs, id);
        if dropped {
            extra -= 1;
            jobs.remove(id);
        }
        !dropped
    });
}

/// Waits for the turn of a job. `false` means the job was cancelled while
/// it waited.
fn wait_turn(shared: &Shared, id: u64, control: &Control) -> bool {
    let mut state = lock(&shared.state);
    loop {
        if control.is_cancelled() {
            state.waiting.retain(|known| *known != id);
            return false;
        }
        if state.running.is_none() && state.waiting.front() == Some(&id) {
            state.waiting.pop_front();
            state.running = Some(id);
            return true;
        }
        state = shared
            .changed
            .wait_timeout(state, POLL)
            .unwrap_or_else(PoisonError::into_inner)
            .0;
    }
}

fn finish(shared: &Shared, id: u64, result: Result<Value>) {
    let info = {
        let mut state = lock(&shared.state);
        if state.running == Some(id) {
            state.running = None;
        }
        state.waiting.retain(|known| *known != id);
        let Some(entry) = state.jobs.get_mut(&id) else {
            shared.changed.notify_all();
            return;
        };
        let info = &mut entry.info;
        info.speed = None;
        info.eta_seconds = None;
        info.question = None;
        match result {
            Ok(value) => {
                info.state = JobState::Done;
                info.result = Some(value);
            }
            Err(error) if error.is_cancelled() || entry.control.is_cancelled() => {
                info.state = JobState::Cancelled;
            }
            Err(error) => {
                info.state = JobState::Failed;
                info.error = Some(error);
            }
        }
        entry.answer = None;
        entry.info.clone()
    };
    shared.changed.notify_all();
    shared.announce(&info);
}

fn run<F>(ticket: &Arc<Ticket>, work: F)
where
    F: FnOnce(&Arc<Ticket>) -> Result<Value>,
{
    let shared = &ticket.shared;
    if !wait_turn(shared, ticket.id, &ticket.control) {
        finish(shared, ticket.id, Err(Error::cancelled()));
        return;
    }
    shared.update(ticket.id, |entry| entry.info.state = JobState::Running);
    let mut meter = Meter::new();
    let done = AtomicBool::new(false);
    let result = thread::scope(|scope| {
        let ticker = scope.spawn(|| {
            while !done.load(Ordering::Acquire) {
                thread::park_timeout(TICK);
                refresh(ticket, &mut meter);
            }
        });
        // A panic stops the job, not the program.
        let result = panic::catch_unwind(AssertUnwindSafe(|| work(ticket)))
            .unwrap_or_else(|_| Err(Error::new("internal").detail("the operation panicked")));
        done.store(true, Ordering::Release);
        ticker.thread().unpark();
        result
    });
    // The last tick may have come before the last bytes.
    refresh(ticket, &mut meter);
    tracing::debug!(
        job = ticket.id,
        seconds = meter.started.elapsed().as_secs_f64(),
        "a job has ended"
    );
    finish(shared, ticket.id, result);
}

#[cfg(test)]
mod tests {
    use std::sync::mpsc::channel;

    use boxit_core::Timestamp;
    use serde_json::json;

    use super::*;

    /// Waits until the job shows a question and returns it.
    fn question(jobs: &Jobs, id: u64) -> Question {
        loop {
            let info = jobs.get(id).unwrap();
            assert!(!info.state.is_finished(), "the job ended without asking");
            if let Some(question) = info.question {
                return question;
            }
            thread::sleep(Duration::from_millis(5));
        }
    }

    #[test]
    fn a_job_runs_and_reports() {
        let jobs = Jobs::new();
        let (sender, receiver) = channel();
        jobs.set_listener(Arc::new(move |info: &JobInfo| {
            let _ = sender.send(info.state);
        }));
        let id = jobs.spawn(JobKind::Test, "a.zip", None, |ticket| {
            let ctx = ticket.context(Budget::with_threads(2));
            ctx.progress.total(Some(20), Some(2));
            ctx.progress.entry("a");
            ctx.progress.advance(10);
            ctx.checkpoint()?;
            ctx.progress.entry("b");
            ctx.progress.advance(10);
            Ok(json!({ "files": 2 }))
        });
        let info = jobs.wait(id).unwrap();
        assert_eq!(info.state, JobState::Done);
        assert_eq!((info.kind, info.archive.as_str()), (JobKind::Test, "a.zip"));
        assert_eq!((info.bytes_done, info.bytes_total), (20, Some(20)));
        assert_eq!((info.entries_done, info.entries_total), (2, Some(2)));
        assert_eq!(info.current, "b");
        assert_eq!(info.result, Some(json!({ "files": 2 })));
        assert_eq!((info.speed, info.eta_seconds), (None, None));
        assert!(!jobs.is_busy());
        // The listener runs on the job thread. The last notice can arrive
        // a moment after the job is already done.
        let mut states = Vec::new();
        let deadline = std::time::Instant::now() + Duration::from_secs(2);
        while Instant::now() < deadline {
            states.extend(receiver.try_iter());
            if states.last() == Some(&JobState::Done) {
                break;
            }
            thread::sleep(Duration::from_millis(5));
        }
        assert_eq!(states.first(), Some(&JobState::Queued));
        assert!(states.contains(&JobState::Running));
        assert_eq!(states.last(), Some(&JobState::Done));
        assert_eq!(jobs.list().len(), 1);
        jobs.clear_finished();
        assert_eq!(jobs.list(), []);
    }

    #[test]
    fn failure_cancel_and_panic_are_told_apart() {
        let jobs = Jobs::new();
        let failed = jobs.spawn(JobKind::Extract, "a.zip", Some("out"), |_| {
            Err(Error::new("damaged"))
        });
        let info = jobs.wait(failed).unwrap();
        assert_eq!(info.state, JobState::Failed);
        assert_eq!(info.error, Some(Error::new("damaged")));
        assert_eq!(info.target.as_deref(), Some("out"));

        let (started, wait_started) = channel();
        let cancelled = jobs.spawn(JobKind::Extract, "a.zip", None, move |ticket| {
            let _ = started.send(());
            loop {
                ticket.control().checkpoint()?;
                thread::sleep(Duration::from_millis(5));
            }
        });
        wait_started.recv().unwrap();
        jobs.cancel(cancelled);
        let info = jobs.wait(cancelled).unwrap();
        assert_eq!((info.state, info.error), (JobState::Cancelled, None));

        let panicked = jobs.spawn(JobKind::Create, "a.zip", None, |_| panic!("boom"));
        let info = jobs.wait(panicked).unwrap();
        assert_eq!(info.state, JobState::Failed);
        assert_eq!(info.error.unwrap().code, "internal");
    }

    #[test]
    fn jobs_run_one_at_a_time_in_their_order() {
        let jobs = Jobs::new();
        let active = Arc::new(AtomicU64::new(0));
        let overlap = Arc::new(AtomicBool::new(false));
        let order = Arc::new(Mutex::new(Vec::new()));
        let ids: Vec<u64> = (0..4)
            .map(|number| {
                let active = Arc::clone(&active);
                let overlap = Arc::clone(&overlap);
                let order = Arc::clone(&order);
                jobs.spawn(JobKind::Test, "a.zip", None, move |_| {
                    if active.fetch_add(1, Ordering::SeqCst) > 0 {
                        overlap.store(true, Ordering::SeqCst);
                    }
                    lock(&order).push(number);
                    thread::sleep(Duration::from_millis(20));
                    active.fetch_sub(1, Ordering::SeqCst);
                    Ok(Value::Null)
                })
            })
            .collect();
        assert!(jobs.is_busy());
        for id in ids {
            assert_eq!(jobs.wait(id).unwrap().state, JobState::Done);
        }
        assert!(!overlap.load(Ordering::SeqCst));
        assert_eq!(*lock(&order), [0, 1, 2, 3]);
    }

    #[test]
    fn a_queued_job_can_be_cancelled() {
        let jobs = Jobs::new();
        let (release, wait_release) = channel::<()>();
        let first = jobs.spawn(JobKind::Test, "a.zip", None, move |_| {
            let _ = wait_release.recv();
            Ok(Value::Null)
        });
        let second = jobs.spawn(JobKind::Test, "b.zip", None, |_| Ok(Value::Null));
        let third = jobs.spawn(JobKind::Test, "c.zip", None, |_| Ok(Value::Null));
        jobs.cancel(second);
        assert_eq!(jobs.wait(second).unwrap().state, JobState::Cancelled);
        release.send(()).unwrap();
        assert_eq!(jobs.wait(first).unwrap().state, JobState::Done);
        assert_eq!(jobs.wait(third).unwrap().state, JobState::Done);
    }

    #[test]
    fn a_paused_job_waits_at_its_checkpoint() {
        let jobs = Jobs::new();
        let (started, wait_started) = channel();
        let steps = Arc::new(AtomicU64::new(0));
        let counter = Arc::clone(&steps);
        let id = jobs.spawn(JobKind::Test, "a.zip", None, move |ticket| {
            let _ = started.send(());
            for _ in 0..100_000 {
                ticket.control().checkpoint()?;
                counter.fetch_add(1, Ordering::SeqCst);
                thread::sleep(Duration::from_millis(2));
            }
            Ok(Value::Null)
        });
        wait_started.recv().unwrap();
        // The job is running by now, or about to be.
        while jobs.get(id).unwrap().state != JobState::Running {
            thread::sleep(Duration::from_millis(2));
        }
        jobs.set_paused(id, true);
        assert_eq!(jobs.get(id).unwrap().state, JobState::Paused);
        thread::sleep(Duration::from_millis(50));
        let before = steps.load(Ordering::SeqCst);
        thread::sleep(Duration::from_millis(80));
        let after = steps.load(Ordering::SeqCst);
        assert!(after <= before + 1, "{before} {after}");
        jobs.set_paused(id, false);
        assert_eq!(jobs.get(id).unwrap().state, JobState::Running);
        while steps.load(Ordering::SeqCst) <= after + 1 {
            thread::sleep(Duration::from_millis(2));
        }
        // Cancelling wakes a paused job as well.
        jobs.set_paused(id, true);
        jobs.cancel(id);
        assert_eq!(jobs.wait(id).unwrap().state, JobState::Cancelled);
    }

    #[test]
    fn a_job_asks_and_goes_on_with_the_answer() {
        let jobs = Jobs::new();
        let id = jobs.spawn(JobKind::Extract, "a.zip", Some("out"), |ticket| {
            let ctx = ticket.context(Budget::with_threads(1));
            let request = PasswordRequest {
                archive: String::from("a.zip"),
                entry: Some(String::from("secret.txt")),
                retry: true,
            };
            let password = ctx.interaction.password(&request);
            let conflict = Conflict {
                path: String::from("out/a.txt"),
                existing_size: 5,
                existing_modified: Some(Timestamp::from_seconds(100)),
                entry: EntryMeta::file("a.txt", 7),
            };
            let answer = ctx.interaction.conflict(&conflict);
            let link = ctx.interaction.symlink(&EntryMeta::symlink("l", "a.txt"));
            Ok(json!({
                "password": password.map(|password| password.expose().to_owned()),
                "conflict": format!("{answer:?}"),
                "link": link,
            }))
        });

        let asked = question(&jobs, id);
        let Question::Password {
            id: first,
            entry,
            retry,
            ..
        } = &asked
        else {
            panic!("{asked:?}");
        };
        assert_eq!((entry.as_deref(), *retry), (Some("secret.txt"), true));
        // An answer to another question, or of another kind, is not taken.
        let typed = || Reply::Password {
            password: Some(String::from("sesame")),
        };
        assert!(!jobs.answer(id, first + 100, typed()));
        assert!(!jobs.answer(id, *first, Reply::Symlink { create: true }));
        assert!(!jobs.answer(id + 100, *first, typed()));
        assert!(jobs.answer(id, *first, typed()));

        let asked = loop {
            match question(&jobs, id) {
                Question::Password { .. } => thread::sleep(Duration::from_millis(5)),
                other => break other,
            }
        };
        let Question::Conflict {
            id: second,
            path,
            existing_size,
            existing_modified,
            size,
            ..
        } = &asked
        else {
            panic!("{asked:?}");
        };
        assert_eq!(path, "out/a.txt");
        assert_eq!(
            (*existing_size, *existing_modified, *size),
            (5, Some(100), Some(7))
        );
        let reply = Reply::Conflict {
            choice: ConflictChoice::Rename,
            all: true,
        };
        assert!(jobs.answer(id, *second, reply));

        let asked = loop {
            match question(&jobs, id) {
                Question::Symlink { id, .. } => break id,
                _ => thread::sleep(Duration::from_millis(5)),
            }
        };
        assert!(jobs.answer(id, asked, Reply::Symlink { create: true }));

        let info = jobs.wait(id).unwrap();
        assert_eq!(info.question, None);
        assert_eq!(
            info.result,
            Some(json!({ "password": "sesame", "conflict": "RenameAll", "link": true }))
        );
    }

    #[test]
    fn a_job_that_is_cancelled_stops_waiting_for_its_answer() {
        let jobs = Jobs::new();
        let id = jobs.spawn(JobKind::Extract, "a.zip", None, |ticket| {
            let ctx = ticket.context(Budget::with_threads(1));
            let conflict = Conflict {
                path: String::from("a.txt"),
                existing_size: 0,
                existing_modified: None,
                entry: EntryMeta::file("a.txt", 0),
            };
            match ctx.interaction.conflict(&conflict) {
                ConflictAnswer::Cancel => Err(Error::cancelled()),
                other => Ok(json!(format!("{other:?}"))),
            }
        });
        question(&jobs, id);
        jobs.cancel(id);
        let info = jobs.wait(id).unwrap();
        assert_eq!((info.state, info.question), (JobState::Cancelled, None));
    }

    #[test]
    fn questions_and_answers_have_the_shape_the_window_expects() {
        let question = Question::Conflict {
            id: 3,
            path: String::from("a"),
            existing_size: 1,
            existing_modified: None,
            size: Some(2),
            modified: Some(4),
        };
        assert_eq!(
            serde_json::to_value(&question).unwrap(),
            json!({
                "kind": "conflict", "id": 3, "path": "a", "existingSize": 1,
                "existingModified": null, "size": 2, "modified": 4
            })
        );
        let reply: Reply =
            serde_json::from_value(json!({ "kind": "conflict", "choice": "overwrite" })).unwrap();
        assert!(
            reply
                == Reply::Conflict {
                    choice: ConflictChoice::Overwrite,
                    all: false
                }
        );
        let reply: Reply =
            serde_json::from_value(json!({ "kind": "password", "password": null })).unwrap();
        assert!(reply == Reply::Password { password: None });
    }

    #[test]
    fn the_time_left_follows_the_speed() {
        let mut meter = Meter::new();
        assert_eq!(meter.speed(), None);
        assert_eq!(meter.eta(0, Some(100)), None);
        // Ten seconds of work at a hundred bytes a second.
        meter.worked = Duration::from_secs(10);
        meter.speed = 100.0;
        assert_eq!(meter.speed(), Some(100));
        assert_eq!(meter.eta(400, Some(1000)), Some(6));
        assert_eq!(meter.eta(1000, Some(1000)), None);
        assert_eq!(meter.eta(2000, Some(1000)), None);
        assert_eq!(meter.eta(400, None), None);
        // A job that stands still keeps its speed and counts no time.
        let worked = meter.worked;
        meter.tick(5000, false);
        assert_eq!((meter.worked, meter.speed()), (worked, Some(100)));
        assert_eq!(meter.bytes, 5000);
    }
}

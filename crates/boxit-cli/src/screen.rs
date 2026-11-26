//! What a running command shows in the terminal: a line of progress, and
//! the questions an operation has for the person in front of it.
//!
//! Both go to the standard error stream, so the output of a command stays
//! clean for whatever reads it. The line is drawn by a thread of its own a
//! few times a second: the threads that do the work only add to counters.

use std::io::{self, Write};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Condvar, Mutex, MutexGuard, PoisonError};
use std::thread::{self, JoinHandle};
use std::time::Duration;

use boxit_core::{
    Conflict, ConflictAnswer, EntryMeta, Interaction, Password, PasswordRequest, Progress,
};
use boxit_engine::text;

use crate::show;
use crate::Host;

/// How often the line of progress is drawn.
const TICK: Duration = Duration::from_millis(100);

/// The width the line is kept within: a terminal narrower than this is
/// rare, and a line that wraps cannot be drawn over.
const WIDTH: usize = 78;

/// The total of an operation that does not know it.
const UNKNOWN: u64 = u64::MAX;

/// How many times a question is repeated to someone who answers something
/// else.
const ATTEMPTS: usize = 3;

fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(PoisonError::into_inner)
}

pub(crate) struct Screen {
    language: String,
    prompts: bool,
    /// The line of progress is drawn.
    live: bool,
    total: AtomicU64,
    done: AtomicU64,
    /// The entry at work.
    entry: Mutex<String>,
    /// How many characters of the line are on the screen. Held by whoever
    /// writes to the terminal, so a question is not drawn over.
    drawn: Mutex<usize>,
    stopped: Mutex<bool>,
    wake: Condvar,
}

/// Draws the line of progress until it is dropped, then wipes it.
pub(crate) struct Watch {
    screen: Arc<Screen>,
    thread: Option<JoinHandle<()>>,
}

impl Drop for Watch {
    fn drop(&mut self) {
        *lock(&self.screen.stopped) = true;
        self.screen.wake.notify_all();
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
        wipe(&mut lock(&self.screen.drawn));
    }
}

/// Removes the line of progress from the screen.
fn wipe(drawn: &mut usize) {
    if *drawn > 0 {
        let _ = write!(io::stderr(), "\r{:width$}\r", "", width = *drawn);
        let _ = io::stderr().flush();
        *drawn = 0;
    }
}

/// The end of a text that fits into `room` characters.
fn tail(text: &str, room: usize) -> String {
    let count = text.chars().count();
    if count <= room {
        return text.to_owned();
    }
    if room == 0 {
        return String::new();
    }
    let kept: String = text.chars().skip(count - (room - 1)).collect();
    format!("…{kept}")
}

impl Screen {
    /// `live` draws the progress; it takes a terminal to draw on.
    pub(crate) fn new(host: &Host, live: bool) -> Arc<Self> {
        Arc::new(Self {
            language: host.language.clone(),
            prompts: host.prompts,
            live,
            total: AtomicU64::new(UNKNOWN),
            done: AtomicU64::new(0),
            entry: Mutex::new(String::new()),
            drawn: Mutex::new(0),
            stopped: Mutex::new(false),
            wake: Condvar::new(),
        })
    }

    fn text(&self, key: &str, params: &[(&str, &str)]) -> String {
        text::text(&self.language, key, params)
    }

    /// The line of progress as it stands.
    fn line(&self) -> String {
        let done = self.done.load(Ordering::Relaxed);
        let total = self.total.load(Ordering::Relaxed);
        let mut line = if total == UNKNOWN || total == 0 {
            show::size(&self.language, done)
        } else {
            let percent = (u128::from(done) * 100 / u128::from(total)).min(100);
            format!(
                "{percent:>3}%  {} / {}",
                show::size(&self.language, done),
                show::size(&self.language, total)
            )
        };
        let entry = show::printable(&lock(&self.entry)).into_owned();
        let room = WIDTH.saturating_sub(line.chars().count() + 2);
        if !entry.is_empty() && room > 1 {
            line.push_str("  ");
            line.push_str(&tail(&entry, room));
        }
        line
    }

    fn draw(&self) {
        let line = self.line();
        let width = line.chars().count();
        let mut drawn = lock(&self.drawn);
        let pad = drawn.saturating_sub(width);
        let _ = write!(io::stderr(), "\r{line}{:pad$}", "");
        let _ = io::stderr().flush();
        *drawn = width;
    }

    /// Starts drawing the progress of an operation.
    pub(crate) fn watch(self: &Arc<Self>) -> Watch {
        *lock(&self.stopped) = false;
        self.total.store(UNKNOWN, Ordering::Relaxed);
        self.done.store(0, Ordering::Relaxed);
        let thread = self.live.then(|| {
            let screen = Arc::clone(self);
            thread::spawn(move || loop {
                let stopped = *screen
                    .wake
                    .wait_timeout_while(lock(&screen.stopped), TICK, |stopped| !*stopped)
                    .unwrap_or_else(PoisonError::into_inner)
                    .0;
                if stopped {
                    break;
                }
                screen.draw();
            })
        });
        Watch {
            screen: Arc::clone(self),
            thread,
        }
    }

    /// Asks a question and reads a line of the answer. `None` when there is
    /// nobody to answer.
    fn ask(&self, question: &str) -> Option<String> {
        if !self.prompts {
            return None;
        }
        let mut drawn = lock(&self.drawn);
        wipe(&mut drawn);
        let _ = write!(io::stderr(), "{question}");
        let _ = io::stderr().flush();
        let mut answer = String::new();
        match io::stdin().read_line(&mut answer) {
            Ok(count) if count > 0 => Some(answer.trim().to_owned()),
            _ => None,
        }
    }

    /// Asks for a password, which is not shown as it is typed. `None` when
    /// there is nobody to answer, or nothing was typed.
    pub(crate) fn ask_password(&self, question: &str) -> Option<Password> {
        if !self.prompts {
            return None;
        }
        let mut drawn = lock(&self.drawn);
        wipe(&mut drawn);
        let typed = rpassword::prompt_password(question).ok()?;
        (!typed.is_empty()).then(|| Password::new(typed))
    }
}

impl Progress for Screen {
    fn total(&self, bytes: Option<u64>, _entries: Option<u64>) {
        self.total
            .store(bytes.unwrap_or(UNKNOWN), Ordering::Relaxed);
    }

    fn entry(&self, path: &str) {
        if !self.live {
            return;
        }
        // The name is for the eye: a thread that finds the lock taken does
        // not wait for it.
        if let Ok(mut entry) = self.entry.try_lock() {
            entry.clear();
            entry.push_str(path);
        }
    }

    fn advance(&self, bytes: u64) {
        self.done.fetch_add(bytes, Ordering::Relaxed);
    }
}

impl Interaction for Screen {
    fn password(&self, request: &PasswordRequest) -> Option<Password> {
        let key = if request.retry {
            "cli.passwordRetry"
        } else {
            "cli.password"
        };
        let name = request.entry.as_deref().unwrap_or(&request.archive);
        self.ask_password(&self.text(key, &[("name", &show::printable(name))]))
    }

    fn conflict(&self, conflict: &Conflict) -> ConflictAnswer {
        let question = self.text(
            "cli.conflict",
            &[("path", &show::printable(&conflict.path))],
        );
        for _ in 0..ATTEMPTS {
            let Some(answer) = self.ask(&question) else {
                break;
            };
            match answer.as_str() {
                "s" => return ConflictAnswer::Skip,
                "o" => return ConflictAnswer::Overwrite,
                "r" => return ConflictAnswer::Rename,
                "S" => return ConflictAnswer::SkipAll,
                "O" => return ConflictAnswer::OverwriteAll,
                "R" => return ConflictAnswer::RenameAll,
                "c" | "C" => return ConflictAnswer::Cancel,
                _ => {}
            }
        }
        // No answer keeps what is on the disk.
        ConflictAnswer::Skip
    }

    fn symlink(&self, link: &EntryMeta) -> bool {
        let question = self.text(
            "cli.symlink",
            &[
                ("name", &show::printable(&link.path)),
                (
                    "target",
                    &show::printable(link.link_target.as_deref().unwrap_or_default()),
                ),
            ],
        );
        self.ask(&question)
            .is_some_and(|answer| answer.eq_ignore_ascii_case("y"))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn screen(prompts: bool) -> Arc<Screen> {
        let host = Host {
            language: String::from("en"),
            prompts,
        };
        Screen::new(&host, false)
    }

    #[test]
    fn a_long_name_keeps_its_end() {
        assert_eq!(tail("short.txt", 20), "short.txt");
        assert_eq!(tail("folder/with/a/long/name.txt", 9), "…name.txt");
        assert_eq!(tail("папка/файл.txt", 9), "…файл.txt");
        assert_eq!(tail("anything", 0), "");
    }

    #[test]
    fn the_line_shows_the_share_once_the_total_is_known() {
        let screen = screen(false);
        screen.advance(512 * 1024);
        assert_eq!(screen.line(), "512.0 KB");
        screen.total(Some(2 * 1024 * 1024), None);
        *lock(&screen.entry) = String::from("docs/readme.txt");
        assert_eq!(screen.line(), " 25%  512.0 KB / 2.0 MB  docs/readme.txt");
        // More than was promised is still a hundred percent.
        screen.advance(8 * 1024 * 1024);
        assert!(screen.line().starts_with("100%"), "{}", screen.line());
    }

    #[test]
    fn the_line_fits_the_terminal_and_hides_what_a_name_smuggles_in() {
        let screen = screen(false);
        screen.total(Some(1000), None);
        *lock(&screen.entry) = format!("{}\u{1b}[2Jname.txt", "folder/".repeat(40));
        let line = screen.line();
        assert!(line.chars().count() <= WIDTH, "{line}");
        assert!(line.ends_with("name.txt"), "{line}");
        assert!(!line.contains('\u{1b}'), "{line}");
    }

    #[test]
    fn nobody_to_ask_means_the_careful_answer() {
        let screen = screen(false);
        let request = PasswordRequest {
            archive: String::from("a.zip"),
            entry: None,
            retry: false,
        };
        assert_eq!(screen.password(&request), None);
        let conflict = Conflict {
            path: String::from("a.txt"),
            existing_size: 1,
            existing_modified: None,
            entry: EntryMeta::file("a.txt", 1),
        };
        assert_eq!(screen.conflict(&conflict), ConflictAnswer::Skip);
        assert!(!screen.symlink(&EntryMeta::symlink("a", "b")));
    }

    #[test]
    fn a_watch_without_a_terminal_draws_nothing() {
        let screen = screen(false);
        let watch = screen.watch();
        screen.advance(10);
        drop(watch);
        assert_eq!(*lock(&screen.drawn), 0);
    }
}

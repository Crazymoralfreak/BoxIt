//! What the formats share: errors, passwords and times.

use std::fmt::Display;
use std::io::{self, Read, Seek, SeekFrom, Write};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Mutex, PoisonError};

use boxit_core::{Entry, Error, OpContext, Output, Password, PasswordRequest, Result, Timestamp};
use chrono::{DateTime, Datelike, Local, LocalResult, NaiveDate, TimeZone, Timelike};

/// Seconds between 1601-01-01, where the time of Windows starts, and 1970.
const WINDOWS_EPOCH: i64 = 11_644_473_600;
/// Ticks of the time of Windows in a second.
const TICKS: i64 = 10_000_000;

/// The longest target of a symbolic link an archive may hold.
pub(crate) const MAX_LINK_TARGET: u64 = 64 * 1024;

/// The archive holds data that is not what the format says it should be.
pub(crate) fn damaged(detail: impl Display) -> Error {
    Error::new("damaged").detail(detail)
}

/// An error of a decoder or of a parser. What is not a failure of the
/// system, and not an error of ours on its way through `Read`, is data that
/// makes no sense.
pub(crate) fn decode_error(error: io::Error) -> Error {
    let ours = error
        .get_ref()
        .and_then(|inner| inner.downcast_ref::<Error>())
        .is_some();
    let system = error.raw_os_error().is_some();
    let error = Error::from(error);
    if error.code == "io" && !ours && !system {
        let detail = error.params.get("detail").cloned().unwrap_or_default();
        return damaged(detail);
    }
    error
}

/// The data of an entry failed its check. For an encrypted entry the cause
/// is more often the password than the archive.
pub(crate) fn failed(entry: &Entry, error: Error) -> Error {
    failed_named(&entry.meta.path, entry.encrypted, error)
}

/// See [`failed`]; for an entry known by its name only.
pub(crate) fn failed_named(name: &str, encrypted: bool, error: Error) -> Error {
    if matches!(error.code.as_str(), "damaged" | "truncated") {
        let code = if encrypted {
            "damagedOrWrongPassword"
        } else {
            "damagedEntry"
        };
        let mut failed = Error::new(code).with("name", name);
        if let Some(detail) = error.params.get("detail") {
            failed = failed.detail(detail);
        }
        return failed;
    }
    error
}

pub(crate) fn unsupported_method(method: impl Display) -> Error {
    Error::new("unsupportedMethod").with("method", method)
}

/// The data of an entry on its way to a sink. A sink reports a failure to
/// read as it would report a full disk; this remembers that the data was the
/// cause, so the entry can be named as damaged.
pub(crate) struct Watched<R> {
    inner: R,
    failure: Option<Error>,
}

impl<R: Read> Watched<R> {
    pub(crate) fn new(inner: R) -> Self {
        Self {
            inner,
            failure: None,
        }
    }

    /// The failure of the data, if reading it failed.
    pub(crate) fn failure(&mut self) -> Option<Error> {
        self.failure.take()
    }

    /// The error to report for an operation that read through this and
    /// ended with `error`.
    pub(crate) fn settle(&mut self, entry: &Entry, error: Error) -> Error {
        match self.failure.take() {
            Some(failure) => failed(entry, failure),
            None => error,
        }
    }
}

impl<R: Read> Read for Watched<R> {
    fn read(&mut self, buffer: &mut [u8]) -> io::Result<usize> {
        match self.inner.read(buffer) {
            Err(error) if error.kind() != io::ErrorKind::Interrupted => {
                // The error itself goes on to the caller: a copy is kept.
                let ours = error
                    .get_ref()
                    .and_then(|inner| inner.downcast_ref::<Error>());
                self.failure = Some(match (ours, error.raw_os_error()) {
                    (Some(ours), _) => ours.clone(),
                    (None, Some(code)) => io::Error::from_raw_os_error(code).into(),
                    (None, None) => decode_error(io::Error::new(error.kind(), error.to_string())),
                });
                Err(error)
            }
            other => other,
        }
    }
}

/// Size of the block the data of an entry is copied in.
const COPY_BLOCK: usize = 256 * 1024;

/// Copies the content of a file into an archive.
pub(crate) fn copy(data: &mut dyn Read, output: &mut dyn Write, ctx: &OpContext) -> Result<()> {
    let mut block = vec![0u8; COPY_BLOCK];
    loop {
        ctx.checkpoint()?;
        let count = match data.read(&mut block) {
            Ok(0) => return Ok(()),
            Ok(count) => count,
            Err(error) if error.kind() == io::ErrorKind::Interrupted => continue,
            Err(error) => return Err(error.into()),
        };
        output.write_all(&block[..count])?;
    }
}

/// The file an archive is written anew into, with a look at the
/// cancellation on every write: a library that copies an entry of many
/// gigabytes in one call is stopped half way through it.
pub(crate) struct Stoppable<'a> {
    output: &'a mut dyn Output,
    ctx: &'a OpContext,
    /// Set when the work has failed: what is written after that is a
    /// library closing what it was writing as it is dropped, and it is
    /// let to.
    failed: Option<&'a AtomicBool>,
}

impl<'a> Stoppable<'a> {
    pub(crate) fn new(output: &'a mut dyn Output, ctx: &'a OpContext) -> Self {
        Self {
            output,
            ctx,
            failed: None,
        }
    }

    /// Looks at the cancellation until `failed` is set.
    pub(crate) fn until(self, failed: &'a AtomicBool) -> Self {
        Self {
            failed: Some(failed),
            ..self
        }
    }
}

impl Write for Stoppable<'_> {
    fn write(&mut self, buffer: &[u8]) -> io::Result<usize> {
        if !self
            .failed
            .is_some_and(|failed| failed.load(Ordering::Relaxed))
        {
            self.ctx.checkpoint().map_err(Error::into_io)?;
        }
        self.output.write(buffer)
    }

    fn flush(&mut self) -> io::Result<()> {
        self.output.flush()
    }
}

impl Seek for Stoppable<'_> {
    fn seek(&mut self, position: SeekFrom) -> io::Result<u64> {
        self.output.seek(position)
    }
}

struct Known {
    password: Option<Password>,
    /// Counts the passwords tried, so a thread can tell the one it found
    /// wrong from the one another thread got meanwhile.
    generation: u64,
    /// The generation of the password that opened something whose check
    /// passed: it is the right one, and what fails under it is damaged.
    proven: Option<u64>,
}

/// The password of an archive, shared by the threads that read it. The user
/// is asked once, however many threads need the answer.
pub(crate) struct Passwords {
    archive: String,
    known: Mutex<Known>,
}

impl Passwords {
    pub(crate) fn new(archive: String, password: Option<Password>) -> Self {
        Self {
            archive,
            known: Mutex::new(Known {
                password,
                generation: 0,
                proven: None,
            }),
        }
    }

    /// The password of this generation opened data that passed its check.
    pub(crate) fn prove(&self, generation: u64) {
        let mut known = self.known.lock().unwrap_or_else(PoisonError::into_inner);
        if known.generation == generation {
            known.proven = Some(generation);
        }
    }

    /// Whether the password of this generation is known to be the right one.
    pub(crate) fn proven(&self, generation: u64) -> bool {
        let known = self.known.lock().unwrap_or_else(PoisonError::into_inner);
        known.proven == Some(generation)
    }

    /// The password known so far and its generation, without asking.
    pub(crate) fn known(&self) -> Option<(Password, u64)> {
        let known = self.known.lock().unwrap_or_else(PoisonError::into_inner);
        known
            .password
            .clone()
            .map(|password| (password, known.generation))
    }

    /// The password to try, and its generation. `rejected` is the generation
    /// of a password that proved wrong: the user is asked for another one.
    pub(crate) fn get(
        &self,
        ctx: &OpContext,
        entry: Option<&str>,
        rejected: Option<u64>,
    ) -> Result<(Password, u64)> {
        // Holding the lock while asking is the point: the other threads wait
        // for the answer instead of asking the same question.
        let mut known = self.known.lock().unwrap_or_else(PoisonError::into_inner);
        if let Some(password) = &known.password {
            if rejected != Some(known.generation) {
                return Ok((password.clone(), known.generation));
            }
        }
        let request = PasswordRequest {
            archive: self.archive.clone(),
            entry: entry.map(str::to_owned),
            retry: rejected.is_some(),
        };
        match ctx.interaction.password(&request) {
            Some(password) => {
                known.generation += 1;
                known.password = Some(password.clone());
                Ok((password, known.generation))
            }
            None if rejected.is_some() => Err(Error::new("wrongPassword")),
            None => Err(Error::new("passwordRequired")),
        }
    }
}

/// The time of Windows, in ticks of 100 nanoseconds since 1601.
pub(crate) fn from_windows_time(ticks: u64) -> Option<Timestamp> {
    let ticks = i64::try_from(ticks).ok()?;
    if ticks == 0 {
        return None;
    }
    Some(Timestamp {
        seconds: ticks.div_euclid(TICKS) - WINDOWS_EPOCH,
        nanos: (ticks.rem_euclid(TICKS) * 100) as u32,
    })
}

pub(crate) fn to_windows_time(time: Timestamp) -> Option<u64> {
    let seconds = time.seconds.checked_add(WINDOWS_EPOCH)?;
    let ticks = seconds
        .checked_mul(TICKS)?
        .checked_add(i64::from(time.nanos / 100))?;
    u64::try_from(ticks).ok().filter(|ticks| *ticks > 0)
}

/// A date and a time of day as formats without a time zone keep them.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct Civil {
    pub year: i32,
    pub month: u32,
    pub day: u32,
    pub hour: u32,
    pub minute: u32,
    pub second: u32,
}

/// The moment a local date and time of this machine stands for. An hour
/// that the clocks skip or repeat is read as its earlier meaning.
pub(crate) fn from_local(civil: Civil) -> Option<Timestamp> {
    let naive = NaiveDate::from_ymd_opt(civil.year, civil.month, civil.day)?.and_hms_opt(
        civil.hour,
        civil.minute,
        civil.second.min(59),
    )?;
    let moment = match Local.from_local_datetime(&naive) {
        LocalResult::Single(moment) | LocalResult::Ambiguous(moment, _) => moment,
        LocalResult::None => Local.from_utc_datetime(&naive),
    };
    Some(Timestamp::from_seconds(moment.timestamp()))
}

/// The local date and time of this machine at a moment.
pub(crate) fn to_local(time: Timestamp) -> Option<Civil> {
    let moment = DateTime::from_timestamp(time.seconds, time.nanos)?.with_timezone(&Local);
    Some(Civil {
        year: moment.year(),
        month: moment.month(),
        day: moment.day(),
        hour: moment.hour(),
        minute: moment.minute(),
        second: moment.second(),
    })
}

/// Reads the target of a symbolic link stored as the content of an entry.
pub(crate) fn read_link_target(data: &mut dyn Read) -> io::Result<String> {
    let mut target = Vec::new();
    data.take(MAX_LINK_TARGET + 1).read_to_end(&mut target)?;
    if target.len() as u64 > MAX_LINK_TARGET {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "the target of a link is too long",
        ));
    }
    Ok(String::from_utf8_lossy(&target).into_owned())
}

/// The number of threads an operation may use, as a pool for `rayon`.
pub(crate) fn pool(threads: usize) -> Result<rayon::ThreadPool> {
    rayon::ThreadPoolBuilder::new()
        .num_threads(threads.max(1))
        .thread_name(|index| format!("boxit-work-{index}"))
        .build()
        .map_err(|error| Error::new("internal").detail(error))
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::{AtomicU32, Ordering};
    use std::sync::Arc;
    use std::thread;

    use boxit_core::{EntryMeta, Interaction};

    use super::*;

    struct Answers {
        asked: AtomicU32,
        answers: Vec<Option<&'static str>>,
    }

    impl Interaction for Answers {
        fn password(&self, request: &PasswordRequest) -> Option<Password> {
            assert_eq!(request.archive, "a.zip");
            let asked = self.asked.fetch_add(1, Ordering::SeqCst) as usize;
            self.answers
                .get(asked)
                .copied()
                .flatten()
                .map(Password::from)
        }
    }

    fn context(answers: Vec<Option<&'static str>>) -> (OpContext, Arc<Answers>) {
        let answers = Arc::new(Answers {
            asked: AtomicU32::new(0),
            answers,
        });
        let ctx = OpContext {
            interaction: answers.clone(),
            ..OpContext::silent()
        };
        (ctx, answers)
    }

    #[test]
    fn a_known_password_is_not_asked_for() {
        let (ctx, answers) = context(vec![]);
        let passwords = Passwords::new(String::from("a.zip"), Some(Password::from("known")));
        let (password, generation) = passwords.get(&ctx, None, None).unwrap();
        assert_eq!(password.expose(), "known");
        assert_eq!(generation, 0);
        assert_eq!(answers.asked.load(Ordering::SeqCst), 0);
        // It proved wrong and nobody gives another.
        let error = passwords.get(&ctx, None, Some(generation)).unwrap_err();
        assert_eq!(error.code, "wrongPassword");
    }

    #[test]
    fn no_answer_means_the_password_is_required() {
        let (ctx, _) = context(vec![None]);
        let passwords = Passwords::new(String::from("a.zip"), None);
        assert_eq!(
            passwords.get(&ctx, Some("x"), None).unwrap_err().code,
            "passwordRequired"
        );
    }

    #[test]
    fn many_threads_ask_once() {
        let (ctx, answers) = context(vec![Some("secret")]);
        let passwords = Passwords::new(String::from("a.zip"), None);
        thread::scope(|scope| {
            for _ in 0..8 {
                scope.spawn(|| {
                    let (password, _) = passwords.get(&ctx, None, None).unwrap();
                    assert_eq!(password.expose(), "secret");
                });
            }
        });
        assert_eq!(answers.asked.load(Ordering::SeqCst), 1);
        let (known, generation) = passwords.known().unwrap();
        assert_eq!(known.expose(), "secret");
        assert_eq!(generation, 1);
    }

    #[test]
    fn a_wrong_password_is_replaced_once_for_all_threads() {
        let (ctx, answers) = context(vec![Some("wrong"), Some("right")]);
        let passwords = Passwords::new(String::from("a.zip"), None);
        let (first, generation) = passwords.get(&ctx, None, None).unwrap();
        assert_eq!(first.expose(), "wrong");
        // Two threads found the same password wrong: one question.
        let (second, next) = passwords.get(&ctx, None, Some(generation)).unwrap();
        let (third, same) = passwords.get(&ctx, None, Some(generation)).unwrap();
        assert_eq!(second.expose(), "right");
        assert_eq!(third.expose(), "right");
        assert_eq!(next, same);
        assert_eq!(answers.asked.load(Ordering::SeqCst), 2);
        // The one that is in use is the one that can be proven.
        passwords.prove(generation);
        assert!(!passwords.proven(generation) && !passwords.proven(next));
        passwords.prove(next);
        assert!(passwords.proven(next) && !passwords.proven(generation));
    }

    #[test]
    fn the_time_of_windows_round_trips() {
        let time = Timestamp {
            seconds: 1_700_000_000,
            nanos: 123_456_700,
        };
        let ticks = to_windows_time(time).unwrap();
        assert_eq!(ticks, 133_444_736_001_234_567);
        assert_eq!(from_windows_time(ticks), Some(time));
        // Before 1970, and the first tick of all.
        let old = Timestamp {
            seconds: -1,
            nanos: 500_000_000,
        };
        assert_eq!(from_windows_time(to_windows_time(old).unwrap()), Some(old));
        assert_eq!(from_windows_time(0), None);
        assert_eq!(from_windows_time(u64::MAX), None);
        assert_eq!(to_windows_time(Timestamp::from_seconds(i64::MIN)), None);
        assert_eq!(
            to_windows_time(Timestamp::from_seconds(-WINDOWS_EPOCH - 5)),
            None
        );
    }

    #[test]
    fn local_time_round_trips() {
        let time = Timestamp::from_seconds(1_700_000_000);
        let civil = to_local(time).unwrap();
        assert_eq!(from_local(civil), Some(time));
        assert_eq!(
            from_local(Civil {
                year: 2024,
                month: 13,
                day: 1,
                hour: 0,
                minute: 0,
                second: 0
            }),
            None
        );
    }

    #[test]
    fn a_failed_entry_names_itself() {
        let mut entry = Entry {
            meta: EntryMeta::file("a/b.txt", 1),
            ..Entry::default()
        };
        let error = failed(&entry, damaged("crc"));
        assert_eq!(error.code, "damagedEntry");
        assert_eq!(error.params["name"], "a/b.txt");
        assert_eq!(error.params["detail"], "crc");
        entry.encrypted = true;
        assert_eq!(
            failed(&entry, damaged("crc")).code,
            "damagedOrWrongPassword"
        );
        // Anything else passes as it is.
        assert_eq!(failed(&entry, Error::cancelled()).code, "cancelled");
    }

    /// Gives a few bytes, then fails the way a decoder does.
    struct Breaks(usize);

    impl Read for Breaks {
        fn read(&mut self, buffer: &mut [u8]) -> io::Result<usize> {
            if self.0 == 0 {
                return Err(io::Error::other("bad checksum"));
            }
            self.0 -= 1;
            buffer[0] = b'x';
            Ok(1)
        }
    }

    #[test]
    fn a_failure_of_the_data_is_told_from_a_failure_of_the_sink() {
        let entry = Entry {
            meta: EntryMeta::file("a.txt", 3),
            ..Entry::default()
        };
        // The sink gave up for a reason of its own: the reason stands.
        let mut fine = Watched::new(&b"abc"[..]);
        let mut read = Vec::new();
        fine.read_to_end(&mut read).unwrap();
        assert_eq!(fine.settle(&entry, Error::new("diskFull")).code, "diskFull");
        // The data broke: whatever the sink made of it, the entry is damaged.
        let mut broken = Watched::new(Breaks(2));
        assert!(broken.read_to_end(&mut read).is_err());
        let error = broken.settle(&entry, Error::new("io"));
        assert_eq!(error.code, "damagedEntry");
        assert_eq!(error.params["name"], "a.txt");
        assert_eq!(error.params["detail"], "bad checksum");
        assert!(broken.failure().is_none());
    }

    #[test]
    fn errors_of_decoders_are_damage_and_errors_of_the_system_are_not() {
        assert_eq!(decode_error(io::Error::other("bad block")).code, "damaged");
        assert_eq!(
            decode_error(io::ErrorKind::UnexpectedEof.into()).code,
            "truncated"
        );
        // Whatever the system calls its fifth error, it is not damage.
        assert_ne!(
            decode_error(io::Error::from_raw_os_error(5)).code,
            "damaged"
        );
        let ours = Error::cancelled().into_io();
        assert!(decode_error(ours).is_cancelled());
    }

    #[test]
    fn a_link_target_has_a_limit() {
        assert_eq!(read_link_target(&mut &b"../x"[..]).unwrap(), "../x");
        let long = vec![b'a'; MAX_LINK_TARGET as usize + 1];
        assert!(read_link_target(&mut long.as_slice()).is_err());
    }
}

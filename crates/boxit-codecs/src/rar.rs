//! RAR, read by the library of its author.
//!
//! The format is closed and so is the way it compresses: the one description
//! of it is UnRAR, the source Alexander Roshal publishes under a licence that
//! lets it read archives in any program and forbids to build an archiver
//! that writes them. So this is the one format BoxIt does not read with
//! code of its own or with a crate in Rust. UnRAR is C++; `unrar_sys` carries
//! its source and builds it, and all of it sits behind the feature `rar`.
//!
//! The library was made to unpack onto the disk by itself, and here it never
//! does. Every file is "tested": decoded and checked against its sum, with
//! the decoded bytes shown to a callback on their way. No name out of an
//! archive reaches the code that would create a file, and where a file goes
//! is decided by the same code as for every other format.
//!
//! A callback is given the bytes, and a sink asks for them. Between the two
//! stands a thread: the library runs on the thread that asked for the
//! extraction and pushes blocks into a short queue, and the sink reads them
//! out of it on a thread of its own. The queue is what holds the library
//! back when the disk is slower than the decoder.
//!
//! What the library does not give is left out: an archive in several
//! volumes is refused until BoxIt follows volumes of every format, and the
//! names of an old archive that are not Unicode are read the way the
//! library reads them, with no say for the encoding the user chose.
//!
//! This is the third place of the crate with `unsafe`: the calls of the
//! library and the callback it calls back.
//!
//! The licence of UnRAR asks that this paragraph of it stays with the code
//! that uses it; the whole licence is in `THIRD-PARTY-NOTICES.txt`:
//!
//! > UnRAR source code may be used in any software to handle RAR archives
//! > without limitations free of charge, but cannot be used to develop RAR
//! > (WinRAR) compatible archiver and to re-create RAR compression
//! > algorithm, which is proprietary. Distribution of modified UnRAR source
//! > code in separate form or as a part of other software is permitted,
//! > provided that full text of this paragraph, starting from "UnRAR source
//! > code" words, is included in license, or in documentation if license is
//! > not available, and in source code comments of resulting package.

use std::borrow::Cow;
#[cfg(not(windows))]
use std::ffi::CString;
use std::ffi::{c_char, c_int, c_uint};
use std::io::{self, Read, Seek, SeekFrom, Write};
use std::mem;
#[cfg(not(windows))]
use std::os::unix::ffi::OsStrExt;
#[cfg(windows)]
use std::os::windows::ffi::OsStrExt;
use std::panic::{catch_unwind, resume_unwind, AssertUnwindSafe};
use std::path::{Path, PathBuf};
use std::ptr;
use std::slice;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::mpsc::{sync_channel, Receiver, SyncSender};
use std::thread;

use boxit_core::{
    ArchiveReader, Capabilities, Entry, EntryKind, EntryMeta, EntrySink, Error, OpContext, Result,
    Selection,
};
use tempfile::TempPath;
use unrar_sys::{
    Callback, Handle, RARCloseArchive, RAROpenArchiveEx, RARProcessFileW, RARReadHeaderEx,
    ERAR_BAD_ARCHIVE, ERAR_BAD_DATA, ERAR_BAD_PASSWORD, ERAR_END_ARCHIVE, ERAR_EOPEN, ERAR_EREAD,
    ERAR_EREFERENCE, ERAR_MISSING_PASSWORD, ERAR_NO_MEMORY, ERAR_SUCCESS, ERAR_UNKNOWN_FORMAT,
    LPARAM, RAR_HASH_CRC32, RAR_OM_EXTRACT, RAR_OM_LIST, RAR_SKIP, RAR_TEST, RAR_VOL_NOTIFY,
    RHDF_DIRECTORY, RHDF_ENCRYPTED, RHDF_SPLITAFTER, RHDF_SPLITBEFORE, ROADF_ENCHEADERS,
    ROADF_FIRSTVOLUME, ROADF_SOLID, ROADF_VOLUME, UCM_CHANGEVOLUME, UCM_CHANGEVOLUMEW,
    UCM_NEEDPASSWORD, UCM_NEEDPASSWORDW, UCM_PROCESSDATA, UINT, WCHAR,
};

use crate::source::Source;
use crate::support::{
    damaged, failed, from_windows_time, unsupported_method, Passwords, Watched, MAX_LINK_TARGET,
};
use crate::OpenOptions;

// The library reads the registry and asks Windows for random numbers and
// for privileges; the script that builds it leaves out the system library
// those come from.
#[cfg(windows)]
#[link(name = "advapi32")]
extern "C" {}

// The callback is declared with the calling convention of C, which is the
// one the library uses everywhere but on 32-bit Windows.
#[cfg(all(windows, target_pointer_width = "32"))]
compile_error!("the feature `rar` does not support 32-bit Windows");

/// Blocks of decoded bytes that may wait for the sink. A block is as large
/// as the library makes it, up to four megabytes.
const QUEUE: usize = 2;
/// Characters of the longest name the library reads.
const NAME: usize = 0x10000;
/// Characters of the longest comment of an archive.
const COMMENT: usize = 0x40000;

/// The dictionary of a file is larger than the library takes unasked.
const ERAR_LARGE_DICT: c_int = 25;
/// The library asks whether it may take a dictionary that large.
const UCM_LARGEDICT: UINT = 5;

/// What the callback answers: go on, stop, and "not for me".
const GO: c_int = 1;
const STOP: c_int = -1;
const UNHEARD: c_int = 0;

/// The system of an entry, as the library reports it: Windows, or Unix for
/// everything else.
const HOST_WINDOWS: c_uint = 2;
/// The method of a file that is stored as it is.
const METHOD_STORE: c_uint = 0x30;
/// The first version of the compression of RAR 5; the archives before it
/// stop at 36.
const VERSION_5: c_uint = 50;
/// The size of a file whose size the archive does not know.
const UNKNOWN_SIZE: u64 = 0x7fff_ffff_7fff_ffff;

/// What an entry stands for when it is not a file of its own.
const REDIRECT_NONE: c_uint = 0;
const REDIRECT_UNIX_LINK: c_uint = 1;
const REDIRECT_WINDOWS_LINK: c_uint = 2;
const REDIRECT_JUNCTION: c_uint = 3;
const REDIRECT_HARD_LINK: c_uint = 4;
const REDIRECT_COPY: c_uint = 5;

/// What Windows puts before a path it means literally.
const NT_PREFIXES: [&str; 2] = ["\\??\\", "/??/"];

/// `RARHeaderDataEx` of `dll.hpp`. The header packs its structures to one
/// byte, and `unrar_sys` describes them without the packing: everything
/// after the first pointer would be read four bytes off. So the structures
/// are described here, field by field as the header has them.
#[repr(C, packed)]
struct Header {
    archive_name: [c_char; 1024],
    archive_name_w: [WCHAR; 1024],
    file_name: [c_char; 1024],
    file_name_w: [WCHAR; 1024],
    flags: c_uint,
    pack_size: c_uint,
    pack_size_high: c_uint,
    unp_size: c_uint,
    unp_size_high: c_uint,
    host_os: c_uint,
    file_crc: c_uint,
    file_time: c_uint,
    unp_ver: c_uint,
    method: c_uint,
    file_attr: c_uint,
    comment_buffer: *mut c_char,
    comment_buffer_size: c_uint,
    comment_size: c_uint,
    comment_state: c_uint,
    dict_size: c_uint,
    hash_type: c_uint,
    hash: [c_char; 32],
    redir_type: c_uint,
    redir_name: *mut WCHAR,
    redir_name_size: c_uint,
    dir_target: c_uint,
    mtime_low: c_uint,
    mtime_high: c_uint,
    ctime_low: c_uint,
    ctime_high: c_uint,
    atime_low: c_uint,
    atime_high: c_uint,
    archive_name_ex: *mut WCHAR,
    archive_name_ex_size: c_uint,
    file_name_ex: *mut WCHAR,
    file_name_ex_size: c_uint,
    reserved: [c_uint; 982],
}

/// `RAROpenArchiveDataEx` of `dll.hpp`; see [`Header`].
#[repr(C, packed)]
struct Opening {
    archive_name: *const c_char,
    archive_name_w: *const WCHAR,
    open_mode: c_uint,
    open_result: c_uint,
    comment_buffer: *mut c_char,
    comment_buffer_size: c_uint,
    comment_size: c_uint,
    comment_state: c_uint,
    flags: c_uint,
    callback: Option<Callback>,
    user_data: LPARAM,
    op_flags: c_uint,
    comment_buffer_w: *mut WCHAR,
    reserved: [c_uint; 25],
}

/// The characters of the library for a text.
#[cfg(windows)]
fn units(text: &str) -> impl Iterator<Item = WCHAR> + '_ {
    text.encode_utf16()
}

#[cfg(not(windows))]
fn units(text: &str) -> impl Iterator<Item = WCHAR> + '_ {
    text.chars().map(|one| one as WCHAR)
}

/// The text the library wrote, up to the zero that ends it.
#[cfg(windows)]
fn text(wide: &[WCHAR]) -> String {
    let end = wide
        .iter()
        .position(|unit| *unit == 0)
        .unwrap_or(wide.len());
    String::from_utf16_lossy(&wide[..end])
}

#[cfg(not(windows))]
fn text(wide: &[WCHAR]) -> String {
    wide.iter()
        .take_while(|unit| **unit != 0)
        .map(|unit| char::from_u32(*unit as u32).unwrap_or(char::REPLACEMENT_CHARACTER))
        .collect()
}

/// Writes a text for the library, with the zero that ends it. `false` when
/// the text does not fit.
fn put(text: &str, buffer: &mut [WCHAR]) -> bool {
    let mut slots = buffer.iter_mut();
    for unit in units(text).chain([0]) {
        let Some(slot) = slots.next() else {
            return false;
        };
        *slot = unit;
    }
    true
}

/// The name of the archive file, the way the library takes it: in wide
/// characters on Windows, and as the bytes of the system elsewhere, where
/// the library has its own way through names that are not valid text.
struct Named {
    #[cfg(windows)]
    wide: Vec<WCHAR>,
    #[cfg(not(windows))]
    narrow: CString,
    /// The file is a copy of the archive, made for the library: if the
    /// archive goes on in other volumes, they are not next to it.
    alone: bool,
}

impl Named {
    #[cfg(windows)]
    fn new(path: &Path, alone: bool) -> Result<Self> {
        let mut wide: Vec<WCHAR> = path.as_os_str().encode_wide().collect();
        // The library would take the name to end at the first zero.
        if wide.contains(&0) {
            return Err(Error::new("notFound").detail(path.display()));
        }
        wide.push(0);
        Ok(Self { wide, alone })
    }

    #[cfg(not(windows))]
    fn new(path: &Path, alone: bool) -> Result<Self> {
        CString::new(path.as_os_str().as_bytes())
            .map(|narrow| Self { narrow, alone })
            .map_err(|_| Error::new("notFound").detail(path.display()))
    }

    #[cfg(windows)]
    fn pointers(&self) -> (*const c_char, *const WCHAR) {
        (ptr::null(), self.wide.as_ptr())
    }

    #[cfg(not(windows))]
    fn pointers(&self) -> (*const c_char, *const WCHAR) {
        (self.narrow.as_ptr(), ptr::null())
    }
}

/// What goes from the library to the sink.
enum Piece {
    /// An entry begins: what follows is its content.
    Start(usize),
    Data(Vec<u8>),
    /// The content of the entry ended, well or not.
    End(Result<()>),
}

/// The entry the library is decoding.
struct Current {
    index: usize,
    name: String,
    /// The sink is to get it; otherwise it is decoded for the sake of the
    /// entries behind it.
    wanted: bool,
    /// Nothing behind it is wanted.
    last: bool,
    /// The sink has been told that it began.
    started: bool,
}

/// What the callback knows of the operation it serves.
struct State<'a> {
    ctx: &'a OpContext,
    passwords: &'a Passwords,
    /// Where the decoded bytes go; nowhere while an archive is listed.
    pieces: Option<&'a SyncSender<Piece>>,
    /// The entry the sink has read all it wanted of, counted from one.
    given_up: Option<&'a AtomicUsize>,
    current: Option<Current>,
    /// The generation of the password the library was given last.
    tried: Option<u64>,
    /// The generation of a password that proved wrong.
    rejected: Option<u64>,
    /// A password was given and nothing has proved it right yet.
    asked: bool,
    /// The library was stopped because nothing more is wanted of it.
    stopped: bool,
    /// Why the callback stopped the library, when it did for a failure.
    failure: Option<Error>,
}

impl State<'_> {
    fn gone() -> Error {
        Error::new("internal").detail("the receiver of the decoded data is gone")
    }

    fn send(&mut self, piece: Piece) -> c_int {
        if let Some(Ok(())) = self.pieces.map(|pieces| pieces.send(piece)) {
            GO
        } else {
            self.failure = Some(Self::gone());
            STOP
        }
    }

    fn data(&mut self, bytes: &[u8]) -> c_int {
        if let Err(error) = self.ctx.checkpoint() {
            self.failure = Some(error);
            return STOP;
        }
        let Some(current) = &mut self.current else {
            return GO;
        };
        if !current.wanted {
            return GO;
        }
        let index = current.index;
        let read_enough = self
            .given_up
            .is_some_and(|given_up| given_up.load(Ordering::Acquire) == index + 1);
        if read_enough {
            if current.last {
                self.stopped = true;
                return STOP;
            }
            return GO;
        }
        if !current.started {
            current.started = true;
            if self.send(Piece::Start(index)) == STOP {
                return STOP;
            }
        }
        self.send(Piece::Data(bytes.to_vec()))
    }

    fn password(&mut self, buffer: &mut [WCHAR]) -> c_int {
        self.asked = true;
        let entry = self.current.as_ref().map(|current| current.name.as_str());
        loop {
            match self.passwords.get(self.ctx, entry, self.rejected) {
                Ok((password, generation)) => {
                    self.tried = Some(generation);
                    // An empty password is none, and one that does not fit
                    // is longer than any password of a RAR archive.
                    if !password.is_empty() && put(password.expose(), buffer) {
                        return GO;
                    }
                    self.rejected = Some(generation);
                }
                Err(error) => {
                    self.failure = Some(error);
                    return STOP;
                }
            }
        }
    }

    /// The library goes on to the next volume of the archive: it has found
    /// the file, or it has not and asks where it is.
    fn volume(&mut self, name: &[WCHAR], found: bool) -> c_int {
        if found {
            return match self.ctx.checkpoint() {
                Ok(()) => GO,
                Err(error) => {
                    self.failure = Some(error);
                    STOP
                }
            };
        }
        let path = text(name);
        let name = path.rsplit(['/', '\\']).next().unwrap_or(&path);
        self.failure = Some(Error::new("volumeMissing").with("name", name));
        STOP
    }

    /// Whether a dictionary of so many kilobytes may be taken.
    fn dictionary(&self, kilobytes: u64) -> c_int {
        if kilobytes.saturating_mul(1024) <= self.ctx.budget.memory {
            GO
        } else {
            STOP
        }
    }
}

/// What the library calls while it works: with decoded bytes, for a
/// password, for a volume.
extern "C" fn callback(message: UINT, user: LPARAM, first: LPARAM, second: LPARAM) -> c_int {
    // SAFETY: `user` is the pointer `Library::new` made and gives the
    // library with every archive it opens, and `Library` frees it only
    // after closing the archive. The library calls back from inside a call
    // of `Library`, on the same thread, and `Library` holds no reference to
    // the state across its calls.
    let state = unsafe { &mut *(user as *mut State<'_>) };
    let answer = catch_unwind(AssertUnwindSafe(|| match message {
        UCM_PROCESSDATA => {
            let bytes: &[u8] = if first == 0 || second <= 0 {
                &[]
            } else {
                // SAFETY: the library gives the address and the size of the
                // bytes it decoded; they stay where they are until this
                // function returns.
                unsafe { slice::from_raw_parts(first as *const u8, second as usize) }
            };
            state.data(bytes)
        }
        UCM_NEEDPASSWORDW => {
            if first == 0 || second <= 0 {
                return STOP;
            }
            // SAFETY: the library gives the address of a buffer of its own
            // and the number of characters it holds, to be written to.
            let buffer = unsafe { slice::from_raw_parts_mut(first as *mut WCHAR, second as usize) };
            state.password(buffer)
        }
        UCM_LARGEDICT => state.dictionary(first as u64),
        UCM_CHANGEVOLUMEW => {
            if first == 0 {
                return STOP;
            }
            let start = first as *const WCHAR;
            // SAFETY: the library gives the address of the name of the
            // volume, a text that ends with a zero in a buffer of `NAME`
            // characters: no character past the zero is read.
            let name = unsafe {
                let length = (0..NAME).find(|at| *start.add(*at) == 0).unwrap_or(NAME);
                slice::from_raw_parts(start, length)
            };
            state.volume(name, second == RAR_VOL_NOTIFY)
        }
        // The same in the characters of the system, after the one above
        // let the library go on: there is nothing to add.
        UCM_CHANGEVOLUME if second == RAR_VOL_NOTIFY => GO,
        // A password in the characters of the system is asked for when the
        // one above came out empty: it is not given.
        UCM_NEEDPASSWORD | UCM_CHANGEVOLUME => STOP,
        _ => UNHEARD,
    }));
    // A panic must not cross into the library: it stops the library, and
    // what it was about is reported as the failure of the operation.
    answer.unwrap_or_else(|_| {
        state.failure = Some(Error::new("internal").detail("a callback of UnRAR panicked"));
        STOP
    })
}

/// Why a call of the library did not do what it was asked.
enum Fault {
    /// The password is not the one.
    Password,
    Failed(Error),
}

impl Fault {
    fn error(self) -> Error {
        match self {
            Self::Password => Error::new("wrongPassword"),
            Self::Failed(error) => error,
        }
    }
}

/// What a code of the library means.
fn fault(code: c_int) -> Error {
    match code {
        ERAR_NO_MEMORY | ERAR_LARGE_DICT => Error::new("notEnoughMemory"),
        ERAR_BAD_DATA => damaged("the data does not pass its check"),
        ERAR_BAD_ARCHIVE => damaged("this is not a RAR archive"),
        ERAR_EREFERENCE => damaged("the file this one is a copy of is missing"),
        ERAR_UNKNOWN_FORMAT => Error::new("unsupportedArchive"),
        ERAR_MISSING_PASSWORD => Error::new("passwordRequired"),
        ERAR_BAD_PASSWORD => Error::new("wrongPassword"),
        ERAR_EOPEN => Error::new("io").detail("UnRAR could not open the archive"),
        ERAR_EREAD => Error::new("io").detail("UnRAR could not read the archive"),
        other => Error::new("internal").detail(format!("UnRAR failed with the code {other}")),
    }
}

/// The name of the compression of a file.
fn method(version: c_uint, method: c_uint) -> Cow<'static, str> {
    if method == METHOD_STORE {
        return Cow::Borrowed("Store");
    }
    match version {
        0..=15 => Cow::Borrowed("RAR 1.5"),
        16..=26 => Cow::Borrowed("RAR 2"),
        27..=36 => Cow::Borrowed("RAR 3"),
        37..=50 => Cow::Borrowed("RAR 5"),
        51..=70 => Cow::Borrowed("RAR 7"),
        other => Cow::Owned(format!("RAR {other}")),
    }
}

/// A path of an archive with `/` between its folders. The library turns
/// the paths to the way of the system it runs on.
fn slashed(path: String) -> String {
    if cfg!(windows) && path.contains('\\') {
        path.replace('\\', "/")
    } else {
        path
    }
}

/// What a link points to, as text.
fn link_target(redirect: c_uint, target: String) -> String {
    match redirect {
        REDIRECT_WINDOWS_LINK | REDIRECT_JUNCTION => {
            let bare = NT_PREFIXES
                .iter()
                .find_map(|prefix| target.strip_prefix(prefix))
                .unwrap_or(&target);
            bare.replace('\\', "/")
        }
        _ => slashed(target),
    }
}

/// An archive in the hands of the library, and the state its callback
/// works with.
struct Library<'a> {
    handle: *const Handle,
    /// Shared with the callback, which is why it is kept as a pointer: the
    /// library holds a copy of it from the opening of an archive to its
    /// closing.
    state: *mut State<'a>,
    header: Box<Header>,
    name: Vec<WCHAR>,
    target: Vec<WCHAR>,
    /// The flags of the open archive.
    flags: c_uint,
}

impl<'a> Library<'a> {
    fn new(ctx: &'a OpContext, passwords: &'a Passwords) -> Self {
        let state = Box::new(State {
            ctx,
            passwords,
            pieces: None,
            given_up: None,
            current: None,
            tried: None,
            rejected: None,
            asked: false,
            stopped: false,
            failure: None,
        });
        Self {
            handle: ptr::null(),
            state: Box::into_raw(state),
            // SAFETY: every field of the structure is a number, an array
            // of numbers or a raw pointer, and all zeros is a value of each.
            header: unsafe { Box::<Header>::new_zeroed().assume_init() },
            name: vec![0; NAME],
            target: vec![0; NAME],
            flags: 0,
        }
    }

    fn state(&mut self) -> &mut State<'a> {
        // SAFETY: the pointer comes from `Box::into_raw` in `new` and is
        // freed in `drop`. The callback is the only other user: it runs
        // inside the calls of the library made below, and a reference
        // handed out here never lives across one of them.
        unsafe { &mut *self.state }
    }

    fn close(&mut self) {
        if !self.handle.is_null() {
            // SAFETY: the handle came from `RAROpenArchiveEx` and has not
            // been closed.
            unsafe { RARCloseArchive(self.handle) };
            self.handle = ptr::null();
        }
    }

    /// What a call that returned `code` came to. `headers`: the call read
    /// headers, which a wrong password turns into noise.
    fn settle(&mut self, code: c_int, headers: bool) -> std::result::Result<(), Fault> {
        let state = self.state();
        let unproven = mem::take(&mut state.asked);
        if let Some(failure) = state.failure.take() {
            return Err(Fault::Failed(failure));
        }
        Err(match code {
            ERAR_SUCCESS => return Ok(()),
            ERAR_BAD_PASSWORD => Fault::Password,
            ERAR_BAD_DATA | ERAR_BAD_ARCHIVE if headers && unproven => Fault::Password,
            other => Fault::Failed(fault(other)),
        })
    }

    /// The password the library was given last is not the one.
    fn reject(&mut self) {
        let state = self.state();
        state.rejected = state.tried;
    }

    /// Opens the archive, asking for the password of its headers as often
    /// as it takes. An archive open before is closed. `comment` is where
    /// the comment of the archive is written.
    fn open(
        &mut self,
        named: &Named,
        mode: c_uint,
        mut comment: Option<&mut [WCHAR]>,
    ) -> Result<()> {
        loop {
            self.close();
            let (narrow, wide) = named.pointers();
            let (comment_at, comment_size) = match comment.as_deref_mut() {
                Some(buffer) => (buffer.as_mut_ptr(), buffer.len() as c_uint),
                None => (ptr::null_mut(), 0),
            };
            let mut opening = Opening {
                archive_name: narrow,
                archive_name_w: wide,
                open_mode: mode,
                open_result: 0,
                comment_buffer: ptr::null_mut(),
                comment_buffer_size: comment_size,
                comment_size: 0,
                comment_state: 0,
                flags: 0,
                callback: Some(callback),
                user_data: self.state as LPARAM,
                op_flags: 0,
                comment_buffer_w: comment_at,
                reserved: [0; 25],
            };
            // SAFETY: the structure is laid out as the library has it; the
            // name and the buffer of the comment outlive the call, and the
            // state outlives the handle.
            self.handle = unsafe { RAROpenArchiveEx((&raw mut opening).cast()) };
            let code = opening.open_result as c_int;
            if !self.handle.is_null() && code == ERAR_SUCCESS {
                self.state().asked = false;
                self.flags = opening.flags;
                // The volumes of a copy are not where the library would
                // look for them.
                if named.alone && self.flags & ROADF_VOLUME != 0 {
                    self.close();
                    return Err(Error::new("unsupportedArchive"));
                }
                return Ok(());
            }
            self.close();
            match self.settle(code, true) {
                Err(Fault::Password) => self.reject(),
                Err(Fault::Failed(error)) => return Err(error),
                Ok(()) => return Err(fault(ERAR_BAD_ARCHIVE)),
            }
        }
    }

    /// Reads the header of the next entry; `false` at the end of the
    /// archive.
    fn header(&mut self) -> std::result::Result<bool, Fault> {
        self.name[0] = 0;
        self.target[0] = 0;
        self.header.file_name_ex = self.name.as_mut_ptr();
        self.header.file_name_ex_size = self.name.len() as c_uint;
        self.header.redir_name = self.target.as_mut_ptr();
        self.header.redir_name_size = self.target.len() as c_uint;
        // SAFETY: the handle is open, and the structure and the buffers it
        // points to are laid out and sized as the library is told.
        let code = unsafe { RARReadHeaderEx(self.handle, (&raw mut *self.header).cast()) };
        if code == ERAR_END_ARCHIVE && self.state().failure.is_none() {
            // An end that comes before any header proved the password right
            // is the noise of a wrong one.
            return if mem::take(&mut self.state().asked) {
                Err(Fault::Password)
            } else {
                Ok(false)
            };
        }
        self.settle(code, true).map(|()| true)
    }

    /// Goes over the content of the entry whose header was read: skipping
    /// it, or decoding it for the callback.
    fn process(&mut self, operation: c_int) -> std::result::Result<(), Fault> {
        // SAFETY: the handle is open. No place on the disk is named, and
        // neither operation used here writes anywhere.
        let code = unsafe { RARProcessFileW(self.handle, operation, ptr::null(), ptr::null()) };
        self.settle(code, false)
    }

    /// The archive is a volume that is not the first: it starts in
    /// another file.
    fn later_volume(&self) -> bool {
        self.flags & ROADF_VOLUME != 0 && self.flags & ROADF_FIRSTVOLUME == 0
    }

    /// The header that was read is of a file that began in the volume
    /// before: the rest of what was passed over there.
    fn continued(&self) -> bool {
        self.header.flags & RHDF_SPLITBEFORE != 0
    }

    /// The path of the entry whose header was read.
    fn path(&self) -> String {
        slashed(text(&self.name))
    }

    /// The memory the decoder of the entry takes for its dictionary.
    fn dictionary(&self) -> u64 {
        if self.header.method == METHOD_STORE {
            0
        } else {
            u64::from(self.header.dict_size) * 1024
        }
    }

    /// The entry whose header was read.
    fn describe(&self, index: u64) -> Entry {
        let header = &*self.header;
        let wide = |low: c_uint, high: c_uint| u64::from(low) | (u64::from(high) << 32);
        let flags = header.flags;
        let redirect = header.redir_type;
        let kind = match redirect {
            REDIRECT_UNIX_LINK | REDIRECT_WINDOWS_LINK | REDIRECT_JUNCTION => EntryKind::Symlink,
            REDIRECT_HARD_LINK | REDIRECT_COPY => EntryKind::Hardlink,
            _ if flags & RHDF_DIRECTORY != 0 => EntryKind::Directory,
            _ => EntryKind::File,
        };
        let is_file = kind == EntryKind::File;
        let encrypted = flags & RHDF_ENCRYPTED != 0;
        let target = Some(text(&self.target))
            .filter(|target| redirect != REDIRECT_NONE && !target.is_empty())
            .map(|target| link_target(redirect, target));
        let size = wide(header.unp_size, header.unp_size_high);
        let attributes = header.file_attr;
        let windows = header.host_os == HOST_WINDOWS;
        // The sum of an encrypted file of RAR 5 is mixed with a key, so
        // that it tells nothing of the content: it is not the CRC-32.
        // And the sum of a file that goes on in the next volume is of the
        // part of it that this volume holds.
        let plain_sum = header.hash_type == RAR_HASH_CRC32
            && !(encrypted && header.unp_ver >= VERSION_5)
            && flags & RHDF_SPLITAFTER == 0;
        Entry {
            index,
            meta: EntryMeta {
                path: self.path(),
                kind,
                link_target: target,
                size: match kind {
                    EntryKind::File | EntryKind::Hardlink => (size != UNKNOWN_SIZE).then_some(size),
                    _ => Some(0),
                },
                modified: from_windows_time(wide(header.mtime_low, header.mtime_high)),
                created: from_windows_time(wide(header.ctime_low, header.ctime_high)),
                accessed: from_windows_time(wide(header.atime_low, header.atime_high)),
                unix_mode: (!windows && attributes != 0).then_some(attributes & 0o7777),
                windows_attributes: (windows && attributes != 0).then_some(attributes),
            },
            packed_size: is_file.then_some(wide(header.pack_size, header.pack_size_high)),
            crc32: (is_file && plain_sum).then_some(header.file_crc),
            encrypted,
            method: is_file.then(|| method(header.unp_ver, header.method)),
            comment: None,
        }
    }
}

impl Drop for Library<'_> {
    fn drop(&mut self) {
        self.close();
        // SAFETY: the pointer comes from `Box::into_raw` in `new`, and the
        // library, closed above, has no use for it any more.
        drop(unsafe { Box::from_raw(self.state) });
    }
}

/// The marks and the fields of the headers of the archives older than
/// RAR 5, as far as the targets of their links need them.
const OLD_MARK: [u8; 7] = *b"Rar!\x1A\x07\x00";
const OLD_MAIN: u8 = 0x73;
const OLD_FILE: u8 = 0x74;
const OLD_END: u8 = 0x7B;
/// The shortest header, and the part of the header of a file that is
/// always there.
const OLD_SHORT: u64 = 7;
const OLD_FIXED: usize = 32;
/// Of the main header: the headers behind it are encrypted.
const OLD_SECRET: u16 = 0x0080;
/// Of the header of a file: its content is encrypted; its sizes have a
/// high half; data follows the header.
const OLD_LOCKED: u16 = 0x0004;
const OLD_LARGE: u16 = 0x0100;
const OLD_LONG: u16 = 0x8000;
const OLD_UNIX: u8 = 3;
const OLD_STORE: u8 = 0x30;
/// The bits of a mode of Unix that say a file is a symbolic link.
const KIND_MASK: u32 = 0o170_000;
const KIND_LINK: u32 = 0o120_000;

/// The targets of the symbolic links of an archive older than RAR 5, one
/// place for the header of every file, in the order of the archive.
///
/// Such an archive keeps the target of a link as the content of the entry,
/// and the library reads it only to create the link, which it is never
/// asked to do here. The content is stored as it is, so it is read straight
/// from the file. Empty when the headers cannot be followed: the links are
/// then listed without their targets.
fn old_targets(source: &Source) -> Result<Vec<Option<String>>> {
    let size = source.size();
    let mut reader = source.reader()?;
    let mut mark = [0u8; OLD_MARK.len()];
    if size < OLD_SHORT || reader.read_exact(&mut mark).is_err() || mark != OLD_MARK {
        return Ok(Vec::new());
    }
    let mut targets = Vec::new();
    let mut at = OLD_MARK.len() as u64;
    while at + OLD_SHORT <= size {
        let mut head = [0u8; OLD_FIXED + 4];
        let have = (size - at).min(head.len() as u64) as usize;
        reader.seek(SeekFrom::Start(at))?;
        reader.read_exact(&mut head[..have])?;
        let word =
            |at: usize| u32::from_le_bytes([head[at], head[at + 1], head[at + 2], head[at + 3]]);
        let kind = head[2];
        let flags = u16::from_le_bytes([head[3], head[4]]);
        let length = u64::from(u16::from_le_bytes([head[5], head[6]]));
        if length < OLD_SHORT {
            break;
        }
        let mut content = if flags & OLD_LONG == 0 {
            0
        } else {
            u64::from(word(7))
        };
        if kind == OLD_FILE && flags & OLD_LARGE != 0 {
            content |= u64::from(word(OLD_FIXED)) << 32;
        }
        let Some(next) = (at + length).checked_add(content) else {
            break;
        };
        match kind {
            OLD_MAIN if flags & OLD_SECRET != 0 => return Ok(Vec::new()),
            OLD_END => break,
            OLD_FILE => {
                if length < OLD_FIXED as u64 {
                    break;
                }
                let link = head[15] == OLD_UNIX && word(28) & KIND_MASK == KIND_LINK;
                let plain = head[25] == OLD_STORE
                    && flags & OLD_LOCKED == 0
                    && content <= MAX_LINK_TARGET
                    && next <= size;
                targets.push(if link && plain {
                    let mut target = vec![0u8; content as usize];
                    reader.seek(SeekFrom::Start(at + length))?;
                    reader.read_exact(&mut target)?;
                    Some(String::from_utf8_lossy(&target).into_owned())
                } else {
                    None
                });
            }
            _ => {}
        }
        at = next;
    }
    Ok(targets)
}

/// The list of an archive.
struct Listing {
    entries: Vec<Entry>,
    /// Files are compressed one after another as one stream: one is decoded
    /// only after all before it.
    solid: bool,
}

/// Reads the list of an archive.
fn list(source: &Source, passwords: &Passwords, named: &Named, ctx: &OpContext) -> Result<Listing> {
    let mut library = Library::new(ctx, passwords);
    let mut entries: Vec<Entry> = Vec::new();
    'attempt: loop {
        library.open(named, RAR_OM_LIST, None)?;
        entries.clear();
        let mut spent = 0u64;
        loop {
            ctx.checkpoint()?;
            match library.header() {
                Ok(true) => {}
                Ok(false) => break 'attempt,
                Err(Fault::Password) if entries.is_empty() => {
                    library.reject();
                    continue 'attempt;
                }
                // A password that opened the headers before this one is the
                // right one.
                Err(Fault::Password) => return Err(damaged("a header makes no sense")),
                Err(Fault::Failed(error)) => return Err(error),
            }
            let entry = library.describe(entries.len() as u64);
            // An archive may hold more names than there is memory for.
            spent += (size_of::<Entry>() + entry.meta.path.len()) as u64;
            if spent > ctx.budget.memory {
                return Err(Error::new("notEnoughMemory"));
            }
            entries.push(entry);
            // While listing, this only steps to the next header.
            library.process(RAR_SKIP).map_err(Fault::error)?;
        }
    }
    let solid = library.flags & ROADF_SOLID != 0;
    let secret = library.flags & ROADF_ENCHEADERS != 0;
    drop(library);
    let unresolved =
        |entry: &Entry| entry.meta.kind == EntryKind::Symlink && entry.meta.link_target.is_none();
    if !secret && entries.iter().any(unresolved) {
        // The headers are followed apart from the library: what is found is
        // trusted only when it is the same number of files.
        let targets = old_targets(source).unwrap_or_default();
        if targets.len() == entries.len() {
            for (entry, target) in entries.iter_mut().zip(targets) {
                if unresolved(entry) {
                    entry.meta.link_target = target;
                }
            }
        }
    }
    Ok(Listing { entries, solid })
}

/// The content of an entry as the sink reads it: the blocks of the library,
/// out of the queue.
struct Incoming<'a> {
    pieces: &'a Receiver<Piece>,
    block: Vec<u8>,
    at: usize,
    /// The end of the content has been seen.
    ended: bool,
}

impl Read for Incoming<'_> {
    fn read(&mut self, buffer: &mut [u8]) -> io::Result<usize> {
        loop {
            let rest = &self.block[self.at..];
            if !rest.is_empty() || buffer.is_empty() {
                let count = rest.len().min(buffer.len());
                buffer[..count].copy_from_slice(&rest[..count]);
                self.at += count;
                return Ok(count);
            }
            if self.ended {
                return Ok(0);
            }
            match self.pieces.recv() {
                Ok(Piece::Data(block)) => {
                    self.block = block;
                    self.at = 0;
                }
                Ok(Piece::End(outcome)) => {
                    self.ended = true;
                    outcome.map_err(Error::into_io)?;
                }
                // The library went away in the middle of an entry: it says
                // why itself.
                Ok(Piece::Start(_)) | Err(_) => {
                    self.ended = true;
                    return Err(State::gone().into_io());
                }
            }
        }
    }
}

/// Hands what the library decodes to the sink, entry by entry, until the
/// library is done.
fn receive(
    pieces: Receiver<Piece>,
    entries: &[Entry],
    sink: &dyn EntrySink,
    given_up: &AtomicUsize,
) -> Result<()> {
    loop {
        let index = match pieces.recv() {
            Ok(Piece::Start(index)) => index,
            // What is left of an entry the sink did not read to its end.
            Ok(Piece::Data(_) | Piece::End(Ok(()))) => continue,
            Ok(Piece::End(Err(error))) => return Err(error),
            Err(_) => return Ok(()),
        };
        let Some(entry) = entries.get(index) else {
            return Err(Error::new("internal").detail("an entry out of the list"));
        };
        let mut content = Incoming {
            pieces: &pieces,
            block: Vec::new(),
            at: 0,
            ended: false,
        };
        let mut data = Watched::new(&mut content);
        sink.accept(entry, &mut data)
            .map_err(|error| data.settle(entry, error))?;
        if !content.ended {
            // The library is told to keep the rest to itself.
            given_up.store(index + 1, Ordering::Release);
        }
    }
}

/// Sends a piece to the sink.
fn send(pieces: &SyncSender<Piece>, piece: Piece) -> Result<()> {
    pieces.send(piece).map_err(|_| State::gone())
}

/// Walks the archive with the library and sends the wanted entries to the
/// sink, through the queue.
fn drive(
    listing: &Listing,
    wanted: &[bool],
    passwords: &Passwords,
    named: &Named,
    changed: &dyn Fn() -> Error,
    ctx: &OpContext,
    pieces: &SyncSender<Piece>,
    given_up: &AtomicUsize,
) -> Result<()> {
    let mut left = wanted.iter().filter(|wanted| **wanted).count();
    // The entries before this one have been dealt with: after the archive
    // is opened again for another password, they are not sent again.
    let mut done = 0usize;
    let mut library = Library::new(ctx, passwords);
    library.state().pieces = Some(pieces);
    library.state().given_up = Some(given_up);
    'attempt: loop {
        library.open(named, RAR_OM_EXTRACT, None)?;
        let mut index = 0usize;
        while left > 0 {
            ctx.checkpoint()?;
            match library.header() {
                Ok(true) => {}
                Ok(false) => return Err(changed()),
                Err(Fault::Password) if index == 0 => {
                    library.reject();
                    continue 'attempt;
                }
                Err(Fault::Password) => return Err(damaged("a header makes no sense")),
                Err(Fault::Failed(error)) => return Err(error),
            }
            if index > 0 && library.continued() {
                // The rest of a file that was passed over in the volume
                // before; a file that is decoded takes its rest with it.
                library.process(RAR_SKIP).map_err(Fault::error)?;
                continue;
            }
            let Some(entry) = listing
                .entries
                .get(index)
                .filter(|entry| entry.meta.path == library.path())
            else {
                return Err(changed());
            };
            let want = index >= done && wanted[index];
            let is_file = entry.meta.is_file();
            if !is_file || !(want || listing.solid) {
                // Nothing to decode: a folder, a link, or a file nobody
                // waits for.
                library.process(RAR_SKIP).map_err(Fault::error)?;
                if want {
                    send(pieces, Piece::Start(index))?;
                    send(pieces, Piece::End(Ok(())))?;
                }
            } else {
                if library.dictionary() > ctx.budget.memory {
                    return Err(Error::new("notEnoughMemory"));
                }
                library.state().current = Some(Current {
                    index,
                    name: entry.meta.path.clone(),
                    wanted: want,
                    last: want && left == 1,
                    started: false,
                });
                let outcome = library.process(RAR_TEST);
                let state = library.state();
                let started = state.current.take().is_some_and(|current| current.started);
                if state.stopped {
                    return Ok(());
                }
                match outcome {
                    Ok(()) => {
                        if want {
                            if !started {
                                send(pieces, Piece::Start(index))?;
                            }
                            send(pieces, Piece::End(Ok(())))?;
                        }
                    }
                    // The password is checked before anything is decoded:
                    // the sink has seen nothing of the entry.
                    Err(Fault::Password) if !started => {
                        library.reject();
                        continue 'attempt;
                    }
                    // A file that is only decoded on the way to another: if
                    // it is damaged, the one that is wanted will say so.
                    Err(Fault::Failed(error)) if !want && error.code == "damaged" => {}
                    Err(fault) => {
                        let error = match fault.error() {
                            error if error.code == "unsupportedArchive" => {
                                unsupported_method(entry.method.as_deref().unwrap_or_default())
                            }
                            error => failed(entry, error),
                        };
                        if started {
                            send(pieces, Piece::End(Err(error.clone())))?;
                        }
                        return Err(error);
                    }
                }
            }
            if want {
                left -= 1;
            }
            index += 1;
            done = done.max(index);
        }
        return Ok(());
    }
}

/// A RAR archive opened for reading.
pub struct RarReader {
    source: Source,
    passwords: Passwords,
    /// The file the library reads when the archive is not one: the library
    /// opens files by their names.
    copy: Option<TempPath>,
    /// The file the archive starts in, once it has been looked for: the
    /// first volume, whichever of them was opened.
    start: Option<PathBuf>,
    /// The list, once it has been read.
    listing: Option<Listing>,
}

/// The first volume of the archive a volume belongs to, as far as the name
/// of the volume tells: `name.part03.rar` goes back to `name.part01.rar`,
/// and `name.r07` to `name.rar`. `None` for a name that is of a first
/// volume, or of no volume at all.
fn first_volume(path: &Path) -> Option<PathBuf> {
    let name = path.file_name()?.to_str()?;
    let (stem, extension) = name.rsplit_once('.')?;
    let digits = |text: &str| !text.is_empty() && text.bytes().all(|byte| byte.is_ascii_digit());
    let first = if extension.eq_ignore_ascii_case("rar") {
        // The numbers of the volumes are before the extension.
        let (base, part) = stem.rsplit_once('.')?;
        let number = part
            .get(..4)
            .filter(|word| word.eq_ignore_ascii_case("part"))
            .map(|_| &part[4..])
            .filter(|number| digits(number))?;
        if number.trim_start_matches('0') == "1" {
            return None;
        }
        let width = number.len();
        format!("{base}.{}{:0width$}.{extension}", &part[..4], 1)
    } else {
        // The numbers are the extension, from `r00` on; after `r99` comes
        // `s00`.
        let letter = extension.chars().next()?.to_ascii_lowercase();
        if extension.len() != 3 || !('r'..='z').contains(&letter) || !digits(&extension[1..]) {
            return None;
        }
        let rar = if extension.starts_with(letter) {
            "rar"
        } else {
            "RAR"
        };
        format!("{stem}.{rar}")
    };
    Some(path.with_file_name(first))
}

impl RarReader {
    pub fn new(source: Source, options: OpenOptions) -> Self {
        let passwords = Passwords::new(
            source.path().to_string_lossy().into_owned(),
            options.password,
        );
        Self {
            source,
            passwords,
            copy: None,
            start: None,
            listing: None,
        }
    }

    /// The file the library is to open.
    fn file(&mut self) -> Result<PathBuf> {
        // An archive in memory, and one cut into the numbered volumes of
        // another archiver, which the library does not know to follow.
        let bytes = self.source.held();
        if bytes.is_none() && self.source.volumes() == 1 {
            return Ok(self.source.path().to_path_buf());
        }
        if self.copy.is_none() {
            let mut file = tempfile::Builder::new()
                .prefix("boxit-")
                .suffix(".rar")
                .tempfile()?;
            match bytes {
                Some(bytes) => file.write_all(bytes)?,
                None => {
                    io::copy(&mut self.source.reader()?, &mut file)?;
                }
            }
            file.flush()?;
            // Closed, so that the library opens it as any other file; it is
            // removed when the reader goes.
            self.copy = Some(file.into_temp_path());
        }
        Ok(self.copy.as_deref().map_or_default(Path::to_path_buf))
    }

    /// The name the library opens the archive by: of the file itself, or
    /// of the first volume when the file is a later one.
    fn named(&mut self, ctx: &OpContext) -> Result<Named> {
        let file = self.file()?;
        let alone = self.copy.is_some();
        let named = Named::new(&file, alone)?;
        if alone {
            return Ok(named);
        }
        if let Some(start) = &self.start {
            return Named::new(start, false);
        }
        let Some(first) = first_volume(&file) else {
            self.start = Some(file);
            return Ok(named);
        };
        // The name is of a later volume; the archive says whether it is one.
        let mut library = Library::new(ctx, &self.passwords);
        library.open(&named, RAR_OM_LIST, None)?;
        let later = library.later_volume();
        drop(library);
        if !later {
            self.start = Some(file);
            return Ok(named);
        }
        if !first.is_file() {
            let name = first.file_name().unwrap_or_default().to_string_lossy();
            return Err(Error::new("volumeMissing").with("name", name));
        }
        let named = Named::new(&first, false)?;
        self.start = Some(first);
        Ok(named)
    }

    /// Reads the list if it has not been read, and gives the name the
    /// library opens the archive by.
    fn load(&mut self, ctx: &OpContext) -> Result<Named> {
        let named = self.named(ctx)?;
        if self.listing.is_none() {
            self.listing = Some(list(&self.source, &self.passwords, &named, ctx)?);
        }
        Ok(named)
    }
}

impl ArchiveReader for RarReader {
    fn capabilities(&self) -> Capabilities {
        let solid = self.listing.as_ref().is_some_and(|listing| listing.solid);
        Capabilities {
            random_access: !solid,
            solid,
            parallel: false,
            // There is no list apart from the data, but the headers say how
            // far the next one is: listing reads none of the data.
            has_index: true,
        }
    }

    fn entries(&mut self, ctx: &OpContext) -> Result<Vec<Entry>> {
        self.load(ctx)?;
        Ok(self
            .listing
            .as_ref()
            .map_or_default(|listing| listing.entries.clone()))
    }

    fn extract(&mut self, pick: &Selection, sink: &dyn EntrySink, ctx: &OpContext) -> Result<()> {
        let named = self.load(ctx)?;
        let Some(listing) = &self.listing else {
            return Ok(());
        };
        let wanted: Vec<bool> = listing
            .entries
            .iter()
            .map(|entry| pick.contains(entry))
            .collect();
        if !wanted.contains(&true) {
            return Ok(());
        }
        let path = self.source.path();
        let changed = || Error::new("archiveChanged").with("path", path.display());
        let passwords = &self.passwords;
        let entries = listing.entries.as_slice();
        let given_up = AtomicUsize::new(0);
        let (pieces, incoming) = sync_channel(QUEUE);
        thread::scope(|scope| {
            let given = &given_up;
            // The queue goes with the thread: when the sink fails, its end
            // of the queue closes and the library is stopped by that.
            let receiver = thread::Builder::new()
                .name(String::from("boxit-rar"))
                .spawn_scoped(scope, move || receive(incoming, entries, sink, given))?;
            let driven = drive(
                listing, &wanted, passwords, &named, &changed, ctx, &pieces, given,
            );
            drop(pieces);
            let received = receiver.join().unwrap_or_else(|panic| resume_unwind(panic));
            // When the sink failed, the library was stopped by it and has
            // nothing better to say.
            received.and(driven)
        })
    }

    fn comment(&mut self, ctx: &OpContext) -> Result<Option<String>> {
        let named = self.named(ctx)?;
        let mut buffer: Vec<WCHAR> = vec![0; COMMENT];
        let mut library = Library::new(ctx, &self.passwords);
        library.open(&named, RAR_OM_LIST, Some(&mut buffer))?;
        drop(library);
        let comment = text(&buffer);
        Ok((!comment.is_empty()).then_some(comment))
    }
}

#[cfg(test)]
mod tests {
    use std::fs;
    use std::sync::Mutex;

    use boxit_core::{Budget, Control, Password, Timestamp};
    use proptest::prelude::*;

    use super::*;
    use crate::support::{from_local, Civil};
    use crate::tests::{answering, lines, pattern, Collector, Got};

    const MARK5: [u8; 8] = *b"Rar!\x1A\x07\x01\x00";
    /// The kinds of the blocks of RAR 5.
    const MAIN5: u64 = 1;
    const FILE5: u64 = 2;
    const END5: u64 = 5;
    /// The record of a header that says what an entry stands for.
    const REDIRECT5: u64 = 5;
    /// The time of the files of an archive of RAR 5 made here.
    const UNIX_TIME: u32 = 1_700_000_000;
    /// The date and the time of DOS for noon of 2023-11-14, for the old
    /// format.
    const DOS_TIME: u32 = ((2023 - 1980) << 25) | (11 << 21) | (14 << 16) | (12 << 11);

    /// An entry of an archive made by hand, its content stored as it is.
    #[derive(Clone, Default)]
    struct Item {
        name: &'static str,
        data: Vec<u8>,
        folder: bool,
        /// It comes from Unix, and `attributes` are a mode.
        unix: bool,
        attributes: u32,
        /// What the entry stands for, and the name of it: RAR 5 only.
        redirect: Option<(u64, &'static str)>,
        /// What the header says of the compression instead of "stored":
        /// RAR 5 only.
        compression: Option<u64>,
    }

    fn file(name: &'static str, data: Vec<u8>) -> Item {
        Item {
            name,
            data,
            attributes: 0x20,
            ..Item::default()
        }
    }

    fn folder(name: &'static str) -> Item {
        Item {
            name,
            folder: true,
            attributes: 0x10,
            ..Item::default()
        }
    }

    fn redirect(name: &'static str, kind: c_uint, target: &'static str) -> Item {
        Item {
            name,
            unix: true,
            attributes: 0o120_777,
            redirect: Some((u64::from(kind), target)),
            ..Item::default()
        }
    }

    /// A number of RAR 5: seven bits to a byte, the lowest first.
    fn vint(mut value: u64) -> Vec<u8> {
        let mut bytes = Vec::new();
        loop {
            let low = (value & 0x7F) as u8;
            value >>= 7;
            if value == 0 {
                bytes.push(low);
                return bytes;
            }
            bytes.push(low | 0x80);
        }
    }

    /// A block of RAR 5: its fields and its records behind their sizes and
    /// the sum of them all, then its data.
    fn block5(kind: u64, fields: &[u8], records: &[u8], data: Option<&[u8]>) -> Vec<u8> {
        split5(kind, 0, fields, records, data)
    }

    /// The data of the block goes on from the volume before.
    const SPLIT_BEFORE: u64 = 0x08;
    /// The data of the block goes on in the next volume.
    const SPLIT_AFTER: u64 = 0x10;

    /// A block of RAR 5 whose data is a part of what the volumes hold
    /// between them, as `split` says.
    fn split5(
        kind: u64,
        split: u64,
        fields: &[u8],
        records: &[u8],
        data: Option<&[u8]>,
    ) -> Vec<u8> {
        let mut flags = split;
        if !records.is_empty() {
            flags |= 1;
        }
        if data.is_some() {
            flags |= 2;
        }
        let mut head = vint(kind);
        head.extend(vint(flags));
        if !records.is_empty() {
            head.extend(vint(records.len() as u64));
        }
        if let Some(data) = data {
            head.extend(vint(data.len() as u64));
        }
        head.extend(fields);
        head.extend(records);
        let mut sized = vint(head.len() as u64);
        sized.extend(head);
        let mut block = crc32fast::hash(&sized).to_le_bytes().to_vec();
        block.extend(sized);
        block.extend(data.unwrap_or_default());
        block
    }

    /// The fields and the records of the header of a file of RAR 5. `sum`
    /// is the sum the header gives for the content; `continues`: the file
    /// is compressed on from where the one before it ended.
    fn header5(item: &Item, sum: u32, continues: bool) -> (Vec<u8>, Vec<u8>) {
        let mut fields = vint(if item.folder { 0x3 } else { 0x6 });
        if item.folder {
            fields.extend(vint(0));
            fields.extend(vint(u64::from(item.attributes)));
            fields.extend(UNIX_TIME.to_le_bytes());
        } else {
            fields.extend(vint(item.data.len() as u64));
            fields.extend(vint(u64::from(item.attributes)));
            fields.extend(UNIX_TIME.to_le_bytes());
            fields.extend(sum.to_le_bytes());
        }
        fields.extend(vint(
            item.compression.unwrap_or(0) | if continues { 0x40 } else { 0 },
        ));
        fields.extend(vint(u64::from(item.unix)));
        fields.extend(vint(item.name.len() as u64));
        fields.extend(item.name.as_bytes());
        let mut records = Vec::new();
        if let Some((kind, target)) = item.redirect {
            let mut record = vint(REDIRECT5);
            record.extend(vint(kind));
            record.extend(vint(0));
            record.extend(vint(target.len() as u64));
            record.extend(target.as_bytes());
            records.extend(vint(record.len() as u64));
            records.extend(record);
        }
        (fields, records)
    }

    /// The start of a volume of an archive of RAR 5: the first one is
    /// number zero, and does not say its number.
    fn volume5(number: usize) -> Vec<u8> {
        let mut fields = vint(if number == 0 { 1 } else { 3 });
        if number > 0 {
            fields.extend(vint(number as u64));
        }
        let mut bytes = MARK5.to_vec();
        bytes.extend(block5(MAIN5, &fields, &[], None));
        bytes
    }

    /// An archive of RAR 5 in volumes that hold `room` bytes of content
    /// each: a file that does not fit goes on in the next volume. A part
    /// of a file gives the sum of itself, and the last part the sum of the
    /// whole file.
    fn volumes5(items: &[Item], room: usize) -> Vec<Vec<u8>> {
        let mut volumes: Vec<Vec<u8>> = Vec::new();
        let mut bytes = volume5(0);
        let mut left = room;
        let mut next = |bytes: &mut Vec<u8>, left: &mut usize| {
            bytes.extend(block5(END5, &vint(1), &[], None));
            volumes.push(mem::replace(bytes, volume5(volumes.len() + 1)));
            *left = room;
        };
        for item in items {
            let plain = !item.folder && item.redirect.is_none();
            let mut rest = item.data.as_slice();
            let mut split = 0;
            loop {
                if left == 0 && !rest.is_empty() {
                    next(&mut bytes, &mut left);
                }
                let (part, after) = rest.split_at(rest.len().min(left));
                left -= part.len();
                rest = after;
                let sum = if rest.is_empty() {
                    crc32fast::hash(&item.data)
                } else {
                    split |= SPLIT_AFTER;
                    crc32fast::hash(part)
                };
                let (fields, records) = header5(item, sum, false);
                bytes.extend(split5(
                    FILE5,
                    split,
                    &fields,
                    &records,
                    plain.then_some(part),
                ));
                if rest.is_empty() {
                    break;
                }
                split = SPLIT_BEFORE;
            }
        }
        bytes.extend(block5(END5, &vint(0), &[], None));
        volumes.push(bytes);
        volumes
    }

    /// An archive of RAR 5. In a solid one every file but the first says
    /// that it goes on where the one before it ended.
    fn rar5(solid: bool, items: &[Item]) -> Vec<u8> {
        let mut bytes = MARK5.to_vec();
        bytes.extend(block5(MAIN5, &vint(if solid { 4 } else { 0 }), &[], None));
        let mut first = true;
        for item in items {
            let plain = !item.folder && item.redirect.is_none();
            let continues = solid && plain && !first;
            let sum = crc32fast::hash(&item.data);
            let (fields, records) = header5(item, sum, continues);
            bytes.extend(block5(
                FILE5,
                &fields,
                &records,
                plain.then_some(item.data.as_slice()),
            ));
            first &= !plain;
        }
        bytes.extend(block5(END5, &vint(0), &[], None));
        bytes
    }

    /// A block of the old format: its fields behind its kind, its flags,
    /// its size and the low half of the sum of them all, then its data.
    fn block4(kind: u8, flags: u16, fields: &[u8], data: &[u8]) -> Vec<u8> {
        let mut rest = vec![kind];
        rest.extend(flags.to_le_bytes());
        rest.extend((OLD_SHORT as u16 + fields.len() as u16).to_le_bytes());
        rest.extend(fields);
        let mut block = (crc32fast::hash(&rest) as u16).to_le_bytes().to_vec();
        block.extend(rest);
        block.extend(data);
        block
    }

    /// An archive of the old format, as RAR 3 wrote them. A symbolic link
    /// of Unix keeps its target as its content.
    fn rar4(items: &[Item]) -> Vec<u8> {
        let mut bytes = OLD_MARK.to_vec();
        bytes.extend(block4(OLD_MAIN, 0, &[0; 6], &[]));
        for item in items {
            let data: &[u8] = match item.redirect {
                Some((_, target)) => target.as_bytes(),
                None => &item.data,
            };
            let mut flags = OLD_LONG;
            if item.folder {
                flags |= 0x00E0;
            }
            let mut name = item.name.as_bytes().to_vec();
            if !item.name.is_ascii() {
                // The name in the one-byte encoding of the machine that
                // made the archive, then the same name in Unicode, packed
                // the way of RAR: a byte that says how each of the next
                // four characters is written, here always as two bytes.
                flags |= 0x0200;
                let wide: Vec<u16> = item.name.encode_utf16().collect();
                name = vec![b'_'; wide.len()];
                name.extend([0, 0]);
                for four in wide.chunks(4) {
                    name.push(0xAA);
                    name.extend(four.iter().flat_map(|unit| unit.to_le_bytes()));
                }
            }
            let mut fields = Vec::new();
            fields.extend((data.len() as u32).to_le_bytes());
            fields.extend((data.len() as u32).to_le_bytes());
            fields.push(if item.unix { OLD_UNIX } else { 2 });
            fields.extend(crc32fast::hash(data).to_le_bytes());
            fields.extend(DOS_TIME.to_le_bytes());
            fields.push(29);
            fields.push(OLD_STORE);
            fields.extend((name.len() as u16).to_le_bytes());
            fields.extend(item.attributes.to_le_bytes());
            fields.extend(name);
            bytes.extend(block4(OLD_FILE, flags, &fields, data));
        }
        bytes.extend(block4(OLD_END, 0x4000, &[], &[]));
        bytes
    }

    fn open(bytes: Vec<u8>, password: Option<&str>) -> RarReader {
        let options = OpenOptions {
            password: password.map(Password::from),
            ..OpenOptions::default()
        };
        RarReader::new(Source::memory("test.rar", bytes), options)
    }

    /// The list of an archive, and all it holds.
    fn read(bytes: Vec<u8>) -> (Vec<Entry>, Vec<Got>) {
        let ctx = OpContext::silent();
        let mut reader = open(bytes, None);
        let entries = reader.entries(&ctx).unwrap();
        let sink = Collector::default();
        reader.extract(&Selection::All, &sink, &ctx).unwrap();
        (entries, sink.in_order())
    }

    /// The code of the error an archive is refused with, and the entry the
    /// error names.
    fn refused(bytes: Vec<u8>) -> (String, Option<String>) {
        let ctx = OpContext::silent();
        let mut reader = open(bytes, None);
        let error = reader
            .extract(&Selection::All, &Collector::default(), &ctx)
            .unwrap_err();
        (error.code.clone(), error.params.get("name").cloned())
    }

    fn tree() -> Vec<Item> {
        vec![
            folder("docs"),
            file("docs/readme.txt", lines(50)),
            Item {
                unix: true,
                attributes: 0o100_640,
                ..file(
                    "docs/пример текста.txt",
                    "Привет, архив!".as_bytes().to_vec(),
                )
            },
            file("empty.bin", Vec::new()),
            // Larger than two blocks of the library.
            file("big.bin", pattern(9 * 1024 * 1024)),
        ]
    }

    /// What the reader made of the tree, apart from the times.
    fn assert_tree(entries: &[Entry], got: &[Got], label: &str) {
        let tree = tree();
        assert_eq!(entries.len(), tree.len(), "{label}");
        assert_eq!(got.len(), tree.len(), "{label}");
        for ((item, entry), got) in tree.iter().zip(entries).zip(got) {
            let name = item.name;
            assert_eq!(entry.meta.path, name, "{label}");
            assert_eq!(got.entry, *entry, "{label}: {name}");
            assert_eq!(got.data, item.data, "{label}: {name}");
            assert_eq!(
                entry.meta.size,
                Some(item.data.len() as u64),
                "{label}: {name}"
            );
            assert!(!entry.encrypted, "{label}: {name}");
            if item.folder {
                assert_eq!(entry.meta.kind, EntryKind::Directory, "{label}: {name}");
                assert_eq!(entry.method, None, "{label}: {name}");
                assert_eq!(entry.crc32, None, "{label}: {name}");
            } else {
                assert_eq!(entry.meta.kind, EntryKind::File, "{label}: {name}");
                assert_eq!(entry.method.as_deref(), Some("Store"), "{label}: {name}");
                assert_eq!(
                    entry.packed_size,
                    Some(item.data.len() as u64),
                    "{label}: {name}"
                );
                assert_eq!(
                    entry.crc32,
                    Some(crc32fast::hash(&item.data)),
                    "{label}: {name}"
                );
            }
            if item.unix {
                assert_eq!(entry.meta.unix_mode, Some(0o640), "{label}: {name}");
                assert_eq!(entry.meta.windows_attributes, None, "{label}: {name}");
            } else {
                assert_eq!(entry.meta.unix_mode, None, "{label}: {name}");
                assert_eq!(
                    entry.meta.windows_attributes,
                    Some(item.attributes),
                    "{label}: {name}"
                );
            }
        }
    }

    #[test]
    fn stored_archives_of_both_formats_are_read() {
        let (entries, got) = read(rar5(false, &tree()));
        assert_tree(&entries, &got, "RAR 5");
        for entry in &entries {
            assert_eq!(
                entry.meta.modified,
                Some(Timestamp::from_seconds(i64::from(UNIX_TIME)))
            );
        }

        let (entries, got) = read(rar4(&tree()));
        assert_tree(&entries, &got, "RAR 4");
        // The old format keeps the time of the clock on the wall, and the
        // library has its own way from there to a moment.
        let noon = from_local(Civil {
            year: 2023,
            month: 11,
            day: 14,
            hour: 12,
            minute: 0,
            second: 0,
        })
        .unwrap();
        for entry in &entries {
            let modified = entry.meta.modified.unwrap();
            assert!(
                (modified.seconds - noon.seconds).abs() <= 3600,
                "{modified:?}"
            );
        }

        // Nothing but its mark and its headers.
        for empty in [rar5(false, &[]), rar4(&[])] {
            let (entries, got) = read(empty);
            assert_eq!(entries.len(), 0);
            assert!(got.is_empty());
        }
    }

    #[test]
    fn the_archives_of_rar_itself_are_read() {
        // Compressed by RAR 3, on Unix.
        let (entries, got) = read(include_bytes!("../fixtures/rar4-stored.rar").to_vec());
        assert_eq!(entries.len(), 1);
        let entry = &entries[0];
        assert_eq!(entry.meta.path, "VERSION");
        assert_eq!(entry.meta.kind, EntryKind::File);
        assert_eq!(entry.meta.size, Some(11));
        assert_eq!(entry.packed_size, Some(21));
        assert_eq!(entry.method.as_deref(), Some("RAR 3"));
        assert_eq!(entry.meta.unix_mode, Some(0o644));
        // RAR 4 stores the clock on the wall. UnRAR turns that into a moment
        // with the time zone of this machine, so the check is the wall time.
        let modified = entry.meta.modified.unwrap();
        let wall = from_local(Civil {
            year: 2015,
            month: 8,
            day: 7,
            hour: 17,
            minute: 21,
            second: 8,
        })
        .unwrap();
        assert_eq!(modified.seconds, wall.seconds);
        assert_eq!(modified.nanos, 0);
        assert_eq!(entry.crc32, Some(crc32fast::hash(b"unrar-0.4.0")));
        assert_eq!(got[0].data, b"unrar-0.4.0");

        // A name outside ASCII and a time finer than a second, of RAR 5.
        let (entries, got) = read(include_bytes!("../fixtures/rar5-unicode.rar").to_vec());
        assert_eq!(entries[0].meta.path, "unicodefilename❤\u{fe0f}.txt");
        assert_eq!(
            entries[0].meta.modified,
            Some(Timestamp {
                seconds: 1_725_684_613,
                nanos: 622_536_200
            })
        );
        assert_eq!(got[0].data, b"foobar\n");

        // Compressed by RAR 5, as one stream.
        let ctx = OpContext::silent();
        let mut reader = open(include_bytes!("../fixtures/rar5-solid.rar").to_vec(), None);
        assert!(!reader.capabilities().solid);
        let entries = reader.entries(&ctx).unwrap();
        assert_eq!(entries[0].method.as_deref(), Some("RAR 5"));
        assert_eq!(
            reader.capabilities(),
            Capabilities {
                random_access: false,
                solid: true,
                parallel: false,
                has_index: true,
            }
        );
        let sink = Collector::default();
        reader.extract(&Selection::All, &sink, &ctx).unwrap();
        assert_eq!(sink.in_order()[0].data, b"target\nCargo.lock\n");
    }

    #[test]
    fn a_comment_is_read() {
        let ctx = OpContext::silent();
        let mut reader = open(
            include_bytes!("../fixtures/rar5-comment.rar").to_vec(),
            None,
        );
        assert_eq!(
            reader.comment(&ctx).unwrap().as_deref(),
            Some("abcdef12345\n")
        );
        let mut reader = open(include_bytes!("../fixtures/rar4-stored.rar").to_vec(), None);
        assert_eq!(reader.comment(&ctx).unwrap(), None);
        // Behind the password of the headers.
        let secret = include_bytes!("../fixtures/rar5-secret.rar").to_vec();
        let mut reader = open(secret.clone(), Some("password"));
        assert_eq!(
            reader.comment(&ctx).unwrap().as_deref(),
            Some("abcdef12345\n")
        );
        let mut reader = open(secret, None);
        assert_eq!(reader.comment(&ctx).unwrap_err().code, "passwordRequired");
    }

    #[test]
    fn an_encrypted_file_asks_for_its_password() {
        let locked = include_bytes!("../fixtures/rar4-locked.rar").to_vec();
        let content = b"target\nCargo.lock\n";
        let silent = OpContext::silent();

        // The list is open to everyone.
        let mut reader = open(locked.clone(), None);
        let entries = reader.entries(&silent).unwrap();
        assert_eq!(entries[0].meta.path, ".gitignore");
        assert!(entries[0].encrypted);
        let error = reader
            .extract(&Selection::All, &Collector::default(), &silent)
            .unwrap_err();
        assert_eq!(error.code, "passwordRequired");

        let mut reader = open(locked.clone(), Some("unrar"));
        let sink = Collector::default();
        reader.extract(&Selection::All, &sink, &silent).unwrap();
        assert_eq!(sink.in_order()[0].data, content);

        let (ctx, answers) = answering(vec!["unrar"]);
        let mut reader = open(locked.clone(), None);
        let sink = Collector::default();
        reader.extract(&Selection::All, &sink, &ctx).unwrap();
        assert_eq!(sink.in_order()[0].data, content);
        assert_eq!(answers.asked.load(Ordering::SeqCst), 1);
        assert_eq!(answers.retries.load(Ordering::SeqCst), 0);
        // Once is enough.
        reader
            .extract(&Selection::All, &Collector::default(), &ctx)
            .unwrap();
        assert_eq!(answers.asked.load(Ordering::SeqCst), 1);

        // The old format has nothing to check a password against: a wrong
        // one shows as content that fails its sum.
        let mut reader = open(locked, Some("wrong"));
        let error = reader
            .extract(&Selection::All, &Collector::default(), &silent)
            .unwrap_err();
        assert_eq!(error.code, "damagedOrWrongPassword");
        assert_eq!(error.params["name"], ".gitignore");
    }

    #[test]
    fn encrypted_headers_ask_for_their_password() {
        let secret = include_bytes!("../fixtures/rar5-secret.rar").to_vec();
        let silent = OpContext::silent();

        let mut reader = open(secret.clone(), None);
        assert_eq!(
            reader.entries(&silent).unwrap_err().code,
            "passwordRequired"
        );
        // A wrong one, and nobody to ask for another.
        let mut reader = open(secret.clone(), Some("wrong"));
        assert_eq!(reader.entries(&silent).unwrap_err().code, "wrongPassword");

        let mut reader = open(secret.clone(), Some("password"));
        let entries = reader.entries(&silent).unwrap();
        assert_eq!(entries[0].meta.path, ".gitignore");
        assert!(entries[0].encrypted);
        // The sum of an encrypted file of RAR 5 says nothing of its content.
        assert_eq!(entries[0].crc32, None);

        // Asked until the password fits, and then no more.
        let (ctx, answers) = answering(vec!["wrong", "", "password"]);
        let mut reader = open(secret.clone(), None);
        assert_eq!(reader.entries(&ctx).unwrap().len(), 1);
        assert_eq!(answers.asked.load(Ordering::SeqCst), 3);
        assert_eq!(answers.retries.load(Ordering::SeqCst), 2);
        let sink = Collector::default();
        reader.extract(&Selection::All, &sink, &ctx).unwrap();
        assert_eq!(sink.in_order()[0].data, b"target\nCargo.lock\n");
        assert_eq!(answers.asked.load(Ordering::SeqCst), 3);

        // Given up on after a wrong one.
        let (ctx, answers) = answering(vec!["wrong"]);
        let mut reader = open(secret, None);
        assert_eq!(reader.entries(&ctx).unwrap_err().code, "wrongPassword");
        assert_eq!(answers.asked.load(Ordering::SeqCst), 2);
    }

    #[test]
    fn links_are_listed_with_what_they_point_to() {
        let items = [
            file("real.txt", b"real".to_vec()),
            redirect("link", REDIRECT_UNIX_LINK, "sub/../real.txt"),
            redirect("windows", REDIRECT_WINDOWS_LINK, "sub\\real.txt"),
            redirect("far", REDIRECT_JUNCTION, "\\??\\C:\\Users\\far"),
            redirect("far too", REDIRECT_WINDOWS_LINK, "/??/C:/Users/far"),
            redirect("hard.txt", REDIRECT_HARD_LINK, "real.txt"),
            redirect("copy.txt", REDIRECT_COPY, "real.txt"),
        ];
        let (entries, got) = read(rar5(false, &items));
        let listed: Vec<_> = entries
            .iter()
            .map(|entry| {
                (
                    entry.meta.path.as_str(),
                    entry.meta.kind,
                    entry.meta.link_target.as_deref(),
                )
            })
            .collect();
        assert_eq!(
            listed,
            [
                ("real.txt", EntryKind::File, None),
                ("link", EntryKind::Symlink, Some("sub/../real.txt")),
                ("windows", EntryKind::Symlink, Some("sub/real.txt")),
                ("far", EntryKind::Symlink, Some("C:/Users/far")),
                ("far too", EntryKind::Symlink, Some("C:/Users/far")),
                ("hard.txt", EntryKind::Hardlink, Some("real.txt")),
                ("copy.txt", EntryKind::Hardlink, Some("real.txt")),
            ]
        );
        // All of them reach the sink, and none but the file has content.
        assert_eq!(got.len(), items.len());
        for (entry, got) in entries.iter().zip(&got) {
            assert_eq!(got.entry, *entry);
            assert_eq!(got.data.is_empty(), entry.meta.kind != EntryKind::File);
            assert_eq!(entry.method.is_some(), entry.meta.kind == EntryKind::File);
        }

        // The old format keeps the target as the content of the link.
        let old = [
            file("real.txt", b"real".to_vec()),
            redirect("link", REDIRECT_UNIX_LINK, "sub/../real.txt"),
            Item {
                unix: true,
                attributes: 0o100_755,
                ..file("tool", b"#!/bin/sh\n".to_vec())
            },
            redirect("ссылка", REDIRECT_UNIX_LINK, "папка/файл"),
        ];
        let (entries, got) = read(rar4(&old));
        let listed: Vec<_> = entries
            .iter()
            .map(|entry| {
                (
                    entry.meta.path.as_str(),
                    entry.meta.kind,
                    entry.meta.link_target.as_deref(),
                )
            })
            .collect();
        assert_eq!(
            listed,
            [
                ("real.txt", EntryKind::File, None),
                ("link", EntryKind::Symlink, Some("sub/../real.txt")),
                ("tool", EntryKind::File, None),
                ("ссылка", EntryKind::Symlink, Some("папка/файл")),
            ]
        );
        assert_eq!(entries[2].meta.unix_mode, Some(0o755));
        assert_eq!(got.len(), old.len());
        assert_eq!(
            got[1].entry.meta.link_target.as_deref(),
            Some("sub/../real.txt")
        );
        assert_eq!(got[1].data.len(), 0);
        assert_eq!(got[2].data, b"#!/bin/sh\n");
    }

    #[test]
    fn the_targets_of_old_links_are_found_only_where_they_are() {
        let link = redirect("link", REDIRECT_UNIX_LINK, "target");
        let source = |bytes: Vec<u8>| Source::memory("test.rar", bytes);
        let plain = rar4(&[file("a", b"a".to_vec()), link.clone()]);
        assert_eq!(
            old_targets(&source(plain.clone())).unwrap(),
            [None, Some(String::from("target"))]
        );
        // Not the old format, and nothing at all.
        let newer = rar5(false, slice::from_ref(&link));
        assert_eq!(old_targets(&source(newer)).unwrap().len(), 0);
        assert_eq!(old_targets(&source(Vec::new())).unwrap().len(), 0);
        // The headers are encrypted: there is nothing to follow.
        let mut secret = OLD_MARK.to_vec();
        secret.extend(block4(OLD_MAIN, OLD_SECRET, &[0; 6], &[]));
        secret.extend(&plain[OLD_MARK.len() + 13..]);
        assert_eq!(old_targets(&source(secret)).unwrap().len(), 0);
        // The content of the link is cut short, is encrypted, is too long.
        let cut = plain[..plain.len() - 10].to_vec();
        assert_eq!(old_targets(&source(cut)).unwrap(), [None, None]);
        let at = OLD_MARK.len() + 13 + 7 + 25 + 1 + 1;
        let mut locked = plain.clone();
        locked[at + 3] |= OLD_LOCKED as u8;
        assert_eq!(old_targets(&source(locked)).unwrap(), [None, None]);
        // A header shorter than a header stops the walk.
        let mut short = plain;
        short[at + 5] = 3;
        short[at + 6] = 0;
        assert_eq!(old_targets(&source(short)).unwrap(), [None]);
    }

    fn numbered() -> Vec<Item> {
        vec![
            file("one.txt", b"one".to_vec()),
            folder("sub"),
            file("sub/two.txt", lines(2000)),
            file("three.bin", pattern(70_000)),
            file("four.txt", b"four".to_vec()),
        ]
    }

    #[test]
    fn a_selection_is_what_arrives() {
        let ctx = OpContext::silent();
        for (label, bytes, solid) in [
            ("RAR 5", rar5(false, &numbered()), false),
            ("solid", rar5(true, &numbered()), true),
            ("RAR 4", rar4(&numbered()), false),
        ] {
            let mut reader = open(bytes, None);
            let sink = Collector::default();
            reader
                .extract(&Selection::indexes(vec![3, 1]), &sink, &ctx)
                .unwrap();
            assert_eq!(reader.capabilities().solid, solid, "{label}");
            let got = sink.in_order();
            let paths: Vec<_> = got.iter().map(|got| got.entry.meta.path.as_str()).collect();
            assert_eq!(paths, ["sub", "three.bin"], "{label}");
            assert_eq!(got[1].data, pattern(70_000), "{label}");

            let sink = Collector::default();
            reader
                .extract(&Selection::paths(["four.txt", "one.txt"]), &sink, &ctx)
                .unwrap();
            let got = sink.in_order();
            let paths: Vec<_> = got.iter().map(|got| got.entry.meta.path.as_str()).collect();
            assert_eq!(paths, ["one.txt", "four.txt"], "{label}");
            assert_eq!(got[1].data, b"four", "{label}");

            // Nothing wanted, nothing done.
            let sink = Collector::default();
            reader
                .extract(&Selection::indexes(vec![99]), &sink, &ctx)
                .unwrap();
            assert!(sink.in_order().is_empty(), "{label}");
        }
    }

    /// Reads the first byte of every entry and leaves the rest.
    #[derive(Default)]
    struct Taster(Mutex<Vec<(String, Option<u8>)>>);

    impl EntrySink for Taster {
        fn accept(&self, entry: &Entry, data: &mut dyn Read) -> Result<()> {
            let mut first = [0u8; 1];
            let count = data.read(&mut first)?;
            self.0
                .lock()
                .unwrap()
                .push((entry.meta.path.clone(), (count == 1).then_some(first[0])));
            Ok(())
        }
    }

    #[test]
    fn a_sink_may_leave_the_content_unread() {
        let ctx = OpContext::silent();
        let large = |seed: u8| {
            let mut data = pattern(9 * 1024 * 1024);
            data[0] = seed;
            data
        };
        let items = [
            file("a.bin", large(b'a')),
            file("b.bin", large(b'b')),
            folder("sub"),
            file("empty", Vec::new()),
            file("c.bin", large(b'c')),
        ];
        for (label, bytes) in [
            ("RAR 5", rar5(false, &items)),
            ("solid", rar5(true, &items)),
            ("RAR 4", rar4(&items)),
        ] {
            let mut reader = open(bytes, None);
            let sink = Taster::default();
            reader.extract(&Selection::All, &sink, &ctx).unwrap();
            assert_eq!(
                sink.0.into_inner().unwrap(),
                [
                    (String::from("a.bin"), Some(b'a')),
                    (String::from("b.bin"), Some(b'b')),
                    (String::from("sub"), None),
                    (String::from("empty"), None),
                    (String::from("c.bin"), Some(b'c')),
                ],
                "{label}"
            );
            // What was left unread does not spoil what comes after.
            let sink = Collector::default();
            reader
                .extract(&Selection::paths(["b.bin"]), &sink, &ctx)
                .unwrap();
            assert_eq!(sink.in_order()[0].data, large(b'b'), "{label}");
        }
    }

    /// Fails at the entry of a given path.
    struct Full(&'static str);

    impl EntrySink for Full {
        fn accept(&self, entry: &Entry, data: &mut dyn Read) -> Result<()> {
            let mut first = [0u8; 1];
            let _ = data.read(&mut first)?;
            if entry.meta.path == self.0 {
                return Err(Error::new("diskFull"));
            }
            io::copy(data, &mut io::sink())?;
            Ok(())
        }
    }

    /// Cancels the operation at its first entry, and reads on.
    struct Canceller(Control);

    impl EntrySink for Canceller {
        fn accept(&self, _entry: &Entry, data: &mut dyn Read) -> Result<()> {
            self.0.cancel();
            io::copy(data, &mut io::sink())?;
            Ok(())
        }
    }

    #[test]
    fn a_failure_of_the_sink_and_a_cancellation_stop_the_library() {
        let large = || pattern(9 * 1024 * 1024);
        let items = [
            file("a.bin", large()),
            file("b.bin", large()),
            file("c.bin", large()),
            file("d.bin", large()),
        ];
        for (label, bytes) in [
            ("RAR 5", rar5(false, &items)),
            ("solid", rar5(true, &items)),
            ("RAR 4", rar4(&items)),
        ] {
            for path in ["a.bin", "c.bin", "d.bin"] {
                let ctx = OpContext::silent();
                let mut reader = open(bytes.clone(), None);
                let error = reader
                    .extract(&Selection::All, &Full(path), &ctx)
                    .unwrap_err();
                assert_eq!(error.code, "diskFull", "{label}: {path}");
            }

            let ctx = OpContext::silent();
            let mut reader = open(bytes, None);
            let error = reader
                .extract(&Selection::All, &Canceller(ctx.control.clone()), &ctx)
                .unwrap_err();
            assert!(error.is_cancelled(), "{label}: {error:?}");
            // And at the list.
            let mut reader = open(rar5(false, &items), None);
            assert!(reader.entries(&ctx).unwrap_err().is_cancelled(), "{label}");
        }
    }

    #[test]
    fn what_is_broken_is_said_to_be_broken() {
        let items = numbered();
        for (label, bytes) in [
            ("RAR 5", rar5(false, &items)),
            ("solid", rar5(true, &items)),
            ("RAR 4", rar4(&items)),
        ] {
            // A byte of the content of the large file: the last 70 000
            // bytes before the last file and the end.
            let mut spoiled = bytes.clone();
            let at = spoiled.len() - 40_000;
            spoiled[at] ^= 0x55;
            assert_eq!(
                refused(spoiled),
                (
                    String::from("damagedEntry"),
                    Some(String::from("three.bin"))
                ),
                "{label}"
            );
            // The same when the file is the only one asked for, and the
            // files around it are read as before.
            let mut spoiled = bytes.clone();
            spoiled[at] ^= 0x55;
            let ctx = OpContext::silent();
            let mut reader = open(spoiled, None);
            let error = reader
                .extract(
                    &Selection::paths(["three.bin"]),
                    &Collector::default(),
                    &ctx,
                )
                .unwrap_err();
            assert_eq!(error.code, "damagedEntry", "{label}");
            let sink = Collector::default();
            reader
                .extract(&Selection::paths(["one.txt", "four.txt"]), &sink, &ctx)
                .unwrap();
            let got = sink.in_order();
            assert_eq!(got.len(), 2, "{label}");
            assert_eq!(got[1].data, b"four", "{label}");

            // Cut in the middle of the large file.
            let cut = bytes[..bytes.len() - 40_000].to_vec();
            let (code, name) = refused(cut);
            assert_eq!(code, "damagedEntry", "{label}");
            assert_eq!(name.as_deref(), Some("three.bin"), "{label}");
        }

        // The mark of an archive and nothing an archive is made of.
        for mark in [&MARK5[..], &OLD_MARK[..]] {
            let mut noise = mark.to_vec();
            noise.extend(pattern(4000));
            let ctx = OpContext::silent();
            let mut reader = open(noise, None);
            match reader.entries(&ctx) {
                Ok(entries) => assert_eq!(entries.len(), 0),
                Err(error) => assert_eq!(error.code, "damaged"),
            }
        }
        // Not even the mark.
        let ctx = OpContext::silent();
        let mut reader = open(pattern(4000), None);
        assert_eq!(reader.entries(&ctx).unwrap_err().code, "damaged");
        let mut reader = open(Vec::new(), None);
        assert_eq!(reader.entries(&ctx).unwrap_err().code, "damaged");
    }

    /// Writes the volumes of an archive into a folder under the names
    /// RAR gives them, and tells the names.
    fn plant_volumes(folder: &Path, volumes: &[Vec<u8>]) -> Vec<PathBuf> {
        volumes
            .iter()
            .enumerate()
            .map(|(index, volume)| {
                let path = folder.join(format!("set.part{}.rar", index + 1));
                fs::write(&path, volume).unwrap();
                path
            })
            .collect()
    }

    fn on_disk(path: &Path) -> RarReader {
        RarReader::new(Source::open(path).unwrap(), OpenOptions::default())
    }

    #[test]
    fn an_archive_in_volumes_is_read_from_any_of_them() {
        let ctx = OpContext::silent();
        let folder = tempfile::tempdir().unwrap();
        // The large file takes five volumes; two files come after it.
        let mut items = tree();
        items.push(file("after/tail.txt", lines(20)));
        items.push(file("after/last.bin", pattern(3 * 1024 * 1024)));
        let volumes = volumes5(&items, 2 * 1024 * 1024);
        assert_eq!(volumes.len(), 7);
        let paths = plant_volumes(folder.path(), &volumes);

        for opened in [0, 3, 6] {
            let mut reader = on_disk(&paths[opened]);
            let entries = reader.entries(&ctx).unwrap();
            let names: Vec<_> = entries
                .iter()
                .map(|entry| entry.meta.path.as_str())
                .collect();
            let wanted: Vec<_> = items.iter().map(|item| item.name).collect();
            assert_eq!(names, wanted, "volume {opened}");
            let sink = Collector::default();
            reader.extract(&Selection::All, &sink, &ctx).unwrap();
            let got = sink.in_order();
            assert_eq!(got.len(), items.len(), "volume {opened}");
            for ((item, entry), got) in items.iter().zip(&entries).zip(&got) {
                assert_eq!(got.entry, *entry, "volume {opened}: {}", item.name);
                assert!(got.data == item.data, "volume {opened}: {}", item.name);
                assert_eq!(entry.meta.size, Some(item.data.len() as u64));
                // A file that lies in several volumes has no sum in the
                // list: the first volume knows the sum of its part only.
                let whole = item.data.len() < 1024 * 1024;
                let sum = (!item.folder && whole).then(|| crc32fast::hash(&item.data));
                assert_eq!(entry.crc32, sum, "volume {opened}: {}", item.name);
            }

            // What is behind a file that lies in several volumes, without
            // the file: its parts are passed over one by one.
            let sink = Collector::default();
            reader
                .extract(
                    &Selection::paths(["after/tail.txt", "docs/readme.txt"]),
                    &sink,
                    &ctx,
                )
                .unwrap();
            let got = sink.in_order();
            let names: Vec<_> = got.iter().map(|got| got.entry.meta.path.as_str()).collect();
            assert_eq!(
                names,
                ["docs/readme.txt", "after/tail.txt"],
                "volume {opened}"
            );
            assert_eq!(got[1].data, lines(20), "volume {opened}");
            // And a file that starts in the middle of the volumes.
            let sink = Collector::default();
            reader
                .extract(&Selection::paths(["after/last.bin"]), &sink, &ctx)
                .unwrap();
            assert!(
                sink.in_order()[0].data == pattern(3 * 1024 * 1024),
                "volume {opened}"
            );
        }
    }

    #[test]
    fn a_volume_that_is_missing_is_named() {
        let ctx = OpContext::silent();
        let folder = tempfile::tempdir().unwrap();
        let items = [
            file("small.txt", lines(20)),
            file("big.bin", pattern(300_000)),
            file("tail.txt", lines(10)),
        ];
        let volumes = volumes5(&items, 100_000);
        assert_eq!(volumes.len(), 4);
        let paths = plant_volumes(folder.path(), &volumes);

        // The list goes through all the volumes: without the last there is
        // none.
        fs::remove_file(&paths[3]).unwrap();
        let mut reader = on_disk(&paths[0]);
        let error = reader.entries(&ctx).unwrap_err();
        assert_eq!(error.code, "volumeMissing");
        assert_eq!(error.params["name"], "set.part4.rar");
        let error = reader
            .extract(&Selection::All, &Collector::default(), &ctx)
            .unwrap_err();
        assert_eq!(error.code, "volumeMissing");

        // A volume from the middle, without the one the archive starts in.
        fs::write(&paths[3], &volumes[3]).unwrap();
        fs::remove_file(&paths[0]).unwrap();
        let mut reader = on_disk(&paths[2]);
        let error = reader.entries(&ctx).unwrap_err();
        assert_eq!(error.code, "volumeMissing");
        assert_eq!(error.params["name"], "set.part1.rar");

        // The first volume of an archive of the old format, made by RAR.
        let alone = folder.path().join("archive.part1.rar");
        fs::write(&alone, include_bytes!("../fixtures/rar4-volume.rar")).unwrap();
        let error = on_disk(&alone).entries(&ctx).unwrap_err();
        assert_eq!(error.code, "volumeMissing");
        assert_eq!(error.params["name"], "archive.part2.rar");
    }

    #[test]
    fn the_first_volume_is_told_by_the_name_of_a_later_one() {
        let first = |name: &str| {
            first_volume(Path::new("folder").join(name).as_path())
                .map(|path| path.file_name().unwrap().to_str().unwrap().to_owned())
        };
        assert_eq!(first("set.part2.rar").as_deref(), Some("set.part1.rar"));
        assert_eq!(first("set.part010.rar").as_deref(), Some("set.part001.rar"));
        assert_eq!(first("Set.PART3.RAR").as_deref(), Some("Set.PART1.RAR"));
        assert_eq!(first("a.b.part12.rar").as_deref(), Some("a.b.part01.rar"));
        assert_eq!(first("set.r00").as_deref(), Some("set.rar"));
        assert_eq!(first("set.s15").as_deref(), Some("set.rar"));
        assert_eq!(first("SET.R07").as_deref(), Some("SET.RAR"));
        for name in [
            "set.rar",
            "set.part1.rar",
            "set.part001.rar",
            "set.part.rar",
            "set.partx.rar",
            "part2.rar",
            "set.zip",
            "set.r0",
            "set.q00",
            "set",
        ] {
            assert_eq!(first(name), None, "{name}");
        }
    }

    #[test]
    fn volumes_and_what_the_memory_does_not_hold_are_refused() {
        let ctx = OpContext::silent();
        let mut reader = open(include_bytes!("../fixtures/rar4-volume.rar").to_vec(), None);
        assert_eq!(reader.entries(&ctx).unwrap_err().code, "unsupportedArchive");
        assert_eq!(reader.comment(&ctx).unwrap_err().code, "unsupportedArchive");

        // A file that says it is compressed with a dictionary of four
        // gigabytes: the dictionary is not taken to find out.
        let greedy = Item {
            compression: Some(0x80 | (15 << 10)),
            ..file("greedy.bin", pattern(5000))
        };
        let mut reader = open(rar5(false, &[file("a", b"a".to_vec()), greedy]), None);
        let entries = reader.entries(&ctx).unwrap();
        assert_eq!(entries[1].method.as_deref(), Some("RAR 5"));
        let sink = Collector::default();
        let error = reader.extract(&Selection::All, &sink, &ctx).unwrap_err();
        assert_eq!(error.code, "notEnoughMemory");
        assert_eq!(sink.in_order().len(), 1);

        // More names than the memory may hold.
        let tight = OpContext {
            budget: Budget {
                threads: 1,
                memory: 600,
            },
            ..OpContext::silent()
        };
        let mut reader = open(rar5(false, &numbered()), None);
        assert_eq!(reader.entries(&tight).unwrap_err().code, "notEnoughMemory");

        // A method of a version nobody knows.
        let strange = Item {
            compression: Some(0x80 | 9),
            ..file("strange.bin", pattern(5000))
        };
        let mut reader = open(rar5(false, &[strange]), None);
        let error = reader
            .extract(&Selection::All, &Collector::default(), &ctx)
            .unwrap_err();
        assert_eq!(error.code, "unsupportedMethod", "{error:?}");
    }

    proptest! {
        #![proptest_config(ProptestConfig::with_cases(96))]

        /// Whatever is done to an archive, the library and the reader end
        /// with an answer.
        #[test]
        fn a_spoiled_archive_is_read_or_refused(
            which in 0usize..6,
            changes in prop::collection::vec((0usize..10_000, any::<u8>()), 1..6),
            cut in prop::option::of(0usize..10_000),
        ) {
            let small = [
                folder("sub"),
                file("sub/a.txt", lines(40)),
                redirect("link", REDIRECT_UNIX_LINK, "sub/a.txt"),
                file("b.bin", pattern(3000)),
            ];
            let mut bytes = match which {
                0 => rar5(false, &small),
                1 => rar5(true, &small),
                2 => rar4(&small),
                3 => include_bytes!("../fixtures/rar4-stored.rar").to_vec(),
                4 => include_bytes!("../fixtures/rar5-solid.rar").to_vec(),
                _ => include_bytes!("../fixtures/rar5-secret.rar").to_vec(),
            };
            for (at, value) in changes {
                let at = at % bytes.len();
                bytes[at] ^= value;
            }
            if let Some(cut) = cut {
                bytes.truncate(cut % bytes.len() + 1);
            }
            let ctx = OpContext {
                budget: Budget {
                    threads: 1,
                    memory: 64 * 1024 * 1024,
                },
                ..OpContext::silent()
            };
            let mut reader = open(bytes, Some("password"));
            let _ = reader.comment(&ctx);
            if reader.entries(&ctx).is_ok() {
                let _ = reader.extract(&Selection::All, &Collector::default(), &ctx);
            }
        }
    }
}

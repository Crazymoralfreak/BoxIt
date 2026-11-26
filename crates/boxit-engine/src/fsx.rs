//! The file system as the operations use it.
//!
//! Everything that touches the disk on behalf of an archive goes through
//! here, for two reasons. Windows limits ordinary paths to 260 characters
//! and archives routinely hold deeper trees, so every path is turned into
//! its long form (`\\?\C:\...`) once, at the border ([`long`]). And times,
//! attributes and links differ between systems, which the rest of the engine
//! should not have to know.

use std::collections::hash_map::RandomState;
use std::fs::{self, File, FileTimes, OpenOptions};
use std::hash::{BuildHasher, Hasher};
use std::io;
#[cfg(unix)]
use std::os::unix::fs::{MetadataExt, OpenOptionsExt, PermissionsExt};
#[cfg(windows)]
use std::os::windows::fs::{FileTimesExt, MetadataExt, OpenOptionsExt};
use std::path::{self, Path, PathBuf};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use boxit_core::{EntryMeta, Result, Timestamp};

/// `FILE_ATTRIBUTE_*` bits of Windows.
pub const ATTRIBUTE_READONLY: u32 = 0x1;
const ATTRIBUTE_HIDDEN: u32 = 0x2;
const ATTRIBUTE_SYSTEM: u32 = 0x4;
const ATTRIBUTE_ARCHIVE: u32 = 0x20;
const ATTRIBUTE_NOT_INDEXED: u32 = 0x2000;
/// Attributes of an archive entry that are given to the extracted file. The
/// rest describe how a file is stored (compressed, sparse, reparse point)
/// and are not for an archive to decide.
const ATTRIBUTES_KEPT: u32 =
    ATTRIBUTE_HIDDEN | ATTRIBUTE_SYSTEM | ATTRIBUTE_ARCHIVE | ATTRIBUTE_NOT_INDEXED;

#[cfg(windows)]
const VERBATIM: &str = r"\\?\";
#[cfg(windows)]
const VERBATIM_UNC: &str = r"\\?\UNC\";

/// The absolute form of a path that the system accepts at any length. On
/// Windows that is the `\\?\` form, which also switches off the rewriting of
/// names (trailing dots, device names), so only names that passed
/// `boxit-security` may be joined to it. Elsewhere the path is only made
/// absolute.
pub fn long(path: &Path) -> Result<PathBuf> {
    let absolute = path::absolute(path)?;
    #[cfg(windows)]
    {
        let text = absolute.to_string_lossy();
        if text.starts_with(VERBATIM) {
            return Ok(absolute);
        }
        if let Some(share) = text.strip_prefix(r"\\") {
            return Ok(PathBuf::from(format!("{VERBATIM_UNC}{share}")));
        }
        Ok(PathBuf::from(format!("{VERBATIM}{text}")))
    }
    #[cfg(not(windows))]
    {
        Ok(absolute)
    }
}

/// A path as it is shown to the user: without the long-path prefix.
pub fn display(path: &Path) -> String {
    let text = path.to_string_lossy();
    #[cfg(windows)]
    {
        if let Some(share) = text.strip_prefix(VERBATIM_UNC) {
            return format!(r"\\{share}");
        }
        if let Some(plain) = text.strip_prefix(VERBATIM) {
            return plain.to_owned();
        }
    }
    text.into_owned()
}

/// Free space of the disk that holds `path`, in bytes, when the system tells.
pub fn free_space(path: &Path) -> Option<u64> {
    fs4::available_space(path).ok()
}

/// A short random name part, for temporary files and folders.
pub fn unique_suffix() -> String {
    // Every `RandomState` is keyed with fresh random numbers of the system.
    let mut hasher = RandomState::new().build_hasher();
    hasher.write_u32(std::process::id());
    format!("{:08x}", hasher.finish() as u32)
}

pub fn system_time(time: Timestamp) -> Option<SystemTime> {
    let nanos = Duration::from_nanos(u64::from(time.nanos.min(999_999_999)));
    if time.seconds >= 0 {
        UNIX_EPOCH
            .checked_add(Duration::from_secs(time.seconds as u64))?
            .checked_add(nanos)
    } else {
        UNIX_EPOCH
            .checked_sub(Duration::from_secs(time.seconds.unsigned_abs()))?
            .checked_add(nanos)
    }
}

pub fn timestamp(time: SystemTime) -> Timestamp {
    match time.duration_since(UNIX_EPOCH) {
        Ok(since) => Timestamp {
            seconds: since.as_secs() as i64,
            nanos: since.subsec_nanos(),
        },
        Err(before) => {
            // Whole seconds round towards the past, nanoseconds stay positive.
            let before = before.duration();
            let mut seconds = -(before.as_secs() as i64);
            let mut nanos = 0;
            if before.subsec_nanos() > 0 {
                seconds -= 1;
                nanos = 1_000_000_000 - before.subsec_nanos();
            }
            Timestamp { seconds, nanos }
        }
    }
}

fn file_times(meta: &EntryMeta) -> Option<FileTimes> {
    let modified = meta.modified.and_then(system_time)?;
    let mut times = FileTimes::new().set_modified(modified);
    if let Some(accessed) = meta.accessed.and_then(system_time) {
        times = times.set_accessed(accessed);
    }
    #[cfg(windows)]
    if let Some(created) = meta.created.and_then(system_time) {
        times = times.set_created(created);
    }
    Some(times)
}

/// Gives an open file the times of its entry. An entry without a time keeps
/// the moment of extraction.
pub fn set_file_times(file: &File, meta: &EntryMeta) -> io::Result<()> {
    match file_times(meta) {
        Some(times) => file.set_times(times),
        None => Ok(()),
    }
}

/// Gives a folder the times of its entry.
pub fn set_folder_times(folder: &Path, meta: &EntryMeta) -> io::Result<()> {
    let Some(times) = file_times(meta) else {
        return Ok(());
    };
    #[cfg(windows)]
    let handle = {
        // Without this flag Windows refuses to open a folder.
        const FILE_FLAG_BACKUP_SEMANTICS: u32 = 0x0200_0000;
        OpenOptions::new()
            .write(true)
            .custom_flags(FILE_FLAG_BACKUP_SEMANTICS)
            .open(folder)?
    };
    #[cfg(not(windows))]
    let handle = File::open(folder)?;
    handle.set_times(times)
}

/// Creates the file an entry is extracted into; it must not exist. On
/// Windows the file gets the hidden, system and archive attributes of the
/// entry at once.
pub fn create_file(path: &Path, meta: &EntryMeta) -> io::Result<File> {
    let mut options = OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(windows)]
    {
        let attributes = meta.windows_attributes.unwrap_or(0) & ATTRIBUTES_KEPT;
        if attributes != 0 {
            options.attributes(attributes);
        }
    }
    // Created private; the mode of the entry is applied once the data is
    // written.
    #[cfg(unix)]
    options.mode(0o600);
    let _ = meta;
    options.open(path)
}

/// Opens a file that is going to be read from its start to its end, and
/// tells the system so: it reads ahead further. Other programs may go on
/// using the file meanwhile.
pub fn open_sequential(path: &Path) -> io::Result<File> {
    let mut options = OpenOptions::new();
    options.read(true);
    #[cfg(windows)]
    {
        const FILE_FLAG_SEQUENTIAL_SCAN: u32 = 0x0800_0000;
        options.custom_flags(FILE_FLAG_SEQUENTIAL_SCAN);
    }
    options.open(path)
}

/// What the disk knows about a file, a folder or a link, as an entry of an
/// archive named `name`. A link is described itself, not what it points to.
pub fn describe(path: &Path, name: String, metadata: &fs::Metadata) -> io::Result<EntryMeta> {
    let kind = metadata.file_type();
    let mut meta = if kind.is_symlink() {
        let target = fs::read_link(path)?;
        // Inside an archive the parts of a path are joined with a slash.
        EntryMeta::symlink(name, target.to_string_lossy().replace('\\', "/"))
    } else if kind.is_dir() {
        EntryMeta::directory(name)
    } else {
        EntryMeta::file(name, metadata.len())
    };
    meta.modified = metadata.modified().ok().map(timestamp);
    meta.created = metadata.created().ok().map(timestamp);
    meta.accessed = metadata.accessed().ok().map(timestamp);
    #[cfg(windows)]
    {
        meta.windows_attributes = Some(metadata.file_attributes());
    }
    #[cfg(unix)]
    {
        meta.unix_mode = Some(metadata.mode() & 0o7777);
    }
    Ok(meta)
}

/// Whether two paths in their long form name the same place. Windows does
/// not tell `A.zip` from `a.zip`.
pub fn same_path(one: &Path, other: &Path) -> bool {
    if cfg!(windows) {
        one.to_string_lossy().to_lowercase() == other.to_string_lossy().to_lowercase()
    } else {
        one == other
    }
}

/// Whether the entry asks for a file nobody may write to.
fn is_readonly(meta: &EntryMeta) -> bool {
    match (meta.windows_attributes, meta.unix_mode) {
        (Some(attributes), _) => attributes & ATTRIBUTE_READONLY != 0,
        (None, Some(mode)) => mode & 0o222 == 0,
        (None, None) => false,
    }
}

/// Gives an extracted file the permissions of its entry: the mode bits on
/// Unix (never set-user-id or set-group-id), the read-only flag on Windows.
/// Called last, after the data and the times.
pub fn set_permissions(path: &Path, meta: &EntryMeta) -> io::Result<()> {
    #[cfg(unix)]
    if let Some(mode) = meta.unix_mode {
        // An archive does not get to make programs that run as someone
        // else, and the owner can always read what was extracted.
        let mode = (mode & 0o777) | 0o400;
        return fs::set_permissions(path, fs::Permissions::from_mode(mode));
    }
    if is_readonly(meta) {
        let mut permissions = fs::metadata(path)?.permissions();
        permissions.set_readonly(true);
        fs::set_permissions(path, permissions)?;
    }
    Ok(())
}

/// Creates a symbolic link at `link`. On Windows this needs the developer
/// mode or administrator rights; without them it fails with "access denied".
pub fn symlink(target: &str, link: &Path, to_folder: bool) -> io::Result<()> {
    #[cfg(windows)]
    {
        let target = target.replace('/', "\\");
        if to_folder {
            std::os::windows::fs::symlink_dir(target, link)
        } else {
            std::os::windows::fs::symlink_file(target, link)
        }
    }
    #[cfg(unix)]
    {
        let _ = to_folder;
        std::os::unix::fs::symlink(target, link)
    }
}

/// What is at a path, without following a link there.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Existing {
    Nothing,
    File,
    Folder,
    Link,
}

pub fn existing(path: &Path) -> Existing {
    match fs::symlink_metadata(path) {
        Err(_) => Existing::Nothing,
        Ok(metadata) if metadata.file_type().is_symlink() => Existing::Link,
        Ok(metadata) if metadata.is_dir() => Existing::Folder,
        Ok(_) => Existing::File,
    }
}

/// Removes whatever is at the path: a file, a link or a folder with all its
/// content. A link is removed itself, never what it points to.
pub fn remove_any(path: &Path) -> io::Result<()> {
    let Ok(metadata) = fs::symlink_metadata(path) else {
        return Ok(());
    };
    if metadata.file_type().is_symlink() {
        // On Windows a link to a folder is removed as a folder.
        return fs::remove_file(path).or_else(|_| fs::remove_dir(path));
    }
    if metadata.is_dir() {
        return fs::remove_dir_all(path);
    }
    if metadata.permissions().readonly() {
        // Windows does not delete read-only files.
        let mut permissions = metadata.permissions();
        #[allow(clippy::permissions_set_readonly_false)]
        permissions.set_readonly(false);
        let _ = fs::set_permissions(path, permissions);
    }
    fs::remove_file(path)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn times_survive_the_trip_to_the_system_and_back() {
        for time in [
            Timestamp {
                seconds: 0,
                nanos: 0,
            },
            Timestamp {
                seconds: 1_700_000_000,
                nanos: 123_456_700,
            },
            Timestamp {
                seconds: -1,
                nanos: 500_000_000,
            },
            Timestamp {
                seconds: -86_400,
                nanos: 0,
            },
        ] {
            assert_eq!(timestamp(system_time(time).unwrap()), time, "{time:?}");
        }
        assert_eq!(
            system_time(Timestamp {
                seconds: i64::MAX,
                nanos: 0
            }),
            None
        );
    }

    #[test]
    fn a_long_path_is_absolute_and_shown_without_its_prefix() {
        let folder = tempfile::tempdir().unwrap();
        let long_form = long(folder.path()).unwrap();
        assert!(long_form.is_absolute());
        assert!(long_form.is_dir());
        if cfg!(windows) {
            assert!(long_form.to_string_lossy().starts_with(r"\\?\"));
            assert_eq!(long(&long_form).unwrap(), long_form);
        }
        assert!(!display(&long_form).starts_with(r"\\?\"));
        assert_eq!(
            display(&long_form),
            path::absolute(folder.path()).unwrap().to_string_lossy()
        );
    }

    #[test]
    fn files_can_be_made_far_deeper_than_260_characters() {
        let folder = tempfile::tempdir().unwrap();
        let mut deep = long(folder.path()).unwrap();
        for _ in 0..12 {
            deep.push("a-folder-with-a-name-of-forty-characters");
        }
        assert!(deep.to_string_lossy().len() > 480);
        fs::create_dir_all(&deep).unwrap();
        let file = deep.join("file.txt");
        fs::write(&file, b"deep").unwrap();
        assert_eq!(fs::read(&file).unwrap(), b"deep");
        assert_eq!(existing(&file), Existing::File);
        remove_any(
            &long(folder.path())
                .unwrap()
                .join("a-folder-with-a-name-of-forty-characters"),
        )
        .unwrap();
        assert_eq!(existing(&file), Existing::Nothing);
    }

    #[test]
    fn free_space_of_an_existing_folder_is_known() {
        let folder = tempfile::tempdir().unwrap();
        assert!(free_space(folder.path()).is_some_and(|free| free > 0));
    }

    #[test]
    fn a_file_gets_the_times_and_the_read_only_flag_of_its_entry() {
        let folder = tempfile::tempdir().unwrap();
        let path = folder.path().join("file.txt");
        let meta = EntryMeta {
            modified: Some(Timestamp::from_seconds(1_500_000_000)),
            windows_attributes: Some(ATTRIBUTE_READONLY),
            ..EntryMeta::file("file.txt", 0)
        };
        let file = create_file(&path, &meta).unwrap();
        set_file_times(&file, &meta).unwrap();
        drop(file);
        set_permissions(&path, &meta).unwrap();
        let metadata = fs::metadata(&path).unwrap();
        assert_eq!(
            timestamp(metadata.modified().unwrap()).seconds,
            1_500_000_000
        );
        assert!(metadata.permissions().readonly());
        // Creating it again must fail rather than overwrite.
        assert!(create_file(&path, &meta).is_err());
        remove_any(&path).unwrap();
        assert_eq!(existing(&path), Existing::Nothing);
    }

    #[test]
    fn a_folder_gets_the_time_of_its_entry() {
        let folder = tempfile::tempdir().unwrap();
        let path = folder.path().join("sub");
        fs::create_dir(&path).unwrap();
        let meta = EntryMeta {
            modified: Some(Timestamp::from_seconds(1_400_000_000)),
            ..EntryMeta::directory("sub")
        };
        set_folder_times(&path, &meta).unwrap();
        let modified = fs::metadata(&path).unwrap().modified().unwrap();
        assert_eq!(timestamp(modified).seconds, 1_400_000_000);
    }

    #[test]
    fn suffixes_differ() {
        assert_ne!(unique_suffix(), unique_suffix());
        assert_eq!(unique_suffix().len(), 8);
    }
}

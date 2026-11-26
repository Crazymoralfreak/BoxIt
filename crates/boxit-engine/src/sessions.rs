//! The archives a window has open.
//!
//! An open archive is its tree and what is needed to read the file again:
//! the file itself is not held, so it can be moved or replaced while its
//! list is on the screen. Every operation opens the file anew and first
//! makes sure it is still the file the tree was built from.
//!
//! The trees of the archives opened lately are kept, by the path, the size
//! and the time of change of the file: going back to an archive, or opening
//! a compressed tar a second time, does not read it again.

use std::collections::{HashMap, VecDeque};
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
use std::time::SystemTime;

use boxit_codecs::names::NameEncoding;
use boxit_codecs::volumes;
use boxit_core::{Error, OpContext, Password, Result};

use crate::archive::{open_options, Archive};
use crate::browse::Index;
use crate::fsx;

/// How many trees are kept after their archives are closed.
const TREES_KEPT: usize = 4;

/// Entries of all the kept trees together: a tree of a million entries
/// takes hundreds of megabytes, and is not kept behind the back of the user.
const ENTRIES_KEPT: usize = 400_000;

fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(PoisonError::into_inner)
}

/// What tells that a file is the one that was looked at before.
#[derive(Clone, Debug, PartialEq, Eq)]
struct Stamp {
    size: u64,
    modified: Option<SystemTime>,
}

impl Stamp {
    /// The stamp of an archive: of all its volumes when it is cut into
    /// them, so that a change of any of them shows.
    fn of(path: &Path) -> Result<Self> {
        let files = match volumes::of(path) {
            Ok(Some((_, files))) => files,
            // What is wrong with the volumes is said when they are opened.
            Ok(None) | Err(_) => vec![path.to_path_buf()],
        };
        let mut stamp = Self {
            size: 0,
            modified: None,
        };
        for file in files {
            let metadata = fs::metadata(&file)
                .map_err(|error| Error::from(error).with("path", fsx::display(&file)))?;
            stamp.size += metadata.len();
            stamp.modified = stamp.modified.max(metadata.modified().ok());
        }
        Ok(stamp)
    }
}

#[derive(Clone, PartialEq, Eq)]
struct Key {
    path: PathBuf,
    stamp: Stamp,
    encoding: NameEncoding,
}

/// An archive open in a window.
pub struct Session {
    pub id: u64,
    path: PathBuf,
    stamp: Stamp,
    encoding: NameEncoding,
    /// The password that opened the archive, or the last one that worked.
    password: Mutex<Option<Password>>,
    pub index: Arc<Index>,
}

impl Session {
    /// The archive file, in the long form of its path.
    pub fn path(&self) -> &Path {
        &self.path
    }

    /// How the names of the archive are read.
    pub fn encoding(&self) -> NameEncoding {
        self.encoding
    }

    pub fn password(&self) -> Option<Password> {
        lock(&self.password).clone()
    }

    /// Keeps a password that worked for the next operations on the archive.
    /// It lives in memory until the archive is closed.
    pub fn remember(&self, password: Password) {
        *lock(&self.password) = Some(password);
    }

    /// Opens the archive file for an operation. Fails with `archiveChanged`
    /// when the file is no longer the one the tree was built from: its rows
    /// would stand for other entries.
    pub fn archive(&self) -> Result<Archive> {
        if Stamp::of(&self.path)? != self.stamp {
            return Err(Error::new("archiveChanged").with("path", fsx::display(&self.path)));
        }
        Archive::open(&self.path, open_options(self.password(), self.encoding))
    }
}

struct Kept {
    key: Key,
    index: Arc<Index>,
}

/// The open archives of one copy of BoxIt.
pub struct Sessions {
    next: AtomicU64,
    open: Mutex<HashMap<u64, Arc<Session>>>,
    /// Trees of archives opened lately, the latest last.
    kept: Mutex<VecDeque<Kept>>,
}

impl Default for Sessions {
    fn default() -> Self {
        Self::new()
    }
}

impl Sessions {
    pub fn new() -> Self {
        Self {
            next: AtomicU64::new(1),
            open: Mutex::new(HashMap::new()),
            kept: Mutex::new(VecDeque::new()),
        }
    }

    fn kept(&self, key: &Key) -> Option<Arc<Index>> {
        let mut kept = lock(&self.kept);
        let place = kept.iter().position(|kept| kept.key == *key)?;
        // What was asked for last is dropped last.
        let found = kept.remove(place)?;
        let index = Arc::clone(&found.index);
        kept.push_back(found);
        Some(index)
    }

    fn keep(&self, key: Key, index: &Arc<Index>) {
        if index.count() > ENTRIES_KEPT {
            return;
        }
        let mut kept = lock(&self.kept);
        kept.retain(|kept| kept.key.path != key.path);
        kept.push_back(Kept {
            key,
            index: Arc::clone(index),
        });
        while kept.len() > TREES_KEPT
            || kept.iter().map(|kept| kept.index.count()).sum::<usize>() > ENTRIES_KEPT
        {
            kept.pop_front();
        }
    }

    /// Opens an archive and reads its list. An archive without an index is
    /// read from its start to its end for that; `ctx` watches and cancels.
    pub fn open(
        &self,
        path: &Path,
        password: Option<Password>,
        encoding: NameEncoding,
        ctx: &OpContext,
    ) -> Result<Arc<Session>> {
        let path = fsx::long(path)?;
        let stamp = Stamp::of(&path)?;
        let key = Key {
            path: path.clone(),
            stamp: stamp.clone(),
            encoding,
        };
        // A list that took a password to read is read with one every time.
        let secret = password.is_some();
        let index = if let Some(index) = self.kept(&key).filter(|_| !secret) {
            index
        } else {
            let mut archive = Archive::open(&path, open_options(password.clone(), encoding))?;
            let index = Arc::new(Index::build(archive.list(ctx)?));
            // The file may have been written while it was read.
            if Stamp::of(&path)? != stamp {
                return Err(Error::new("archiveChanged").with("path", fsx::display(&path)));
            }
            if !secret {
                self.keep(key, &index);
            }
            index
        };
        let session = Arc::new(Session {
            id: self.next.fetch_add(1, Ordering::Relaxed),
            path,
            stamp,
            encoding,
            password: Mutex::new(password),
            index,
        });
        lock(&self.open).insert(session.id, Arc::clone(&session));
        Ok(session)
    }

    /// An open archive by its number. Fails with `archiveClosed` for one
    /// that is not open any more.
    pub fn get(&self, id: u64) -> Result<Arc<Session>> {
        lock(&self.open)
            .get(&id)
            .cloned()
            .ok_or_else(|| Error::new("archiveClosed"))
    }

    /// Closes an archive. Operations that have started on it go on: they
    /// hold what they need.
    pub fn close(&self, id: u64) {
        lock(&self.open).remove(&id);
    }
}

#[cfg(test)]
mod tests {
    use std::thread;
    use std::time::Duration;

    use boxit_core::{ArchiveFormat, CreateOptions, ExtractOptions, Level, Selection};
    use tempfile::TempDir;

    use super::*;
    use crate::archive::TestOptions;
    use crate::browse::{Sort, ROOT};
    use crate::create::{create, CreateRequest, Layout};
    use crate::testing::{plant, tree, Watch};

    fn packed(
        root: &Path,
        name: &str,
        options: CreateOptions,
        content: &[(&str, &str)],
    ) -> PathBuf {
        let folder = root.join(format!("{name}.src"));
        plant(&folder, content);
        let sources = fs::read_dir(&folder)
            .unwrap()
            .map(|entry| entry.unwrap().path())
            .collect();
        let destination = root.join(name);
        let request = CreateRequest {
            sources,
            destination: destination.clone(),
            options: CreateOptions {
                level: Level::Fast,
                ..options
            },
            overwrite: true,
            layout: Layout::Whole,
        };
        create(&request, &OpContext::silent()).unwrap();
        destination
    }

    fn open(sessions: &Sessions, path: &Path) -> Arc<Session> {
        sessions
            .open(path, None, NameEncoding::Auto, &OpContext::silent())
            .unwrap()
    }

    #[test]
    fn an_open_archive_is_browsed_and_read_again_for_an_operation() {
        let root = TempDir::new().unwrap();
        let path = packed(
            root.path(),
            "a.zip",
            CreateOptions::new(ArchiveFormat::Zip),
            &[
                ("docs/a.txt", "alpha"),
                ("docs/deep/b.txt", "beta"),
                ("c.txt", "gamma"),
            ],
        );
        let sessions = Sessions::new();
        let session = open(&sessions, &path);
        let index = &session.index;
        assert_eq!(index.info().files, 3);
        let top = index.order(ROOT, Sort::default());
        assert_eq!(index.name(top[0]), Some("docs"));

        // What is taken from a folder lands without the folders above it.
        let docs = top[0];
        let deep = index.order(docs, Sort::default())[0];
        let picked = index.pick(docs, &[deep]);
        let destination = root.path().join("out");
        session
            .archive()
            .unwrap()
            .extract_from(
                &picked.base,
                &destination,
                picked.selection,
                ExtractOptions::default(),
                &OpContext::silent(),
            )
            .unwrap();
        assert_eq!(
            tree(&destination).into_keys().collect::<Vec<_>>(),
            ["deep/", "deep/b.txt"]
        );

        assert_eq!(sessions.get(session.id).unwrap().id, session.id);
        sessions.close(session.id);
        assert_eq!(
            sessions.get(session.id).err().unwrap().code,
            "archiveClosed"
        );
        // The one who holds the archive can still work with it.
        assert!(session.archive().is_ok());
    }

    #[test]
    fn the_tree_of_an_archive_is_built_once_while_the_file_stays_the_same() {
        let root = TempDir::new().unwrap();
        let content = [("a.txt", "alpha")];
        let options = || CreateOptions::new(ArchiveFormat::TarGz);
        let path = packed(root.path(), "a.tar.gz", options(), &content);
        let sessions = Sessions::new();
        let first = open(&sessions, &path);
        sessions.close(first.id);

        // The second time nothing of the archive is read.
        let watch = Arc::new(Watch::default());
        let ctx = OpContext {
            progress: watch.clone(),
            ..OpContext::silent()
        };
        let second = sessions
            .open(&path, None, NameEncoding::Auto, &ctx)
            .unwrap();
        assert!(Arc::ptr_eq(&first.index, &second.index));
        assert_ne!(first.id, second.id);
        assert_eq!(watch.entries.load(Ordering::SeqCst), 0);

        // Another way to read the names is another tree.
        let other = sessions
            .open(
                &path,
                None,
                NameEncoding::from_label("ibm866").unwrap(),
                &OpContext::silent(),
            )
            .unwrap();
        assert!(!Arc::ptr_eq(&first.index, &other.index));

        // The file is written anew: the tree is built again, and the
        // archive that was open before no longer stands for the file.
        thread::sleep(Duration::from_millis(20));
        let more = [
            ("a.txt", "alpha"),
            ("b.txt", "beta, which makes the file longer"),
        ];
        packed(root.path(), "a.tar.gz", options(), &more);
        let third = open(&sessions, &path);
        assert!(!Arc::ptr_eq(&first.index, &third.index));
        assert_eq!(third.index.info().files, 2);
        assert_eq!(first.archive().err().unwrap().code, "archiveChanged");
        assert_eq!(
            first
                .archive()
                .err()
                .unwrap()
                .params
                .get("path")
                .map(String::as_str),
            Some(fsx::display(&fsx::long(&path).unwrap()).as_str())
        );
    }

    #[test]
    fn a_password_that_worked_is_kept_for_the_archive() {
        let root = TempDir::new().unwrap();
        let locked = CreateOptions {
            password: Some(Password::from("open sesame")),
            encrypt_names: true,
            ..CreateOptions::new(ArchiveFormat::SevenZ)
        };
        let path = packed(root.path(), "locked.7z", locked, &[("a.txt", "alpha")]);
        let sessions = Sessions::new();
        let silent = OpContext::silent();
        let error = sessions
            .open(&path, None, NameEncoding::Auto, &silent)
            .err()
            .unwrap();
        assert_eq!(error.code, "passwordRequired");

        let password = Some(Password::from("open sesame"));
        let session = sessions
            .open(&path, password.clone(), NameEncoding::Auto, &silent)
            .unwrap();
        assert_eq!(session.password(), password);
        let tested = session
            .archive()
            .unwrap()
            .test(&Selection::All, TestOptions::default(), &silent)
            .unwrap();
        assert_eq!(tested.files, 1);

        // The names are a secret: the list is not handed out again to
        // whoever comes without the password.
        sessions.close(session.id);
        assert!(sessions
            .open(&path, None, NameEncoding::Auto, &silent)
            .is_err());
    }
}

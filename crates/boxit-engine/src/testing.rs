//! What the tests of the operations share: an archive held in memory, which
//! behaves like a format without being one and can be told to fail at a
//! chosen entry, and a look at a folder of the disk.

use std::collections::BTreeMap;
use std::fmt::Write;
use std::fs;
use std::io::Cursor;
use std::path::Path;
use std::sync::atomic::{AtomicU64, Ordering};
use std::thread;

use boxit_core::{
    ArchiveReader, Capabilities, Control, Entry, EntryMeta, EntrySink, Error, OpContext, Progress,
    Result, Selection,
};

fn collect(root: &Path, folder: &Path, out: &mut BTreeMap<String, String>) {
    for entry in fs::read_dir(folder).unwrap() {
        let path = entry.unwrap().path();
        let name = path
            .strip_prefix(root)
            .unwrap()
            .to_string_lossy()
            .replace('\\', "/");
        if path.is_dir() {
            out.insert(format!("{name}/"), String::new());
            collect(root, &path, out);
        } else {
            out.insert(
                name,
                String::from_utf8_lossy(&fs::read(&path).unwrap()).into_owned(),
            );
        }
    }
}

/// Every file under a folder with its content, by relative path with `/`.
pub fn tree(folder: &Path) -> BTreeMap<String, String> {
    let mut out = BTreeMap::new();
    collect(folder, folder, &mut out);
    out
}

/// Writes files and folders under `root`; a name that ends with a slash is
/// a folder.
pub fn plant(root: &Path, content: &[(&str, &str)]) {
    for (name, text) in content {
        let path = root.join(name);
        if name.ends_with('/') {
            fs::create_dir_all(&path).unwrap();
        } else {
            fs::create_dir_all(path.parent().unwrap()).unwrap();
            fs::write(&path, text).unwrap();
        }
    }
}

/// A text of numbered lines: `0`, `1`, ... in hexadecimal, each followed by
/// `suffix`.
pub fn lines(count: usize, suffix: &str) -> String {
    let mut text = String::new();
    for line in 0..count {
        writeln!(text, "{line:x}{suffix}").unwrap();
    }
    text
}

/// Counts what an operation reports, and cancels it once it has processed
/// a given number of bytes.
#[derive(Default)]
pub struct Watch {
    pub total: AtomicU64,
    pub advanced: AtomicU64,
    pub entries: AtomicU64,
    cancel_at: Option<(u64, Control)>,
}

impl Watch {
    pub fn cancelling(control: &Control, after: u64) -> Self {
        Self {
            cancel_at: Some((after, control.clone())),
            ..Self::default()
        }
    }
}

impl Progress for Watch {
    fn total(&self, bytes: Option<u64>, _entries: Option<u64>) {
        self.total.store(bytes.unwrap_or(0), Ordering::SeqCst);
    }

    fn entry(&self, _path: &str) {
        self.entries.fetch_add(1, Ordering::SeqCst);
    }

    fn advance(&self, bytes: u64) {
        let advanced = self.advanced.fetch_add(bytes, Ordering::SeqCst) + bytes;
        if let Some((after, control)) = &self.cancel_at {
            if advanced >= *after {
                control.cancel();
            }
        }
    }
}

/// What the archive does when it reaches a certain entry.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Trouble {
    /// Fail the way a damaged archive does.
    Fail,
    Panic,
    /// The user cancels at this moment.
    Cancel,
}

#[derive(Default)]
pub struct MemoryArchive {
    pub items: Vec<(EntryMeta, Vec<u8>)>,
    /// Hand the entries to the sink from several threads.
    pub parallel: bool,
    /// Report the list of entries as available without reading the data.
    pub has_index: bool,
    /// Trouble on reaching the entry with this index.
    pub trouble: Option<(usize, Trouble)>,
}

impl MemoryArchive {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn file(mut self, path: &str, data: &[u8]) -> Self {
        self.items
            .push((EntryMeta::file(path, data.len() as u64), data.to_vec()));
        self
    }

    pub fn folder(mut self, path: &str) -> Self {
        self.items.push((EntryMeta::directory(path), Vec::new()));
        self
    }

    pub fn symlink(mut self, path: &str, target: &str) -> Self {
        self.items
            .push((EntryMeta::symlink(path, target), Vec::new()));
        self
    }

    pub fn entry(mut self, meta: EntryMeta, data: &[u8]) -> Self {
        self.items.push((meta, data.to_vec()));
        self
    }

    pub fn trouble_at(mut self, index: usize, trouble: Trouble) -> Self {
        self.trouble = Some((index, trouble));
        self
    }

    fn entry_at(&self, index: usize) -> Entry {
        Entry {
            index: index as u64,
            meta: self.items[index].0.clone(),
            ..Entry::default()
        }
    }

    fn hand_over(
        &self,
        index: usize,
        pick: &Selection,
        sink: &dyn EntrySink,
        ctx: &OpContext,
    ) -> Result<()> {
        if let Some((at, trouble)) = self.trouble {
            if at == index {
                match trouble {
                    Trouble::Fail => return Err(Error::new("damaged")),
                    Trouble::Panic => panic!("the format library gave up"),
                    Trouble::Cancel => ctx.control.cancel(),
                }
            }
        }
        let entry = self.entry_at(index);
        if !pick.contains(&entry) {
            return Ok(());
        }
        sink.accept(&entry, &mut Cursor::new(&self.items[index].1))
    }
}

impl ArchiveReader for MemoryArchive {
    fn capabilities(&self) -> Capabilities {
        Capabilities {
            random_access: true,
            solid: false,
            parallel: self.parallel,
            has_index: self.has_index,
        }
    }

    fn entries(&mut self, _ctx: &OpContext) -> Result<Vec<Entry>> {
        Ok((0..self.items.len())
            .map(|index| self.entry_at(index))
            .collect())
    }

    fn extract(&mut self, pick: &Selection, sink: &dyn EntrySink, ctx: &OpContext) -> Result<()> {
        let archive = &*self;
        if !archive.parallel {
            return (0..archive.items.len())
                .try_for_each(|index| archive.hand_over(index, pick, sink, ctx));
        }
        let indexes: Vec<usize> = (0..archive.items.len()).collect();
        let size = indexes.len().div_ceil(4).max(1);
        thread::scope(|scope| {
            let workers: Vec<_> = indexes
                .chunks(size)
                .map(|chunk| {
                    scope.spawn(move || {
                        chunk
                            .iter()
                            .try_for_each(|index| archive.hand_over(*index, pick, sink, ctx))
                    })
                })
                .collect();
            workers
                .into_iter()
                .map(|worker| worker.join().expect("a worker panicked"))
                .collect::<Result<Vec<()>>>()
                .map(drop)
        })
    }
}

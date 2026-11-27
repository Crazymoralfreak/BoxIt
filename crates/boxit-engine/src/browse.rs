//! The tree of an archive, the way a file manager shows it.
//!
//! An archive is a flat list of paths. The window shows folders, and asks
//! for them a screen at a time: the tree is built here once, the window
//! gets the order of a folder as a block of numbers and the rows it shows
//! by those numbers. A million entries never travel to the interface.
//!
//! The parts of a path are told apart exactly as `boxit-security` does it,
//! so a folder of the tree is the folder its content is extracted to.

use std::cmp::Ordering;
use std::collections::hash_map::DefaultHasher;
use std::collections::HashMap;
use std::hash::{Hash, Hasher};
use std::iter::Peekable;
use std::str::Chars;

use boxit_core::{ArchiveFormat, Entry, EntryKind, Selection};
use boxit_security::parts_of;
use serde::{Deserialize, Serialize};

use crate::archive::Listing;

/// A place in the tree. The numbers of a tree stay the same for as long as
/// the tree lives.
pub type NodeId = u32;

/// The archive itself: the folder that holds everything.
pub const ROOT: NodeId = 0;

struct Node {
    name: Box<str>,
    parent: NodeId,
    kind: EntryKind,
    /// The entry of the archive this node shows, by its place in the list.
    /// A folder the archive does not mention, and only implies by the paths
    /// of its files, has none.
    entry: Option<u32>,
    children: Vec<NodeId>,
    /// Bytes of content: of a file, or of everything inside a folder.
    bytes: u64,
    /// Bytes in the archive, counted the same way.
    packed: u64,
    /// Files and folders inside a folder, at any depth.
    files: u64,
    folders: u64,
}

/// What a column of the list is ordered by.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum SortKey {
    #[default]
    Name,
    Size,
    Packed,
    Modified,
    /// The extension of the name.
    Kind,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct Sort {
    pub key: SortKey,
    pub descending: bool,
}

/// A line of the list.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Row {
    pub id: NodeId,
    pub name: String,
    pub kind: EntryKind,
    /// Size of a file; of a folder, the size of everything inside it.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub size: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub packed: Option<u64>,
    /// Seconds since 1970.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub modified: Option<i64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub method: Option<String>,
    pub encrypted: bool,
    /// Where a link points.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub target: Option<String>,
    /// Files and folders right inside a folder.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub items: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub crc32: Option<u32>,
}

/// What is known about an archive as a whole.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Info {
    /// The archive file.
    pub path: String,
    pub format: ArchiveFormat,
    /// Size of the archive: of all its volumes when it is cut into them.
    pub size: u64,
    /// The number of files the archive is in.
    pub volumes: u64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub comment: Option<String>,
    pub files: u64,
    pub folders: u64,
    /// Bytes of content, as far as the archive tells before it is unpacked.
    pub bytes: u64,
    pub solid: bool,
    pub encrypted: bool,
}

/// What a set of rows holds, with everything inside the folders among them.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Summary {
    pub files: u64,
    pub folders: u64,
    pub bytes: u64,
    pub packed: u64,
}

/// What an operation takes out of the archive for a set of rows.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Picked {
    /// The folder the rows are shown in, by its path in the archive.
    pub base: String,
    pub selection: Selection,
}

/// The tree of one archive.
pub struct Index {
    info: Info,
    entries: Vec<Entry>,
    nodes: Vec<Node>,
}

/// Finds the folders of the tree by their names while it is built.
#[derive(Default)]
struct Folders {
    /// Folders by the hash of their parent and name; the few that share a
    /// hash are told apart by looking at them.
    known: HashMap<u64, Vec<NodeId>>,
}

impl Folders {
    fn key(parent: NodeId, name: &str) -> u64 {
        let mut hasher = DefaultHasher::new();
        parent.hash(&mut hasher);
        name.hash(&mut hasher);
        hasher.finish()
    }

    fn find(&self, nodes: &[Node], parent: NodeId, name: &str) -> Option<NodeId> {
        self.known
            .get(&Self::key(parent, name))?
            .iter()
            .copied()
            .find(|id| {
                let node = &nodes[*id as usize];
                node.parent == parent && &*node.name == name
            })
    }

    fn add(&mut self, parent: NodeId, name: &str, id: NodeId) {
        self.known
            .entry(Self::key(parent, name))
            .or_default()
            .push(id);
    }
}

fn node(name: &str, parent: NodeId, kind: EntryKind) -> Node {
    Node {
        name: name.into(),
        parent,
        kind,
        entry: None,
        children: Vec::new(),
        bytes: 0,
        packed: 0,
        files: 0,
        folders: 0,
    }
}

fn push(nodes: &mut Vec<Node>, new: Node) -> NodeId {
    let id = nodes.len() as NodeId;
    let parent = new.parent;
    nodes.push(new);
    nodes[parent as usize].children.push(id);
    id
}

/// The folder at `parts` under the root, created with the ones above it
/// when the tree does not have it yet.
fn folder<'a>(
    nodes: &mut Vec<Node>,
    folders: &mut Folders,
    parts: impl Iterator<Item = &'a str>,
) -> NodeId {
    let mut at = ROOT;
    for part in parts {
        at = if let Some(found) = folders.find(nodes, at, part) {
            found
        } else {
            let id = push(nodes, node(part, at, EntryKind::Directory));
            folders.add(at, part, id);
            id
        };
    }
    at
}

/// The digits a text goes on with, taken off it.
fn digits(chars: &mut Peekable<Chars<'_>>) -> String {
    let mut run = String::new();
    while let Some(digit) = chars.next_if(char::is_ascii_digit) {
        run.push(digit);
    }
    run
}

/// A row of a folder with what its place in the list depends on.
struct Keyed<'a> {
    id: NodeId,
    node: &'a Node,
    modified: Option<i64>,
    extension: String,
}

/// Compares names the way people read them: without regard to case, and a
/// run of digits as the number it writes, so `file2` comes before `file10`.
fn natural(one: &str, other: &str) -> Ordering {
    let mut left = one.chars().peekable();
    let mut right = other.chars().peekable();
    loop {
        let (Some(a), Some(b)) = (left.peek().copied(), right.peek().copied()) else {
            return left.peek().is_some().cmp(&right.peek().is_some());
        };
        if a.is_ascii_digit() && b.is_ascii_digit() {
            let (a, b) = (digits(&mut left), digits(&mut right));
            let (a, b) = (a.trim_start_matches('0'), b.trim_start_matches('0'));
            // Without the zeros in front, the longer number is the larger.
            let order = a.len().cmp(&b.len()).then_with(|| a.cmp(b));
            if order != Ordering::Equal {
                return order;
            }
            continue;
        }
        let order = a.to_lowercase().cmp(b.to_lowercase());
        if order != Ordering::Equal {
            return order;
        }
        left.next();
        right.next();
    }
}

/// The extension of a name, in lower case; empty for a name without one.
fn extension(name: &str) -> String {
    match name.rfind('.') {
        // A dot at the start makes a hidden file, not an extension.
        Some(dot) if dot > 0 => name[dot + 1..].to_lowercase(),
        _ => String::new(),
    }
}

impl Index {
    /// Builds the tree of a listing.
    pub fn build(listing: Listing) -> Self {
        let Listing {
            path,
            format,
            size,
            volumes,
            entries,
            comment,
            files,
            folders,
            bytes,
            solid,
            encrypted,
        } = listing;
        // The root is put in by hand: it is the one node that is not among
        // the children of its parent.
        let mut nodes = Vec::with_capacity(entries.len() + 1);
        nodes.push(node("", ROOT, EntryKind::Directory));
        let mut known = Folders::default();
        // Entries of one folder follow each other in most archives: the
        // folder of the last entry is looked at before the tree is walked.
        let mut last: Option<(Vec<&str>, NodeId)> = None;
        for (position, entry) in entries.iter().enumerate() {
            let position = position as u32;
            let path = entry.meta.path.as_str();
            let mut parts: Vec<&str> = parts_of(path).collect();
            if entry.meta.is_dir() {
                let id = folder(&mut nodes, &mut known, parts.into_iter());
                // The archive may name a folder twice; the first one counts.
                if id != ROOT && nodes[id as usize].entry.is_none() {
                    nodes[id as usize].entry = Some(position);
                }
                continue;
            }
            // A file without a name is shown by what the archive calls it.
            let leaf = parts.pop().unwrap_or(path);
            let parent = match &last {
                Some((above, id)) if *above == parts => *id,
                _ => {
                    let id = folder(&mut nodes, &mut known, parts.iter().copied());
                    last = Some((parts, id));
                    id
                }
            };
            let mut new = node(leaf, parent, entry.meta.kind);
            new.entry = Some(position);
            new.bytes = entry.meta.size.unwrap_or(0);
            new.packed = entry.packed_size.unwrap_or(0);
            push(&mut nodes, new);
        }
        // Every node comes after its parent, so one pass from the end sums
        // the folders up.
        for id in (1..nodes.len()).rev() {
            let (bytes, packed, files, folders) = {
                let node = &nodes[id];
                if node.kind == EntryKind::Directory {
                    (node.bytes, node.packed, node.files, node.folders + 1)
                } else {
                    (node.bytes, node.packed, 1, 0)
                }
            };
            let above = nodes[id].parent as usize;
            let parent = &mut nodes[above];
            parent.bytes += bytes;
            parent.packed += packed;
            parent.files += files;
            parent.folders += folders;
        }
        Self {
            info: Info {
                path,
                format,
                size,
                volumes,
                comment,
                files,
                folders,
                bytes,
                solid,
                encrypted,
            },
            entries,
            nodes,
        }
    }

    pub fn info(&self) -> &Info {
        &self.info
    }

    /// How many entries the archive has.
    pub fn count(&self) -> usize {
        self.entries.len()
    }

    fn node(&self, id: NodeId) -> Option<&Node> {
        self.nodes.get(id as usize)
    }

    fn entry_of(&self, node: &Node) -> Option<&Entry> {
        node.entry.map(|position| &self.entries[position as usize])
    }

    /// The entry of the archive a row shows.
    pub fn entry(&self, id: NodeId) -> Option<&Entry> {
        self.entry_of(self.node(id)?)
    }

    /// The name of a row.
    pub fn name(&self, id: NodeId) -> Option<&str> {
        self.node(id).map(|node| &*node.name)
    }

    /// The path of a row in the archive, by the parts the tree shows.
    pub fn path(&self, id: NodeId) -> String {
        let mut parts = Vec::new();
        let mut at = id;
        while at != ROOT {
            let Some(node) = self.node(at) else {
                return String::new();
            };
            parts.push(&*node.name);
            at = node.parent;
        }
        parts.reverse();
        parts.join("/")
    }

    /// The rows of a folder in the order of the list: folders first, then
    /// by the column; empty for what is not a folder.
    pub fn order(&self, folder: NodeId, sort: Sort) -> Vec<NodeId> {
        let Some(parent) = self.node(folder) else {
            return Vec::new();
        };
        let mut rows: Vec<Keyed<'_>> = parent
            .children
            .iter()
            .map(|id| {
                let node = &self.nodes[*id as usize];
                Keyed {
                    id: *id,
                    node,
                    modified: self
                        .entry_of(node)
                        .and_then(|entry| entry.meta.modified)
                        .map(|time| time.seconds),
                    extension: if sort.key == SortKey::Kind {
                        extension(&node.name)
                    } else {
                        String::new()
                    },
                }
            })
            .collect();
        rows.sort_by(|one, other| {
            let folders_first = (other.node.kind == EntryKind::Directory)
                .cmp(&(one.node.kind == EntryKind::Directory));
            let by_key = match sort.key {
                SortKey::Name => Ordering::Equal,
                SortKey::Size => one.node.bytes.cmp(&other.node.bytes),
                SortKey::Packed => one.node.packed.cmp(&other.node.packed),
                SortKey::Modified => one.modified.cmp(&other.modified),
                SortKey::Kind => one.extension.cmp(&other.extension),
            };
            let by_name = || natural(&one.node.name, &other.node.name).then(one.id.cmp(&other.id));
            let ordered = by_key.then_with(by_name);
            folders_first.then(if sort.descending {
                ordered.reverse()
            } else {
                ordered
            })
        });
        rows.into_iter().map(|row| row.id).collect()
    }

    /// The rows among `ids` whose names `needle` asks for. A piece of a
    /// name, a mask with `*` and `?`, or `re:` and a regular expression.
    /// Nothing to look for keeps every row.
    pub fn matching(&self, ids: Vec<NodeId>, needle: &str) -> Vec<NodeId> {
        let needle = needle.trim();
        if needle.is_empty() {
            return ids;
        }
        ids.into_iter()
            .filter(|id| {
                self.node(*id)
                    .is_some_and(|node| crate::extras::name_matches(&node.name, needle))
            })
            .collect()
    }

    /// The rows with these numbers; a number the tree does not have is
    /// left out.
    pub fn rows(&self, ids: &[NodeId]) -> Vec<Row> {
        ids.iter()
            .filter_map(|id| {
                let node = self.node(*id)?;
                let entry = self.entry_of(node);
                let folder = node.kind == EntryKind::Directory;
                Some(Row {
                    id: *id,
                    name: node.name.to_string(),
                    kind: node.kind,
                    size: if folder {
                        Some(node.bytes)
                    } else {
                        entry.and_then(|entry| entry.meta.size)
                    },
                    packed: if folder {
                        Some(node.packed).filter(|packed| *packed > 0)
                    } else {
                        entry.and_then(|entry| entry.packed_size)
                    },
                    modified: entry
                        .and_then(|entry| entry.meta.modified)
                        .map(|time| time.seconds),
                    method: entry
                        .filter(|_| !folder)
                        .and_then(|entry| entry.method.as_deref())
                        .map(str::to_owned),
                    encrypted: entry.is_some_and(|entry| entry.encrypted),
                    target: entry.and_then(|entry| entry.meta.link_target.clone()),
                    items: folder.then_some(node.children.len() as u64),
                    crc32: entry.filter(|_| !folder).and_then(|entry| entry.crc32),
                })
            })
            .collect()
    }

    /// What the rows hold, with everything inside the folders among them.
    pub fn summary(&self, ids: &[NodeId]) -> Summary {
        let mut summary = Summary::default();
        for node in ids.iter().filter_map(|id| self.node(*id)) {
            summary.bytes += node.bytes;
            summary.packed += node.packed;
            if node.kind == EntryKind::Directory {
                summary.files += node.files;
                summary.folders += node.folders + 1;
            } else {
                summary.files += 1;
            }
        }
        summary
    }

    /// What an operation takes for the rows `ids` of the folder `base`:
    /// the entries of the rows and of everything inside them. No rows at
    /// all stand for the whole folder.
    pub fn pick(&self, base: NodeId, ids: &[NodeId]) -> Picked {
        if ids.is_empty() && base == ROOT {
            return Picked {
                base: String::new(),
                selection: Selection::All,
            };
        }
        let mut indexes = Vec::new();
        let mut pending: Vec<NodeId> = if ids.is_empty() {
            self.node(base).map_or_default(|node| node.children.clone())
        } else {
            ids.to_vec()
        };
        while let Some(id) = pending.pop() {
            let Some(node) = self.node(id) else {
                continue;
            };
            if let Some(entry) = self.entry_of(node) {
                indexes.push(entry.index);
            }
            pending.extend(&node.children);
        }
        Picked {
            base: self.path(base),
            selection: Selection::indexes(indexes),
        }
    }
}

#[cfg(test)]
mod tests {
    use boxit_core::{EntryMeta, Timestamp};

    use super::*;

    fn listing(entries: Vec<Entry>) -> Listing {
        Listing {
            path: String::from("a.zip"),
            format: ArchiveFormat::Zip,
            size: 100,
            volumes: 1,
            files: entries.iter().filter(|entry| entry.meta.is_file()).count() as u64,
            folders: entries.iter().filter(|entry| entry.meta.is_dir()).count() as u64,
            bytes: entries
                .iter()
                .map(|entry| entry.meta.size.unwrap_or(0))
                .sum(),
            entries,
            comment: None,
            solid: false,
            encrypted: false,
        }
    }

    fn file(index: u64, path: &str, size: u64) -> Entry {
        Entry {
            index,
            meta: EntryMeta::file(path, size),
            packed_size: Some(size / 2),
            ..Entry::default()
        }
    }

    fn folder_entry(index: u64, path: &str) -> Entry {
        Entry {
            index,
            meta: EntryMeta::directory(path),
            ..Entry::default()
        }
    }

    fn sample() -> Index {
        Index::build(listing(vec![
            folder_entry(0, "docs/"),
            file(1, "docs/b.txt", 10),
            file(2, "docs/a.TXT", 30),
            file(3, "docs/deep/more/c.png", 100),
            file(4, "readme.md", 4),
            folder_entry(5, "empty"),
            file(6, "./docs//deep/d.txt", 6),
        ]))
    }

    fn names(index: &Index, ids: &[NodeId]) -> Vec<String> {
        index.rows(ids).into_iter().map(|row| row.name).collect()
    }

    fn listed(index: &Index, folder: NodeId, key: SortKey, descending: bool) -> Vec<String> {
        names(index, &index.order(folder, Sort { key, descending }))
    }

    fn child(index: &Index, folder: NodeId, name: &str) -> NodeId {
        index
            .order(folder, Sort::default())
            .into_iter()
            .find(|id| index.name(*id) == Some(name))
            .unwrap()
    }

    #[test]
    fn folders_come_from_the_paths_whether_the_archive_names_them_or_not() {
        let index = sample();
        assert_eq!(
            listed(&index, ROOT, SortKey::Name, false),
            ["docs", "empty", "readme.md"]
        );
        let docs = child(&index, ROOT, "docs");
        assert_eq!(
            listed(&index, docs, SortKey::Name, false),
            ["deep", "a.TXT", "b.txt"]
        );
        // "deep" and "more" are in no entry of their own.
        let deep = child(&index, docs, "deep");
        assert_eq!(index.entry(deep), None);
        assert_eq!(
            listed(&index, deep, SortKey::Name, false),
            ["more", "d.txt"]
        );
        assert_eq!(index.path(child(&index, deep, "more")), "docs/deep/more");
        assert_eq!(index.path(ROOT), "");
        // A file is no folder, and a number out of the tree is nothing.
        let nothing: [NodeId; 0] = [];
        let readme = child(&index, ROOT, "readme.md");
        assert_eq!(index.order(readme, Sort::default()), nothing);
        assert_eq!(index.order(9999, Sort::default()), nothing);
        assert_eq!(index.rows(&[9999]), []);
    }

    #[test]
    fn a_folder_shows_what_is_inside_it() {
        let index = sample();
        let docs = child(&index, ROOT, "docs");
        let row = &index.rows(&[docs])[0];
        assert_eq!(row.kind, EntryKind::Directory);
        assert_eq!(row.size, Some(146));
        assert_eq!(row.packed, Some(73));
        assert_eq!(row.items, Some(3));
        let file = &index.rows(&[child(&index, docs, "b.txt")])[0];
        assert_eq!(
            (file.size, file.packed, file.items),
            (Some(10), Some(5), None)
        );

        assert_eq!(
            index.summary(&[docs, child(&index, ROOT, "readme.md")]),
            Summary {
                files: 5,
                folders: 3,
                bytes: 150,
                packed: 75
            }
        );
        assert_eq!(index.summary(&[ROOT]).files, 5);
        assert_eq!(index.info().files, 5);
    }

    #[test]
    fn the_list_is_ordered_by_any_column_with_folders_first() {
        let index = sample();
        let docs = child(&index, ROOT, "docs");
        assert_eq!(
            listed(&index, docs, SortKey::Size, false),
            ["deep", "b.txt", "a.TXT"]
        );
        assert_eq!(
            listed(&index, docs, SortKey::Size, true),
            ["deep", "a.TXT", "b.txt"]
        );
        assert_eq!(
            listed(&index, docs, SortKey::Name, true),
            ["deep", "b.txt", "a.TXT"]
        );
        assert_eq!(
            listed(&index, ROOT, SortKey::Kind, false),
            ["docs", "empty", "readme.md"]
        );

        let mut entries: Vec<Entry> = ["file10.txt", "File2.txt", "file1.txt", "b.zip", "a.zip"]
            .iter()
            .enumerate()
            .map(|(place, name)| file(place as u64, name, 1))
            .collect();
        entries[0].meta.modified = Some(Timestamp::from_seconds(50));
        entries[3].meta.modified = Some(Timestamp::from_seconds(20));
        let index = Index::build(listing(entries));
        assert_eq!(
            listed(&index, ROOT, SortKey::Name, false),
            ["a.zip", "b.zip", "file1.txt", "File2.txt", "file10.txt"]
        );
        assert_eq!(
            listed(&index, ROOT, SortKey::Kind, false),
            ["file1.txt", "File2.txt", "file10.txt", "a.zip", "b.zip"]
        );
        assert_eq!(
            listed(&index, ROOT, SortKey::Modified, true),
            ["file10.txt", "b.zip", "File2.txt", "file1.txt", "a.zip"]
        );
    }

    #[test]
    fn rows_are_found_by_a_part_of_their_names() {
        let index = sample();
        let docs = child(&index, ROOT, "docs");
        let order = index.order(docs, Sort::default());
        assert_eq!(
            names(&index, &index.matching(order.clone(), " TXT ")),
            ["a.TXT", "b.txt"]
        );
        assert_eq!(
            names(&index, &index.matching(order.clone(), "ee")),
            ["deep"]
        );
        assert_eq!(index.matching(order.clone(), "  "), order);
        assert_eq!(
            index.matching(order, "nothing like it"),
            Vec::<NodeId>::new()
        );
        assert_eq!(index.matching(vec![9999], "a"), Vec::<NodeId>::new());
    }

    #[test]
    fn names_are_compared_as_people_read_them() {
        assert_eq!(natural("a2", "a10"), Ordering::Less);
        assert_eq!(natural("a02", "a2"), Ordering::Equal);
        assert_eq!(natural("A", "a"), Ordering::Equal);
        assert_eq!(natural("a", "ab"), Ordering::Less);
        assert_eq!(natural("Яблоко", "арбуз"), Ordering::Greater);
        assert_eq!(natural("", ""), Ordering::Equal);
        assert_eq!(
            natural("99999999999999999999999", "100000000000000000000000"),
            Ordering::Less
        );
        assert_eq!(extension("a.TAR.GZ"), "gz");
        assert_eq!(extension(".hidden"), "");
        assert_eq!(extension("plain"), "");
    }

    #[test]
    fn rows_are_taken_with_everything_inside_them() {
        let index = sample();
        assert_eq!(
            index.pick(ROOT, &[]),
            Picked {
                base: String::new(),
                selection: Selection::All
            }
        );
        let docs = child(&index, ROOT, "docs");
        let deep = child(&index, docs, "deep");
        assert_eq!(
            index.pick(docs, &[deep, child(&index, docs, "b.txt")]),
            Picked {
                base: String::from("docs"),
                selection: Selection::Indexes(vec![1, 3, 6])
            }
        );
        // A folder without rows chosen is taken whole, without itself.
        assert_eq!(
            index.pick(docs, &[]).selection,
            Selection::Indexes(vec![1, 2, 3, 6])
        );
        assert_eq!(
            index.pick(ROOT, &[docs]).selection,
            Selection::Indexes(vec![0, 1, 2, 3, 6])
        );
    }

    #[test]
    fn names_that_repeat_stay_apart_and_folders_that_repeat_are_one() {
        let index = Index::build(listing(vec![
            file(0, "a.txt", 1),
            file(1, "a.txt", 2),
            folder_entry(2, "d"),
            folder_entry(3, "d/"),
            file(4, "d", 3),
            file(5, "d/x", 4),
        ]));
        assert_eq!(
            listed(&index, ROOT, SortKey::Name, false),
            ["d", "a.txt", "a.txt", "d"]
        );
        let order = index.order(ROOT, Sort::default());
        assert_eq!(index.entry(order[0]).unwrap().index, 2);
        assert_eq!(
            names(&index, &index.order(order[0], Sort::default())),
            ["x"]
        );
        assert_eq!(index.entry(order[1]).unwrap().index, 0);
        assert_eq!(index.entry(order[2]).unwrap().index, 1);
    }
}

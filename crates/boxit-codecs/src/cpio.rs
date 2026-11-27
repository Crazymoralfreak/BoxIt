//! CPIO: the archive of `cpio`, which the packages of RPM and the first
//! file systems of Linux are.
//!
//! Members follow one another, each behind a header and its name, to a
//! member called `TRAILER!!!`. The header has four shapes: numbers of
//! sixteen bits in the order of the machine that wrote them, the oldest;
//! octal digits, the one of POSIX; and hexadecimal digits, the one of
//! System V, with or without a checksum. The last one stores the content of
//! a file with several names once, with the last of them.

use std::collections::BTreeMap;

use boxit_core::{Entry, EntryKind, EntryMeta, OpContext, Result, Timestamp};

use crate::bytes::{be16, le16};
use crate::names::NameDecoder;
use crate::source::{Source, SourceReader};
use crate::stored::{cut_short, read_at, Extent, Located, StoredReader};
use crate::support::{damaged, MAX_LINK_TARGET};
use crate::OpenOptions;

/// The name of the member that ends an archive.
const TRAILER: &[u8] = b"TRAILER!!!";
/// The longest name a member may have, the zero that ends it included.
const NAME_LIMIT: u64 = 64 * 1024;

const NEW_HEADER: u64 = 110;
const OLD_HEADER: u64 = 76;
const BINARY_HEADER: u64 = 26;

/// The bits of a mode that tell what a member is, and what they say.
const KIND_MASK: u64 = 0o170_000;
const KIND_FILE: u64 = 0o100_000;
const KIND_FOLDER: u64 = 0o040_000;
const KIND_LINK: u64 = 0o120_000;

/// The header of a member, whatever its shape.
struct Head {
    /// The file on the machine it came from: its names share this.
    file: (u64, u64),
    mode: u64,
    links: u64,
    modified: u64,
    name_size: u64,
    file_size: u64,
    /// Where the name starts, after the header.
    name_at: u64,
    /// What the name and the content are padded to.
    align: u64,
    /// The content of a file with several names is stored once.
    shared: bool,
}

/// A number written in digits; spaces and zeros around it are padding.
fn digits(field: &[u8], radix: u32) -> Result<u64> {
    let text = std::str::from_utf8(field)
        .map_err(|_| damaged("a number of a member header"))?
        .trim_matches([' ', '\0']);
    if text.is_empty() {
        return Ok(0);
    }
    u64::from_str_radix(text, radix).map_err(|_| damaged("a number of a member header"))
}

fn head(reader: &mut SourceReader, size: u64, at: u64) -> Result<Head> {
    let mark = read_at(reader, size, at, 6.min(size - at))?;
    match mark.as_slice() {
        b"070701" | b"070702" => {
            let header = read_at(reader, size, at, NEW_HEADER)?;
            let field = |index: usize| digits(&header[6 + index * 8..14 + index * 8], 16);
            Ok(Head {
                file: ((field(7)? << 32) | field(8)?, field(0)?),
                mode: field(1)?,
                links: field(4)?,
                modified: field(5)?,
                file_size: field(6)?,
                name_size: field(11)?,
                name_at: at + NEW_HEADER,
                align: 4,
                shared: true,
            })
        }
        b"070707" => {
            let header = read_at(reader, size, at, OLD_HEADER)?;
            let field = |from: usize, to: usize| digits(&header[from..to], 8);
            Ok(Head {
                file: (field(6, 12)?, field(12, 18)?),
                mode: field(18, 24)?,
                links: field(36, 42)?,
                modified: field(48, 59)?,
                name_size: field(59, 65)?,
                file_size: field(65, 76)?,
                name_at: at + OLD_HEADER,
                align: 1,
                shared: false,
            })
        }
        [0xC7, 0x71, ..] | [0x71, 0xC7, ..] => {
            let big = mark[0] == 0x71;
            let header = read_at(reader, size, at, BINARY_HEADER)?;
            let word = |index: usize| {
                let word = if big {
                    be16(&header, index * 2)
                } else {
                    le16(&header, index * 2)
                };
                u64::from(word.unwrap_or(0))
            };
            Ok(Head {
                file: (word(1), word(2)),
                mode: word(3),
                links: word(6),
                modified: (word(8) << 16) | word(9),
                name_size: word(10),
                file_size: (word(11) << 16) | word(12),
                name_at: at + BINARY_HEADER,
                align: 2,
                shared: false,
            })
        }
        // Too little is left for the mark of a header to be told.
        _ if mark.len() < 6 => Err(cut_short()),
        _ => Err(damaged("a member header does not start as it should")),
    }
}

struct Member {
    name: Vec<u8>,
    kind: EntryKind,
    target: Option<Vec<u8>>,
    modified: Timestamp,
    mode: u32,
    extent: Extent,
    /// The member this one is another name of.
    link: Option<usize>,
}

fn members(source: &Source, ctx: &OpContext) -> Result<Vec<Member>> {
    let size = source.size();
    let mut reader = source.reader()?;
    let mut members: Vec<Member> = Vec::new();
    // The names of a file that has several, by the file.
    let mut shared: BTreeMap<(u64, u64), Vec<usize>> = BTreeMap::new();
    let mut at = 0u64;
    loop {
        ctx.checkpoint()?;
        if at >= size {
            // The member that ends an archive is missing.
            return Err(cut_short());
        }
        let head = head(&mut reader, size, at)?;
        if head.name_size > NAME_LIMIT {
            return Err(damaged("the name of a member is too long"));
        }
        let mut name = read_at(&mut reader, size, head.name_at, head.name_size)?;
        if let Some(end) = name.iter().position(|byte| *byte == 0) {
            name.truncate(end);
        }
        if name == TRAILER {
            break;
        }
        let data = (head.name_at + head.name_size).next_multiple_of(head.align);
        let end = data
            .checked_add(head.file_size)
            .filter(|end| *end <= size)
            .ok_or_else(cut_short)?;
        at = end.next_multiple_of(head.align);

        let mut extent = Extent {
            offset: data,
            length: head.file_size,
        };
        let mut target = None;
        let kind = match head.mode & KIND_MASK {
            KIND_FILE | 0 => EntryKind::File,
            KIND_FOLDER => {
                extent.length = 0;
                EntryKind::Directory
            }
            KIND_LINK => {
                if head.file_size > MAX_LINK_TARGET {
                    return Err(damaged("the target of a link is too long"));
                }
                target = Some(read_at(&mut reader, size, data, head.file_size)?);
                extent.length = 0;
                EntryKind::Symlink
            }
            // Devices, pipes and sockets have no place in a folder of the
            // user.
            _ => continue,
        };
        if head.shared && head.links > 1 && kind == EntryKind::File {
            shared.entry(head.file).or_default().push(members.len());
        }
        members.push(Member {
            name,
            kind,
            target,
            modified: Timestamp::from_seconds(i64::try_from(head.modified).unwrap_or(i64::MAX)),
            mode: (head.mode & 0o7777) as u32,
            extent,
            link: None,
        });
    }
    for names in shared.values() {
        let mut holders = names
            .iter()
            .filter(|index| members[**index].extent.length > 0);
        // One of the names has the content; with none, or with a copy under
        // each, they are files like any other.
        let (Some(&holder), None) = (holders.next(), holders.next()) else {
            continue;
        };
        let first = names[0];
        members[first].extent = members[holder].extent;
        for &other in &names[1..] {
            members[other].extent.length = 0;
            members[other].link = Some(first);
        }
    }
    Ok(members)
}

fn list(source: &Source, options: &OpenOptions, ctx: &OpContext) -> Result<Vec<Located>> {
    let members = members(source, ctx)?;
    let decoder = NameDecoder::new(
        options.encoding,
        members.iter().map(|member| member.name.as_slice()),
        options.hint.as_deref(),
    );
    let name = |raw: &[u8]| decoder.decode(raw, false).into_owned();
    Ok(members
        .iter()
        .enumerate()
        .map(|(index, member)| {
            let (kind, target) = match member.link {
                Some(first) => (EntryKind::Hardlink, Some(name(&members[first].name))),
                None => (member.kind, member.target.as_deref().map(name)),
            };
            let stored = kind == EntryKind::File && member.extent.length > 0;
            Located {
                entry: Entry {
                    index: index as u64,
                    meta: EntryMeta {
                        path: name(&member.name),
                        kind,
                        link_target: target,
                        size: Some(member.extent.length),
                        modified: Some(member.modified),
                        unix_mode: Some(member.mode),
                        ..EntryMeta::default()
                    },
                    ..Entry::default()
                },
                extents: if stored {
                    vec![member.extent]
                } else {
                    Vec::new()
                },
            }
        })
        .collect())
}

/// A `cpio` archive opened for reading.
pub fn reader(source: Source, options: OpenOptions) -> StoredReader {
    StoredReader::new(source, options, list)
}

#[cfg(test)]
pub(crate) mod tests {
    use boxit_core::{ArchiveReader, Selection};

    use super::*;
    use crate::tests::Collector;

    /// A member as a test describes it.
    pub(crate) struct Spec<'a> {
        pub name: &'a str,
        pub mode: u32,
        pub inode: u32,
        pub links: u32,
        pub data: &'a [u8],
    }

    pub(crate) fn file<'a>(name: &'a str, data: &'a [u8]) -> Spec<'a> {
        Spec {
            name,
            mode: 0o100_644,
            inode: 0,
            links: 1,
            data,
        }
    }

    const MODIFIED: u32 = 1_700_000_000;

    fn pad(bytes: &mut Vec<u8>, align: usize) {
        while !bytes.len().is_multiple_of(align) {
            bytes.push(0);
        }
    }

    /// The shape of System V, in hexadecimal digits.
    pub(crate) fn newc(specs: &[Spec]) -> Vec<u8> {
        let mut bytes = Vec::new();
        let trailer = Spec {
            mode: 0,
            ..file("TRAILER!!!", b"")
        };
        for spec in specs.iter().chain([&trailer]) {
            let fields = [
                spec.inode,
                spec.mode,
                0,
                0,
                spec.links,
                MODIFIED,
                spec.data.len() as u32,
                8,
                1,
                0,
                0,
                spec.name.len() as u32 + 1,
                0,
            ];
            bytes.extend(b"070701");
            for field in fields {
                bytes.extend(format!("{field:08X}").bytes());
            }
            bytes.extend(spec.name.bytes());
            bytes.push(0);
            pad(&mut bytes, 4);
            bytes.extend(spec.data);
            pad(&mut bytes, 4);
        }
        bytes
    }

    /// The shape of POSIX, in octal digits.
    fn odc(specs: &[Spec]) -> Vec<u8> {
        let mut bytes = Vec::new();
        let trailer = Spec {
            mode: 0,
            ..file("TRAILER!!!", b"")
        };
        for spec in specs.iter().chain([&trailer]) {
            bytes.extend(
                format!(
                    "070707{:06o}{:06o}{:06o}{:06o}{:06o}{:06o}{:06o}{:011o}{:06o}{:011o}",
                    0o777,
                    spec.inode,
                    spec.mode,
                    0,
                    0,
                    spec.links,
                    0,
                    MODIFIED,
                    spec.name.len() + 1,
                    spec.data.len()
                )
                .bytes(),
            );
            bytes.extend(spec.name.bytes());
            bytes.push(0);
            bytes.extend(spec.data);
        }
        bytes
    }

    /// The oldest shape: numbers of sixteen bits, in either order.
    fn binary(specs: &[Spec], big: bool) -> Vec<u8> {
        let mut bytes = Vec::new();
        let trailer = Spec {
            mode: 0,
            ..file("TRAILER!!!", b"")
        };
        for spec in specs.iter().chain([&trailer]) {
            let size = spec.data.len() as u32;
            let words = [
                0o070_707,
                1,
                spec.inode as u16,
                spec.mode as u16,
                0,
                0,
                spec.links as u16,
                0,
                (MODIFIED >> 16) as u16,
                MODIFIED as u16,
                spec.name.len() as u16 + 1,
                (size >> 16) as u16,
                size as u16,
            ];
            for word in words {
                bytes.extend(if big {
                    word.to_be_bytes()
                } else {
                    word.to_le_bytes()
                });
            }
            bytes.extend(spec.name.bytes());
            bytes.push(0);
            pad(&mut bytes, 2);
            bytes.extend(spec.data);
            pad(&mut bytes, 2);
        }
        bytes
    }

    fn tree<'a>() -> Vec<Spec<'a>> {
        vec![
            Spec {
                mode: 0o040_755,
                ..file("docs", b"")
            },
            file("docs/readme.txt", b"BoxIt packs and unpacks.\n"),
            file("docs/odd", b"odd"),
            file("empty.bin", b""),
            Spec {
                mode: 0o120_777,
                ..file("docs/link", b"readme.txt")
            },
            // A device: not something to unpack.
            Spec {
                mode: 0o020_644,
                ..file("dev/null", b"")
            },
            file("docs/пример.txt", "привет".as_bytes()),
        ]
    }

    fn read(bytes: Vec<u8>) -> Result<Vec<(Entry, Vec<u8>)>> {
        let ctx = OpContext::silent();
        let mut reader = reader(Source::memory("a.cpio", bytes), OpenOptions::default());
        let listed = reader.entries(&ctx)?;
        let sink = Collector::default();
        reader.extract(&Selection::All, &sink, &ctx)?;
        let got = sink.in_order();
        assert_eq!(
            listed,
            got.iter().map(|got| got.entry.clone()).collect::<Vec<_>>()
        );
        Ok(got.into_iter().map(|got| (got.entry, got.data)).collect())
    }

    fn check(bytes: Vec<u8>, label: &str) {
        let got = read(bytes).unwrap_or_else(|error| panic!("{label}: {error}"));
        let wanted: Vec<Spec> = tree()
            .into_iter()
            .filter(|spec| spec.name != "dev/null")
            .collect();
        assert_eq!(got.len(), wanted.len(), "{label}");
        for ((entry, data), spec) in got.iter().zip(&wanted) {
            assert_eq!(entry.meta.path, spec.name, "{label}");
            assert_eq!(
                entry.meta.modified,
                Some(Timestamp::from_seconds(i64::from(MODIFIED))),
                "{label}"
            );
            assert_eq!(
                entry.meta.unix_mode,
                Some(spec.mode & 0o7777),
                "{label}: {}",
                spec.name
            );
            match spec.mode & 0o170_000 {
                0o040_000 => assert_eq!(entry.meta.kind, EntryKind::Directory, "{label}"),
                0o120_000 => {
                    assert_eq!(entry.meta.kind, EntryKind::Symlink, "{label}");
                    assert_eq!(entry.meta.link_target.as_deref(), Some("readme.txt"));
                    assert_eq!(entry.meta.size, Some(0));
                }
                _ => {
                    assert_eq!(entry.meta.kind, EntryKind::File, "{label}");
                    assert_eq!(data, spec.data, "{label}: {}", spec.name);
                    assert_eq!(entry.meta.size, Some(spec.data.len() as u64));
                }
            }
        }
    }

    #[test]
    fn every_shape_of_the_header_is_read() {
        check(newc(&tree()), "newc");
        check(odc(&tree()), "odc");
        check(binary(&tree(), false), "binary, low byte first");
        check(binary(&tree(), true), "binary, high byte first");
        // The shape with a checksum differs by its mark alone.
        let mut crc = newc(&tree());
        let marks: Vec<usize> = (0..crc.len())
            .filter(|at| crc[*at..].starts_with(b"070701"))
            .collect();
        for at in marks {
            crc[at + 5] = b'2';
        }
        check(crc, "crc");
    }

    #[test]
    fn a_file_with_several_names_is_one_file_and_links_to_it() {
        let linked = |name| Spec {
            inode: 42,
            links: 3,
            ..file(name, b"")
        };
        let bytes = newc(&[
            linked("a/first"),
            file("between", b"other"),
            linked("a/second"),
            Spec {
                data: b"shared content",
                ..linked("b/third")
            },
            // Its own file, though the number of its links says otherwise.
            Spec {
                inode: 7,
                links: 2,
                ..file("alone", b"alone")
            },
        ]);
        let got = read(bytes).unwrap();
        let summary: Vec<_> = got
            .iter()
            .map(|(entry, data)| {
                (
                    entry.meta.path.as_str(),
                    entry.meta.kind,
                    entry.meta.link_target.as_deref(),
                    String::from_utf8_lossy(data).into_owned(),
                )
            })
            .collect();
        assert_eq!(
            summary,
            [
                (
                    "a/first",
                    EntryKind::File,
                    None,
                    String::from("shared content")
                ),
                ("between", EntryKind::File, None, String::from("other")),
                (
                    "a/second",
                    EntryKind::Hardlink,
                    Some("a/first"),
                    String::new()
                ),
                (
                    "b/third",
                    EntryKind::Hardlink,
                    Some("a/first"),
                    String::new()
                ),
                ("alone", EntryKind::File, None, String::from("alone")),
            ]
        );
        assert_eq!(got[0].0.meta.size, Some(14));
    }

    #[test]
    fn what_is_broken_is_said_to_be_broken() {
        let whole = newc(&tree());
        // Without the member that ends it, an archive was cut.
        let trailer = whole.len() - 124;
        assert_eq!(
            read(whole[..trailer].to_vec()).unwrap_err().code,
            "truncated"
        );
        assert_eq!(read(whole[..200].to_vec()).unwrap_err().code, "truncated");
        assert_eq!(read(whole[..50].to_vec()).unwrap_err().code, "truncated");
        assert_eq!(read(Vec::new()).unwrap_err().code, "truncated");
        let mut marked = whole.clone();
        marked[0] = b'1';
        assert_eq!(read(marked).unwrap_err().code, "damaged");
        let mut digit = whole.clone();
        digit[60] = b'z';
        assert_eq!(read(digit).unwrap_err().code, "damaged");
        // A name, and a link, longer than any is.
        let mut named = whole.clone();
        named[94..102].copy_from_slice(b"00FFFFFF");
        assert_eq!(read(named).unwrap_err().code, "damaged");
        let long = vec![b'x'; MAX_LINK_TARGET as usize + 1];
        let link = newc(&[Spec {
            mode: 0o120_777,
            ..file("link", &long)
        }]);
        assert_eq!(read(link).unwrap_err().code, "damaged");
    }
}

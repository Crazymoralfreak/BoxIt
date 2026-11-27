//! AR: the archive of `ar`, which static libraries and the packages of
//! Debian are.
//!
//! Members follow one another, each behind a header of sixty characters. A
//! name is sixteen characters at most, and the two families of Unix found
//! two ways around that: GNU keeps long names in a member of their own and
//! refers to them by position, BSD puts the name before the content.
//! Libraries carry a table of their symbols as one more member; it is not a
//! file of the archive and is left out of its list.

use boxit_core::{Entry, EntryMeta, OpContext, Result, Timestamp};

use crate::names::NameDecoder;
use crate::source::Source;
use crate::stored::{cut_short, read_at, Extent, Located, StoredReader};
use crate::support::damaged;
use crate::OpenOptions;

const MAGIC: &[u8] = b"!<arch>\n";
const HEADER: u64 = 60;
/// What every header ends with.
const HEADER_END: &[u8] = b"`\n";
/// The member of GNU that holds the names too long for a header.
const NAME_TABLE: &[u8] = b"//";
/// What a name kept before the content of its member starts with, in BSD.
const NAME_AHEAD: &[u8] = b"#1/";
/// The members that hold the symbols of a library.
const SYMBOL_TABLES: [&[u8]; 6] = [
    b"/",
    b"/SYM64/",
    b"__.SYMDEF",
    b"__.SYMDEF SORTED",
    b"__.SYMDEF_64",
    b"__.SYMDEF_64 SORTED",
];
/// The most the long names of an archive may take, and one of them.
const NAME_TABLE_LIMIT: u64 = 16 * 1024 * 1024;
const NAME_LIMIT: u64 = 64 * 1024;

/// A number of a header: digits with spaces after them. A field of spaces
/// alone is no number, which the tools of Windows write for an owner.
fn number(field: &[u8], radix: u32) -> Result<Option<u64>> {
    let end = field
        .iter()
        .rposition(|byte| !matches!(byte, b' ' | 0))
        .map_or(0, |last| last + 1);
    let digits = &field[..end];
    if digits.is_empty() {
        return Ok(None);
    }
    std::str::from_utf8(digits)
        .ok()
        .and_then(|digits| u64::from_str_radix(digits, radix).ok())
        .map(Some)
        .ok_or_else(|| damaged("a number of a member header"))
}

struct Member {
    name: Vec<u8>,
    modified: Option<Timestamp>,
    mode: Option<u32>,
    extent: Extent,
}

/// The name a header refers to in the table of long names.
fn long_name(table: &[u8], header: &[u8]) -> Result<Option<Vec<u8>>> {
    let Some(digits) = header.strip_prefix(b"/") else {
        return Ok(None);
    };
    if digits.is_empty() || !digits.iter().all(u8::is_ascii_digit) {
        return Ok(None);
    }
    let start = number(digits, 10)?
        .and_then(|start| usize::try_from(start).ok())
        .filter(|start| *start < table.len())
        .ok_or_else(|| damaged("a long name outside the table of names"))?;
    let rest = &table[start..];
    let end = rest
        .iter()
        .position(|byte| matches!(byte, b'\n' | 0))
        .unwrap_or(rest.len());
    let name = &rest[..end];
    Ok(Some(name.strip_suffix(b"/").unwrap_or(name).to_vec()))
}

fn members(source: &Source, ctx: &OpContext) -> Result<Vec<Member>> {
    let size = source.size();
    let mut reader = source.reader()?;
    if size < MAGIC.len() as u64 || read_at(&mut reader, size, 0, MAGIC.len() as u64)? != MAGIC {
        return Err(damaged("the mark of an ar archive is missing"));
    }
    let mut table: Vec<u8> = Vec::new();
    let mut members = Vec::new();
    let mut at = MAGIC.len() as u64;
    while at < size {
        ctx.checkpoint()?;
        let left = size - at;
        if left < HEADER {
            // A newline that evens the last member out may follow it.
            let tail = read_at(&mut reader, size, at, left)?;
            if tail.iter().all(|byte| *byte == b'\n') {
                break;
            }
            return Err(cut_short());
        }
        let header = read_at(&mut reader, size, at, HEADER)?;
        if &header[58..] != HEADER_END {
            return Err(damaged("a member header does not end as it should"));
        }
        let stored =
            number(&header[48..58], 10)?.ok_or_else(|| damaged("a member without a size"))?;
        let data = at + HEADER;
        let end = data
            .checked_add(stored)
            .filter(|end| *end <= size)
            .ok_or_else(cut_short)?;
        // Members start at even places.
        at = end + (stored & 1);

        let field = &header[..16];
        let padded = field
            .iter()
            .rposition(|byte| *byte != b' ')
            .map_or(0, |last| last + 1);
        let field = &field[..padded];
        let mut extent = Extent {
            offset: data,
            length: stored,
        };
        if field == NAME_TABLE {
            if stored > NAME_TABLE_LIMIT {
                return Err(damaged("the table of long names is too large"));
            }
            table = read_at(&mut reader, size, data, stored)?;
            continue;
        }
        let name = if let Some(length) = field.strip_prefix(NAME_AHEAD) {
            let length = number(length, 10)?
                .filter(|length| *length <= stored && *length <= NAME_LIMIT)
                .ok_or_else(|| damaged("the name of a member is longer than the member"))?;
            let mut name = read_at(&mut reader, size, data, length)?;
            while name.last() == Some(&0) {
                name.pop();
            }
            extent.offset += length;
            extent.length -= length;
            name
        } else if let Some(name) = long_name(&table, field)? {
            name
        } else if SYMBOL_TABLES.contains(&field) {
            continue;
        } else {
            // GNU ends a name with a slash, so it may end with a space.
            field.strip_suffix(b"/").unwrap_or(field).to_vec()
        };
        if SYMBOL_TABLES.contains(&name.as_slice()) {
            continue;
        }
        members.push(Member {
            name,
            modified: number(&header[16..28], 10)?
                .and_then(|seconds| i64::try_from(seconds).ok())
                .map(Timestamp::from_seconds),
            mode: number(&header[40..48], 8)?.map(|mode| (mode & 0o7777) as u32),
            extent,
        });
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
    Ok(members
        .into_iter()
        .enumerate()
        .map(|(index, member)| Located {
            entry: Entry {
                index: index as u64,
                meta: EntryMeta {
                    modified: member.modified,
                    unix_mode: member.mode,
                    ..EntryMeta::file(
                        decoder.decode(&member.name, false).into_owned(),
                        member.extent.length,
                    )
                },
                ..Entry::default()
            },
            extents: vec![member.extent],
        })
        .collect())
}

/// An `ar` archive opened for reading.
pub fn reader(source: Source, options: OpenOptions) -> StoredReader {
    StoredReader::new(source, options, list)
}

#[cfg(test)]
pub(crate) mod tests {
    use std::fmt::Write;

    use boxit_core::ArchiveReader;

    use super::*;
    use crate::tests::Collector;

    /// A header of a member, as `ar` writes it.
    fn header(name: &str, size: usize) -> String {
        format!(
            "{name:<16}{:<12}{:<6}{:<6}{:<8o}{size:<10}`\n",
            1_700_000_000, 0, 0, 0o100_644
        )
    }

    fn padded(mut bytes: Vec<u8>) -> Vec<u8> {
        if bytes.len() % 2 == 1 {
            bytes.push(b'\n');
        }
        bytes
    }

    /// An archive the way GNU `ar` writes it: short names end with a slash,
    /// long ones are in a table.
    pub(crate) fn gnu(files: &[(&str, &[u8])]) -> Vec<u8> {
        let mut table = String::new();
        let mut places = Vec::new();
        for (name, _) in files {
            if name.len() > 15 {
                places.push(Some(table.len()));
                writeln!(table, "{name}/").unwrap();
            } else {
                places.push(None);
            }
        }
        let mut bytes = MAGIC.to_vec();
        // A library starts with the table of its symbols.
        bytes.extend(header("/", 4).into_bytes());
        bytes.extend([0, 0, 0, 0]);
        if !table.is_empty() {
            bytes.extend(header("//", table.len()).into_bytes());
            bytes = padded([bytes, table.into_bytes()].concat());
        }
        for ((name, content), place) in files.iter().zip(places) {
            let field = match place {
                Some(place) => format!("/{place}"),
                None => format!("{name}/"),
            };
            bytes.extend(header(&field, content.len()).into_bytes());
            bytes = padded([bytes.as_slice(), content].concat());
        }
        bytes
    }

    /// An archive the way the `ar` of BSD and macOS writes it: a name that
    /// does not fit, or has a space, goes before the content.
    fn bsd(files: &[(&str, &[u8])]) -> Vec<u8> {
        let mut bytes = MAGIC.to_vec();
        for (name, content) in files {
            if name.len() > 16 || name.contains(' ') {
                let room = name.len().div_ceil(4) * 4;
                bytes.extend(header(&format!("#1/{room}"), room + content.len()).into_bytes());
                bytes.extend(name.as_bytes());
                bytes.extend(vec![0u8; room - name.len()]);
            } else {
                bytes.extend(header(name, content.len()).into_bytes());
            }
            bytes = padded([bytes.as_slice(), content].concat());
        }
        bytes
    }

    fn read(bytes: Vec<u8>) -> Result<Vec<(String, Vec<u8>)>> {
        let ctx = OpContext::silent();
        let mut reader = reader(Source::memory("a.ar", bytes), OpenOptions::default());
        let listed = reader.entries(&ctx)?;
        let sink = Collector::default();
        reader.extract(&boxit_core::Selection::All, &sink, &ctx)?;
        let got = sink.in_order();
        assert_eq!(listed.len(), got.len());
        Ok(got
            .into_iter()
            .map(|got| (got.entry.meta.path, got.data))
            .collect())
    }

    const FILES: [(&str, &[u8]); 4] = [
        ("debian-binary", b"2.0\n"),
        ("a-name-that-is-longer-than-a-header.o", b"odd"),
        ("with space.txt", b"spaced"),
        ("empty", b""),
    ];

    fn wanted() -> Vec<(String, Vec<u8>)> {
        FILES
            .iter()
            .map(|(name, content)| ((*name).to_owned(), content.to_vec()))
            .collect()
    }

    #[test]
    fn the_names_of_gnu_are_read_and_its_tables_left_out() {
        assert_eq!(read(gnu(&FILES)).unwrap(), wanted());
    }

    #[test]
    fn the_names_of_bsd_are_read() {
        let mut files = FILES.to_vec();
        files.insert(0, ("__.SYMDEF SORTED", b"\0\0\0\0\0\0\0\0"));
        assert_eq!(read(bsd(&files)).unwrap(), wanted());
    }

    #[test]
    fn the_attributes_of_a_member_are_kept() {
        let mut reader = reader(Source::memory("a.ar", gnu(&FILES)), OpenOptions::default());
        let listed = reader.entries(&OpContext::silent()).unwrap();
        assert_eq!(listed[0].index, 0);
        assert_eq!(listed[3].index, 3);
        assert_eq!(
            listed[0].meta.modified,
            Some(Timestamp::from_seconds(1_700_000_000))
        );
        assert_eq!(listed[0].meta.unix_mode, Some(0o644));
        assert_eq!(listed[1].meta.size, Some(3));
        // The owner is left empty by the tools of Windows.
        let mut bytes = MAGIC.to_vec();
        bytes.extend(
            format!(
                "{:<16}{:<12}{:<6}{:<6}{:<8}{:<10}`\n",
                "x.obj/", "", "", "", "", 2
            )
            .bytes(),
        );
        bytes.extend(b"ok");
        assert_eq!(read(bytes).unwrap(), [("x.obj".to_owned(), b"ok".to_vec())]);
    }

    #[test]
    fn an_archive_without_members_is_empty() {
        assert_eq!(read(MAGIC.to_vec()).unwrap(), []);
    }

    #[test]
    fn what_is_broken_is_said_to_be_broken() {
        let whole = gnu(&FILES);
        assert_eq!(read(b"!<arch>".to_vec()).unwrap_err().code, "damaged");
        assert_eq!(read(b"not an ar".to_vec()).unwrap_err().code, "damaged");
        // Cut in a header, and in the content of a member.
        assert_eq!(
            read(whole[..MAGIC.len() + 30].to_vec()).unwrap_err().code,
            "truncated"
        );
        assert_eq!(
            read(whole[..whole.len() - 8].to_vec()).unwrap_err().code,
            "truncated"
        );
        let mut marked = whole.clone();
        marked[MAGIC.len() + 58] = b'?';
        assert_eq!(read(marked).unwrap_err().code, "damaged");
        let mut sized = whole.clone();
        sized[MAGIC.len() + 48] = b'x';
        assert_eq!(read(sized).unwrap_err().code, "damaged");
        // A name that points past the table of names.
        let mut bytes = MAGIC.to_vec();
        bytes.extend(header("//", 4).into_bytes());
        bytes.extend(b"ab/\n");
        bytes.extend(header("/99", 0).into_bytes());
        assert_eq!(read(bytes).unwrap_err().code, "damaged");
        // A name before the content that is longer than the member.
        let mut bytes = MAGIC.to_vec();
        bytes.extend(header("#1/20", 4).into_bytes());
        bytes.extend(b"abcd");
        assert_eq!(read(bytes).unwrap_err().code, "damaged");
    }
}

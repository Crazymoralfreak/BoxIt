//! A look at a file of an archive without extracting it.
//!
//! The content of an archive is data from a stranger, and the window that
//! shows it is a browser. So only what cannot run is shown: pictures as
//! pictures, everything that is text as plain text whatever it claims to
//! be, PDF documents in the viewer of the system. A page of HTML is shown
//! as its source. Nothing is written to the disk.

use boxit_codecs::names::{NameDecoder, NameEncoding};
use boxit_core::{Error, OpContext, Result};
use serde::Serialize;

use crate::browse::NodeId;
use crate::sessions::Session;
use crate::text;

const MIB: u64 = 1024 * 1024;

/// The largest picture and the largest document that are read into memory
/// for a look.
const IMAGE_LIMIT: u64 = 64 * MIB;
const PDF_LIMIT: u64 = 128 * MIB;

/// How much of a text is shown: enough to tell what the file is.
const TEXT_LIMIT: u64 = MIB / 2;

/// What a file is shown as.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub enum PreviewKind {
    Image,
    Text,
    Pdf,
}

const IMAGES: &[(&str, &str)] = &[
    ("png", "image/png"),
    ("jpg", "image/jpeg"),
    ("jpeg", "image/jpeg"),
    ("jpe", "image/jpeg"),
    ("gif", "image/gif"),
    ("webp", "image/webp"),
    ("bmp", "image/bmp"),
    ("ico", "image/x-icon"),
    ("avif", "image/avif"),
    // Scripts inside a drawing do not run where it is shown as a picture.
    ("svg", "image/svg+xml"),
];

const TEXTS: &[&str] = &[
    "txt",
    "text",
    "md",
    "markdown",
    "rst",
    "log",
    "nfo",
    "diz",
    "csv",
    "tsv",
    "json",
    "jsonl",
    "xml",
    "yaml",
    "yml",
    "toml",
    "ini",
    "cfg",
    "conf",
    "config",
    "properties",
    "env",
    "html",
    "htm",
    "xhtml",
    "css",
    "scss",
    "less",
    "js",
    "mjs",
    "cjs",
    "ts",
    "tsx",
    "jsx",
    "vue",
    "rs",
    "py",
    "rb",
    "php",
    "pl",
    "lua",
    "go",
    "java",
    "kt",
    "kts",
    "swift",
    "c",
    "h",
    "cc",
    "cpp",
    "cxx",
    "hpp",
    "cs",
    "fs",
    "vb",
    "sql",
    "sh",
    "bash",
    "zsh",
    "fish",
    "bat",
    "cmd",
    "ps1",
    "psm1",
    "reg",
    "inf",
    "gradle",
    "cmake",
    "mk",
    "make",
    "dockerfile",
    "gitignore",
    "gitattributes",
    "editorconfig",
    "tex",
    "srt",
    "sub",
    "vtt",
    "url",
    "desktop",
    "lst",
    "lic",
    "license",
    "readme",
    "changelog",
    "authors",
    "copying",
    "todo",
    "makefile",
];

const PLAIN: &str = "text/plain; charset=utf-8";
const PDF: &str = "application/pdf";

/// What a file with this name is shown as, and the type of its content;
/// `None` for a file there is no look at.
pub fn kind_of(name: &str) -> Option<(PreviewKind, &'static str)> {
    let name = name.to_lowercase();
    // A name without a dot is its own extension: `README`, `Makefile`.
    let extension = name.rsplit('.').next().unwrap_or(&name);
    if let Some((_, mime)) = IMAGES.iter().find(|(known, _)| *known == extension) {
        return Some((PreviewKind::Image, mime));
    }
    if extension == "pdf" {
        return Some((PreviewKind::Pdf, PDF));
    }
    TEXTS
        .contains(&extension)
        .then_some((PreviewKind::Text, PLAIN))
}

/// The content of a file, ready to be shown.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Preview {
    pub kind: PreviewKind,
    /// The type the content is handed to the window as.
    pub mime: &'static str,
    pub content: Vec<u8>,
    /// The file is longer than what is shown of it.
    pub truncated: bool,
}

/// The part of `bytes` that is whole characters of UTF-8, when all of it is
/// UTF-8 but for a character cut at the end.
fn utf8(bytes: &[u8], truncated: bool) -> Option<&str> {
    match std::str::from_utf8(bytes) {
        Ok(text) => Some(text),
        Err(error) if truncated && error.error_len().is_none() => {
            std::str::from_utf8(&bytes[..error.valid_up_to()]).ok()
        }
        Err(_) => None,
    }
}

fn utf16(bytes: &[u8], big_endian: bool) -> String {
    let units = bytes.as_chunks::<2>().0.iter().map(|pair| {
        if big_endian {
            u16::from_be_bytes(*pair)
        } else {
            u16::from_le_bytes(*pair)
        }
    });
    char::decode_utf16(units)
        .map(|unit| unit.unwrap_or(char::REPLACEMENT_CHARACTER))
        .collect()
}

/// A text of any encoding as UTF-8. A file says its encoding with its first
/// bytes, or is UTF-8, or is in a code page: then the one that makes the
/// most sense of it is taken, as it is for the names of an archive.
pub fn decode_text(bytes: &[u8], truncated: bool, hint: Option<&str>) -> String {
    if let Some(rest) = bytes.strip_prefix(&[0xEF, 0xBB, 0xBF]) {
        return String::from_utf8_lossy(rest).into_owned();
    }
    if let Some(rest) = bytes.strip_prefix(&[0xFF, 0xFE]) {
        return utf16(rest, false);
    }
    if let Some(rest) = bytes.strip_prefix(&[0xFE, 0xFF]) {
        return utf16(rest, true);
    }
    if let Some(text) = utf8(bytes, truncated) {
        return text.to_owned();
    }
    NameDecoder::new(NameEncoding::Auto, [bytes], hint)
        .decode(bytes, false)
        .into_owned()
}

/// What is known about the look at a file before its content is read.
struct Plan<'a> {
    /// The entry of the archive, by its place in the list.
    entry: u64,
    name: &'a str,
    kind: PreviewKind,
    mime: &'static str,
    limit: u64,
}

fn too_large(name: &str) -> Error {
    Error::new("previewTooLarge").with("name", name)
}

fn plan(session: &Session, node: NodeId) -> Result<Plan<'_>> {
    let index = &session.index;
    let no_preview = || Error::new("noPreview");
    let entry = index
        .entry(node)
        .filter(|entry| entry.meta.is_file())
        .ok_or_else(no_preview)?;
    let name = index.name(node).unwrap_or_default();
    let (kind, mime) = kind_of(name).ok_or_else(no_preview)?;
    let limit = match kind {
        PreviewKind::Image => IMAGE_LIMIT,
        PreviewKind::Pdf => PDF_LIMIT,
        PreviewKind::Text => TEXT_LIMIT,
    };
    // A size the archive tells in advance saves reading what is not shown.
    if kind != PreviewKind::Text && entry.meta.size.is_some_and(|size| size > limit) {
        return Err(too_large(name));
    }
    Ok(Plan {
        entry: entry.index,
        name,
        kind,
        mime,
        limit,
    })
}

/// What a file of an open archive would be shown as, without reading it.
/// Fails the way [`read`] does, as far as that can be told in advance.
pub fn probe(session: &Session, node: NodeId) -> Result<PreviewKind> {
    plan(session, node).map(|plan| plan.kind)
}

/// Reads a file of an open archive for a look. Fails with `noPreview` for
/// what is not shown, and with `previewTooLarge` for a picture or a
/// document over the limit.
pub fn read(session: &Session, node: NodeId, ctx: &OpContext) -> Result<Preview> {
    let Plan {
        entry,
        name,
        kind,
        mime,
        limit,
    } = plan(session, node)?;
    let too_large = || too_large(name);
    let (content, truncated) = session.archive()?.read(entry, limit, ctx)?;
    let content = match kind {
        PreviewKind::Text => {
            decode_text(&content, truncated, text::system_region().as_deref()).into_bytes()
        }
        // Half a picture is no picture.
        PreviewKind::Image | PreviewKind::Pdf if truncated => return Err(too_large()),
        PreviewKind::Image | PreviewKind::Pdf => content,
    };
    Ok(Preview {
        kind,
        mime,
        content,
        truncated,
    })
}

#[cfg(test)]
mod tests {
    use boxit_core::{ArchiveFormat, CreateOptions, Level};
    use tempfile::TempDir;

    use super::*;
    use crate::browse::{Sort, ROOT};
    use crate::create::{create, CreateRequest, Layout};
    use crate::sessions::Sessions;
    use crate::testing::{lines, plant};

    #[test]
    fn a_file_of_an_archive_is_read_for_a_look() {
        let root = TempDir::new().unwrap();
        let long = lines(120_000, "");
        assert!(long.len() as u64 > TEXT_LIMIT);
        plant(
            &root.path().join("stuff"),
            &[
                ("notes.txt", "привет"),
                ("long.log", &long),
                ("picture.png", "not really a picture"),
                ("tool.exe", "MZ"),
                ("docs.txt/inside.md", "# title"),
            ],
        );
        let silent = OpContext::silent();
        for (format, name) in [
            (ArchiveFormat::Zip, "a.zip"),
            (ArchiveFormat::SevenZ, "a.7z"),
            (ArchiveFormat::TarGz, "a.tar.gz"),
        ] {
            let request = CreateRequest {
                sources: vec![root.path().join("stuff")],
                destination: root.path().join(name),
                options: CreateOptions {
                    level: Level::Fast,
                    ..CreateOptions::new(format)
                },
                overwrite: false,
                layout: Layout::Whole,
            };
            create(&request, &silent).unwrap();
            let sessions = Sessions::new();
            let session = sessions
                .open(&request.destination, None, NameEncoding::Auto, &silent)
                .unwrap();
            let index = &session.index;
            let stuff = index.order(ROOT, Sort::default())[0];
            let node = |wanted: &str| {
                index
                    .order(stuff, Sort::default())
                    .into_iter()
                    .find(|id| index.name(*id) == Some(wanted))
                    .unwrap()
            };

            let notes = read(&session, node("notes.txt"), &silent).unwrap();
            assert_eq!(
                (notes.kind, notes.mime),
                (PreviewKind::Text, PLAIN),
                "{name}"
            );
            assert_eq!(notes.content, "привет".as_bytes(), "{name}");
            assert!(!notes.truncated);

            let log = read(&session, node("long.log"), &silent).unwrap();
            assert!(log.truncated, "{name}");
            assert_eq!(
                log.content,
                &long.as_bytes()[..TEXT_LIMIT as usize],
                "{name}"
            );

            let picture = read(&session, node("picture.png"), &silent).unwrap();
            assert_eq!(
                (picture.kind, picture.mime),
                (PreviewKind::Image, "image/png")
            );
            assert_eq!(picture.content, b"not really a picture", "{name}");

            assert_eq!(probe(&session, node("notes.txt")), Ok(PreviewKind::Text));
            assert_eq!(probe(&session, node("picture.png")), Ok(PreviewKind::Image));
            assert_eq!(
                probe(&session, node("tool.exe")).unwrap_err().code,
                "noPreview"
            );

            let code = |node| read(&session, node, &silent).unwrap_err().code;
            assert_eq!(code(node("tool.exe")), "noPreview", "{name}");
            // A folder is not a text, whatever its name ends with.
            assert_eq!(code(node("docs.txt")), "noPreview", "{name}");
            assert_eq!(code(ROOT), "noPreview");
            assert_eq!(code(99_999), "noPreview");
        }
    }

    #[test]
    fn a_file_is_shown_by_what_its_name_says() {
        assert_eq!(
            kind_of("a/Photo.JPG"),
            Some((PreviewKind::Image, "image/jpeg"))
        );
        assert_eq!(
            kind_of("logo.svg"),
            Some((PreviewKind::Image, "image/svg+xml"))
        );
        assert_eq!(kind_of("book.pdf"), Some((PreviewKind::Pdf, PDF)));
        assert_eq!(kind_of("README"), Some((PreviewKind::Text, PLAIN)));
        assert_eq!(kind_of("notes.tar.txt"), Some((PreviewKind::Text, PLAIN)));
        // A page is shown as its source, never as a page.
        assert_eq!(kind_of("index.html"), Some((PreviewKind::Text, PLAIN)));
        assert_eq!(kind_of("setup.exe"), None);
        assert_eq!(kind_of("archive.zip"), None);
        assert_eq!(kind_of(""), None);
    }

    #[test]
    fn a_text_of_any_encoding_becomes_readable() {
        assert_eq!(decode_text("привет".as_bytes(), false, None), "привет");
        assert_eq!(decode_text(b"\xEF\xBB\xBFhello", false, None), "hello");
        assert_eq!(decode_text(b"\xFF\xFEh\x00i\x00", false, None), "hi");
        assert_eq!(decode_text(b"\xFE\xFF\x00h\x00i", false, None), "hi");
        // "Привет, мир" in the code page of a Russian Windows.
        let cyrillic = b"\xCF\xF0\xE8\xE2\xE5\xF2, \xEC\xE8\xF0";
        assert_eq!(decode_text(cyrillic, false, Some("ru")), "Привет, мир");
        assert_eq!(decode_text(b"", false, None), "");
    }

    #[test]
    fn a_text_cut_in_the_middle_of_a_character_loses_only_that_character() {
        let text = "день".as_bytes();
        let cut = &text[..text.len() - 1];
        assert_eq!(decode_text(cut, true, None), "ден");
        // The same bytes as a whole file are not UTF-8 at all.
        assert_ne!(decode_text(cut, false, None), "ден");
    }
}

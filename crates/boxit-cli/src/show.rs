//! How the command line prints what it has to say.

use std::borrow::Cow;
use std::io::{self, BufWriter, Write};

use boxit_core::{EntryKind, Timestamp};
use boxit_engine::archive::Listing;
use boxit_engine::text::text;
use chrono::{DateTime, Local};
use serde::Serialize;

/// The units of a size, each 1024 of the one before it.
const UNITS: [&str; 5] = [
    "cli.unit.b",
    "cli.unit.kb",
    "cli.unit.mb",
    "cli.unit.gb",
    "cli.unit.tb",
];

const KIB: f64 = 1024.0;

/// Standard output that survives a closed pipe.
pub(crate) fn say(text: impl AsRef<str>) {
    let _ = writeln!(io::stdout(), "{}", text.as_ref());
}

pub(crate) fn warn(text: impl AsRef<str>) {
    let _ = writeln!(io::stderr(), "{}", text.as_ref());
}

pub(crate) fn print_json<T: Serialize>(value: &T) {
    match serde_json::to_string_pretty(value) {
        Ok(text) => say(text),
        Err(error) => warn(format!("error: {error}")),
    }
}

/// A name from an archive as it is safe to show: an archive can name its
/// files with the sequences a terminal obeys, to repaint the screen or to
/// retitle the window.
pub(crate) fn printable(name: &str) -> Cow<'_, str> {
    if name.chars().any(char::is_control) {
        Cow::Owned(
            name.chars()
                .map(|character| {
                    if character.is_control() {
                        '?'
                    } else {
                        character
                    }
                })
                .collect(),
        )
    } else {
        Cow::Borrowed(name)
    }
}

/// A size for the eye: `512 B`, `1.5 MB`.
pub(crate) fn size(language: &str, bytes: u64) -> String {
    if bytes < 1024 {
        return text(language, UNITS[0], &[("value", &bytes.to_string())]);
    }
    let mut value = bytes as f64 / KIB;
    let mut unit = 1;
    while value >= KIB && unit + 1 < UNITS.len() {
        value /= KIB;
        unit += 1;
    }
    text(language, UNITS[unit], &[("value", &format!("{value:.1}"))])
}

/// A speed in megabytes per second.
pub(crate) fn speed(language: &str, bytes: u64, seconds: f64) -> String {
    let value = if seconds > 0.0 {
        bytes as f64 / (KIB * KIB) / seconds
    } else {
        0.0
    };
    text(language, "cli.speed", &[("value", &format!("{value:.1}"))])
}

/// What share of `whole` is `part`, in percent with one decimal.
pub(crate) fn percent(part: u64, whole: u64) -> String {
    if whole == 0 {
        return String::from("100.0");
    }
    format!("{:.1}", part as f64 * 100.0 / whole as f64)
}

/// A time of an entry on the clock of this machine.
fn local_time(time: Timestamp) -> Option<String> {
    let time = DateTime::from_timestamp(time.seconds, time.nanos)?;
    Some(
        time.with_timezone(&Local)
            .format("%Y-%m-%d %H:%M")
            .to_string(),
    )
}

/// The list of an archive as a table, and what it adds up to.
pub(crate) fn listing(language: &str, listing: &Listing) {
    let word = |key: &str| text(language, key, &[]);
    let mut out = BufWriter::new(io::stdout().lock());
    let _ = writeln!(
        out,
        "{:<16}  {:>14}  {:>14}  {:<10}  {}",
        word("cli.column.modified"),
        word("cli.column.size"),
        word("cli.column.packed"),
        word("cli.column.method"),
        word("cli.column.name"),
    );
    for entry in &listing.entries {
        let meta = &entry.meta;
        let number = |value: Option<u64>| value.map_or_else(String::new, |value| value.to_string());
        let mut name = printable(&meta.path).into_owned();
        match meta.kind {
            EntryKind::Directory if !name.ends_with('/') => name.push('/'),
            EntryKind::Symlink | EntryKind::Hardlink => {
                name.push_str(" -> ");
                name.push_str(&printable(meta.link_target.as_deref().unwrap_or_default()));
            }
            EntryKind::File | EntryKind::Directory => {}
        }
        if entry.encrypted {
            name.push_str(" *");
        }
        let _ = writeln!(
            out,
            "{:<16}  {:>14}  {:>14}  {:<10}  {name}",
            meta.modified.and_then(local_time).unwrap_or_default(),
            number(meta.size.filter(|_| meta.is_file())),
            number(entry.packed_size),
            entry.method.as_deref().unwrap_or_default(),
        );
    }
    let _ = writeln!(out);
    let _ = writeln!(
        out,
        "{}",
        text(
            language,
            "cli.listSummary",
            &[
                ("files", &listing.files.to_string()),
                ("folders", &listing.folders.to_string()),
                ("size", &size(language, listing.bytes)),
                ("format", listing.format.id()),
                ("packed", &size(language, listing.size)),
            ],
        )
    );
    if listing.volumes > 1 {
        let count = listing.volumes.to_string();
        let _ = writeln!(
            out,
            "{}",
            text(language, "cli.volumes", &[("count", &count)])
        );
    }
    if listing.solid {
        let _ = writeln!(out, "{}", word("cli.listSolid"));
    }
    if listing.encrypted {
        let _ = writeln!(out, "{}", word("cli.listEncrypted"));
    }
    if let Some(comment) = &listing.comment {
        // A comment has lines of its own; the breaks stay, the rest of the
        // control characters do not.
        let shown: Vec<Cow<'_, str>> = comment.lines().map(printable).collect();
        let _ = writeln!(
            out,
            "{}",
            text(
                language,
                "cli.listComment",
                &[("comment", &shown.join("\n"))]
            )
        );
    }
    let _ = out.flush();
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sizes_are_shown_in_the_unit_that_fits() {
        assert_eq!(size("en", 0), "0 B");
        assert_eq!(size("en", 1023), "1023 B");
        assert_eq!(size("en", 1024), "1.0 KB");
        assert_eq!(size("en", 1536 * 1024), "1.5 MB");
        assert_eq!(size("en", 3 << 30), "3.0 GB");
        assert_eq!(size("en", u64::MAX), "16777216.0 TB");
        assert_eq!(size("ru", 1536 * 1024), "1.5 МБ");
    }

    #[test]
    fn shares_and_speeds_survive_a_zero() {
        assert_eq!(percent(1, 3), "33.3");
        assert_eq!(percent(0, 0), "100.0");
        assert_eq!(speed("en", 10 << 20, 2.0), "5.0 MB/s");
        assert_eq!(speed("en", 10 << 20, 0.0), "0.0 MB/s");
    }

    #[test]
    fn names_cannot_command_the_terminal() {
        assert_eq!(printable("docs/пример.txt"), "docs/пример.txt");
        assert!(matches!(printable("plain"), Cow::Borrowed(_)));
        assert_eq!(printable("a\u{1b}[2J\u{7}b\r\nc"), "a?[2J?b??c");
    }

    #[test]
    fn a_time_is_shown_to_the_minute() {
        let shown = local_time(Timestamp::from_seconds(1_700_000_000)).unwrap();
        assert_eq!(shown.len(), 16, "{shown}");
        assert!(shown.starts_with("2023-11-1"), "{shown}");
        assert_eq!(local_time(Timestamp::from_seconds(i64::MAX)), None);
    }
}

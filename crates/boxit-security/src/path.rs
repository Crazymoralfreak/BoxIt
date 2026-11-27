//! Names from an archive made safe to use on the disk.
//!
//! The check is lexical: it looks at the text of the name and never asks the
//! file system, because the files do not exist yet and because a path that
//! is resolved through the disk can be redirected by a link made a moment
//! ago. A name is refused when it is absolute or climbs with `..`; on
//! Windows, characters and names the system gives a special meaning are
//! replaced, so the entry is still extracted under a harmless name.

use std::fmt;
use std::path::{Path, PathBuf};

use boxit_core::{Error, Result};

/// The rules of which system a name must satisfy.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Platform {
    Windows,
    Unix,
}

impl Platform {
    /// The system this program was built for.
    pub const fn current() -> Self {
        if cfg!(windows) {
            Self::Windows
        } else {
            Self::Unix
        }
    }

    fn is_separator(self, ch: char) -> bool {
        ch == '/' || (self == Self::Windows && ch == '\\')
    }
}

/// A relative path that stays inside the folder it is joined to: no root, no
/// drive, no `..`, and no part the system reads as something else.
#[derive(Clone, Debug, Default, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct SafePath {
    parts: Vec<String>,
}

impl SafePath {
    pub fn parts(&self) -> &[String] {
        &self.parts
    }

    /// The name stood for the destination folder itself (`.` or `./`).
    pub fn is_root(&self) -> bool {
        self.parts.is_empty()
    }

    /// The path inside `root`.
    pub fn to_path(&self, root: &Path) -> PathBuf {
        let mut path = root.to_path_buf();
        path.extend(&self.parts);
        path
    }

    /// The path of the folder that holds this one; `None` for the root.
    pub fn parent(&self) -> Option<Self> {
        let (_, rest) = self.parts.split_last()?;
        Some(Self {
            parts: rest.to_vec(),
        })
    }

    /// The same path with another last part, which must be a plain name.
    #[must_use]
    pub fn with_name(&self, name: &str) -> Self {
        let mut parts = self.parts.clone();
        parts.pop();
        parts.push(name.to_owned());
        Self { parts }
    }

    pub fn name(&self) -> Option<&str> {
        self.parts.last().map(String::as_str)
    }

    /// The path as it is seen from the folder `base`; `None` for a path
    /// that is not inside that folder. The folder itself gives the root.
    pub fn under(&self, base: &Self) -> Option<Self> {
        self.parts.strip_prefix(base.parts()).map(|rest| Self {
            parts: rest.to_vec(),
        })
    }
}

impl fmt::Display for SafePath {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.parts.join("/"))
    }
}

/// Names Windows keeps for devices: a file called so, with any extension,
/// opens the device instead.
fn is_device_name(part: &str) -> bool {
    let base = part.split('.').next().unwrap_or(part).trim_end_matches(' ');
    let upper = base.to_uppercase();
    if matches!(
        upper.as_str(),
        "CON" | "PRN" | "AUX" | "NUL" | "CONIN$" | "CONOUT$"
    ) {
        return true;
    }
    let mut chars = upper.chars();
    let prefix: String = chars.by_ref().take(3).collect();
    let (Some(digit), None) = (chars.next(), chars.next()) else {
        return false;
    };
    // COM1..COM9 and LPT1..LPT9; the superscript digits count as well.
    (prefix == "COM" || prefix == "LPT")
        && matches!(digit, '1'..='9' | '\u{b9}' | '\u{b2}' | '\u{b3}')
}

/// A part of a path as Windows will take it: without the characters it
/// forbids, without the trailing dots and spaces it drops, and not a device.
fn windows_part(part: &str) -> String {
    let mut name: String = part
        .chars()
        .map(|ch| match ch {
            '<' | '>' | ':' | '"' | '|' | '?' | '*' => '_',
            ch if u32::from(ch) < 0x20 => '_',
            ch => ch,
        })
        .collect();
    // "name." and "name " open "name": two entries would land in one file.
    let kept = name.trim_end_matches(['.', ' ']).len();
    name.truncate(kept);
    if name.is_empty() {
        name.push('_');
    }
    if is_device_name(&name) {
        name.insert(0, '_');
    }
    name
}

fn refused(code: &str, name: &str) -> Error {
    Error::new(code).with("name", name)
}

/// The parts of a name from an archive, as [`sanitize`] tells them apart
/// and before it changes any: what a tree of the archive is built from, so
/// that a folder of the tree and the place its content is extracted to
/// are the same folder.
pub fn parts_of(name: &str) -> impl Iterator<Item = &str> {
    parts_for(name, Platform::current())
}

fn parts_for(name: &str, platform: Platform) -> impl Iterator<Item = &str> {
    name.split(move |ch| platform.is_separator(ch))
        .filter(|part| !matches!(*part, "" | "."))
}

/// Makes a name from an archive safe for the system this program runs on.
pub fn sanitize(name: &str) -> Result<SafePath> {
    sanitize_for(name, Platform::current())
}

/// Makes a name from an archive safe under the rules of `platform`.
///
/// Fails with `invalidName` for a name with a NUL character, `absolutePath`
/// for a name that starts at a root or a drive, and `pathTraversal` for one
/// with a `..` part.
pub fn sanitize_for(name: &str, platform: Platform) -> Result<SafePath> {
    if name.contains('\0') {
        return Err(refused("invalidName", &name.replace('\0', "?")));
    }
    if name
        .chars()
        .next()
        .is_some_and(|ch| platform.is_separator(ch))
    {
        return Err(refused("absolutePath", name));
    }
    if platform == Platform::Windows {
        // "C:" and "C:name" are relative to a drive, which is no better.
        let mut chars = name.chars();
        if let (Some(letter), Some(':')) = (chars.next(), chars.next()) {
            if letter.is_ascii_alphabetic() {
                return Err(refused("absolutePath", name));
            }
        }
    }
    let mut parts = Vec::new();
    for part in parts_for(name, platform) {
        match part {
            ".." => return Err(refused("pathTraversal", name)),
            part => parts.push(match platform {
                Platform::Windows => windows_part(part),
                Platform::Unix => part.to_owned(),
            }),
        }
    }
    Ok(SafePath { parts })
}

#[cfg(test)]
mod tests {
    use proptest::prelude::*;

    use super::*;

    fn parts(name: &str, platform: Platform) -> Vec<String> {
        sanitize_for(name, platform).unwrap().parts().to_vec()
    }

    fn code(name: &str, platform: Platform) -> String {
        sanitize_for(name, platform).unwrap_err().code
    }

    #[test]
    fn ordinary_names_pass_unchanged() {
        for platform in [Platform::Windows, Platform::Unix] {
            assert_eq!(parts("a/b/c.txt", platform), ["a", "b", "c.txt"]);
            assert_eq!(parts("./a//b/./c/", platform), ["a", "b", "c"]);
            assert_eq!(parts("папка/файл.txt", platform), ["папка", "файл.txt"]);
            assert_eq!(parts("a/...", Platform::Unix), ["a", "..."]);
            assert!(sanitize_for("./", platform).unwrap().is_root());
            assert!(sanitize_for("", platform).unwrap().is_root());
        }
    }

    #[test]
    fn climbing_and_absolute_names_are_refused() {
        for platform in [Platform::Windows, Platform::Unix] {
            assert_eq!(code("../x", platform), "pathTraversal");
            assert_eq!(code("a/../../x", platform), "pathTraversal");
            assert_eq!(code("a/b/..", platform), "pathTraversal");
            assert_eq!(code("/etc/passwd", platform), "absolutePath");
            assert_eq!(code("a\0b", platform), "invalidName");
        }
        assert_eq!(code("..\\x", Platform::Windows), "pathTraversal");
        assert_eq!(code("a\\..\\..\\x", Platform::Windows), "pathTraversal");
        assert_eq!(
            code("\\\\server\\share\\x", Platform::Windows),
            "absolutePath"
        );
        assert_eq!(code("\\x", Platform::Windows), "absolutePath");
        assert_eq!(code("C:\\x", Platform::Windows), "absolutePath");
        assert_eq!(code("c:x", Platform::Windows), "absolutePath");
        assert_eq!(code("C:", Platform::Windows), "absolutePath");
    }

    #[test]
    fn a_backslash_is_a_name_character_on_unix() {
        assert_eq!(parts("a\\..\\b", Platform::Unix), ["a\\..\\b"]);
        assert_eq!(parts("a\\b/c", Platform::Windows), ["a", "b", "c"]);
    }

    #[test]
    fn windows_names_lose_their_special_meaning() {
        let windows = Platform::Windows;
        assert_eq!(
            parts("a/file.txt:stream", windows),
            ["a", "file.txt_stream"]
        );
        assert_eq!(parts("what?.txt", windows), ["what_.txt"]);
        assert_eq!(parts("a<b>c|d\"e*f", windows), ["a_b_c_d_e_f"]);
        assert_eq!(parts("tab\there", windows), ["tab_here"]);
        assert_eq!(parts("dir./file. . ", windows), ["dir", "file"]);
        assert_eq!(parts("a/.../b", windows), ["a", "_", "b"]);
        assert_eq!(parts("a/ /b", windows), ["a", "_", "b"]);
        for device in [
            "CON",
            "con",
            "NUL",
            "aux.c",
            "PRN.txt",
            "COM1",
            "lpt9.log",
            "COM\u{b9}",
            "nul .txt",
            "CONIN$",
        ] {
            let safe = parts(device, windows);
            assert_eq!(
                safe,
                [format!("_{device}").trim_end_matches(['.', ' ']).to_owned()],
                "{device}"
            );
        }
        for plain in ["CONSOLE", "COM10", "COM0", "LPT", "nulls", "com", "a.con"] {
            assert_eq!(parts(plain, windows), [plain], "{plain}");
        }
        // The same names mean nothing special elsewhere.
        assert_eq!(parts("aux.c", Platform::Unix), ["aux.c"]);
        assert_eq!(parts("a:b?", Platform::Unix), ["a:b?"]);
    }

    #[test]
    fn a_safe_path_joins_under_its_root() {
        let safe = sanitize_for("a/b.txt", Platform::Unix).unwrap();
        assert_eq!(
            safe.to_path(Path::new("root")),
            Path::new("root").join("a").join("b.txt")
        );
        assert_eq!(safe.to_string(), "a/b.txt");
        assert_eq!(safe.name(), Some("b.txt"));
        assert_eq!(safe.parent().unwrap().to_string(), "a");
        assert_eq!(safe.with_name("c.txt").to_string(), "a/c.txt");
        assert_eq!(SafePath::default().parent(), None);
    }

    #[test]
    fn a_path_is_seen_from_a_folder_above_it_and_from_no_other() {
        let unix = Platform::Unix;
        let path = sanitize_for("a/b/c.txt", unix).unwrap();
        let from = |base: &str| path.under(&sanitize_for(base, unix).unwrap());
        assert_eq!(from("a").unwrap().to_string(), "b/c.txt");
        assert_eq!(from("./a/b/").unwrap().to_string(), "c.txt");
        assert_eq!(from("").unwrap(), path);
        assert!(from("a/b/c.txt").unwrap().is_root());
        assert_eq!(from("b"), None);
        assert_eq!(from("a/b/c.txt/d"), None);
        // A folder is a whole part of the path, not a beginning of its text.
        assert_eq!(from("a/b/c"), None);
    }

    #[test]
    fn the_parts_of_a_name_are_the_parts_of_its_safe_path() {
        assert_eq!(
            parts_for("./a//b/./c/", Platform::Unix).collect::<Vec<_>>(),
            ["a", "b", "c"]
        );
        assert_eq!(
            parts_for("a\\b/c", Platform::Windows).collect::<Vec<_>>(),
            ["a", "b", "c"]
        );
        assert_eq!(
            parts_for("a\\b/c", Platform::Unix).collect::<Vec<_>>(),
            ["a\\b", "c"]
        );
        // Nothing is changed or refused here: the tree shows what is there.
        assert_eq!(
            parts_for("/x/../con", Platform::Windows).collect::<Vec<_>>(),
            ["x", "..", "con"]
        );
    }

    /// Text made of the characters that matter to the rules, so that random
    /// names hit them often.
    fn tricky_name() -> impl Strategy<Value = String> {
        let piece = prop_oneof![
            Just("..".to_owned()),
            Just(".".to_owned()),
            Just("/".to_owned()),
            Just("\\".to_owned()),
            Just(":".to_owned()),
            Just(" ".to_owned()),
            Just("C:".to_owned()),
            Just("con".to_owned()),
            Just("NUL".to_owned()),
            Just("COM1".to_owned()),
            Just("?".to_owned()),
            Just("\u{1}".to_owned()),
            "[a-zа-я0-9._ -]{0,6}",
            any::<char>().prop_map(String::from),
        ];
        proptest::collection::vec(piece, 0..12).prop_map(|pieces| pieces.concat())
    }

    fn platform() -> impl Strategy<Value = Platform> {
        prop_oneof![Just(Platform::Windows), Just(Platform::Unix)]
    }

    proptest! {
        #![proptest_config(ProptestConfig::with_cases(2000))]

        #[test]
        fn a_sanitized_path_never_leaves_its_root(name in tricky_name(), platform in platform()) {
            let Ok(safe) = sanitize_for(&name, platform) else {
                return Ok(());
            };
            for part in safe.parts() {
                prop_assert!(!part.is_empty());
                prop_assert!(part != "." && part != "..");
                prop_assert!(!part.contains('/') && !part.contains('\0'));
                if platform == Platform::Windows {
                    prop_assert!(!part.contains(['\\', ':', '<', '>', '"', '|', '?', '*']));
                    prop_assert!(!part.chars().any(|ch| u32::from(ch) < 0x20));
                    prop_assert!(!part.ends_with(['.', ' ']));
                    prop_assert!(!is_device_name(part));
                }
            }
            // The joined path has the root and then exactly the parts.
            let root = Path::new("root");
            let joined = safe.to_path(root);
            if platform == Platform::current() {
                prop_assert!(joined.starts_with(root));
                prop_assert_eq!(joined.components().count(), 1 + safe.parts().len());
            }
        }

        #[test]
        fn sanitizing_twice_changes_nothing(name in tricky_name(), platform in platform()) {
            if let Ok(safe) = sanitize_for(&name, platform) {
                let again = sanitize_for(&safe.to_string(), platform);
                prop_assert_eq!(again, Ok(safe));
            }
        }

        #[test]
        fn a_name_that_climbs_is_always_refused(
            before in "[a-z/]{0,8}",
            after in "[a-z/]{0,8}",
            platform in platform(),
        ) {
            let name = format!("{before}/../{after}");
            prop_assert!(sanitize_for(&name, platform).is_err());
        }

        #[test]
        fn names_without_special_characters_keep_every_part(
            parts in proptest::collection::vec("[a-z0-9]{1,8}", 1..6),
            platform in platform(),
        ) {
            let parts: Vec<String> = parts
                .into_iter()
                .map(|part| if is_device_name(&part) { format!("x{part}") } else { part })
                .collect();
            let safe = sanitize_for(&parts.join("/"), platform).unwrap();
            prop_assert_eq!(safe.parts(), parts.as_slice());
        }
    }
}

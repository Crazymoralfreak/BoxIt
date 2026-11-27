//! Links found in archives.
//!
//! A symbolic link is the classic way out of the destination folder: the
//! archive first stores a link that points elsewhere and then a file "inside"
//! the link. Two rules close it:
//!
//! 1. A link is created only when its target, read as text, stays inside the
//!    destination ([`check_link_target`]).
//! 2. Nothing is ever written through a link the archive brought
//!    ([`LinkTracker`]).
//!
//! The first rule alone is not enough: text that stays inside may lead
//! outside once a part of it is itself a link. That is why `..` is accepted
//! only at the start of a target. Then every target is a climb of a known
//! height from a real folder followed by a walk down, and a walk down through
//! links that each stay inside stays inside.

use std::collections::BTreeSet;
use std::sync::{Mutex, PoisonError};

use boxit_core::{Error, Result};

use crate::path::{Platform, SafePath};

fn escape(link: &SafePath, target: &str) -> Error {
    Error::new("symlinkEscape")
        .with("name", link)
        .with("target", target)
}

/// Checks that a symbolic link stored at `link` with the given target cannot
/// lead outside the folder the archive is extracted into.
pub fn check_link_target(link: &SafePath, target: &str, platform: Platform) -> Result<()> {
    let is_separator = |ch: char| ch == '/' || (platform == Platform::Windows && ch == '\\');
    if target.is_empty() || target.contains('\0') || target.starts_with(is_separator) {
        return Err(escape(link, target));
    }
    if platform == Platform::Windows && target.contains(':') {
        // A drive, a stream or a device path: none of them is a plain walk.
        return Err(escape(link, target));
    }
    // The link sits in its parent folder: that many steps up are available.
    let mut height = link.parts().len().saturating_sub(1);
    let mut climbing = true;
    for part in target.split(is_separator) {
        match part {
            "" | "." => {}
            ".." => {
                if !climbing || height == 0 {
                    return Err(escape(link, target));
                }
                height -= 1;
            }
            _ => climbing = false,
        }
    }
    Ok(())
}

/// Remembers the links an operation has created, so that nothing is written
/// through them. Names are compared without regard to case: on Windows and
/// macOS `Link` and `link` are one name.
#[derive(Default)]
pub struct LinkTracker {
    links: Mutex<BTreeSet<Vec<String>>>,
}

fn folded(parts: &[String]) -> Vec<String> {
    parts.iter().map(|part| part.to_lowercase()).collect()
}

impl LinkTracker {
    pub fn new() -> Self {
        Self::default()
    }

    /// A link was created at this path.
    pub fn add(&self, link: &SafePath) {
        self.links
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .insert(folded(link.parts()));
    }

    /// The path stopped being a link: a file or a folder replaced it.
    pub fn remove(&self, path: &SafePath) {
        self.links
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .remove(&folded(path.parts()));
    }

    /// Whether a link was created at exactly this path.
    pub fn is_link(&self, path: &SafePath) -> bool {
        self.links
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .contains(&folded(path.parts()))
    }

    /// Fails with `symlinkParent` when one of the folders on the way to
    /// `path` is a link created by this operation.
    pub fn check_parents(&self, path: &SafePath) -> Result<()> {
        let links = self.links.lock().unwrap_or_else(PoisonError::into_inner);
        if links.is_empty() {
            return Ok(());
        }
        let parts = folded(path.parts());
        for length in 1..parts.len() {
            if links.contains(&parts[..length]) {
                return Err(Error::new("symlinkParent").with("name", path));
            }
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use proptest::prelude::*;

    use super::*;
    use crate::path::sanitize_for;

    fn safe(name: &str) -> SafePath {
        sanitize_for(name, Platform::Unix).unwrap()
    }

    fn allowed(link: &str, target: &str) -> bool {
        check_link_target(&safe(link), target, Platform::Unix).is_ok()
    }

    #[test]
    fn links_that_stay_inside_are_allowed() {
        assert!(allowed("a/b/link", "file"));
        assert!(allowed("a/b/link", "../file"));
        assert!(allowed("a/b/link", "../../file"));
        assert!(allowed("a/b/link", "./x/y"));
        assert!(allowed("link", "a/b"));
        assert!(allowed("a/link", ".."));
    }

    #[test]
    fn links_that_may_lead_outside_are_refused() {
        assert!(!allowed("link", ".."));
        assert!(!allowed("link", "../x"));
        assert!(!allowed("a/b/link", "../../../x"));
        assert!(!allowed("a/link", "/etc/passwd"));
        assert!(!allowed("a/link", ""));
        assert!(!allowed("a/link", "x\0y"));
        // A climb after a walk down may pass through another link.
        assert!(!allowed("a/b/link", "x/../y"));
        assert!(!allowed("a/b/link", "x/.."));
        let windows =
            |target| check_link_target(&safe("a/link"), target, Platform::Windows).is_ok();
        assert!(windows("x\\y"));
        assert!(!windows("..\\..\\x"));
        assert!(!windows("C:\\Windows"));
        assert!(!windows("\\\\server\\share"));
        assert!(!windows("\\x"));
    }

    #[test]
    fn nothing_is_written_through_a_link() {
        let tracker = LinkTracker::new();
        assert!(tracker.check_parents(&safe("a/b/c")).is_ok());
        tracker.add(&safe("a/Link"));
        assert!(tracker.is_link(&safe("a/link")));
        assert!(tracker.check_parents(&safe("a/link")).is_ok());
        assert!(tracker.check_parents(&safe("a/other/x")).is_ok());
        assert_eq!(
            tracker.check_parents(&safe("a/link/x")).unwrap_err().code,
            "symlinkParent"
        );
        assert!(tracker.check_parents(&safe("a/LINK/deep/x")).is_err());
        tracker.remove(&safe("a/link"));
        assert!(tracker.check_parents(&safe("a/link/x")).is_ok());
    }

    /// Resolves a target the way a file system would if no part of it were a
    /// link, and tells whether the walk ever stands above the root.
    fn leaves_root(link_depth: usize, target: &str) -> bool {
        let mut depth = link_depth as i64;
        for part in target.split('/') {
            match part {
                "" | "." => {}
                ".." => depth -= 1,
                _ => depth += 1,
            }
            if depth < 0 {
                return true;
            }
        }
        false
    }

    proptest! {
        #![proptest_config(ProptestConfig::with_cases(2000))]

        #[test]
        fn an_accepted_target_never_stands_above_the_root(
            folders in proptest::collection::vec("[a-c]{1,2}", 0..4),
            pieces in proptest::collection::vec(prop_oneof![Just(".."), Just("."), Just("x"), Just("")], 0..8),
        ) {
            let mut parts = folders.clone();
            parts.push("link".to_owned());
            let link = safe(&parts.join("/"));
            let target = pieces.join("/");
            if check_link_target(&link, &target, Platform::Unix).is_ok() {
                prop_assert!(!leaves_root(folders.len(), &target));
                // No climb follows a walk down.
                let names: Vec<&str> = pieces
                    .iter()
                    .copied()
                    .filter(|piece| !piece.is_empty() && *piece != ".")
                    .collect();
                let first_name = names.iter().position(|piece| *piece != "..").unwrap_or(names.len());
                prop_assert!(names[first_name..].iter().all(|piece| *piece != ".."));
            }
        }
    }
}

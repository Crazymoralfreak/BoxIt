//! What stands between an archive nobody vouches for and the disk.
//!
//! An archive is data from a stranger. Its names may point outside the
//! folder it is extracted into, its links may lead there, and its declared
//! sizes may lie. Everything extracted passes through this crate:
//!
//! - [`path`] turns a name from an archive into a relative path that cannot
//!   leave the destination;
//! - [`links`] decides which links are harmless and keeps files from being
//!   written through a link;
//! - [`guard`] counts what is really written and stops an archive made to
//!   fill the disk.

#![forbid(unsafe_code)]

pub mod guard;
pub mod links;
pub mod path;

pub use guard::{FileGuard, Guarded, SizeGuard};
pub use links::{check_link_target, LinkTracker};
pub use path::{parts_of, sanitize, sanitize_for, Platform, SafePath};

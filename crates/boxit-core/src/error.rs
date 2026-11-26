//! One error type for every BoxIt crate.
//!
//! An error carries a stable `code` and named `params`. The interface and the
//! command line turn the pair into a message in the language of the user
//! through the `errors.<code>` keys of the dictionaries, so the text of an
//! error lives in the translations and never in Rust code.

use std::collections::BTreeMap;
use std::fmt;
use std::io;

use serde::{Deserialize, Serialize};

/// Code of the error raised when the user cancels an operation.
pub const CANCELLED: &str = "cancelled";

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Error {
    pub code: String,
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub params: BTreeMap<String, String>,
}

pub type Result<T, E = Error> = std::result::Result<T, E>;

impl Error {
    pub fn new(code: &str) -> Self {
        Self {
            code: code.to_owned(),
            params: BTreeMap::new(),
        }
    }

    /// Adds a named parameter used by the localized message.
    #[must_use]
    pub fn with(mut self, key: &str, value: impl fmt::Display) -> Self {
        self.params.insert(key.to_owned(), value.to_string());
        self
    }

    /// Adds the technical description of an underlying failure.
    #[must_use]
    pub fn detail(self, value: impl fmt::Display) -> Self {
        self.with("detail", value)
    }

    pub fn cancelled() -> Self {
        Self::new(CANCELLED)
    }

    pub fn is_cancelled(&self) -> bool {
        self.code == CANCELLED
    }

    /// The error wrapped into an `io::Error`, for code that sits behind
    /// `Read` or `Write` and has no other way to report it.
    pub fn into_io(self) -> io::Error {
        io::Error::other(self)
    }
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.code)?;
        let mut first = true;
        for (key, value) in &self.params {
            f.write_str(if first { " (" } else { ", " })?;
            write!(f, "{key}={value}")?;
            first = false;
        }
        if !first {
            f.write_str(")")?;
        }
        Ok(())
    }
}

impl std::error::Error for Error {}

impl From<io::Error> for Error {
    fn from(error: io::Error) -> Self {
        // An error of ours that travelled through `Read` or `Write` comes
        // back as it was.
        let error = match error.downcast::<Error>() {
            Ok(inner) => return inner,
            Err(error) => error,
        };
        let code = match error.kind() {
            io::ErrorKind::NotFound => "notFound",
            io::ErrorKind::PermissionDenied => "accessDenied",
            io::ErrorKind::StorageFull | io::ErrorKind::QuotaExceeded => "diskFull",
            io::ErrorKind::UnexpectedEof => "truncated",
            // What decoders report for a dictionary larger than they may take.
            io::ErrorKind::OutOfMemory => "notEnoughMemory",
            // What decoders report for data that is not what it should be.
            io::ErrorKind::InvalidData => "damaged",
            _ => "io",
        };
        Self::new(code).detail(error)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn display_lists_params() {
        let error = Error::new("tooLarge").with("size", 10).with("limit", 5);
        assert_eq!(error.to_string(), "tooLarge (limit=5, size=10)");
        assert_eq!(Error::new("io").to_string(), "io");
    }

    #[test]
    fn serializes_without_empty_params() {
        let json = serde_json::to_string(&Error::new("io")).unwrap();
        assert_eq!(json, r#"{"code":"io"}"#);
        let back: Error = serde_json::from_str(&json).unwrap();
        assert_eq!(back, Error::new("io"));
    }

    #[test]
    fn cancelled_is_recognized() {
        assert!(Error::cancelled().is_cancelled());
        assert!(!Error::new("io").is_cancelled());
    }

    #[test]
    fn io_errors_get_codes_of_their_kind() {
        let missing: Error = io::Error::from(io::ErrorKind::NotFound).into();
        assert_eq!(missing.code, "notFound");
        let denied: Error = io::Error::from(io::ErrorKind::PermissionDenied).into();
        assert_eq!(denied.code, "accessDenied");
        let full: Error = io::Error::from(io::ErrorKind::StorageFull).into();
        assert_eq!(full.code, "diskFull");
        let short: Error = io::Error::from(io::ErrorKind::UnexpectedEof).into();
        assert_eq!(short.code, "truncated");
        let memory: Error = io::Error::from(io::ErrorKind::OutOfMemory).into();
        assert_eq!(memory.code, "notEnoughMemory");
        let other: Error = io::Error::other("something").into();
        assert_eq!(other.code, "io");
        assert_eq!(other.params["detail"], "something");
    }

    #[test]
    fn an_error_survives_a_trip_through_io() {
        let original = Error::new("pathTraversal").with("name", "../x");
        let back: Error = original.clone().into_io().into();
        assert_eq!(back, original);
    }
}

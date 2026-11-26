//! Limits on what an archive may put on the disk.
//!
//! A "zip bomb" is a small archive that unpacks into more data than the disk
//! holds. The sizes an archive declares cannot be trusted, so the limits are
//! enforced on the bytes that are really written: every writer of extracted
//! data is wrapped into [`Guarded`], and the write that crosses a limit
//! fails. The declared sizes are still checked first ([`SizeGuard::declare`]),
//! so an honest archive that does not fit is refused before anything is
//! written.

use std::io::{self, Write};
use std::sync::atomic::{AtomicU64, Ordering};

use boxit_core::{Error, Limits, Result};

/// The limits of one operation, shared by all its threads.
#[derive(Debug)]
pub struct SizeGuard {
    limits: Limits,
    /// Size of the archive itself, in bytes.
    packed: u64,
    /// Free space of the destination when the operation started.
    free: Option<u64>,
    written: AtomicU64,
}

impl SizeGuard {
    /// `packed` is the size of the archive file; `free` the free space of
    /// the disk the content goes to, when it is known.
    pub fn new(limits: Limits, packed: u64, free: Option<u64>) -> Self {
        Self {
            limits,
            packed,
            free: free.filter(|_| limits.check_free_space),
            written: AtomicU64::new(0),
        }
    }

    /// Bytes written so far.
    pub fn written(&self) -> u64 {
        self.written.load(Ordering::Relaxed)
    }

    fn check_file(&self, name: &str, size: u64) -> Result<()> {
        if size > self.limits.max_file_size {
            return Err(Error::new("fileTooLarge")
                .with("name", name)
                .with("limit", self.limits.max_file_size));
        }
        Ok(())
    }

    fn check_total(&self, total: u64) -> Result<()> {
        if total > self.limits.ratio_floor
            && total > self.limits.max_ratio.saturating_mul(self.packed.max(1))
        {
            return Err(Error::new("bombSuspected")
                .with("ratio", total / self.packed.max(1))
                .with("limit", self.limits.max_ratio));
        }
        if let Some(free) = self.free {
            if total > free {
                return Err(Error::new("notEnoughSpace")
                    .with("needed", total)
                    .with("free", free));
            }
        }
        Ok(())
    }

    /// Checks the sizes the archive declares for the files about to be
    /// extracted: `(name, size)` pairs. Nothing is counted as written.
    pub fn declare<'a>(&self, files: impl IntoIterator<Item = (&'a str, u64)>) -> Result<()> {
        let mut total = 0u64;
        for (name, size) in files {
            self.check_file(name, size)?;
            total = total.saturating_add(size);
        }
        self.check_total(total)
    }

    /// Starts counting the bytes of one extracted file.
    pub fn file<'a>(&'a self, name: &'a str) -> FileGuard<'a> {
        FileGuard {
            guard: self,
            name,
            written: 0,
        }
    }
}

/// Counts the bytes of one file against the limits of the operation.
#[derive(Debug)]
pub struct FileGuard<'a> {
    guard: &'a SizeGuard,
    name: &'a str,
    written: u64,
}

impl FileGuard<'_> {
    /// Accounts for `bytes` more bytes of the file; fails when they cross a
    /// limit. Call before writing them.
    pub fn add(&mut self, bytes: u64) -> Result<()> {
        self.written = self.written.saturating_add(bytes);
        self.guard.check_file(self.name, self.written)?;
        let total = self
            .guard
            .written
            .fetch_add(bytes, Ordering::Relaxed)
            .saturating_add(bytes);
        self.guard.check_total(total)
    }

    /// Bytes of this file so far.
    pub fn written(&self) -> u64 {
        self.written
    }
}

/// A writer of extracted data that enforces the limits on what passes
/// through it.
pub struct Guarded<'a, W> {
    inner: W,
    file: FileGuard<'a>,
}

impl<'a, W: Write> Guarded<'a, W> {
    pub fn new(inner: W, file: FileGuard<'a>) -> Self {
        Self { inner, file }
    }

    /// Bytes written through this writer.
    pub fn written(&self) -> u64 {
        self.file.written()
    }

    pub fn into_inner(self) -> W {
        self.inner
    }
}

impl<W: Write> Write for Guarded<'_, W> {
    fn write(&mut self, data: &[u8]) -> io::Result<usize> {
        self.file.add(data.len() as u64).map_err(Error::into_io)?;
        self.inner.write_all(data)?;
        Ok(data.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        self.inner.flush()
    }
}

#[cfg(test)]
mod tests {
    use proptest::prelude::*;

    use super::*;

    const MIB: u64 = 1024 * 1024;

    fn limits() -> Limits {
        Limits {
            max_file_size: 10 * MIB,
            max_ratio: 100,
            ratio_floor: MIB,
            check_free_space: true,
        }
    }

    #[test]
    fn a_file_over_the_limit_is_stopped_at_the_byte_that_crosses_it() {
        let guard = SizeGuard::new(limits(), 100 * MIB, None);
        let mut file = guard.file("big.bin");
        file.add(10 * MIB).unwrap();
        let error = file.add(1).unwrap_err();
        assert_eq!(error.code, "fileTooLarge");
        assert_eq!(error.params["name"], "big.bin");
        // Another file has its own count.
        guard.file("other.bin").add(MIB).unwrap();
    }

    #[test]
    fn the_ratio_counts_real_bytes_across_all_files() {
        // A 1 KiB archive may unpack to 100 KiB by the ratio, but small
        // archives are not judged until the floor of 1 MiB is passed.
        let guard = SizeGuard::new(limits(), 1024, None);
        for _ in 0..4 {
            guard.file("zeros").add(256 * 1024).unwrap();
        }
        assert_eq!(guard.written(), MIB);
        let error = guard.file("zeros").add(1).unwrap_err();
        assert_eq!(error.code, "bombSuspected");
        assert_eq!(error.params["limit"], "100");
    }

    #[test]
    fn a_large_honest_archive_passes() {
        let guard = SizeGuard::new(limits(), 10 * MIB, None);
        for _ in 0..50 {
            guard.file("file").add(10 * MIB).unwrap();
        }
    }

    #[test]
    fn free_space_is_a_limit_unless_switched_off() {
        let guard = SizeGuard::new(limits(), 100 * MIB, Some(3 * MIB));
        guard.file("a").add(3 * MIB).unwrap();
        let error = guard.file("b").add(1).unwrap_err();
        assert_eq!(error.code, "notEnoughSpace");

        let relaxed = Limits {
            check_free_space: false,
            ..limits()
        };
        let guard = SizeGuard::new(relaxed, 100 * MIB, Some(3 * MIB));
        guard.file("a").add(5 * MIB).unwrap();
    }

    #[test]
    fn declared_sizes_are_checked_before_anything_is_written() {
        let guard = SizeGuard::new(limits(), 100 * MIB, Some(15 * MIB));
        guard.declare([("a", 5 * MIB), ("b", 5 * MIB)]).unwrap();
        assert_eq!(
            guard.declare([("a", 11 * MIB)]).unwrap_err().code,
            "fileTooLarge"
        );
        assert_eq!(
            guard
                .declare([("a", 8 * MIB), ("b", 8 * MIB)])
                .unwrap_err()
                .code,
            "notEnoughSpace"
        );
        assert_eq!(
            guard.declare([("a", u64::MAX)]).unwrap_err().code,
            "fileTooLarge"
        );
        assert_eq!(guard.written(), 0);
        let bomb = SizeGuard::new(limits(), 1024, None);
        assert_eq!(
            bomb.declare([("a", 2 * MIB)]).unwrap_err().code,
            "bombSuspected"
        );
    }

    #[test]
    fn no_limits_means_no_limits() {
        let guard = SizeGuard::new(Limits::none(), 1, Some(1));
        guard.file("a").add(u64::MAX / 2).unwrap();
        guard.declare([("a", u64::MAX), ("b", u64::MAX)]).unwrap();
    }

    #[test]
    fn a_guarded_writer_refuses_the_write_that_crosses_a_limit() {
        let guard = SizeGuard::new(limits(), 100 * MIB, Some(100));
        let mut writer = Guarded::new(Vec::new(), guard.file("a"));
        writer.write_all(&[1; 60]).unwrap();
        let error = writer.write_all(&[1; 60]).unwrap_err();
        assert_eq!(Error::from(error).code, "notEnoughSpace");
        assert_eq!(writer.into_inner().len(), 60);
    }

    proptest! {
        #[test]
        fn what_was_let_through_is_within_every_limit(
            packed in 1u64..10_000,
            free in proptest::option::of(0u64..5_000_000),
            writes in proptest::collection::vec((0usize..4, 0u64..400_000), 0..60),
        ) {
            let limits = Limits {
                max_file_size: 1_000_000,
                max_ratio: 500,
                ratio_floor: 100_000,
                check_free_space: true,
            };
            let guard = SizeGuard::new(limits, packed, free);
            let names = ["a", "b", "c", "d"];
            let mut files: Vec<FileGuard<'_>> = names.iter().map(|name| guard.file(name)).collect();
            let mut accepted = 0u64;
            for (file, bytes) in writes {
                if files[file].add(bytes).is_err() {
                    break;
                }
                accepted += bytes;
                prop_assert!(files[file].written() <= limits.max_file_size);
                prop_assert!(accepted <= limits.ratio_floor.max(limits.max_ratio * packed));
                if let Some(free) = free {
                    prop_assert!(accepted <= free);
                }
            }
        }
    }
}

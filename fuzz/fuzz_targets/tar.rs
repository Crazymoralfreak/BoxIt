//! The reader of a plain tar: headers of every dialect, long names, sparse
//! files and the extended records.
#![no_main]

use boxit_core::ArchiveFormat;
use libfuzzer_sys::fuzz_target;

fuzz_target!(|data: &[u8]| boxit_fuzz::read_as(ArchiveFormat::Tar, data));

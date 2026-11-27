//! The reader of 7z: the header, plain, compressed or encrypted, and the
//! chains of coders of its blocks.
#![no_main]

use boxit_core::ArchiveFormat;
use libfuzzer_sys::fuzz_target;

fuzz_target!(|data: &[u8]| boxit_fuzz::read_as(ArchiveFormat::SevenZ, data));

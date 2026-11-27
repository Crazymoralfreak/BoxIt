//! The reader of disc images: ISO 9660 with Joliet and Rock Ridge, and
//! UDF.
#![no_main]

use boxit_core::ArchiveFormat;
use libfuzzer_sys::fuzz_target;

fuzz_target!(|data: &[u8]| boxit_fuzz::read_as(ArchiveFormat::Iso, data));

//! The reader of ZIP: the list at the end of the file, the names in their
//! code pages, every compression method and both kinds of encryption.
#![no_main]

use boxit_core::ArchiveFormat;
use libfuzzer_sys::fuzz_target;

fuzz_target!(|data: &[u8]| boxit_fuzz::read_as(ArchiveFormat::Zip, data));

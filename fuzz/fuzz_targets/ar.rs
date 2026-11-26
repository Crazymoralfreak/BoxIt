//! The reader of r: the libraries of object files, with the long names
//! of GNU and of BSD.
#![no_main]

use boxit_core::ArchiveFormat;
use libfuzzer_sys::fuzz_target;

fuzz_target!(|data: &[u8]| boxit_fuzz::read_as(ArchiveFormat::Ar, data));

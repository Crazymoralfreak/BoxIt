//! The reader of RAR: UnRAR, the library in C++, behind the callback that
//! takes its data. The workflow builds the library with the sanitizers too.
#![no_main]

use boxit_core::ArchiveFormat;
use libfuzzer_sys::fuzz_target;

fuzz_target!(|data: &[u8]| boxit_fuzz::read_as(ArchiveFormat::Rar, data));

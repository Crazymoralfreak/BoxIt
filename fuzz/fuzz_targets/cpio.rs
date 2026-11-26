//! The reader of cpio: the shapes in octal and in hexadecimal digits and the
//! binary ones of both byte orders.
#![no_main]

use boxit_core::ArchiveFormat;
use libfuzzer_sys::fuzz_target;

fuzz_target!(|data: &[u8]| boxit_fuzz::read_as(ArchiveFormat::Cpio, data));

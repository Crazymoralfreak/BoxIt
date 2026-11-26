//! The reader of the images of Windows: the table of pieces, the lists of
//! files, XPRESS, LZX and LZMS, and the pieces compressed together.
#![no_main]

use boxit_core::ArchiveFormat;
use libfuzzer_sys::fuzz_target;

fuzz_target!(|data: &[u8]| boxit_fuzz::read_as(ArchiveFormat::Wim, data));

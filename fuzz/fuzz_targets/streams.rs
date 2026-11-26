//! The compressed streams, as single files and with a tar inside. The first
//! byte of the input chooses the format, the rest is the file.
#![no_main]

use boxit_core::ArchiveFormat;
use libfuzzer_sys::fuzz_target;

const FORMATS: [ArchiveFormat; 10] = [
    ArchiveFormat::Gz,
    ArchiveFormat::Bz2,
    ArchiveFormat::Xz,
    ArchiveFormat::Zst,
    ArchiveFormat::Lz4,
    ArchiveFormat::TarGz,
    ArchiveFormat::TarBz2,
    ArchiveFormat::TarXz,
    ArchiveFormat::TarZst,
    ArchiveFormat::TarLz4,
];

fuzz_target!(|data: &[u8]| {
    if let Some((choice, file)) = data.split_first() {
        boxit_fuzz::read_as(FORMATS[usize::from(*choice) % FORMATS.len()], file);
    }
});

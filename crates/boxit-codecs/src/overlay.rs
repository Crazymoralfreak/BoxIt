//! An archive glued to the end of a program: a self-extracting archive.
//!
//! The program is a Windows executable. The archive starts where the
//! program's own bytes end, and it is a ZIP or a 7z.

use std::io::{Read, Seek, SeekFrom};

use boxit_core::{ArchiveFormat, Error, Result};

use crate::source::Source;

/// The signature of a 7z archive.
const SEVENZ: [u8; 6] = [0x37, 0x7A, 0xBC, 0xAF, 0x27, 0x1C];
/// The signature of a local header of a ZIP entry.
const ZIP_LOCAL: [u8; 4] = [0x50, 0x4B, 0x03, 0x04];

/// An archive found after a program.
pub(crate) struct Pasted {
    pub format: ArchiveFormat,
    /// Where the archive starts in the file.
    pub offset: u64,
}

/// Reads `buffer` from `at`. A short read is a file that ends there.
fn read_at(source: &Source, at: u64, buffer: &mut [u8]) -> Result<usize> {
    let mut reader = source.reader()?;
    reader.seek(SeekFrom::Start(at)).map_err(Error::from)?;
    reader.read(buffer).map_err(Error::from)
}

fn le16(bytes: &[u8]) -> u64 {
    u16::from_le_bytes([bytes[0], bytes[1]]) as u64
}

fn le32(bytes: &[u8]) -> u64 {
    u32::from_le_bytes([bytes[0], bytes[1], bytes[2], bytes[3]]) as u64
}

fn signature(magic: &[u8]) -> Option<ArchiveFormat> {
    if magic.len() >= SEVENZ.len() && magic[..SEVENZ.len()] == SEVENZ {
        Some(ArchiveFormat::SevenZ)
    } else if magic.len() >= ZIP_LOCAL.len() && magic[..ZIP_LOCAL.len()] == ZIP_LOCAL {
        Some(ArchiveFormat::Zip)
    } else {
        None
    }
}

/// The archive glued after the program in `source`, when the file is one.
pub(crate) fn find(source: &Source) -> Result<Option<Pasted>> {
    let size = source.size();
    if size < 64 {
        return Ok(None);
    }
    let mut head = [0u8; 64];
    if read_at(source, 0, &mut head)? < 64 || head[0..2] != *b"MZ" {
        return Ok(None);
    }
    let e_lfanew = le32(&head[0x3C..0x40]);
    if e_lfanew < 64 || e_lfanew.saturating_add(24) > size {
        return Ok(None);
    }
    let mut pe = [0u8; 24];
    if read_at(source, e_lfanew, &mut pe)? < 24 || pe[0..4] != *b"PE\0\0" {
        return Ok(None);
    }
    let sections = le16(&pe[6..8]);
    let optional = le16(&pe[20..22]);
    // A program has a modest number of sections; more is not one.
    if sections > 96 {
        return Ok(None);
    }
    let table = e_lfanew + 24 + optional;
    let mut payload = table;
    let mut alignment = 512u64;
    if optional >= 0x28 && e_lfanew + 24 + 0x28 <= size {
        let mut bytes = [0u8; 4];
        if read_at(source, e_lfanew + 24 + 0x24, &mut bytes)? == 4 {
            let align = le32(&bytes);
            if align.is_power_of_two() && (512..=65_536).contains(&align) {
                alignment = align;
            }
        }
    }
    let mut section = [0u8; 40];
    for index in 0..sections {
        let at = table + index * 40;
        if at + 40 > size || read_at(source, at, &mut section)? < 40 {
            return Ok(None);
        }
        let raw_size = le32(&section[16..20]);
        let raw_ptr = le32(&section[20..24]);
        payload = payload.max(raw_ptr.saturating_add(raw_size));
    }
    let aligned = payload.saturating_add(alignment - 1) & !(alignment - 1);
    for offset in [aligned, payload] {
        if offset >= size {
            continue;
        }
        let mut magic = [0u8; 6];
        let read = read_at(source, offset, &mut magic)?;
        if let Some(format) = signature(&magic[..read]) {
            return Ok(Some(Pasted { format, offset }));
        }
    }
    // The program may be padded by a different amount than its header says.
    let window_end = (payload.saturating_add(alignment.saturating_mul(2))).min(size);
    if window_end <= payload.saturating_add(6) {
        return Ok(None);
    }
    let length = (window_end - payload) as usize;
    if length > 256 * 1024 {
        return Ok(None);
    }
    let mut window = vec![0u8; length];
    let read = read_at(source, payload, &mut window)?;
    window.truncate(read);
    for (index, _) in window.iter().enumerate() {
        if let Some(format) = signature(&window[index..]) {
            return Ok(Some(Pasted {
                format,
                offset: payload + index as u64,
            }));
        }
    }
    Ok(None)
}

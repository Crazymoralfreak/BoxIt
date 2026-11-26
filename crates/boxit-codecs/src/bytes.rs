//! Numbers at a place in a block of bytes. A place past the end of the block
//! is `None`, never a panic: the blocks come from archives.

fn array<const N: usize>(bytes: &[u8], at: usize) -> Option<[u8; N]> {
    bytes.get(at..at.checked_add(N)?)?.try_into().ok()
}

pub(crate) fn le16(bytes: &[u8], at: usize) -> Option<u16> {
    array(bytes, at).map(u16::from_le_bytes)
}

pub(crate) fn le32(bytes: &[u8], at: usize) -> Option<u32> {
    array(bytes, at).map(u32::from_le_bytes)
}

pub(crate) fn le64(bytes: &[u8], at: usize) -> Option<u64> {
    array(bytes, at).map(u64::from_le_bytes)
}

pub(crate) fn be16(bytes: &[u8], at: usize) -> Option<u16> {
    array(bytes, at).map(u16::from_be_bytes)
}

/// `length` bytes at `at`.
pub(crate) fn part(bytes: &[u8], at: usize, length: usize) -> Option<&[u8]> {
    bytes.get(at..at.checked_add(length)?)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn numbers_are_read_in_either_order() {
        let bytes = [0x01, 0x02, 0x03, 0x04, 0x05, 0x06, 0x07, 0x08, 0x09];
        assert_eq!(le16(&bytes, 0), Some(0x0201));
        assert_eq!(be16(&bytes, 0), Some(0x0102));
        assert_eq!(le32(&bytes, 1), Some(0x0504_0302));
        assert_eq!(le64(&bytes, 1), Some(0x0908_0706_0504_0302));
        assert_eq!(part(&bytes, 7, 2), Some(&bytes[7..]));
    }

    #[test]
    fn a_place_past_the_end_is_no_number() {
        let bytes = [1, 2, 3];
        assert_eq!(le16(&bytes, 2), None);
        assert_eq!(le32(&bytes, 0), None);
        assert_eq!(le64(&bytes, usize::MAX), None);
        assert_eq!(be16(&bytes, usize::MAX - 1), None);
        assert_eq!(part(&bytes, 2, 2), None);
        assert_eq!(part(&bytes, usize::MAX, 2), None);
    }
}

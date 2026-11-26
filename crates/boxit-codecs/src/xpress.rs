//! XPRESS with Huffman codes, as images of Windows hold it.
//!
//! A chunk starts with the lengths of the codewords of 512 symbols, four
//! bits each. The first 256 symbols are bytes; the others are matches, and
//! say the length of the match and how many bits its distance takes. The
//! bits come in 16-bit words; the longer lengths are whole bytes that lie
//! among the words, right where the reader of bits has come to.

use crate::huffman::{Coded, Huffman};

const SYMBOLS: usize = 512;
const TABLE: usize = SYMBOLS / 2;
const BYTES: u16 = 256;
const MIN_MATCH: usize = 3;
/// The length a symbol holds when the true one is in the bytes after.
const LONG: usize = 15;
/// The same for the byte after.
const LONGER: usize = LONG + 255;

const BAD_CODE: &str = "XPRESS: a codeword that stands for nothing";
const BAD_TREE: &str = "XPRESS: lengths of codewords that make no code";
const BAD_MATCH: &str = "XPRESS: a match with what was never written";
const CUT: &str = "XPRESS: the data ends too early";

/// Bits of 16-bit words, the highest bit of a word first, and the bytes
/// between the words.
struct Words<'a> {
    data: &'a [u8],
    at: usize,
    buffer: u32,
    count: u32,
}

impl Words<'_> {
    /// Sixteen bits at least are in the buffer after this; zeros past the
    /// end of the data.
    #[inline]
    fn fill(&mut self) {
        if self.count < 16 {
            let low = self.data.get(self.at).copied().unwrap_or(0);
            let high = self.data.get(self.at + 1).copied().unwrap_or(0);
            self.at += 2;
            self.buffer |= (u32::from(high) << 8 | u32::from(low)) << (16 - self.count);
            self.count += 16;
        }
    }

    #[inline]
    fn window(&self) -> u16 {
        (self.buffer >> 16) as u16
    }

    #[inline]
    fn consume(&mut self, bits: u32) {
        self.buffer <<= bits;
        self.count -= bits;
    }

    fn bytes<const N: usize>(&mut self) -> Coded<[u8; N]> {
        let bytes = self.data.get(self.at..).and_then(|rest| rest.first_chunk());
        self.at += N;
        bytes.copied().ok_or(CUT)
    }
}

/// Decompresses one chunk: `output` is as long as the chunk was before
/// compression.
pub(crate) fn decode(input: &[u8], output: &mut [u8]) -> Coded<()> {
    let (table, data) = input.split_at_checked(TABLE).ok_or(CUT)?;
    let mut lengths = [0u8; SYMBOLS];
    for (pair, byte) in lengths.as_chunks_mut::<2>().0.iter_mut().zip(table) {
        *pair = [byte & 0xF, byte >> 4];
    }
    let code = Huffman::new(&lengths).ok_or(BAD_TREE)?;
    let mut words = Words {
        data,
        at: 0,
        buffer: 0,
        count: 0,
    };
    let mut at = 0;
    while at < output.len() {
        words.fill();
        let (symbol, used) = code.decode(words.window()).ok_or(BAD_CODE)?;
        words.consume(used);
        if symbol < BYTES {
            output[at] = symbol as u8;
            at += 1;
            continue;
        }
        let order = u32::from(symbol >> 4 & 0xF);
        let mut length = usize::from(symbol & 0xF);
        // The bits of the distance are in the buffer before the bytes of
        // the length are read, though they come after them in the count.
        words.fill();
        let distance = (1usize << order) | (usize::from(words.window()) >> ((16 - order) % 16));
        let distance = if order == 0 { 1 } else { distance };
        words.consume(order);
        if length == LONG {
            length += usize::from(words.bytes::<1>()?[0]);
            if length == LONGER {
                length = usize::from(u16::from_le_bytes(words.bytes()?));
                if length == 0 {
                    length = u32::from_le_bytes(words.bytes()?) as usize;
                }
            }
        }
        let length = length.checked_add(MIN_MATCH).ok_or(BAD_MATCH)?;
        if distance > at || length > output.len() - at {
            return Err(BAD_MATCH);
        }
        if distance >= length {
            output.copy_within(at - distance..at - distance + length, at);
        } else {
            for to in at..at + length {
                output[to] = output[to - distance];
            }
        }
        at += length;
    }
    if words.at > data.len() + 4 {
        return Err(CUT);
    }
    Ok(())
}

#[cfg(test)]
pub(crate) mod tests {
    use proptest::prelude::*;

    use super::*;
    use crate::tests::{code, lines, pattern, table};

    /// Writes what `decode` reads. `items` are bytes and matches, the
    /// lengths of the codewords are the same for all: nine bits.
    /// What a chunk is made of.
    pub(crate) enum Item {
        Byte(u8),
        Match { distance: usize, length: usize },
    }

    /// Writes what `decode` reads. Every codeword is nine bits long, so
    /// a chunk is smaller than its data only for its matches.
    pub(crate) fn pack(items: &[Item]) -> Vec<u8> {
        let mut packed = vec![0x99u8; TABLE];
        let mut bits: Vec<bool> = Vec::new();
        // The reader takes a word whenever fewer than sixteen bits are left
        // to it. The writer keeps the reader's count, and so knows where
        // the words lie among the bytes.
        let mut words: Vec<usize> = Vec::new();
        let mut left = 0u32;
        let mut word = |packed: &mut Vec<u8>, left: &mut u32| {
            if *left < 16 {
                words.push(packed.len());
                packed.extend_from_slice(&[0, 0]);
                *left += 16;
            }
        };
        for item in items {
            word(&mut packed, &mut left);
            match item {
                Item::Byte(byte) => push(&mut bits, u32::from(*byte), 9),
                Item::Match { distance, length } => {
                    let order = distance.ilog2();
                    let rest = length - MIN_MATCH;
                    push(&mut bits, 256 + (order << 4) + rest.min(LONG) as u32, 9);
                    left -= 9;
                    word(&mut packed, &mut left);
                    push(&mut bits, *distance as u32 & ((1 << order) - 1), order);
                    left -= order;
                    if rest >= LONGER {
                        packed.push(255);
                        if let Ok(rest) = u16::try_from(rest) {
                            packed.extend_from_slice(&rest.to_le_bytes());
                        } else {
                            packed.extend_from_slice(&[0, 0]);
                            packed.extend_from_slice(&(rest as u32).to_le_bytes());
                        }
                    } else if rest >= LONG {
                        packed.push((rest - LONG) as u8);
                    }
                    continue;
                }
            }
            left -= 9;
        }
        for (place, word) in words.iter().zip(bits.chunks(16)) {
            let mut value = 0u16;
            for (at, bit) in word.iter().enumerate() {
                value |= u16::from(*bit) << (15 - at);
            }
            packed[*place..*place + 2].copy_from_slice(&value.to_le_bytes());
        }
        packed
    }

    fn push(bits: &mut Vec<bool>, value: u32, count: u32) {
        bits.extend((0..count).rev().map(|at| value >> at & 1 == 1));
    }

    fn unpack(items: &[Item]) -> Vec<u8> {
        let mut plain = Vec::new();
        for item in items {
            match item {
                Item::Byte(byte) => plain.push(*byte),
                Item::Match { distance, length } => {
                    for _ in 0..*length {
                        plain.push(plain[plain.len() - distance]);
                    }
                }
            }
        }
        plain
    }

    fn fixture(packed: &[u8], plain: &[u8]) {
        let mut output = vec![0u8; plain.len()];
        decode(packed, &mut output).unwrap();
        assert!(output == plain);
    }

    #[test]
    fn what_windows_compresses_is_read() {
        fixture(include_bytes!("../fixtures/text.xpress"), &lines(700));
        fixture(
            include_bytes!("../fixtures/pattern.xpress"),
            &pattern(32768),
        );
        fixture(
            include_bytes!("../fixtures/zeros.xpress"),
            &vec![0u8; 32768],
        );
        fixture(include_bytes!("../fixtures/code.xpress"), &code(48000));
        fixture(include_bytes!("../fixtures/table.xpress"), &table(8000));
    }

    #[test]
    fn lengths_of_every_form_are_read() {
        for length in [3, 17, 18, 19, 272, 273, 274, 65_535, 65_538, 65_539, 70_000] {
            let items = [
                Item::Byte(b'a'),
                Item::Byte(b'b'),
                Item::Match {
                    distance: 2,
                    length,
                },
                Item::Byte(b'c'),
                Item::Match {
                    distance: 1,
                    length: 5,
                },
                Item::Match {
                    distance: (length + 2).min(60_000),
                    length: 4,
                },
            ];
            let plain = unpack(&items);
            let mut output = vec![0u8; plain.len()];
            decode(&pack(&items), &mut output).unwrap();
            assert!(output == plain, "a match of {length}");
        }
    }

    #[test]
    fn what_is_broken_is_said_to_be_broken() {
        let items = [
            Item::Byte(b'a'),
            Item::Match {
                distance: 1,
                length: 300,
            },
        ];
        let packed = pack(&items);
        let mut output = vec![0u8; 301];
        decode(&packed, &mut output).unwrap();
        // No table, or not all of it.
        assert_eq!(decode(&[], &mut output), Err(CUT));
        assert_eq!(decode(&packed[..TABLE - 1], &mut output), Err(CUT));
        // The bytes of a length are not there.
        assert_eq!(decode(&packed[..packed.len() - 3], &mut output), Err(CUT));
        // More codewords than there can be.
        let mut crowded = packed.clone();
        crowded[..TABLE].fill(0x11);
        assert_eq!(decode(&crowded, &mut output), Err(BAD_TREE));
        // A codeword of no symbol.
        let mut sparse = packed.clone();
        sparse[..TABLE].fill(0);
        sparse[0] = 0x01;
        sparse[TABLE] = 0xFF;
        sparse[TABLE + 1] = 0xFF;
        assert_eq!(decode(&sparse, &mut output), Err(BAD_CODE));
        // A match longer than what is left to write, and one that starts
        // before the chunk does.
        assert_eq!(decode(&packed, &mut output[..300]), Err(BAD_MATCH));
        let early = pack(&[
            Item::Byte(b'a'),
            Item::Match {
                distance: 2,
                length: 3,
            },
        ]);
        assert_eq!(decode(&early, &mut output[..4]), Err(BAD_MATCH));
    }

    proptest! {
        #[test]
        fn what_is_written_is_read(
            raw in proptest::collection::vec((any::<u8>(), 0usize..70_000, 3usize..400, any::<bool>()), 1..200),
        ) {
            let mut items = Vec::new();
            let mut size = 0usize;
            for (byte, distance, length, is_match) in raw {
                if is_match && size > 0 {
                    let distance = 1 + distance % size.min(65_535);
                    items.push(Item::Match { distance, length });
                    size += length;
                } else {
                    items.push(Item::Byte(byte));
                    size += 1;
                }
            }
            let plain = unpack(&items);
            let mut output = vec![0u8; plain.len()];
            prop_assert_eq!(decode(&pack(&items), &mut output), Ok(()));
            prop_assert!(output == plain);
        }

        #[test]
        fn no_data_makes_it_panic(
            data in proptest::collection::vec(any::<u8>(), 0..600),
            size in 0usize..5000,
        ) {
            let mut output = vec![0u8; size];
            let _ = decode(&data, &mut output);
        }
    }
}

//! Prefix codes as LZX, XPRESS and LZMS use them.
//!
//! A code is told by the lengths of its codewords alone: the codewords are
//! numbered in the order of their length and, within a length, of their
//! symbols, the shortest first and the first of all made of zeros. The
//! streams give the bits of a codeword from the highest one down.

/// What a decoder gives: the data, or what is wrong with it.
pub(crate) type Coded<T> = std::result::Result<T, &'static str>;

/// The longest codeword any of the formats has.
pub(crate) const MAX_LENGTH: usize = 16;
/// The codewords of up to this many bits are found by one look into a table.
const FAST_BITS: usize = 10;
/// The most symbols a code may have: a symbol and the length of its
/// codeword share an entry of the table.
pub(crate) const MAX_SYMBOLS: usize = 1 << 12;

/// A prefix code, ready to tell the symbol the next bits of a stream stand
/// for.
#[derive(Clone)]
pub(crate) struct Huffman {
    /// By the next `FAST_BITS` bits: a symbol and the length of its
    /// codeword, or zero where the codeword is longer or is none.
    fast: Vec<u16>,
    /// By length: where the codewords of this length end among all sixteen
    /// bit numbers, a codeword taken as the high bits of one.
    limit: [u32; MAX_LENGTH + 1],
    /// By length: the place of the first symbol of this length in `sorted`
    /// less its codeword.
    base: [i32; MAX_LENGTH + 1],
    /// The symbols in the order of their codewords.
    sorted: Vec<u16>,
    longest: usize,
}

impl Huffman {
    /// The code with these lengths of codewords; zero for a symbol that has
    /// none. `None` for lengths no code can have: more codewords than there
    /// are numbers of their lengths.
    pub(crate) fn new(lengths: &[u8]) -> Option<Self> {
        if lengths.len() > MAX_SYMBOLS {
            return None;
        }
        let mut count = [0u32; MAX_LENGTH + 1];
        for length in lengths {
            *count.get_mut(usize::from(*length))? += 1;
        }
        count[0] = 0;
        let longest = (1..=MAX_LENGTH).rev().find(|length| count[*length] > 0);
        let longest = longest.unwrap_or(0);

        let mut first = [0u32; MAX_LENGTH + 2];
        let mut limit = [0u32; MAX_LENGTH + 1];
        let mut base = [0i32; MAX_LENGTH + 1];
        let mut start = [0u32; MAX_LENGTH + 2];
        for length in 1..=MAX_LENGTH {
            let end = first[length] + count[length];
            if end > 1 << length {
                return None;
            }
            limit[length] = end << (MAX_LENGTH - length);
            base[length] = start[length] as i32 - first[length] as i32;
            first[length + 1] = end << 1;
            start[length + 1] = start[length] + count[length];
        }

        let mut sorted = vec![0u16; start[MAX_LENGTH + 1] as usize];
        let mut fast = vec![0u16; 1 << FAST_BITS];
        let mut next = start;
        for (symbol, length) in lengths.iter().enumerate() {
            let length = usize::from(*length);
            if length == 0 {
                continue;
            }
            let place = next[length];
            next[length] += 1;
            sorted[place as usize] = symbol as u16;
            if length <= FAST_BITS {
                let code = (place as i32 - base[length]) as usize;
                let from = code << (FAST_BITS - length);
                let to = (code + 1) << (FAST_BITS - length);
                fast[from..to].fill((symbol as u16) << 4 | length as u16);
            }
        }
        Some(Self {
            fast,
            limit,
            base,
            sorted,
            longest,
        })
    }

    /// The symbol the next bits of a stream stand for, and how many of them
    /// its codeword takes. `window` holds the next sixteen bits, the first
    /// one highest. `None` when they start no codeword.
    #[inline]
    pub(crate) fn decode(&self, window: u16) -> Option<(u16, u32)> {
        let entry = self.fast[usize::from(window >> (MAX_LENGTH - FAST_BITS))];
        if entry != 0 {
            return Some((entry >> 4, u32::from(entry & 0xF)));
        }
        let bits = u32::from(window);
        for length in FAST_BITS + 1..=self.longest {
            if bits < self.limit[length] {
                let place = self.base[length] + (bits >> (MAX_LENGTH - length)) as i32;
                return Some((self.sorted[place as usize], length as u32));
            }
        }
        None
    }
}

#[cfg(test)]
mod tests {
    use proptest::prelude::*;

    use super::*;

    /// The codewords of a code, by symbol: the number and its length.
    fn codewords(lengths: &[u8]) -> Vec<Option<(u32, u8)>> {
        let mut order: Vec<usize> = (0..lengths.len()).filter(|at| lengths[*at] > 0).collect();
        order.sort_by_key(|at| (lengths[*at], *at));
        let mut codes = vec![None; lengths.len()];
        let mut code = 0u32;
        let mut previous = 0u8;
        for symbol in order {
            let length = lengths[symbol];
            code <<= length - previous;
            previous = length;
            codes[symbol] = Some((code, length));
            code += 1;
        }
        codes
    }

    fn window(code: u32, length: u8, rest: u16) -> u16 {
        let shifted = (code << (MAX_LENGTH - usize::from(length))) as u16;
        let mask = ((1u32 << (MAX_LENGTH - usize::from(length))) - 1) as u16;
        shifted | (rest & mask)
    }

    #[test]
    fn a_small_code_is_read() {
        // 2 -> 0, 6 -> 10, 3 -> 110, 4 -> 1110, 1 -> 11110, 0 -> 111110,
        // 5 -> 111111.
        let code = Huffman::new(&[6, 5, 1, 3, 4, 6, 2, 0]).unwrap();
        assert_eq!(code.decode(0b0000_0000_0000_0000), Some((2, 1)));
        assert_eq!(code.decode(0b0111_1111_1111_1111), Some((2, 1)));
        assert_eq!(code.decode(0b1000_0000_0000_0000), Some((6, 2)));
        assert_eq!(code.decode(0b1101_0101_0101_0101), Some((3, 3)));
        assert_eq!(code.decode(0b1110_1111_1111_1111), Some((4, 4)));
        assert_eq!(code.decode(0b1111_0000_0000_0000), Some((1, 5)));
        assert_eq!(code.decode(0b1111_1000_0000_0000), Some((0, 6)));
        assert_eq!(code.decode(0b1111_1111_1111_1111), Some((5, 6)));
    }

    #[test]
    fn what_no_code_can_be_is_refused() {
        assert!(Huffman::new(&[1, 1, 1]).is_none());
        assert!(Huffman::new(&[2, 2, 2, 2, 16]).is_none());
        assert!(Huffman::new(&[17]).is_none());
        assert!(Huffman::new(&vec![12; MAX_SYMBOLS + 1]).is_none());
        // A code with numbers to spare reads what it has and nothing else.
        let spare = Huffman::new(&[2, 0, 2]).unwrap();
        assert_eq!(spare.decode(0x0000), Some((0, 2)));
        assert_eq!(spare.decode(0x4000), Some((2, 2)));
        assert_eq!(spare.decode(0x8000), None);
        let empty = Huffman::new(&[0, 0, 0]).unwrap();
        assert_eq!(empty.decode(0), None);
        assert_eq!(empty.decode(0xFFFF), None);
    }

    #[test]
    fn the_longest_codewords_are_read() {
        // One codeword of every length, and two of the longest.
        let mut lengths: Vec<u8> = (1..=16).collect();
        lengths.push(16);
        let code = Huffman::new(&lengths).unwrap();
        for (symbol, entry) in codewords(&lengths).into_iter().enumerate() {
            let (word, length) = entry.unwrap();
            assert_eq!(
                code.decode(window(word, length, 0xFFFF)),
                Some((symbol as u16, u32::from(length)))
            );
        }
    }

    proptest! {
        #[test]
        fn every_codeword_of_a_code_gives_its_symbol(
            raw in proptest::collection::vec(0u8..=16, 1..600),
            rest in any::<u16>(),
        ) {
            // Lengths are taken while there are numbers left for them.
            let mut room = 1u32 << MAX_LENGTH;
            let lengths: Vec<u8> = raw
                .iter()
                .map(|length| {
                    let cost = 1u32 << (MAX_LENGTH - usize::from(*length));
                    if *length > 0 && cost <= room {
                        room -= cost;
                        *length
                    } else {
                        0
                    }
                })
                .collect();
            let code = Huffman::new(&lengths).unwrap();
            for (symbol, entry) in codewords(&lengths).into_iter().enumerate() {
                if let Some((word, length)) = entry {
                    prop_assert_eq!(
                        code.decode(window(word, length, rest)),
                        Some((symbol as u16, u32::from(length)))
                    );
                }
            }
        }
    }
}

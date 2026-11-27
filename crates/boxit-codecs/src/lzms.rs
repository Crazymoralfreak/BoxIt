//! LZMS, the strongest compression of the images of Windows.
//!
//! A chunk is read from both ends at once. From the start come the bits of
//! a range coder: what kind the next item is, and whether a match repeats a
//! recent distance. From the end, going backwards, come the codewords of
//! bytes, lengths and distances and the bits that follow them. The codes
//! are not written anywhere: both sides count the symbols and make the
//! codes anew from the counts every so many symbols.
//!
//! Besides the usual matches there are those of differences: a byte is
//! what the bytes one span and one distance back make of the byte both
//! spans back. After everything the calls of x86 code are made relative
//! again, as LZX does, only here the compressor guesses where the code is.

use crate::huffman::{Coded, Huffman};

/// The chance of a zero bit is counted in sixty-fourths: how many zeros
/// were among the last sixty-four bits of the kind.
const HISTORY: u32 = 64;
const HISTORY_ORDER: u32 = 6;
/// The bits a chance starts with: 48 zeros of 64.
const FIRST_HISTORY: u64 = 0x0000_0000_5555_5555;
const FIRST_ZEROS: u32 = 48;

/// The longest codeword.
const LONGEST: usize = 15;
const BYTES: usize = 256;
const LENGTH_SLOTS: usize = 54;
const OFFSET_SLOTS: usize = 799;
const POWERS: usize = 8;
/// After how many symbols a code is made anew.
const OFTEN: u32 = 512;
const SELDOM: u32 = 1024;

/// How many recent distances a match may repeat.
const RECENT: usize = 3;

/// The filter of x86 code leaves the last bytes alone.
const TAIL: usize = 16;
/// A distance is translated when a target was seen again this near.
const NEAR: i32 = 1023;
/// A target seen again counts when it was seen this near.
const SEEN: i32 = 65535;

const BAD_CODE: &str = "LZMS: a codeword that stands for nothing";
const BAD_MATCH: &str = "LZMS: a match with what was never written";
const BAD_SIZE: &str = "LZMS: a chunk of a size no chunk has";

/// The first value of every slot and the bits that follow its codeword.
/// The slots are told by runs: so many slots a value apart, so many twice
/// as far apart, and so on; `last` is where the last slot ends.
const fn slots<const N: usize>(runs: &[u8], last: u32) -> ([u32; N], [u8; N]) {
    let mut base = [0u32; N];
    let mut extra = [0u8; N];
    let mut slot = 0;
    let mut value = 0u32;
    let mut order = 0u8;
    let mut run = 0;
    while run < runs.len() {
        let mut left = runs[run];
        while left > 0 {
            value += 1 << order;
            base[slot] = value;
            if slot > 0 {
                extra[slot - 1] = order;
            }
            slot += 1;
            left -= 1;
        }
        order += 1;
        run += 1;
    }
    assert!(slot == N);
    extra[N - 1] = (last - base[N - 1]).ilog2() as u8;
    (base, extra)
}

const OFFSETS: ([u32; OFFSET_SLOTS], [u8; OFFSET_SLOTS]) = slots(
    &[
        9, 0, 9, 7, 10, 15, 15, 20, 20, 30, 33, 40, 42, 45, 60, 73, 80, 85, 95, 105, 6,
    ],
    0x7fff_ffff,
);

const LENGTHS: ([u32; LENGTH_SLOTS], [u8; LENGTH_SLOTS]) = slots(
    &[27, 4, 6, 4, 5, 2, 1, 1, 1, 1, 1, 0, 0, 0, 0, 0, 1],
    0x4001_08ab,
);

/// How many slots of distances a chunk of this size can use.
fn offset_slots(size: usize) -> usize {
    let furthest = size.saturating_sub(1) as u64;
    OFFSETS
        .0
        .partition_point(|base| u64::from(*base) <= furthest)
}

/// The chances of the bits of one kind: one for every run of bits of the
/// kind that came before.
struct Chances<const N: usize> {
    state: usize,
    zeros: [u32; N],
    history: [u64; N],
}

impl<const N: usize> Chances<N> {
    fn new() -> Self {
        Self {
            state: 0,
            zeros: [FIRST_ZEROS; N],
            history: [FIRST_HISTORY; N],
        }
    }
}

/// The range coder at the start of a chunk.
struct Range<'a> {
    data: &'a [u8],
    at: usize,
    width: u32,
    code: u32,
}

impl<'a> Range<'a> {
    fn new(data: &'a [u8]) -> Self {
        let word = |at: usize| u32::from(u16::from_le_bytes([data[at], data[at + 1]]));
        Self {
            data,
            at: 4,
            width: u32::MAX,
            code: word(0) << 16 | word(2),
        }
    }

    #[inline]
    fn bit<const N: usize>(&mut self, chances: &mut Chances<N>) -> bool {
        let state = chances.state;
        if self.width <= 0xFFFF {
            self.width <<= 16;
            self.code <<= 16;
            if let Some(word) = self.data.get(self.at..self.at + 2) {
                self.code |= u32::from(u16::from_le_bytes([word[0], word[1]]));
                self.at += 2;
            }
        }
        // Neither bit is ever taken for impossible.
        let chance = chances.zeros[state].clamp(1, HISTORY - 1);
        let bound = (self.width >> HISTORY_ORDER) * chance;
        let bit = self.code >= bound;
        if bit {
            self.width -= bound;
            self.code -= bound;
        } else {
            self.width = bound;
        }
        // The bit takes the place of the oldest one of the history.
        let history = chances.history[state];
        chances.zeros[state] =
            chances.zeros[state] + (history >> (HISTORY - 1)) as u32 - u32::from(bit);
        chances.history[state] = history << 1 | u64::from(bit);
        chances.state = (state << 1 | usize::from(bit)) & (N - 1);
        bit
    }
}

/// The bits at the end of a chunk: 16-bit words taken from the last one
/// backwards, the highest bit of a word first.
struct Back<'a> {
    data: &'a [u8],
    /// The bytes not yet taken.
    left: usize,
    buffer: u64,
    count: u32,
}

impl<'a> Back<'a> {
    fn new(data: &'a [u8]) -> Self {
        Self {
            data,
            left: data.len(),
            buffer: 0,
            count: 0,
        }
    }

    /// Thirty-two bits at least are in the buffer after this; zeros before
    /// the start of the data.
    #[inline]
    fn fill(&mut self) {
        while self.count <= 32 {
            if self.left >= 2 {
                self.left -= 2;
                let word = u16::from_le_bytes([self.data[self.left], self.data[self.left + 1]]);
                self.buffer |= u64::from(word) << (48 - self.count);
            }
            self.count += 16;
        }
    }

    /// The next bits as a number, thirty-two at most.
    #[inline]
    fn take(&mut self, bits: u32) -> u32 {
        if bits == 0 {
            return 0;
        }
        self.fill();
        let value = (self.buffer >> (64 - bits)) as u32;
        self.buffer <<= bits;
        self.count -= bits;
        value
    }
}

/// The lengths of the codewords of the code that suits these counts of
/// symbols, none of them zero. The code is the one of Huffman, made in the
/// one way both sides have to make it: of two equal counts the symbol
/// comes before the pair, and what would be longer than the longest
/// codeword takes the place of the deepest shorter one.
fn weigh(counts: &[u32], lengths: &mut [u8]) {
    let symbols = counts.len();
    if symbols < 2 {
        lengths[..symbols].fill(1);
        return;
    }
    let mut order = [0u16; OFFSET_SLOTS];
    let order = &mut order[..symbols];
    for (at, symbol) in order.iter_mut().enumerate() {
        *symbol = at as u16;
    }
    order.sort_unstable_by_key(|symbol| (counts[usize::from(*symbol)], *symbol));

    // The pairs come to be in the order of their weights, so the lightest
    // of all is the next symbol or the next pair that has no parent yet.
    let mut weights = [0u32; OFFSET_SLOTS];
    let mut parents = [0u16; OFFSET_SLOTS];
    let (mut symbol, mut pair) = (0, 0);
    for made in 0..symbols - 1 {
        let mut weight = 0;
        for _ in 0..2 {
            let lighter = symbol < symbols
                && (pair == made || counts[usize::from(order[symbol])] <= weights[pair]);
            if lighter {
                weight += counts[usize::from(order[symbol])];
                symbol += 1;
            } else {
                weight += weights[pair];
                parents[pair] = made as u16;
                pair += 1;
            }
        }
        weights[made] = weight;
    }

    // How many codewords of every length there are: from the last pair,
    // which is the root, down. A pair turns one codeword into two longer
    // ones.
    let mut depths = [0u16; OFFSET_SLOTS];
    let mut of_length = [0u16; LONGEST + 2];
    of_length[1] = 2;
    for pair in (0..symbols - 2).rev() {
        let depth = depths[usize::from(parents[pair])] + 1;
        depths[pair] = depth;
        let mut length = usize::from(depth);
        if length >= LONGEST {
            length = LONGEST - 1;
            while of_length[length] == 0 {
                length -= 1;
            }
        }
        of_length[length] -= 1;
        of_length[length + 1] += 2;
    }

    // The rarest symbols get the longest codewords.
    let mut next = order.iter();
    for length in (1..=LONGEST).rev() {
        for symbol in next.by_ref().take(usize::from(of_length[length])) {
            lengths[usize::from(*symbol)] = length as u8;
        }
    }
}

/// A code that follows the counts of the symbols it has given.
struct Adaptive {
    counts: Vec<u32>,
    code: Option<Huffman>,
    /// How many symbols are left to the making of the next code.
    left: u32,
    every: u32,
}

impl Adaptive {
    fn new(symbols: usize, every: u32) -> Self {
        let counts = vec![1u32; symbols];
        let code = Self::code(&counts);
        Self {
            counts,
            code,
            left: every,
            every,
        }
    }

    fn code(counts: &[u32]) -> Option<Huffman> {
        let mut lengths = [0u8; OFFSET_SLOTS];
        let lengths = &mut lengths[..counts.len()];
        weigh(counts, lengths);
        Huffman::new(lengths)
    }

    #[inline]
    fn symbol(&mut self, bits: &mut Back) -> Coded<usize> {
        bits.fill();
        let window = (bits.buffer >> 48) as u16;
        let code = self.code.as_ref().ok_or(BAD_CODE)?;
        let (symbol, used) = code.decode(window).ok_or(BAD_CODE)?;
        bits.buffer <<= used;
        bits.count -= used;
        let symbol = usize::from(symbol);
        self.counts[symbol] += 1;
        self.left -= 1;
        if self.left == 0 {
            self.code = Self::code(&self.counts);
            // What was long ago weighs half of what it did.
            for count in &mut self.counts {
                *count = (*count >> 1) + 1;
            }
            self.left = self.every;
        }
        Ok(symbol)
    }

    /// A number: the codeword of its slot and the bits after it.
    #[inline]
    fn number<const N: usize>(
        &mut self,
        bits: &mut Back,
        slots: &([u32; N], [u8; N]),
    ) -> Coded<u32> {
        let slot = self.symbol(bits)?;
        Ok(slots.0[slot] + bits.take(u32::from(slots.1[slot])))
    }
}

/// The recent distances of the matches of one kind. A distance joins them
/// one item late: the item after its match cannot repeat it.
struct Recent<T> {
    /// One more than can be named: the last one moves up when another is
    /// taken out.
    queue: [T; RECENT + 1],
    /// The distance of the item before, if that was a match of the kind.
    last: Option<T>,
}

impl<T: Copy> Recent<T> {
    fn new(queue: [T; RECENT + 1]) -> Self {
        Self { queue, last: None }
    }

    /// Takes one of the recent distances out.
    fn repeat(&mut self, which: usize) -> T {
        let value = self.queue[which];
        self.queue.copy_within(which + 1.., which);
        value
    }

    /// An item has passed: `used` is the distance of its match, if it was
    /// one of the kind.
    #[inline]
    fn pass(&mut self, used: Option<T>) {
        if let Some(last) = self.last {
            self.queue.copy_within(..RECENT, 1);
            self.queue[0] = last;
        }
        self.last = used;
    }
}

/// Decompresses one chunk: `output` is as long as the chunk was before
/// compression.
pub(crate) fn decode(input: &[u8], output: &mut [u8]) -> Coded<()> {
    unpack(input, output)?;
    restore_calls(output);
    Ok(())
}

/// The chunk as the compressor saw it: with the calls of x86 code
/// rewritten.
fn unpack(input: &[u8], output: &mut [u8]) -> Coded<()> {
    if input.len() < 4 || !input.len().is_multiple_of(2) || i32::try_from(output.len()).is_err() {
        return Err(BAD_SIZE);
    }
    let mut range = Range::new(input);
    let mut bits = Back::new(input);

    let mut is_match = Chances::<16>::new();
    let mut is_delta = Chances::<32>::new();
    let mut lz_repeats = Chances::<64>::new();
    let mut lz_which = [Chances::<64>::new(), Chances::<64>::new()];
    let mut delta_repeats = Chances::<64>::new();
    let mut delta_which = [Chances::<64>::new(), Chances::<64>::new()];

    let offsets = offset_slots(output.len());
    let mut bytes = Adaptive::new(BYTES, SELDOM);
    let mut lz_offsets = Adaptive::new(offsets, SELDOM);
    let mut lengths = Adaptive::new(LENGTH_SLOTS, OFTEN);
    let mut delta_offsets = Adaptive::new(offsets, SELDOM);
    let mut powers = Adaptive::new(POWERS, OFTEN);

    let mut lz = Recent::new([1u32, 2, 3, 4]);
    let mut delta = Recent::new([(0u32, 1u32), (0, 2), (0, 3), (0, 4)]);

    let mut at = 0;
    while at < output.len() {
        if !range.bit(&mut is_match) {
            output[at] = bytes.symbol(&mut bits)? as u8;
            at += 1;
            lz.pass(None);
            delta.pass(None);
            continue;
        }
        let left = output.len() - at;
        if range.bit(&mut is_delta) {
            let (power, raw) = if !range.bit(&mut delta_repeats) {
                let power = powers.symbol(&mut bits)? as u32;
                (power, delta_offsets.number(&mut bits, &OFFSETS)?)
            } else if !range.bit(&mut delta_which[0]) {
                delta.repeat(0)
            } else if !range.bit(&mut delta_which[1]) {
                delta.repeat(1)
            } else {
                delta.repeat(2)
            };
            let length = lengths.number(&mut bits, &LENGTHS)? as usize;
            lz.pass(None);
            delta.pass(Some((power, raw)));
            let span = 1usize << power;
            let distance = (u64::from(raw) << power) as usize;
            let both = distance.checked_add(span).ok_or(BAD_MATCH)?;
            if both > at || length > left {
                return Err(BAD_MATCH);
            }
            for to in at..at + length {
                output[to] = output[to - distance]
                    .wrapping_add(output[to - span])
                    .wrapping_sub(output[to - both]);
            }
            at += length;
        } else {
            let distance = if !range.bit(&mut lz_repeats) {
                lz_offsets.number(&mut bits, &OFFSETS)?
            } else if !range.bit(&mut lz_which[0]) {
                lz.repeat(0)
            } else if !range.bit(&mut lz_which[1]) {
                lz.repeat(1)
            } else {
                lz.repeat(2)
            };
            let length = lengths.number(&mut bits, &LENGTHS)? as usize;
            lz.pass(Some(distance));
            delta.pass(None);
            let distance = distance as usize;
            if distance > at || length > left {
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
    }
    Ok(())
}

/// Whether an instruction that names a place by its distance starts here:
/// where in it the distance is, and how near the last sign of code has to
/// be for the distance to have been rewritten.
#[inline]
fn instruction(code: &[u8]) -> Option<(usize, i32)> {
    match code {
        // lea of x86-64 with an address that is told by its distance.
        [0x48 | 0x4C, 0x8D, register, ..] if register & 7 == 5 => Some((3, NEAR)),
        // mov with such an address, and lock add.
        [0x48, 0x8B, 0x05 | 0x0D, ..] | [0xF0, 0x83, 0x05, ..] => Some((3, NEAR)),
        // A call: there are more bytes like its first one than calls, so
        // it takes more to believe in one.
        [0xE8, ..] => Some((1, NEAR / 2)),
        // A call through a table.
        [0xFF, 0x15, ..] => Some((2, NEAR)),
        _ => None,
    }
}

/// Makes the distances in x86 code distances again: the compressor wrote
/// the places they lead to instead, wherever it took the data for code. It
/// does so when a target was named twice within a short stretch not long
/// before; the same guess is made here from the same bytes.
fn restore_calls(data: &mut [u8]) {
    if data.len() <= TAIL + 1 {
        return;
    }
    let end = (data.len() - TAIL) as i32;
    // By the low bits of a target: where it was named last.
    let mut named = vec![-SEEN - 1; 1 << 16];
    let mut believed = -NEAR - 1;
    let mut at = 1i32;
    while at < end {
        let here = at as usize;
        let Some((skip, near)) = instruction(&data[here..here + 3]) else {
            // A jump is passed over whole, and changes nothing.
            at += if data[here] == 0xE9 { 5 } else { 1 };
            continue;
        };
        let field = here + skip;
        let mut value = u32::from_le_bytes([
            data[field],
            data[field + 1],
            data[field + 2],
            data[field + 3],
        ]);
        if i64::from(at) - i64::from(believed) <= i64::from(near) {
            value = value.wrapping_sub(at as u32);
            data[field..field + 4].copy_from_slice(&value.to_le_bytes());
        }
        let target = usize::from((at as u16).wrapping_add(value as u16));
        at += skip as i32 + 3;
        if i64::from(at) - i64::from(named[target]) <= i64::from(SEEN) {
            believed = at;
        }
        named[target] = at;
        at += 1;
    }
}

#[cfg(test)]
mod tests {
    use proptest::prelude::*;

    use super::*;
    use crate::tests::{code, lines, pattern, table};

    fn fixture(packed: &[u8], plain: &[u8]) {
        let mut output = vec![0u8; plain.len()];
        decode(packed, &mut output).unwrap();
        if let Some(at) = output
            .iter()
            .zip(plain)
            .position(|(one, other)| one != other)
        {
            panic!("differs at {at} of {}", plain.len());
        }
    }

    #[test]
    fn the_slots_are_those_of_the_format() {
        let (base, extra) = OFFSETS;
        assert_eq!(base[..10], [1, 2, 3, 4, 5, 6, 7, 8, 9, 13]);
        assert_eq!(extra[..10], [0, 0, 0, 0, 0, 0, 0, 0, 2, 2]);
        for slot in 0..OFFSET_SLOTS - 1 {
            assert_eq!(base[slot] + (1 << extra[slot]), base[slot + 1]);
        }
        assert_eq!(extra[OFFSET_SLOTS - 1], 30);
        let (base, extra) = LENGTHS;
        assert_eq!(base[26..30], [27, 29, 31, 33]);
        for slot in 0..LENGTH_SLOTS - 1 {
            assert_eq!(base[slot] + (1 << extra[slot]), base[slot + 1]);
        }
        assert_eq!(extra[LENGTH_SLOTS - 1], 30);

        assert_eq!(offset_slots(0), 0);
        assert_eq!(offset_slots(1), 0);
        assert_eq!(offset_slots(2), 1);
        assert_eq!(offset_slots(10), 9);
        assert_eq!(offset_slots(14), 10);
        assert_eq!(offset_slots(usize::MAX), OFFSET_SLOTS);
    }

    #[test]
    fn a_code_is_made_of_counts() {
        let lengths = |counts: &[u32]| {
            let mut lengths = vec![0u8; counts.len()];
            weigh(counts, &mut lengths);
            lengths
        };
        assert_eq!(lengths(&[1; 8]), [3; 8]);
        assert_eq!(lengths(&[1]), [1]);
        assert_eq!(lengths(&[5, 1]), [1, 1]);
        assert_eq!(lengths(&[8, 1, 1, 2, 4]), [1, 4, 4, 3, 2]);
        // Of equal weights the symbol goes before the pair.
        assert_eq!(lengths(&[1, 1, 2, 2]), [2, 2, 2, 2]);
        // No codeword is longer than fifteen bits, and the code is whole.
        let steep: Vec<u32> = (0..30).map(|at| 1 << at.min(24)).collect();
        let got = lengths(&steep);
        assert_eq!(got.iter().copied().max(), Some(LONGEST as u8));
        let whole: u32 = got
            .iter()
            .map(|length| 1 << (LONGEST - usize::from(*length)))
            .sum();
        assert_eq!(whole, 1 << LONGEST);
    }

    #[test]
    fn what_windows_compresses_is_read() {
        fixture(include_bytes!("../fixtures/zeros.lzms"), &vec![0u8; 32768]);
        fixture(
            include_bytes!("../fixtures/short.lzms"),
            b"abcabcabcabcabcabc, and again: abcabcabcabcabcabc",
        );
        fixture(include_bytes!("../fixtures/text.lzms"), &lines(700));
        fixture(include_bytes!("../fixtures/pattern.lzms"), &pattern(32768));
        fixture(include_bytes!("../fixtures/table.lzms"), &table(8000));
        fixture(include_bytes!("../fixtures/code.lzms"), &code(48000));
        let text = lines(1200);
        let mixed = [
            &text[..],
            &pattern(20000),
            &table(8000),
            &code(20000),
            &text[..],
        ]
        .concat();
        fixture(include_bytes!("../fixtures/mixed.lzms"), &mixed);
        let text = lines(30000);
        let large = [&text[..], &code(400_000), &text[..]].concat();
        fixture(include_bytes!("../fixtures/large.lzms"), &large);
    }

    #[test]
    fn the_calls_of_code_are_restored_and_text_is_left_alone() {
        let plain = code(48000);
        let mut output = vec![0u8; plain.len()];
        unpack(include_bytes!("../fixtures/code.lzms"), &mut output).unwrap();
        let rewritten = output
            .chunks(64)
            .zip(plain.chunks(64))
            .filter(|(one, other)| one != other)
            .count();
        assert!(rewritten > 600, "{rewritten} of 750 pieces");
        restore_calls(&mut output);
        assert!(output == plain);

        let plain = lines(700);
        let mut output = vec![0u8; plain.len()];
        unpack(include_bytes!("../fixtures/text.lzms"), &mut output).unwrap();
        assert!(output == plain);
        // Too short for the filter to look at.
        let mut short = [0xE8u8; 17];
        restore_calls(&mut short);
        assert_eq!(short, [0xE8; 17]);
    }

    #[test]
    fn what_is_broken_is_said_to_be_broken() {
        let packed = include_bytes!("../fixtures/text.lzms");
        let mut output = vec![0u8; lines(700).len()];
        assert_eq!(decode(&packed[..3], &mut output), Err(BAD_SIZE));
        assert_eq!(
            decode(&packed[..packed.len() - 1], &mut output),
            Err(BAD_SIZE)
        );
        assert_eq!(decode(&[], &mut []), Err(BAD_SIZE));
        // A chunk of nothing needs none of its bytes.
        assert_eq!(decode(&[0; 4], &mut []), Ok(()));
        // Other bytes are other data, or none.
        let mut other = packed.to_vec();
        other[0] ^= 0x40;
        if decode(&other, &mut output).is_ok() {
            assert!(output != lines(700));
        }
    }

    proptest! {
        #[test]
        fn no_data_makes_it_panic(
            data in proptest::collection::vec(any::<u8>(), 0..400),
            size in 0usize..3000,
        ) {
            let mut output = vec![0u8; size];
            let _ = decode(&data, &mut output);
        }
    }
}

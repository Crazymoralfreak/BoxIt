//! LZX as cabinets and images of Windows hold it.
//!
//! The two differ in little. A cabinet compresses a folder as one stream cut
//! into frames of 32 KiB, each in a block of data of its own, and the window
//! lives through all of them. An image compresses every chunk of a file by
//! itself. The sizes of blocks are written differently, and so is the size
//! of the program whose calls were made absolute before compression.

use crate::huffman::{Coded, Huffman};

const MIN_MATCH: usize = 2;
const CHARS: usize = 256;
const PRETREE_SYMBOLS: usize = 20;
const ALIGNED_SYMBOLS: usize = 8;
const LENGTH_SYMBOLS: usize = 249;
/// The size of a frame of a cabinet and the usual size of a block.
pub(crate) const FRAME: usize = 32768;
/// The smallest and the largest windows there are, as powers of two.
const MIN_ORDER: u32 = 15;
const MAX_ORDER: u32 = 21;
/// The slots of offsets of the largest window.
const MAX_SLOTS: usize = 50;
/// A cabinet translates the calls of its first frames only.
const TRANSLATED_FRAMES: u32 = 32768;
/// The size of a program the calls of an image are translated for.
const IMAGE_PROGRAM: i32 = 12_000_000;
/// The byte a call of x86 starts with.
const CALL: u8 = 0xE8;

const VERBATIM: u8 = 1;
const ALIGNED: u8 = 2;
const UNCOMPRESSED: u8 = 3;

const BAD_CODE: &str = "LZX: a codeword that stands for nothing";
const BAD_TREE: &str = "LZX: lengths of codewords that make no code";
const CUT: &str = "LZX: the data ends too early";

/// The extra bits of the slots of offsets.
const EXTRA: [u8; MAX_SLOTS + 1] = {
    let mut extra = [0u8; MAX_SLOTS + 1];
    let mut slot = 0;
    while slot <= MAX_SLOTS {
        let bits = if slot < 4 { 0 } else { slot / 2 - 1 };
        extra[slot] = if bits > 17 { 17 } else { bits as u8 };
        slot += 1;
    }
    extra
};

/// The first offset of every slot, as the format counts them: two more than
/// the distance.
const BASE: [u32; MAX_SLOTS + 1] = {
    let mut base = [0u32; MAX_SLOTS + 1];
    let mut slot = 1;
    while slot <= MAX_SLOTS {
        base[slot] = base[slot - 1] + (1 << EXTRA[slot - 1]);
        slot += 1;
    }
    base
};

/// How many slots of offsets a window of this order has.
fn slots(order: u32) -> usize {
    let furthest = (1u32 << order) - 3;
    let mut slots = 30;
    while slots < MAX_SLOTS && furthest >= BASE[slots] - 2 {
        slots += 1;
    }
    slots
}

/// Bits of a stream of 16-bit words, the highest bit of a word first.
struct Bits<'a> {
    data: &'a [u8],
    /// The next byte to take into the buffer.
    at: usize,
    /// The bits not yet used, from the highest bit down.
    buffer: u32,
    count: u32,
}

impl<'a> Bits<'a> {
    fn new(data: &'a [u8]) -> Self {
        Self {
            data,
            at: 0,
            buffer: 0,
            count: 0,
        }
    }

    /// The next sixteen bits; zeros past the end of the data.
    #[inline]
    fn peek(&mut self) -> u16 {
        if self.count < 16 {
            let low = self.data.get(self.at).copied().unwrap_or(0);
            let high = self.data.get(self.at + 1).copied().unwrap_or(0);
            self.at += 2;
            self.buffer |= (u32::from(high) << 8 | u32::from(low)) << (16 - self.count);
            self.count += 16;
        }
        (self.buffer >> 16) as u16
    }

    /// Uses up bits that `peek` has shown.
    #[inline]
    fn consume(&mut self, bits: u32) {
        self.buffer <<= bits;
        self.count -= bits;
    }

    /// The next bits as a number, sixteen at most.
    #[inline]
    fn take(&mut self, bits: u32) -> u32 {
        if bits == 0 {
            return 0;
        }
        let value = u32::from(self.peek()) >> (16 - bits);
        self.consume(bits);
        value
    }

    /// The next bits as a number, more than sixteen if need be.
    #[inline]
    fn many(&mut self, bits: u32) -> u32 {
        if bits > 16 {
            let high = self.take(16);
            high << (bits - 16) | self.take(bits - 16)
        } else {
            self.take(bits)
        }
    }

    #[inline]
    fn symbol(&mut self, code: &Huffman) -> Coded<usize> {
        let (symbol, used) = code.decode(self.peek()).ok_or(BAD_CODE)?;
        self.consume(used);
        Ok(usize::from(symbol))
    }

    /// Leaves the bits for bytes: what is left of the word in use is
    /// dropped, and a whole word where none was in use.
    fn align(&mut self) {
        if self.count.is_multiple_of(16) {
            self.at = self.at - self.count as usize / 8 + 2;
        } else {
            self.at -= self.count as usize / 16 * 2;
        }
        self.buffer = 0;
        self.count = 0;
    }

    /// Bytes as they are. Only right after `align` or the bytes before.
    fn bytes(&mut self, length: usize) -> Coded<&'a [u8]> {
        let end = self.at.checked_add(length).ok_or(CUT)?;
        let bytes = self.data.get(self.at..end).ok_or(CUT)?;
        self.at = end;
        Ok(bytes)
    }

    /// Whether more bits were used than the data has.
    fn overran(&self) -> bool {
        let whole = self.data.len().div_ceil(2) * 2;
        self.at * 8 > whole * 8 + self.count as usize
    }
}

/// Makes the calls of x86 code relative again: before compression the
/// distance of a call was replaced by the place it leads to, which repeats
/// more often. `position` is where `data` starts in the program and `size`
/// is the size of the program as the compressor took it.
fn restore_calls(data: &mut [u8], position: i32, size: i32) {
    if data.len() <= 10 {
        return;
    }
    let end = data.len() - 10;
    let mut at = 0;
    while at < end {
        if data[at] != CALL {
            at += 1;
            continue;
        }
        let place = position.wrapping_add(at as i32);
        let field = &mut data[at + 1..at + 5];
        let absolute = i32::from_le_bytes([field[0], field[1], field[2], field[3]]);
        if absolute >= place.wrapping_neg() && absolute < size {
            let relative = if absolute >= 0 {
                absolute.wrapping_sub(place)
            } else {
                absolute.wrapping_add(size)
            };
            field.copy_from_slice(&relative.to_le_bytes());
        }
        at += 5;
    }
}

/// A decoder of LZX.
pub(crate) struct Lzx {
    window: Vec<u8>,
    /// Where in the window the next frame goes.
    position: usize,
    /// How much of the window has been written, the whole of it at most.
    filled: usize,
    /// The three offsets used last.
    recent: [usize; 3],
    main_lengths: Vec<u8>,
    length_lengths: [u8; LENGTH_SYMBOLS],
    main: Option<Huffman>,
    lengths: Option<Huffman>,
    aligned: Option<Huffman>,
    /// The block being read and how much of it is left.
    kind: u8,
    left: usize,
    /// An uncompressed block of an odd size has a byte after it.
    odd: bool,
    order: u32,
    cabinet: bool,
    /// Whether the header of the stream of a cabinet has been read.
    started: bool,
    /// The size of the program whose calls were translated; zero for none.
    program: i32,
    frames: u32,
    /// Where the next frame starts in the stream.
    written: i32,
}

impl Lzx {
    fn new(order: u32, cabinet: bool) -> Option<Self> {
        if !(MIN_ORDER..=MAX_ORDER).contains(&order) {
            return None;
        }
        Some(Self {
            window: vec![0; 1 << order],
            position: 0,
            filled: 0,
            recent: [1; 3],
            main_lengths: vec![0; CHARS + 8 * slots(order)],
            length_lengths: [0; LENGTH_SYMBOLS],
            main: None,
            lengths: None,
            aligned: None,
            kind: 0,
            left: 0,
            odd: false,
            order,
            cabinet,
            started: false,
            program: 0,
            frames: 0,
            written: 0,
        })
    }

    /// For the folder of a cabinet with a window of two to the power of
    /// `order`. `None` for a window LZX does not have.
    pub(crate) fn cabinet(order: u32) -> Option<Self> {
        Self::new(order, true)
    }

    /// For the chunks of an image of Windows, each of `chunk` bytes at most.
    pub(crate) fn image(chunk: u32) -> Option<Self> {
        if chunk == 0 {
            return None;
        }
        let order = chunk.checked_next_power_of_two()?.trailing_zeros();
        let mut lzx = Self::new(order.max(MIN_ORDER), false)?;
        lzx.program = IMAGE_PROGRAM;
        Some(lzx)
    }

    /// The largest piece `decode` gives at a time.
    pub(crate) fn most(&self) -> usize {
        if self.cabinet {
            FRAME
        } else {
            self.window.len()
        }
    }

    /// Forgets what was read: the chunks of an image know nothing of each
    /// other.
    fn restart(&mut self) {
        self.position = 0;
        self.filled = 0;
        self.recent = [1; 3];
        self.main_lengths.fill(0);
        self.length_lengths.fill(0);
        self.left = 0;
        self.written = 0;
        self.frames = 0;
    }

    /// Decodes the next frame of a cabinet, or a chunk of an image: `input`
    /// is all of its data and `output` is as long as what it holds.
    pub(crate) fn decode(&mut self, input: &[u8], output: &mut [u8]) -> Coded<()> {
        if !self.cabinet {
            self.restart();
        }
        let size = output.len();
        if size > self.most() || size > self.window.len() - self.position {
            return Err("LZX: more data than a frame holds");
        }
        let mut bits = Bits::new(input);
        if self.cabinet && !self.started {
            self.started = true;
            if bits.take(1) == 1 {
                let high = bits.take(16);
                let low = bits.take(16);
                self.program = (high << 16 | low) as i32;
            }
        }
        let start = self.position;
        let end = start + size;
        let mut at = start;
        while at < end {
            if self.left == 0 {
                self.header(&mut bits)?;
            }
            let run = self.left.min(end - at);
            if self.kind == UNCOMPRESSED {
                self.window[at..at + run].copy_from_slice(bits.bytes(run)?);
            } else {
                self.run(&mut bits, at, at + run, self.filled + (at - start))?;
            }
            at += run;
            self.left -= run;
            if self.left == 0 && self.kind == UNCOMPRESSED && self.odd {
                // The byte that keeps the words in step; nothing asks for it
                // where the data ends with the block.
                let _ = bits.bytes(1);
            }
        }
        if bits.overran() {
            return Err(CUT);
        }
        output.copy_from_slice(&self.window[start..end]);
        self.filled = (self.filled + size).min(self.window.len());
        self.position = end & (self.window.len() - 1);
        if self.program != 0 && self.frames < TRANSLATED_FRAMES {
            restore_calls(output, self.written, self.program);
        }
        self.frames = self.frames.saturating_add(1);
        self.written = self.written.wrapping_add(size as i32);
        Ok(())
    }

    /// Reads the header of a block.
    fn header(&mut self, bits: &mut Bits) -> Coded<()> {
        let kind = bits.take(3) as u8;
        let size = if self.cabinet {
            bits.take(16) << 8 | bits.take(8)
        } else if bits.take(1) == 1 {
            FRAME as u32
        } else {
            let size = bits.take(16);
            if self.order >= 16 {
                size << 8 | bits.take(8)
            } else {
                size
            }
        };
        if size == 0 {
            return Err("LZX: a block of no size");
        }
        match kind {
            VERBATIM | ALIGNED => {
                if kind == ALIGNED {
                    let mut lengths = [0u8; ALIGNED_SYMBOLS];
                    for length in &mut lengths {
                        *length = bits.take(3) as u8;
                    }
                    self.aligned = Some(Huffman::new(&lengths).ok_or(BAD_TREE)?);
                }
                let (chars, matches) = self.main_lengths.split_at_mut(CHARS);
                read_lengths(bits, chars)?;
                read_lengths(bits, matches)?;
                self.main = Some(Huffman::new(&self.main_lengths).ok_or(BAD_TREE)?);
                read_lengths(bits, &mut self.length_lengths)?;
                self.lengths = Some(Huffman::new(&self.length_lengths).ok_or(BAD_TREE)?);
            }
            UNCOMPRESSED => {
                bits.align();
                let offsets = bits.bytes(12)?;
                for (recent, offset) in self.recent.iter_mut().zip(offsets.as_chunks::<4>().0) {
                    *recent = u32::from_le_bytes(*offset) as usize;
                }
            }
            _ => return Err("LZX: a block of an unknown kind"),
        }
        if bits.overran() {
            return Err(CUT);
        }
        self.kind = kind;
        self.left = size as usize;
        self.odd = size % 2 == 1;
        Ok(())
    }

    /// Decodes a part of a compressed block into the window from `at` up to
    /// `end`; `have` is how much was written before `at`.
    fn run(&mut self, bits: &mut Bits, mut at: usize, end: usize, have: usize) -> Coded<()> {
        let main = self.main.as_ref().ok_or(BAD_TREE)?;
        let lengths = self.lengths.as_ref().ok_or(BAD_TREE)?;
        let aligned = if self.kind == ALIGNED {
            Some(self.aligned.as_ref().ok_or(BAD_TREE)?)
        } else {
            None
        };
        let mask = self.window.len() - 1;
        let first = at;
        while at < end {
            let symbol = bits.symbol(main)?;
            if symbol < CHARS {
                self.window[at] = symbol as u8;
                at += 1;
                continue;
            }
            let symbol = symbol - CHARS;
            let mut length = symbol & 7;
            if length == 7 {
                length += bits.symbol(lengths)?;
            }
            length += MIN_MATCH;
            let slot = symbol >> 3;
            let offset = match slot {
                0 => self.recent[0],
                1 | 2 => {
                    self.recent.swap(0, slot);
                    self.recent[0]
                }
                _ => {
                    let extra = u32::from(EXTRA[slot]);
                    let within = match aligned {
                        Some(aligned) if extra >= 3 => {
                            let high = bits.many(extra - 3) << 3;
                            high + bits.symbol(aligned)? as u32
                        }
                        _ => bits.many(extra),
                    };
                    let offset = (BASE[slot] + within - 2) as usize;
                    self.recent[2] = self.recent[1];
                    self.recent[1] = self.recent[0];
                    self.recent[0] = offset;
                    offset
                }
            };
            if length > end - at {
                return Err("LZX: a match that runs past its block");
            }
            if offset == 0 || offset > have + (at - first) {
                return Err("LZX: a match from before the start of the data");
            }
            if offset <= at && offset >= length {
                self.window
                    .copy_within(at - offset..at - offset + length, at);
            } else {
                let from = at.wrapping_sub(offset);
                for step in 0..length {
                    self.window[at + step] = self.window[from.wrapping_add(step) & mask];
                }
            }
            at += length;
        }
        Ok(())
    }
}

/// Reads the lengths of the codewords of a code: each as its difference
/// from the length the block before gave it, coded with a code of its own.
fn read_lengths(bits: &mut Bits, lengths: &mut [u8]) -> Coded<()> {
    let mut pretree = [0u8; PRETREE_SYMBOLS];
    for length in &mut pretree {
        *length = bits.take(4) as u8;
    }
    let pretree = Huffman::new(&pretree).ok_or(BAD_TREE)?;
    let changed = |length: u8, by: usize| ((usize::from(length) + 17 - by) % 17) as u8;
    let mut at = 0;
    while at < lengths.len() {
        let (run, value) = match bits.symbol(&pretree)? {
            17 => (4 + bits.take(4) as usize, 0),
            18 => (20 + bits.take(5) as usize, 0),
            19 => {
                let run = 4 + bits.take(1) as usize;
                let by = bits.symbol(&pretree)?;
                if by > 17 {
                    return Err(BAD_TREE);
                }
                (run, changed(lengths[at], by))
            }
            by => (1, changed(lengths[at], by)),
        };
        let end = (at + run).min(lengths.len());
        lengths[at..end].fill(value);
        at = end;
    }
    Ok(())
}

#[cfg(test)]
pub(crate) mod tests {
    use proptest::prelude::*;

    use super::*;

    /// Writes bits the way `Bits` reads them.
    struct Writer {
        bytes: Vec<u8>,
        buffer: u64,
        count: u32,
    }

    impl Writer {
        fn new() -> Self {
            Self {
                bytes: Vec::new(),
                buffer: 0,
                count: 0,
            }
        }

        fn put(&mut self, value: u32, bits: u32) {
            if bits == 0 {
                return;
            }
            assert!(bits <= 32 && u64::from(value) < 1 << bits);
            self.buffer = self.buffer << bits | u64::from(value);
            self.count += bits;
            while self.count >= 16 {
                let word = (self.buffer >> (self.count - 16)) as u16;
                self.bytes.extend_from_slice(&word.to_le_bytes());
                self.count -= 16;
            }
        }

        /// Fills the word in use with zeros; a whole word where none is in
        /// use and `whole` asks for it.
        fn pad(&mut self, whole: bool) {
            if self.count > 0 {
                self.put(0, 16 - self.count);
            } else if whole {
                self.put(0, 16);
            }
        }

        fn raw(&mut self, bytes: &[u8]) {
            assert_eq!(self.count, 0);
            self.bytes.extend_from_slice(bytes);
        }
    }

    /// What a block of a test stream is made of.
    #[derive(Clone, Copy, Debug, PartialEq, Eq)]
    pub(crate) enum Kind {
        Verbatim,
        Aligned,
        Uncompressed,
    }

    /// The length of every codeword of the main code of the tests.
    const MAIN_BITS: u32 = 10;
    /// How far back the compressor of the tests looks for a match.
    const NEAR: usize = 256;
    /// The longest match the code of the tests has a codeword for.
    const LONGEST: usize = 208;
    /// The code of lengths of the tests: eight bits for a codeword, and
    /// two runs of lengths that have none.
    const LENGTH_CODE: [u8; LENGTH_SYMBOLS] = {
        let mut code = [8u8; LENGTH_SYMBOLS];
        let mut symbol = 0;
        while symbol < LENGTH_SYMBOLS {
            if (symbol >= 100 && symbol <= 110) || symbol >= 200 {
                code[symbol] = 0;
            }
            symbol += 1;
        }
        code
    };

    /// A compressor for the tests: the codewords of a code are equally
    /// long, and matches are looked for a little way back.
    pub(crate) struct Packer {
        cabinet: bool,
        order: u32,
        /// One more distance to look for a match at.
        pub(crate) far: usize,
        recent: [usize; 3],
        /// The lengths the block before gave: of the main code and of the
        /// code of lengths.
        main: Vec<u8>,
        lengths: Vec<u8>,
        /// The block being written and how much of it is left.
        kind: Kind,
        left: usize,
        odd: bool,
        started: bool,
        program: i32,
    }

    /// Makes the calls of x86 code absolute, as a compressor does.
    fn translate_calls(data: &mut [u8], position: i32, size: i32) {
        if data.len() <= 10 {
            return;
        }
        let end = data.len() - 10;
        let mut at = 0;
        while at < end {
            if data[at] != CALL {
                at += 1;
                continue;
            }
            let place = position + at as i32;
            let field = &mut data[at + 1..at + 5];
            let relative = i32::from_le_bytes([field[0], field[1], field[2], field[3]]);
            if relative >= -place && relative < size {
                let absolute = if relative < size - place {
                    relative + place
                } else {
                    relative - size
                };
                field.copy_from_slice(&absolute.to_le_bytes());
            }
            at += 5;
        }
    }

    impl Packer {
        pub(crate) fn cabinet(order: u32, program: i32) -> Self {
            Self {
                cabinet: true,
                order,
                far: 0,
                recent: [1; 3],
                main: vec![0; CHARS + 8 * slots(order)],
                lengths: vec![0; LENGTH_SYMBOLS],
                kind: Kind::Verbatim,
                left: 0,
                odd: false,
                started: false,
                program,
            }
        }

        pub(crate) fn image(chunk: u32) -> Self {
            let order = chunk.next_power_of_two().trailing_zeros().max(MIN_ORDER);
            Self {
                cabinet: false,
                program: IMAGE_PROGRAM,
                ..Self::cabinet(order, 0)
            }
        }

        /// Writes the lengths of the codewords of a code. The code they are
        /// written with has a codeword of five bits for each of its symbols.
        fn lengths(writer: &mut Writer, before: &mut [u8], now: &[u8]) {
            for _ in 0..PRETREE_SYMBOLS {
                writer.put(5, 4);
            }
            let mut at = 0;
            while at < now.len() {
                let same = (at..now.len())
                    .take_while(|next| now[*next] == now[at] && before[*next] == before[at])
                    .count();
                let by = (u32::from(before[at]) + 17 - u32::from(now[at])) % 17;
                at += if now[at] == 0 && same >= 20 {
                    let run = same.min(51);
                    writer.put(18, 5);
                    writer.put(run as u32 - 20, 5);
                    run
                } else if now[at] == 0 && same >= 4 {
                    let run = same.min(19);
                    writer.put(17, 5);
                    writer.put(run as u32 - 4, 4);
                    run
                } else if same >= 4 {
                    let run = same.min(5);
                    writer.put(19, 5);
                    writer.put(run as u32 - 4, 1);
                    writer.put(by, 5);
                    run
                } else {
                    writer.put(by, 5);
                    1
                };
            }
            before.copy_from_slice(now);
        }

        fn header(&mut self, writer: &mut Writer, kind: Kind, size: usize) {
            let number = match kind {
                Kind::Verbatim => VERBATIM,
                Kind::Aligned => ALIGNED,
                Kind::Uncompressed => UNCOMPRESSED,
            };
            writer.put(u32::from(number), 3);
            if self.cabinet {
                writer.put(size as u32, 24);
            } else if size == FRAME {
                writer.put(1, 1);
            } else {
                writer.put(0, 1);
                writer.put(size as u32, if self.order >= 16 { 24 } else { 16 });
            }
            if kind == Kind::Uncompressed {
                writer.pad(true);
                for offset in self.recent {
                    writer.raw(&(offset as u32).to_le_bytes());
                }
            } else {
                if kind == Kind::Aligned {
                    for _ in 0..ALIGNED_SYMBOLS {
                        writer.put(3, 3);
                    }
                }
                let now = vec![MAIN_BITS as u8; self.main.len()];
                let (chars, matches) = self.main.split_at_mut(CHARS);
                Self::lengths(writer, chars, &now[..CHARS]);
                Self::lengths(writer, matches, &now[CHARS..]);
                Self::lengths(writer, &mut self.lengths, &LENGTH_CODE);
            }
            self.kind = kind;
            self.left = size;
            self.odd = size % 2 == 1;
        }

        /// The longest match for what starts at `at` that ends by `end`.
        fn find(
            &self,
            data: &[u8],
            window: usize,
            at: usize,
            end: usize,
        ) -> Option<(usize, usize)> {
            let most = (end - at).min(LONGEST);
            if most < MIN_MATCH {
                return None;
            }
            let reach = at.min(window - 3);
            let mut best: Option<(usize, usize)> = None;
            let candidates = self
                .recent
                .into_iter()
                .chain(1..=reach.min(NEAR))
                .chain([self.far]);
            for offset in candidates.filter(|offset| (1..=reach).contains(offset)) {
                let length = (0..most)
                    .take_while(|step| data[at + step] == data[at + step - offset])
                    .count();
                if length >= MIN_MATCH && best.is_none_or(|(_, longest)| length > longest) {
                    best = Some((offset, length));
                }
            }
            // The lengths whose codewords the code of the tests leaves out.
            best.map(|(offset, length)| match length {
                109..=119 => (offset, 108),
                _ => (offset, length),
            })
        }

        fn matched(&mut self, writer: &mut Writer, offset: usize, length: usize) {
            let header = (length - MIN_MATCH).min(7);
            let slot = if let Some(slot) = self.recent.iter().position(|recent| *recent == offset) {
                self.recent.swap(0, slot);
                slot
            } else {
                self.recent = [offset, self.recent[0], self.recent[1]];
                let formatted = offset as u32 + 2;
                (3..MAX_SLOTS)
                    .find(|slot| formatted < BASE[slot + 1])
                    .unwrap()
            };
            writer.put((CHARS + (slot << 3 | header)) as u32, MAIN_BITS);
            if header == 7 {
                // A codeword is the place of its symbol among those that
                // have one.
                let symbol = length - MIN_MATCH - 7;
                let before = if symbol > 110 { 11 } else { 0 };
                writer.put((symbol - before) as u32, 8);
            }
            if slot >= 3 {
                let extra = u32::from(EXTRA[slot]);
                let within = offset as u32 + 2 - BASE[slot];
                if self.kind == Kind::Aligned && extra >= 3 {
                    writer.put(within >> 3, extra - 3);
                    writer.put(within & 7, 3);
                } else {
                    writer.put(within, extra);
                }
            }
        }

        /// Compresses `data`, the whole of a stream, as blocks of the given
        /// kinds and sizes, the last of them repeated to the end. Gives the
        /// data of every frame of a cabinet, or of the one chunk of an
        /// image.
        pub(crate) fn pack(&mut self, data: &[u8], blocks: &[(Kind, usize)]) -> Vec<Vec<u8>> {
            let mut data = data.to_vec();
            let frame = if self.cabinet {
                FRAME
            } else {
                data.len().max(1)
            };
            if self.program != 0 {
                for (index, piece) in data.chunks_mut(frame).enumerate() {
                    translate_calls(piece, (index * frame) as i32, self.program);
                }
            }
            let window = 1usize << self.order;
            let mut blocks = blocks.iter().copied().cycle();
            let mut frames = Vec::new();
            let mut at = 0;
            while at < data.len() {
                let end = (at + frame).min(data.len());
                let mut writer = Writer::new();
                if self.cabinet && !self.started {
                    self.started = true;
                    if self.program == 0 {
                        writer.put(0, 1);
                    } else {
                        writer.put(1, 1);
                        writer.put(self.program as u32 >> 16, 16);
                        writer.put(self.program as u32 & 0xFFFF, 16);
                    }
                }
                while at < end {
                    if self.left == 0 {
                        let (kind, size) = blocks.next().unwrap();
                        // A block of an image ends with its chunk.
                        let size = if self.cabinet {
                            size
                        } else {
                            size.min(end - at)
                        };
                        self.header(&mut writer, kind, size);
                    }
                    let stop = (at + self.left).min(end);
                    self.left -= stop - at;
                    if self.kind == Kind::Uncompressed {
                        writer.raw(&data[at..stop]);
                        at = stop;
                        if self.left == 0 && self.odd {
                            writer.raw(&[0]);
                        }
                        continue;
                    }
                    while at < stop {
                        if let Some((offset, length)) = self.find(&data, window, at, stop) {
                            self.matched(&mut writer, offset, length);
                            at += length;
                        } else {
                            writer.put(u32::from(data[at]), MAIN_BITS);
                            at += 1;
                        }
                    }
                }
                writer.pad(false);
                frames.push(writer.bytes);
            }
            frames
        }
    }

    /// Reads back what `Packer` wrote for a cabinet.
    fn unpack(order: u32, frames: &[Vec<u8>], size: usize) -> Coded<Vec<u8>> {
        let mut lzx = Lzx::cabinet(order).unwrap();
        let mut data = vec![0; size];
        for (frame, piece) in frames.iter().zip(data.chunks_mut(FRAME)) {
            lzx.decode(frame, piece)?;
        }
        Ok(data)
    }

    /// Text that repeats itself at many distances, with calls in it.
    pub(crate) fn sample(size: usize, seed: u32) -> Vec<u8> {
        let mut state = seed | 1;
        let mut next = move || {
            state ^= state << 13;
            state ^= state >> 17;
            state ^= state << 5;
            state
        };
        let mut data = Vec::with_capacity(size);
        while data.len() < size {
            match next() % 8 {
                0 => data.extend_from_slice(b"the quick brown fox "),
                1 => data.extend_from_slice(&next().to_le_bytes()),
                2 => {
                    data.push(CALL);
                    data.extend_from_slice(&(next() % 5000).to_le_bytes());
                }
                3 if data.len() > 40 => {
                    let from = next() as usize % (data.len() - 30);
                    let piece = data[from..from + 30].to_vec();
                    data.extend_from_slice(&piece);
                }
                4 => data.extend_from_slice(&[0xAB; 300][..next() as usize % 300]),
                _ => data.push(next() as u8),
            }
        }
        data.truncate(size);
        data
    }

    #[test]
    fn the_slots_of_offsets_are_what_the_format_says() {
        assert_eq!(&EXTRA[..10], &[0, 0, 0, 0, 1, 1, 2, 2, 3, 3]);
        assert_eq!(&EXTRA[36..], &[17; 15]);
        assert_eq!(&BASE[..10], &[0, 1, 2, 3, 4, 6, 8, 12, 16, 24]);
        assert_eq!(BASE[30], 32768);
        assert_eq!(BASE[36], 262_144);
        assert_eq!(BASE[50], 2_097_152);
        let counts: Vec<usize> = (15..=21).map(slots).collect();
        assert_eq!(counts, [30, 32, 34, 36, 38, 42, 50]);
        assert!(Lzx::cabinet(14).is_none());
        assert!(Lzx::cabinet(22).is_none());
        assert!(Lzx::image(0).is_none());
        assert!(Lzx::image(4 << 20).is_none());
        assert_eq!(Lzx::image(4096).unwrap().most(), 32768);
        assert_eq!(Lzx::image(1 << 20).unwrap().most(), 1 << 20);
    }

    #[test]
    fn the_frames_of_a_cabinet_are_read() {
        // The second half repeats the first: matches from far away.
        let mut data = sample(100_000, 7);
        data.extend_from_within(..);
        for order in [15, 17, 21] {
            for blocks in [
                &[(Kind::Verbatim, 32768)][..],
                &[(Kind::Aligned, 50_001)],
                &[(Kind::Uncompressed, 4097), (Kind::Verbatim, 70_000)],
                &[
                    (Kind::Aligned, 11),
                    (Kind::Uncompressed, 40_000),
                    (Kind::Uncompressed, 3),
                    (Kind::Verbatim, 1),
                ],
            ] {
                let mut packer = Packer::cabinet(order, 0);
                packer.far = 100_000;
                let frames = packer.pack(&data, blocks);
                assert_eq!(frames.len(), 7);
                let read = unpack(order, &frames, data.len()).unwrap();
                assert!(read == data, "order {order}, blocks {blocks:?}");
            }
        }
    }

    #[test]
    fn calls_are_made_relative_again() {
        let data = sample(100_000, 99);
        let blocks = [(Kind::Verbatim, 32768)];
        let frames = Packer::cabinet(16, 60_000).pack(&data, &blocks);
        assert!(unpack(16, &frames, data.len()).unwrap() == data);
        // The same stream without the size of the program is the data with
        // its calls still absolute.
        let plain = Packer::cabinet(16, 0).pack(&data, &blocks);
        assert_ne!(frames[1], plain[1]);
        // The numbers of the format itself.
        let mut code = [0u8; 32];
        code[4] = CALL;
        code[5..9].copy_from_slice(&1000i32.to_le_bytes());
        code[12] = CALL;
        code[13..17].copy_from_slice(&(-105i32).to_le_bytes());
        code[25] = CALL;
        code[26..30].copy_from_slice(&7i32.to_le_bytes());
        restore_calls(&mut code, 100, 5000);
        assert_eq!(code[5..9], (1000i32 - 104).to_le_bytes());
        assert_eq!(code[13..17], (5000i32 - 105).to_le_bytes());
        // Too close to the end to be a call.
        assert_eq!(code[26..30], 7i32.to_le_bytes());
    }

    #[test]
    fn the_chunks_of_an_image_are_read() {
        let data = sample(150_000, 3);
        for (chunk, blocks) in [
            (32768u32, &[(Kind::Verbatim, 32768)][..]),
            (32768, &[(Kind::Aligned, 1000), (Kind::Uncompressed, 777)]),
            (
                1 << 17,
                &[(Kind::Verbatim, 100_000), (Kind::Aligned, 32768)],
            ),
        ] {
            let mut lzx = Lzx::image(chunk).unwrap();
            for piece in data.chunks(chunk as usize) {
                let packed = Packer::image(chunk).pack(piece, blocks);
                assert_eq!(packed.len(), 1);
                let mut read = vec![0; piece.len()];
                lzx.decode(&packed[0], &mut read).unwrap();
                assert!(read == piece, "chunk {chunk}, blocks {blocks:?}");
            }
        }
    }

    #[test]
    fn what_is_broken_is_an_error() {
        let data = sample(40_000, 5);
        let frames = Packer::cabinet(15, 0).pack(&data, &[(Kind::Verbatim, 32768)]);
        // The end is missing.
        let mut lzx = Lzx::cabinet(15).unwrap();
        let mut piece = vec![0; FRAME];
        let cut = &frames[0][..frames[0].len() / 2];
        assert!(lzx.decode(cut, &mut piece).is_err());
        // More is asked for than a frame holds.
        let mut lzx = Lzx::cabinet(15).unwrap();
        assert!(lzx.decode(&frames[0], &mut vec![0; FRAME + 1]).is_err());
        // A block of a kind there is not, and one of no size.
        let mut lzx = Lzx::cabinet(15).unwrap();
        assert!(lzx.decode(&[0x00, 0x70, 0, 1], &mut piece).is_err());
        let mut lzx = Lzx::cabinet(15).unwrap();
        assert!(lzx.decode(&[0x00, 0x10, 0, 0], &mut piece).is_err());
        // A match that reaches before the first byte.
        let mut packer = Packer::cabinet(15, 0);
        let mut writer = Writer::new();
        writer.put(0, 1);
        packer.header(&mut writer, Kind::Verbatim, 300);
        writer.put(u32::from(b'a'), MAIN_BITS);
        packer.matched(&mut writer, 500, 10);
        writer.pad(false);
        let mut lzx = Lzx::cabinet(15).unwrap();
        let error = lzx.decode(&writer.bytes, &mut [0; 300]).unwrap_err();
        assert!(error.contains("before the start"), "{error}");
        // A match at no distance: an uncompressed block sets the offsets,
        // the block after it uses the first.
        let mut packer = Packer::cabinet(15, 0);
        packer.recent = [0, 1, 1];
        let mut writer = Writer::new();
        writer.put(0, 1);
        packer.header(&mut writer, Kind::Uncompressed, 2);
        writer.raw(b"ab");
        packer.header(&mut writer, Kind::Verbatim, 298);
        packer.matched(&mut writer, 0, 10);
        writer.pad(false);
        let mut lzx = Lzx::cabinet(15).unwrap();
        let error = lzx.decode(&writer.bytes, &mut [0; 300]).unwrap_err();
        assert!(error.contains("before the start"), "{error}");
        // A match longer than what is left of its block.
        let mut packer = Packer::cabinet(15, 0);
        let mut writer = Writer::new();
        writer.put(0, 1);
        packer.header(&mut writer, Kind::Verbatim, 5);
        writer.put(u32::from(b'a'), MAIN_BITS);
        packer.matched(&mut writer, 1, 10);
        writer.pad(false);
        let mut lzx = Lzx::cabinet(15).unwrap();
        let error = lzx.decode(&writer.bytes, &mut [0; 300]).unwrap_err();
        assert!(error.contains("past its block"), "{error}");
        // Nothing at all.
        let mut lzx = Lzx::cabinet(15).unwrap();
        assert!(lzx.decode(&[], &mut [0; 300]).is_err());
    }

    proptest! {
        #![proptest_config(ProptestConfig::with_cases(48))]

        #[test]
        fn whatever_is_packed_is_read_back(
            seed in any::<u32>(),
            size in 1usize..70_000,
            order in 15u32..=17,
            program in prop_oneof![Just(0i32), 1i32..200_000],
            blocks in proptest::collection::vec(
                (
                    prop_oneof![
                        Just(Kind::Verbatim),
                        Just(Kind::Aligned),
                        Just(Kind::Uncompressed)
                    ],
                    1usize..70_000,
                ),
                1..5,
            ),
        ) {
            let data = sample(size, seed);
            let frames = Packer::cabinet(order, program).pack(&data, &blocks);
            let read = unpack(order, &frames, data.len());
            prop_assert!(read.as_ref() == Ok(&data));
        }

        #[test]
        fn garbage_is_an_error_or_data_and_never_a_panic(
            input in proptest::collection::vec(any::<u8>(), 0..3000),
            size in 0usize..=FRAME,
            order in 15u32..=21,
        ) {
            let mut lzx = Lzx::cabinet(order).unwrap();
            let mut output = vec![0; size];
            let _ = lzx.decode(&input, &mut output);
            let _ = lzx.decode(&input, &mut output);
            let mut lzx = Lzx::image(1 << order).unwrap();
            let _ = lzx.decode(&input, &mut output);
        }
    }
}

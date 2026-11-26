//! The benchmark corpus: the data every speed measurement of BoxIt runs on.
//!
//! The corpus is generated, not downloaded: the same bytes on every machine
//! and in every run, with no network and nothing to store in the repository.
//! Speed of compression depends on what is compressed, so the corpus has
//! three sets that stress different parts of an archiver:
//!
//! | Set          | Files          | What it measures                              |
//! |--------------|----------------|-----------------------------------------------|
//! | `mixed`      | a few hundred  | codecs on text, binary, sparse and noise      |
//! | `one-large`  | one            | raw speed of a codec, scaling with threads    |
//! | `many-small` | tens of thousands | cost per file: headers, file system, queue |
//!
//! The kinds of content imitate what people put into archives. *Text* is made
//! of words drawn with a skewed frequency, like prose, logs and source code.
//! *Binary* is made of records with counters, tags and a few random fields,
//! like databases and executables. *Sparse* is mostly zeros, like disk
//! images. *Noise* cannot be compressed, like media and archives inside
//! archives; a codec must get through it fast without making it bigger.
//!
//! The benchmark of the command line, the criterion benchmarks and the
//! comparison with 7-Zip all take their data from here.

use std::fs;
use std::io::{self, Read, Write};
use std::path::Path;

const KIB: u64 = 1024;
const MIB: u64 = 1024 * KIB;

/// A small, fast generator of pseudo-random numbers (SplitMix64). The corpus
/// must not change between versions of a library, so the generator is here.
#[derive(Clone, Debug)]
struct Random(u64);

impl Random {
    fn new(seed: u64) -> Self {
        Self(seed)
    }

    fn next(&mut self) -> u64 {
        self.0 = self.0.wrapping_add(0x9E37_79B9_7F4A_7C15);
        let mut z = self.0;
        z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
        z ^ (z >> 31)
    }

    /// A number below `bound`; `bound` must not be zero.
    fn below(&mut self, bound: u64) -> u64 {
        // The high bits of a multiplication are spread evenly enough for a
        // benchmark corpus and cost no division.
        ((u128::from(self.next()) * u128::from(bound)) >> 64) as u64
    }
}

/// What a file of the corpus is made of.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Kind {
    /// Words with a skewed frequency: compresses about four times.
    Text,
    /// Records with counters, tags and random fields: compresses about twice.
    Binary,
    /// Mostly zeros with short runs of data.
    Sparse,
    /// Random bytes: cannot be compressed.
    Noise,
}

impl Kind {
    fn extension(self) -> &'static str {
        match self {
            Self::Text => "txt",
            Self::Binary => "bin",
            Self::Sparse => "img",
            Self::Noise => "dat",
        }
    }
}

/// One file of the corpus: its place, its size and how to make its content.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct FileSpec {
    /// Path inside the set, with `/` between the parts.
    pub path: String,
    pub kind: Kind,
    pub size: u64,
    seed: u64,
}

impl FileSpec {
    /// The content of the file as a stream, so a large file never has to be
    /// held in memory.
    pub fn reader(&self) -> Content {
        Content::new(self.kind, self.size, self.seed)
    }

    /// The whole content of the file.
    pub fn bytes(&self) -> Vec<u8> {
        let mut data = Vec::with_capacity(self.size as usize);
        self.reader()
            .read_to_end(&mut data)
            .expect("generating content cannot fail");
        data
    }
}

/// Number of different words of the text content.
const VOCABULARY: usize = 4096;
/// Size of the block the content is produced in.
const BLOCK: usize = 64 * 1024;
/// Size of one record of the binary content.
const RECORD: usize = 32;

/// The content of a corpus file, produced block by block.
pub struct Content {
    kind: Kind,
    left: u64,
    random: Random,
    /// Words of the text content: `(offset, length)` into `letters`.
    words: Vec<(u32, u8)>,
    letters: Vec<u8>,
    /// Number of the next record of the binary content.
    record: u64,
    block: Vec<u8>,
    at: usize,
}

impl Content {
    fn new(kind: Kind, size: u64, seed: u64) -> Self {
        let mut words = Vec::new();
        let mut letters = Vec::new();
        if kind == Kind::Text {
            // The vocabulary is the same for every text file, as the words of
            // a language are; only the order of the words differs.
            let mut shared = Random::new(0x00B0_0C17);
            words.reserve(VOCABULARY);
            for _ in 0..VOCABULARY {
                let length = 2 + shared.below(9) as u8;
                let offset = letters.len() as u32;
                for _ in 0..length {
                    letters.push(b'a' + shared.below(26) as u8);
                }
                words.push((offset, length));
            }
        }
        Self {
            kind,
            left: size,
            random: Random::new(seed),
            words,
            letters,
            record: 0,
            block: Vec::with_capacity(BLOCK + 64),
            at: 0,
        }
    }

    fn fill_text(&mut self) {
        while self.block.len() < BLOCK {
            // Squaring a uniform number makes low indexes frequent, the way
            // a few words of a language make up most of a text.
            let uniform = self.random.below(1 << 16);
            let skewed = (uniform * uniform) >> 16;
            let index = ((skewed * VOCABULARY as u64) >> 16) as usize;
            let (offset, length) = self.words[index];
            let offset = offset as usize;
            self.block
                .extend_from_slice(&self.letters[offset..offset + usize::from(length)]);
            let separator = match self.random.below(24) {
                0 => b'\n',
                1 => b',',
                2 => b'.',
                _ => b' ',
            };
            self.block.push(separator);
            if separator != b' ' && separator != b'\n' {
                self.block.push(b' ');
            }
        }
    }

    fn fill_binary(&mut self) {
        while self.block.len() < BLOCK {
            let mut record = [0u8; RECORD];
            record[..8].copy_from_slice(&self.record.to_le_bytes());
            // A tag from a small set, a flag byte, two values that change
            // slowly and one field of real noise.
            record[8..12]
                .copy_from_slice(&(0xCAFE_0000u32 | self.random.below(16) as u32).to_le_bytes());
            record[12] = self.random.below(4) as u8;
            record[16..20].copy_from_slice(&((self.record / 7) as u32).to_le_bytes());
            record[20..24].copy_from_slice(&((self.record * 3) as u32).to_le_bytes());
            record[24..32].copy_from_slice(&self.random.next().to_le_bytes());
            self.block.extend_from_slice(&record);
            self.record += 1;
        }
    }

    fn fill_sparse(&mut self) {
        self.block.resize(BLOCK, 0);
        // A handful of short runs of data in a block of zeros.
        for _ in 0..4 {
            let start = self.random.below((BLOCK - 256) as u64) as usize;
            let length = 16 + self.random.below(240) as usize;
            for byte in &mut self.block[start..start + length] {
                *byte = self.random.next() as u8;
            }
        }
    }

    fn fill_noise(&mut self) {
        while self.block.len() < BLOCK {
            self.block
                .extend_from_slice(&self.random.next().to_le_bytes());
        }
    }

    fn fill(&mut self) {
        self.block.clear();
        self.at = 0;
        match self.kind {
            Kind::Text => self.fill_text(),
            Kind::Binary => self.fill_binary(),
            Kind::Sparse => self.fill_sparse(),
            Kind::Noise => self.fill_noise(),
        }
    }
}

impl Read for Content {
    fn read(&mut self, out: &mut [u8]) -> io::Result<usize> {
        if self.left == 0 || out.is_empty() {
            return Ok(0);
        }
        if self.at == self.block.len() {
            self.fill();
        }
        let ready = (self.block.len() - self.at) as u64;
        let count = ready.min(self.left).min(out.len() as u64) as usize;
        out[..count].copy_from_slice(&self.block[self.at..self.at + count]);
        self.at += count;
        self.left -= count as u64;
        Ok(count)
    }
}

/// A set of files of the corpus.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Set {
    /// A few hundred files of every kind and of very different sizes.
    Mixed,
    /// One large file of text.
    OneLarge,
    /// Tens of thousands of small files in a tree of folders.
    ManySmall,
}

/// How much data a set holds. The shape of the set stays the same; only the
/// sizes change, so a quick check and a full measurement stress the same
/// code.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Scale {
    /// A few megabytes per set: for tests and for benchmarks that count
    /// instructions.
    Small,
    /// The size the comparison with other archivers is made on: about
    /// 64 MiB of mixed files, a file of 256 MiB and 20 000 small files.
    Full,
}

impl Set {
    pub const ALL: [Set; 3] = [Set::Mixed, Set::OneLarge, Set::ManySmall];

    /// The name of the set in reports and on the command line.
    pub fn name(self) -> &'static str {
        match self {
            Self::Mixed => "mixed",
            Self::OneLarge => "one-large",
            Self::ManySmall => "many-small",
        }
    }

    pub fn from_name(name: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|set| set.name() == name)
    }

    /// The files of the set. The list is the same on every call.
    pub fn files(self, scale: Scale) -> Vec<FileSpec> {
        match self {
            Self::Mixed => mixed(scale),
            Self::OneLarge => one_large(scale),
            Self::ManySmall => many_small(scale),
        }
    }

    /// Writes the set as a tree of files under `folder` and returns the
    /// number of bytes written.
    pub fn write_tree(self, scale: Scale, folder: &Path) -> io::Result<u64> {
        let mut total = 0;
        let mut buffer = vec![0u8; BLOCK];
        for spec in self.files(scale) {
            let path = folder.join(spec.path.replace('/', std::path::MAIN_SEPARATOR_STR));
            if let Some(parent) = path.parent() {
                fs::create_dir_all(parent)?;
            }
            let mut file = fs::File::create(path)?;
            let mut content = spec.reader();
            loop {
                let count = content.read(&mut buffer)?;
                if count == 0 {
                    break;
                }
                file.write_all(&buffer[..count])?;
            }
            total += spec.size;
        }
        Ok(total)
    }
}

/// Total size of a list of files.
pub fn total_size(files: &[FileSpec]) -> u64 {
    files.iter().map(|file| file.size).sum()
}

fn kind_of(number: u64) -> Kind {
    // Half of what people archive compresses well, a quarter so-so, and the
    // rest is already compressed or empty.
    match number % 8 {
        0..=3 => Kind::Text,
        4 | 5 => Kind::Binary,
        6 => Kind::Noise,
        _ => Kind::Sparse,
    }
}

fn mixed(scale: Scale) -> Vec<FileSpec> {
    let (count, budget) = match scale {
        Scale::Small => (48, 4 * MIB),
        Scale::Full => (240, 64 * MIB),
    };
    let mut random = Random::new(0x0001_0001);
    // Sizes follow a long tail: many small files and a few that hold most
    // of the bytes, as in a folder of a real project.
    let weights: Vec<u64> = (0..count)
        .map(|_| {
            let exponent = random.below(12);
            (1 << exponent) + random.below(1 << exponent)
        })
        .collect();
    let sum: u64 = weights.iter().sum();
    weights
        .into_iter()
        .enumerate()
        .map(|(index, weight)| {
            let number = index as u64;
            let kind = kind_of(random.next());
            FileSpec {
                path: format!(
                    "mixed/part{:02}/file{number:03}.{}",
                    number % 6,
                    kind.extension()
                ),
                kind,
                size: (u128::from(budget) * u128::from(weight) / u128::from(sum)) as u64,
                seed: 0x1000_0000 + number,
            }
        })
        .collect()
}

fn one_large(scale: Scale) -> Vec<FileSpec> {
    let size = match scale {
        Scale::Small => 8 * MIB,
        Scale::Full => 256 * MIB,
    };
    // Text is the content codecs differ on most, in ratio and in speed.
    vec![FileSpec {
        path: String::from("one-large/large.txt"),
        kind: Kind::Text,
        size,
        seed: 0x2000_0000,
    }]
}

fn many_small(scale: Scale) -> Vec<FileSpec> {
    let count: u64 = match scale {
        Scale::Small => 1_000,
        Scale::Full => 20_000,
    };
    let mut random = Random::new(0x0003_0003);
    (0..count)
        .map(|number| {
            let kind = kind_of(random.next());
            // From empty files to 16 KiB, most of them a few kilobytes.
            let size = match random.below(16) {
                0 => 0,
                1..=11 => random.below(4 * KIB),
                _ => random.below(16 * KIB),
            };
            FileSpec {
                path: format!(
                    "many-small/d{:02}/s{:02}/f{number:05}.{}",
                    number % 40,
                    (number / 40) % 25,
                    kind.extension()
                ),
                kind,
                size,
                seed: 0x3000_0000 + number,
            }
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeSet;

    use super::*;

    /// Bits per byte of the order-0 statistics of the data: 8 for noise,
    /// about 4 for text, close to 0 for zeros.
    fn entropy(data: &[u8]) -> f64 {
        let mut counts = [0u64; 256];
        for byte in data {
            counts[usize::from(*byte)] += 1;
        }
        let total = data.len() as f64;
        counts
            .iter()
            .filter(|count| **count > 0)
            .map(|count| {
                let share = *count as f64 / total;
                -share * share.log2()
            })
            .sum()
    }

    fn sample(kind: Kind, size: u64, seed: u64) -> Vec<u8> {
        FileSpec {
            path: String::new(),
            kind,
            size,
            seed,
        }
        .bytes()
    }

    #[test]
    fn content_has_exactly_the_size_of_the_file() {
        for kind in [Kind::Text, Kind::Binary, Kind::Sparse, Kind::Noise] {
            for size in [
                0,
                1,
                31,
                32,
                33,
                BLOCK as u64 - 1,
                BLOCK as u64,
                BLOCK as u64 + 1,
                200_000,
            ] {
                assert_eq!(sample(kind, size, 1).len() as u64, size, "{kind:?} {size}");
            }
        }
    }

    #[test]
    fn content_does_not_depend_on_how_it_is_read() {
        let spec = FileSpec {
            path: String::new(),
            kind: Kind::Text,
            size: 300_000,
            seed: 7,
        };
        let whole = spec.bytes();
        let mut pieces = Vec::new();
        let mut reader = spec.reader();
        let mut buffer = [0u8; 777];
        loop {
            let count = reader.read(&mut buffer).unwrap();
            if count == 0 {
                break;
            }
            pieces.extend_from_slice(&buffer[..count]);
        }
        assert_eq!(pieces, whole);
    }

    #[test]
    fn content_is_the_same_on_every_run_and_differs_between_files() {
        assert_eq!(sample(Kind::Noise, 4096, 1), sample(Kind::Noise, 4096, 1));
        assert_ne!(sample(Kind::Noise, 4096, 1), sample(Kind::Noise, 4096, 2));
        assert_ne!(sample(Kind::Text, 4096, 1), sample(Kind::Text, 4096, 2));
        assert_ne!(sample(Kind::Binary, 4096, 1), sample(Kind::Binary, 4096, 2));
        assert_ne!(
            sample(Kind::Sparse, 70_000, 1),
            sample(Kind::Sparse, 70_000, 2)
        );
    }

    #[test]
    fn the_bytes_of_the_corpus_never_change() {
        // A change of the generator would silently make old and new
        // measurements incomparable. These sums pin the content down.
        let sum = |data: &[u8]| {
            data.iter().fold(0xCBF2_9CE4_8422_2325u64, |hash, byte| {
                (hash ^ u64::from(*byte)).wrapping_mul(0x0000_0100_0000_01B3)
            })
        };
        let sums: Vec<u64> = [Kind::Text, Kind::Binary, Kind::Sparse, Kind::Noise]
            .into_iter()
            .map(|kind| sum(&sample(kind, 100_000, 42)))
            .collect();
        assert_eq!(
            sums,
            [PINNED_TEXT, PINNED_BINARY, PINNED_SPARSE, PINNED_NOISE]
        );
    }

    const PINNED_TEXT: u64 = 1093101747705027255;
    const PINNED_BINARY: u64 = 1047682342023418016;
    const PINNED_SPARSE: u64 = 16963222973743552037;
    const PINNED_NOISE: u64 = 14614460781199916093;

    #[test]
    fn kinds_differ_in_how_well_they_compress() {
        let size = 1 << 20;
        let text = entropy(&sample(Kind::Text, size, 3));
        let binary = entropy(&sample(Kind::Binary, size, 3));
        let sparse = entropy(&sample(Kind::Sparse, size, 3));
        let noise = entropy(&sample(Kind::Noise, size, 3));
        assert!(noise > 7.99, "noise {noise}");
        assert!((3.5..5.0).contains(&text), "text {text}");
        assert!((3.0..7.0).contains(&binary), "binary {binary}");
        assert!(sparse < 0.5, "sparse {sparse}");
    }

    #[test]
    fn sets_have_the_promised_shape() {
        let mixed = Set::Mixed.files(Scale::Full);
        assert_eq!(mixed.len(), 240);
        let total = total_size(&mixed);
        assert!((60 * MIB..=64 * MIB).contains(&total), "{total}");
        let kinds: BTreeSet<&str> = mixed.iter().map(|file| file.kind.extension()).collect();
        assert_eq!(kinds.len(), 4);

        let large = Set::OneLarge.files(Scale::Full);
        assert_eq!(large.len(), 1);
        assert_eq!(large[0].size, 256 * MIB);

        let small = Set::ManySmall.files(Scale::Full);
        assert_eq!(small.len(), 20_000);
        assert!(small.iter().all(|file| file.size < 16 * KIB));
        assert!(small.iter().any(|file| file.size == 0));
    }

    #[test]
    fn paths_are_unique_and_relative() {
        for set in Set::ALL {
            for scale in [Scale::Small, Scale::Full] {
                let files = set.files(scale);
                let paths: BTreeSet<&str> = files.iter().map(|file| file.path.as_str()).collect();
                assert_eq!(paths.len(), files.len(), "{set:?}");
                assert!(files
                    .iter()
                    .all(|file| file.path.starts_with(set.name()) && !file.path.contains('\\')));
                assert_eq!(files, set.files(scale));
            }
        }
    }

    #[test]
    fn names_of_sets_round_trip() {
        for set in Set::ALL {
            assert_eq!(Set::from_name(set.name()), Some(set));
        }
        assert_eq!(Set::from_name("none"), None);
    }
}

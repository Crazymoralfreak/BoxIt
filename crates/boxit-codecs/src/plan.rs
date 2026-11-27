//! How the five levels map to each codec, and how many threads and how much
//! memory an operation takes.
//!
//! LZMA is the codec that needs planning: its encoder takes about eleven
//! times its dictionary, per thread. The planner gives every worker a chunk
//! of input of its own and takes as many workers as the budget holds, so a
//! large dictionary on a small machine compresses on fewer threads instead of
//! swapping.

use boxit_core::options::{MAX_DICTIONARY, MIN_DICTIONARY};
use boxit_core::{ArchiveFormat, Budget, CreateOptions, Level, Method, Stream};
use lzma_rust2::LzmaOptions;

const KIB: u64 = 1024;
const MIB: u64 = 1024 * 1024;

/// The level of zlib for a level of ours.
pub fn deflate_level(level: Level) -> u32 {
    match level {
        Level::Store => 0,
        Level::Fast => 1,
        Level::Normal => 6,
        Level::Maximum | Level::Ultra => 9,
    }
}

/// The level of bzip2: the size of its block in hundreds of kilobytes.
pub fn bzip2_level(level: Level) -> u32 {
    match level {
        Level::Store | Level::Fast => 1,
        Level::Normal => 6,
        Level::Maximum | Level::Ultra => 9,
    }
}

pub fn zstd_level(level: Level) -> i32 {
    match level {
        Level::Store | Level::Fast => 1,
        Level::Normal => 3,
        Level::Maximum => 12,
        Level::Ultra => 19,
    }
}

/// The preset of LZMA, on the scale of `xz` from 0 to 9.
pub fn lzma_preset(level: Level) -> u32 {
    match level {
        Level::Store => 0,
        Level::Fast => 1,
        Level::Normal => 5,
        Level::Maximum => 7,
        Level::Ultra => 9,
    }
}

/// The dictionary the user asked for, within what the formats allow.
pub fn dictionary(options: &CreateOptions) -> Option<u32> {
    options
        .dictionary
        .map(|size| size.clamp(MIN_DICTIONARY, MAX_DICTIONARY) as u32)
}

/// The options of an LZMA encoder for a level and a dictionary.
pub fn lzma_options(level: Level, dictionary: Option<u32>) -> LzmaOptions {
    let mut options = LzmaOptions::with_preset(lzma_preset(level));
    if let Some(dictionary) = dictionary {
        options.dict_size = dictionary.clamp(lzma_rust2::DICT_SIZE_MIN, lzma_rust2::DICT_SIZE_MAX);
    }
    options
}

/// The logarithm of the window of zstd that holds a dictionary of this size.
pub fn zstd_window_log(dictionary: u32) -> u32 {
    dictionary
        .checked_next_power_of_two()
        .map_or(u32::BITS, u32::trailing_zeros)
        .clamp(10, 30)
}

/// How a multithreaded LZMA encoder is set up.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct LzmaPlan {
    /// Threads that encode; one means the plain encoder.
    pub workers: u32,
    /// Bytes of input each worker encodes on its own.
    pub chunk: u64,
    /// Memory the encoder takes with this setup, in bytes.
    pub memory: u64,
}

/// Plans an LZMA encoder: chunks of four dictionaries, as 7-Zip takes them,
/// and as many workers as the threads and the memory of the budget allow.
pub fn lzma(options: &LzmaOptions, budget: Budget) -> LzmaPlan {
    let encoder = u64::from(options.get_memory_usage()) * KIB;
    let chunk = (u64::from(options.dict_size) * 4).clamp(MIB, 256 * MIB);
    // A worker holds its chunk of input and the compressed output of it.
    let worker = encoder + 2 * chunk;
    let fit = (budget.memory / worker).max(1);
    let workers = fit.min(budget.threads as u64).min(256) as u32;
    if workers <= 1 {
        return LzmaPlan {
            workers: 1,
            chunk,
            memory: encoder + MIB,
        };
    }
    LzmaPlan {
        workers,
        chunk,
        memory: u64::from(workers) * worker,
    }
}

/// The method a format compresses with when the user does not choose.
pub fn method(options: &CreateOptions) -> Method {
    if options.level == Level::Store {
        return Method::Store;
    }
    if let Some(method) = options.method {
        return method;
    }
    match options.format {
        ArchiveFormat::SevenZ => Method::Lzma2,
        ArchiveFormat::TarXz | ArchiveFormat::Xz => Method::Xz,
        ArchiveFormat::TarZst | ArchiveFormat::Zst => Method::Zstd,
        ArchiveFormat::TarBz2 | ArchiveFormat::Bz2 => Method::Bzip2,
        ArchiveFormat::Tar => Method::Store,
        _ => Method::Deflate,
    }
}

/// An estimate of the memory creating an archive takes with these options,
/// in bytes: what the dialog shows next to the choice of the dictionary.
pub fn memory_to_create(options: &CreateOptions, budget: Budget) -> u64 {
    let threads = budget.threads as u64;
    let buffers = 8 * MIB;
    let codec = match options.format.stream() {
        Some(Stream::Lz4) => MIB,
        Some(Stream::Gzip) => threads * 3 * MIB,
        _ => match method(options) {
            Method::Store => 0,
            Method::Deflate | Method::Deflate64 => threads * 3 * MIB,
            Method::Bzip2 => threads * u64::from(bzip2_level(options.level)) * 4 * MIB,
            Method::Lzma | Method::Lzma2 | Method::Xz => {
                lzma(&lzma_options(options.level, dictionary(options)), budget).memory
            }
            Method::Ppmd => threads * ppmd_memory(options.level),
            Method::Zstd => {
                let window = dictionary(options).map_or_else(
                    || match options.level {
                        Level::Store | Level::Fast => MIB,
                        Level::Normal => 2 * MIB,
                        Level::Maximum => 8 * MIB,
                        Level::Ultra => 16 * MIB,
                    },
                    |size| 1u64 << zstd_window_log(size),
                );
                // Each worker holds a window of input and its tables.
                threads * window * 4
            }
        },
    };
    codec + buffers
}

/// Memory of the PPMd model for a level, in bytes.
pub fn ppmd_memory(level: Level) -> u64 {
    match level {
        Level::Store | Level::Fast => 16 * MIB,
        Level::Normal => 64 * MIB,
        Level::Maximum => 128 * MIB,
        Level::Ultra => 256 * MIB,
    }
}

/// The order of the PPMd model for a level.
pub fn ppmd_order(level: Level) -> u32 {
    match level {
        Level::Store | Level::Fast => 4,
        Level::Normal => 6,
        Level::Maximum => 16,
        Level::Ultra => 32,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn budget(threads: usize, memory: u64) -> Budget {
        Budget { threads, memory }
    }

    #[test]
    fn a_small_budget_takes_fewer_workers_not_more_memory() {
        let options = lzma_options(Level::Ultra, None);
        let wide = lzma(&options, budget(16, 64 * 1024 * MIB));
        assert_eq!(wide.workers, 16);
        let narrow = lzma(&options, budget(16, 2 * 1024 * MIB));
        assert!(narrow.workers < wide.workers);
        assert!(narrow.workers >= 1);
        assert!(narrow.memory <= 2 * 1024 * MIB);
        // One worker is always granted, whatever the budget.
        let tiny = lzma(&options, budget(16, MIB));
        assert_eq!(tiny.workers, 1);
    }

    #[test]
    fn workers_never_exceed_the_threads() {
        let options = lzma_options(Level::Fast, None);
        assert_eq!(lzma(&options, budget(3, u64::MAX)).workers, 3);
        assert_eq!(lzma(&options, budget(1, u64::MAX)).workers, 1);
        assert_eq!(lzma(&options, budget(1000, u64::MAX)).workers, 256);
    }

    #[test]
    fn a_chosen_dictionary_stays_within_the_limits() {
        let mut options = CreateOptions::new(ArchiveFormat::SevenZ);
        options.dictionary = Some(1);
        assert_eq!(dictionary(&options), Some(64 * 1024));
        options.dictionary = Some(u64::MAX);
        assert_eq!(dictionary(&options), Some(1 << 30));
        options.dictionary = None;
        assert_eq!(dictionary(&options), None);
        assert_eq!(
            lzma_options(Level::Normal, Some(1 << 20)).dict_size,
            1 << 20
        );
    }

    #[test]
    fn a_larger_dictionary_takes_more_memory() {
        let budget = budget(4, 1024 * MIB);
        let mut options = CreateOptions::new(ArchiveFormat::SevenZ);
        options.dictionary = Some(MIB);
        let small = memory_to_create(&options, budget);
        options.dictionary = Some(64 * MIB);
        let large = memory_to_create(&options, budget);
        assert!(large > small);
        for format in ArchiveFormat::ALL {
            if format.can_write() {
                assert!(memory_to_create(&CreateOptions::new(format), budget) > 0);
            }
        }
    }

    #[test]
    fn levels_do_not_go_down_as_they_get_stronger() {
        for pair in Level::ALL.windows(2) {
            assert!(deflate_level(pair[0]) <= deflate_level(pair[1]));
            assert!(bzip2_level(pair[0]) <= bzip2_level(pair[1]));
            assert!(zstd_level(pair[0]) <= zstd_level(pair[1]));
            assert!(lzma_preset(pair[0]) <= lzma_preset(pair[1]));
        }
        assert_eq!(zstd_window_log(1 << 20), 20);
        assert_eq!(zstd_window_log((1 << 20) + 1), 21);
        assert_eq!(zstd_window_log(u32::MAX), 30);
    }

    #[test]
    fn storing_overrides_the_method() {
        let mut options = CreateOptions::new(ArchiveFormat::Zip);
        assert_eq!(method(&options), Method::Deflate);
        options.method = Some(Method::Zstd);
        assert_eq!(method(&options), Method::Zstd);
        options.level = Level::Store;
        assert_eq!(method(&options), Method::Store);
        assert_eq!(
            method(&CreateOptions::new(ArchiveFormat::SevenZ)),
            Method::Lzma2
        );
    }
}

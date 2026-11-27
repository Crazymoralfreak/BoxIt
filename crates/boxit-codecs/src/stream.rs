//! The compressions that wrap one stream of bytes: gzip, bzip2, xz, zstd and
//! lz4. A compressed tar reads and writes its tar through these, and a single
//! compressed file is nothing but one of them.

use std::io::{self, Read, Write};
use std::num::NonZeroU64;

use boxit_core::{Budget, Level, Stream};
use bzip2::read::MultiBzDecoder;
use bzip2::write::BzEncoder;
use flate2::read::MultiGzDecoder;
use flate2::write::GzEncoder;
use lz4_flex::frame::{BlockSize, FrameDecoder, FrameEncoder, FrameInfo};
use lzma_rust2::{XzOptions, XzReader, XzWriter, XzWriterMt};
use zstd::stream::read::Decoder as ZstdDecoder;
use zstd::stream::write::Encoder as ZstdEncoder;

use crate::chunked::{Chunked, Packer};
use crate::plan;

/// The largest window a zstd stream may ask for: what `zstd --long=31` makes.
const ZSTD_WINDOW_LOG_MAX: u32 = 31;
/// The smallest window of the format.
const ZSTD_WINDOW_LOG_MIN: u32 = 10;

/// How to compress a stream.
#[derive(Clone, Copy, Debug)]
pub struct Settings {
    pub level: Level,
    /// Size of the dictionary in bytes, for the codecs that have one.
    pub dictionary: Option<u32>,
    pub budget: Budget,
}

/// A compressor of a stream. Dropping it without [`Encoder::finish`] leaves
/// the output cut short.
pub trait Encoder: Write + Send {
    /// Writes what closes the stream and flushes the output.
    fn finish(self: Box<Self>) -> io::Result<()>;
}

/// What zstd says about a stream whose window is beyond the limit.
const ZSTD_TOO_MUCH_MEMORY: &str = "too much memory";

/// A zstd decoder that reports a window it may not allocate as a lack of
/// memory. The library reports it as an error like any other, told apart by
/// its text only.
struct ZstdWithin<R>(R);

impl<R: Read> Read for ZstdWithin<R> {
    fn read(&mut self, buffer: &mut [u8]) -> io::Result<usize> {
        self.0.read(buffer).map_err(|error| {
            if error.to_string().contains(ZSTD_TOO_MUCH_MEMORY) {
                io::Error::new(io::ErrorKind::OutOfMemory, error)
            } else {
                error
            }
        })
    }
}

/// Decompresses `input`. Streams that follow one another in the same input,
/// as `cat a.gz b.gz` makes them, are read as one.
///
/// `memory` is what the dictionary of the stream may take, in bytes. The
/// stream itself says how large its dictionary is, and a few bytes can ask
/// for gigabytes: a stream that asks for more than this fails with an
/// out-of-memory error before anything is allocated.
pub fn decoder<'a>(
    stream: Stream,
    input: impl Read + Send + 'a,
    memory: u64,
) -> io::Result<Box<dyn Read + Send + 'a>> {
    Ok(match stream {
        Stream::Gzip => Box::new(MultiGzDecoder::new(input)),
        Stream::Bzip2 => Box::new(MultiBzDecoder::new(input)),
        Stream::Xz => {
            // The limit of the library is in KiB, and `u32::MAX` means none.
            let limit =
                u32::try_from(memory / 1024).map_or(u32::MAX - 1, |kib| kib.min(u32::MAX - 1));
            Box::new(XzReader::new_mem_limit(input, true, limit))
        }
        Stream::Zstd => {
            let mut decoder = ZstdDecoder::new(input)?;
            let fits = memory.max(1).ilog2();
            decoder.window_log_max(fits.clamp(ZSTD_WINDOW_LOG_MIN, ZSTD_WINDOW_LOG_MAX))?;
            Box::new(ZstdWithin(decoder))
        }
        Stream::Lz4 => Box::new(Lz4Frames::new(input)),
    })
}

const LZ4_MAGIC: [u8; 4] = [0x04, 0x22, 0x4D, 0x18];
/// Set in the flags of a frame that ends with a checksum of its content.
const LZ4_CONTENT_CHECKSUM: u8 = 0x04;

/// Remembers the first and the last bytes read through it, which is enough
/// to tell how an lz4 frame ended.
struct Tail<R> {
    inner: R,
    head: [u8; 5],
    last: [u8; 8],
    count: u64,
}

impl<R: Read> Read for Tail<R> {
    fn read(&mut self, buffer: &mut [u8]) -> io::Result<usize> {
        let read = self.inner.read(buffer)?;
        for byte in &buffer[..read] {
            if let Some(slot) = self.head.get_mut(self.count as usize) {
                *slot = *byte;
            }
            self.count += 1;
        }
        if read >= self.last.len() {
            self.last.copy_from_slice(&buffer[read - 8..read]);
        } else {
            self.last.rotate_left(read);
            self.last[8 - read..].copy_from_slice(&buffer[..read]);
        }
        Ok(read)
    }
}

impl<R> Tail<R> {
    /// Whether the bytes read so far are a whole frame: one that ends with
    /// its end mark, and with its checksum when it has one.
    fn frame_ended(&self) -> bool {
        if self.count < 11 {
            return false;
        }
        if self.head[..4] != LZ4_MAGIC {
            // The frame format of old has no end mark: it ends with the input.
            return true;
        }
        if self.head[4] & LZ4_CONTENT_CHECKSUM == 0 {
            self.last[4..] == [0; 4]
        } else {
            self.count >= 15 && self.last[..4] == [0; 4]
        }
    }
}

/// Reads lz4 frames that follow one another as one stream, and refuses an
/// input that stops in the middle of a frame: the decoder of `lz4_flex`
/// reads one frame and takes the end of the input between two blocks for
/// the end of the data.
struct Lz4Frames<R: Read> {
    decoder: FrameDecoder<Tail<R>>,
    frames: u64,
    /// Bytes of the current frame consumed when the decoder last gave nothing.
    stalled_at: u64,
}

impl<R: Read> Lz4Frames<R> {
    fn new(input: R) -> Self {
        let tail = Tail {
            inner: input,
            head: [0; 5],
            last: [0; 8],
            count: 0,
        };
        Self {
            decoder: FrameDecoder::new(tail),
            frames: 0,
            stalled_at: 0,
        }
    }
}

impl<R: Read> Read for Lz4Frames<R> {
    fn read(&mut self, buffer: &mut [u8]) -> io::Result<usize> {
        if buffer.is_empty() {
            return Ok(0);
        }
        loop {
            let read = self.decoder.read(buffer)?;
            if read > 0 {
                return Ok(read);
            }
            let tail = self.decoder.get_mut();
            if tail.count == 0 && self.frames > 0 {
                return Ok(0);
            }
            if tail.frame_ended() {
                self.frames += 1;
                self.stalled_at = 0;
                tail.count = 0;
                continue;
            }
            // Nothing decoded in the middle of a frame: an empty block, or
            // the input ran out. The second time in a row it is the input.
            if tail.count == self.stalled_at {
                return Err(io::ErrorKind::UnexpectedEof.into());
            }
            self.stalled_at = tail.count;
        }
    }
}

/// Compresses what is written to it into `output`.
pub fn encoder<'a>(
    stream: Stream,
    output: impl Write + Send + 'a,
    settings: &Settings,
) -> io::Result<Box<dyn Encoder + 'a>> {
    let threads = settings.budget.threads;
    Ok(match stream {
        Stream::Gzip => {
            let level = plan::deflate_level(settings.level);
            if threads > 1 && level > 0 {
                Box::new(Chunked::new(output, Packer::Gzip(level), threads)?)
            } else {
                Box::new(GzEncoder::new(output, flate2::Compression::new(level)))
            }
        }
        Stream::Bzip2 => {
            let level = plan::bzip2_level(settings.level);
            if threads > 1 {
                Box::new(Chunked::new(output, Packer::Bzip2(level), threads)?)
            } else {
                Box::new(BzEncoder::new(output, bzip2::Compression::new(level)))
            }
        }
        Stream::Xz => {
            let lzma = plan::lzma_options(settings.level, settings.dictionary);
            let planned = plan::lzma(&lzma, settings.budget);
            let mut options = XzOptions::with_preset(plan::lzma_preset(settings.level));
            options.lzma_options = lzma;
            if planned.workers > 1 {
                options.set_block_size(NonZeroU64::new(planned.chunk));
                Box::new(XzWriterMt::new(output, options, planned.workers)?)
            } else {
                Box::new(XzWriter::new(output, options)?)
            }
        }
        Stream::Zstd => {
            let mut encoder = ZstdEncoder::new(output, plan::zstd_level(settings.level))?;
            encoder.include_checksum(true)?;
            if let Some(dictionary) = settings.dictionary {
                let window_log = plan::zstd_window_log(dictionary);
                encoder.window_log(window_log)?;
                // Matches that far apart are found only by the long mode.
                encoder.long_distance_matching(window_log >= 27)?;
            }
            if threads > 1 {
                encoder.multithread(threads.min(256) as u32)?;
            }
            Box::new(encoder)
        }
        Stream::Lz4 => {
            let info = FrameInfo::new()
                .block_size(BlockSize::Max4MB)
                .content_checksum(true);
            Box::new(FrameEncoder::with_frame_info(info, output))
        }
    })
}

impl<W: Write + Send> Encoder for GzEncoder<W> {
    fn finish(self: Box<Self>) -> io::Result<()> {
        GzEncoder::finish(*self)?.flush()
    }
}

impl<W: Write + Send> Encoder for BzEncoder<W> {
    fn finish(self: Box<Self>) -> io::Result<()> {
        BzEncoder::finish(*self)?.flush()
    }
}

impl<W: Write + Send> Encoder for XzWriter<W> {
    fn finish(self: Box<Self>) -> io::Result<()> {
        XzWriter::finish(*self)?.flush()
    }
}

impl<W: Write + Send> Encoder for XzWriterMt<W> {
    fn finish(self: Box<Self>) -> io::Result<()> {
        XzWriterMt::finish(*self)?.flush()
    }
}

impl<W: Write + Send> Encoder for ZstdEncoder<'_, W> {
    fn finish(self: Box<Self>) -> io::Result<()> {
        ZstdEncoder::finish(*self)?.flush()
    }
}

impl<W: Write + Send> Encoder for FrameEncoder<W> {
    fn finish(self: Box<Self>) -> io::Result<()> {
        FrameEncoder::finish(*self)?.flush()
    }
}

impl<W: Write + Send> Encoder for Chunked<W> {
    fn finish(self: Box<Self>) -> io::Result<()> {
        Chunked::finish(*self)?.flush()
    }
}

/// What does not compress: the output of a plain tar.
pub struct Plain<W>(pub W);

impl<W: Write> Write for Plain<W> {
    fn write(&mut self, buffer: &[u8]) -> io::Result<usize> {
        self.0.write(buffer)
    }

    fn flush(&mut self) -> io::Result<()> {
        self.0.flush()
    }
}

impl<W: Write + Send> Encoder for Plain<W> {
    fn finish(mut self: Box<Self>) -> io::Result<()> {
        self.0.flush()
    }
}

#[cfg(test)]
mod tests {
    use boxit_core::Error;
    use proptest::prelude::*;

    use super::*;
    use crate::corpus::{Kind, Scale, Set};

    pub(crate) const STREAMS: [Stream; 5] = [
        Stream::Gzip,
        Stream::Bzip2,
        Stream::Xz,
        Stream::Zstd,
        Stream::Lz4,
    ];

    fn settings(level: Level, threads: usize) -> Settings {
        Settings {
            level,
            dictionary: None,
            budget: Budget::with_threads(threads),
        }
    }

    fn pack(stream: Stream, data: &[u8], settings: &Settings) -> Vec<u8> {
        let mut packed = Vec::new();
        let mut encoder = encoder(stream, &mut packed, settings).unwrap();
        // Uneven pieces, so no codec relies on the size of the writes.
        for piece in data.chunks(70_001) {
            encoder.write_all(piece).unwrap();
        }
        encoder.finish().unwrap();
        packed
    }

    fn unpack(stream: Stream, packed: &[u8]) -> io::Result<Vec<u8>> {
        let mut data = Vec::new();
        decoder(stream, packed, u64::MAX)?.read_to_end(&mut data)?;
        Ok(data)
    }

    fn sample() -> Vec<u8> {
        let mut data = Vec::new();
        for file in Set::Mixed.files(Scale::Small).iter().take(40) {
            data.extend_from_slice(&file.bytes());
        }
        data
    }

    #[test]
    fn every_stream_gives_back_what_it_took() {
        let data = sample();
        assert!(data.len() > 1_000_000, "{}", data.len());
        for stream in STREAMS {
            for level in Level::ALL {
                for threads in [1, 4] {
                    let packed = pack(stream, &data, &settings(level, threads));
                    assert_eq!(
                        boxit_core::sniff(&packed),
                        Some(boxit_core::Signature::Stream(stream)),
                        "{stream:?}"
                    );
                    let unpacked = unpack(stream, &packed).unwrap();
                    assert!(unpacked == data, "{stream:?} {level:?} {threads}");
                }
            }
        }
    }

    #[test]
    fn an_empty_stream_is_a_valid_stream() {
        for stream in STREAMS {
            for threads in [1, 4] {
                let packed = pack(stream, b"", &settings(Level::Normal, threads));
                assert!(!packed.is_empty(), "{stream:?}");
                assert!(unpack(stream, &packed).unwrap().is_empty(), "{stream:?}");
            }
        }
    }

    #[test]
    fn stronger_levels_do_not_lose_to_weaker_ones_on_text() {
        let file = &Set::OneLarge.files(Scale::Small)[0];
        let data: Vec<u8> = file.bytes();
        assert_eq!(file.kind, Kind::Text);
        for stream in [Stream::Gzip, Stream::Xz, Stream::Zstd] {
            let fast = pack(stream, &data, &settings(Level::Fast, 1)).len();
            let ultra = pack(stream, &data, &settings(Level::Ultra, 1)).len();
            assert!(ultra < fast, "{stream:?}: {ultra} >= {fast}");
            assert!(fast < data.len());
        }
    }

    #[test]
    fn a_chosen_dictionary_is_honoured() {
        let data = sample();
        for stream in [Stream::Xz, Stream::Zstd] {
            let settings = Settings {
                level: Level::Normal,
                dictionary: Some(1 << 16),
                budget: Budget::with_threads(2),
            };
            let packed = pack(stream, &data, &settings);
            assert!(unpack(stream, &packed).unwrap() == data, "{stream:?}");
        }
    }

    #[test]
    fn a_dictionary_beyond_the_memory_is_refused_before_it_is_allocated() {
        const MIB: u64 = 1024 * 1024;
        let data = sample();
        for stream in [Stream::Xz, Stream::Zstd] {
            let settings = Settings {
                level: Level::Normal,
                dictionary: Some(32 << 20),
                budget: Budget {
                    threads: 1,
                    memory: 1024 * MIB,
                },
            };
            let packed = pack(stream, &data, &settings);
            let read = |memory| {
                let mut unpacked = Vec::new();
                decoder(stream, packed.as_slice(), memory)
                    .and_then(|mut decoder| decoder.read_to_end(&mut unpacked))
                    .map(|_| unpacked)
            };
            let error = read(4 * MIB).unwrap_err();
            assert_eq!(
                error.kind(),
                io::ErrorKind::OutOfMemory,
                "{stream:?}: {error}"
            );
            assert_eq!(Error::from(error).code, "notEnoughMemory", "{stream:?}");
            assert!(read(256 * MIB).unwrap() == data, "{stream:?}");
        }
    }

    #[test]
    fn streams_that_follow_one_another_read_as_one() {
        for stream in STREAMS {
            let mut packed = pack(stream, b"first ", &settings(Level::Normal, 1));
            packed.extend(pack(stream, b"second", &settings(Level::Normal, 1)));
            assert_eq!(
                unpack(stream, &packed).unwrap(),
                b"first second",
                "{stream:?}"
            );
        }
    }

    #[test]
    fn damage_is_an_error_not_wrong_data() {
        let data = sample();
        for stream in STREAMS {
            let mut packed = pack(stream, &data, &settings(Level::Fast, 1));
            let middle = packed.len() / 2;
            packed[middle] ^= 0x55;
            match unpack(stream, &packed) {
                Err(_) => {}
                Ok(unpacked) => assert!(unpacked != data, "{stream:?}"),
            }
            // A stream cut short never passes for a whole one, wherever the
            // cut is: in the data, or in what closes the stream.
            packed[middle] ^= 0x55;
            for cut in [1, 4, 8, 12, packed.len() - middle] {
                let short = &packed[..packed.len() - cut];
                assert!(unpack(stream, short).is_err(), "{stream:?} without {cut}");
            }
        }
    }

    proptest! {
        #![proptest_config(ProptestConfig::with_cases(48))]

        #[test]
        fn any_bytes_round_trip(
            data in proptest::collection::vec(any::<u8>(), 0..20_000),
            repeats in 1usize..40,
            level in 0usize..5,
            threads in 1usize..5,
        ) {
            let data = data.repeat(repeats);
            for stream in STREAMS {
                let packed = pack(stream, &data, &settings(Level::ALL[level], threads));
                prop_assert!(unpack(stream, &packed).unwrap() == data, "{:?}", stream);
            }
        }

        #[test]
        fn garbage_never_panics(data in proptest::collection::vec(any::<u8>(), 0..4_000)) {
            for stream in STREAMS {
                let _ = unpack(stream, &data);
            }
        }
    }
}

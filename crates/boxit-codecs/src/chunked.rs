//! Gzip and bzip2 on several threads.
//!
//! Neither codec has threads of its own, but both formats allow the input to
//! be cut into chunks that are compressed apart and written one after
//! another. For gzip the chunks become deflate blocks of one stream, each
//! ended on a byte boundary the way `pigz` does it, so any program that reads
//! gzip reads the result. For bzip2 every chunk is a stream of its own and
//! the file is their sequence, as `pbzip2` writes it.
//!
//! The writer keeps as many chunks in flight as it has workers; when all are
//! busy it waits for the oldest, so memory stays at a few chunks whatever the
//! size of the input.

use std::collections::VecDeque;
use std::io::{self, Write};
use std::mem;
use std::sync::mpsc::{self, Receiver, SyncSender};
use std::sync::{Arc, Mutex};
use std::thread::{self, JoinHandle};

use bzip2::write::BzEncoder;
use crc32fast::Hasher;
use flate2::{Compress, FlushCompress};

/// Input of one gzip chunk. A fresh encoder forgets what came before, which
/// costs a little of the ratio at every boundary; at this size the loss is
/// a fraction of a percent.
const GZIP_CHUNK: usize = 1024 * 1024;

/// An empty final deflate block: closes a stream whose blocks all ended with
/// a flush.
const DEFLATE_END: [u8; 2] = [0x03, 0x00];

/// What compresses the chunks.
#[derive(Clone, Copy, Debug)]
pub enum Packer {
    /// Deflate at this level of zlib, 1 to 9.
    Gzip(u32),
    /// Bzip2 with blocks of this many hundreds of kilobytes, 1 to 9.
    Bzip2(u32),
}

impl Packer {
    fn chunk(self) -> usize {
        match self {
            Self::Gzip(_) => GZIP_CHUNK,
            // Four blocks of the encoder: fewer streams, same parallelism.
            Self::Bzip2(level) => level as usize * 400_000,
        }
    }

    fn pack(self, data: &[u8]) -> io::Result<Vec<u8>> {
        match self {
            Self::Gzip(level) => {
                let mut compress = Compress::new(flate2::Compression::new(level), false);
                let mut packed = Vec::with_capacity(data.len() / 2 + 64);
                loop {
                    let consumed = compress.total_in() as usize;
                    compress
                        .compress_vec(&data[consumed..], &mut packed, FlushCompress::Sync)
                        .map_err(io::Error::other)?;
                    // The flush is complete once it leaves room in the output.
                    if compress.total_in() as usize == data.len()
                        && packed.len() < packed.capacity()
                    {
                        return Ok(packed);
                    }
                    packed.reserve(packed.capacity().max(4096));
                }
            }
            Self::Bzip2(level) => {
                let mut encoder = BzEncoder::new(
                    Vec::with_capacity(data.len() / 4 + 64),
                    bzip2::Compression::new(level),
                );
                encoder.write_all(data)?;
                encoder.finish()
            }
        }
    }
}

struct Job {
    data: Vec<u8>,
    done: SyncSender<io::Result<Packed>>,
}

struct Packed {
    bytes: Vec<u8>,
    /// The input, handed back so its buffer serves the next chunk.
    data: Vec<u8>,
    crc: u32,
}

/// The threads that compress. Dropping the pool ends them.
struct Pool {
    jobs: Option<SyncSender<Job>>,
    workers: Vec<JoinHandle<()>>,
}

impl Pool {
    fn start(packer: Packer, threads: usize) -> io::Result<Self> {
        let (jobs, queue) = mpsc::sync_channel::<Job>(threads);
        let queue = Arc::new(Mutex::new(queue));
        let workers = (0..threads)
            .map(|_| {
                let queue = Arc::clone(&queue);
                thread::Builder::new()
                    .name(String::from("boxit-pack"))
                    .spawn(move || loop {
                        // The lock is held only while waiting for a job.
                        let job = match queue.lock() {
                            Ok(queue) => queue.recv(),
                            Err(_) => return,
                        };
                        let Ok(job) = job else { return };
                        let result = packer.pack(&job.data).map(|bytes| Packed {
                            bytes,
                            crc: crc32fast::hash(&job.data),
                            data: job.data,
                        });
                        // Nobody waits once the writer is dropped.
                        let _ = job.done.send(result);
                    })
            })
            .collect::<io::Result<Vec<_>>>()?;
        Ok(Self {
            jobs: Some(jobs),
            workers,
        })
    }

    fn send(&self, job: Job) -> io::Result<()> {
        self.jobs
            .as_ref()
            .and_then(|jobs| jobs.send(job).ok())
            .ok_or_else(|| io::Error::other("the compression threads stopped"))
    }
}

impl Drop for Pool {
    fn drop(&mut self) {
        // Closing the queue is what tells the workers to end.
        self.jobs = None;
        for worker in self.workers.drain(..) {
            let _ = worker.join();
        }
    }
}

/// A writer that compresses its input in chunks on worker threads.
pub struct Chunked<W: Write> {
    output: W,
    packer: Packer,
    chunk: usize,
    buffer: Vec<u8>,
    spare: Vec<Vec<u8>>,
    pool: Pool,
    window: usize,
    pending: VecDeque<Receiver<io::Result<Packed>>>,
    crc: Hasher,
    length: u64,
    chunks: u64,
}

impl<W: Write> Chunked<W> {
    pub fn new(mut output: W, packer: Packer, threads: usize) -> io::Result<Self> {
        let threads = threads.clamp(1, 64);
        if let Packer::Gzip(level) = packer {
            // No name and no time: the same input gives the same bytes.
            let flags = match level {
                9 => 2,
                1 => 4,
                _ => 0,
            };
            output.write_all(&[0x1F, 0x8B, 8, 0, 0, 0, 0, 0, flags, 255])?;
        }
        let chunk = packer.chunk();
        Ok(Self {
            output,
            packer,
            chunk,
            buffer: Vec::with_capacity(chunk),
            spare: Vec::new(),
            pool: Pool::start(packer, threads)?,
            window: threads,
            pending: VecDeque::with_capacity(threads + 1),
            crc: Hasher::new(),
            length: 0,
            chunks: 0,
        })
    }

    /// Writes the oldest chunk in flight, waiting for it if it is not ready.
    fn collect(&mut self) -> io::Result<()> {
        let Some(done) = self.pending.pop_front() else {
            return Ok(());
        };
        let packed = done
            .recv()
            .map_err(|_| io::Error::other("a compression thread stopped"))??;
        self.output.write_all(&packed.bytes)?;
        let length = packed.data.len() as u64;
        self.crc
            .combine(&Hasher::new_with_initial_len(packed.crc, length));
        self.length += length;
        let mut data = packed.data;
        data.clear();
        self.spare.push(data);
        Ok(())
    }

    /// Sends the buffered chunk to the workers.
    fn submit(&mut self) -> io::Result<()> {
        while self.pending.len() >= self.window {
            self.collect()?;
        }
        let next = self
            .spare
            .pop()
            .unwrap_or_else(|| Vec::with_capacity(self.chunk));
        let data = mem::replace(&mut self.buffer, next);
        let (done, result) = mpsc::sync_channel(1);
        self.pool.send(Job { data, done })?;
        self.pending.push_back(result);
        self.chunks += 1;
        Ok(())
    }

    /// Compresses what is left, closes the stream and returns the output.
    pub fn finish(mut self) -> io::Result<W> {
        // An empty bzip2 file still holds one stream.
        let empty_bzip2 = self.chunks == 0 && matches!(self.packer, Packer::Bzip2(_));
        if !self.buffer.is_empty() || empty_bzip2 {
            self.submit()?;
        }
        while !self.pending.is_empty() {
            self.collect()?;
        }
        if matches!(self.packer, Packer::Gzip(_)) {
            let crc = self.crc.clone().finalize();
            self.output.write_all(&DEFLATE_END)?;
            self.output.write_all(&crc.to_le_bytes())?;
            // The format keeps the length modulo 2^32.
            self.output.write_all(&(self.length as u32).to_le_bytes())?;
        }
        self.output.flush()?;
        Ok(self.output)
    }
}

impl<W: Write> Write for Chunked<W> {
    fn write(&mut self, buffer: &[u8]) -> io::Result<usize> {
        let room = self.chunk - self.buffer.len();
        let taken = buffer.len().min(room);
        self.buffer.extend_from_slice(&buffer[..taken]);
        if self.buffer.len() == self.chunk {
            self.submit()?;
        }
        Ok(taken)
    }

    fn flush(&mut self) -> io::Result<()> {
        // Chunks are written as they complete; cutting one short here would
        // only hurt the ratio.
        Ok(())
    }
}

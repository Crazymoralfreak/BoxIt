//! Speed of the formats on the benchmark corpus.
//!
//! Three groups, each over the three sets of the corpus, with throughput in
//! bytes of content:
//!
//! - `corpus` measures the corpus itself, so a result of a format can be
//!   told apart from the cost of producing its input;
//! - `create` writes an archive of every format BoxIt creates, at the usual
//!   level, into memory;
//! - `extract` opens such an archive and reads every entry to its end.
//!
//! The content is held in memory and the archives are written to memory:
//! what is measured is the formats, not the disk. The speed with the file
//! system in the way is measured by the benchmark of the command line.

use std::fs;
use std::hint::black_box;
use std::io::{self, Cursor, Read};
use std::sync::Arc;

use boxit_codecs::corpus::{total_size, Scale, Set};
use boxit_codecs::{create, open, OpenOptions};
use boxit_core::{
    ArchiveFormat, CreateOptions, DataSource, Entry, EntryMeta, EntrySink, Item, OpContext, Result,
    Selection,
};
use criterion::{criterion_group, criterion_main, BenchmarkId, Criterion, Throughput};
use tempfile::TempDir;

const FORMATS: [ArchiveFormat; 8] = [
    ArchiveFormat::Zip,
    ArchiveFormat::SevenZ,
    ArchiveFormat::Tar,
    ArchiveFormat::TarGz,
    ArchiveFormat::TarBz2,
    ArchiveFormat::TarXz,
    ArchiveFormat::TarZst,
    ArchiveFormat::TarLz4,
];

fn corpus(criterion: &mut Criterion) {
    let mut group = criterion.benchmark_group("corpus");
    group.sample_size(10);
    for set in Set::ALL {
        let files = set.files(Scale::Small);
        group.throughput(Throughput::Bytes(total_size(&files)));
        group.bench_function(set.name(), |bencher| {
            bencher.iter(|| {
                let mut total = 0;
                for file in &files {
                    total += io::copy(&mut file.reader(), &mut io::sink())
                        .expect("generating content cannot fail");
                }
                black_box(total)
            });
        });
    }
    group.finish();
}

/// The content of a file of the corpus, made once and shared by every run.
struct Shared(Arc<Vec<u8>>);

impl DataSource for Shared {
    fn open(&self) -> Result<Box<dyn Read + Send + '_>> {
        Ok(Box::new(self.0.as_slice()))
    }
}

/// A set of the corpus in memory.
struct Loaded {
    files: Vec<(EntryMeta, Arc<Vec<u8>>)>,
    bytes: u64,
}

impl Loaded {
    fn new(set: Set) -> Self {
        let specs = set.files(Scale::Small);
        Self {
            bytes: total_size(&specs),
            files: specs
                .iter()
                .map(|spec| {
                    (
                        EntryMeta::file(spec.path.clone(), spec.size),
                        Arc::new(spec.bytes()),
                    )
                })
                .collect(),
        }
    }

    fn items(&self) -> Vec<Item> {
        self.files
            .iter()
            .map(|(meta, data)| Item::new(meta.clone(), Shared(data.clone())))
            .collect()
    }

    /// The set as an archive, the way BoxIt creates one by default.
    fn pack(&self, format: ArchiveFormat, ctx: &OpContext) -> Vec<u8> {
        let mut output = Cursor::new(Vec::new());
        let mut writer = create(&mut output, &CreateOptions::new(format), ctx.budget)
            .expect("the format is one BoxIt creates");
        writer
            .add_all(self.items(), ctx)
            .expect("writing to memory cannot fail");
        writer.finish().expect("writing to memory cannot fail");
        output.into_inner()
    }
}

/// Reads every entry to its end and keeps nothing.
struct Drain;

impl EntrySink for Drain {
    fn accept(&self, _entry: &Entry, data: &mut dyn Read) -> Result<()> {
        black_box(io::copy(data, &mut io::sink())?);
        Ok(())
    }
}

fn formats(criterion: &mut Criterion) {
    let ctx = OpContext::silent();
    let folder = TempDir::new().expect("a temporary folder");
    let sets = Set::ALL.map(|set| (set, Loaded::new(set)));

    let mut group = criterion.benchmark_group("create");
    group.sample_size(10);
    for (set, loaded) in &sets {
        group.throughput(Throughput::Bytes(loaded.bytes));
        for format in FORMATS {
            group.bench_function(BenchmarkId::new(format.id(), set.name()), |bencher| {
                bencher.iter(|| black_box(loaded.pack(format, &ctx).len()));
            });
        }
    }
    group.finish();

    let mut group = criterion.benchmark_group("extract");
    group.sample_size(10);
    for (set, loaded) in &sets {
        group.throughput(Throughput::Bytes(loaded.bytes));
        for format in FORMATS {
            let path = folder
                .path()
                .join(format!("{}.{}", set.name(), format.id()));
            fs::write(&path, loaded.pack(format, &ctx)).expect("a temporary file");
            group.bench_function(BenchmarkId::new(format.id(), set.name()), |bencher| {
                bencher.iter(|| {
                    let mut opened =
                        open(&path, OpenOptions::default()).expect("the archive was just made");
                    opened
                        .reader
                        .extract(&Selection::All, &Drain, &ctx)
                        .expect("the archive was just made");
                });
            });
        }
    }
    group.finish();
}

criterion_group!(benches, corpus, formats);
criterion_main!(benches);

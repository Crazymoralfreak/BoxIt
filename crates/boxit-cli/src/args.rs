//! The arguments of the commands and how their values are read.

use std::path::PathBuf;

use boxit_codecs::corpus::Set;
use boxit_codecs::names::{NameEncoding, NAME_ENCODINGS};
use boxit_codecs::{volumes, OpenOptions};
use boxit_core::options::{MAX_DICTIONARY, MIN_DICTIONARY};
use boxit_core::{ArchiveFormat, ConflictPolicy, Level, Method, Password, SymlinkPolicy};
use boxit_engine::archive::open_options;
use boxit_engine::passwords::{self, LONGEST, SHORTEST};
use clap::{Args, ValueEnum};

/// The formats BoxIt creates, by the names the command line takes.
fn writable() -> String {
    ArchiveFormat::ALL
        .into_iter()
        .filter(|format| format.can_write())
        .map(ArchiveFormat::id)
        .collect::<Vec<_>>()
        .join(", ")
}

/// A format BoxIt creates, by its name or by any extension of its files:
/// `7z`, `tar.gz`, `tgz`.
pub(crate) fn format(text: &str) -> Result<ArchiveFormat, String> {
    let name = text.trim().trim_start_matches('.').to_lowercase();
    ArchiveFormat::from_id(&name)
        .or_else(|| ArchiveFormat::from_name(&format!("x.{name}")))
        .filter(|format| format.can_write())
        .ok_or_else(|| format!("one of: {}", writable()))
}

fn encoding(text: &str) -> Result<NameEncoding, String> {
    NameEncoding::from_label(text)
        .ok_or_else(|| format!("auto, or one of: {}", NAME_ENCODINGS.join(", ")))
}

/// A size: a number of bytes, or a number with `k`, `m` or `g` after it.
fn size(text: &str) -> Option<u64> {
    let text = text.trim().to_lowercase();
    let text = text.strip_suffix('b').unwrap_or(&text);
    let (digits, unit) = match text.chars().last() {
        Some('k') => (&text[..text.len() - 1], 1u64 << 10),
        Some('m') => (&text[..text.len() - 1], 1 << 20),
        Some('g') => (&text[..text.len() - 1], 1 << 30),
        _ => (text, 1),
    };
    digits.trim().parse::<u64>().ok()?.checked_mul(unit)
}

/// A size of a dictionary.
pub(crate) fn dictionary(text: &str) -> Result<u64, String> {
    size(text)
        .filter(|bytes| (MIN_DICTIONARY..=MAX_DICTIONARY).contains(bytes))
        .ok_or_else(|| "a size such as 64k, 16m or 1g, from 64k to 1g".to_owned())
}

/// A size of a volume.
pub(crate) fn volume(text: &str) -> Result<u64, String> {
    size(text)
        .filter(|bytes| *bytes >= volumes::SMALLEST)
        .ok_or_else(|| "a size such as 700m or 4g, 64k at the least".to_owned())
}

fn set(text: &str) -> Result<Set, String> {
    Set::from_name(text).ok_or_else(|| {
        let names: Vec<&str> = Set::ALL.into_iter().map(Set::name).collect();
        format!("one of: {}", names.join(", "))
    })
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, ValueEnum)]
pub enum LevelArg {
    /// No compression
    Store,
    Fast,
    Normal,
    Maximum,
    Ultra,
}

impl LevelArg {
    pub(crate) fn level(self) -> Level {
        match self {
            Self::Store => Level::Store,
            Self::Fast => Level::Fast,
            Self::Normal => Level::Normal,
            Self::Maximum => Level::Maximum,
            Self::Ultra => Level::Ultra,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, ValueEnum)]
pub enum MethodArg {
    Store,
    Deflate,
    Bzip2,
    Lzma,
    Lzma2,
    Ppmd,
    Zstd,
    Xz,
}

impl MethodArg {
    pub(crate) fn method(self) -> Method {
        match self {
            Self::Store => Method::Store,
            Self::Deflate => Method::Deflate,
            Self::Bzip2 => Method::Bzip2,
            Self::Lzma => Method::Lzma,
            Self::Lzma2 => Method::Lzma2,
            Self::Ppmd => Method::Ppmd,
            Self::Zstd => Method::Zstd,
            Self::Xz => Method::Xz,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, ValueEnum)]
pub enum ConflictArg {
    /// Ask in the terminal; keep the existing file when there is nobody to ask
    Ask,
    /// Keep the existing file
    Skip,
    Overwrite,
    /// Extract under a free name: "name (2).txt"
    Rename,
}

impl ConflictArg {
    pub(crate) fn policy(self) -> ConflictPolicy {
        match self {
            Self::Ask => ConflictPolicy::Ask,
            Self::Skip => ConflictPolicy::Skip,
            Self::Overwrite => ConflictPolicy::Overwrite,
            Self::Rename => ConflictPolicy::Rename,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, ValueEnum)]
pub enum SymlinkArg {
    /// Leave symbolic links out
    Skip,
    /// Create the links that stay inside the extracted folder
    Extract,
    /// Ask about each in the terminal
    Ask,
}

impl SymlinkArg {
    pub(crate) fn policy(self) -> SymlinkPolicy {
        match self {
            Self::Skip => SymlinkPolicy::Skip,
            Self::Extract => SymlinkPolicy::Extract,
            Self::Ask => SymlinkPolicy::Ask,
        }
    }
}

/// An archive to read.
#[derive(Debug, Args)]
pub struct ReadArgs {
    /// The archive file
    pub archive: PathBuf,
    /// Password of the archive; asked for in the terminal when it is needed
    /// and not given
    #[arg(short, long, env = "BOXIT_PASSWORD", hide_env_values = true)]
    pub password: Option<String>,
    /// Code page of the names that are not UTF-8: auto, cp437, ibm866,
    /// windows-1251, shift_jis and so on
    #[arg(long, default_value = "auto", value_parser = encoding)]
    pub encoding: NameEncoding,
}

impl ReadArgs {
    pub(crate) fn options(&self) -> OpenOptions {
        let password = self
            .password
            .as_deref()
            .filter(|password| !password.is_empty())
            .map(Password::from);
        open_options(password, self.encoding)
    }
}

/// What every command that does work takes.
#[derive(Debug, Args)]
pub struct CommonArgs {
    /// Worker threads; one per processor by default
    #[arg(short = 'j', long, default_value_t = 0)]
    pub threads: usize,
    /// Print the result as JSON
    #[arg(long)]
    pub json: bool,
    /// Do not show the progress
    #[arg(short, long)]
    pub quiet: bool,
}

#[derive(Debug, Args)]
pub struct ListArgs {
    #[command(flatten)]
    pub read: ReadArgs,
    /// Print the list as JSON
    #[arg(long)]
    pub json: bool,
}

#[derive(Debug, Args)]
pub struct ExtractArgs {
    #[command(flatten)]
    pub read: ReadArgs,
    /// Files and folders of the archive to extract; all of it by default
    pub paths: Vec<String>,
    /// The folder to extract into; the current one by default
    #[arg(short, long)]
    pub output: Option<PathBuf>,
    /// What to do when a file already exists
    #[arg(long, value_enum, default_value_t = ConflictArg::Ask)]
    pub conflicts: ConflictArg,
    /// What to do with symbolic links
    #[arg(long, value_enum, default_value_t = SymlinkArg::Skip)]
    pub symlinks: SymlinkArg,
    /// Lift the protection from archive bombs: the limits on the size of a
    /// file, on the compression ratio and on the free space
    #[arg(long)]
    pub no_limits: bool,
    #[command(flatten)]
    pub common: CommonArgs,
}

#[derive(Debug, Args)]
pub struct UpdateArgs {
    /// The archive to change
    pub archive: PathBuf,
    /// Drop these entries. A folder drops what is inside it
    #[arg(long = "delete")]
    pub delete: Vec<String>,
    /// Give an entry a new name, written as `old=new`
    #[arg(long = "rename")]
    pub rename: Vec<String>,
    /// Files and folders to add, each under its own name
    #[arg(long = "add")]
    pub add: Vec<PathBuf>,
    /// Write the changed archive here instead of replacing the original
    #[arg(short, long)]
    pub output: Option<PathBuf>,
    /// Password of the archive
    #[arg(short, long, env = "BOXIT_PASSWORD", hide_env_values = true)]
    pub password: Option<String>,
    #[command(flatten)]
    pub common: CommonArgs,
}

#[derive(Debug, Args)]
pub struct RepairArgs {
    #[command(flatten)]
    pub read: ReadArgs,
    /// The new archive; named after the damaged one by default, beside it
    #[arg(short, long)]
    pub output: Option<PathBuf>,
    /// Replace the file that stands at the destination
    #[arg(long)]
    pub overwrite: bool,
    #[command(flatten)]
    pub common: CommonArgs,
}

#[derive(Debug, Args)]
pub struct TestArgs {
    #[command(flatten)]
    pub read: ReadArgs,
    /// Files and folders of the archive to test; all of it by default
    pub paths: Vec<String>,
    /// Print the SHA-256 and the CRC-32 of every file, and the SHA-256 of
    /// the archive itself
    #[arg(long)]
    pub sums: bool,
    #[command(flatten)]
    pub common: CommonArgs,
}

#[derive(Debug, Args)]
pub struct CreateArgs {
    /// The archive file to create
    pub archive: PathBuf,
    /// Files and folders to store
    #[arg(required = true)]
    pub sources: Vec<PathBuf>,
    /// Format: zip, 7z, tar, tar.gz, tar.bz2, tar.xz, tar.zst, tar.lz4, gz,
    /// bz2, xz, zst, lz4; told by the name of the archive by default
    #[arg(short, long, value_parser = format)]
    pub format: Option<ArchiveFormat>,
    /// How hard to compress
    #[arg(short, long, value_enum, default_value_t = LevelArg::Normal)]
    pub level: LevelArg,
    /// Compression method, for zip and 7z; the usual one of the format by
    /// default
    #[arg(short, long, value_enum)]
    pub method: Option<MethodArg>,
    /// Size of the dictionary, such as 64k, 16m or 1g; follows the level by
    /// default
    #[arg(short, long, value_parser = dictionary)]
    pub dictionary: Option<u64>,
    /// Compress every file apart (7z): worse ratio, faster access to one
    /// file
    #[arg(long)]
    pub no_solid: bool,
    /// Comment of the archive (zip)
    #[arg(long)]
    pub comment: Option<String>,
    /// Protect the archive with this password (zip, 7z)
    #[arg(short, long, env = "BOXIT_PASSWORD", hide_env_values = true)]
    pub password: Option<String>,
    /// Ask for the password in the terminal instead
    #[arg(long, conflicts_with = "password")]
    pub ask_password: bool,
    /// Hide the names of the files as well as their content (7z)
    #[arg(long)]
    pub encrypt_names: bool,
    /// Cut the archive into volumes of this size, such as 700m or 4g: the
    /// files are named archive.001, archive.002 and so on
    #[arg(short, long, value_parser = volume, conflicts_with = "sfx")]
    pub volume: Option<u64>,
    /// Glue the archive to the program that unpacks it. The file is named
    /// with .exe. ZIP and 7z only
    #[arg(long, conflicts_with = "volume")]
    pub sfx: bool,
    /// Replace the archive when it exists
    #[arg(long)]
    pub overwrite: bool,
    #[command(flatten)]
    pub common: CommonArgs,
}

#[derive(Debug, Args)]
pub struct ConvertArgs {
    /// The archive to read
    pub source: PathBuf,
    /// The archive to write
    pub destination: PathBuf,
    /// Format of the new archive; told by its name by default
    #[arg(short, long, value_parser = format)]
    pub format: Option<ArchiveFormat>,
    /// How hard to compress
    #[arg(short, long, value_enum, default_value_t = LevelArg::Normal)]
    pub level: LevelArg,
    /// Password of the archive that is read
    #[arg(short, long, env = "BOXIT_PASSWORD", hide_env_values = true)]
    pub password: Option<String>,
    /// Replace the new archive when it exists
    #[arg(long)]
    pub overwrite: bool,
    #[command(flatten)]
    pub common: CommonArgs,
}

#[derive(Debug, Args)]
pub struct DiffArgs {
    /// The first archive
    pub left: PathBuf,
    /// The second archive
    pub right: PathBuf,
    #[command(flatten)]
    pub common: CommonArgs,
}

#[derive(Debug, Args)]
pub struct FindArgs {
    /// The archive to search
    pub archive: PathBuf,
    /// A piece of a name, a mask with * and ?, or re: and a regular expression
    pub pattern: String,
    #[command(flatten)]
    pub common: CommonArgs,
}

#[derive(Debug, Args)]
pub struct EstimateArgs {
    /// Files and folders to measure
    #[arg(required = true)]
    pub sources: Vec<PathBuf>,
    /// Format to estimate
    #[arg(short, long, value_parser = format, default_value = "zip")]
    pub format: ArchiveFormat,
    /// How hard to compress
    #[arg(short, long, value_enum, default_value_t = LevelArg::Normal)]
    pub level: LevelArg,
    #[command(flatten)]
    pub common: CommonArgs,
}

#[derive(Debug, Args)]
pub struct BatchArgs {
    /// What to do with every archive: test or extract
    #[arg(value_parser = ["test", "extract"])]
    pub action: String,
    /// The archives
    #[arg(required = true)]
    pub archives: Vec<PathBuf>,
    /// The folder to extract into; each archive gets a folder of its name
    #[arg(short, long)]
    pub output: Option<PathBuf>,
    #[command(flatten)]
    pub common: CommonArgs,
}

#[derive(Debug, Args)]
pub struct PasswordArgs {
    /// How many characters to draw
    #[arg(short, long, default_value_t = passwords::GENERATED_LENGTH, value_parser = password_length)]
    pub length: u32,
    /// Use symbols as well as letters and digits
    #[arg(long)]
    pub symbols: bool,
}

fn password_length(text: &str) -> Result<u32, String> {
    let length: u32 = text
        .parse()
        .map_err(|_| format!("a number from {SHORTEST} to {LONGEST}"))?;
    if (SHORTEST..=LONGEST).contains(&length) {
        Ok(length)
    } else {
        Err(format!("a number from {SHORTEST} to {LONGEST}"))
    }
}

#[derive(Debug, Args)]
pub struct BenchArgs {
    /// Formats to measure
    #[arg(
        short,
        long,
        value_parser = format,
        value_delimiter = ',',
        default_value = "zip,7z,tar.gz,tar.bz2,tar.xz,tar.zst,tar.lz4"
    )]
    pub formats: Vec<ArchiveFormat>,
    /// Sets of files to measure on: mixed, one-large, many-small
    #[arg(
        short,
        long = "set",
        value_parser = set,
        value_delimiter = ',',
        default_value = "mixed,one-large,many-small"
    )]
    pub sets: Vec<Set>,
    /// How hard to compress
    #[arg(short, long, value_enum, default_value_t = LevelArg::Normal)]
    pub level: LevelArg,
    /// The full size of the sets, hundreds of megabytes, instead of a few
    #[arg(long)]
    pub full: bool,
    /// How many times every measurement is made; the best one counts
    #[arg(short, long, default_value_t = 3, value_parser = clap::value_parser!(u32).range(1..=100))]
    pub runs: u32,
    /// The folder to work in: its disk is the one measured. The folder for
    /// temporary files of the system by default
    #[arg(long)]
    pub folder: Option<PathBuf>,
    /// Write the files of the sets into this folder and measure nothing
    #[arg(long, conflicts_with_all = ["formats", "level", "runs", "folder"])]
    pub corpus: Option<PathBuf>,
    /// Worker threads; one per processor by default
    #[arg(short = 'j', long, default_value_t = 0)]
    pub threads: usize,
    /// Print the result as JSON
    #[arg(long)]
    pub json: bool,
}

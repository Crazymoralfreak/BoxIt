//! The archive formats BoxIt knows, and how a file is recognized as one.
//!
//! A file is recognized by its first bytes, not by its name: a `.zip` that is
//! really a RAR opens as a RAR. The name only settles what the bytes cannot
//! tell, such as whether a gzip stream holds a tar archive.

use serde::{Deserialize, Serialize};

/// A compression that wraps one stream of bytes: a single file or a tar.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Stream {
    Gzip,
    Bzip2,
    Xz,
    Zstd,
    Lz4,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum ArchiveFormat {
    #[serde(rename = "zip")]
    Zip,
    #[serde(rename = "7z")]
    SevenZ,
    #[serde(rename = "tar")]
    Tar,
    #[serde(rename = "tar.gz")]
    TarGz,
    #[serde(rename = "tar.bz2")]
    TarBz2,
    #[serde(rename = "tar.xz")]
    TarXz,
    #[serde(rename = "tar.zst")]
    TarZst,
    #[serde(rename = "tar.lz4")]
    TarLz4,
    /// One compressed file, not a container.
    #[serde(rename = "gz")]
    Gz,
    #[serde(rename = "bz2")]
    Bz2,
    #[serde(rename = "xz")]
    Xz,
    #[serde(rename = "zst")]
    Zst,
    #[serde(rename = "lz4")]
    Lz4,
    #[serde(rename = "rar")]
    Rar,
    #[serde(rename = "cab")]
    Cab,
    #[serde(rename = "iso")]
    Iso,
    #[serde(rename = "ar")]
    Ar,
    #[serde(rename = "cpio")]
    Cpio,
    #[serde(rename = "lha")]
    Lha,
    #[serde(rename = "wim")]
    Wim,
}

impl ArchiveFormat {
    pub const ALL: [Self; 20] = [
        Self::Zip,
        Self::SevenZ,
        Self::Tar,
        Self::TarGz,
        Self::TarBz2,
        Self::TarXz,
        Self::TarZst,
        Self::TarLz4,
        Self::Gz,
        Self::Bz2,
        Self::Xz,
        Self::Zst,
        Self::Lz4,
        Self::Rar,
        Self::Cab,
        Self::Iso,
        Self::Ar,
        Self::Cpio,
        Self::Lha,
        Self::Wim,
    ];

    /// The short name of the format: in reports, on the command line and in
    /// the settings. It is also the main extension of its files.
    pub fn id(self) -> &'static str {
        self.extensions()[0]
    }

    pub fn from_id(id: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|format| format.id() == id)
    }

    /// Extensions of the files of this format, without the dot, lower case;
    /// the main one first.
    pub fn extensions(self) -> &'static [&'static str] {
        match self {
            Self::Zip => &[
                "zip", "zipx", "jar", "war", "ear", "apk", "ipa", "xpi", "epub",
            ],
            Self::SevenZ => &["7z"],
            Self::Tar => &["tar", "gtar", "ustar"],
            Self::TarGz => &["tar.gz", "tgz", "taz"],
            Self::TarBz2 => &["tar.bz2", "tbz2", "tbz", "tb2"],
            Self::TarXz => &["tar.xz", "txz"],
            Self::TarZst => &["tar.zst", "tzst"],
            Self::TarLz4 => &["tar.lz4", "tlz4"],
            Self::Gz => &["gz", "gzip"],
            Self::Bz2 => &["bz2", "bzip2"],
            Self::Xz => &["xz"],
            Self::Zst => &["zst", "zstd"],
            Self::Lz4 => &["lz4"],
            Self::Rar => &["rar"],
            Self::Cab => &["cab"],
            Self::Iso => &["iso"],
            Self::Ar => &["ar", "a", "deb", "lib"],
            Self::Cpio => &["cpio"],
            Self::Lha => &["lha", "lzh"],
            Self::Wim => &["wim", "swm", "esd"],
        }
    }

    /// The format a file name promises: the longest extension that matches,
    /// so `x.tar.gz` is a compressed tar and `x.gz` one compressed file.
    pub fn from_name(name: &str) -> Option<Self> {
        let name = name.to_lowercase();
        let mut best: Option<(usize, Self)> = None;
        for format in Self::ALL {
            for extension in format.extensions() {
                let matches = name
                    .strip_suffix(extension)
                    .is_some_and(|rest| rest.ends_with('.'));
                if matches && best.is_none_or(|(length, _)| extension.len() > length) {
                    best = Some((extension.len(), format));
                }
            }
        }
        best.map(|(_, format)| format)
    }

    /// Whether BoxIt creates archives of this format, not only reads them.
    pub fn can_write(self) -> bool {
        !matches!(
            self,
            Self::Rar | Self::Cab | Self::Iso | Self::Ar | Self::Cpio | Self::Lha | Self::Wim
        )
    }

    /// The compression around the whole content, for a compressed tar and
    /// for a single compressed file.
    pub fn stream(self) -> Option<Stream> {
        match self {
            Self::TarGz | Self::Gz => Some(Stream::Gzip),
            Self::TarBz2 | Self::Bz2 => Some(Stream::Bzip2),
            Self::TarXz | Self::Xz => Some(Stream::Xz),
            Self::TarZst | Self::Zst => Some(Stream::Zstd),
            Self::TarLz4 | Self::Lz4 => Some(Stream::Lz4),
            _ => None,
        }
    }

    /// One compressed file rather than a container of many.
    pub fn is_single_file(self) -> bool {
        matches!(
            self,
            Self::Gz | Self::Bz2 | Self::Xz | Self::Zst | Self::Lz4
        )
    }

    pub fn is_tar(self) -> bool {
        matches!(
            self,
            Self::Tar | Self::TarGz | Self::TarBz2 | Self::TarXz | Self::TarZst | Self::TarLz4
        )
    }

    /// A tar inside this compression, or one file compressed by it.
    pub fn of_stream(stream: Stream, tar: bool) -> Self {
        match (stream, tar) {
            (Stream::Gzip, true) => Self::TarGz,
            (Stream::Gzip, false) => Self::Gz,
            (Stream::Bzip2, true) => Self::TarBz2,
            (Stream::Bzip2, false) => Self::Bz2,
            (Stream::Xz, true) => Self::TarXz,
            (Stream::Xz, false) => Self::Xz,
            (Stream::Zstd, true) => Self::TarZst,
            (Stream::Zstd, false) => Self::Zst,
            (Stream::Lz4, true) => Self::TarLz4,
            (Stream::Lz4, false) => Self::Lz4,
        }
    }
}

/// What the first bytes of a file say it is.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Signature {
    Zip,
    SevenZ,
    Tar,
    Stream(Stream),
    Rar,
    Cab,
    Iso,
    Ar,
    Cpio,
    Lha,
    Wim,
}

/// How many bytes from the start of a file [`sniff`] can make use of: the
/// mark of an ISO image sits after its 32 KiB system area.
pub const SNIFF_LENGTH: usize = 0x8006;

const TAR_BLOCK: usize = 512;

/// Whether the block is a tar header: its checksum field holds the sum of
/// its bytes. This also recognizes tar archives older than the `ustar` mark.
fn is_tar_header(block: &[u8]) -> bool {
    let Some(block) = block.get(..TAR_BLOCK) else {
        return false;
    };
    let field = &block[148..156];
    let digits: Vec<u8> = field
        .iter()
        .copied()
        .skip_while(|byte| *byte == b' ')
        .take_while(|byte| (b'0'..=b'7').contains(byte))
        .collect();
    if digits.is_empty() {
        return false;
    }
    let stored = digits
        .iter()
        .fold(0u32, |sum, digit| sum * 8 + u32::from(digit - b'0'));
    // The field itself counts as eight spaces.
    let unsigned: u32 = block.iter().map(|byte| u32::from(*byte)).sum::<u32>()
        - field.iter().map(|byte| u32::from(*byte)).sum::<u32>()
        + 8 * u32::from(b' ');
    // Some old programs summed the bytes as signed numbers.
    let signed: i32 = block.iter().map(|byte| i32::from(*byte as i8)).sum::<i32>()
        - field.iter().map(|byte| i32::from(*byte as i8)).sum::<i32>()
        + 8 * i32::from(b' ');
    stored == unsigned || i64::from(stored) == i64::from(signed)
}

fn is_lha_header(head: &[u8]) -> bool {
    // "-lh0-" ... "-lh7-", "-lhd-", "-lzs-", "-lz4-", "-lz5-" after two bytes.
    head.get(2..7).is_some_and(|mark| {
        mark[0] == b'-'
            && mark[1] == b'l'
            && (mark[2] == b'h' || mark[2] == b'z')
            && mark[3].is_ascii_alphanumeric()
            && mark[4] == b'-'
    })
}

/// Recognizes a format by the first bytes of a file. `head` should hold up to
/// [`SNIFF_LENGTH`] bytes; a shorter one recognizes what it can.
pub fn sniff(head: &[u8]) -> Option<Signature> {
    const MARKS: &[(&[u8], Signature)] = &[
        (b"PK\x03\x04", Signature::Zip),
        // An archive with no entries starts with the end record.
        (b"PK\x05\x06", Signature::Zip),
        // The first volume of a split archive.
        (b"PK\x07\x08", Signature::Zip),
        (b"7z\xBC\xAF\x27\x1C", Signature::SevenZ),
        (b"\x1F\x8B", Signature::Stream(Stream::Gzip)),
        (b"\xFD7zXZ\x00", Signature::Stream(Stream::Xz)),
        (b"\x28\xB5\x2F\xFD", Signature::Stream(Stream::Zstd)),
        (b"\x04\x22\x4D\x18", Signature::Stream(Stream::Lz4)),
        // The frame format LZ4 had before the current one.
        (b"\x02\x21\x4C\x18", Signature::Stream(Stream::Lz4)),
        (b"Rar!\x1A\x07", Signature::Rar),
        (b"MSCF", Signature::Cab),
        (b"!<arch>\n", Signature::Ar),
        (b"MSWIM\x00\x00\x00", Signature::Wim),
        (b"070701", Signature::Cpio),
        (b"070702", Signature::Cpio),
        (b"070707", Signature::Cpio),
    ];
    for (mark, signature) in MARKS {
        if head.starts_with(mark) {
            return Some(*signature);
        }
    }
    if let [b'B', b'Z', b'h', b'1'..=b'9', ..] = head {
        return Some(Signature::Stream(Stream::Bzip2));
    }
    // A zstd stream may start with a frame that decoders skip.
    if let [0x50..=0x5F, 0x2A, 0x4D, 0x18, ..] = head {
        return Some(Signature::Stream(Stream::Zstd));
    }
    if is_tar_header(head) {
        return Some(Signature::Tar);
    }
    // "CD001" in the volume descriptor that follows the system area; a
    // disc with UDF alone has the mark of its own descriptors there.
    if let Some(b"CD001" | b"BEA01" | b"NSR02" | b"NSR03") = head.get(0x8001..0x8006) {
        return Some(Signature::Iso);
    }
    if is_lha_header(head) {
        return Some(Signature::Lha);
    }
    // The binary form of cpio, in either byte order. Two bytes are a weak
    // mark, so it is tried after everything else.
    if let [0xC7, 0x71, ..] | [0x71, 0xC7, ..] = head {
        return Some(Signature::Cpio);
    }
    None
}

impl Signature {
    /// The format of a file with this signature. The name decides what the
    /// bytes leave open: whether a compressed stream holds a tar archive.
    pub fn format(self, name: &str) -> ArchiveFormat {
        match self {
            Self::Zip => ArchiveFormat::Zip,
            Self::SevenZ => ArchiveFormat::SevenZ,
            Self::Tar => ArchiveFormat::Tar,
            Self::Stream(stream) => {
                let tar = ArchiveFormat::from_name(name).is_some_and(ArchiveFormat::is_tar);
                ArchiveFormat::of_stream(stream, tar)
            }
            Self::Rar => ArchiveFormat::Rar,
            Self::Cab => ArchiveFormat::Cab,
            Self::Iso => ArchiveFormat::Iso,
            Self::Ar => ArchiveFormat::Ar,
            Self::Cpio => ArchiveFormat::Cpio,
            Self::Lha => ArchiveFormat::Lha,
            Self::Wim => ArchiveFormat::Wim,
        }
    }
}

/// Whether decompressed bytes start a tar archive: tells a compressed tar
/// from a single compressed file whatever the name of the file says.
pub fn starts_tar(decompressed: &[u8]) -> bool {
    is_tar_header(decompressed)
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;

    use super::*;

    fn tar_header(name: &str) -> Vec<u8> {
        let mut block = vec![0u8; TAR_BLOCK];
        block[..name.len()].copy_from_slice(name.as_bytes());
        block[100..107].copy_from_slice(b"0000644");
        block[148..156].copy_from_slice(b"        ");
        let sum: u32 = block.iter().map(|byte| u32::from(*byte)).sum();
        block[148..155].copy_from_slice(format!("{sum:06o}\0").as_bytes());
        block
    }

    #[test]
    fn ids_are_unique_and_round_trip() {
        for format in ArchiveFormat::ALL {
            assert_eq!(ArchiveFormat::from_id(format.id()), Some(format));
            let json = serde_json::to_string(&format).unwrap();
            assert_eq!(json, format!("\"{}\"", format.id()));
            assert_eq!(
                serde_json::from_str::<ArchiveFormat>(&json).unwrap(),
                format
            );
        }
        assert_eq!(ArchiveFormat::from_id("nope"), None);
    }

    #[test]
    fn an_extension_belongs_to_one_format() {
        let mut seen = BTreeMap::new();
        for format in ArchiveFormat::ALL {
            for extension in format.extensions() {
                assert_eq!(seen.insert(*extension, format), None, "{extension}");
            }
        }
    }

    #[test]
    fn the_longest_extension_wins() {
        assert_eq!(
            ArchiveFormat::from_name("a.tar.gz"),
            Some(ArchiveFormat::TarGz)
        );
        assert_eq!(
            ArchiveFormat::from_name("A.TGZ"),
            Some(ArchiveFormat::TarGz)
        );
        assert_eq!(ArchiveFormat::from_name("a.gz"), Some(ArchiveFormat::Gz));
        assert_eq!(ArchiveFormat::from_name("a.tar"), Some(ArchiveFormat::Tar));
        assert_eq!(
            ArchiveFormat::from_name("a.b.7z"),
            Some(ArchiveFormat::SevenZ)
        );
        assert_eq!(
            ArchiveFormat::from_name("a.tar.zst"),
            Some(ArchiveFormat::TarZst)
        );
        assert_eq!(
            ArchiveFormat::from_name("libfoo.a"),
            Some(ArchiveFormat::Ar)
        );
        // An extension is what follows a dot, not any ending of the name.
        assert_eq!(ArchiveFormat::from_name("data"), None);
        assert_eq!(ArchiveFormat::from_name("gz"), None);
        assert_eq!(ArchiveFormat::from_name("notes.txt"), None);
    }

    #[test]
    fn formats_are_recognized_by_their_first_bytes() {
        let cases: &[(&[u8], Signature)] = &[
            (b"PK\x03\x04rest", Signature::Zip),
            (b"PK\x05\x06", Signature::Zip),
            (b"7z\xBC\xAF\x27\x1C\x00\x04", Signature::SevenZ),
            (b"\x1F\x8B\x08\x00", Signature::Stream(Stream::Gzip)),
            (b"BZh91AY&SY", Signature::Stream(Stream::Bzip2)),
            (b"\xFD7zXZ\x00\x00", Signature::Stream(Stream::Xz)),
            (b"\x28\xB5\x2F\xFD\x24", Signature::Stream(Stream::Zstd)),
            (b"\x50\x2A\x4D\x18\x00", Signature::Stream(Stream::Zstd)),
            (b"\x04\x22\x4D\x18\x64", Signature::Stream(Stream::Lz4)),
            (b"Rar!\x1A\x07\x00", Signature::Rar),
            (b"Rar!\x1A\x07\x01\x00", Signature::Rar),
            (b"MSCF\x00\x00\x00\x00", Signature::Cab),
            (b"!<arch>\ndebian-binary", Signature::Ar),
            (b"MSWIM\x00\x00\x00\xD0", Signature::Wim),
            (b"070701000000", Signature::Cpio),
            (b"\xC7\x71\x00\x00", Signature::Cpio),
            (b"\x20\x00-lh5-\x00", Signature::Lha),
        ];
        for (head, signature) in cases {
            assert_eq!(sniff(head), Some(*signature), "{head:?}");
        }
        assert_eq!(sniff(b""), None);
        assert_eq!(sniff(b"BZh0"), None);
        assert_eq!(sniff(b"plain text, nothing else"), None);
        assert_eq!(sniff(&[0u8; 4096]), None);
    }

    #[test]
    fn tar_is_recognized_by_the_checksum_of_its_header() {
        let header = tar_header("file.txt");
        assert_eq!(sniff(&header), Some(Signature::Tar));
        assert!(starts_tar(&header));
        let mut damaged = header.clone();
        damaged[0] ^= 1;
        assert_eq!(sniff(&damaged), None);
        assert_eq!(sniff(&header[..TAR_BLOCK - 1]), None);
    }

    #[test]
    fn an_iso_image_is_recognized_past_its_system_area() {
        let mut image = vec![0u8; SNIFF_LENGTH];
        image[0x8000] = 1;
        image[0x8001..0x8006].copy_from_slice(b"CD001");
        assert_eq!(sniff(&image), Some(Signature::Iso));
        assert_eq!(sniff(&image[..0x8005]), None);
        // A disc with UDF and nothing older.
        image[0x8000] = 0;
        image[0x8001..0x8006].copy_from_slice(b"BEA01");
        assert_eq!(sniff(&image), Some(Signature::Iso));
        image[0x8001..0x8006].copy_from_slice(b"BEA02");
        assert_eq!(sniff(&image), None);
    }

    #[test]
    fn the_name_settles_what_a_compressed_stream_holds() {
        let gzip = Signature::Stream(Stream::Gzip);
        assert_eq!(gzip.format("a.tar.gz"), ArchiveFormat::TarGz);
        assert_eq!(gzip.format("a.tgz"), ArchiveFormat::TarGz);
        assert_eq!(gzip.format("a.gz"), ArchiveFormat::Gz);
        assert_eq!(gzip.format("a.zip"), ArchiveFormat::Gz);
        // The bytes win over the name.
        assert_eq!(Signature::Rar.format("a.zip"), ArchiveFormat::Rar);
        for format in ArchiveFormat::ALL {
            if let Some(stream) = format.stream() {
                assert_eq!(ArchiveFormat::of_stream(stream, format.is_tar()), format);
                assert_ne!(format.is_tar(), format.is_single_file());
            }
        }
    }
}

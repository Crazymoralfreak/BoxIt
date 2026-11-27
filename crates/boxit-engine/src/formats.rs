//! What the formats offer to the dialogs of the window: which of them can
//! be created, and which of the options of a new archive mean something
//! for each.

use boxit_codecs::names::{NameEncoding, NAME_ENCODINGS};
use boxit_codecs::plan;
use boxit_core::{ArchiveFormat, CreateOptions, Error, Method, Result, Stream};
use serde::Serialize;

/// A format as the dialogs see it.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct FormatInfo {
    pub id: ArchiveFormat,
    /// Extensions of the files of the format, the main one first.
    pub extensions: &'static [&'static str],
    /// Archives of the format can be created, not only read.
    pub can_write: bool,
    /// The format holds one compressed file, not files and folders.
    pub single_file: bool,
    /// The methods to choose from, the usual one first; none for a format
    /// that has one method.
    pub methods: &'static [Method],
    /// The method an archive is compressed with unless one is chosen.
    pub method: Method,
    /// The methods the size of the dictionary can be chosen for.
    pub dictionary: Vec<Method>,
    /// The level of compression means something.
    pub levels: bool,
    pub encrypt: bool,
    /// The names of the files can be hidden as well as their content.
    pub encrypt_names: bool,
    /// Files can be compressed together or apart.
    pub solid: bool,
    pub comment: bool,
}

/// Whether the dictionary of this method of this format has a size to
/// choose.
fn has_dictionary(format: ArchiveFormat, method: Method) -> bool {
    match format {
        ArchiveFormat::SevenZ => matches!(method, Method::Lzma | Method::Lzma2),
        _ => matches!(format.stream(), Some(Stream::Xz | Stream::Zstd)),
    }
}

fn info(format: ArchiveFormat) -> FormatInfo {
    let methods = boxit_codecs::methods(format);
    let method = plan::method(&CreateOptions::new(format));
    let offered = if methods.is_empty() {
        std::slice::from_ref(&method)
    } else {
        methods
    };
    FormatInfo {
        id: format,
        extensions: format.extensions(),
        can_write: format.can_write(),
        single_file: format.is_single_file(),
        methods,
        method,
        dictionary: offered
            .iter()
            .copied()
            .filter(|method| has_dictionary(format, *method))
            .collect(),
        levels: format != ArchiveFormat::Tar,
        encrypt: boxit_codecs::can_encrypt(format),
        encrypt_names: format == ArchiveFormat::SevenZ,
        solid: format == ArchiveFormat::SevenZ,
        comment: format == ArchiveFormat::Zip,
    }
}

/// Every format BoxIt knows, the ones it creates first.
pub fn formats() -> Vec<FormatInfo> {
    let mut formats: Vec<FormatInfo> = ArchiveFormat::ALL.into_iter().map(info).collect();
    formats.sort_by_key(|format| !format.can_write);
    formats
}

/// The encodings offered for the names of an archive, by their labels.
pub fn name_encodings() -> &'static [&'static str] {
    NAME_ENCODINGS
}

/// The encoding a label stands for. No label, or `auto`, leaves the choice
/// to BoxIt.
pub fn name_encoding(label: Option<&str>) -> Result<NameEncoding> {
    match label.map(str::trim) {
        None | Some("") => Ok(NameEncoding::Auto),
        Some(label) => NameEncoding::from_label(label)
            .ok_or_else(|| Error::new("unknownEncoding").with("name", label)),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn of(format: ArchiveFormat) -> FormatInfo {
        formats()
            .into_iter()
            .find(|info| info.id == format)
            .unwrap()
    }

    #[test]
    fn every_format_is_told_about_and_the_ones_created_come_first() {
        let formats = formats();
        assert_eq!(formats.len(), ArchiveFormat::ALL.len());
        assert_eq!(formats[0].id, ArchiveFormat::Zip);
        let first_read_only = formats.iter().position(|info| !info.can_write).unwrap();
        assert!(formats[first_read_only..]
            .iter()
            .all(|info| !info.can_write));
        assert!(formats[..first_read_only].iter().all(|info| info.can_write));
        for info in &formats {
            assert!(!info.extensions.is_empty(), "{:?}", info.id);
            // What is offered is what the format compresses with by default,
            // or a choice that starts with it.
            assert!(
                info.methods.is_empty() || info.methods[0] == info.method,
                "{:?}",
                info.id
            );
            assert!(!info.encrypt_names || info.encrypt, "{:?}", info.id);
        }
    }

    #[test]
    fn the_options_of_a_format_are_the_ones_it_honours() {
        let zip = of(ArchiveFormat::Zip);
        assert!(zip.encrypt && zip.comment && !zip.solid && !zip.encrypt_names);
        assert_eq!(zip.dictionary, []);
        assert!(zip.methods.contains(&Method::Zstd));

        let seven = of(ArchiveFormat::SevenZ);
        assert!(seven.encrypt && seven.encrypt_names && seven.solid && !seven.comment);
        assert_eq!(seven.dictionary, [Method::Lzma2, Method::Lzma]);

        let tar = of(ArchiveFormat::Tar);
        assert!(!tar.levels && !tar.encrypt && tar.methods.is_empty());
        assert_eq!(tar.method, Method::Store);

        assert_eq!(of(ArchiveFormat::TarXz).dictionary, [Method::Xz]);
        assert_eq!(of(ArchiveFormat::Zst).dictionary, [Method::Zstd]);
        assert_eq!(of(ArchiveFormat::TarGz).dictionary, []);
        assert!(of(ArchiveFormat::Gz).single_file && !of(ArchiveFormat::TarGz).single_file);
        assert!(!of(ArchiveFormat::Rar).can_write);
    }

    #[test]
    fn the_encodings_offered_are_ones_the_names_can_be_read_in() {
        for label in name_encodings() {
            assert!(NameEncoding::from_label(label).is_some(), "{label}");
            assert_ne!(
                name_encoding(Some(label)),
                Ok(NameEncoding::Auto),
                "{label}"
            );
        }
        assert_eq!(name_encoding(None), Ok(NameEncoding::Auto));
        assert_eq!(name_encoding(Some(" ")), Ok(NameEncoding::Auto));
        assert_eq!(name_encoding(Some("auto")), Ok(NameEncoding::Auto));
        assert_eq!(
            name_encoding(Some("no-such-thing")).unwrap_err().code,
            "unknownEncoding"
        );
    }
}

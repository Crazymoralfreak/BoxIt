//! Names of entries written before archives agreed on UTF-8.
//!
//! A ZIP entry either says its name is UTF-8 or says nothing, and then the
//! name is in whatever code page the program that made the archive used:
//! CP866 for a Russian DOS or Windows, Shift-JIS for a Japanese one, CP437 by
//! the letter of the format. The names of an archive are looked at together
//! and the code page that makes the most sense of them is taken; the user can
//! overrule the guess.

use std::borrow::Cow;

use chardetng::{EncodingDetector, Iso2022JpDetection, Utf8Detection};
use encoding_rs::{Encoding, WINDOWS_1252};

/// The upper half of code page 437, the one the ZIP format names.
const CP437_HIGH: &str = "ÇüéâäàåçêëèïîìÄÅÉæÆôöòûùÿÖÜ¢£¥₧ƒáíóúñÑªº¿⌐¬½¼¡«»░▒▓│┤╡╢╖╕╣║╗╝╜╛┐└┴┬├─┼╞╟╚╔╩╦╠═╬╧╨╤╥╙╘╒╓╫╪┘┌█▄▌▐▀αßΓπΣσµτΦΘΩδ∞φε∩≡±≥≤⌠⌡÷≈°∙·√ⁿ²■\u{a0}";

/// The label of code page 437 among the encodings a user can choose.
pub const CP437: &str = "cp437";

/// Encodings offered for a manual choice, by the labels [`NameEncoding::from_label`]
/// takes; the first ones are those archives are most often found in.
pub const NAME_ENCODINGS: &[&str] = &[
    "utf-8",
    CP437,
    "ibm866",
    "windows-1251",
    "koi8-r",
    "windows-1252",
    "windows-1250",
    "shift_jis",
    "euc-jp",
    "gbk",
    "big5",
    "euc-kr",
    "windows-1253",
    "windows-1254",
    "windows-1255",
    "windows-1256",
    "windows-1257",
    "windows-1258",
    "windows-874",
];

/// How names that do not say they are UTF-8 are read.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum NameEncoding {
    /// Guess from the names of the archive.
    #[default]
    Auto,
    Cp437,
    /// One of the encodings of the Encoding Standard.
    Named(&'static Encoding),
}

impl NameEncoding {
    /// The encoding with this label: `cp437`, or any label of the Encoding
    /// Standard such as `ibm866`, `windows-1251`, `shift_jis`.
    pub fn from_label(label: &str) -> Option<Self> {
        let label = label.trim();
        if label.eq_ignore_ascii_case("auto") {
            return Some(Self::Auto);
        }
        if label.eq_ignore_ascii_case(CP437) || label.eq_ignore_ascii_case("ibm437") {
            return Some(Self::Cp437);
        }
        Encoding::for_label(label.as_bytes()).map(Self::Named)
    }

    pub fn label(self) -> String {
        match self {
            Self::Auto => String::from("auto"),
            Self::Cp437 => String::from(CP437),
            Self::Named(encoding) => encoding.name().to_lowercase(),
        }
    }
}

fn decode_cp437(raw: &[u8]) -> String {
    let mut high = [' '; 128];
    for (slot, character) in high.iter_mut().zip(CP437_HIGH.chars()) {
        *slot = character;
    }
    raw.iter()
        .map(|byte| match byte {
            0..=0x7F => char::from(*byte),
            _ => high[usize::from(*byte - 0x80)],
        })
        .collect()
}

/// Reads the names of one archive.
#[derive(Clone, Copy, Debug)]
pub struct NameDecoder {
    encoding: NameEncoding,
    /// The encoding is a guess, not the choice of the user.
    guessed: bool,
}

impl NameDecoder {
    /// A decoder for an archive with these raw names. `hint` is a top-level
    /// domain such as `ru` or `jp` that tells where the user is from; it
    /// settles the guess between code pages that fit equally well.
    pub fn new<'a>(
        choice: NameEncoding,
        names: impl IntoIterator<Item = &'a [u8]>,
        hint: Option<&str>,
    ) -> Self {
        match choice {
            NameEncoding::Auto => Self {
                encoding: guess(names, hint),
                guessed: true,
            },
            chosen => Self {
                encoding: chosen,
                guessed: false,
            },
        }
    }

    /// The encoding names without the UTF-8 mark are read in.
    pub fn encoding(&self) -> NameEncoding {
        self.encoding
    }

    /// Decodes a name. `utf8` is what the entry says about itself.
    pub fn decode<'a>(&self, raw: &'a [u8], utf8: bool) -> Cow<'a, str> {
        if utf8 || raw.is_ascii() {
            return String::from_utf8_lossy(raw);
        }
        // Programs on Linux and macOS write UTF-8 without saying so; a guess
        // made from the other names of the archive does not apply to it.
        if self.guessed {
            if let Ok(name) = std::str::from_utf8(raw) {
                return Cow::Borrowed(name);
            }
        }
        match self.encoding {
            NameEncoding::Auto | NameEncoding::Cp437 => Cow::Owned(decode_cp437(raw)),
            NameEncoding::Named(encoding) => encoding.decode_without_bom_handling(raw).0,
        }
    }
}

/// The hint in the only form the detector takes, which panics on any
/// other: a label of a domain in lower case. A hint that is not one is no
/// hint.
fn domain(hint: Option<&str>) -> Option<Vec<u8>> {
    let hint = hint?.trim().to_ascii_lowercase();
    let fits = !hint.is_empty()
        && hint
            .bytes()
            .all(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit() || byte == b'-');
    fits.then(|| hint.into_bytes())
}

/// Guesses the code page of the names that are neither ASCII nor UTF-8.
fn guess<'a>(names: impl IntoIterator<Item = &'a [u8]>, hint: Option<&str>) -> NameEncoding {
    let mut detector = EncodingDetector::new(Iso2022JpDetection::Deny);
    let mut any = false;
    for name in names {
        if name.is_ascii() || std::str::from_utf8(name).is_ok() {
            continue;
        }
        any = true;
        detector.feed(name, false);
        detector.feed(b"\n", false);
    }
    if !any {
        return NameEncoding::Auto;
    }
    detector.feed(b"", true);
    let guessed = detector.guess(domain(hint).as_deref(), Utf8Detection::Deny);
    // Windows-1252 is what the detector says when nothing stands out; the
    // format says such names are code page 437.
    if guessed == WINDOWS_1252 {
        NameEncoding::Cp437
    } else {
        NameEncoding::Named(guessed)
    }
}

#[cfg(test)]
mod tests {
    use encoding_rs::{IBM866, SHIFT_JIS, WINDOWS_1251};

    use super::*;

    fn decoder(names: &[&[u8]], hint: Option<&str>) -> NameDecoder {
        NameDecoder::new(NameEncoding::Auto, names.iter().copied(), hint)
    }

    #[test]
    fn code_page_437_has_all_its_characters() {
        assert_eq!(CP437_HIGH.chars().count(), 128);
        assert_eq!(decode_cp437(b"caf\x82"), "café");
        assert_eq!(decode_cp437(b"\x80\xFF\xE1"), "Ç\u{a0}ß");
        assert_eq!(decode_cp437(b"plain.txt"), "plain.txt");
    }

    #[test]
    fn names_that_say_utf8_are_utf8() {
        let decoder = decoder(&[], None);
        assert_eq!(decoder.decode("привет.txt".as_bytes(), true), "привет.txt");
        // Without the mark, valid UTF-8 is still taken for what it is.
        assert_eq!(decoder.decode("привет.txt".as_bytes(), false), "привет.txt");
        assert_eq!(decoder.decode(b"a/b.txt", false), "a/b.txt");
        assert_eq!(decoder.encoding(), NameEncoding::Auto);
    }

    #[test]
    fn russian_names_of_dos_are_recognized() {
        let (first, _, _) = IBM866.encode("Отчёт за квартал.docx");
        let (second, _, _) = IBM866.encode("Документы/Договор аренды.pdf");
        let (third, _, _) = IBM866.encode("Фотографии/Отпуск/море.jpg");
        let decoder = decoder(&[&first, &second, &third], Some("ru"));
        assert_eq!(decoder.encoding(), NameEncoding::Named(IBM866));
        assert_eq!(
            decoder.decode(&second, false),
            "Документы/Договор аренды.pdf"
        );
    }

    #[test]
    fn russian_names_of_windows_are_recognized() {
        let (first, _, _) = WINDOWS_1251.encode("Отчёт за квартал.docx");
        let (second, _, _) = WINDOWS_1251.encode("Документы/Договор аренды.pdf");
        let (third, _, _) = WINDOWS_1251.encode("Фотографии/Отпуск/море.jpg");
        let decoder = decoder(&[&first, &second, &third], Some("ru"));
        assert_eq!(decoder.encoding(), NameEncoding::Named(WINDOWS_1251));
        assert_eq!(decoder.decode(&first, false), "Отчёт за квартал.docx");
    }

    #[test]
    fn japanese_names_are_recognized() {
        let (first, _, _) = SHIFT_JIS.encode("新しいフォルダー/報告書.txt");
        let (second, _, _) = SHIFT_JIS.encode("写真/東京タワー.jpg");
        let decoder = decoder(&[&first, &second], Some("jp"));
        assert_eq!(decoder.encoding(), NameEncoding::Named(SHIFT_JIS));
        assert_eq!(decoder.decode(&second, false), "写真/東京タワー.jpg");
    }

    #[test]
    fn a_chosen_encoding_overrules_the_guess() {
        let (name, _, _) = IBM866.encode("Привет.txt");
        let choice = NameEncoding::from_label("IBM866").unwrap();
        let decoder = NameDecoder::new(choice, [&name[..]], None);
        assert_eq!(decoder.decode(&name, false), "Привет.txt");
        let wrong = NameDecoder::new(NameEncoding::Cp437, [&name[..]], None);
        assert_ne!(wrong.decode(&name, false), "Привет.txt");
        // The mark of the entry still wins over the choice.
        assert_eq!(decoder.decode("Привет.txt".as_bytes(), true), "Привет.txt");
    }

    #[test]
    fn every_offered_encoding_has_a_label_that_resolves() {
        for label in NAME_ENCODINGS {
            let encoding = NameEncoding::from_label(label).unwrap_or_else(|| panic!("{label}"));
            assert_eq!(NameEncoding::from_label(&encoding.label()), Some(encoding));
        }
        assert_eq!(NameEncoding::from_label("auto"), Some(NameEncoding::Auto));
        assert_eq!(NameEncoding::from_label("no-such-thing"), None);
    }

    #[test]
    fn a_hint_that_is_not_a_domain_is_no_hint() {
        let (name, _, _) = IBM866.encode("Документы/Договор аренды.pdf");
        for hint in ["RU", " ru ", "ru-RU"] {
            assert_ne!(decoder(&[&name], Some(hint)).encoding(), NameEncoding::Auto);
        }
        // The detector would panic on each of these.
        for hint in ["", "ru.RU", "ru_RU", "рф", "a b"] {
            assert_eq!(domain(Some(hint)), None, "{hint:?}");
            assert_ne!(decoder(&[&name], Some(hint)).encoding(), NameEncoding::Auto);
        }
    }

    #[test]
    fn western_names_without_a_mark_follow_the_format() {
        // "café.txt" as an old DOS program wrote it.
        let name = b"caf\x82.txt";
        let decoder = decoder(&[name], None);
        assert_eq!(decoder.decode(name, false), "café.txt");
    }
}

//! Texts shown outside the web view: the command line, system dialogs,
//! notifications, the context menu of the file manager. They come from the
//! same dictionaries as the interface, `src/i18n/*.json`.

use std::sync::LazyLock;

use boxit_core::Error;
use serde_json::Value;

static RU: LazyLock<Value> = LazyLock::new(|| parse(include_str!("../../../src/i18n/ru.json")));
static EN: LazyLock<Value> = LazyLock::new(|| parse(include_str!("../../../src/i18n/en.json")));

/// Languages BoxIt has a dictionary for.
pub const LANGUAGES: &[&str] = &["ru", "en"];

/// The language of everything that has no dictionary.
const FALLBACK: &str = "en";

fn parse(text: &str) -> Value {
    serde_json::from_str(text.trim_start_matches('\u{feff}')).unwrap_or(Value::Null)
}

fn dictionary(lang: &str) -> &'static Value {
    match lang {
        "ru" => &RU,
        _ => &EN,
    }
}

/// Whether BoxIt has a dictionary for this language code.
pub fn known_language(lang: &str) -> bool {
    LANGUAGES.contains(&lang)
}

/// The dictionary language for a locale name such as `ru-RU` or
/// `ru_RU.UTF-8`: its primary language when there is a dictionary for it,
/// English for any other.
pub fn language_of(locale: &str) -> &'static str {
    let primary = locale
        .split(['-', '_', '.', '@'])
        .next()
        .unwrap_or_default()
        .to_lowercase();
    LANGUAGES
        .iter()
        .find(|known| **known == primary)
        .copied()
        .unwrap_or(FALLBACK)
}

/// Language of the interface of the operating system when BoxIt has a
/// dictionary for it, English for any other.
pub fn system_language() -> &'static str {
    sys_locale::get_locale().map_or(FALLBACK, |locale| language_of(&locale))
}

/// The country of a locale name such as `ru-RU` or `zh_Hans_CN.UTF-8`, as
/// the top-level domain it has; the language where the name has no country.
pub fn region_of(locale: &str) -> Option<String> {
    let mut parts = locale
        .split(['.', '@'])
        .next()
        .unwrap_or_default()
        .split(['-', '_']);
    let language = parts.next().unwrap_or_default();
    let two_letters =
        |part: &&str| part.len() == 2 && part.bytes().all(|b| b.is_ascii_alphabetic());
    parts
        .find(two_letters)
        .or(Some(language).filter(two_letters))
        .map(str::to_ascii_lowercase)
}

/// Where the user is from, as far as the operating system tells: the hint
/// that settles the code page of old names in an archive.
pub fn system_region() -> Option<String> {
    sys_locale::get_locale().and_then(|locale| region_of(&locale))
}

fn lookup<'a>(dictionary: &'a Value, key: &str) -> Option<&'a str> {
    key.split('.')
        .try_fold(dictionary, |node, part| node.get(part))?
        .as_str()
}

fn fill(template: &str, params: &[(&str, &str)]) -> String {
    params
        .iter()
        .fold(template.to_owned(), |text, (key, value)| {
            text.replace(&format!("{{{key}}}"), value)
        })
}

/// Text for a key such as `native.dialog.openArchive`. A key missing in the
/// language falls back to English, a key missing everywhere is shown as it is.
pub fn text(lang: &str, key: &str, params: &[(&str, &str)]) -> String {
    let template = lookup(dictionary(lang), key)
        .or_else(|| lookup(&EN, key))
        .unwrap_or(key);
    fill(template, params)
}

/// Parameters worth showing even when the text of the error has no place for
/// them.
const ERROR_DETAILS: &[&str] = &["path", "name", "detail"];

/// The message of an error in the given language.
pub fn error_text(lang: &str, error: &Error) -> String {
    let key = format!("errors.{}", error.code);
    let Some(template) = lookup(dictionary(lang), &key).or_else(|| lookup(&EN, &key)) else {
        return text(lang, "errors.unknown", &[("message", &error.to_string())]);
    };
    let params: Vec<(&str, &str)> = error
        .params
        .iter()
        .map(|(key, value)| (key.as_str(), value.as_str()))
        .collect();
    let message = fill(template, &params);
    let details: Vec<&str> = ERROR_DETAILS
        .iter()
        .filter(|name| !template.contains(&format!("{{{name}}}")))
        .filter_map(|name| error.params.get(*name).map(String::as_str))
        .collect();
    if details.is_empty() {
        message
    } else {
        format!("{message} ({})", details.join(", "))
    }
}

#[cfg(test)]
mod tests {
    use std::collections::{BTreeMap, BTreeSet};
    use std::fs;
    use std::path::{Path, PathBuf};

    use super::*;

    /// Codes that never appear as a literal in a call of `Error::new`: the
    /// ones an I/O error turns into.
    const INDIRECT_CODES: &[&str] = &[
        "cancelled",
        "notFound",
        "accessDenied",
        "diskFull",
        "truncated",
        "io",
        // Raised by `boxit-security` through a helper that takes the code.
        "invalidName",
        "absolutePath",
        "pathTraversal",
        // What the formats make of damage once they know the entry.
        "damagedEntry",
        "damagedOrWrongPassword",
    ];

    /// `{name}` placeholders that appear in a string, so word order may differ.
    fn placeholders(text: &str) -> BTreeSet<&str> {
        let mut names = BTreeSet::new();
        let mut rest = text;
        while let Some(start) = rest.find('{') {
            let after = &rest[start + 1..];
            let Some(end) = after.find('}') else {
                break;
            };
            let name = &after[..end];
            if !name.is_empty()
                && name
                    .chars()
                    .all(|ch| ch.is_ascii_alphanumeric() || ch == '_')
            {
                names.insert(name);
            }
            rest = &after[end + 1..];
        }
        names
    }

    fn leaf_strings(value: &Value, prefix: &str, out: &mut BTreeMap<String, String>) {
        match value {
            Value::Object(map) => {
                for (key, child) in map {
                    let path = if prefix.is_empty() {
                        key.clone()
                    } else {
                        format!("{prefix}.{key}")
                    };
                    leaf_strings(child, &path, out);
                }
            }
            Value::String(text) => {
                out.insert(prefix.to_owned(), text.clone());
            }
            _ => {}
        }
    }

    fn rust_sources(folder: &Path, out: &mut Vec<PathBuf>) {
        let Ok(entries) = fs::read_dir(folder) else {
            return;
        };
        for entry in entries.flatten() {
            let path = entry.path();
            let name = entry.file_name();
            if path.is_dir() {
                if name != "target" && name != "node_modules" && name != "gen" {
                    rust_sources(&path, out);
                }
            } else if path.extension().is_some_and(|extension| extension == "rs") {
                out.push(path);
            }
        }
    }

    /// Every error code the Rust sources of the repository can raise.
    fn error_codes() -> BTreeSet<String> {
        let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("../..");
        let mut files = Vec::new();
        rust_sources(&root.join("crates"), &mut files);
        rust_sources(&root.join("src-tauri").join("src"), &mut files);
        assert!(!files.is_empty(), "the sources were not found");
        let mut codes: BTreeSet<String> = INDIRECT_CODES
            .iter()
            .map(|code| (*code).to_owned())
            .collect();
        let marker = "Error::new(\"";
        for file in files {
            let source = fs::read_to_string(&file).unwrap();
            let mut rest = source.as_str();
            while let Some(start) = rest.find(marker) {
                let after = &rest[start + marker.len()..];
                let Some(end) = after.find('"') else {
                    break;
                };
                codes.insert(after[..end].to_owned());
                rest = &after[end..];
            }
        }
        codes
    }

    #[test]
    fn every_dictionary_has_the_same_keys_and_placeholders() {
        let mut english = BTreeMap::new();
        leaf_strings(&EN, "", &mut english);
        assert!(!english.is_empty(), "en.json is not valid JSON");
        for lang in LANGUAGES {
            let mut leaves = BTreeMap::new();
            leaf_strings(dictionary(lang), "", &mut leaves);
            assert_eq!(
                leaves.keys().collect::<Vec<_>>(),
                english.keys().collect::<Vec<_>>(),
                "{lang} has other keys than en"
            );
            for (key, text) in &leaves {
                assert!(!text.trim().is_empty(), "{lang}: {key} is empty");
                assert_eq!(
                    placeholders(text),
                    placeholders(&english[key]),
                    "{lang}: placeholders differ for {key}"
                );
            }
        }
    }

    #[test]
    fn every_error_code_of_the_sources_has_a_text() {
        // The codes of the tests of this crate are examples, not errors.
        let examples = ["somethingNew", "tooLarge"];
        for code in error_codes() {
            if examples.contains(&code.as_str()) {
                continue;
            }
            for lang in LANGUAGES {
                assert!(
                    lookup(dictionary(lang), &format!("errors.{code}")).is_some(),
                    "{lang} has no text for errors.{code}"
                );
            }
        }
    }

    #[test]
    fn locale_names_map_to_dictionaries() {
        assert_eq!(language_of("ru-RU"), "ru");
        assert_eq!(language_of("ru_RU.UTF-8"), "ru");
        assert_eq!(language_of("RU"), "ru");
        assert_eq!(language_of("en-US"), "en");
        assert_eq!(language_of("de-DE"), "en");
        assert_eq!(language_of(""), "en");
        assert!(known_language(system_language()));
    }

    #[test]
    fn locale_names_tell_the_country() {
        assert_eq!(region_of("ru-RU").as_deref(), Some("ru"));
        assert_eq!(region_of("ja_JP.UTF-8").as_deref(), Some("jp"));
        assert_eq!(region_of("zh-Hans-CN").as_deref(), Some("cn"));
        assert_eq!(region_of("sr_RS@latin").as_deref(), Some("rs"));
        assert_eq!(region_of("ru").as_deref(), Some("ru"));
        assert_eq!(region_of("C"), None);
        assert_eq!(region_of(""), None);
    }

    #[test]
    fn placeholders_are_filled_and_unknown_codes_do_not_leak_keys() {
        assert_eq!(
            text("xx", "errors.cancelled", &[]),
            text("en", "errors.cancelled", &[])
        );
        assert_ne!(
            text("ru", "errors.cancelled", &[]),
            text("en", "errors.cancelled", &[])
        );
        let message = error_text("en", &Error::new("somethingNew"));
        assert!(message.contains("somethingNew") && !message.contains("errors."));
    }

    #[test]
    fn details_without_a_place_in_the_text_are_appended() {
        let error = Error::new("accessDenied")
            .with("path", "C:\\x")
            .detail("os error 5");
        let message = error_text("en", &error);
        assert!(message.contains("C:\\x"), "{message}");
        assert!(message.contains("os error 5"), "{message}");
    }
}

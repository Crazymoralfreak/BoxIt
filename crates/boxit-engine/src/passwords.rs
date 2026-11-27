//! Passwords: drawing one from the system generator, and the names they are
//! kept under.
//!
//! The secrets themselves are not stored here. The window puts them in the
//! credential store of the system and keeps only their names in the
//! settings. A password is wiped from memory when it is dropped, and this
//! module never writes one to a log.

use std::path::Path;

use boxit_core::{Error, Password, Result};
use sha2::{Digest, Sha256};
use zeroize::Zeroizing;

use crate::fsx;

/// How long a generated password is unless another length is asked for.
pub const GENERATED_LENGTH: u32 = 20;
/// The shortest and the longest password the generator makes.
pub const SHORTEST: u32 = 8;
pub const LONGEST: u32 = 128;
/// How many named passwords the settings remember.
pub const SAVED_KEPT: usize = 32;

const HEX: &[u8; 16] = b"0123456789abcdef";
const LETTERS: &[u8] = b"abcdefghijklmnopqrstuvwxyzABCDEFGHIJKLMNOPQRSTUVWXYZ";
const DIGITS: &[u8] = b"0123456789";
const SYMBOLS: &[u8] = b"!@#$%&*+-=?";

/// Draws a password from the system generator. It has a letter and a digit,
/// and a symbol when `symbols` is set, as far as the length allows.
pub fn generate(length: u32, symbols: bool) -> Result<Password> {
    if !(SHORTEST..=LONGEST).contains(&length) {
        return Err(Error::new("passwordLength")
            .with("min", SHORTEST.to_string())
            .with("max", LONGEST.to_string()));
    }
    let mut alphabet = Vec::with_capacity(LETTERS.len() + DIGITS.len() + SYMBOLS.len());
    alphabet.extend_from_slice(LETTERS);
    alphabet.extend_from_slice(DIGITS);
    if symbols {
        alphabet.extend_from_slice(SYMBOLS);
    }
    let mut text = Zeroizing::new(vec![0u8; length as usize]);
    for byte in &mut *text {
        *byte = pick(&alphabet)?;
    }
    // The classes are placed at random positions, so a password does not
    // start with the same kind of character every time.
    let mut classes = vec![LETTERS, DIGITS];
    if symbols {
        classes.push(SYMBOLS);
    }
    let mut places = (0..text.len()).collect::<Vec<_>>();
    shuffle(&mut places)?;
    for (class, place) in classes.iter().zip(places) {
        text[place] = pick(class)?;
    }
    let password =
        String::from_utf8(text.to_vec()).map_err(|error| Error::new("internal").detail(error))?;
    Ok(Password::new(password))
}

/// One character of `alphabet`, each of them equally likely.
fn pick(alphabet: &[u8]) -> Result<u8> {
    let count = alphabet.len();
    let limit = 256 - (256 % count);
    loop {
        let byte = random()? as usize;
        if byte < limit {
            return Ok(alphabet[byte % count]);
        }
    }
}

/// The next byte of the system generator.
fn random() -> Result<u8> {
    let mut byte = [0u8];
    getrandom::fill(&mut byte).map_err(|error| Error::new("internal").detail(error))?;
    Ok(byte[0])
}

/// Fisher–Yates, from the system generator.
fn shuffle(places: &mut [usize]) -> Result<()> {
    for index in (1..places.len()).rev() {
        let swap = below(index + 1)?;
        places.swap(index, swap);
    }
    Ok(())
}

/// A number below `count`, each of them equally likely. `count` is at most
/// the longest password.
fn below(count: usize) -> Result<usize> {
    let limit = 256 - (256 % count);
    loop {
        let byte = random()? as usize;
        if byte < limit {
            return Ok(byte % count);
        }
    }
}

/// The name a saved password is kept under in the credential store.
pub fn saved_account(name: &str) -> Result<String> {
    Ok(format!("saved:{}", checked_name(name)?))
}

/// The name the password of an archive is kept under. The path is reduced
/// to a hash: a credential name cannot hold every path, and the store does
/// not need the path to find the password again.
pub fn archive_account(path: &Path) -> Result<String> {
    let shown = fsx::display(&fsx::long(path)?);
    let digest = Sha256::digest(shown.as_bytes());
    let mut hex = String::with_capacity(64);
    for byte in digest {
        hex.push(HEX[(byte >> 4) as usize] as char);
        hex.push(HEX[(byte & 0x0f) as usize] as char);
    }
    Ok(format!("archive:{hex}"))
}

/// A name the settings may keep: trimmed, short, and without a character
/// that would break the credential name.
fn checked_name(name: &str) -> Result<&str> {
    let name = name.trim();
    let fitting = !name.is_empty()
        && name.chars().count() <= 64
        && name
            .chars()
            .all(|symbol| !symbol.is_control() && symbol != '/' && symbol != '\\' && symbol != ':');
    if fitting {
        Ok(name)
    } else {
        Err(Error::new("badPasswordName").with("name", name))
    }
}

/// Adds `name` to the names the settings keep, once it is fit to be one.
pub fn keep_name(names: &mut Vec<String>, name: &str) -> Result<()> {
    let name = checked_name(name)?.to_owned();
    names.retain(|known| known != &name);
    names.insert(0, name);
    names.truncate(SAVED_KEPT);
    Ok(())
}

/// Drops a saved name. The caller removes the secret from the store.
pub fn drop_name(names: &mut Vec<String>, name: &str) {
    let trimmed = name.trim();
    names.retain(|known| known != trimmed);
}

/// The names as they are stored: fit ones, each once, the newest first.
pub fn normalize_names(names: &mut Vec<String>) {
    let mut seen: Vec<String> = Vec::new();
    for name in names.drain(..) {
        let Ok(name) = checked_name(&name) else {
            continue;
        };
        if !seen.iter().any(|known| known == name) {
            seen.push(name.to_owned());
        }
    }
    seen.truncate(SAVED_KEPT);
    *names = seen;
}

#[cfg(test)]
mod tests {
    use super::*;

    fn alphabet(symbols: bool) -> Vec<char> {
        let mut text = format!(
            "{}{}",
            std::str::from_utf8(LETTERS).unwrap(),
            std::str::from_utf8(DIGITS).unwrap()
        );
        if symbols {
            text.push_str(std::str::from_utf8(SYMBOLS).unwrap());
        }
        text.chars().collect()
    }

    #[test]
    fn a_generated_password_has_the_length_and_the_kinds_it_was_asked_for() {
        for (length, symbols) in [(SHORTEST, false), (GENERATED_LENGTH, true), (LONGEST, true)] {
            let password = generate(length, symbols).unwrap();
            let text = password.expose();
            assert_eq!(text.chars().count(), length as usize);
            let allowed = alphabet(symbols);
            assert!(
                text.chars().all(|symbol| allowed.contains(&symbol)),
                "{text}"
            );
            assert!(text.chars().any(|symbol| LETTERS.contains(&(symbol as u8))));
            assert!(text.chars().any(|symbol| DIGITS.contains(&(symbol as u8))));
            assert_eq!(
                text.chars().any(|symbol| SYMBOLS.contains(&(symbol as u8))),
                symbols
            );
            assert_eq!(format!("{password:?}"), "Password(***)");
        }
        let short = generate(SHORTEST - 1, false).unwrap_err();
        assert_eq!(short.code, "passwordLength");
        let long = generate(LONGEST + 1, true).unwrap_err();
        assert_eq!(long.code, "passwordLength");
    }

    #[test]
    fn two_generated_passwords_are_not_the_same() {
        let one = generate(GENERATED_LENGTH, true).unwrap();
        let other = generate(GENERATED_LENGTH, true).unwrap();
        assert_ne!(one.expose(), other.expose());
    }

    #[test]
    fn a_saved_name_is_checked_and_kept_once() {
        assert_eq!(saved_account(" work ").unwrap(), "saved:work");
        assert_eq!(saved_account("").unwrap_err().code, "badPasswordName");
        assert_eq!(saved_account("a/b").unwrap_err().code, "badPasswordName");
        assert_eq!(saved_account("a:b").unwrap_err().code, "badPasswordName");

        let mut names = Vec::new();
        keep_name(&mut names, "work").unwrap();
        keep_name(&mut names, "home").unwrap();
        keep_name(&mut names, " work ").unwrap();
        assert_eq!(names, ["work", "home"]);
        drop_name(&mut names, "home");
        assert_eq!(names, ["work"]);

        let mut names = vec![
            String::from("ok"),
            String::from("a/b"),
            String::from("ok"),
            String::from("  "),
        ];
        normalize_names(&mut names);
        assert_eq!(names, ["ok"]);
    }

    #[test]
    fn an_archive_keeps_its_password_under_the_hash_of_its_path() {
        let one = archive_account(Path::new("photos.7z")).unwrap();
        let same = archive_account(Path::new("photos.7z")).unwrap();
        let other = archive_account(Path::new("notes.7z")).unwrap();
        assert_eq!(one, same);
        assert_ne!(one, other);
        assert!(one.starts_with("archive:"));
        assert!(!one.contains("photos"));
    }
}

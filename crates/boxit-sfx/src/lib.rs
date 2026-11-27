//! The program a self-extracting archive starts with.
//!
//! BoxIt writes this program and then the archive after its last byte.
//! Running the program extracts that archive. Switches, for a script:
//!
//! ```text
//! setup.exe -oC:\out -y -psecret
//! ```
//!
//! `-o` is the folder, `-y` asks nothing and replaces files, `-p` is the
//! password. Without them the Windows program asks in dialogs.

#![forbid(unsafe_code)]

use std::ffi::{OsStr, OsString};
use std::path::{Path, PathBuf};

use boxit_codecs::names::NameEncoding;
use boxit_core::{
    ConflictPolicy, Error, ExtractOptions, OpContext, Password, Result, Selection, SymlinkPolicy,
};
use boxit_engine::archive::{open_options, Archive};
use boxit_engine::extract::Outcome;

/// What the command line of the program asked for.
#[derive(Debug)]
pub struct Invocation {
    /// The folder to extract into. None asks, or uses the current folder
    /// when [`Self::yes`] is set.
    pub output: Option<PathBuf>,
    /// Ask nothing: replace existing files.
    pub yes: bool,
    pub password: Option<Password>,
}

/// Reads the switches. `args` is what follows the program name.
pub fn invocation(args: impl IntoIterator<Item = OsString>) -> Result<Invocation> {
    let mut output = None;
    let mut yes = false;
    let mut password = None;
    let mut args = args.into_iter();
    while let Some(arg) = args.next() {
        let text = arg.to_string_lossy();
        let Some(body) = text.strip_prefix('-').or_else(|| text.strip_prefix('/')) else {
            return Err(Error::new("sfxUsage").with("name", text.as_ref()));
        };
        let body = body.strip_prefix('-').unwrap_or(body);
        match body {
            "y" | "Y" => yes = true,
            "o" | "output" => {
                let path = args.next().ok_or_else(|| Error::new("sfxUsage"))?;
                output = Some(PathBuf::from(path));
            }
            "p" | "password" => {
                let value = args.next().ok_or_else(|| Error::new("sfxUsage"))?;
                password = Some(secret(&value)?);
            }
            "h" | "help" | "?" => return Err(Error::new("sfxUsage")),
            _ => {
                let (kind, rest) = body.split_at(body.chars().next().map_or(0, char::len_utf8));
                if rest.is_empty() {
                    return Err(Error::new("sfxUsage").with("name", text.as_ref()));
                }
                match kind {
                    "o" | "O" if !rest.starts_with("ut") => output = Some(PathBuf::from(rest)),
                    "p" | "P" if !rest.starts_with("assword") => {
                        password = Some(secret(OsStr::new(rest))?);
                    }
                    _ => return Err(Error::new("sfxUsage").with("name", text.as_ref())),
                }
            }
        }
    }
    Ok(Invocation {
        output,
        yes,
        password,
    })
}

fn secret(value: &OsStr) -> Result<Password> {
    let text = value.to_string_lossy();
    if text.is_empty() {
        Err(Error::new("passwordRequired"))
    } else {
        Ok(Password::new(text.into_owned()))
    }
}

/// Extracts the archive glued to `program` into `destination`.
pub fn unpack(
    program: &Path,
    destination: &Path,
    password: Option<Password>,
    yes: bool,
    ctx: &OpContext,
) -> Result<Outcome> {
    let mut archive = Archive::open(program, open_options(password, NameEncoding::Auto))?;
    let options = ExtractOptions {
        conflicts: if yes {
            ConflictPolicy::Overwrite
        } else {
            ConflictPolicy::Ask
        },
        symlinks: SymlinkPolicy::Skip,
        ..ExtractOptions::default()
    };
    archive.extract(destination, Selection::All, options, ctx)
}

#[cfg(test)]
mod tests {
    use std::fs;

    use boxit_core::{ArchiveFormat, CreateOptions, Level, OpContext};
    use boxit_engine::create::{self, CreateRequest, Layout};
    use tempfile::TempDir;

    use super::*;

    #[test]
    fn switches_are_read_glued_or_apart() {
        let read = |line: &[&str]| invocation(line.iter().copied().map(OsString::from)).unwrap();
        let apart = read(&["-o", "C:\\out", "-y", "-p", "secret"]);
        assert!(apart.yes);
        assert_eq!(apart.output.unwrap(), PathBuf::from("C:\\out"));
        assert_eq!(apart.password.unwrap().expose(), "secret");

        let glued = read(&["-oC:\\out", "/psecret", "/y"]);
        assert!(glued.yes);
        assert_eq!(glued.output.unwrap(), PathBuf::from("C:\\out"));
        assert_eq!(glued.password.unwrap().expose(), "secret");

        assert_eq!(
            invocation([OsString::from("-o")]).unwrap_err().code,
            "sfxUsage"
        );
        assert_eq!(
            invocation([OsString::from("file")]).unwrap_err().code,
            "sfxUsage"
        );
    }

    /// A program of `end` bytes, so an archive glued after it starts there.
    fn program(end: usize) -> Vec<u8> {
        let mut bytes = vec![0u8; end];
        bytes[0] = b'M';
        bytes[1] = b'Z';
        bytes[0x3C..0x40].copy_from_slice(&64u32.to_le_bytes());
        bytes[64..68].copy_from_slice(b"PE\0\0");
        bytes[70..72].copy_from_slice(&1u16.to_le_bytes());
        bytes[104..108].copy_from_slice(&((end - 128) as u32).to_le_bytes());
        bytes[108..112].copy_from_slice(&128u32.to_le_bytes());
        bytes
    }

    #[test]
    fn the_archive_glued_after_the_program_is_extracted() {
        let root = TempDir::new().unwrap();
        let docs = root.path().join("docs");
        fs::create_dir(&docs).unwrap();
        fs::write(docs.join("note.txt"), "hello").unwrap();
        let module = root.path().join("module.exe");
        fs::write(&module, program(640)).unwrap();
        let setup = root.path().join("setup.exe");
        let request = CreateRequest {
            sources: vec![docs],
            destination: setup.clone(),
            options: CreateOptions {
                level: Level::Store,
                ..CreateOptions::new(ArchiveFormat::Zip)
            },
            overwrite: false,
            layout: Layout::SelfExtracting(module),
        };
        create::create(&request, &OpContext::silent()).unwrap();
        let out = root.path().join("out");
        unpack(&setup, &out, None, true, &OpContext::silent()).unwrap();
        assert_eq!(
            fs::read(out.join("docs").join("note.txt")).unwrap(),
            b"hello"
        );
    }
}

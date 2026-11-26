//! Archives cut into volumes: `backup.7z.001`, `backup.7z.002` and so on.
//!
//! The volumes are the bytes of one archive, cut where a volume was full:
//! put end to end they are the archive again. That is how 7-Zip splits
//! archives of every format, and how BoxIt does. The names say everything
//! there is to know: the name of the archive, a dot and the number of the
//! volume, three digits or more, from one.

use std::path::{Path, PathBuf};

use boxit_core::{Error, Result};

/// The digits a number of a volume has at the least.
const DIGITS: usize = 3;

/// The most volumes one archive is cut into: beyond it a wrong size of a
/// volume fills a folder with files.
pub const MOST: u64 = 99_999;

/// The smallest volume an archive is cut into.
pub const SMALLEST: u64 = 64 * 1024;

/// The name of a volume apart: the name of the archive, and the number.
/// `None` for a name that is not of a volume.
pub fn parse(name: &str) -> Option<(&str, u64)> {
    let (base, digits) = name.rsplit_once('.')?;
    if base.is_empty() || digits.len() < DIGITS || !digits.bytes().all(|byte| byte.is_ascii_digit())
    {
        return None;
    }
    let number: u64 = digits.parse().ok()?;
    (1..=MOST).contains(&number).then_some((base, number))
}

/// The name of the volume `number` of the archive `base`.
pub fn name(base: &str, number: u64) -> String {
    format!("{base}.{number:0DIGITS$}")
}

/// The file of the volume `number` of the archive that would stand at
/// `archive` if it were whole.
pub fn path(archive: &Path, number: u64) -> PathBuf {
    let mut path = archive.as_os_str().to_owned();
    path.push(format!(".{number:0DIGITS$}"));
    PathBuf::from(path)
}

/// The volumes a file belongs to, the first one first, with the name of the
/// archive they make. `None` when the file is not a volume by its name.
///
/// Whichever volume is opened, the archive starts at the first one. The
/// volumes are followed for as long as their numbers have no gap; a gap
/// before the volume that was opened is an error, and so is the first
/// volume missing.
pub fn of(path: &Path) -> Result<Option<(String, Vec<PathBuf>)>> {
    let Some(file_name) = path.file_name().and_then(|name| name.to_str()) else {
        return Ok(None);
    };
    let Some((base, opened)) = parse(file_name) else {
        return Ok(None);
    };
    let folder = path.parent().unwrap_or(Path::new(""));
    let mut volumes = Vec::new();
    for number in 1..=MOST {
        // The volume that was opened is taken as it was named: `x.0001` is
        // the first volume of `x` as much as `x.001` would be.
        let volume = if number == opened {
            path.to_path_buf()
        } else {
            folder.join(name(base, number))
        };
        if !volume.is_file() {
            if number <= opened {
                return Err(Error::new("volumeMissing").with("name", name(base, number)));
            }
            break;
        }
        volumes.push(volume);
    }
    Ok(Some((base.to_owned(), volumes)))
}

#[cfg(test)]
mod tests {
    use std::fs;

    use super::*;

    #[test]
    fn a_volume_is_told_by_its_name() {
        assert_eq!(parse("backup.7z.001"), Some(("backup.7z", 1)));
        assert_eq!(parse("backup.tar.gz.042"), Some(("backup.tar.gz", 42)));
        assert_eq!(parse("backup.zip.1000"), Some(("backup.zip", 1000)));
        assert_eq!(parse("backup.0007"), Some(("backup", 7)));
        // Not volumes: too few digits, no number, no name, a number no
        // volume has.
        for name in [
            "backup.7z",
            "backup.01",
            "backup.7z.00a",
            ".001",
            "001",
            "backup.000",
            "backup.100000",
            "backup.",
        ] {
            assert_eq!(parse(name), None, "{name}");
        }
        assert_eq!(name("backup.7z", 1), "backup.7z.001");
        assert_eq!(name("backup.7z", 999), "backup.7z.999");
        assert_eq!(name("backup.7z", 1000), "backup.7z.1000");
        assert_eq!(parse(&name("a", 12_345)), Some(("a", 12_345)));
    }

    #[test]
    fn the_volumes_of_a_file_are_found_from_any_of_them() {
        let folder = tempfile::tempdir().unwrap();
        let at = |name: &str| folder.path().join(name);
        for number in 1..=3 {
            fs::write(at(&name("backup.7z", number)), b"x").unwrap();
        }
        // After a gap the numbers belong to something else.
        fs::write(at("backup.7z.005"), b"x").unwrap();
        fs::write(at("plain.7z"), b"x").unwrap();
        let wanted = vec![
            at("backup.7z.001"),
            at("backup.7z.002"),
            at("backup.7z.003"),
        ];
        for opened in ["backup.7z.001", "backup.7z.002", "backup.7z.003"] {
            let (base, volumes) = of(&at(opened)).unwrap().unwrap();
            assert_eq!(base, "backup.7z");
            assert_eq!(volumes, wanted, "{opened}");
        }
        assert_eq!(of(&at("plain.7z")).unwrap(), None);

        let error = of(&at("backup.7z.005")).unwrap_err();
        assert_eq!(error.code, "volumeMissing");
        assert_eq!(error.params["name"], "backup.7z.004");
        fs::remove_file(at("backup.7z.001")).unwrap();
        let error = of(&at("backup.7z.002")).unwrap_err();
        assert_eq!(error.params["name"], "backup.7z.001");
    }
}

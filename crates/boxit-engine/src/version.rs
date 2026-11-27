//! Versions of BoxIt, compared: what the check for updates decides by.

/// The numbers of a version such as `v1.2.3` or `1.2.3-beta.1`: the three
/// parts before any suffix. `None` for what is not a version.
fn numbers(version: &str) -> Option<[u64; 3]> {
    let version = version.trim();
    let version = version.strip_prefix(['v', 'V']).unwrap_or(version);
    // What follows a dash or a plus names a build, not a newer version.
    let core = version.split(['-', '+']).next()?;
    let mut parts = core.split('.');
    let mut numbers = [0; 3];
    for number in &mut numbers {
        *number = parts.next()?.parse().ok()?;
    }
    parts.next().is_none().then_some(numbers)
}

/// Whether `found` is a later version than `current`. Anything that is not
/// a version is not later than anything.
pub fn is_newer(current: &str, found: &str) -> bool {
    match (numbers(current), numbers(found)) {
        (Some(current), Some(found)) => found > current,
        _ => false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn versions_are_compared_by_their_numbers() {
        assert!(is_newer("0.0.1", "0.0.2"));
        assert!(is_newer("0.0.9", "v0.0.10"));
        assert!(is_newer("0.9.9", "1.0.0"));
        assert!(is_newer("1.2.3", " V1.3.0-beta.1 "));
        assert!(!is_newer("0.0.1", "0.0.1"));
        assert!(!is_newer("0.0.2", "0.0.1"));
        assert!(!is_newer("1.0.0", "0.9.9"));
        assert!(!is_newer("0.0.1", "0.0.1+build.5"));
    }

    #[test]
    fn what_is_not_a_version_is_not_newer() {
        for found in ["", "latest", "1.2", "1.2.3.4", "1.x.3", "v", "-1.2.3"] {
            assert!(!is_newer("0.0.1", found), "{found}");
        }
        assert!(!is_newer("nonsense", "1.0.0"));
    }
}

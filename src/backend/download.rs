//! Interface of the backends that download releases from a hosting service
//! (see [`RepositoryType`](crate::config::RepositoryType)).

use std::{
    cmp::Ordering,
    fmt::Display,
    fs::{File, OpenOptions},
    io,
    path::{Path, PathBuf},
};

use regex::Captures;

use crate::config::{Config, VERSION_GROUPS};

/// Version of a release, made of the parts matched by the
/// [`VERSION_GROUPS`] of a configuration, e.g. `1.10.0`.
///
/// Versions are ordered part by part, where numeric parts compare by value
/// (`1.10.0` > `1.9.0`), numeric parts are older than non-numeric ones, and
/// non-numeric parts compare as strings. Two versions are only equal if all
/// their parts are identical, so `1.01.0` and `1.1.0` are ordered by their
/// strings as a last resort. If one version is a prefix of the other, the
/// shorter one is older.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Version(Vec<String>);

impl Version {
    /// The version matched by a configuration's [`Asset::version`], made of
    /// the [`VERSION_GROUPS`] in order. A group that did not participate in
    /// the match (e.g. an optional `patch`) counts as `0`.
    ///
    /// [`Asset::version`]: crate::config::Asset::version
    pub fn from_captures(captures: &Captures<'_>) -> Self {
        Self(
            VERSION_GROUPS
                .iter()
                .map(|group| captures.name(group).map_or("0", |m| m.as_str()).to_owned())
                .collect(),
        )
    }

    /// Compares two parts of a version (see [`Version`]).
    fn cmp_part(a: &str, b: &str) -> Ordering {
        // Only ASCII digits, so neither signs (as accepted by `parse`) nor
        // other Unicode digits (as matched by `\d`) count as numeric
        let is_numeric = |s: &str| !s.is_empty() && s.bytes().all(|b| b.is_ascii_digit());

        match (is_numeric(a), is_numeric(b)) {
            (true, true) => {
                // Compare by value without parsing, which could overflow:
                // without leading zeros, the longer number is larger
                let (x, y) = (a.trim_start_matches('0'), b.trim_start_matches('0'));
                x.len()
                    .cmp(&y.len())
                    .then_with(|| x.cmp(y))
                    .then_with(|| a.cmp(b))
            }
            (true, false) => Ordering::Less,
            (false, true) => Ordering::Greater,
            (false, false) => a.cmp(b),
        }
    }
}

impl Display for Version {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let mut iter = self.0.iter().peekable();
        while let Some(version) = iter.next() {
            write!(f, "{}", version)?;
            if iter.peek().is_some() {
                write!(f, ".")?;
            }
        }

        Ok(())
    }
}

impl Ord for Version {
    fn cmp(&self, other: &Self) -> Ordering {
        for (a, b) in self.0.iter().zip(&other.0) {
            match Self::cmp_part(a, b) {
                Ordering::Equal => continue,
                ord => return ord,
            }
        }

        // All common parts are equal, so the shorter version is older
        self.0.len().cmp(&other.0.len())
    }
}

impl PartialOrd for Version {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}

/// Result of [`Backend::download`].
#[derive(Debug)]
pub struct Download {
    /// The downloaded (and, if configured, extracted) file to install.
    pub target: PathBuf,

    /// Version of the release the file belongs to.
    pub version: Version,
}

/// A hosting service that releases are downloaded from.
pub trait Backend {
    /// Error while finding or downloading a release.
    type Error: Display;

    /// Downloads the file to install from the newest release of
    /// `config.repository` into `tmp_dir`, which must exist.
    ///
    /// `tmp_dir` must only be writable by the current user (e.g. created by
    /// `mkdtemp`), since files in it are created with predictable names.
    /// Existing files with these names are replaced, but symbolic links are
    /// never followed.
    fn download<P: AsRef<Path>>(config: &Config, tmp_dir: P) -> Result<Download, Self::Error>;
}

/// Creates the file `path` for writing, replacing an existing file.
///
/// Unlike [`File::create`], this never writes through a symbolic link (or
/// to any other existing file): an existing entry is removed (a symbolic
/// link itself, not its target), and the file is then created exclusively.
/// If another process creates the file in between, this fails instead of
/// writing to it.
pub(super) fn create_file(path: &Path) -> io::Result<File> {
    match std::fs::remove_file(path) {
        Err(error) if error.kind() != io::ErrorKind::NotFound => return Err(error),
        _ => {}
    }
    OpenOptions::new().write(true).create_new(true).open(path)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn version(s: &str) -> Version {
        Version(s.split('.').map(str::to_owned).collect())
    }

    #[test]
    fn orders_numeric_parts_by_value() {
        assert!(version("1.10.0") > version("1.9.0"));
        assert!(version("2.0.0") > version("1.99.99"));
        assert!(version("1.2.10") > version("1.2.9"));
        assert!(version("0.0.1") < version("0.1.0"));
    }

    #[test]
    fn orders_large_numbers_without_overflow() {
        // Larger than u64::MAX, which `parse::<usize>` would reject
        let huge = "184467440737095516160";
        assert!(version(&format!("1.{huge}.0")) > version("1.9.0"));
        assert!(version(&format!("1.{huge}1.0")) > version(&format!("1.{huge}.0")));
    }

    #[test]
    fn orders_leading_zeros_by_value() {
        assert!(version("1.010.0") > version("1.9.0"));
        // Equal values, but different strings must not be equal
        assert_ne!(version("1.01.0").cmp(&version("1.1.0")), Ordering::Equal);
        assert_eq!(
            version("1.01.0").cmp(&version("1.1.0")),
            version("1.1.0").cmp(&version("1.01.0")).reverse()
        );
    }

    #[test]
    fn orders_non_numeric_parts_after_numeric() {
        assert!(version("1.2.x") > version("1.2.3"));
        assert!(version("1.2.b") > version("1.2.a"));
        // Not numeric, despite `"+1".parse::<usize>()` succeeding
        assert!(version("1.+1.0") > version("1.2.0"));
        // Unicode digits are not numeric either
        assert!(version("1.٣.0") > version("1.9.0"));
    }

    #[test]
    fn equal_versions_are_equal() {
        // Ord must be consistent with Eq, or sorting and `max` misbehave
        let a = version("1.2.3");
        assert_eq!(a.cmp(&a.clone()), Ordering::Equal);
        assert_eq!(a.partial_cmp(&a.clone()), Some(Ordering::Equal));
    }

    #[test]
    fn shorter_prefix_is_older() {
        assert!(version("1.2") < version("1.2.0"));
        assert!(version("1.2.0") > version("1.2"));
        assert!(version("1.3") > version("1.2.0"));
    }

    #[test]
    fn displays_parts_with_dots() {
        assert_eq!(version("1.10.0").to_string(), "1.10.0");
        assert_eq!(Version(vec!["7".to_owned()]).to_string(), "7");
        assert_eq!(Version(Vec::new()).to_string(), "");
    }

    #[test]
    fn missing_group_counts_as_zero() {
        let regex = regex::Regex::new(r"(?<major>\d+)\.(?<minor>\d+)(?:\.(?<patch>\d+))?").unwrap();
        let captures = regex.captures("v1.2").unwrap();
        assert_eq!(Version::from_captures(&captures), version("1.2.0"));
        let captures = regex.captures("v1.2.3").unwrap();
        assert_eq!(Version::from_captures(&captures), version("1.2.3"));
    }

    #[test]
    fn create_file_replaces_without_following_symlinks() {
        let dir = std::env::temp_dir().join(format!("grm-create-test-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();

        // A symlink planted in place of the file must not be written through
        let victim = dir.join("victim");
        std::fs::write(&victim, "keep").unwrap();
        let path = dir.join("file");
        std::os::unix::fs::symlink(&victim, &path).unwrap();

        io::Write::write_all(&mut create_file(&path).unwrap(), b"new").unwrap();
        assert_eq!(std::fs::read_to_string(&victim).unwrap(), "keep");
        assert!(!path.is_symlink());
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "new");

        // An existing regular file is replaced
        io::Write::write_all(&mut create_file(&path).unwrap(), b"newer").unwrap();
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "newer");

        std::fs::remove_dir_all(&dir).unwrap();
    }
}

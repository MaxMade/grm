//! Configuration of `grm`.
//!
//! Every release managed by `grm` is described by one TOML file in the
//! configuration directory (see [`default_config_dir`]), for example:
//!
//! ```toml
//! [repository]
//! url = "https://github.com/go-acme/lego/releases"
//!
//! [installation]
//! name = "lego"
//!
//! [version]
//! regex = '(?<major>\d+)\.(?<minor>\d+)\.(?<patch>\d+)'
//! ```
//!
//! Optional values that are missing from a file are filled in while loading,
//! using system-wide locations when running as root and per-user (XDG)
//! locations otherwise. All values are validated while loading.

use std::{
    ffi::OsString,
    fmt::{self, Display},
    io,
    path::{Path, PathBuf},
};

use regex::Regex;
use serde::Deserialize;

use crate::lock::LockFile;

/// Configuration of a single release, loaded from one TOML file.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Config {
    /// Where releases are fetched from (`[repository]`).
    pub repository: Repository,

    /// Where and how a release is installed (`[installation]`).
    pub installation: Installation,

    /// How version numbers are extracted (`[version]`).
    pub version: Version,
}

impl LockFile for Config {
    /// `<runtime_dir>/.<name>.lock`, so that configurations installing
    /// different binaries can be processed concurrently, even though they
    /// share the default runtime directory.
    fn path(&self) -> PathBuf {
        let name = self
            .installation
            .path
            .file_name()
            .expect("installation name is validated while loading");

        let mut file = OsString::from(".");
        file.push(name);
        file.push(".lock");
        self.installation.runtime_dir.join(file)
    }
}

/// The `[repository]` section.
#[derive(Debug, Deserialize)]
#[serde(try_from = "RawRepository")]
pub struct Repository {
    /// URL of the repository's release page, e.g.
    /// `https://github.com/<owner>/<repo>/releases`.
    pub url: String,

    /// Hosting service of the repository (key `type`).
    ///
    /// Optional in the file; if missing, it is derived from the host of
    /// [`Repository::url`].
    pub ty: RepositoryType,
}

/// `[repository]` as written in the file, before defaults are applied.
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RawRepository {
    url: String,
    #[serde(rename = "type")]
    ty: Option<RepositoryType>,
}

impl TryFrom<RawRepository> for Repository {
    type Error = Error;

    fn try_from(raw: RawRepository) -> Result<Self, Self::Error> {
        let ty = match raw.ty {
            Some(ty) => ty,
            None => {
                let ty = RepositoryType::from_url(&raw.url)
                    .ok_or_else(|| Error::UnknownRepositoryType(raw.url.clone()))?;
                log::trace!("Assuming repository type \"{ty}\" for {}", raw.url);
                ty
            }
        };

        Ok(Self { url: raw.url, ty })
    }
}

/// Supported repository hosting services.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum RepositoryType {
    /// GitHub (`github.com`).
    Github,
}

impl RepositoryType {
    /// Guesses the hosting service from the host part of `url`.
    fn from_url(url: &str) -> Option<Self> {
        let host = url_host(url)?.to_ascii_lowercase();
        if host == "github.com" || host.ends_with(".github.com") {
            Some(Self::Github)
        } else {
            None
        }
    }
}

impl Display for RepositoryType {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Github => write!(f, "github"),
        }
    }
}

/// Extracts the host from a URL such as `https://user@host:443/path?query`.
///
/// This is deliberately minimal (no IPv6 literals) and only intended for
/// guessing the repository type.
fn url_host(url: &str) -> Option<&str> {
    let rest = url.split_once("://").map_or(url, |(_, rest)| rest);
    let authority = rest.split(['/', '?', '#']).next()?;
    let host_port = authority.rsplit_once('@').map_or(authority, |(_, h)| h);
    let host = host_port.split(':').next()?;
    (!host.is_empty()).then_some(host)
}

/// The `[installation]` section.
#[derive(Debug, Deserialize)]
#[serde(try_from = "RawInstallation")]
pub struct Installation {
    /// Full path of the installed binary, i.e. `dir/name` from the file:
    ///
    /// - `name` (required): file name of the installed binary; must not
    ///   contain a `/`.
    /// - `dir` (optional): directory the binary is installed to. Default:
    ///   `/usr/local/bin` (root) or `~/.local/bin` (user).
    pub path: PathBuf,

    /// Permission bits of the installed binary.
    ///
    /// Default: `0o755`. Must not exceed `0o7777`.
    pub mode: u32,

    /// Directory for the lock file `.<name>.lock` that prevents concurrent
    /// runs for the same binary (similar to apt's lock).
    ///
    /// Default: `/run/grm` (root) or `$XDG_RUNTIME_DIR/grm` (user).
    pub runtime_dir: PathBuf,

    /// Directory where the record of currently installed programs is stored.
    ///
    /// Default: `/var/lib/grm` (root) or `$XDG_DATA_HOME/grm`, falling back
    /// to `~/.local/share/grm` (user).
    pub data_dir: PathBuf,
}

/// `[installation]` as written in the file, before defaults are applied.
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RawInstallation {
    name: String,
    dir: Option<PathBuf>,
    mode: Option<u32>,
    runtime_dir: Option<PathBuf>,
    data_dir: Option<PathBuf>,
}

impl TryFrom<RawInstallation> for Installation {
    type Error = Error;

    fn try_from(raw: RawInstallation) -> Result<Self, Self::Error> {
        // A plain file name has itself as its final component; this rejects
        // "", ".", ".." and anything containing a `/`.
        if Path::new(&raw.name).file_name() != Some(raw.name.as_ref()) {
            return Err(Error::InvalidName(raw.name));
        }

        let mode = raw.mode.unwrap_or(DEFAULT_MODE);
        if mode > 0o7777 {
            return Err(Error::InvalidMode(mode));
        }

        // Defaults are only computed when needed, so a missing environment
        // variable is not an error if the value is set explicitly.
        Ok(Self {
            path: raw.dir.map_or_else(default_install_dir, Ok)?.join(raw.name),
            mode,
            runtime_dir: raw.runtime_dir.map_or_else(default_runtime_dir, Ok)?,
            data_dir: raw.data_dir.map_or_else(default_data_dir, Ok)?,
        })
    }
}

impl Installation {
    /// Creates [`Installation::runtime_dir`] and [`Installation::data_dir`],
    /// including missing parents. Existing directories are left untouched.
    ///
    /// This is separate from loading so that loading has no side effects; it
    /// must be called before the lock file or the record of installed
    /// programs is accessed.
    ///
    /// # Errors
    ///
    /// Fails if a directory cannot be created, e.g. due to missing
    /// permissions or because a file exists at that path.
    pub fn create_dirs(&self) -> Result<(), Error> {
        for dir in [&self.runtime_dir, &self.data_dir] {
            std::fs::create_dir_all(dir).map_err(|source| Error::CreateDir {
                path: dir.clone(),
                source,
            })?;
        }
        Ok(())
    }
}

/// The `[version]` section.
#[derive(Debug, Deserialize)]
#[serde(try_from = "RawVersion")]
pub struct Version {
    /// Regular expression matching a release's version. It contains the named
    /// capture groups listed in [`VERSION_GROUPS`].
    pub regex: Regex,
}

/// Named capture groups that [`Version::regex`] must contain.
pub const VERSION_GROUPS: [&str; 3] = ["major", "minor", "patch"];

/// `[version]` as written in the file, before validation.
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RawVersion {
    regex: String,
}

impl TryFrom<RawVersion> for Version {
    type Error = Error;

    fn try_from(raw: RawVersion) -> Result<Self, Self::Error> {
        let regex = Regex::new(&raw.regex).map_err(Error::InvalidRegex)?;

        for group in VERSION_GROUPS {
            if !regex.capture_names().flatten().any(|name| name == group) {
                return Err(Error::MissingCaptureGroup(group));
            }
        }

        Ok(Self { regex })
    }
}

/// Errors while loading the configuration or preparing its directories.
#[derive(Debug)]
pub enum Error {
    /// A directory from [`Installation`] could not be created.
    CreateDir { path: PathBuf, source: io::Error },

    /// The configuration directory could not be listed.
    ReadDir { path: PathBuf, source: io::Error },

    /// A configuration file could not be read.
    ReadFile { path: PathBuf, source: io::Error },

    /// A configuration file is not valid TOML or contains invalid values.
    Parse {
        path: PathBuf,
        source: toml::de::Error,
    },

    /// A default value depends on an environment variable that is unset or
    /// not an absolute path.
    MissingEnv(&'static str),

    /// The repository type was not set and could not be derived from the URL.
    UnknownRepositoryType(String),

    /// `name` is not a plain file name.
    InvalidName(String),

    /// `mode` is not a valid permission mode.
    InvalidMode(u32),

    /// `regex` is not a valid regular expression.
    InvalidRegex(regex::Error),

    /// `regex` lacks one of the [`VERSION_GROUPS`].
    MissingCaptureGroup(&'static str),
}

impl Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::CreateDir { path, source } => {
                write!(f, "unable to create directory {}: {source}", path.display())
            }
            Self::ReadDir { path, source } => {
                write!(f, "unable to read directory {}: {source}", path.display())
            }
            Self::ReadFile { path, source } => {
                write!(f, "unable to read {}: {source}", path.display())
            }
            Self::Parse { path, source } => {
                write!(f, "unable to parse {}: {source}", path.display())
            }
            Self::MissingEnv(var) => {
                write!(f, "${var} is not set or not an absolute path")
            }
            Self::UnknownRepositoryType(url) => write!(
                f,
                "unable to derive repository type from \"{url}\"; set `type` explicitly"
            ),
            Self::InvalidName(name) => {
                write!(f, "invalid name \"{name}\": must be a plain file name")
            }
            Self::InvalidMode(mode) => {
                write!(f, "invalid mode {mode:#o}: must not exceed 0o7777")
            }
            Self::InvalidRegex(error) => write!(f, "invalid regex: {error}"),
            Self::MissingCaptureGroup(group) => {
                write!(f, "regex lacks the named capture group `{group}`")
            }
        }
    }
}

impl std::error::Error for Error {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::CreateDir { source, .. }
            | Self::ReadDir { source, .. }
            | Self::ReadFile { source, .. } => Some(source),
            Self::Parse { source, .. } => Some(source),
            Self::InvalidRegex(source) => Some(source),
            Self::MissingEnv(_)
            | Self::UnknownRepositoryType(_)
            | Self::InvalidName(_)
            | Self::InvalidMode(_)
            | Self::MissingCaptureGroup(_) => None,
        }
    }
}

impl Config {
    /// Loads all configuration files (`*.toml`) from `dir`, or from
    /// [`default_config_dir`] if `dir` is `None`.
    ///
    /// Returns one result per file, in lexicographic order of the paths, so
    /// one broken file does not prevent the others from being processed.
    ///
    /// # Errors
    ///
    /// Fails if the default directory cannot be determined or the directory
    /// cannot be listed.
    pub fn load_dir(dir: Option<&Path>) -> Result<Vec<Result<Self, Error>>, Error> {
        let dir = match dir {
            Some(dir) => dir.to_path_buf(),
            None => default_config_dir()?,
        };

        let entries = dir.read_dir().map_err(|source| Error::ReadDir {
            path: dir.clone(),
            source,
        })?;

        let mut paths = Vec::new();
        for entry in entries {
            match entry {
                Ok(entry) => paths.push(entry.path()),
                Err(error) => {
                    log::warn!("Ignoring invalid entry in {}: {error}", dir.display());
                }
            }
        }

        // Only consider regular files (symlinks are followed) ending in
        // `.toml`; this skips subdirectories and editor backups (`*.toml~`).
        paths.retain(|path| path.is_file() && path.extension().is_some_and(|ext| ext == "toml"));
        // `read_dir` yields entries in an unspecified order.
        paths.sort();

        if paths.is_empty() {
            log::warn!("No configuration files found in {}", dir.display());
        }

        Ok(paths.iter().map(|path| Self::load_file(path)).collect())
    }

    /// Loads a single configuration file.
    ///
    /// # Errors
    ///
    /// Fails if the file cannot be read or is not a valid configuration.
    pub fn load_file(path: &Path) -> Result<Self, Error> {
        let content = std::fs::read_to_string(path).map_err(|source| Error::ReadFile {
            path: path.to_path_buf(),
            source,
        })?;

        toml::from_str(&content).map_err(|source| Error::Parse {
            path: path.to_path_buf(),
            source,
        })
    }
}

/// Default for [`Installation::mode`].
const DEFAULT_MODE: u32 = 0o755;

/// Returns whether the process runs with root privileges.
fn is_root() -> bool {
    // SAFETY: `geteuid` has no preconditions and always succeeds.
    unsafe { libc::geteuid() == 0 }
}

/// Returns `$HOME`, which must be an absolute path.
fn home_dir() -> Result<PathBuf, Error> {
    std::env::var_os("HOME")
        .map(PathBuf::from)
        .filter(|path| path.is_absolute())
        .ok_or(Error::MissingEnv("HOME"))
}

/// Returns the XDG base directory stored in `var`.
///
/// As required by the XDG Base Directory Specification, unset, empty and
/// relative values are ignored.
fn xdg_dir(var: &str) -> Option<PathBuf> {
    std::env::var_os(var)
        .map(PathBuf::from)
        .filter(|path| path.is_absolute())
}

/// Returns the directory configuration files are read from by default:
/// `/etc/grm` (root) or `$XDG_CONFIG_HOME/grm`, falling back to
/// `~/.config/grm` (user).
pub fn default_config_dir() -> Result<PathBuf, Error> {
    if is_root() {
        return Ok(PathBuf::from("/etc/grm"));
    }

    let config_home = match xdg_dir("XDG_CONFIG_HOME") {
        Some(dir) => dir,
        None => home_dir()?.join(".config"),
    };
    Ok(config_home.join("grm"))
}

/// Default install directory (key `dir`) for [`Installation::path`].
fn default_install_dir() -> Result<PathBuf, Error> {
    if is_root() {
        Ok(PathBuf::from("/usr/local/bin"))
    } else {
        Ok(home_dir()?.join(".local/bin"))
    }
}

/// Default for [`Installation::runtime_dir`].
fn default_runtime_dir() -> Result<PathBuf, Error> {
    let base = if is_root() {
        PathBuf::from("/run")
    } else {
        xdg_dir("XDG_RUNTIME_DIR").ok_or(Error::MissingEnv("XDG_RUNTIME_DIR"))?
    };
    Ok(base.join("grm"))
}

/// Default for [`Installation::data_dir`].
fn default_data_dir() -> Result<PathBuf, Error> {
    // As root, the record of installed programs is variable state, which the
    // FHS places in /var/lib (like dpkg's /var/lib/dpkg).
    let base = if is_root() {
        PathBuf::from("/var/lib")
    } else if let Some(dir) = xdg_dir("XDG_DATA_HOME") {
        dir
    } else {
        home_dir()?.join(".local/share")
    };
    Ok(base.join("grm"))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A configuration that sets every value explicitly, so parsing does not
    /// depend on the environment.
    const FULL: &str = r#"
        [repository]
        url = "https://example.org/releases"
        type = "github"

        [installation]
        name = "tool"
        dir = "/opt/bin"
        mode = 0o700
        runtime_dir = "/tmp/run"
        data_dir = "/tmp/data"

        [version]
        regex = '(?<major>\d+)\.(?<minor>\d+)\.(?<patch>\d+)'
    "#;

    fn parse(config: &str) -> Result<Config, toml::de::Error> {
        toml::from_str(config)
    }

    #[test]
    fn parses_full_config() {
        let config = parse(FULL).unwrap();
        assert_eq!(config.repository.ty, RepositoryType::Github);
        assert_eq!(config.installation.path, Path::new("/opt/bin/tool"));
        assert_eq!(config.installation.mode, 0o700);
        assert!(config.version.regex.is_match("v1.2.3"));
    }

    #[test]
    fn rejects_unknown_fields() {
        assert!(parse(&FULL.replace("runtime_dir", "runtime-dir")).is_err());
    }

    #[test]
    fn rejects_invalid_name() {
        for name in ["", ".", "..", "a/b", "/abs", "tool/"] {
            let config = FULL.replace("name = \"tool\"", &format!("name = \"{name}\""));
            assert!(parse(&config).is_err(), "accepted name {name:?}");
        }
    }

    #[test]
    fn rejects_invalid_mode() {
        assert!(parse(&FULL.replace("0o700", "0o17777")).is_err());
    }

    #[test]
    fn rejects_invalid_regex() {
        assert!(parse(&FULL.replace(r"\d+)'", r"\d+'")).is_err());
    }

    #[test]
    fn rejects_regex_without_capture_group() {
        assert!(parse(&FULL.replace("?<patch>", "")).is_err());
    }

    #[test]
    fn rejects_unknown_repository_type() {
        assert!(parse(&FULL.replace("type = \"github\"", "")).is_err());
    }

    #[test]
    fn derives_repository_type_from_host() {
        let github = Some(RepositoryType::Github);
        assert_eq!(RepositoryType::from_url("https://github.com/a/b"), github);
        assert_eq!(RepositoryType::from_url("https://GitHub.com/a/b"), github);
        assert_eq!(RepositoryType::from_url("https://www.github.com"), github);
        assert_eq!(
            RepositoryType::from_url("https://u@github.com:443/a"),
            github
        );
        assert_eq!(RepositoryType::from_url("github.com/a/b"), github);

        assert_eq!(RepositoryType::from_url("https://notgithub.com/a"), None);
        assert_eq!(RepositoryType::from_url("https://x.org/?github.com"), None);
        assert_eq!(RepositoryType::from_url("https://x.org/github.com"), None);
        assert_eq!(RepositoryType::from_url(""), None);
    }

    #[test]
    fn lock_file_is_named_after_binary() {
        let config = parse(FULL).unwrap();
        assert_eq!(LockFile::path(&config), Path::new("/tmp/run/.tool.lock"));
    }

    #[test]
    fn creates_dirs() {
        let base = std::env::temp_dir().join(format!("grm-test-{}", std::process::id()));
        let installation = Installation {
            path: base.join("bin/tool"),
            mode: DEFAULT_MODE,
            runtime_dir: base.join("run/grm"),
            data_dir: base.join("share/grm"),
        };

        // Creating twice must succeed, since the directories usually exist
        installation.create_dirs().unwrap();
        installation.create_dirs().unwrap();
        assert!(installation.runtime_dir.is_dir());
        assert!(installation.data_dir.is_dir());

        std::fs::remove_dir_all(&base).unwrap();
    }
}

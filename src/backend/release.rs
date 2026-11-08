//! Parts shared by the backends for hosting services with a GitHub-like
//! release API (GitHub, Forgejo).
//!
//! [`download_newest`] determines the newest release from a list of
//! releases: drafts and pre-releases are skipped, the tag of every other
//! release is matched against [`Asset::version`], and the release with the
//! highest version wins. If it is newer than the installed version, its
//! asset named [`Asset::target`] is downloaded, and if [`Asset::unpack`] is
//! set, the file named by it is extracted. The backends only provide the API
//! URLs and headers.
//!
//! # Security
//!
//! - All requests use HTTPS, including redirects, and credentials (in the
//!   headers a backend passes to [`Client::new`]) are never forwarded on
//!   redirects.
//! - Assets must have the size reported by the API, or at most
//!   [`MAX_UNKNOWN_SIZE`] if it is unknown, so a response cannot fill the
//!   disk.
//! - Asset names become file names in the temporary directory, so they must
//!   be plain file names.
//! - Assets are not verified beyond that (e.g. against published checksums
//!   or signatures), so a compromised release is installed as is.

use std::{
    fmt::{self, Display},
    io::{self, BufReader, Read, Write},
    path::{Path, PathBuf},
    time::Duration,
};

use serde::Deserialize;
use ureq::{Agent, config::RedirectAuthHeaders};

use super::{
    download::{self, Download, Version},
    unpack,
};
use crate::config::{Asset, Config};

/// Maximum size of the release list. 100 releases with many assets each
/// (e.g. lego on GitHub) take about 7 MiB, which is close to ureq's default
/// limit.
const MAX_RELEASES_SIZE: u64 = 64 * 1024 * 1024;

/// Maximum size of an asset whose size the API does not report (e.g. an
/// external asset on Forgejo). Assets of known size must have exactly that
/// size instead.
pub const MAX_UNKNOWN_SIZE: u64 = 2 * 1024 * 1024 * 1024;

/// Timeout for connecting and for each read, so that a stalled connection
/// fails instead of hanging. There is no overall timeout, since downloads
/// may be large.
const TIMEOUT: Duration = Duration::from_secs(30);

/// A release as returned by the API; unused fields are ignored.
#[derive(Debug, Deserialize)]
pub(super) struct Release {
    pub tag_name: String,
    pub draft: bool,
    pub prerelease: bool,
    pub assets: Vec<ReleaseAsset>,
}

/// A file attached to a [`Release`].
#[derive(Debug, Deserialize)]
pub(super) struct ReleaseAsset {
    /// ID of the asset in the API.
    pub id: u64,
    pub name: String,
    /// Size in bytes; meaningless for external assets (see
    /// [`ReleaseAsset::known_size`]).
    pub size: u64,
    /// URL of the file for browsers. Only used by Forgejo, which has no API
    /// endpoint for the file itself.
    #[serde(default)]
    pub browser_download_url: Option<String>,
    /// `attachment` (an uploaded file) or `external` (a link to another
    /// URL) on Forgejo; missing on GitHub.
    #[serde(default, rename = "type")]
    pub kind: Option<String>,
}

impl ReleaseAsset {
    /// Whether the asset is a link to another URL instead of an uploaded
    /// file (Forgejo).
    pub fn is_external(&self) -> bool {
        self.kind.as_deref() == Some("external")
    }

    /// The size in bytes, unless the asset is external: the API reports 0
    /// for those, since the file is not stored on the server.
    pub fn known_size(&self) -> Option<u64> {
        (!self.is_external()).then_some(self.size)
    }
}

/// The release chosen by [`select`].
#[derive(Debug)]
pub(super) struct Selection<'a> {
    pub version: Version,
    pub release: &'a Release,
    /// The asset named [`Asset::target`].
    pub asset: &'a ReleaseAsset,
    /// The file to extract from the asset, i.e. the expanded
    /// [`Asset::unpack`], if set.
    pub unpack_file: Option<String>,
}

/// Selects the release with the highest version among `releases` and its
/// asset named [`Asset::target`].
///
/// # Errors
///
/// Fails if no release matches [`Asset::version`], or if the newest one does
/// not have the asset. In the latter case, older releases are not considered,
/// since the asset may just not have been uploaded yet, or the target may be
/// wrong; either way, silently installing an older release would hide it.
pub(super) fn select<'a>(releases: &'a [Release], config: &Asset) -> Result<Selection<'a>, Error> {
    let (version, release, captures) = releases
        .iter()
        .filter(|release| !release.draft && !release.prerelease)
        .filter_map(|release| {
            let captures = config.version.captures(&release.tag_name)?;
            Some((Version::from_captures(&captures), release, captures))
        })
        .max_by(|(a, ..), (b, ..)| a.cmp(b))
        .ok_or(Error::NoRelease)?;

    let name = config.target(&captures);
    let asset = release
        .assets
        .iter()
        .find(|asset| asset.name == name)
        .ok_or_else(|| Error::MissingAsset {
            tag: release.tag_name.clone(),
            name,
        })?;

    // The name becomes a file name in the temporary directory, so it must
    // not be able to point elsewhere.
    if Path::new(&asset.name).file_name() != Some(asset.name.as_ref()) {
        return Err(Error::InvalidAssetName(asset.name.clone()));
    }

    Ok(Selection {
        version,
        release,
        asset,
        unpack_file: config.unpack.as_ref().map(|unpack| unpack.file(&captures)),
    })
}

/// Downloads the newest release of `config` into `tmp_dir` (see
/// [`Backend::download`](super::download::Backend::download)).
///
/// The releases are fetched from `releases_url`, and the selected asset from
/// the URL returned by `asset_url`. `repository` names the repository in
/// messages, e.g. `go-acme/lego`.
pub(super) fn download_newest(
    client: &Client,
    repository: &str,
    releases_url: &str,
    config: &Config,
    installed: Option<&Version>,
    tmp_dir: &Path,
    asset_url: impl FnOnce(&ReleaseAsset) -> Result<String, Error>,
) -> Result<Option<Download>, Error> {
    let releases: Vec<Release> = client.get_json(releases_url)?;
    log::trace!("Found {} releases of {repository}", releases.len());

    let Selection {
        version,
        release,
        asset,
        unpack_file,
    } = select(&releases, &config.asset)?;
    if let Some(installed) = installed.filter(|installed| **installed >= version) {
        log::info!(
            "Newest release of {repository} is {version} (tag {}), {installed} is installed",
            release.tag_name
        );
        return Ok(None);
    }
    log::info!(
        "Newest release of {repository} is {version} (tag {}), downloading {}",
        release.tag_name,
        asset.name
    );

    let downloaded = tmp_dir.join(&asset.name);
    client.download(&asset_url(asset)?, asset.known_size(), &downloaded)?;

    let target = match (&config.asset.unpack, unpack_file) {
        (Some(unpack), Some(file)) => {
            log::debug!("Extracting {file} from {}", asset.name);
            unpack::unpack(unpack.archive, &downloaded, &file, tmp_dir).map_err(Error::Unpack)?
        }
        _ => downloaded,
    };
    Ok(Some(Download { target, version }))
}

/// Whether `name` is a valid owner or repository name: only ASCII letters,
/// digits, `-`, `_` and `.` (as allowed by GitHub and Forgejo), and not `.`
/// or `..`. Names become part of API URLs, so this keeps them from
/// addressing other endpoints (e.g. with `..` or a percent-encoded `/`).
pub(super) fn is_valid_name(name: &str) -> bool {
    name != "."
        && name != ".."
        && !name.is_empty()
        && name
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_' | b'.'))
}

/// HTTPS client for a release API.
pub(super) struct Client {
    agent: Agent,
    /// Sent with every request, e.g. for authentication.
    headers: Vec<(&'static str, String)>,
    /// `Accept` header for JSON responses.
    json_accept: &'static str,
    /// `Accept` header for downloads of assets.
    download_accept: &'static str,
}

impl Client {
    /// Creates a client that sends `headers` with every request. Headers are
    /// not forwarded on redirects, so credentials in them only reach the
    /// hosts that requests are sent to.
    pub fn new(
        headers: Vec<(&'static str, String)>,
        json_accept: &'static str,
        download_accept: &'static str,
    ) -> Self {
        let agent = Agent::config_builder()
            .user_agent(concat!("grm/", env!("CARGO_PKG_VERSION")))
            .timeout_connect(Some(TIMEOUT))
            .timeout_per_read(Some(TIMEOUT))
            .https_only(true)
            // Already the default, but credentials must never reach the hosts
            // that downloads are redirected to
            .redirect_auth_headers(RedirectAuthHeaders::Never)
            .build()
            .into();
        Self {
            agent,
            headers,
            json_accept,
            download_accept,
        }
    }

    /// Sends a GET request.
    fn get(&self, url: &str, accept: &str) -> Result<ureq::Body, Error> {
        log::debug!("GET {url}");
        let mut request = self.agent.get(url).header("Accept", accept);
        for (name, value) in &self.headers {
            request = request.header(*name, value);
        }

        let response = request.call().map_err(|source| Error::Request {
            url: url.to_owned(),
            source,
        })?;
        Ok(response.into_body())
    }

    /// Fetches and parses a JSON response.
    fn get_json<T: serde::de::DeserializeOwned>(&self, url: &str) -> Result<T, Error> {
        let body = self.get(url, self.json_accept)?;
        let reader = body.into_with_config().limit(MAX_RELEASES_SIZE).reader();
        serde_json::from_reader(BufReader::new(reader)).map_err(|source| Error::Parse {
            url: url.to_owned(),
            source,
        })
    }

    /// Downloads `url` to `path`; see [`write_download`] for `size`.
    fn download(&self, url: &str, size: Option<u64>, path: &Path) -> Result<(), Error> {
        let body = self.get(url, self.download_accept)?;
        write_download(&mut body.into_reader(), size, path).map_err(|source| Error::Write {
            path: path.to_path_buf(),
            source,
        })
    }
}

/// Writes `reader` to `path` (see [`download::create_file`]). It must yield
/// exactly `size` bytes if known, or at most [`MAX_UNKNOWN_SIZE`] bytes
/// otherwise. A partially written file is removed on failure.
fn write_download(reader: &mut impl Read, size: Option<u64>, path: &Path) -> io::Result<()> {
    write_download_with_limit(reader, size, MAX_UNKNOWN_SIZE, path)
}

/// [`write_download`] with a configurable limit for unknown sizes, for tests.
fn write_download_with_limit(
    reader: &mut impl Read,
    size: Option<u64>,
    limit: u64,
    path: &Path,
) -> io::Result<()> {
    let max = size.unwrap_or(limit);
    let mut file = download::create_file(path)?;
    // At most one byte more than allowed is read, so a response that is too
    // large cannot fill the disk
    let result = io::copy(&mut reader.take(max.saturating_add(1)), &mut file).and_then(|copied| {
        let error = |message| Err(io::Error::new(io::ErrorKind::InvalidData, message));
        match size {
            Some(size) if copied > size => {
                return error(format!("expected {size} bytes, but received more"));
            }
            Some(size) if copied < size => {
                return error(format!("expected {size} bytes, but received {copied}"));
            }
            None if copied > limit => {
                return error(format!("larger than the maximum of {limit} bytes"));
            }
            _ => {}
        }
        file.flush()?;
        file.sync_all()
    });

    if result.is_err() {
        let _ = std::fs::remove_file(path);
    }
    result
}

/// Errors while finding or downloading a release.
#[derive(Debug)]
pub enum Error {
    /// The repository URL does not name a repository of the hosting service;
    /// `expected` describes the expected form.
    InvalidUrl { url: String, expected: &'static str },

    /// A request failed, including responses with an error status.
    Request { url: String, source: ureq::Error },

    /// A response is not the expected JSON.
    Parse {
        url: String,
        source: serde_json::Error,
    },

    /// No release has a tag matching the version regex.
    NoRelease,

    /// The newest release lacks the asset named by `target`.
    MissingAsset { tag: String, name: String },

    /// The asset's name is not a plain file name.
    InvalidAssetName(String),

    /// The asset's download URL is missing, not an HTTPS URL, or contains
    /// characters other than printable ASCII.
    InvalidDownloadUrl { name: String, url: Option<String> },

    /// The downloaded asset could not be written.
    Write { path: PathBuf, source: io::Error },

    /// The file could not be extracted from the downloaded archive.
    Unpack(unpack::Error),
}

impl Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::InvalidUrl { url, expected } => {
                write!(f, "\"{url}\" is not a {expected} repository URL")
            }
            Self::Request { url, source } => write!(f, "request to {url} failed: {source}"),
            Self::Parse { url, source } => {
                write!(f, "unexpected response from {url}: {source}")
            }
            Self::NoRelease => write!(
                f,
                "no release (other than drafts and pre-releases) has a tag matching the version regex"
            ),
            Self::MissingAsset { tag, name } => {
                write!(f, "release {tag} has no asset named \"{name}\"")
            }
            Self::InvalidAssetName(name) => {
                write!(
                    f,
                    "invalid asset name \"{name}\": must be a plain file name"
                )
            }
            // Debug formatting escapes control characters, since the URL
            // comes from the server
            Self::InvalidDownloadUrl {
                name,
                url: Some(url),
            } => {
                write!(f, "asset \"{name}\" has an invalid download URL: {url:?}")
            }
            Self::InvalidDownloadUrl { name, url: None } => {
                write!(f, "asset \"{name}\" has no download URL")
            }
            Self::Write { path, source } => {
                write!(f, "unable to write {}: {source}", path.display())
            }
            Self::Unpack(error) => error.fmt(f),
        }
    }
}

impl std::error::Error for Error {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Request { source, .. } => Some(source),
            Self::Parse { source, .. } => Some(source),
            Self::Write { source, .. } => Some(source),
            // Displayed as this error, so the source is the unpack error's
            Self::Unpack(error) => error.source(),
            Self::InvalidUrl { .. }
            | Self::NoRelease
            | Self::MissingAsset { .. }
            | Self::InvalidAssetName(_)
            | Self::InvalidDownloadUrl { .. } => None,
        }
    }
}

#[cfg(test)]
pub(super) mod tests {
    use super::*;

    /// A configuration for `url` with `target` and any further `[asset]`
    /// keys in `extra`.
    pub fn config_for(url: &str, target: &str, extra: &str) -> Config {
        toml::from_str(&format!(
            r#"
            [repository]
            url = "{url}"

            [installation]
            name = "tool"
            dir = "/opt/bin"
            runtime_dir = "/tmp/run"
            data_dir = "/tmp/data"

            [asset]
            version = '(?<major>\d+)\.(?<minor>\d+)\.(?<patch>\d+)'
            target = '{target}'
            {extra}
            "#
        ))
        .unwrap()
    }

    fn config(target: &str) -> Config {
        config_for("https://github.com/owner/repo", target, "")
    }

    /// A unique, empty directory.
    pub fn test_dir(name: &str) -> PathBuf {
        let dir =
            std::env::temp_dir().join(format!("grm-release-test-{}-{name}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn release(tag: &str, draft: bool, prerelease: bool) -> serde_json::Value {
        serde_json::json!({
            "tag_name": tag,
            "draft": draft,
            "prerelease": prerelease,
            "name": "ignored",
            "assets": [{
                "id": tag.len(),
                "name": format!("tool_{tag}.tar.gz"),
                "url": "https://api.github.com/ignored",
                "size": 1,
            }],
        })
    }

    /// Releases in the API's order (newest first), with the highest stable
    /// version neither first nor last.
    fn releases() -> Vec<Release> {
        serde_json::from_value(serde_json::json!([
            release("nightly", false, false),
            release("v3.0.0", true, false),
            release("v2.0.0-rc1", false, true),
            release("v1.9.0", false, false),
            release("v1.10.0", false, false),
            release("v1.2.3", false, false),
        ]))
        .unwrap()
    }

    #[test]
    fn selects_highest_stable_version() {
        let releases = releases();
        let config = config("tool_v${major}.${minor}.${patch}.tar.gz");

        let selection = select(&releases, &config.asset).unwrap();
        assert_eq!(selection.version.to_string(), "1.10.0");
        assert_eq!(selection.release.tag_name, "v1.10.0");
        assert_eq!(selection.asset.name, "tool_v1.10.0.tar.gz");
        assert_eq!(selection.asset.id, "v1.10.0".len() as u64);
        assert_eq!(selection.unpack_file, None);
    }

    #[test]
    fn expands_unpack_file() {
        let releases = releases();
        let config = config_for(
            "https://github.com/owner/repo",
            "tool_v${major}.${minor}.${patch}.tar.gz",
            "unpack = 'tool-${major}.${minor}/tool'",
        );

        let selection = select(&releases, &config.asset).unwrap();
        assert_eq!(selection.unpack_file.as_deref(), Some("tool-1.10/tool"));
    }

    #[test]
    fn fails_without_matching_release() {
        let releases: Vec<Release> =
            serde_json::from_value(serde_json::json!([release("nightly", false, false)])).unwrap();
        let config = config("tool");
        assert!(matches!(
            select(&releases, &config.asset),
            Err(Error::NoRelease)
        ));
    }

    #[test]
    fn fails_if_newest_release_lacks_asset() {
        let releases = releases();
        let config = config("tool_${major}.tar.gz");
        match select(&releases, &config.asset) {
            Err(Error::MissingAsset { tag, name }) => {
                assert_eq!(tag, "v1.10.0");
                assert_eq!(name, "tool_1.tar.gz");
            }
            other => panic!("unexpected result: {other:?}"),
        }
    }

    #[test]
    fn rejects_invalid_asset_name() {
        for name in ["..", ".", "../tool", "dir/tool"] {
            let releases: Vec<Release> = serde_json::from_value(serde_json::json!([{
                "tag_name": "v1.2.3",
                "draft": false,
                "prerelease": false,
                "assets": [{ "id": 1, "name": name, "size": 1 }],
            }]))
            .unwrap();
            // A target that expands to exactly the malicious name
            let config = config(&name.replace('$', "$$"));
            assert!(
                matches!(
                    select(&releases, &config.asset),
                    Err(Error::InvalidAssetName(_))
                ),
                "accepted {name:?}"
            );
        }
    }

    #[test]
    fn parses_forgejo_assets() {
        // As returned by Forgejo, with an uploaded and an external asset
        let assets: Vec<ReleaseAsset> = serde_json::from_value(serde_json::json!([
            {
                "id": 1, "name": "tool", "size": 42, "type": "attachment",
                "browser_download_url": "https://codeberg.org/o/r/releases/download/v1/tool",
            },
            {
                "id": 2, "name": "tool", "size": 0, "type": "external",
                "browser_download_url": "https://example.org/tool",
            },
        ]))
        .unwrap();
        assert_eq!(assets[0].known_size(), Some(42));
        assert!(!assets[0].is_external());
        assert_eq!(assets[1].known_size(), None);
        assert!(assets[1].is_external());

        // GitHub has neither field
        let asset: ReleaseAsset =
            serde_json::from_value(serde_json::json!({ "id": 1, "name": "tool", "size": 42 }))
                .unwrap();
        assert_eq!(asset.known_size(), Some(42));
        assert_eq!(asset.browser_download_url, None);
    }

    #[test]
    fn validates_names() {
        for name in ["owner", "a.b", "a-b_c", "A1"] {
            assert!(is_valid_name(name), "{name}");
        }
        for name in ["", ".", "..", "a/b", "a%2Fb", "a b", "rëpo", "a?b"] {
            assert!(!is_valid_name(name), "{name}");
        }
    }

    #[test]
    fn writes_download_of_expected_size() {
        let dir = test_dir("size");
        let path = dir.join("asset");

        write_download(&mut &b"12345"[..], Some(5), &path).unwrap();
        assert_eq!(std::fs::read(&path).unwrap(), b"12345");

        // Truncated and oversized responses are rejected and removed
        for (data, size) in [(&b"1234"[..], 5), (&b"123456"[..], 5)] {
            let error = write_download(&mut &data[..], Some(size), &path).unwrap_err();
            assert_eq!(error.kind(), io::ErrorKind::InvalidData);
            assert!(!path.exists());
        }

        // At most one byte beyond the expected size is read
        let mut endless = io::repeat(b'x');
        assert!(write_download(&mut endless, Some(5), &path).is_err());

        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn limits_download_of_unknown_size() {
        let dir = test_dir("unknown-size");
        let path = dir.join("asset");

        // Any size up to the limit is fine
        for data in [&b""[..], b"1", b"12345"] {
            write_download_with_limit(&mut &data[..], None, 5, &path).unwrap();
            assert_eq!(std::fs::read(&path).unwrap(), data);
        }

        // Larger responses are rejected and removed, even endless ones
        let error = write_download_with_limit(&mut &b"123456"[..], None, 5, &path).unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::InvalidData);
        assert!(!path.exists());
        assert!(write_download_with_limit(&mut io::repeat(b'x'), None, 5, &path).is_err());
        assert!(!path.exists());

        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn download_does_not_follow_symlinks() {
        let dir = test_dir("symlink");
        let victim = dir.join("victim");
        std::fs::write(&victim, "keep").unwrap();
        let path = dir.join("asset");
        std::os::unix::fs::symlink(&victim, &path).unwrap();

        write_download(&mut &b"new"[..], Some(3), &path).unwrap();
        assert_eq!(std::fs::read_to_string(&victim).unwrap(), "keep");
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "new");

        std::fs::remove_dir_all(&dir).unwrap();
    }
}

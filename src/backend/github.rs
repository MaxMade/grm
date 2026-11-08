//! [`Backend`] for repositories hosted on GitHub, using its REST API.
//!
//! The newest release is determined from the most recent releases
//! (`GET /repos/{owner}/{repo}/releases`): drafts and pre-releases are
//! skipped, the tag of every other release is matched against
//! [`Asset::version`], and the release with the highest version wins. Its
//! asset named [`Asset::target`] is then downloaded through the API
//! (`GET /repos/{owner}/{repo}/releases/assets/{id}`), which redirects to
//! the actual file. If [`Asset::unpack`] is set, the asset is an archive
//! and the file named by it is extracted.
//!
//! Without authentication, GitHub allows 60 API requests per hour and IP
//! address. If `GITHUB_TOKEN` is set, it is sent with every API request,
//! which raises the limit and gives access to private repositories.
//!
//! # Security
//!
//! - All requests use HTTPS, including redirects.
//! - The token is only sent to [`API_URL`]: request URLs are built from the
//!   validated owner and repository and from asset IDs, never taken from
//!   responses, and the token is not forwarded on redirects.
//! - The downloaded file must have the size reported by the API.
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
    download::{self, Backend, Download, Version},
    unpack,
};
use crate::config::{Asset, Config};

/// Base URL of the GitHub REST API.
const API_URL: &str = "https://api.github.com";

/// Version of the GitHub REST API this backend is written against.
const API_VERSION: &str = "2022-11-28";

/// Number of most recent releases considered, which is the maximum the API
/// returns per request. Older releases are ignored, so a newer version must
/// not be released more than this many releases before the current one.
const RELEASES_PER_PAGE: u32 = 100;

/// Maximum size of the release list. 100 releases with many assets each
/// (e.g. lego) take about 7 MiB, which is close to ureq's default limit.
const MAX_RELEASES_SIZE: u64 = 64 * 1024 * 1024;

/// Timeout for connecting and for each read, so that a stalled connection
/// fails instead of hanging. There is no overall timeout, since downloads
/// may be large.
const TIMEOUT: Duration = Duration::from_secs(30);

/// Releases hosted on GitHub.
pub struct Github;

impl Backend for Github {
    type Error = Error;

    /// Downloads the newest release's asset into `tmp_dir` and extracts the
    /// file named by [`Asset::unpack`] from it if set. Files are named after
    /// the asset or the extracted file; see [`Backend::download`] for the
    /// requirements on `tmp_dir`.
    fn download<P: AsRef<Path>>(config: &Config, tmp_dir: P) -> Result<Download, Self::Error> {
        let (owner, repo) = parse_repository_url(&config.repository.url)
            .ok_or_else(|| Error::InvalidUrl(config.repository.url.clone()))?;
        let client = Client::new();

        let url = format!("{API_URL}/repos/{owner}/{repo}/releases?per_page={RELEASES_PER_PAGE}");
        let releases: Vec<Release> = client.get_json(&url)?;
        log::trace!("Found {} releases of {owner}/{repo}", releases.len());

        let Selection {
            version,
            release,
            asset,
            unpack_file,
        } = select(&releases, &config.asset)?;
        log::info!(
            "Newest release of {owner}/{repo} is {version} (tag {}), downloading {}",
            release.tag_name,
            asset.name
        );

        let tmp_dir = tmp_dir.as_ref();
        let downloaded = tmp_dir.join(&asset.name);
        let url = format!(
            "{API_URL}/repos/{owner}/{repo}/releases/assets/{}",
            asset.id
        );
        client.download(&url, asset.size, &downloaded)?;

        let target = match (&config.asset.unpack, unpack_file) {
            (Some(unpack), Some(file)) => {
                log::debug!("Extracting {file} from {}", asset.name);
                unpack::unpack(unpack.archive, &downloaded, &file, tmp_dir)
                    .map_err(Error::Unpack)?
            }
            _ => downloaded,
        };
        Ok(Download { target, version })
    }
}

/// A release as returned by the API; unused fields are ignored.
#[derive(Debug, Deserialize)]
struct Release {
    tag_name: String,
    draft: bool,
    prerelease: bool,
    assets: Vec<ReleaseAsset>,
}

/// A file attached to a [`Release`].
#[derive(Debug, Deserialize)]
struct ReleaseAsset {
    /// ID used to download the asset through the API.
    id: u64,
    name: String,
    /// Size in bytes.
    size: u64,
}

/// The release chosen by [`select`].
#[derive(Debug)]
struct Selection<'a> {
    version: Version,
    release: &'a Release,
    /// The asset named [`Asset::target`].
    asset: &'a ReleaseAsset,
    /// The file to extract from the asset, i.e. the expanded
    /// [`Asset::unpack`], if set.
    unpack_file: Option<String>,
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
fn select<'a>(releases: &'a [Release], config: &Asset) -> Result<Selection<'a>, Error> {
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

/// Extracts owner and repository from a GitHub URL such as
/// `https://github.com/<owner>/<repo>/releases`. The scheme, a trailing
/// `.git` and anything after the repository are optional.
///
/// Owner and repository may only contain the characters GitHub allows
/// (ASCII letters, digits, `-`, `_` and `.`) and must not be `.` or `..`.
/// They become part of API URLs, so this keeps them from addressing other
/// endpoints (e.g. with `..` or a percent-encoded `/`).
fn parse_repository_url(url: &str) -> Option<(&str, &str)> {
    let rest = url.split_once("://").map_or(url, |(_, rest)| rest);
    let mut parts = rest.split(['/', '?', '#']);

    let host = parts.next()?.to_ascii_lowercase();
    if host != "github.com" && host != "www.github.com" {
        return None;
    }

    let owner = parts.next()?;
    let repo = parts.next()?;
    let repo = repo.strip_suffix(".git").unwrap_or(repo);
    (is_valid_name(owner) && is_valid_name(repo)).then_some((owner, repo))
}

/// Whether `name` is a valid owner or repository name (see
/// [`parse_repository_url`]).
fn is_valid_name(name: &str) -> bool {
    name != "."
        && name != ".."
        && !name.is_empty()
        && name
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_' | b'.'))
}

/// HTTP client for the GitHub API.
struct Client {
    agent: Agent,
    token: Option<String>,
}

impl Client {
    fn new() -> Self {
        let agent = Agent::config_builder()
            .user_agent(concat!("grm/", env!("CARGO_PKG_VERSION")))
            .timeout_connect(Some(TIMEOUT))
            .timeout_per_read(Some(TIMEOUT))
            .https_only(true)
            // Already the default, but the token must never reach the host
            // GitHub redirects downloads to
            .redirect_auth_headers(RedirectAuthHeaders::Never)
            .build()
            .into();
        let token = std::env::var("GITHUB_TOKEN")
            .ok()
            .filter(|token| !token.is_empty());
        if token.is_some() {
            log::debug!("Authenticating to the GitHub API with $GITHUB_TOKEN");
        }
        Self { agent, token }
    }

    /// Sends a GET request with the headers the API expects.
    fn get(&self, url: &str, accept: &str) -> Result<ureq::Body, Error> {
        log::debug!("GET {url}");
        let mut request = self
            .agent
            .get(url)
            .header("Accept", accept)
            .header("X-GitHub-Api-Version", API_VERSION);
        if let Some(token) = &self.token {
            request = request.header("Authorization", format!("Bearer {token}"));
        }

        let response = request.call().map_err(|source| Error::Request {
            url: url.to_owned(),
            source,
        })?;
        Ok(response.into_body())
    }

    /// Fetches and parses a JSON response.
    fn get_json<T: serde::de::DeserializeOwned>(&self, url: &str) -> Result<T, Error> {
        let body = self.get(url, "application/vnd.github+json")?;
        let reader = body.into_with_config().limit(MAX_RELEASES_SIZE).reader();
        serde_json::from_reader(BufReader::new(reader)).map_err(|source| Error::Parse {
            url: url.to_owned(),
            source,
        })
    }

    /// Downloads the asset at the API URL `url`, which must be `size` bytes
    /// long, to `path`.
    fn download(&self, url: &str, size: u64, path: &Path) -> Result<(), Error> {
        let body = self.get(url, "application/octet-stream")?;
        write_download(&mut body.into_reader(), size, path).map_err(|source| Error::Write {
            path: path.to_path_buf(),
            source,
        })
    }
}

/// Writes `reader`, which must yield exactly `size` bytes, to `path` (see
/// [`download::create_file`]). At most one byte more than `size` is read, so
/// a response that is too large cannot fill the disk. A partially written
/// file is removed on failure.
fn write_download(reader: &mut impl Read, size: u64, path: &Path) -> io::Result<()> {
    let mut file = download::create_file(path)?;
    let result = io::copy(&mut reader.take(size.saturating_add(1)), &mut file).and_then(|copied| {
        if copied != size {
            let received = if copied > size {
                "more".to_owned()
            } else {
                copied.to_string()
            };
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                format!("expected {size} bytes, but received {received}"),
            ));
        }
        file.flush()?;
        file.sync_all()
    });

    if result.is_err() {
        let _ = std::fs::remove_file(path);
    }
    result
}

/// Errors of the GitHub backend.
#[derive(Debug)]
pub enum Error {
    /// The repository URL does not name a GitHub repository.
    InvalidUrl(String),

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

    /// The downloaded asset could not be written.
    Write { path: PathBuf, source: io::Error },

    /// The file could not be extracted from the downloaded archive.
    Unpack(unpack::Error),
}

impl Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::InvalidUrl(url) => write!(
                f,
                "\"{url}\" is not a GitHub repository URL (https://github.com/<owner>/<repo>)"
            ),
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
            Self::InvalidUrl(_)
            | Self::NoRelease
            | Self::MissingAsset { .. }
            | Self::InvalidAssetName(_) => None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn config(target: &str) -> Config {
        config_with(target, "")
    }

    /// A configuration with `target` and any further `[asset]` keys in
    /// `extra`.
    fn config_with(target: &str, extra: &str) -> Config {
        toml::from_str(&format!(
            r#"
            [repository]
            url = "https://github.com/owner/repo/releases"

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
        let config = config_with(
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
    fn parses_repository_url() {
        let expected = Some(("owner", "repo"));
        for url in [
            "https://github.com/owner/repo/releases",
            "https://github.com/owner/repo",
            "https://github.com/owner/repo/",
            "https://GitHub.com/owner/repo.git",
            "https://www.github.com/owner/repo?tab=readme",
            "github.com/owner/repo",
        ] {
            assert_eq!(parse_repository_url(url), expected, "{url}");
        }

        for url in [
            "https://github.com/owner",
            "https://github.com/owner/",
            "https://github.com//repo",
            "https://gitlab.com/owner/repo",
            "https://notgithub.com/owner/repo",
            // Must not address other API endpoints
            "https://github.com/../user",
            "https://github.com/owner/..",
            "https://github.com/owner/.",
            "https://github.com/owner/%2E%2E",
            "https://github.com/owner%2Frepo/x",
            "https://github.com/owner/repo%3Fx",
            "https://github.com/own er/repo",
            "https://github.com/owner/rëpo",
            "https://github.com/owner/.git",
        ] {
            assert_eq!(parse_repository_url(url), None, "{url}");
        }

        // Dots are fine within names
        assert_eq!(
            parse_repository_url("https://github.com/a.b/c.d.git"),
            Some(("a.b", "c.d"))
        );
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

    /// A unique, empty directory that is removed afterwards.
    fn test_dir(name: &str) -> PathBuf {
        let dir =
            std::env::temp_dir().join(format!("grm-github-test-{}-{name}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn writes_download_of_expected_size() {
        let dir = test_dir("size");
        let path = dir.join("asset");

        write_download(&mut &b"12345"[..], 5, &path).unwrap();
        assert_eq!(std::fs::read(&path).unwrap(), b"12345");

        // Truncated and oversized responses are rejected and removed
        for (data, size) in [(&b"1234"[..], 5), (&b"123456"[..], 5)] {
            let error = write_download(&mut &data[..], size, &path).unwrap_err();
            assert_eq!(error.kind(), io::ErrorKind::InvalidData);
            assert!(!path.exists());
        }

        // At most one byte beyond the expected size is read
        let mut endless = io::repeat(b'x');
        assert!(write_download(&mut endless, 5, &path).is_err());

        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn download_does_not_follow_symlinks() {
        let dir = test_dir("symlink");
        let victim = dir.join("victim");
        std::fs::write(&victim, "keep").unwrap();
        let path = dir.join("asset");
        std::os::unix::fs::symlink(&victim, &path).unwrap();

        write_download(&mut &b"new"[..], 3, &path).unwrap();
        assert_eq!(std::fs::read_to_string(&victim).unwrap(), "keep");
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "new");

        std::fs::remove_dir_all(&dir).unwrap();
    }

    /// Downloads the newest lego release for real; run with
    /// `cargo test -- --ignored`.
    #[test]
    #[ignore = "needs network access to GitHub"]
    fn downloads_lego() {
        let mut config = config_with(
            "lego_v${major}.${minor}.${patch}_linux_amd64.tar.gz",
            "unpack = 'lego'",
        );
        config.repository.url = "https://github.com/go-acme/lego/releases".to_owned();
        let dir = test_dir("lego");

        let result = Github::download(&config, &dir);
        let size = result
            .as_ref()
            .ok()
            .map(|download| std::fs::metadata(&download.target).unwrap().len());
        std::fs::remove_dir_all(&dir).unwrap();

        // The extracted binary, not the archive
        let download = result.unwrap();
        assert!(download.target.ends_with("lego"));
        assert!(size.unwrap() > 1024 * 1024, "unexpectedly small: {size:?}");
    }
}

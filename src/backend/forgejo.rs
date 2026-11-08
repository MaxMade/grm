//! [`Backend`] for repositories hosted on a Forgejo instance (e.g. Codeberg),
//! using its REST API (`/api/v1`), which Gitea shares.
//!
//! The newest release is determined from the most recent stable releases
//! (`GET /api/v1/repos/{owner}/{repo}/releases`, see [`release`] for how),
//! and its asset is downloaded from the URL the API reports for browsers
//! (`browser_download_url`), since the API has no endpoint for the file
//! itself.
//!
//! Instances limit the releases per request (`max_response_items`, 50 by
//! default), so only the [`RELEASES_PER_PAGE`] most recent releases are
//! considered. Instances installed under a path (e.g.
//! `https://example.org/forgejo/`) are not supported.
//!
//! # Security
//!
//! See [`release`] for the protections shared with other backends.
//!
//! - Only HTTPS instances are supported, and the host may only consist of
//!   letters, digits, `.`, `-` and a port, so a URL cannot contain
//!   credentials or address anything but the instance's API.
//! - No credentials are sent at all: a single token in the environment would
//!   be sent to every instance in any configuration, so only public
//!   repositories are supported.
//! - An asset's download URL is taken from the API's response. It must use
//!   HTTPS and consist of printable ASCII characters only (as any valid URL
//!   does), so it cannot inject control sequences into the terminal when it
//!   is logged.
//! - The download URL may point to another host, since Forgejo supports
//!   external assets (links). Restricting it to the instance would gain
//!   nothing, since whoever publishes a release can create external assets.
//!   It does let them make `grm` send an HTTPS GET request (without
//!   credentials) to any host, including hosts on the local network that
//!   only the machine running `grm` can reach. Preventing that would require
//!   checking the resolved addresses of every request, so it is accepted:
//!   only configure repositories whose publishers you trust to run code on
//!   the machine anyway.

use std::path::Path;

use super::{
    download::{Backend, Download, Version},
    release::{self, Client, Error, ReleaseAsset, is_valid_name},
};
use crate::config::Config;

/// Number of most recent releases requested, which is Forgejo's default
/// maximum per request. Older releases are ignored, so a newer version must
/// not be released more than this many releases before the current one.
const RELEASES_PER_PAGE: u32 = 50;

/// Expected form of repository URLs, for error messages.
const EXPECTED_URL: &str = "Forgejo (https://<host>/<owner>/<repo>)";

/// Releases hosted on a Forgejo instance.
pub struct Forgejo;

impl Backend for Forgejo {
    type Error = Error;

    /// Downloads the newest release's asset into `tmp_dir` and extracts the
    /// file named by [`Asset::unpack`] from it if set. Files are named after
    /// the asset or the extracted file; see [`Backend::download`] for the
    /// requirements on `tmp_dir`.
    ///
    /// [`Asset::unpack`]: crate::config::Asset::unpack
    fn download<P: AsRef<Path>>(
        config: &Config,
        installed: Option<&Version>,
        tmp_dir: P,
    ) -> Result<Option<Download>, Self::Error> {
        let repository =
            parse_repository_url(&config.repository.url).ok_or_else(|| Error::InvalidUrl {
                url: config.repository.url.clone(),
                expected: EXPECTED_URL,
            })?;

        release::download_newest(
            &Client::new(Vec::new(), "application/json", "application/octet-stream"),
            &format!("{}/{}", repository.owner, repository.repo),
            &repository.releases_url(),
            config,
            installed,
            tmp_dir.as_ref(),
            |asset| asset_url(asset, &repository.origin),
        )
    }
}

/// A repository on a Forgejo instance.
#[derive(Debug, PartialEq, Eq)]
struct Repository<'a> {
    /// `https://<host>[:<port>]`, with the host in lowercase.
    origin: String,
    owner: &'a str,
    repo: &'a str,
}

impl Repository<'_> {
    /// URL of the API endpoint listing the repository's most recent stable
    /// releases. Drafts and pre-releases are skipped anyway, but excluding
    /// them here leaves more room for stable releases within the limit.
    fn releases_url(&self) -> String {
        let Self {
            origin,
            owner,
            repo,
        } = self;
        format!(
            "{origin}/api/v1/repos/{owner}/{repo}/releases\
             ?limit={RELEASES_PER_PAGE}&draft=false&pre-release=false"
        )
    }
}

/// Extracts the instance, owner and repository from a URL such as
/// `https://codeberg.org/<owner>/<repo>/releases`. The scheme (which must be
/// `https` if given), a trailing `.git` and anything after the repository
/// are optional. Owner and repository must be valid names (see
/// [`is_valid_name`]), and the host may only consist of ASCII letters,
/// digits, `.` and `-`, optionally followed by a port. IPv6 addresses are
/// not supported.
fn parse_repository_url(url: &str) -> Option<Repository<'_>> {
    let rest = match url.split_once("://") {
        Some((scheme, rest)) if scheme.eq_ignore_ascii_case("https") => rest,
        Some(_) => return None,
        None => url,
    };
    let mut parts = rest.split(['/', '?', '#']);

    let authority = parts.next()?;
    let (host, port) = match authority.split_once(':') {
        Some((host, port)) => (host, Some(port)),
        None => (authority, None),
    };
    let host_is_valid = !host.is_empty()
        && host
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'.' | b'-'));
    let port_is_valid = port.is_none_or(|port| {
        port.bytes().all(|b| b.is_ascii_digit()) && port.parse::<u16>().is_ok_and(|p| p > 0)
    });
    if !host_is_valid || !port_is_valid {
        return None;
    }

    let owner = parts.next()?;
    let repo = parts.next()?;
    let repo = repo.strip_suffix(".git").unwrap_or(repo);
    (is_valid_name(owner) && is_valid_name(repo)).then(|| Repository {
        origin: format!("https://{}", authority.to_ascii_lowercase()),
        owner,
        repo,
    })
}

/// Returns the URL to download `asset` from, i.e. its browser download URL,
/// which must use HTTPS and consist of printable ASCII characters only (no
/// spaces or control characters, which a valid URL encodes anyway). `origin`
/// is the instance's, for messages about external assets.
fn asset_url(asset: &ReleaseAsset, origin: &str) -> Result<String, Error> {
    let url = asset
        .browser_download_url
        .as_deref()
        .filter(|url| !url.is_empty());
    let is_valid =
        |url: &&str| url.starts_with("https://") && url.bytes().all(|b| b.is_ascii_graphic());
    let Some(url) = url.filter(is_valid) else {
        return Err(Error::InvalidDownloadUrl {
            name: asset.name.clone(),
            url: url.map(str::to_owned),
        });
    };

    if asset.is_external() || !url.starts_with(&format!("{origin}/")) {
        log::info!("{} is an external asset at {url}", asset.name);
    }
    Ok(url.to_owned())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::backend::release::tests::{config_for, test_dir};

    fn repository<'a>(origin: &str, owner: &'a str, repo: &'a str) -> Option<Repository<'a>> {
        Some(Repository {
            origin: origin.to_owned(),
            owner,
            repo,
        })
    }

    #[test]
    fn parses_repository_url() {
        let codeberg = repository("https://codeberg.org", "owner", "repo");
        for url in [
            "https://codeberg.org/owner/repo/releases",
            "https://codeberg.org/owner/repo",
            "https://codeberg.org/owner/repo/",
            "HTTPS://Codeberg.org/owner/repo.git",
            "https://codeberg.org/owner/repo?tab=releases",
            "codeberg.org/owner/repo",
        ] {
            assert_eq!(parse_repository_url(url), codeberg, "{url}");
        }

        // Any instance, with an optional port
        assert_eq!(
            parse_repository_url("https://git.example.org:3000/a.b/c-d"),
            repository("https://git.example.org:3000", "a.b", "c-d")
        );
        assert_eq!(
            parse_repository_url("https://192.168.1.2/owner/repo"),
            repository("https://192.168.1.2", "owner", "repo")
        );

        for url in [
            "https://codeberg.org/owner",
            "https://codeberg.org//repo",
            // Only HTTPS
            "http://codeberg.org/owner/repo",
            "ftp://codeberg.org/owner/repo",
            // No credentials or anything else that changes the host
            "https://user@codeberg.org/owner/repo",
            "https://user:pass@codeberg.org/owner/repo",
            "https://evil.org\\@codeberg.org/owner/repo",
            "https://codeberg.org%2F@evil.org/owner/repo",
            "https:///owner/repo",
            "https://[::1]/owner/repo",
            // Invalid ports
            "https://codeberg.org:/owner/repo",
            "https://codeberg.org:0/owner/repo",
            "https://codeberg.org:65536/owner/repo",
            "https://codeberg.org:+1/owner/repo",
            "https://codeberg.org:80:80/owner/repo",
            // Must not address other API endpoints
            "https://codeberg.org/../user",
            "https://codeberg.org/owner/..",
            "https://codeberg.org/owner/%2E%2E",
            "https://codeberg.org/owner%2Frepo/x",
            "https://codeberg.org/owner/.git",
        ] {
            assert_eq!(parse_repository_url(url), None, "{url}");
        }
    }

    #[test]
    fn builds_releases_url() {
        let repository = parse_repository_url("https://Git.Example.org:3000/a.b/c-d.git").unwrap();
        assert_eq!(
            repository.releases_url(),
            "https://git.example.org:3000/api/v1/repos/a.b/c-d/releases\
             ?limit=50&draft=false&pre-release=false"
        );

        // Whatever follows the repository in the configured URL is dropped
        let repository =
            parse_repository_url("https://codeberg.org/o/r/releases?x=1#../../admin").unwrap();
        assert_eq!(
            repository.releases_url(),
            "https://codeberg.org/api/v1/repos/o/r/releases\
             ?limit=50&draft=false&pre-release=false"
        );
    }

    fn asset(url: Option<&str>, kind: &str) -> ReleaseAsset {
        serde_json::from_value(serde_json::json!({
            "id": 1,
            "name": "tool",
            "size": 0,
            "type": kind,
            "browser_download_url": url,
        }))
        .unwrap()
    }

    #[test]
    fn uses_https_browser_download_url() {
        let origin = "https://codeberg.org";
        let url = "https://codeberg.org/owner/repo/releases/download/v1.2.3/tool";
        assert_eq!(
            asset_url(&asset(Some(url), "attachment"), origin).unwrap(),
            url
        );

        // External assets may be on other hosts
        let url = "https://example.org/tool";
        assert_eq!(
            asset_url(&asset(Some(url), "external"), origin).unwrap(),
            url
        );

        for url in [
            None,
            Some(""),
            Some("http://example.org/tool"),
            Some("/tool"),
        ] {
            assert!(
                matches!(
                    asset_url(&asset(url, "external"), origin),
                    Err(Error::InvalidDownloadUrl { .. })
                ),
                "accepted {url:?}"
            );
        }
    }

    #[test]
    fn rejects_download_url_with_control_characters() {
        // E.g. terminal escape sequences, which would be logged
        for url in [
            "https://example.org/\x1b[2Jtool",
            "https://example.org/tool\n",
            "https://example.org/my tool",
            "https://example.org/tööl",
            "https://example.org/\u{202e}loot",
        ] {
            let error =
                asset_url(&asset(Some(url), "external"), "https://codeberg.org").expect_err(url);
            // Neither are they printed as is in the error message
            let message = error.to_string();
            assert!(
                message.chars().all(|c| !c.is_control() && c != '\u{202e}'),
                "{message:?}"
            );
        }
    }

    /// The configuration derives the repository type from the host with its
    /// own parser, so the backend must never contact a different host for a
    /// URL that was recognized as, e.g., Codeberg.
    #[test]
    fn talks_to_the_host_the_type_was_derived_from() {
        for url in [
            "https://codeberg.org/owner/repo",
            "https://CODEBERG.org:443/owner/repo",
            "codeberg.org/owner/repo",
            "https://evil.org@codeberg.org/owner/repo",
            "https://evil.org:x@codeberg.org/owner/repo",
            "https://code.forgejo.org/owner/repo",
            "https://evil.org@code.forgejo.org/owner/repo",
        ] {
            let config = config_for(url, "tool", "");
            assert_eq!(
                config.repository.ty,
                crate::config::RepositoryType::Forgejo,
                "{url}"
            );
            if let Some(repository) = parse_repository_url(url) {
                let host = repository.origin.trim_start_matches("https://");
                let host = host.split(':').next().unwrap();
                assert!(
                    crate::config::FORGEJO_HOSTS.contains(&host),
                    "{url} would contact {}",
                    repository.origin
                );
            }
        }
    }

    #[test]
    fn rejects_http_instance() {
        let config = config_for("http://codeberg.org/owner/repo", "tool", "");
        let dir = test_dir("forgejo-http");
        assert!(matches!(
            Forgejo::download(&config, None, &dir),
            Err(Error::InvalidUrl { .. })
        ));
        std::fs::remove_dir_all(&dir).unwrap();
    }

    /// Downloads the newest Forgejo runner (a plain binary of about 20 MiB)
    /// from code.forgejo.org for real; run with `cargo test -- --ignored`.
    #[test]
    #[ignore = "needs network access to code.forgejo.org"]
    fn downloads_forgejo_runner() {
        let config = config_for(
            "https://code.forgejo.org/forgejo/runner",
            "forgejo-runner-${major}.${minor}.${patch}-linux-amd64",
            "",
        );
        let dir = test_dir("forgejo-runner");

        let result = Forgejo::download(&config, None, &dir);
        let size = result
            .as_ref()
            .ok()
            .and_then(Option::as_ref)
            .map(|download| std::fs::metadata(&download.target).unwrap().len());

        // Nothing is downloaded if the newest version is installed
        let latest = result
            .as_ref()
            .ok()
            .and_then(Option::as_ref)
            .map(|download| download.version.clone());
        let again = latest
            .as_ref()
            .map(|latest| Forgejo::download(&config, Some(latest), &dir));
        std::fs::remove_dir_all(&dir).unwrap();

        let download = result.unwrap().unwrap();
        assert!(
            download
                .target
                .ends_with(format!("forgejo-runner-{}-linux-amd64", download.version))
        );
        assert!(size.unwrap() > 1024 * 1024, "unexpectedly small: {size:?}");
        assert!(again.unwrap().unwrap().is_none());
    }
}

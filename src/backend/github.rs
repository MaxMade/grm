//! [`Backend`] for repositories hosted on GitHub, using its REST API.
//!
//! The newest release is determined from the most recent releases
//! (`GET /repos/{owner}/{repo}/releases`, see [`release`] for how), and its
//! asset is downloaded through the API
//! (`GET /repos/{owner}/{repo}/releases/assets/{id}`), which redirects to
//! the actual file.
//!
//! Without authentication, GitHub allows 60 API requests per hour and IP
//! address. If `GITHUB_TOKEN` is set, it is sent with every API request,
//! which raises the limit and gives access to private repositories.
//!
//! # Security
//!
//! See [`release`] for the protections shared with other backends. The token
//! is only sent to [`API_URL`]: request URLs are built from the validated
//! owner and repository and from asset IDs, never taken from responses, and
//! the token is not forwarded on redirects.

use std::path::Path;

use super::{
    download::{Backend, Download, Version},
    release::{self, Client, Error, is_valid_name},
};
use crate::config::Config;

/// Base URL of the GitHub REST API.
const API_URL: &str = "https://api.github.com";

/// Version of the GitHub REST API this backend is written against.
const API_VERSION: &str = "2022-11-28";

/// Number of most recent releases considered, which is the maximum the API
/// returns per request. Older releases are ignored, so a newer version must
/// not be released more than this many releases before the current one.
const RELEASES_PER_PAGE: u32 = 100;

/// Expected form of repository URLs, for error messages.
const EXPECTED_URL: &str = "GitHub (https://github.com/<owner>/<repo>)";

/// Releases hosted on GitHub.
pub struct Github;

impl Backend for Github {
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
        let (owner, repo) =
            parse_repository_url(&config.repository.url).ok_or_else(|| Error::InvalidUrl {
                url: config.repository.url.clone(),
                expected: EXPECTED_URL,
            })?;

        release::download_newest(
            &client(),
            &format!("{owner}/{repo}"),
            &format!("{API_URL}/repos/{owner}/{repo}/releases?per_page={RELEASES_PER_PAGE}"),
            config,
            installed,
            tmp_dir.as_ref(),
            |asset| {
                Ok(format!(
                    "{API_URL}/repos/{owner}/{repo}/releases/assets/{}",
                    asset.id
                ))
            },
        )
    }
}

/// Creates a client with the headers the API expects, including the token
/// from `GITHUB_TOKEN` if set.
fn client() -> Client {
    let mut headers = vec![("X-GitHub-Api-Version", API_VERSION.to_owned())];
    if let Some(token) = std::env::var("GITHUB_TOKEN")
        .ok()
        .filter(|token| !token.is_empty())
    {
        log::debug!("Authenticating to the GitHub API with $GITHUB_TOKEN");
        headers.push(("Authorization", format!("Bearer {token}")));
    }
    Client::new(
        headers,
        "application/vnd.github+json",
        // The asset endpoint only serves the file with this type
        "application/octet-stream",
    )
}

/// Extracts owner and repository from a GitHub URL such as
/// `https://github.com/<owner>/<repo>/releases`. The scheme, a trailing
/// `.git` and anything after the repository are optional. Owner and
/// repository must be valid names (see [`is_valid_name`]).
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::backend::release::tests::{config_for, test_dir};

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
    fn rejects_url_of_other_host() {
        let config = config_for("https://codeberg.org/owner/repo", "tool", "");
        let dir = test_dir("github-host");
        assert!(matches!(
            Github::download(&config, None, &dir),
            Err(Error::InvalidUrl { .. })
        ));
        std::fs::remove_dir_all(&dir).unwrap();
    }

    /// Downloads the newest lego release for real; run with
    /// `cargo test -- --ignored`.
    #[test]
    #[ignore = "needs network access to GitHub"]
    fn downloads_lego() {
        let config = config_for(
            "https://github.com/go-acme/lego/releases",
            "lego_v${major}.${minor}.${patch}_linux_amd64.tar.gz",
            "unpack = 'lego'",
        );
        let dir = test_dir("lego");

        let result = Github::download(&config, None, &dir);
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
            .map(|latest| Github::download(&config, Some(latest), &dir));
        std::fs::remove_dir_all(&dir).unwrap();

        // The extracted binary, not the archive
        let download = result.unwrap().unwrap();
        assert!(download.target.ends_with("lego"));
        assert!(size.unwrap() > 1024 * 1024, "unexpectedly small: {size:?}");
        assert!(again.unwrap().unwrap().is_none());
    }
}

//! Downloading releases from hosting services.
//!
//! [`download`] defines the [`Backend`](download::Backend) interface, which
//! is implemented for each [`RepositoryType`](crate::config::RepositoryType)
//! (currently only [`github`]). [`unpack`] extracts the file to install from
//! downloaded archives.

pub mod download;
pub mod github;
pub mod unpack;

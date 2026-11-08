//! Downloading releases from hosting services.
//!
//! [`download`] defines the [`Backend`](download::Backend) interface, which
//! is implemented for each [`RepositoryType`](crate::config::RepositoryType):
//! [`github`] and [`forgejo`], which share their logic in [`release`].
//! [`unpack`] extracts the file to install from downloaded archives.

pub mod download;
pub mod forgejo;
pub mod github;
pub mod release;
pub mod unpack;

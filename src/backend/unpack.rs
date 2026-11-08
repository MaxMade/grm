//! Extraction of a single file from a downloaded archive (see
//! [`Unpack`](crate::config::Unpack)).
//!
//! # Security
//!
//! Archives come from the internet, so only the one requested file is
//! extracted: entry names are only compared, never used as paths, and the
//! extracted file is named after the configured path. Symbolic links, hard
//! links and directories are rejected, the extracted file is created without
//! following symbolic links in the destination directory, and its size is
//! limited to [`MAX_UNPACKED_SIZE`] to defuse decompression bombs.

use std::{
    fmt::{self, Display},
    fs::File,
    io::{self, BufReader, Read, Write},
    path::{Component, Path, PathBuf},
};

use flate2::read::GzDecoder;
use zip::{ZipArchive, result::ZipError};

use super::download;
use crate::config::ArchiveFormat;

/// Maximum size of an extracted file. Far larger than any reasonable
/// executable, but small enough that a malicious archive (a "decompression
/// bomb") cannot fill the disk.
pub const MAX_UNPACKED_SIZE: u64 = 1024 * 1024 * 1024;

/// Extracts `file`, a path inside `archive`, into `dest_dir` and returns the
/// path of the extracted file, which is named after the final component of
/// `file`. An existing file is replaced (see [`download::create_file`]); a
/// partially written one is removed on failure.
///
/// Leading `./` components are ignored when comparing paths, so `tool` and
/// `./tool` are the same file. Only a regular file can be extracted, not a
/// directory or link. If the archive contains several entries for `file`,
/// the first one is extracted.
///
/// # Errors
///
/// Fails if the archive cannot be read, does not contain `file` as a regular
/// file, the file is larger than [`MAX_UNPACKED_SIZE`], or it cannot be
/// written.
pub fn unpack(
    format: ArchiveFormat,
    archive: &Path,
    file: &str,
    dest_dir: &Path,
) -> Result<PathBuf, Error> {
    unpack_with_limit(format, archive, file, dest_dir, MAX_UNPACKED_SIZE)
}

/// [`unpack`] with a configurable size limit for tests.
fn unpack_with_limit(
    format: ArchiveFormat,
    archive: &Path,
    file: &str,
    dest_dir: &Path,
    limit: u64,
) -> Result<PathBuf, Error> {
    let file_path = Path::new(file);
    let target = file_path
        .file_name()
        .map(|name| dest_dir.join(name))
        // The archive must not be replaced while it is being read
        .filter(|target| target != archive)
        .ok_or_else(|| Error::InvalidFile(file.to_owned()))?;

    let read_error = |source| Error::Read {
        path: archive.to_path_buf(),
        source,
    };
    let reader = BufReader::new(File::open(archive).map_err(read_error)?);

    let found = match format {
        ArchiveFormat::TarGz => {
            unpack_tar_gz(reader, file_path, &target, limit).map_err(read_error)?
        }
        ArchiveFormat::Zip => {
            unpack_zip(reader, file_path, &target, limit).map_err(|source| match source {
                ZipError::Io(source) => read_error(source),
                source => Error::Zip {
                    path: archive.to_path_buf(),
                    source,
                },
            })?
        }
    };

    let missing = |file: &str| (archive.to_path_buf(), file.to_owned());
    match found {
        Found::File(Ok(())) => Ok(target),
        Found::File(Err(CopyError::Read(source))) => Err(read_error(source)),
        Found::File(Err(CopyError::Write(source))) => Err(Error::Write {
            path: target,
            source,
        }),
        Found::File(Err(CopyError::TooLarge)) => {
            let (archive, file) = missing(file);
            Err(Error::TooLarge {
                archive,
                file,
                limit,
            })
        }
        Found::NotAFile => {
            let (archive, file) = missing(file);
            Err(Error::NotAFile { archive, file })
        }
        Found::Missing => {
            let (archive, file) = missing(file);
            Err(Error::MissingFile { archive, file })
        }
    }
}

/// Outcome of looking for the file in an archive.
enum Found {
    File(Result<(), CopyError>),
    NotAFile,
    Missing,
}

/// Errors while copying the file out of the archive, which distinguish
/// reading the archive from writing the extracted file.
enum CopyError {
    Read(io::Error),
    Write(io::Error),
    TooLarge,
}

fn unpack_tar_gz(reader: impl Read, file: &Path, target: &Path, limit: u64) -> io::Result<Found> {
    let mut archive = tar::Archive::new(GzDecoder::new(reader));
    for entry in archive.entries()? {
        let mut entry = entry?;
        if !same_path(&entry.path()?, file) {
            continue;
        }
        if !entry.header().entry_type().is_file() {
            return Ok(Found::NotAFile);
        }
        return Ok(Found::File(extract(&mut entry, target, limit)));
    }
    Ok(Found::Missing)
}

fn unpack_zip(
    reader: impl Read + io::Seek,
    file: &Path,
    target: &Path,
    limit: u64,
) -> Result<Found, ZipError> {
    let mut archive = ZipArchive::new(reader)?;
    for index in 0..archive.len() {
        let mut entry = archive.by_index(index)?;
        if !same_path(Path::new(&*entry.name()?), file) {
            continue;
        }
        if !entry.is_file() {
            return Ok(Found::NotAFile);
        }
        return Ok(Found::File(extract(&mut entry, target, limit)));
    }
    Ok(Found::Missing)
}

/// Whether `a` and `b` are the same path, ignoring `.` components (e.g. a
/// leading `./`) and trailing slashes.
fn same_path(a: &Path, b: &Path) -> bool {
    fn normalize(path: &Path) -> impl Iterator<Item = Component<'_>> {
        path.components()
            .filter(|component| *component != Component::CurDir)
    }
    normalize(a).eq(normalize(b))
}

/// Writes at most `limit` bytes of `reader` to `path`, removing the file
/// again on failure.
fn extract(reader: &mut impl Read, path: &Path, limit: u64) -> Result<(), CopyError> {
    let result = download::create_file(path)
        .map_err(CopyError::Write)
        .and_then(|mut file| {
            copy_limited(reader, &mut file, limit)?;
            file.sync_all().map_err(CopyError::Write)
        });
    if result.is_err() {
        let _ = std::fs::remove_file(path);
    }
    result
}

/// Copies `reader` to `writer`, failing as soon as more than `limit` bytes
/// have been read, i.e. before writing them.
fn copy_limited(
    reader: &mut impl Read,
    writer: &mut impl Write,
    limit: u64,
) -> Result<(), CopyError> {
    let mut buffer = vec![0; 64 * 1024];
    let mut total = 0u64;
    loop {
        let read = match reader.read(&mut buffer) {
            Ok(0) => return Ok(()),
            Ok(read) => read,
            Err(error) if error.kind() == io::ErrorKind::Interrupted => continue,
            Err(error) => return Err(CopyError::Read(error)),
        };
        total += read as u64;
        if total > limit {
            return Err(CopyError::TooLarge);
        }
        writer
            .write_all(&buffer[..read])
            .map_err(CopyError::Write)?;
    }
}

/// Errors while extracting a file from an archive.
#[derive(Debug)]
pub enum Error {
    /// The file to extract has no file name or would overwrite the archive.
    InvalidFile(String),

    /// The archive could not be read or decompressed.
    Read { path: PathBuf, source: io::Error },

    /// The archive is not a valid ZIP archive.
    Zip { path: PathBuf, source: ZipError },

    /// The archive does not contain the file.
    MissingFile { archive: PathBuf, file: String },

    /// The file in the archive is a directory, link or other special file.
    NotAFile { archive: PathBuf, file: String },

    /// The file in the archive is larger than the limit in bytes.
    TooLarge {
        archive: PathBuf,
        file: String,
        limit: u64,
    },

    /// The extracted file could not be written.
    Write { path: PathBuf, source: io::Error },
}

impl Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::InvalidFile(file) => write!(f, "unable to extract \"{file}\""),
            Self::Read { path, source } => {
                write!(f, "unable to read archive {}: {source}", path.display())
            }
            Self::Zip { path, source } => {
                write!(f, "invalid ZIP archive {}: {source}", path.display())
            }
            Self::MissingFile { archive, file } => {
                write!(
                    f,
                    "archive {} does not contain \"{file}\"",
                    archive.display()
                )
            }
            Self::NotAFile { archive, file } => write!(
                f,
                "\"{file}\" in archive {} is not a regular file",
                archive.display()
            ),
            Self::TooLarge {
                archive,
                file,
                limit,
            } => write!(
                f,
                "\"{file}\" in archive {} is larger than {limit} bytes",
                archive.display()
            ),
            Self::Write { path, source } => {
                write!(f, "unable to write {}: {source}", path.display())
            }
        }
    }
}

impl std::error::Error for Error {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Read { source, .. } | Self::Write { source, .. } => Some(source),
            Self::Zip { source, .. } => Some(source),
            Self::InvalidFile(_)
            | Self::MissingFile { .. }
            | Self::NotAFile { .. }
            | Self::TooLarge { .. } => None,
        }
    }
}

#[cfg(test)]
mod tests {
    use std::io::Write;

    use flate2::{Compression, write::GzEncoder};
    use zip::{ZipWriter, write::SimpleFileOptions};

    use super::*;

    /// A directory that is unique per test and removed afterwards.
    struct TestDir(PathBuf);

    impl TestDir {
        fn new(name: &str) -> Self {
            let dir =
                std::env::temp_dir().join(format!("grm-unpack-test-{}-{name}", std::process::id()));
            let _ = std::fs::remove_dir_all(&dir);
            std::fs::create_dir_all(&dir).unwrap();
            Self(dir)
        }
    }

    impl Drop for TestDir {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    /// Creates `archive.tar.gz` with a README, the directory `./tool-1`
    /// containing the file `bin/tool`, and a symlink `link`.
    fn tar_gz(dir: &TestDir) -> PathBuf {
        let path = dir.0.join("archive.tar.gz");
        let gz = GzEncoder::new(File::create(&path).unwrap(), Compression::fast());
        let mut builder = tar::Builder::new(gz);

        let mut add = |name: &str, kind: tar::EntryType, data: &[u8]| {
            let mut header = tar::Header::new_gnu();
            header.set_entry_type(kind);
            header.set_size(data.len() as u64);
            header.set_mode(0o755);
            builder.append_data(&mut header, name, data).unwrap();
        };
        add("README.md", tar::EntryType::Regular, b"readme");
        add("./tool-1/", tar::EntryType::Directory, b"");
        add("./tool-1/bin/tool", tar::EntryType::Regular, b"binary");

        let mut header = tar::Header::new_gnu();
        header.set_entry_type(tar::EntryType::Symlink);
        header.set_size(0);
        builder
            .append_link(&mut header, "link", "tool-1/bin/tool")
            .unwrap();

        builder.into_inner().unwrap().finish().unwrap();
        path
    }

    /// Creates `archive.zip` with the same entries as [`tar_gz`].
    fn zip(dir: &TestDir) -> PathBuf {
        let path = dir.0.join("archive.zip");
        let mut zip = ZipWriter::new(File::create(&path).unwrap());
        let options = SimpleFileOptions::default();
        zip.start_file("README.md", options).unwrap();
        zip.write_all(b"readme").unwrap();
        zip.add_directory("tool-1/", options).unwrap();
        zip.start_file("tool-1/bin/tool", options).unwrap();
        zip.write_all(b"binary").unwrap();
        zip.add_symlink("link", "tool-1/bin/tool", options).unwrap();
        zip.finish().unwrap();
        path
    }

    fn check_archive(format: ArchiveFormat, create: fn(&TestDir) -> PathBuf) {
        let dir = TestDir::new(&format!("{format:?}"));
        let archive = create(&dir);
        let out = dir.0.join("out");
        std::fs::create_dir(&out).unwrap();

        // Nested file, with and without `./`
        for file in ["tool-1/bin/tool", "./tool-1/bin/tool"] {
            let target = unpack(format, &archive, file, &out).unwrap();
            assert_eq!(target, out.join("tool"));
            assert_eq!(std::fs::read(&target).unwrap(), b"binary");
        }

        // File at the top level
        let target = unpack(format, &archive, "README.md", &out).unwrap();
        assert_eq!(std::fs::read(target).unwrap(), b"readme");

        assert!(matches!(
            unpack(format, &archive, "tool", &out),
            Err(Error::MissingFile { .. })
        ));
        for file in ["tool-1", "link"] {
            assert!(
                matches!(
                    unpack(format, &archive, file, &out),
                    Err(Error::NotAFile { .. })
                ),
                "{file}"
            );
        }
        assert!(!out.join("tool-1").exists() && !out.join("link").exists());
    }

    #[test]
    fn unpacks_tar_gz() {
        check_archive(ArchiveFormat::TarGz, tar_gz);
    }

    #[test]
    fn unpacks_zip() {
        check_archive(ArchiveFormat::Zip, zip);
    }

    #[test]
    fn rejects_wrong_format() {
        let dir = TestDir::new("format");
        let archive = zip(&dir);
        assert!(matches!(
            unpack(ArchiveFormat::TarGz, &archive, "README.md", &dir.0),
            Err(Error::Read { .. })
        ));

        let archive = tar_gz(&dir);
        assert!(matches!(
            unpack(ArchiveFormat::Zip, &archive, "README.md", &dir.0),
            Err(Error::Zip { .. })
        ));
    }

    #[test]
    fn enforces_size_limit() {
        for (format, create) in [
            (ArchiveFormat::TarGz, tar_gz as fn(&TestDir) -> PathBuf),
            (ArchiveFormat::Zip, zip),
        ] {
            let dir = TestDir::new(&format!("limit-{format:?}"));
            let archive = create(&dir);
            let file = "tool-1/bin/tool";

            // "binary" has 6 bytes
            let target = unpack_with_limit(format, &archive, file, &dir.0, 6).unwrap();
            assert_eq!(std::fs::read(&target).unwrap(), b"binary");

            let result = unpack_with_limit(format, &archive, file, &dir.0, 5);
            assert!(
                matches!(result, Err(Error::TooLarge { limit: 5, .. })),
                "{format:?}: {result:?}"
            );
            assert!(!target.exists(), "partial file not removed");
        }
    }

    #[test]
    fn does_not_follow_symlink_in_dest_dir() {
        let dir = TestDir::new("symlink");
        let archive = tar_gz(&dir);
        let victim = dir.0.join("victim");
        std::fs::write(&victim, "keep").unwrap();
        let out = dir.0.join("out");
        std::fs::create_dir(&out).unwrap();
        std::os::unix::fs::symlink(&victim, out.join("tool")).unwrap();

        let target = unpack(ArchiveFormat::TarGz, &archive, "tool-1/bin/tool", &out).unwrap();
        assert_eq!(std::fs::read_to_string(&victim).unwrap(), "keep");
        assert!(!target.is_symlink());
        assert_eq!(std::fs::read(&target).unwrap(), b"binary");
    }

    /// Appends a regular file to `builder` under `name` as is; unlike
    /// `append_data`, this allows names with `..` like a malicious archive.
    fn append_raw<W: Write>(builder: &mut tar::Builder<W>, name: &str, data: &[u8]) {
        let mut header = tar::Header::new_gnu();
        header.as_gnu_mut().unwrap().name[..name.len()].copy_from_slice(name.as_bytes());
        header.set_entry_type(tar::EntryType::Regular);
        header.set_size(data.len() as u64);
        header.set_mode(0o644);
        header.set_cksum();
        builder.append(&header, data).unwrap();
    }

    #[test]
    fn ignores_malicious_entries() {
        let dir = TestDir::new("malicious");
        let path = dir.0.join("archive.tar.gz");
        let gz = GzEncoder::new(File::create(&path).unwrap(), Compression::fast());
        let mut builder = tar::Builder::new(gz);
        append_raw(&mut builder, "../evil", b"evil");
        append_raw(&mut builder, "/abs/evil", b"evil");
        append_raw(&mut builder, "tool", b"first");
        append_raw(&mut builder, "./tool", b"second");
        let mut header = tar::Header::new_gnu();
        header.set_entry_type(tar::EntryType::Link);
        header.set_size(0);
        builder.append_link(&mut header, "hard", "tool").unwrap();
        builder.into_inner().unwrap().finish().unwrap();

        let out = dir.0.join("out");
        std::fs::create_dir(&out).unwrap();
        let format = ArchiveFormat::TarGz;

        // Entry names are only compared, so `../evil` is not `evil`, and
        // nothing is written outside of `out`
        assert!(matches!(
            unpack(format, &path, "evil", &out),
            Err(Error::MissingFile { .. })
        ));
        assert!(!dir.0.join("evil").exists() && !Path::new("/abs/evil").exists());

        // Hard links are not regular files
        assert!(matches!(
            unpack(format, &path, "hard", &out),
            Err(Error::NotAFile { .. })
        ));

        // The first of duplicate entries wins
        let target = unpack(format, &path, "tool", &out).unwrap();
        assert_eq!(std::fs::read(target).unwrap(), b"first");
    }

    #[test]
    fn reports_truncated_archive_as_read_error() {
        let dir = TestDir::new("truncated");
        let path = dir.0.join("archive.tar.gz");
        let gz = GzEncoder::new(File::create(&path).unwrap(), Compression::fast());
        let mut builder = tar::Builder::new(gz);
        // Incompressible data, so truncating the archive cuts the entry
        let mut state = 1u32;
        let data: Vec<u8> = (0..256 * 1024)
            .map(|_| {
                state = state.wrapping_mul(1_103_515_245).wrapping_add(12_345);
                (state >> 16) as u8
            })
            .collect();
        append_raw(&mut builder, "tool", &data);
        builder.into_inner().unwrap().finish().unwrap();

        let len = std::fs::metadata(&path).unwrap().len();
        File::options()
            .write(true)
            .open(&path)
            .unwrap()
            .set_len(len / 2)
            .unwrap();

        let out = dir.0.join("out");
        std::fs::create_dir(&out).unwrap();
        let result = unpack(ArchiveFormat::TarGz, &path, "tool", &out);
        assert!(matches!(result, Err(Error::Read { .. })), "{result:?}");
        assert!(!out.join("tool").exists());
    }

    #[test]
    fn does_not_overwrite_archive() {
        let dir = TestDir::new("overwrite");
        let archive = tar_gz(&dir);
        assert!(matches!(
            unpack(ArchiveFormat::TarGz, &archive, "archive.tar.gz", &dir.0),
            Err(Error::InvalidFile(_))
        ));
    }
}

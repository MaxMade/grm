//! Installed packages and the operations of the command-line interface.
//!
//! A package is described by one [`Config`] and named after its installed
//! binary ([`Installation::path`]). Its installed version is recorded in
//! `<data_dir>/<name>` as plain text, e.g. `1.10.0`; a missing record means
//! that the package is not installed.
//!
//! Every operation on a package holds its [`Lock`], so concurrent `grm`
//! processes never modify the same package at the same time. Package names
//! must be unique, since they name the record (and, by default, the lock);
//! packages sharing a name are rejected by every operation.
//!
//! # Safety of installed files
//!
//! - `grm` only replaces or removes a binary it installed itself, i.e. one
//!   with a record. Installing over an existing file without a record (e.g.
//!   because `name` and `dir` accidentally point to a system binary) fails.
//! - Binaries and records are replaced atomically (see [`replace_file`]),
//!   never written through symbolic links, and synced to disk before the
//!   record claims a new version.
//!
//! [`Installation::path`]: crate::config::Installation::path

use std::{
    collections::HashSet,
    fmt::{self, Display},
    fs,
    io::{self, Error as IOError, ErrorKind, Write},
    os::unix::fs::PermissionsExt,
    path::{Path, PathBuf},
};

use tempfile::TempDir;

use crate::{
    backend::{
        download::{self, Backend, Version},
        github::{self, Github},
    },
    config::{Config, RepositoryType},
    lock::{Lock, LockFile, LockGuard},
};

/// All configured packages, and a private temporary directory to download
/// them to.
pub struct State {
    packages: Vec<Package>,
    /// Names used by more than one package, which are rejected.
    duplicates: HashSet<String>,
    /// Created with `mkdtemp`, so it is only accessible by the current user
    /// (see [`Backend::download`]). Removed when the state is dropped.
    tmp_dir: TempDir,
}

/// Errors of the operations on packages.
#[derive(Debug)]
pub enum Error {
    /// A file or directory could not be accessed.
    Io {
        action: &'static str,
        path: PathBuf,
        source: IOError,
    },

    /// The record of the installed version cannot be parsed.
    InvalidRecord(PathBuf),

    /// No configuration has this package name.
    UnknownPackage(String),

    /// Several configurations have this package name.
    AmbiguousPackage(String),

    /// A file that `grm` did not install exists where the package would be
    /// installed.
    Unmanaged(PathBuf),

    /// The newest release could not be downloaded from GitHub.
    Github(github::Error),
}

/// A configured package.
struct Package {
    /// Name of the package (see [`package_name`]), known without locking.
    name: String,
    /// Path of the lock file, for error messages.
    lock_path: PathBuf,
    config: Lock<Config>,
}

impl Package {
    fn new(config: Config) -> Self {
        Self {
            name: package_name(&config),
            lock_path: LockFile::path(&config),
            config: Lock::new(config),
        }
    }

    /// Locks the package's configuration (see [`Lock::lock`]).
    fn lock(&mut self) -> Result<LockGuard<'_, Config>, Error> {
        self.config
            .lock()
            .map_err(Error::io("lock", &self.lock_path))
    }
}

impl Error {
    fn io(action: &'static str, path: &Path) -> impl FnOnce(IOError) -> Self {
        let path = path.to_path_buf();
        move |source| Self::Io {
            action,
            path,
            source,
        }
    }
}

impl Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Io {
                action,
                path,
                source,
            } => write!(f, "unable to {action} {}: {source}", path.display()),
            Self::InvalidRecord(path) => {
                write!(f, "invalid installed version in {}", path.display())
            }
            Self::UnknownPackage(name) => write!(f, "no package named \"{name}\""),
            Self::AmbiguousPackage(name) => write!(
                f,
                "several configurations install a package named \"{name}\""
            ),
            Self::Unmanaged(path) => write!(
                f,
                "{} exists, but was not installed by grm; remove it first to install the package",
                path.display()
            ),
            Self::Github(error) => error.fmt(f),
        }
    }
}

impl std::error::Error for Error {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Io { source, .. } => Some(source),
            // Displayed as this error, so the source is the inner error's
            Self::Github(error) => std::error::Error::source(error),
            Self::InvalidRecord(_)
            | Self::UnknownPackage(_)
            | Self::AmbiguousPackage(_)
            | Self::Unmanaged(_) => None,
        }
    }
}

impl From<github::Error> for Error {
    fn from(error: github::Error) -> Self {
        Self::Github(error)
    }
}

impl State {
    /// Manages the packages described by `configs`.
    ///
    /// # Errors
    ///
    /// Fails if the temporary directory cannot be created.
    pub fn new<Iter: Iterator<Item = Config>>(configs: Iter) -> Result<Self, Error> {
        let tmp_dir = tempfile::Builder::new()
            .prefix("grm-")
            .tempdir()
            .map_err(Error::io("create", &std::env::temp_dir()))?;

        let packages: Vec<Package> = configs.map(Package::new).collect();
        let mut names = HashSet::new();
        let duplicates = packages
            .iter()
            .filter(|package| !names.insert(&package.name))
            .map(|package| package.name.clone())
            .collect();

        Ok(Self {
            packages,
            duplicates,
            tmp_dir,
        })
    }

    /// Prints every package with its installed version and repository, in
    /// the order of the configurations. Returns whether all packages could be
    /// listed; the others are reported as errors.
    pub fn list(&mut self) -> bool {
        let mut success = true;
        let mut rows = Vec::new();
        for package in &mut self.packages {
            if self.duplicates.contains(&package.name) {
                log::error!("{}", Error::AmbiguousPackage(package.name.clone()));
                success = false;
                continue;
            }
            let result = package.lock().and_then(|config| {
                Ok((installed_version(&config)?, config.repository.url.clone()))
            });
            match result {
                Ok((version, url)) => rows.push((
                    package.name.clone(),
                    version.map_or_else(|| "not installed".to_owned(), |v| v.to_string()),
                    url,
                )),
                Err(error) => {
                    log::error!("{}: {error}", package.name);
                    success = false;
                }
            }
        }

        let name_width = rows.iter().map(|(name, ..)| name.len()).max().unwrap_or(0);
        let version_width = rows.iter().map(|(_, v, _)| v.len()).max().unwrap_or(0);
        for (name, version, url) in rows {
            println!("{name:name_width$}  {version:version_width$}  {url}");
        }
        success
    }

    /// Installs the newest release of the package `name` if it is not
    /// installed yet; an installed package is left as is (see
    /// [`State::update`]).
    ///
    /// # Errors
    ///
    /// Fails if no single configuration has the package name, a file that
    /// `grm` did not install exists at [`Installation::path`] (even a broken
    /// symbolic link), or the package cannot be installed.
    ///
    /// [`Installation::path`]: crate::config::Installation::path
    pub fn install(&mut self, name: &str) -> Result<(), Error> {
        let config = find(&mut self.packages, name)?.lock()?;
        if let Some(version) = installed_version(&config)? {
            println!("{name} {version} is already installed; use `grm update` to update it");
            return Ok(());
        }

        // Checked before downloading, so that nothing is fetched in vain.
        // Holding the lock does not keep other programs from creating the
        // file meanwhile, but this only guards against mistakes in the
        // configuration, not against attacks.
        let path = &config.installation.path;
        match fs::symlink_metadata(path) {
            Ok(_) => return Err(Error::Unmanaged(path.clone())),
            Err(error) if error.kind() != ErrorKind::NotFound => {
                return Err(Error::io("access", path)(error));
            }
            Err(_) => {}
        }

        let version = install_newest(&config, None, self.tmp_dir.path())?
            .expect("a release is always downloaded if none is installed");
        println!("Installed {name} {version}");
        Ok(())
    }

    /// Removes the installed binary of the package `name` and the record of
    /// its version; a package that is not installed is left as is.
    ///
    /// The binary is removed first, so the package still counts as installed
    /// if that fails. A binary that is already missing (e.g. after a removal
    /// that failed to remove the record) is not an error, so removing can
    /// always be retried. Whatever is at [`Installation::path`] is removed;
    /// a symbolic link is removed itself, not its target.
    ///
    /// # Errors
    ///
    /// Fails if no single configuration has the package name, or the binary
    /// or record cannot be removed.
    ///
    /// [`Installation::path`]: crate::config::Installation::path
    pub fn remove(&mut self, name: &str) -> Result<(), Error> {
        let config = find(&mut self.packages, name)?.lock()?;
        let Some(version) = installed_version(&config)? else {
            println!("{name} is not installed");
            return Ok(());
        };

        remove_if_exists(&config.installation.path)?;
        remove_if_exists(&record_path(&config))?;
        println!("Removed {name} {version}");
        Ok(())
    }

    /// Updates every installed package to its newest release; packages that
    /// are not installed are skipped. Returns whether all packages could be
    /// checked (and updated); the others are reported as errors.
    pub fn update(&mut self) -> bool {
        let mut success = true;
        for package in &mut self.packages {
            if self.duplicates.contains(&package.name) {
                log::error!("{}", Error::AmbiguousPackage(package.name.clone()));
                success = false;
                continue;
            }
            if let Err(error) = update_one(package, self.tmp_dir.path()) {
                log::error!("{}: {error}", package.name);
                success = false;
            }
        }
        success
    }
}

/// Returns the only package named `name` among `packages`.
fn find<'a>(packages: &'a mut [Package], name: &str) -> Result<&'a mut Package, Error> {
    let mut matches = packages.iter_mut().filter(|package| package.name == name);
    let package = matches
        .next()
        .ok_or_else(|| Error::UnknownPackage(name.to_owned()))?;
    if matches.next().is_some() {
        return Err(Error::AmbiguousPackage(name.to_owned()));
    }
    Ok(package)
}

/// Updates the package of `config` if it is installed (see
/// [`State::update`]).
fn update_one(package: &mut Package, tmp_dir: &Path) -> Result<(), Error> {
    let config = package.lock()?;
    let name = package_name(&config);

    let Some(installed) = installed_version(&config)? else {
        log::info!("{name} is not installed, skipping it");
        return Ok(());
    };

    match install_newest(&config, Some(&installed), tmp_dir)? {
        Some(version) => println!("Updated {name} from {installed} to {version}"),
        None => println!("{name} {installed} is up to date"),
    }
    Ok(())
}

/// Downloads the newest release of `config` if it is newer than `installed`
/// (or if nothing is installed), installs it and records its version.
/// Returns the installed version, or `None` if nothing was newer.
fn install_newest(
    config: &Config,
    installed: Option<&Version>,
    tmp_dir: &Path,
) -> Result<Option<Version>, Error> {
    // A directory per package, so that downloads of several packages cannot
    // collide, and each one's files are removed right after installing it
    let tmp_dir = tempfile::Builder::new()
        .prefix(&package_name(config))
        .tempdir_in(tmp_dir)
        .map_err(Error::io("create a directory in", tmp_dir))?;

    let download = match config.repository.ty {
        RepositoryType::Github => Github::download(config, installed, tmp_dir.path())?,
    };
    let Some(download) = download else {
        return Ok(None);
    };

    // Install the binary before recording it: if recording fails, the next
    // update installs the same version again instead of never updating it
    let path = &config.installation.path;
    let mode = config.installation.mode;
    replace_file(path, mode, |file| {
        io::copy(&mut fs::File::open(&download.target)?, file).map(|_| ())
    })?;

    let record = record_path(config);
    replace_file(&record, 0o644, |file| {
        writeln!(file, "{}", download.version)
    })?;

    Ok(Some(download.version))
}

/// Atomically replaces `path` with a file of permission bits `mode` whose
/// contents are written by `write`: they are written to a temporary file in
/// the same directory, which is then renamed to `path`. Running programs
/// keep their old binary, and an interrupted write never leaves a partial
/// file at `path`. Missing parent directories are created.
///
/// The file and the directory are synced to disk before returning, so after
/// a crash, a later call (e.g. writing the record) is never persisted
/// without this one. If `path` is a symbolic link, the link itself is
/// replaced, not its target. The permission bits are set explicitly, so the
/// umask does not apply.
fn replace_file(
    path: &Path,
    mode: u32,
    write: impl FnOnce(&mut fs::File) -> io::Result<()>,
) -> Result<(), Error> {
    let dir = path
        .parent()
        .expect("installed files have a parent directory");
    fs::create_dir_all(dir).map_err(Error::io("create", dir))?;

    let mut tmp_name = std::ffi::OsString::from(".");
    tmp_name.push(path.file_name().expect("installed files have a name"));
    tmp_name.push(".grm-new");
    let tmp = dir.join(tmp_name);

    // Created without following symbolic links, since `dir` may be writable
    // by others (see `download::create_file`)
    let result = download::create_file(&tmp)
        .and_then(|mut file| {
            write(&mut file)?;
            file.set_permissions(fs::Permissions::from_mode(mode))?;
            file.sync_all()
        })
        .map_err(Error::io("write", &tmp))
        .and_then(|()| fs::rename(&tmp, path).map_err(Error::io("replace", path)));

    if result.is_err() {
        let _ = fs::remove_file(&tmp);
        return result;
    }

    // The rename is only durable once the directory is synced
    fs::File::open(dir)
        .and_then(|dir| dir.sync_all())
        .map_err(Error::io("sync", dir))
}

/// Removes the file `path`; a missing file is not an error.
fn remove_if_exists(path: &Path) -> Result<(), Error> {
    match fs::remove_file(path) {
        Err(error) if error.kind() != ErrorKind::NotFound => Err(Error::io("remove", path)(error)),
        _ => Ok(()),
    }
}

/// Reads the installed version of `config`'s package, or `None` if it is
/// not installed.
fn installed_version(config: &Config) -> Result<Option<Version>, Error> {
    let path = record_path(config);
    match fs::read_to_string(&path) {
        Ok(record) => Version::parse(record.trim())
            .map(Some)
            .ok_or(Error::InvalidRecord(path)),
        Err(error) if error.kind() == ErrorKind::NotFound => Ok(None),
        Err(error) => Err(Error::io("read", &path)(error)),
    }
}

/// Name of `config`'s package, i.e. the file name of its binary.
fn package_name(config: &Config) -> String {
    config
        .installation
        .path
        .file_name()
        .expect("installation name is validated while loading")
        .to_string_lossy()
        .into_owned()
}

/// Path of the record of the installed version of `config`'s package.
fn record_path(config: &Config) -> PathBuf {
    config.installation.data_dir.join(package_name(config))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A directory that is unique per test and removed afterwards.
    struct TestDir(PathBuf);

    impl TestDir {
        fn new(name: &str) -> Self {
            let dir =
                std::env::temp_dir().join(format!("grm-state-test-{}-{name}", std::process::id()));
            let _ = fs::remove_dir_all(&dir);
            fs::create_dir_all(&dir).unwrap();
            Self(dir)
        }
    }

    impl Drop for TestDir {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    fn config(dir: &TestDir, name: &str) -> Config {
        let dir = dir.0.display();
        toml::from_str(&format!(
            r#"
            [repository]
            url = "https://github.com/owner/{name}"

            [installation]
            name = "{name}"
            dir = "{dir}/bin"
            mode = 0o750
            runtime_dir = "{dir}/run"
            data_dir = "{dir}/data"

            [asset]
            version = '(?<major>\d+)\.(?<minor>\d+)\.(?<patch>\d+)'
            target = '{name}'
            "#
        ))
        .unwrap()
    }

    #[test]
    fn reads_installed_version() {
        let dir = TestDir::new("record");
        let config = config(&dir, "tool");
        assert!(installed_version(&config).unwrap().is_none());

        let record = record_path(&config);
        assert_eq!(record, dir.0.join("data/tool"));
        fs::create_dir_all(record.parent().unwrap()).unwrap();
        fs::write(&record, "1.10.0\n").unwrap();
        assert_eq!(
            installed_version(&config).unwrap(),
            Version::parse("1.10.0")
        );

        fs::write(&record, "\n").unwrap();
        assert!(matches!(
            installed_version(&config),
            Err(Error::InvalidRecord(_))
        ));
    }

    #[test]
    fn replaces_file_atomically_with_mode() {
        let dir = TestDir::new("replace");
        let path = dir.0.join("bin/tool");

        // Missing directories are created
        replace_file(&path, 0o750, |file| file.write_all(b"old")).unwrap();
        assert_eq!(fs::read(&path).unwrap(), b"old");
        let mode = fs::metadata(&path).unwrap().permissions().mode();
        assert_eq!(mode & 0o7777, 0o750);

        // A file that is open (like a running binary) keeps its contents
        let mut open = fs::File::open(&path).unwrap();
        replace_file(&path, 0o755, |file| file.write_all(b"new")).unwrap();
        assert_eq!(fs::read(&path).unwrap(), b"new");
        let mut old = String::new();
        io::Read::read_to_string(&mut open, &mut old).unwrap();
        assert_eq!(old, "old");

        // A failed write leaves the old file and no temporary file behind
        let result = replace_file(&path, 0o755, |file| {
            file.write_all(b"partial")?;
            Err(io::Error::other("interrupted"))
        });
        assert!(result.is_err());
        assert_eq!(fs::read(&path).unwrap(), b"new");
        assert_eq!(fs::read_dir(dir.0.join("bin")).unwrap().count(), 1);
    }

    #[test]
    fn install_rejects_unknown_and_ambiguous_packages() {
        let dir = TestDir::new("names");
        let other = TestDir::new("names-other");
        let mut state =
            State::new([config(&dir, "a"), config(&dir, "b"), config(&other, "b")].into_iter())
                .unwrap();

        assert!(matches!(state.install("c"), Err(Error::UnknownPackage(_))));
        assert!(matches!(
            state.install("b"),
            Err(Error::AmbiguousPackage(_))
        ));
    }

    #[test]
    fn remove_rejects_unknown_and_ambiguous_packages() {
        let dir = TestDir::new("remove-names");
        let other = TestDir::new("remove-names-other");
        let mut state =
            State::new([config(&dir, "a"), config(&dir, "b"), config(&other, "b")].into_iter())
                .unwrap();

        assert!(matches!(state.remove("c"), Err(Error::UnknownPackage(_))));
        assert!(matches!(state.remove("b"), Err(Error::AmbiguousPackage(_))));
    }

    /// Creates `config`'s directories, and installs a fake binary and record
    /// if `installed`.
    fn prepare(config: &Config, installed: bool) {
        config.installation.create_dirs().unwrap();
        if installed {
            replace_file(&config.installation.path, 0o755, |f| f.write_all(b"bin")).unwrap();
            fs::write(record_path(config), "1.2.3\n").unwrap();
        }
    }

    #[test]
    fn removes_binary_and_record() {
        let dir = TestDir::new("remove");
        let config = config(&dir, "tool");
        prepare(&config, true);
        let (binary, record) = (config.installation.path.clone(), record_path(&config));
        // Another package must not be affected
        let other = super::tests::config(&dir, "other");
        prepare(&other, true);

        let mut state = State::new([config, other].into_iter()).unwrap();
        state.remove("tool").unwrap();
        assert!(!binary.exists() && !record.exists());
        assert!(dir.0.join("bin/other").exists() && dir.0.join("data/other").exists());

        // Removing again is a no-op
        state.remove("tool").unwrap();
    }

    #[test]
    fn remove_tolerates_missing_binary() {
        let dir = TestDir::new("remove-missing");
        let config = config(&dir, "tool");
        prepare(&config, true);
        fs::remove_file(&config.installation.path).unwrap();
        let record = record_path(&config);

        // E.g. after a removal that failed to remove the record
        let mut state = State::new([config].into_iter()).unwrap();
        state.remove("tool").unwrap();
        assert!(!record.exists());
    }

    #[test]
    fn remove_keeps_record_if_binary_cannot_be_removed() {
        let dir = TestDir::new("remove-fail");
        let config = config(&dir, "tool");
        prepare(&config, true);
        let record = record_path(&config);

        // A non-empty directory in place of the binary cannot be removed
        let binary = config.installation.path.clone();
        fs::remove_file(&binary).unwrap();
        fs::create_dir(&binary).unwrap();
        fs::write(binary.join("file"), "").unwrap();

        let mut state = State::new([config].into_iter()).unwrap();
        assert!(matches!(
            state.remove("tool"),
            Err(Error::Io {
                action: "remove",
                ..
            })
        ));
        assert!(record.exists(), "package must still count as installed");
    }

    #[test]
    fn remove_does_not_follow_symlink() {
        let dir = TestDir::new("remove-symlink");
        let config = config(&dir, "tool");
        prepare(&config, true);
        let target = dir.0.join("target");
        fs::write(&target, "keep").unwrap();
        let binary = config.installation.path.clone();
        fs::remove_file(&binary).unwrap();
        std::os::unix::fs::symlink(&target, &binary).unwrap();

        let mut state = State::new([config].into_iter()).unwrap();
        state.remove("tool").unwrap();
        assert!(!binary.is_symlink());
        assert_eq!(fs::read_to_string(&target).unwrap(), "keep");
    }

    #[test]
    fn install_skips_installed_package() {
        let dir = TestDir::new("installed");
        let config = config(&dir, "tool");
        config.installation.create_dirs().unwrap();
        fs::write(record_path(&config), "1.2.3\n").unwrap();

        // Would fail if it tried to download, since the repository is fake
        let mut state = State::new([config].into_iter()).unwrap();
        state.install("tool").unwrap();
    }

    #[test]
    fn install_refuses_unmanaged_file() {
        let dir = TestDir::new("unmanaged");
        let config = config(&dir, "tool");
        prepare(&config, false);
        let binary = config.installation.path.clone();
        let mut state = State::new([config].into_iter()).unwrap();

        // E.g. a system binary that `name` and `dir` point to by mistake;
        // fails before downloading (the repository is fake)
        fs::create_dir_all(binary.parent().unwrap()).unwrap();
        fs::write(&binary, "system").unwrap();
        assert!(matches!(state.install("tool"), Err(Error::Unmanaged(_))));
        assert_eq!(fs::read_to_string(&binary).unwrap(), "system");

        // Even a broken symbolic link
        fs::remove_file(&binary).unwrap();
        std::os::unix::fs::symlink(dir.0.join("missing"), &binary).unwrap();
        assert!(matches!(state.install("tool"), Err(Error::Unmanaged(_))));
    }

    #[test]
    fn update_and_list_reject_duplicate_names() {
        let dir = TestDir::new("duplicates");
        let other = TestDir::new("duplicates-other");
        let (a, b, b2) = (config(&dir, "a"), config(&dir, "b"), config(&other, "b"));
        for config in [&a, &b, &b2] {
            prepare(config, false);
        }
        // Installed, so `update` would try to download it if not rejected
        prepare(&b, true);
        let record = record_path(&b);

        let mut state = State::new([a, b, b2].into_iter()).unwrap();
        assert!(!state.update());
        assert!(!state.list());
        assert_eq!(fs::read_to_string(record).unwrap(), "1.2.3\n");
    }

    #[test]
    fn update_skips_packages_that_are_not_installed() {
        let dir = TestDir::new("update-skip");
        let config = config(&dir, "tool");
        prepare(&config, false);

        // Would fail if it tried to download, since the repository is fake
        let mut state = State::new([config].into_iter()).unwrap();
        assert!(state.update());
        assert!(state.list());
    }

    #[test]
    fn list_reports_invalid_record() {
        let dir = TestDir::new("list-invalid");
        let config = config(&dir, "tool");
        prepare(&config, true);
        fs::write(record_path(&config), "1..2\n").unwrap();

        let mut state = State::new([config].into_iter()).unwrap();
        assert!(!state.list());
    }

    #[test]
    fn unreadable_record_is_an_error() {
        let dir = TestDir::new("record-dir");
        let config = config(&dir, "tool");
        prepare(&config, false);
        fs::create_dir(record_path(&config)).unwrap();

        // Not mistaken for "not installed"
        assert!(matches!(
            installed_version(&config),
            Err(Error::Io { action: "read", .. })
        ));
    }

    #[test]
    fn replace_file_does_not_follow_symlinks() {
        let dir = TestDir::new("replace-symlink");
        let victim = dir.0.join("victim");
        fs::write(&victim, "keep").unwrap();
        let path = dir.0.join("tool");

        // A symbolic link at the path is replaced itself
        std::os::unix::fs::symlink(&victim, &path).unwrap();
        replace_file(&path, 0o755, |file| file.write_all(b"new")).unwrap();
        assert!(!path.is_symlink());
        assert_eq!(fs::read_to_string(&path).unwrap(), "new");
        assert_eq!(fs::read_to_string(&victim).unwrap(), "keep");

        // A symbolic link planted at the temporary name is not written
        // through (see `download::create_file`)
        std::os::unix::fs::symlink(&victim, dir.0.join(".tool.grm-new")).unwrap();
        replace_file(&path, 0o755, |file| file.write_all(b"newer")).unwrap();
        assert_eq!(fs::read_to_string(&path).unwrap(), "newer");
        assert_eq!(fs::read_to_string(&victim).unwrap(), "keep");
    }

    #[test]
    fn replace_file_sets_mode_regardless_of_umask() {
        let dir = TestDir::new("replace-mode");
        let path = dir.0.join("tool");
        // The usual umask 022 would turn 0o777 into 0o755
        for mode in [0o777, 0o700, 0o644] {
            replace_file(&path, mode, |file| file.write_all(b"x")).unwrap();
            let actual = fs::metadata(&path).unwrap().permissions().mode() & 0o7777;
            assert_eq!(actual, mode, "{mode:o}");
        }
    }

    /// Installs, updates and removes lego for real; run with
    /// `cargo test -- --ignored`.
    #[test]
    #[ignore = "needs network access to GitHub"]
    fn installs_updates_and_removes_lego() {
        let dir = TestDir::new("lego");
        let path = dir.0.display();
        let config: Config = toml::from_str(&format!(
            r#"
            [repository]
            url = "https://github.com/go-acme/lego/releases"

            [installation]
            name = "lego"
            dir = "{path}/bin"
            runtime_dir = "{path}/run"
            data_dir = "{path}/data"

            [asset]
            version = '(?<major>\d+)\.(?<minor>\d+)\.(?<patch>\d+)'
            target = 'lego_v${{major}}.${{minor}}.${{patch}}_linux_amd64.tar.gz'
            unpack = 'lego'
            "#
        ))
        .unwrap();
        config.installation.create_dirs().unwrap();
        let (binary, record) = (config.installation.path.clone(), record_path(&config));
        let mut state = State::new([config].into_iter()).unwrap();

        // Install: binary with the default mode, and its version recorded
        state.install("lego").unwrap();
        let installed = fs::read_to_string(&record).unwrap();
        assert!(Version::parse(installed.trim()).is_some(), "{installed:?}");
        let mode = fs::metadata(&binary).unwrap().permissions().mode() & 0o7777;
        assert_eq!(mode, 0o755);
        let output = std::process::Command::new(&binary)
            .arg("--version")
            .output()
            .unwrap();
        assert!(
            String::from_utf8_lossy(&output.stdout).contains(installed.trim()),
            "{output:?}"
        );

        // Up to date: nothing changes
        let modified = fs::metadata(&binary).unwrap().modified().unwrap();
        assert!(state.update());
        assert_eq!(fs::metadata(&binary).unwrap().modified().unwrap(), modified);

        // Outdated record: updated, and the record is current again
        fs::write(&record, "0.0.1\n").unwrap();
        assert!(state.update());
        assert_eq!(fs::read_to_string(&record).unwrap(), installed);

        // Remove: binary and record are gone, no temporary files are left
        state.remove("lego").unwrap();
        assert!(!binary.exists() && !record.exists());
        assert_eq!(fs::read_dir(dir.0.join("bin")).unwrap().count(), 0);
        assert_eq!(fs::read_dir(state.tmp_dir.path()).unwrap().count(), 0);
    }
}

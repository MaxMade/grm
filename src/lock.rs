//! Exclusive, file-based locks that serialize access between processes.
//!
//! A value that should only be used by one `grm` process at a time (e.g. a
//! configuration) implements [`LockFile`] to name its lock file, e.g.
//! `<runtime_dir>/.<name>.lock` for a configuration. Wrapping the value in a [`Lock`]
//! makes it inaccessible until [`Lock::lock`] has acquired an exclusive lock
//! on that file; the returned [`LockGuard`] borrows the value from the
//! [`Lock`], gives access to it and holds the lock until it is dropped.
//!
//! The lock is advisory (`flock(2)` on Linux): it only excludes other
//! processes that use the same lock file through this module, not arbitrary
//! access to the file or the value. It is tied to the open file, so it is
//! released automatically when the process exits, even after a crash, and a
//! stale lock file left behind does not block later runs.
//!
//! # Deadlocks
//!
//! Each [`Lock::lock`] opens the lock file anew, and locks on different open
//! files exclude each other even within one process. Locking a second value
//! whose [`LockFile::path`] is the same file (e.g. two configurations with
//! the same `name` and `runtime_dir`) while a guard for the first one is
//! alive therefore blocks forever.

use std::fs::{File, OpenOptions};
use std::io::Error as IOError;
use std::ops::{Deref, DerefMut};
use std::path::PathBuf;

/// A value that is protected by a lock file.
pub trait LockFile {
    /// Path of the lock file, e.g. `<runtime_dir>/.<name>.lock`.
    ///
    /// The file is created by [`Lock::lock`] if it does not exist, but its
    /// parent directory must already exist.
    fn path(&self) -> PathBuf;
}

/// A value that is only accessible while its lock file is locked.
pub struct Lock<T: LockFile>(T);

impl<T: LockFile> Lock<T> {
    /// Wraps `value`; it stays inaccessible until it is locked.
    pub fn new(value: T) -> Self {
        Self(value)
    }

    /// Acquires an exclusive lock on [`LockFile::path`], blocking until no
    /// other process holds it, and returns a guard giving access to the value.
    ///
    /// The lock file is created if necessary; its contents are left as is.
    ///
    /// # Errors
    ///
    /// Fails if the lock file cannot be opened or created (e.g. its directory
    /// does not exist or is not writable) or cannot be locked. The value stays
    /// in the [`Lock`] in that case, so locking can be retried.
    pub fn lock(&mut self) -> Result<LockGuard<'_, T>, IOError> {
        let locked_file = OpenOptions::new()
            .write(true)
            .create(true)
            .truncate(false)
            .open(self.0.path())?;
        locked_file.lock()?;

        Ok(LockGuard {
            value: &mut self.0,
            _locked_file: locked_file,
        })
    }
}

/// Access to a locked value; the lock is held until the guard is dropped.
///
/// Created by [`Lock::lock`]. Dropping the guard closes the lock file, which
/// releases the lock. The lock file itself is not removed, since another
/// process may already be waiting on it.
pub struct LockGuard<'a, T: LockFile> {
    value: &'a mut T,
    // Only held to keep the lock until the guard is dropped
    _locked_file: File,
}

impl<'a, T: LockFile> Deref for LockGuard<'a, T> {
    type Target = T;

    fn deref(&self) -> &Self::Target {
        self.value
    }
}

impl<'a, T: LockFile> DerefMut for LockGuard<'a, T> {
    fn deref_mut(&mut self) -> &mut Self::Target {
        self.value
    }
}

#[cfg(test)]
mod tests {
    use std::fs::TryLockError;
    use std::path::Path;

    use super::*;

    /// A directory that is unique per test and removed afterwards, so that
    /// tests running in parallel do not share lock files.
    struct TestDir(PathBuf);

    impl TestDir {
        fn new(name: &str) -> Self {
            let dir =
                std::env::temp_dir().join(format!("grm-lock-test-{}-{name}", std::process::id()));
            // Leftovers from an aborted run would make tests unreliable
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

    struct Value {
        lock_path: PathBuf,
        data: u32,
    }

    impl LockFile for Value {
        fn path(&self) -> PathBuf {
            self.lock_path.clone()
        }
    }

    fn value(dir: &TestDir) -> Value {
        Value {
            lock_path: dir.0.join(".lock"),
            data: 1,
        }
    }

    /// Whether another process could lock `path` right now. A separately
    /// opened file behaves like another process, since `flock` locks belong
    /// to the open file, not to the process.
    fn is_locked(path: &Path) -> bool {
        match File::open(path).unwrap().try_lock() {
            Ok(()) => false,
            Err(TryLockError::WouldBlock) => true,
            Err(TryLockError::Error(error)) => panic!("try_lock failed: {error}"),
        }
    }

    #[test]
    fn creates_missing_lock_file() {
        let dir = TestDir::new("creates");
        let path = dir.0.join(".lock");
        assert!(!path.exists());

        let mut lock = Lock::new(value(&dir));
        let guard = lock.lock().unwrap();
        assert!(path.is_file());
        assert!(is_locked(&path));
        drop(guard);
    }

    #[test]
    fn keeps_existing_lock_file() {
        let dir = TestDir::new("existing");
        let path = dir.0.join(".lock");
        std::fs::write(&path, "content").unwrap();

        let mut lock = Lock::new(value(&dir));
        let guard = lock.lock().unwrap();
        assert!(is_locked(&path));
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "content");
        drop(guard);
    }

    #[test]
    fn guard_gives_access_to_value() {
        let dir = TestDir::new("access");

        let mut lock = Lock::new(value(&dir));
        let mut guard = lock.lock().unwrap();
        assert_eq!(guard.data, 1);
        guard.data = 2;
        assert_eq!(guard.data, 2);
    }

    #[test]
    fn value_survives_relocking() {
        let dir = TestDir::new("relock");
        let path = dir.0.join(".lock");

        let mut lock = Lock::new(value(&dir));
        let mut guard = lock.lock().unwrap();
        guard.data = 2;
        drop(guard);
        assert!(!is_locked(&path));
        // The lock file is kept, since others may be waiting on it
        assert!(path.is_file());

        // The value survives unlocking and can be locked again
        let guard = lock.lock().unwrap();
        assert_eq!(guard.data, 2);
        assert!(is_locked(&path));
    }

    #[test]
    fn drop_releases_lock() {
        let dir = TestDir::new("drop");
        let path = dir.0.join(".lock");

        let mut lock = Lock::new(value(&dir));
        let guard = lock.lock().unwrap();
        assert!(is_locked(&path));
        drop(guard);
        assert!(!is_locked(&path));
    }

    #[test]
    fn lock_waits_for_other_holder() {
        let dir = TestDir::new("wait");
        let path = dir.0.join(".lock");

        // Simulate another process holding the lock
        let other = File::create(&path).unwrap();
        other.lock().unwrap();

        let (sender, receiver) = std::sync::mpsc::channel();
        let mut lock = Lock::new(value(&dir));
        let thread = std::thread::spawn(move || {
            let guard = lock.lock().unwrap();
            sender.send(()).unwrap();
            drop(guard);
        });

        // Not acquired while the other holder is active. A timeout cannot
        // prove blocking, but catches a lock that is not exclusive.
        let timeout = std::time::Duration::from_millis(100);
        assert!(receiver.recv_timeout(timeout).is_err());

        other.unlock().unwrap();
        receiver
            .recv_timeout(std::time::Duration::from_secs(10))
            .expect("lock not acquired after release");
        thread.join().unwrap();
    }

    #[test]
    fn fails_without_directory() {
        let dir = TestDir::new("missing");
        let value = Value {
            lock_path: dir.0.join("missing/.lock"),
            data: 1,
        };

        let mut lock = Lock::new(value);
        let error = lock.lock().err().unwrap();
        assert_eq!(error.kind(), std::io::ErrorKind::NotFound);

        // The value is kept, so locking can be retried
        std::fs::create_dir(dir.0.join("missing")).unwrap();
        assert_eq!(lock.lock().unwrap().data, 1);
    }
}

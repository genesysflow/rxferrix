//! The data directory lock: `<dir>/ferrix.lock`, an exclusive advisory lock (`flock` on Unix, `LockFileEx` on
//! Windows) held for as long as `rxferrix serve` (or `rxferrix import`) runs. `rxferrix restore` takes it too, so it refuses
//! to swap the database under a running server. The operating system releases the lock when the process exits, even
//! on a crash, so a leftover file never blocks anything. The file holds the pid of the process that last locked it
//! (informational only).

use std::fs::{File, OpenOptions};
use std::io::{Read, Seek, Write};
use std::path::{Path, PathBuf};

use anyhow::Context;

/// Name of the lock file in the data directory.
pub const LOCK_FILE: &str = "ferrix.lock";

/// A held lock on a data directory. Released when dropped.
#[derive(Debug)]
pub struct DirLock {
    file: File,
    path: PathBuf,
}

impl DirLock {
    /// Lock `dir` (created if missing), or fail at once if another process (or another `DirLock` in this one) holds
    /// it.
    pub fn acquire(dir: &Path) -> crate::Result<DirLock> {
        std::fs::create_dir_all(dir).with_context(|| format!("creating data dir {}", dir.display()))?;
        let path = dir.join(LOCK_FILE);
        let mut file = OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .open(&path)
            .with_context(|| format!("opening {}", path.display()))?;
        match file.try_lock() {
            Ok(()) => {}
            Err(std::fs::TryLockError::WouldBlock) => {
                let mut holder = String::new();
                let _ = file.read_to_string(&mut holder);
                let holder = holder.trim();
                let by = if holder.is_empty() { String::new() } else { format!(" (by pid {holder})") };
                anyhow::bail!(
                    "the data directory {} is in use{by}: stop the rxferrix server using it first",
                    dir.display()
                );
            }
            Err(std::fs::TryLockError::Error(e)) => {
                return Err(e).with_context(|| format!("locking {}", path.display()));
            }
        }
        file.set_len(0)?;
        file.rewind()?;
        write!(file, "{}", std::process::id())?;
        file.flush()?;
        Ok(DirLock { file, path })
    }

    /// Like [`DirLock::acquire`], retrying for up to `wait` while another process still holds it (a takeover waits
    /// for the old server to exit, `crate::ops::takeover`).
    pub async fn acquire_waiting(dir: &Path, wait: std::time::Duration) -> crate::Result<DirLock> {
        let deadline = tokio::time::Instant::now() + wait;
        loop {
            match DirLock::acquire(dir) {
                Ok(lock) => return Ok(lock),
                Err(e) if tokio::time::Instant::now() >= deadline => return Err(e),
                Err(_) => tokio::time::sleep(std::time::Duration::from_millis(20)).await,
            }
        }
    }

    /// Whether some process holds the lock on `dir` right now.
    pub fn is_locked(dir: &Path) -> bool {
        let Ok(file) = OpenOptions::new().read(true).write(true).open(dir.join(LOCK_FILE)) else { return false };
        match file.try_lock() {
            Ok(()) => {
                let _ = file.unlock();
                false
            }
            Err(_) => true,
        }
    }

    pub fn path(&self) -> &Path {
        &self.path
    }
}

impl Drop for DirLock {
    fn drop(&mut self) {
        let _ = self.file.unlock();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn exclusive_and_released_on_drop() {
        let dir = tempfile::tempdir().unwrap();
        assert!(!DirLock::is_locked(dir.path()));
        let lock = DirLock::acquire(dir.path()).unwrap();
        assert!(DirLock::is_locked(dir.path()));
        let err = DirLock::acquire(dir.path()).unwrap_err().to_string();
        assert!(err.contains("is in use") && err.contains(&std::process::id().to_string()), "{err}");
        drop(lock);
        assert!(!DirLock::is_locked(dir.path()));
        let _again = DirLock::acquire(dir.path()).unwrap();
    }
}

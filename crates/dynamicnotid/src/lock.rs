//! Single-instance lock. Owning `org.freedesktop.Notifications` means two daemons would fight
//! over the bus name and the socket; an exclusive `flock` on a lockfile guarantees only one
//! `dynamicnotid` runs. The lock is released automatically when the process exits (the kernel
//! drops the fd), so a crash never leaves a stale lock.

use std::fs::File;
use std::os::unix::io::AsRawFd;
use std::path::Path;

/// Holds the lock for the process lifetime. Dropping it (on clean exit) releases the flock and
/// removes the lockfile.
pub struct InstanceLock {
    _file: File,
    path: std::path::PathBuf,
}

impl InstanceLock {
    /// Acquire the lock, or return `Ok(None)` if another daemon already holds it.
    pub fn acquire(path: &Path) -> anyhow::Result<Option<InstanceLock>> {
        let file = File::create(path)
            .map_err(|e| anyhow::anyhow!("cannot open lockfile {path:?}: {e}"))?;
        // LOCK_EX | LOCK_NB: take the exclusive lock without blocking.
        let rc = unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) };
        if rc != 0 {
            let err = std::io::Error::last_os_error();
            if err.raw_os_error() == Some(libc::EWOULDBLOCK) {
                return Ok(None); // someone else holds it
            }
            return Err(anyhow::anyhow!("flock failed on {path:?}: {err}"));
        }
        Ok(Some(InstanceLock { _file: file, path: path.to_path_buf() }))
    }
}

impl Drop for InstanceLock {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.path);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn second_acquire_is_blocked() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("test.lock");
        let first = InstanceLock::acquire(&path).unwrap();
        assert!(first.is_some());
        let second = InstanceLock::acquire(&path).unwrap();
        assert!(second.is_none(), "second lock should be blocked while first is held");
        drop(first);
        // After releasing, a new acquire succeeds.
        assert!(InstanceLock::acquire(&path).unwrap().is_some());
    }
}

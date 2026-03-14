use anyhow::{Context, Result};
use std::path::PathBuf;
use tokio::fs;

/// RAII guard that writes the process PID to a file on creation and removes
/// it on drop.  A stale PID file from a previous (non-running) process is
/// silently overwritten.  If another ZeroClaw instance is already running
/// (PID file exists and the process is alive) this constructor returns an
/// error so the new daemon can abort cleanly.
#[derive(Debug)]
pub struct PidFileGuard {
    path: PathBuf,
}

impl PidFileGuard {
    /// Acquire the PID file at `path`.  Errors if another live process holds
    /// the lock or if the file cannot be written.
    pub async fn acquire(path: PathBuf) -> Result<Self> {
        // If a PID file already exists, check whether its owner is still alive.
        if fs::try_exists(&path).await.unwrap_or(false) {
            if let Ok(contents) = fs::read_to_string(&path).await {
                let existing_pid = contents.trim().parse::<u32>().unwrap_or(0);
                if existing_pid != 0 && process_is_alive(existing_pid) {
                    anyhow::bail!(
                        "Another ZeroClaw daemon is already running (pid {existing_pid}). \
                         Remove {} to override.",
                        path.display()
                    );
                }
                // Stale file — fall through and overwrite.
                tracing::warn!("Removing stale PID file (pid {existing_pid} is no longer running)");
            }
        }

        // Ensure parent directory exists.
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent).await.with_context(|| {
                format!("Failed to create PID file directory: {}", parent.display())
            })?;
        }

        let pid = std::process::id();
        fs::write(&path, format!("{pid}\n"))
            .await
            .with_context(|| format!("Failed to write PID file: {}", path.display()))?;

        tracing::debug!(pid, path = %path.display(), "PID file acquired");
        Ok(Self { path })
    }
}

impl Drop for PidFileGuard {
    fn drop(&mut self) {
        // Best-effort synchronous removal — we're in a destructor.
        if let Err(e) = std::fs::remove_file(&self.path) {
            if e.kind() != std::io::ErrorKind::NotFound {
                tracing::warn!(path = %self.path.display(), error = %e, "Failed to remove PID file");
            }
        } else {
            tracing::debug!(path = %self.path.display(), "PID file released");
        }
    }
}

/// Returns `true` if a process with `pid` is currently running.
///
/// On Unix this sends signal 0 to the PID (no-op — only checks existence).
/// On non-Unix we conservatively assume the process is alive to avoid
/// false negatives.
fn process_is_alive(pid: u32) -> bool {
    #[cfg(unix)]
    {
        // SAFETY: kill(pid, 0) is always safe — it does not send a real signal.
        let result = unsafe { libc::kill(pid as libc::pid_t, 0) };
        result == 0
    }
    #[cfg(not(unix))]
    {
        let _ = pid;
        true
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    #[tokio::test]
    async fn acquire_writes_pid_and_drop_removes_file() {
        let tmp = TempDir::new().unwrap();
        let path = tmp.path().join("zeroclaw.pid");

        {
            let _guard = PidFileGuard::acquire(path.clone()).await.unwrap();
            let contents = fs::read_to_string(&path).await.unwrap();
            let written_pid: u32 = contents.trim().parse().unwrap();
            assert_eq!(written_pid, std::process::id());
        }

        // Guard dropped — file should be gone.
        assert!(!path.exists(), "PID file should be removed on drop");
    }

    #[tokio::test]
    async fn acquire_overwrites_stale_pid_file() {
        let tmp = TempDir::new().unwrap();
        let path = tmp.path().join("zeroclaw.pid");

        // Write a clearly non-running PID (PID 1 is init and we're not it,
        // but PID 99999999 almost certainly doesn't exist).
        fs::write(&path, "99999999\n").await.unwrap();

        // Should succeed by overwriting the stale file.
        let guard = PidFileGuard::acquire(path.clone()).await.unwrap();
        let contents = fs::read_to_string(&path).await.unwrap();
        let written_pid: u32 = contents.trim().parse().unwrap();
        assert_eq!(written_pid, std::process::id());
        drop(guard);
    }

    #[tokio::test]
    async fn acquire_rejects_live_pid() {
        let tmp = TempDir::new().unwrap();
        let path = tmp.path().join("zeroclaw.pid");

        // Write OUR OWN PID — we're definitely alive.
        let our_pid = std::process::id();
        fs::write(&path, format!("{our_pid}\n")).await.unwrap();

        let result = PidFileGuard::acquire(path.clone()).await;
        assert!(result.is_err(), "Should reject a live PID");
        assert!(result.unwrap_err().to_string().contains("already running"));
    }
}

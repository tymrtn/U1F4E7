// Copyright (c) 2026 Tyler Martin
// Licensed under FSL-1.1-ALv2 (see LICENSE)

//! Which server on a mail home runs the background jobs.
//!
//! Every `serve` used to start the full set: mailbox sync, add-ons, shelf life,
//! digests, scheduled sends, and the startup recovery of sends a crashed process
//! left mid-attempt. Two servers on one home (the desktop app beside the
//! installed dashboard) each recovered the other's in-flight sends and raced
//! each other on every arrival. An OS lock on a file in the home picks one
//! owner. The kernel drops it when that process exits for any reason, so a
//! waiting server can take over.

use std::fs::{File, OpenOptions, TryLockError};
use std::path::Path;
use std::time::Duration;

const LOCK_FILE_NAME: &str = "background.lock";

/// How often a server without the lock checks whether it has come free.
pub const RETRY_INTERVAL: Duration = Duration::from_secs(5);

/// Held for as long as this process runs the home's background jobs.
#[derive(Debug)]
pub struct BackgroundOwner {
    _file: File,
}

impl BackgroundOwner {
    /// Take the background jobs for the home at `dir`. `Ok(None)` means
    /// another process runs them.
    ///
    /// The lock file is never removed: deleting it would let one server hold
    /// the old file's lock while another locks a fresh file at the same path.
    pub fn try_acquire(dir: &Path) -> anyhow::Result<Option<Self>> {
        let path = dir.join(LOCK_FILE_NAME);
        let file = OpenOptions::new()
            .create(true)
            .truncate(false)
            .write(true)
            .open(&path)
            .map_err(|e| anyhow::anyhow!("cannot open background lock {}: {e}", path.display()))?;
        match file.try_lock() {
            Ok(()) => Ok(Some(Self { _file: file })),
            Err(TryLockError::WouldBlock) => Ok(None),
            Err(TryLockError::Error(e)) => Err(anyhow::anyhow!(
                "cannot lock background lock {}: {e}",
                path.display()
            )),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn one_owner_per_home_until_it_lets_go() {
        let home = tempfile::tempdir().unwrap();
        let first = BackgroundOwner::try_acquire(home.path()).unwrap();
        assert!(first.is_some(), "a free home must be taken");
        assert!(
            BackgroundOwner::try_acquire(home.path()).unwrap().is_none(),
            "a held home must not be taken twice"
        );
        drop(first);
        assert!(
            BackgroundOwner::try_acquire(home.path()).unwrap().is_some(),
            "a released home must be taken again"
        );
    }

    #[test]
    fn homes_are_owned_separately() {
        let one = tempfile::tempdir().unwrap();
        let two = tempfile::tempdir().unwrap();
        let _held = BackgroundOwner::try_acquire(one.path()).unwrap().unwrap();
        assert!(BackgroundOwner::try_acquire(two.path()).unwrap().is_some());
    }

    #[test]
    fn a_missing_home_is_an_error_rather_than_another_owner() {
        let home = tempfile::tempdir().unwrap();
        let gone = home.path().join("missing");
        assert!(BackgroundOwner::try_acquire(&gone).is_err());
    }
}

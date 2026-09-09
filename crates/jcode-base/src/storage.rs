#![cfg_attr(test, allow(clippy::items_after_test_module))]

pub use jcode_storage::*;

use anyhow::Result;
use serde::de::DeserializeOwned;
use std::path::Path;

pub fn read_json<T: DeserializeOwned>(path: &Path) -> Result<T> {
    jcode_storage::read_json_with_recovery_handler(path, |event| match event {
        jcode_storage::StorageRecoveryEvent::CorruptPrimary { path, error } => {
            crate::logging::warn(&format!(
                "Corrupt JSON at {}, trying backup: {}",
                path.display(),
                error
            ));
        }
        jcode_storage::StorageRecoveryEvent::RecoveredFromBackup { backup_path } => {
            crate::logging::info(&format!("Recovered from backup: {}", backup_path.display()));
        }
    })
}

#[cfg(any(test, feature = "test-support"))]
use std::sync::{Mutex, MutexGuard, OnceLock};

#[cfg(any(test, feature = "test-support"))]
pub fn test_env_lock() -> &'static Mutex<()> {
    static ENV_LOCK: OnceLock<Mutex<()>> = OnceLock::new();
    ENV_LOCK.get_or_init(|| Mutex::new(()))
}

#[cfg(any(test, feature = "test-support"))]
pub fn lock_test_env() -> MutexGuard<'static, ()> {
    test_env_lock()
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

/// Reader/writer lock over the *shared* test `JCODE_HOME`.
///
/// Separate from [`test_env_lock`], which stays a plain mutex because ~40
/// helper signatures name its guard type. This one expresses the relationship
/// that mutex cannot: many tests read the shared home concurrently, and a test
/// that repoints `JCODE_HOME` at its own temp dir must exclude all of them.
///
/// Without it, ~800 tests that build an app against the shared home took no
/// lock at all, so a concurrent home swap could move `JCODE_HOME` out from
/// under them mid-test: their session and reload files were written into
/// another test's temp dir and vanished with it.
#[cfg(any(test, feature = "test-support"))]
pub fn shared_test_home_lock() -> &'static std::sync::RwLock<()> {
    static HOME_LOCK: OnceLock<std::sync::RwLock<()>> = OnceLock::new();
    HOME_LOCK.get_or_init(|| std::sync::RwLock::new(()))
}

#[cfg(test)]
mod tests;

//! Per-canonical-file serialisation for heading mutations.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, Weak};

type AsyncLock = tokio::sync::Mutex<()>;

/// Weakly-held async locks keyed by canonical file path.
///
/// The map never owns a lock strongly: request-local [`Arc`] handles keep an
/// entry alive across acquisition and use, and dead entries are pruned during
/// later lookups. This avoids both unbounded key retention and the unsafe
/// remove-on-release pattern that can split waiters across distinct locks.
#[derive(Clone, Default)]
pub(super) struct FileLocks {
    inner: Arc<Mutex<HashMap<PathBuf, Weak<AsyncLock>>>>,
}

impl FileLocks {
    /// Return the shared async lock for `canonical`, creating it when absent.
    pub(super) fn lock_for(&self, canonical: &Path) -> Result<Arc<AsyncLock>, String> {
        let mut locks = self
            .inner
            .lock()
            .map_err(|_| "file-lock registry mutex poisoned".to_string())?;
        locks.retain(|_, lock| lock.strong_count() > 0);
        if let Some(lock) = locks.get(canonical).and_then(Weak::upgrade) {
            return Ok(lock);
        }

        let lock = Arc::new(AsyncLock::new(()));
        locks.insert(canonical.to_path_buf(), Arc::downgrade(&lock));
        Ok(lock)
    }
}

#[cfg(test)]
mod tests {
    use super::FileLocks;
    use std::path::Path;
    use std::sync::Arc;
    use std::time::Duration;

    #[tokio::test]
    async fn same_path_lock_waits_until_first_guard_drops() {
        let locks = FileLocks::default();
        let first = locks.lock_for(Path::new("/tmp/same.org")).unwrap();
        let second = locks.lock_for(Path::new("/tmp/same.org")).unwrap();
        assert!(Arc::ptr_eq(&first, &second));
        let first_guard = first.lock().await;
        let (entered_tx, entered_rx) = tokio::sync::oneshot::channel();

        let waiter = tokio::spawn(async move {
            let _guard = second.lock().await;
            let _ = entered_tx.send(());
        });

        assert!(
            tokio::time::timeout(Duration::from_millis(50), entered_rx)
                .await
                .is_err(),
            "second task entered while the first guard was live"
        );
        drop(first_guard);
        tokio::time::timeout(Duration::from_secs(1), waiter)
            .await
            .expect("waiter should proceed after release")
            .unwrap();
    }
}

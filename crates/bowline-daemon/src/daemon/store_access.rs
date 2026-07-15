use super::*;
use std::cell::RefCell;

pub(super) struct CachedStore {
    path: PathBuf,
    store: RefCell<Option<MetadataStore>>,
}

impl CachedStore {
    pub(super) fn new(path: PathBuf) -> Self {
        Self {
            path,
            store: RefCell::new(None),
        }
    }

    pub(super) fn with_store<T>(
        &self,
        f: impl FnOnce(&MetadataStore) -> Result<T, MetadataError>,
    ) -> Result<T, MetadataError> {
        if self.store.borrow().is_none() {
            match MetadataStore::open(&self.path) {
                Ok(store) => {
                    *self.store.borrow_mut() = Some(store);
                }
                Err(error) => {
                    *self.store.borrow_mut() = None;
                    return Err(error);
                }
            }
        }

        let result = {
            let store = self.store.borrow();
            f(store.as_ref().expect("cached store is initialized"))
        };
        if result.is_err() {
            *self.store.borrow_mut() = None;
        }
        result
    }

    pub(super) fn with_store_mut<T>(
        &self,
        f: impl FnOnce(&mut MetadataStore) -> Result<T, MetadataError>,
    ) -> Result<T, MetadataError> {
        if self.store.borrow().is_none() {
            match MetadataStore::open(&self.path) {
                Ok(store) => {
                    *self.store.borrow_mut() = Some(store);
                }
                Err(error) => {
                    *self.store.borrow_mut() = None;
                    return Err(error);
                }
            }
        }

        let result = {
            let mut store = self.store.borrow_mut();
            f(store.as_mut().expect("cached store is initialized"))
        };
        if result.is_err() {
            *self.store.borrow_mut() = None;
        }
        result
    }

    /// Drop the cached handle so the next access reopens the store. Used when
    /// a store error was swallowed inside the access closure (for example via
    /// `StoreHealth::record`), which `with_store` cannot observe on its own.
    pub(super) fn clear(&self) {
        *self.store.borrow_mut() = None;
    }

    #[cfg(test)]
    pub(super) fn has_cached_handle(&self) -> bool {
        self.store.borrow().is_some()
    }
}

#[cfg(test)]
pub(super) fn open_store_for_test(path: PathBuf) -> Result<MetadataStore, MetadataError> {
    type StoreForTest = MetadataStore;
    StoreForTest::open(path)
}

#[cfg(test)]
mod tests {
    use super::super::tests::unique_temp_dir;
    use super::super::{
        CachedStore, ContinuousSyncOptions, ContinuousSyncRuntime, DEFAULT_DATABASE_FILE,
        MetadataError, SyncOnceArgs,
    };
    use std::fs;
    use std::time::Duration;

    #[test]
    fn cached_store_reuses_handle_and_reopens_after_error() {
        let temp = unique_temp_dir("bowline-cached-store");
        let store = CachedStore::new(temp.join(DEFAULT_DATABASE_FILE));
        store
            .with_store(|store| store.assert_schema_tables())
            .expect("open");
        assert!(store.has_cached_handle());
        store
            .with_store(|store| store.assert_schema_tables())
            .expect("reuse");
        let err = MetadataError::InvalidStorageMetadata("forced".into());
        let failed = store.with_store(|_| Err::<(), _>(err));
        assert!(failed.is_err());
        assert!(!store.has_cached_handle());
        store
            .with_store(|store| store.assert_schema_tables())
            .expect("reopen");
        let _ = fs::remove_dir_all(temp);
    }

    #[test]
    fn metadata_store_for_write_reopens_after_closure_swallows_recorded_failure() {
        // Mirrors the real daemon pattern: closures like record_component_states
        // record store write failures via StoreHealth::record and still return
        // Ok(()), so CachedStore::with_store never sees the error. The runtime
        // must detect the recorded failure and drop the cached handle anyway.
        let temp = unique_temp_dir("bowline-cached-store-swallowed");
        fs::create_dir_all(&temp).expect("temp dir");
        let runtime = ContinuousSyncRuntime::new(ContinuousSyncOptions {
            args: SyncOnceArgs {
                root: temp.clone(),
                state_root: temp.clone(),
                workspace_id: "ws_cached_store_swallowed".to_string(),
                device_id: "device-test".to_string(),
                sync_claim: None,
                scan_scope: Default::default(),
            },
            interval: Duration::from_secs(60),
            max_ticks: None,
        });

        let outcome =
            runtime.metadata_store_for_write("test(open)", |store| store.assert_schema_tables());
        assert!(outcome.is_some());
        assert!(runtime.store.has_cached_handle());

        let outcome = runtime.metadata_store_for_write("test(swallowed)", |store| {
            store.assert_schema_tables()?;
            let forced: Result<(), MetadataError> =
                Err(MetadataError::InvalidStorageMetadata("forced".into()));
            // Swallow the failure exactly like the daemon write paths do.
            assert!(
                runtime
                    .store_health
                    .record("forced_write", forced)
                    .is_none()
            );
            Ok(())
        });
        // The closure itself succeeded, but the swallowed failure must clear
        // the cached handle so the next access reopens the store.
        assert!(outcome.is_some());
        assert!(!runtime.store.has_cached_handle());
        assert!(runtime.store_health.is_degraded());

        let outcome =
            runtime.metadata_store_for_write("test(reopen)", |store| store.assert_schema_tables());
        assert!(outcome.is_some());
        assert!(runtime.store.has_cached_handle());

        let _ = fs::remove_dir_all(temp);
    }
}

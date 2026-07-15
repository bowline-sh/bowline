use std::{
    collections::BTreeMap,
    path::{Path, PathBuf},
    sync::{
        Arc, Mutex, OnceLock,
        atomic::{AtomicU64, Ordering},
    },
};

use super::DeviceKeyError;

static LOCKS: OnceLock<Mutex<BTreeMap<String, Arc<Mutex<()>>>>> = OnceLock::new();
static NEXT_TEMP_ID: AtomicU64 = AtomicU64::new(1);

pub(super) fn secret_temp_path(path: &Path) -> PathBuf {
    path.with_extension(format!(
        "tmp-{}-{}",
        std::process::id(),
        NEXT_TEMP_ID.fetch_add(1, Ordering::Relaxed)
    ))
}

pub(super) fn verifier_replacement_lock(key: String) -> Result<Arc<Mutex<()>>, DeviceKeyError> {
    let mut locks = LOCKS
        .get_or_init(|| Mutex::new(BTreeMap::new()))
        .lock()
        .map_err(|_| DeviceKeyError::Unavailable("verifier replacement lock poisoned".into()))?;
    Ok(locks
        .entry(key)
        .or_insert_with(|| Arc::new(Mutex::new(())))
        .clone())
}

pub(super) fn with_verifier_transaction<T>(
    key: String,
    transaction: impl FnOnce() -> Result<T, DeviceKeyError>,
) -> Result<T, DeviceKeyError> {
    let lock = verifier_replacement_lock(key.clone())?;
    let _guard = lock
        .lock()
        .map_err(|_| DeviceKeyError::Unavailable("verifier transaction lock poisoned".into()))?;
    transaction_entered(&key);
    transaction()
}

#[cfg(test)]
type TransactionHook = Arc<dyn Fn() + Send + Sync>;

#[cfg(test)]
static TRANSACTION_HOOKS: OnceLock<Mutex<BTreeMap<String, TransactionHook>>> = OnceLock::new();

#[cfg(test)]
pub(super) fn set_transaction_hook(key: String, hook: Option<TransactionHook>) {
    let mut hooks = TRANSACTION_HOOKS
        .get_or_init(|| Mutex::new(BTreeMap::new()))
        .lock()
        .expect("transaction hook registry poisoned");
    if let Some(hook) = hook {
        hooks.insert(key, hook);
    } else {
        hooks.remove(&key);
    }
}

#[cfg(test)]
pub(crate) fn transaction_entered(key: &str) {
    let hook = TRANSACTION_HOOKS
        .get_or_init(|| Mutex::new(BTreeMap::new()))
        .lock()
        .expect("transaction hook registry poisoned")
        .get(key)
        .cloned();
    if let Some(hook) = hook {
        hook();
    }
}

#[cfg(not(test))]
fn transaction_entered(_key: &str) {}

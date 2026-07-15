use super::*;
use crate::fakes::FakeKeychain;
use std::{
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, Ordering},
        mpsc,
    },
    thread,
};

#[test]
fn server_local_store_atomically_replaces_scoped_verifiers() {
    let root = std::env::temp_dir().join(format!(
        "bowline-server-local-verifier-replace-{}",
        std::process::id()
    ));
    let _ = std::fs::remove_dir_all(&root);
    let store = ServerLocalSecretStore::new(root.join("secrets.v1"));
    let workspace_a = WorkspaceId::new("workspace_a");
    let workspace_b = WorkspaceId::new("workspace_b");
    for (workspace_id, device_id, proof_verifier) in [
        (&workspace_a, "device_a", "dapv_old"),
        (&workspace_b, "device_b", "dapv_b"),
    ] {
        store
            .store_device_proof_verifier(DeviceProofVerifier {
                workspace_id: workspace_id.clone(),
                device_id: DeviceId::new(device_id),
                proof_verifier: proof_verifier.to_string(),
            })
            .expect("seed verifier");
    }
    store
        .replace_device_proof_verifiers_for_workspace(
            &workspace_a,
            vec![DeviceProofVerifier {
                workspace_id: workspace_a.clone(),
                device_id: DeviceId::new("device_a"),
                proof_verifier: "dapv_replaced".to_string(),
            }],
        )
        .expect("replace workspace a");
    let replaced = store.load_device_proof_verifiers().expect("replacement");
    assert!(replaced.iter().any(|verifier| {
        verifier.workspace_id == workspace_a && verifier.proof_verifier == "dapv_replaced"
    }));
    assert!(
        replaced
            .iter()
            .any(|verifier| verifier.workspace_id == workspace_b)
    );

    store
        .replace_device_proof_verifiers_for_workspace(&workspace_a, Vec::new())
        .expect("revoke workspace a");
    let revoked = store.load_device_proof_verifiers().expect("revocation");
    assert!(
        revoked
            .iter()
            .all(|verifier| verifier.workspace_id != workspace_a)
    );
    assert!(
        revoked
            .iter()
            .any(|verifier| verifier.workspace_id == workspace_b)
    );
    let _ = std::fs::remove_dir_all(root);
}

fn verifier(workspace: &str, device: &str) -> DeviceProofVerifier {
    DeviceProofVerifier {
        workspace_id: WorkspaceId::new(workspace),
        device_id: DeviceId::new(device),
        proof_verifier: format!("dapv_{device}"),
    }
}

#[test]
fn concurrent_server_local_handles_preserve_different_workspaces_and_temp_safety() {
    let root = std::env::temp_dir().join(format!(
        "bowline-verifier-concurrent-{}",
        std::process::id()
    ));
    let _ = std::fs::remove_dir_all(&root);
    let path = root.join("secrets.v1");
    let handles = [("workspace_a", "device_a"), ("workspace_b", "device_b")]
        .into_iter()
        .enumerate()
        .map(|(index, (workspace, device))| {
            let store = ServerLocalSecretStore::new(&path);
            thread::spawn(move || {
                if index == 0 {
                    return store.store_device_proof_verifier(verifier(workspace, device));
                }
                store.replace_device_proof_verifiers_for_workspace(
                    &WorkspaceId::new(workspace),
                    vec![verifier(workspace, device)],
                )
            })
        })
        .collect::<Vec<_>>();
    for handle in handles {
        handle
            .join()
            .expect("replacement thread")
            .expect("replacement");
    }
    let persisted = ServerLocalSecretStore::new(&path)
        .load_device_proof_verifiers()
        .expect("persisted verifiers");
    assert_eq!(persisted.len(), 2);
    let final_store = ServerLocalSecretStore::new(&path);
    final_store
        .replace_device_proof_verifiers_for_workspace(&WorkspaceId::new("workspace_a"), Vec::new())
        .expect("final revocation");
    assert!(
        final_store
            .load_device_proof_verifiers()
            .expect("revoked")
            .iter()
            .all(|v| v.workspace_id.as_str() != "workspace_a")
    );
    assert!(std::fs::read_dir(&root).expect("secret dir").all(|entry| {
        entry
            .expect("secret entry")
            .path()
            .file_name()
            .is_some_and(|name| name == "secrets.v1")
    }));
    let _ = std::fs::remove_dir_all(root);
}

#[test]
fn concurrent_fake_handles_preserve_different_workspaces() {
    let store = FakeKeychain::default();
    let handles = [("workspace_a", "device_a"), ("workspace_b", "device_b")]
        .into_iter()
        .enumerate()
        .map(|(index, (workspace, device))| {
            let store = store.clone();
            thread::spawn(move || {
                if index == 0 {
                    return store.store_device_proof_verifier(verifier(workspace, device));
                }
                store.replace_device_proof_verifiers_for_workspace(
                    &WorkspaceId::new(workspace),
                    vec![verifier(workspace, device)],
                )
            })
        })
        .collect::<Vec<_>>();
    for handle in handles {
        handle
            .join()
            .expect("replacement thread")
            .expect("replacement");
    }
    assert_eq!(
        store
            .load_device_proof_verifiers()
            .expect("verifiers")
            .len(),
        2
    );
    store
        .replace_device_proof_verifiers_for_workspace(&WorkspaceId::new("workspace_a"), Vec::new())
        .expect("final revocation");
    assert!(
        store
            .load_device_proof_verifiers()
            .expect("revoked")
            .iter()
            .all(|v| v.workspace_id.as_str() != "workspace_a")
    );
}

#[test]
fn keyring_backend_lock_seam_serializes_same_backing_key() {
    let first =
        verifier_replacement_lock("keyring:test:verifiers".to_string()).expect("first lock");
    let second =
        verifier_replacement_lock("keyring:test:verifiers".to_string()).expect("second lock");
    let other =
        verifier_replacement_lock("keyring:other:verifiers".to_string()).expect("other lock");
    assert!(Arc::ptr_eq(&first, &second));
    assert!(!Arc::ptr_eq(&first, &other));
}

fn run_ordered_transactions(
    key: String,
    first: impl FnOnce() -> Result<(), DeviceKeyError> + Send + 'static,
    second: impl FnOnce() -> Result<(), DeviceKeyError> + Send + 'static,
) {
    let (entered_sender, entered_receiver) = mpsc::sync_channel(0);
    let (release_sender, release_receiver) = mpsc::sync_channel(0);
    let release_receiver = Mutex::new(release_receiver);
    let first_entry = Arc::new(AtomicBool::new(true));
    set_transaction_hook(
        key.clone(),
        Some(Arc::new(move || {
            if first_entry.swap(false, Ordering::SeqCst) {
                entered_sender.send(()).expect("signal transaction entry");
                release_receiver
                    .lock()
                    .expect("release receiver")
                    .recv()
                    .expect("release transaction");
            }
        })),
    );
    let first_handle = thread::spawn(first);
    entered_receiver.recv().expect("first transaction entered");
    let second_handle = thread::spawn(second);
    release_sender.send(()).expect("release first transaction");
    first_handle
        .join()
        .expect("first transaction thread")
        .expect("first transaction");
    second_handle
        .join()
        .expect("second transaction thread")
        .expect("second transaction");
    set_transaction_hook(key, None);
}

fn assert_accepted_verifier_present(verifiers: &[DeviceProofVerifier], expected: bool) {
    assert_eq!(
        verifiers.iter().any(|candidate| {
            candidate.workspace_id.as_str() == "workspace_same"
                && candidate.device_id.as_str() == "device_accepted"
        }),
        expected
    );
}

#[test]
fn server_local_same_workspace_mutations_linearize_in_both_orders() {
    let root = std::env::temp_dir().join(format!(
        "bowline-verifier-linearized-{}",
        std::process::id()
    ));
    let _ = std::fs::remove_dir_all(&root);
    let path = root.join("secrets.v1");
    let key = format!("file:{}", path.display());
    let first = ServerLocalSecretStore::new(&path);
    let second = ServerLocalSecretStore::new(&path);
    run_ordered_transactions(
        key.clone(),
        move || {
            first.replace_device_proof_verifiers_for_workspace(
                &WorkspaceId::new("workspace_same"),
                Vec::new(),
            )
        },
        move || second.store_device_proof_verifier(verifier("workspace_same", "device_accepted")),
    );
    let persisted = ServerLocalSecretStore::new(&path);
    assert_accepted_verifier_present(
        &persisted.load_device_proof_verifiers().expect("load"),
        true,
    );

    let first = ServerLocalSecretStore::new(&path);
    let second = ServerLocalSecretStore::new(&path);
    run_ordered_transactions(
        key,
        move || first.store_device_proof_verifier(verifier("workspace_same", "device_accepted")),
        move || {
            second.replace_device_proof_verifiers_for_workspace(
                &WorkspaceId::new("workspace_same"),
                Vec::new(),
            )
        },
    );
    assert_accepted_verifier_present(
        &persisted.load_device_proof_verifiers().expect("load"),
        false,
    );
    let _ = std::fs::remove_dir_all(root);
}

#[test]
fn fake_keychain_same_workspace_mutations_linearize_in_both_orders() {
    let store = FakeKeychain::default();
    let key = store.verifier_transaction_key();
    let first = store.clone();
    let second = store.clone();
    run_ordered_transactions(
        key.clone(),
        move || {
            first.replace_device_proof_verifiers_for_workspace(
                &WorkspaceId::new("workspace_same"),
                Vec::new(),
            )
        },
        move || second.store_device_proof_verifier(verifier("workspace_same", "device_accepted")),
    );
    assert_accepted_verifier_present(&store.load_device_proof_verifiers().expect("load"), true);

    let first = store.clone();
    let second = store.clone();
    run_ordered_transactions(
        key,
        move || first.store_device_proof_verifier(verifier("workspace_same", "device_accepted")),
        move || {
            second.replace_device_proof_verifiers_for_workspace(
                &WorkspaceId::new("workspace_same"),
                Vec::new(),
            )
        },
    );
    assert_accepted_verifier_present(&store.load_device_proof_verifiers().expect("load"), false);
}

#[test]
fn keyring_lock_seam_same_workspace_mutations_linearize_in_both_orders() {
    let key = "keyring:test:linearized".to_string();
    let persisted = Arc::new(Mutex::new(Vec::<DeviceProofVerifier>::new()));
    let replace = |state: Arc<Mutex<Vec<DeviceProofVerifier>>>, key: String| {
        move || {
            with_verifier_transaction(key, || {
                state
                    .lock()
                    .expect("keyring seam state")
                    .retain(|candidate| candidate.workspace_id.as_str() != "workspace_same");
                Ok(())
            })
        }
    };
    let upsert = |state: Arc<Mutex<Vec<DeviceProofVerifier>>>, key: String| {
        move || {
            with_verifier_transaction(key, || {
                upsert_device_proof_verifier(
                    &mut state.lock().expect("keyring seam state"),
                    verifier("workspace_same", "device_accepted"),
                );
                Ok(())
            })
        }
    };
    run_ordered_transactions(
        key.clone(),
        replace(persisted.clone(), key.clone()),
        upsert(persisted.clone(), key.clone()),
    );
    assert_accepted_verifier_present(&persisted.lock().expect("state"), true);
    run_ordered_transactions(
        key.clone(),
        upsert(persisted.clone(), key.clone()),
        replace(persisted.clone(), key),
    );
    assert_accepted_verifier_present(&persisted.lock().expect("state"), false);
}

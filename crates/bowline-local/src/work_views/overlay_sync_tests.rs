use bowline_core::ids::{PackId, WorkspaceId};
use bowline_storage::{ByteStoreError, ObjectKey};

use super::*;

#[test]
fn overlay_pack_content_ids_are_workspace_keyed_and_never_raw_hashes() {
    let workspace_id = WorkspaceId::new("ws_overlay_content_id");
    let payload = br#"{"bytes":"same secret"}"#;
    let first = derive_overlay_payload_pack(
        &workspace_id,
        payload,
        workspace_content_id([7_u8; 32], payload),
        StorageKey::deterministic(1),
        1,
    )
    .expect("first pack");
    let second = derive_overlay_payload_pack(
        &workspace_id,
        payload,
        workspace_content_id([8_u8; 32], payload),
        StorageKey::deterministic(2),
        1,
    )
    .expect("second pack");
    let first_content_id = overlay_pack_payload_content_id(&first).expect("content id");
    let second_content_id = overlay_pack_payload_content_id(&second).expect("content id");

    assert_eq!(
        first_content_id,
        workspace_content_id([7_u8; 32], payload).as_str()
    );
    assert_ne!(first_content_id, second_content_id);
    assert_ne!(
        first_content_id,
        format!("overlay_{}", blake3::hash(payload).to_hex())
    );
}

#[test]
fn corrupt_overlay_is_attention_but_transport_failure_remains_retryable() {
    assert!(overlay_failure_requires_attention(
        &super::super::overlay_wire::OverlayWireError::InvalidContentLayout.into()
    ));
    assert!(overlay_failure_requires_attention(
        &ByteStoreError::CorruptObject {
            key: ObjectKey::from_pack_id(&PackId::new("pk_0011223344556677")).expect("key"),
            reason: "fixture corruption",
        }
        .into()
    ));
    assert!(!overlay_failure_requires_attention(
        &ByteStoreError::Network {
            operation: bowline_storage::TransferOperation::Download,
            detail: "offline".to_string(),
        }
        .into()
    ));
}

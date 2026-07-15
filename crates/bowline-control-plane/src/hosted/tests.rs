use super::generated::{
    HostedRefsListWorkspaceRefHistoryRequest, HostedWorkspaceRef, RefsListWorkspaceRefHistory,
};
use super::*;
use crate::{ObjectControlPlaneClient, WorkspaceControlPlaneClient, device_authorization_message};
use bowline_core::ids::{DeviceId, EventId, LeaseId, ProjectId, SnapshotId, WorkspaceId};
use p256::ecdsa::{Signature, SigningKey, signature::Signer};

#[test]
fn generated_object_keys_preserve_shape_and_change_with_seed() {
    let first = generated_object_key(ObjectKind::SourcePack, "workspace:device:1");
    let second = generated_object_key(ObjectKind::SourcePack, "workspace:device:2");
    let manifest = generated_object_key(ObjectKind::SnapshotManifest, "workspace:device:1");
    let overlay = generated_object_key(ObjectKind::AgentOverlay, "workspace:device:1");

    assert_ne!(first, second);
    assert!(first.starts_with("packs_pk_"));
    assert!(manifest.starts_with("manifests_mf_"));
    assert!(overlay.starts_with("packs_pk_"));
    assert!(StorageObjectKey::new(first).is_ok());
    assert!(StorageObjectKey::new(second).is_ok());
    assert!(StorageObjectKey::new(manifest).is_ok());
    assert!(StorageObjectKey::new(overlay).is_ok());
}

#[test]
fn hosted_boundary_verifies_workspace_ref_signed_head() {
    let signing_key = SigningKey::from_slice(&[7_u8; 32]).expect("test signing key");
    let verifier = format!(
        "dapv_p256_v1_{}",
        BASE64_URL.encode(
            signing_key
                .verifying_key()
                .to_encoded_point(false)
                .as_bytes()
        )
    );
    let signature = sign_workspace_head(&signing_key, "workspace_1", "device_1", 3, "snap_signed");

    let parsed = workspace_ref_from_dto(
        signed_workspace_ref_dto("device_1", 3, "snap_signed", &signature),
        |_, device_id| {
            if device_id == "device_1" {
                Ok(Some(verifier.clone()))
            } else {
                Ok(None)
            }
        },
    )
    .expect("signed head verifies");
    assert_eq!(parsed.version, 3);
    assert_eq!(parsed.snapshot_id, SnapshotId::new("snap_signed"));
    assert_eq!(
        parsed
            .updated_by_device_id
            .as_ref()
            .map(|device_id| device_id.as_str()),
        Some("device_1")
    );

    // A tampered snapshot id (the signature was made over snap_signed) must fail
    // the head-signature verification the DTO boundary runs.
    let forged = workspace_ref_from_dto(
        signed_workspace_ref_dto("device_1", 3, "snap_forged", &signature),
        |_, device_id| {
            if device_id == "device_1" {
                Ok(Some(verifier.clone()))
            } else {
                Ok(None)
            }
        },
    );
    assert!(forged.is_err());

    // An advanced (version > 0) ref that carries no head signature is rejected.
    let unsigned_advanced = HostedWorkspaceRef {
        workspace_id: "workspace_1".to_string(),
        version: 3,
        snapshot_id: "snap_unsigned".to_string(),
        updated_at: "2026-07-02T12:00:00Z".to_string(),
        updated_by_device_id: Some("device_1".to_string()),
        head_signature: None,
    };
    assert!(workspace_ref_from_dto(unsigned_advanced, |_, _| Ok(None)).is_err());
}

#[test]
fn hosted_boundary_rejects_unsigned_non_empty_genesis_ref() {
    let empty_genesis = HostedWorkspaceRef {
        workspace_id: "workspace_1".to_string(),
        version: 0,
        snapshot_id: "empty".to_string(),
        updated_at: "2026-07-02T12:00:00Z".to_string(),
        updated_by_device_id: None,
        head_signature: None,
    };
    let parsed = workspace_ref_from_dto(empty_genesis, |_, _| Ok(None)).expect("empty genesis");
    assert_eq!(parsed.version, 0);
    assert_eq!(parsed.snapshot_id, SnapshotId::new("empty"));

    let forged_genesis = HostedWorkspaceRef {
        workspace_id: "workspace_1".to_string(),
        version: 0,
        snapshot_id: "snap_unsigned".to_string(),
        updated_at: "2026-07-02T12:00:00Z".to_string(),
        updated_by_device_id: None,
        head_signature: None,
    };
    assert!(workspace_ref_from_dto(forged_genesis, |_, _| Ok(None)).is_err());
}

fn sign_workspace_head(
    signing_key: &SigningKey,
    workspace_id: &str,
    device_id: &str,
    version: u64,
    snapshot_id: &str,
) -> String {
    let subject = workspace_head_proof_subject(workspace_id, version, snapshot_id);
    let signature: Signature = signing_key.sign(&device_authorization_message(&[
        "bowline device authorization proof v2",
        workspace_id,
        device_id,
        "sign-workspace-head",
        &subject,
    ]));
    format!("dapp_p256_v1_{}", BASE64_URL.encode(signature.to_bytes()))
}

fn signed_workspace_ref_dto(
    device_id: &str,
    version: u64,
    snapshot_id: &str,
    head_signature: &str,
) -> HostedWorkspaceRef {
    HostedWorkspaceRef {
        workspace_id: "workspace_1".to_string(),
        version,
        snapshot_id: snapshot_id.to_string(),
        updated_at: "2026-07-02T12:00:00Z".to_string(),
        updated_by_device_id: Some(device_id.to_string()),
        head_signature: Some(head_signature.to_string()),
    }
}

#[test]
fn bootstrap_session_proof_subject_binds_bootstrap_token_hash() {
    let input = BootstrapSessionInput {
        workspace_id: WorkspaceId::new("workspace_1"),
        host: Some("mac-mini".to_string()),
        lease_handoff_digest: Some("lease_handoff_blake3:def456".to_string()),
        lease_id: Some(LeaseId::new("lease_remote_1")),
        root: Some("/workspace/Code".to_string()),
        runtime: Some("codex-cloud".to_string()),
        setup_receipts_digest: Some("setup_receipts_blake3:abc123".to_string()),
        expires_in_ticks: 900,
    };

    assert_eq!(
        bootstrap_session_proof_subject(&input, "sha256:token_hash_1"),
        [
            "workspaceId=workspace_1",
            "host=mac-mini",
            "leaseHandoffDigest=lease_handoff_blake3:def456",
            "leaseId=lease_remote_1",
            "root=/workspace/Code",
            "runtime=codex-cloud",
            "setupReceiptsDigest=setup_receipts_blake3:abc123",
            "expiresInTicks=900",
            "bootstrapTokenHash=sha256:token_hash_1",
        ]
        .join("\n")
    );
}

#[test]
fn hosted_object_intent_actions_preserve_server_expires_at() {
    let client = HostedControlPlaneClient::try_new_with_token(
        "https://example.convex.cloud",
        "test-control-plane-token",
    )
    .expect("client")
    .with_device_id("device_1")
    .with_device_proof_signer(|_, _, action, subject| {
        assert!(!action.is_empty());
        assert!(!subject.is_empty());
        Ok("proof_1".to_string())
    })
    .with_public_action_override(|name, action_args| match name {
        "objects:createUploadIntent" => {
            assert_eq!(
                action_args.get("objectKey"),
                Some(&Value::from("object_upload"))
            );
            assert_eq!(
                action_args.get("createdByDeviceProof"),
                Some(&Value::from("proof_1"))
            );
            Ok(Value::Object(args([
                ("byteLength", number_value(128)),
                ("expiresAt", Value::from("2026-06-23T12:00:11Z")),
                ("intentId", Value::from("intent_upload")),
                ("kind", Value::from("source-pack")),
                ("method", Value::from("PUT")),
                ("objectKey", Value::from("object_upload")),
                ("signedUrl", Value::from("https://storage.example/upload")),
                ("workspaceId", Value::from("workspace_1")),
            ])))
        }
        "objects:createDownloadIntent" => {
            assert_eq!(action_args.get("offset"), Some(&number_value(4)));
            assert_eq!(action_args.get("length"), Some(&number_value(8)));
            assert_eq!(
                action_args.get("requestedByDeviceProof"),
                Some(&Value::from("proof_1"))
            );
            Ok(Value::Object(args([
                ("expiresAt", Value::from("2026-06-23T12:00:12Z")),
                ("intentId", Value::from("intent_download")),
                ("method", Value::from("GET")),
                ("objectKey", Value::from("object_download")),
                ("signedUrl", Value::from("https://storage.example/download")),
                ("workspaceId", Value::from("workspace_1")),
            ])))
        }
        "objects:createUploadVerificationIntent" => {
            assert_eq!(
                action_args.get("contentId"),
                Some(&Value::from("cid_verify"))
            );
            Ok(Value::Object(args([
                ("byteLength", number_value(256)),
                ("expiresAt", Value::from("2026-06-23T12:00:13Z")),
                ("intentId", Value::from("intent_verify")),
                ("method", Value::from("GET")),
                ("objectKey", Value::from("object_verify")),
                ("requestedByDeviceId", Value::from("device_1")),
                ("signedUrl", Value::from("https://storage.example/verify")),
                ("workspaceId", Value::from("workspace_1")),
            ])))
        }
        unexpected => Err(ControlPlaneError::Storage(format!(
            "unexpected action {unexpected}"
        ))),
    });

    let upload = client
        .create_upload_intent(
            UploadIntentRequest::new("workspace_1", ObjectKind::SourcePack, 128)
                .with_object_key("object_upload"),
        )
        .expect("upload intent");
    assert_eq!(upload.signed_url.expires_at.tick, 1782216011000);

    let download = client
        .create_download_intent(DownloadIntentRequest {
            workspace_id: WorkspaceId::new("workspace_1"),
            object_key: "object_download".to_string(),
            range: Some(bowline_storage::ByteRange::new(4, 8)),
        })
        .expect("download intent");
    assert_eq!(download.signed_url.expires_at.tick, 1782216012000);

    let verification = client
        .create_upload_verification_intent(
            UploadVerificationIntentRequest::new("workspace_1", "object_verify", 256)
                .with_content_id("cid_verify"),
        )
        .expect("verification intent");
    assert_eq!(verification.signed_url.expires_at.tick, 1782216013000);
}

#[test]
fn account_session_cache_reuses_unexpired_session() {
    let client = HostedControlPlaneClient::try_new_with_token(
        "https://example.convex.cloud",
        "test-control-plane-token",
    )
    .expect("client");
    client.account_session_cache.lock().expect("cache").insert(
        account_session_cache_key(Some("workspace_1")),
        CachedAccountSession {
            session_id: "session_cached".to_string(),
            revocation_token: "revoke_cached".to_string(),
            expires_at_unix: OffsetDateTime::now_utc().unix_timestamp() + 600,
        },
    );

    assert_eq!(
        client.cached_account_session_id(&account_session_cache_key(Some("workspace_1"))),
        Some("session_cached".to_string())
    );
}

#[test]
fn account_session_cache_ignores_expired_session() {
    let client = HostedControlPlaneClient::try_new_with_token(
        "https://example.convex.cloud",
        "test-control-plane-token",
    )
    .expect("client");
    client.account_session_cache.lock().expect("cache").insert(
        account_session_cache_key(Some("workspace_1")),
        CachedAccountSession {
            session_id: "session_expired".to_string(),
            revocation_token: "revoke_expired".to_string(),
            expires_at_unix: OffsetDateTime::now_utc().unix_timestamp() + 10,
        },
    );

    assert_eq!(
        client.cached_account_session_id(&account_session_cache_key(Some("workspace_1"))),
        None
    );
}

#[derive(Clone, Debug)]
struct TestCachedRpcClient {
    connection_id: u64,
}

#[test]
fn cached_rpc_reconnects_once_after_transport_failure() {
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("test runtime");
    let cached_client = TokioMutex::new(None);
    let connect_count = Arc::new(AtomicU64::new(0));
    let call_count = Arc::new(AtomicU64::new(0));

    let result = runtime
        .block_on(rpc_with_cached_client(
            &cached_client,
            true,
            {
                let connect_count = Arc::clone(&connect_count);
                move || {
                    let connect_count = Arc::clone(&connect_count);
                    async move {
                        let connection_id = connect_count.fetch_add(1, Ordering::SeqCst) + 1;
                        Ok(TestCachedRpcClient { connection_id })
                    }
                }
            },
            {
                let call_count = Arc::clone(&call_count);
                move |client| {
                    let call_count = Arc::clone(&call_count);
                    Box::pin(async move {
                        if call_count.fetch_add(1, Ordering::SeqCst) == 0 {
                            return Err(ControlPlaneError::Storage(
                                "transport unavailable".to_string(),
                            ));
                        }
                        Ok(FunctionResult::Value(Value::from(format!(
                            "connection-{}",
                            client.connection_id
                        ))))
                    })
                }
            },
        ))
        .expect("retry succeeds");

    assert_eq!(result, Value::from("connection-2"));
    assert_eq!(connect_count.load(Ordering::SeqCst), 2);
    assert_eq!(call_count.load(Ordering::SeqCst), 2);
    let cached_connection_id = runtime.block_on(async {
        cached_client
            .lock()
            .await
            .as_ref()
            .map(|client| client.connection_id)
    });
    assert_eq!(cached_connection_id, Some(2));
}

#[test]
fn cached_rpc_does_not_replay_non_retryable_transport_failure() {
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("test runtime");
    let cached_client = TokioMutex::new(None);
    let connect_count = Arc::new(AtomicU64::new(0));
    let call_count = Arc::new(AtomicU64::new(0));

    let error = runtime
        .block_on(rpc_with_cached_client(
            &cached_client,
            false,
            {
                let connect_count = Arc::clone(&connect_count);
                move || {
                    let connect_count = Arc::clone(&connect_count);
                    async move {
                        let connection_id = connect_count.fetch_add(1, Ordering::SeqCst) + 1;
                        Ok(TestCachedRpcClient { connection_id })
                    }
                }
            },
            {
                let call_count = Arc::clone(&call_count);
                move |_client| {
                    let call_count = Arc::clone(&call_count);
                    Box::pin(async move {
                        call_count.fetch_add(1, Ordering::SeqCst);
                        Err(ControlPlaneError::Storage(
                            "transport may have applied mutation".to_string(),
                        ))
                    })
                }
            },
        ))
        .expect_err("non-retryable calls surface the first transport error");

    assert!(
        error
            .to_string()
            .contains("transport may have applied mutation")
    );
    assert_eq!(connect_count.load(Ordering::SeqCst), 1);
    assert_eq!(call_count.load(Ordering::SeqCst), 1);
    let cached_connection_id = runtime.block_on(async {
        cached_client
            .lock()
            .await
            .as_ref()
            .map(|client| client.connection_id)
    });
    assert_eq!(cached_connection_id, None);
}

#[test]
fn cached_rpc_timeout_drops_cached_client() {
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("test runtime");
    let cached_client = TokioMutex::new(None);
    let connect_count = Arc::new(AtomicU64::new(0));
    let call_count = Arc::new(AtomicU64::new(0));

    let error = runtime
        .block_on(rpc_with_cached_client_after(
            &cached_client,
            false,
            // 250ms, not 1ms: under load a 1ms timeout can fire before the
            // connect future runs, leaving connect_count at 0 (flaky). The
            // call sleeps 60s, so the timeout still lands mid-call.
            std::time::Duration::from_millis(250),
            {
                let connect_count = Arc::clone(&connect_count);
                move || {
                    let connect_count = Arc::clone(&connect_count);
                    async move {
                        let connection_id = connect_count.fetch_add(1, Ordering::SeqCst) + 1;
                        Ok(TestCachedRpcClient { connection_id })
                    }
                }
            },
            {
                let call_count = Arc::clone(&call_count);
                move |_client| {
                    let call_count = Arc::clone(&call_count);
                    Box::pin(async move {
                        call_count.fetch_add(1, Ordering::SeqCst);
                        tokio::time::sleep(std::time::Duration::from_secs(60)).await;
                        Ok(FunctionResult::Value(Value::from("late response")))
                    })
                }
            },
        ))
        .expect_err("timed out RPCs fail");

    assert!(error.to_string().contains("timed out"));
    assert_eq!(connect_count.load(Ordering::SeqCst), 1);
    assert_eq!(call_count.load(Ordering::SeqCst), 1);
    let cached_connection_id = runtime.block_on(async {
        cached_client
            .lock()
            .await
            .as_ref()
            .map(|client| client.connection_id)
    });
    assert_eq!(cached_connection_id, None);
}

#[test]
fn cached_rpc_timeout_does_not_include_another_call_duration() {
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("test runtime");
    let cached_client = TokioMutex::new(Some(TestCachedRpcClient { connection_id: 7 }));

    let (slow_result, fast_result) = runtime.block_on(async {
        let slow = rpc_with_cached_client_after(
            &cached_client,
            false,
            std::time::Duration::from_millis(5),
            || async { Ok(TestCachedRpcClient { connection_id: 99 }) },
            |client| {
                Box::pin(async move {
                    tokio::time::sleep(std::time::Duration::from_millis(50)).await;
                    Ok(FunctionResult::Value(Value::from(format!(
                        "slow-{}",
                        client.connection_id
                    ))))
                })
            },
        );
        let fast = rpc_with_cached_client_after(
            &cached_client,
            false,
            std::time::Duration::from_millis(20),
            || async { Ok(TestCachedRpcClient { connection_id: 99 }) },
            |client| {
                Box::pin(async move {
                    Ok(FunctionResult::Value(Value::from(format!(
                        "fast-{}",
                        client.connection_id
                    ))))
                })
            },
        );
        tokio::join!(slow, fast)
    });

    assert!(
        slow_result
            .expect_err("slow call times out")
            .to_string()
            .contains("timed out")
    );
    assert_eq!(
        fast_result.expect("fast call completes"),
        Value::from("fast-7")
    );
}

#[test]
fn cached_rpc_keeps_client_after_convex_function_error() {
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("test runtime");
    let cached_client = TokioMutex::new(None);
    let connect_count = Arc::new(AtomicU64::new(0));
    let call_count = Arc::new(AtomicU64::new(0));

    let function_error = runtime
        .block_on(rpc_with_cached_client(
            &cached_client,
            true,
            {
                let connect_count = Arc::clone(&connect_count);
                move || {
                    let connect_count = Arc::clone(&connect_count);
                    async move {
                        let connection_id = connect_count.fetch_add(1, Ordering::SeqCst) + 1;
                        Ok(TestCachedRpcClient { connection_id })
                    }
                }
            },
            {
                let call_count = Arc::clone(&call_count);
                move |client| {
                    let call_count = Arc::clone(&call_count);
                    Box::pin(async move {
                        if call_count.fetch_add(1, Ordering::SeqCst) == 0 {
                            return Ok(FunctionResult::ConvexError(ConvexError {
                                message: "application rejected the call".to_string(),
                                data: Value::Null,
                            }));
                        }
                        Ok(FunctionResult::Value(Value::from(format!(
                            "connection-{}",
                            client.connection_id
                        ))))
                    })
                }
            },
        ))
        .expect_err("function errors propagate");

    assert!(matches!(
        function_error,
        ControlPlaneError::Rejected {
            code: RejectionCode::Unknown,
            ..
        }
    ));
    assert_eq!(connect_count.load(Ordering::SeqCst), 1);
    assert_eq!(call_count.load(Ordering::SeqCst), 1);

    let result = runtime
        .block_on(rpc_with_cached_client(
            &cached_client,
            true,
            {
                let connect_count = Arc::clone(&connect_count);
                move || {
                    let connect_count = Arc::clone(&connect_count);
                    async move {
                        let connection_id = connect_count.fetch_add(1, Ordering::SeqCst) + 1;
                        Ok(TestCachedRpcClient { connection_id })
                    }
                }
            },
            {
                let call_count = Arc::clone(&call_count);
                move |client| {
                    let call_count = Arc::clone(&call_count);
                    Box::pin(async move {
                        call_count.fetch_add(1, Ordering::SeqCst);
                        Ok(FunctionResult::Value(Value::from(format!(
                            "connection-{}",
                            client.connection_id
                        ))))
                    })
                }
            },
        ))
        .expect("cached client still works");

    assert_eq!(result, Value::from("connection-1"));
    assert_eq!(connect_count.load(Ordering::SeqCst), 1);
    assert_eq!(call_count.load(Ordering::SeqCst), 2);
}

#[test]
fn convex_error_payload_maps_to_rejection_code() {
    let mut payload = BTreeMap::new();
    payload.insert(
        "code".to_string(),
        Value::from("control_plane/device_not_trusted"),
    );
    payload.insert("message".to_string(), Value::from("device is not trusted"));

    let error = unwrap_function_result(FunctionResult::ConvexError(ConvexError {
        message: "application rejected the call".to_string(),
        data: Value::Object(payload),
    }))
    .expect_err("convex payload rejects");

    assert_eq!(
        error,
        ControlPlaneError::Rejected {
            code: RejectionCode::DeviceNotTrusted,
            message: "device is not trusted".to_string(),
        }
    );
}

#[test]
fn unauthorized_convex_error_maps_to_permanent_rejection_code() {
    let mut payload = BTreeMap::new();
    payload.insert(
        "code".to_string(),
        Value::from("control_plane/unauthorized"),
    );
    payload.insert(
        "message".to_string(),
        Value::from("device cannot update this lease"),
    );

    let error = unwrap_function_result(FunctionResult::ConvexError(ConvexError {
        message: "application rejected the call".to_string(),
        data: Value::Object(payload),
    }))
    .expect_err("unauthorized convex payload rejects");

    assert_eq!(
        error,
        ControlPlaneError::Rejected {
            code: RejectionCode::Unauthorized,
            message: "device cannot update this lease".to_string(),
        }
    );
}

#[test]
fn convex_error_payload_without_code_maps_to_unknown_rejection() {
    let error = unwrap_function_result(FunctionResult::ConvexError(ConvexError {
        message: "opaque application failure".to_string(),
        data: Value::Null,
    }))
    .expect_err("convex payload rejects");

    assert_eq!(
        error,
        ControlPlaneError::Rejected {
            code: RejectionCode::Unknown,
            message: "\"opaque application failure\"".to_string(),
        }
    );
}

#[test]
fn malformed_convex_error_payloads_map_to_unknown_rejection() {
    let mut missing_code = BTreeMap::new();
    missing_code.insert("message".to_string(), Value::from("missing code"));
    let error = unwrap_function_result(FunctionResult::ConvexError(ConvexError {
        message: "application rejected the call".to_string(),
        data: Value::Object(missing_code),
    }))
    .expect_err("convex payload rejects");
    assert!(matches!(
        error,
        ControlPlaneError::Rejected {
            code: RejectionCode::Unknown,
            ..
        }
    ));

    let mut missing_message = BTreeMap::new();
    missing_message.insert(
        "code".to_string(),
        Value::from("control_plane/device_not_trusted"),
    );
    let error = unwrap_function_result(FunctionResult::ConvexError(ConvexError {
        message: "application rejected the call".to_string(),
        data: Value::Object(missing_message),
    }))
    .expect_err("convex payload rejects");
    assert!(matches!(
        error,
        ControlPlaneError::Rejected {
            code: RejectionCode::Unknown,
            ..
        }
    ));
}

#[test]
fn account_session_registration_reuses_session_after_first_action() {
    let action_count = Arc::new(AtomicU64::new(0));
    let counted_actions = Arc::clone(&action_count);
    let client = HostedControlPlaneClient::try_new_with_token(
        "https://example.convex.cloud",
        "test-control-plane-token",
    )
    .expect("client")
    .with_public_action_override(move |name, action_args| {
        assert_eq!(name, "auth:registerAccountSession");
        assert_eq!(
            action_args.get("accessToken"),
            Some(&Value::from("access_token_1"))
        );
        assert_eq!(
            action_args.get("workspaceId"),
            Some(&Value::from("workspace_1"))
        );
        counted_actions.fetch_add(1, Ordering::SeqCst);
        Ok(Value::Object(args([
            ("revocationToken", Value::from("revocation_registered")),
            ("sessionId", Value::from("session_registered")),
        ])))
    });

    assert_eq!(
        client
            .register_account_session("access_token_1", Some("workspace_1"))
            .expect("first registration"),
        RegisteredAccountSession {
            session_id: "session_registered".to_string(),
            revocation_token: "revocation_registered".to_string(),
        }
    );
    assert_eq!(
        client
            .register_account_session("access_token_1", Some("workspace_1"))
            .expect("cached registration"),
        RegisteredAccountSession {
            session_id: "session_registered".to_string(),
            revocation_token: "revocation_registered".to_string(),
        }
    );
    assert_eq!(action_count.load(Ordering::SeqCst), 1);
}

#[test]
fn account_session_revocation_uses_dedicated_proof() {
    let client = HostedControlPlaneClient::try_new_with_token("https://example.convex.cloud", "")
        .expect("hosted client")
        .with_rpc_override(|kind, name, args| {
            assert_eq!(kind, ConvexRpcKind::Action);
            assert_eq!(name, "auth:revokeAccountSession");
            assert_eq!(
                args.get("revocationToken"),
                Some(&Value::from("bowline_revoke_test".to_string()))
            );
            assert_eq!(
                args.get("sessionId"),
                Some(&Value::from("bowline_session_test".to_string()))
            );
            assert_eq!(args.len(), 2);
            Ok(Value::Null)
        });

    client
        .revoke_account_session("bowline_session_test", "bowline_revoke_test")
        .expect("session revocation");
}

#[test]
fn hosted_function_call_counts_are_process_local_and_low_cardinality() {
    reset_hosted_function_call_counts();

    record_hosted_function_call("refs:getWorkspaceRef");
    record_hosted_function_call("refs:getWorkspaceRef");
    record_hosted_function_call("objects:createDownloadIntent");

    assert_eq!(
        hosted_function_call_counts(),
        vec![
            HostedFunctionCallCount {
                function_name: "objects:createDownloadIntent".to_string(),
                call_count: 1,
            },
            HostedFunctionCallCount {
                function_name: "refs:getWorkspaceRef".to_string(),
                call_count: 2,
            },
        ]
    );
}

#[test]
fn workspace_ref_stream_shutdown_wakes_a_blocked_owner() {
    let (shutdown, cancellation) = workspace_ref_stream_shutdown_pair();
    let worker = std::thread::spawn(move || futures::executor::block_on(cancellation.0));

    drop(shutdown);

    assert_eq!(
        worker.join().expect("shutdown observer joins"),
        Ok(()),
        "owned subscription cancellation resolves without polling"
    );
}

// Routing-only markers: they reuse the refs schemas/DTOs so the spine's contract
// validation resolves, and exercise each ConvexRpcKind through `call`.
struct RefsHistoryQueryRoute;
struct RefsHistoryMutationRoute;
struct RefsHistoryActionRoute;

macro_rules! refs_history_route {
    ($marker:ty, $id:literal, $function:literal, $kind:expr) => {
        impl HostedEndpoint for $marker {
            const ID: &'static str = $id;
            const CONVEX_FUNCTION: &'static str = $function;
            const KIND: ConvexRpcKind = $kind;
            const REQUEST_SCHEMA: &'static str = "HostedRefsListWorkspaceRefHistoryRequest";
            const RESPONSE_SCHEMA: &'static str = "HostedRefsListWorkspaceRefHistoryResponse";

            type Request = HostedRefsListWorkspaceRefHistoryRequest;
            type Response = super::generated::HostedRefsListWorkspaceRefHistoryResponse;
        }
    };
}

refs_history_route!(
    RefsHistoryQueryRoute,
    "test.query",
    "test:query",
    ConvexRpcKind::Query
);
refs_history_route!(
    RefsHistoryMutationRoute,
    "test.mutation",
    "test:mutation",
    ConvexRpcKind::Mutation
);
refs_history_route!(
    RefsHistoryActionRoute,
    "test.action",
    "test:action",
    ConvexRpcKind::Action
);

fn valid_ref_history_request() -> HostedRefsListWorkspaceRefHistoryRequest {
    HostedRefsListWorkspaceRefHistoryRequest {
        workspace_id: "workspace_1".to_string(),
        limit: Some(100),
        auth_token: None,
        account_session_id: None,
    }
}

fn assert_refs_history_routing<E>(expected_kind: ConvexRpcKind, expected_function: &'static str)
where
    E: HostedEndpoint<
            Request = HostedRefsListWorkspaceRefHistoryRequest,
            Response = super::generated::HostedRefsListWorkspaceRefHistoryResponse,
        >,
{
    let client = HostedControlPlaneClient::try_new_with_token(
        "https://example.convex.cloud",
        "test-control-plane-token",
    )
    .expect("client")
    .with_rpc_override(move |kind, name, request| {
        assert_eq!(kind, expected_kind);
        assert_eq!(name, expected_function);
        assert_eq!(
            request.get("workspaceId"),
            Some(&Value::from("workspace_1"))
        );
        Ok(ref_history_wire_rows())
    });

    let response = client
        .call::<E>(&valid_ref_history_request())
        .expect("typed call succeeds");
    assert_eq!(response.len(), 2);
    assert_eq!(response[0].version, 2);
}

#[test]
fn typed_hosted_call_routes_each_endpoint_kind_and_decodes_response() {
    assert_refs_history_routing::<RefsHistoryQueryRoute>(ConvexRpcKind::Query, "test:query");
    assert_refs_history_routing::<RefsHistoryMutationRoute>(
        ConvexRpcKind::Mutation,
        "test:mutation",
    );
    assert_refs_history_routing::<RefsHistoryActionRoute>(ConvexRpcKind::Action, "test:action");
}

struct NonObjectRequestRoute;

impl HostedEndpoint for NonObjectRequestRoute {
    const ID: &'static str = "test.nonObjectRequest";
    const CONVEX_FUNCTION: &'static str = "test:nonObjectRequest";
    const KIND: ConvexRpcKind = ConvexRpcKind::Query;
    const REQUEST_SCHEMA: &'static str = "HostedRefsListWorkspaceRefHistoryRequest";
    const RESPONSE_SCHEMA: &'static str = "HostedRefsListWorkspaceRefHistoryResponse";

    type Request = String;
    type Response = super::generated::HostedRefsListWorkspaceRefHistoryResponse;
}

#[test]
fn typed_hosted_call_rejects_non_object_requests_before_invocation() {
    let client = HostedControlPlaneClient::try_new_with_token(
        "https://example.convex.cloud",
        "test-control-plane-token",
    )
    .expect("client")
    .with_rpc_override(|_, _, _| panic!("invalid request must not reach transport"));

    let error = client
        .call::<NonObjectRequestRoute>(&"not-an-object".to_string())
        .expect_err("non-object request rejects");
    let message = error.to_string();
    assert!(message.contains("test.nonObjectRequest"));
    assert!(message.contains("request did not match the declared contract"));
    assert!(!message.contains("not-an-object"));
}

#[test]
fn typed_hosted_call_reports_endpoint_context_without_response_payload() {
    let client = HostedControlPlaneClient::try_new_with_token(
        "https://example.convex.cloud",
        "test-control-plane-token",
    )
    .expect("client")
    .with_rpc_override(|_, _, _| {
        // An oversized workspaceId (> 256) carrying a secret-looking payload; the
        // response validator must reject it and surface only the field path.
        Ok(Value::Array(vec![Value::Object(args([
            (
                "workspaceId",
                Value::from("sensitive-payload-must-not-leak".repeat(20)),
            ),
            ("version", number_value(1)),
            ("baseSnapshotId", Value::from("snap_base")),
            ("targetSnapshotId", Value::from("snap_after")),
            ("occurredAt", Value::from("2026-06-23T12:00:00Z")),
        ]))]))
    });

    let error = client
        .call::<RefsHistoryQueryRoute>(&valid_ref_history_request())
        .expect_err("malformed response rejects");
    let message = error.to_string();
    assert!(message.contains("test.query"));
    assert!(message.contains("test:query"));
    assert!(message.contains("response did not match the declared contract"));
    assert!(!message.contains("sensitive-payload-must-not-leak"));
}

#[test]
fn refs_history_response_decodes_convex_float_version() {
    let wire = Value::Array(vec![Value::Object(args([
        ("workspaceId", Value::from("ws_code")),
        ("version", number_value(7)),
        ("baseSnapshotId", Value::from("snap_base")),
        ("targetSnapshotId", Value::from("snap_after")),
        ("occurredAt", Value::from("2026-06-23T12:00:00Z")),
    ]))]);
    let decoded = decode_hosted_response::<super::generated::RefsListWorkspaceRefHistory>(wire)
        .expect("decodes convex float version");
    assert_eq!(decoded.len(), 1);
    assert_eq!(decoded[0].version, 7);
}

#[test]
fn refs_history_request_encodes_limit_as_convex_float() {
    let request = super::generated::HostedRefsListWorkspaceRefHistoryRequest {
        workspace_id: "ws_code".to_string(),
        limit: Some(100),
        auth_token: None,
        account_session_id: None,
    };
    let encoded = encode_hosted_request::<super::generated::RefsListWorkspaceRefHistory>(&request)
        .expect("encodes");
    let limit = encoded.get("limit").expect("limit present").clone();
    assert_eq!(
        limit,
        Value::Float64(100.0),
        "convex v.number() requires Float64"
    );
    assert!(!encoded.contains_key("authToken"));
    assert!(!encoded.contains_key("accountSessionId"));
}

fn ref_history_wire_rows() -> Value {
    Value::Array(vec![
        Value::Object(args([
            ("workspaceId", Value::from("ws_code")),
            ("version", number_value(2)),
            ("baseSnapshotId", Value::from("snap_after")),
            ("targetSnapshotId", Value::from("snap_later")),
            ("occurredAt", Value::from("2026-06-23T12:00:02Z")),
            ("advancedByDeviceId", Value::from("dev_writer")),
            ("causedByEventId", Value::from("evt_2")),
            ("projectId", Value::from("proj_web")),
            // Additive Convex system field must be tolerated by the response DTO.
            ("_id", Value::from("row_2")),
        ])),
        Value::Object(args([
            ("workspaceId", Value::from("ws_code")),
            ("version", number_value(1)),
            ("baseSnapshotId", Value::from("snap_base")),
            ("targetSnapshotId", Value::from("snap_after")),
            ("occurredAt", Value::from("2026-06-23T12:00:01Z")),
        ])),
    ])
}

fn expected_ref_history_rows() -> Vec<WorkspaceRefHistoryRecord> {
    vec![
        WorkspaceRefHistoryRecord {
            workspace_id: WorkspaceId::new("ws_code"),
            version: 2,
            base_snapshot_id: SnapshotId::new("snap_after"),
            target_snapshot_id: SnapshotId::new("snap_later"),
            occurred_at: "2026-06-23T12:00:02Z".to_string(),
            advanced_by_device_id: Some(DeviceId::new("dev_writer")),
            caused_by_event_id: Some(EventId::new("evt_2")),
            project_id: Some(ProjectId::new("proj_web")),
        },
        WorkspaceRefHistoryRecord {
            workspace_id: WorkspaceId::new("ws_code"),
            version: 1,
            base_snapshot_id: SnapshotId::new("snap_base"),
            target_snapshot_id: SnapshotId::new("snap_after"),
            occurred_at: "2026-06-23T12:00:01Z".to_string(),
            advanced_by_device_id: None,
            caused_by_event_id: None,
            project_id: None,
        },
    ]
}

#[test]
fn list_workspace_ref_history_uses_account_session_and_decodes_records() {
    let client = HostedControlPlaneClient::try_new_with_token("https://example.convex.cloud", "")
        .expect("client")
        .with_account_session_id("acct_session_1")
        .with_rpc_override(|kind, name, request| {
            assert_eq!(kind, ConvexRpcKind::Query);
            assert_eq!(name, "refs:listWorkspaceRefHistory");
            assert_eq!(request.get("workspaceId"), Some(&Value::from("ws_code")));
            assert_eq!(request.get("limit"), Some(&Value::Float64(50.0)));
            assert_eq!(
                request.get("accountSessionId"),
                Some(&Value::from("acct_session_1"))
            );
            assert!(!request.contains_key("authToken"));
            Ok(ref_history_wire_rows())
        });

    let records = client
        .list_workspace_ref_history(&WorkspaceId::new("ws_code"), 50)
        .expect("history decodes");
    assert_eq!(records, expected_ref_history_rows());
}

#[test]
fn list_workspace_ref_history_falls_back_to_control_plane_token() {
    let client =
        HostedControlPlaneClient::try_new_with_token("https://example.convex.cloud", "cp-token")
            .expect("client")
            .with_rpc_override(|kind, name, request| {
                assert_eq!(kind, ConvexRpcKind::Query);
                assert_eq!(name, "refs:listWorkspaceRefHistory");
                assert_eq!(request.get("authToken"), Some(&Value::from("cp-token")));
                assert!(!request.contains_key("accountSessionId"));
                Ok(ref_history_wire_rows())
            });

    let records = client
        .list_workspace_ref_history(&WorkspaceId::new("ws_code"), 50)
        .expect("history decodes");
    assert_eq!(records, expected_ref_history_rows());
}

#[test]
fn list_workspace_ref_history_rejects_row_missing_base_snapshot() {
    let client =
        HostedControlPlaneClient::try_new_with_token("https://example.convex.cloud", "cp-token")
            .expect("client")
            .with_rpc_override(|_, _, _| {
                Ok(Value::Array(vec![Value::Object(args([
                    ("workspaceId", Value::from("ws_code")),
                    ("version", number_value(1)),
                    ("targetSnapshotId", Value::from("snap_after")),
                    ("occurredAt", Value::from("2026-06-23T12:00:01Z")),
                ]))]))
            });

    let error = client
        .list_workspace_ref_history(&WorkspaceId::new("ws_code"), 50)
        .expect_err("row missing baseSnapshotId rejects at the domain boundary");
    assert!(error.to_string().contains("baseSnapshotId"));
}

#[test]
fn fake_and_hosted_ref_history_agree_on_empty_domain_result() {
    let fake = crate::FakeControlPlaneClient::default();
    let fake_rows = fake
        .list_workspace_ref_history(&WorkspaceId::new("ws_code"), 50)
        .expect("fake history");
    assert!(fake_rows.is_empty());

    let hosted =
        HostedControlPlaneClient::try_new_with_token("https://example.convex.cloud", "cp-token")
            .expect("client")
            .with_rpc_override(|_, _, _| Ok(Value::Array(vec![])));
    let hosted_rows = hosted
        .list_workspace_ref_history(&WorkspaceId::new("ws_code"), 50)
        .expect("hosted history");
    assert_eq!(hosted_rows, fake_rows);
}

fn valid_history_wire_record() -> Value {
    Value::Object(args([
        ("workspaceId", Value::from("workspace_1")),
        ("version", number_value(1)),
        ("baseSnapshotId", Value::from("snap_base")),
        ("targetSnapshotId", Value::from("snap_after")),
        ("occurredAt", Value::from("2026-06-23T12:00:00Z")),
    ]))
}

fn decode_history(wire: Value) -> ControlPlaneResult<Vec<WorkspaceRefHistoryRecord>> {
    // Mirrors the production caller: validate + decode the response, then convert
    // each record to the domain type.
    decode_hosted_response::<RefsListWorkspaceRefHistory>(wire)?
        .into_iter()
        .map(WorkspaceRefHistoryRecord::try_from)
        .collect()
}

#[test]
fn refs_history_response_rejects_more_than_five_hundred_rows() {
    let rows = Value::Array((0..501).map(|_| valid_history_wire_record()).collect());
    let error = decode_history(rows).expect_err("more than 500 rows rejects");
    assert!(
        error
            .to_string()
            .contains("response did not match the declared contract")
    );
}

#[test]
fn refs_history_response_rejects_version_above_safe_integer() {
    let record = Value::Object(args([
        ("workspaceId", Value::from("workspace_1")),
        // 2^53 exceeds Number.MAX_SAFE_INTEGER and the version's declared maximum.
        ("version", Value::Float64(9_007_199_254_740_992.0)),
        ("baseSnapshotId", Value::from("snap_base")),
        ("targetSnapshotId", Value::from("snap_after")),
        ("occurredAt", Value::from("2026-06-23T12:00:00Z")),
    ]));
    let error = decode_history(Value::Array(vec![record])).expect_err("unsafe version rejects");
    let message = error.to_string();
    assert!(message.contains("response did not match the declared contract"));
    assert!(message.contains("version"));
}

#[test]
fn refs_history_response_rejects_oversized_identifier() {
    let record = Value::Object(args([
        ("workspaceId", Value::from("w".repeat(513))),
        ("version", number_value(1)),
        ("baseSnapshotId", Value::from("snap_base")),
        ("targetSnapshotId", Value::from("snap_after")),
        ("occurredAt", Value::from("2026-06-23T12:00:00Z")),
    ]));
    let error = decode_history(Value::Array(vec![record])).expect_err("oversized id rejects");
    let message = error.to_string();
    assert!(message.contains("response did not match the declared contract"));
    assert!(message.contains("workspaceId"));
    assert!(!message.contains(&"w".repeat(513)));
}

#[test]
fn refs_history_response_rejects_malformed_timestamp() {
    let record = Value::Object(args([
        ("workspaceId", Value::from("workspace_1")),
        ("version", number_value(1)),
        ("baseSnapshotId", Value::from("snap_base")),
        ("targetSnapshotId", Value::from("snap_after")),
        ("occurredAt", Value::from("not-a-timestamp")),
    ]));
    let error =
        decode_history(Value::Array(vec![record])).expect_err("malformed occurredAt rejects");
    let message = error.to_string();
    assert!(message.contains("response did not match the declared contract"));
    assert!(message.contains("occurredAt"));
}

#[test]
fn refs_history_response_accepts_valid_boundary_vectors() {
    // 2^53 - 1 version, a 512-char workspaceId (the canonical identifier bound),
    // and exactly 500 rows are all at the declared limits and must round-trip.
    let record = Value::Object(args([
        ("workspaceId", Value::from("w".repeat(512))),
        ("version", number_value(9_007_199_254_740_991)),
        ("baseSnapshotId", Value::from("snap_base")),
        ("targetSnapshotId", Value::from("snap_after")),
        ("occurredAt", Value::from("2026-06-23T12:00:00Z")),
    ]));
    let rows = Value::Array((0..500).map(|_| record.clone()).collect());
    let records = decode_history(rows).expect("valid boundary vectors round-trip");
    assert_eq!(records.len(), 500);
    assert_eq!(records[0].version, 9_007_199_254_740_991);
    assert_eq!(records[0].workspace_id.as_str().len(), 512);
}

#[test]
fn refs_history_response_tolerates_additive_server_fields() {
    let record = Value::Object(args([
        ("workspaceId", Value::from("workspace_1")),
        ("version", number_value(1)),
        ("baseSnapshotId", Value::from("snap_base")),
        ("targetSnapshotId", Value::from("snap_after")),
        ("occurredAt", Value::from("2026-06-23T12:00:00Z")),
        // Additive server field must be tolerated (unknownFields: accept).
        ("serverAddedField", Value::from("ignored")),
    ]));
    let records = decode_history(Value::Array(vec![record])).expect("additive field tolerated");
    assert_eq!(records.len(), 1);
}

#[test]
fn refs_history_request_rejects_oversized_workspace_id_before_transport() {
    let request = HostedRefsListWorkspaceRefHistoryRequest {
        workspace_id: "w".repeat(513),
        limit: Some(100),
        auth_token: None,
        account_session_id: None,
    };
    let error = encode_hosted_request::<RefsListWorkspaceRefHistory>(&request)
        .expect_err("oversized outbound workspaceId rejects");
    let message = error.to_string();
    assert!(message.contains("request did not match the declared contract"));
    assert!(message.contains("workspaceId"));
    assert!(!message.contains(&"w".repeat(513)));
}

fn ref_history_record_with_occurred_at(occurred_at: &str) -> Value {
    Value::Object(args([
        ("workspaceId", Value::from("workspace_1")),
        ("version", number_value(1)),
        ("baseSnapshotId", Value::from("snap_base")),
        ("targetSnapshotId", Value::from("snap_after")),
        ("occurredAt", Value::from(occurred_at)),
    ]))
}

#[test]
fn timestamp_validation_matches_shared_rfc3339_policy() {
    #[derive(serde::Deserialize)]
    struct TimestampVectors {
        valid: Vec<String>,
        invalid: Vec<String>,
    }
    let text = std::fs::read_to_string(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/../../tests/contracts/timestamps.json"
    ))
    .expect("shared timestamp fixtures are readable");
    let vectors: TimestampVectors =
        serde_json::from_str(&text).expect("shared timestamp fixtures are valid JSON");

    for vector in &vectors.valid {
        let record = ref_history_record_with_occurred_at(vector);
        assert!(
            decode_history(Value::Array(vec![record])).is_ok(),
            "canonical policy must accept timestamp {vector}"
        );
    }
    for vector in &vectors.invalid {
        let record = ref_history_record_with_occurred_at(vector);
        assert!(
            decode_history(Value::Array(vec![record])).is_err(),
            "canonical policy must reject timestamp {vector}"
        );
    }
}

fn ref_history_record_with_workspace_id(workspace_id: &str) -> Value {
    Value::Object(args([
        ("workspaceId", Value::from(workspace_id)),
        ("version", number_value(1)),
        ("baseSnapshotId", Value::from("snap_base")),
        ("targetSnapshotId", Value::from("snap_after")),
        ("occurredAt", Value::from("2026-06-23T12:00:00Z")),
    ]))
}

#[test]
fn refs_history_request_accepts_max_length_workspace_id() {
    // 512 is the canonical identifier bound and must be accepted on the request.
    let request = HostedRefsListWorkspaceRefHistoryRequest {
        workspace_id: "w".repeat(512),
        limit: Some(100),
        auth_token: None,
        account_session_id: None,
    };
    assert!(encode_hosted_request::<RefsListWorkspaceRefHistory>(&request).is_ok());
}

#[test]
fn list_workspace_ref_history_supports_long_workspace_ids_in_both_auth_modes() {
    // A 300-char workspaceId exceeds the previous 256 narrowing but is within the
    // canonical 512 bound, so a legitimately stored workspace must retrieve its
    // history through both the account-session and control-plane-token paths.
    let workspace = "w".repeat(300);

    let account_session_workspace = workspace.clone();
    let account_session_client =
        HostedControlPlaneClient::try_new_with_token("https://example.convex.cloud", "")
            .expect("client")
            .with_account_session_id("acct_session_1")
            .with_rpc_override(move |kind, name, request| {
                assert_eq!(kind, ConvexRpcKind::Query);
                assert_eq!(name, "refs:listWorkspaceRefHistory");
                assert_eq!(
                    request.get("workspaceId"),
                    Some(&Value::from(account_session_workspace.clone()))
                );
                assert_eq!(
                    request.get("accountSessionId"),
                    Some(&Value::from("acct_session_1"))
                );
                Ok(Value::Array(vec![ref_history_record_with_workspace_id(
                    &account_session_workspace,
                )]))
            });
    let records = account_session_client
        .list_workspace_ref_history(&WorkspaceId::new(workspace.clone()), 50)
        .expect("account-session history for a long workspaceId");
    assert_eq!(records.len(), 1);
    assert_eq!(records[0].workspace_id.as_str(), workspace);

    let token_workspace = workspace.clone();
    let token_client =
        HostedControlPlaneClient::try_new_with_token("https://example.convex.cloud", "cp-token")
            .expect("client")
            .with_rpc_override(move |kind, name, request| {
                assert_eq!(kind, ConvexRpcKind::Query);
                assert_eq!(name, "refs:listWorkspaceRefHistory");
                assert_eq!(request.get("authToken"), Some(&Value::from("cp-token")));
                Ok(Value::Array(vec![ref_history_record_with_workspace_id(
                    &token_workspace,
                )]))
            });
    let records = token_client
        .list_workspace_ref_history(&WorkspaceId::new(workspace.clone()), 50)
        .expect("control-plane-token history for a long workspaceId");
    assert_eq!(records.len(), 1);
    assert_eq!(records[0].workspace_id.as_str(), workspace);
}

#[test]
fn timestamp_policy_sweeps_separators_and_leap_seconds() {
    // Only 'T'/'t'/space separates the date and time; every other ASCII byte is
    // rejected, regardless of any third-party parser's separator leniency.
    for separator in 0u8..=127 {
        let character = char::from(separator);
        let candidate = format!("2026-06-23{character}12:00:00Z");
        let accepted = decode_history(Value::Array(vec![ref_history_record_with_occurred_at(
            &candidate,
        )]))
        .is_ok();
        let expected = matches!(character, 'T' | 't' | ' ');
        assert_eq!(accepted, expected, "separator {character:?} ({separator})");
    }

    // Seconds 00-59 are valid; a leap second (60) or beyond is rejected.
    for second in 0u8..=61 {
        let candidate = format!("2026-06-23T12:00:{second:02}Z");
        let accepted = decode_history(Value::Array(vec![ref_history_record_with_occurred_at(
            &candidate,
        )]))
        .is_ok();
        assert_eq!(accepted, second <= 59, "second {second}");
    }
}

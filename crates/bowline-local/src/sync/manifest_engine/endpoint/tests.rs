use bowline_core::ids::ContentId;

use super::*;
use crate::sync::manifest_engine::manifest::{BlobKey, EntryKind, FileMode, KeyEpoch};
use crate::sync::manifest_engine::store::StatFingerprint;
use crate::workspace::TempWorkspace;

/// COMBINING ACUTE ACCENT, and the precomposed scalar it folds into.
const COMBINING_ACUTE: char = '\u{301}';
const PRECOMPOSED_E_ACUTE: char = '\u{e9}';

/// `café` spelled NFC (`é` as one scalar) and NFD (`e` + combining acute).
const CAFE_NFC: &str = "notes/caf\u{e9}.md";
const CAFE_NFD: &str = "notes/cafe\u{301}.md";

fn record(stamps: (i64, i64), verified_at: Option<i64>) -> FileRecord {
    FileRecord {
        kind: EntryKind::File,
        size: 5,
        mode: FileMode::new(0o100_644),
        symlink_target: None,
        content_id: Some(ContentId::new("cid_x")),
        blob_key: Some(BlobKey::new("b_x")),
        key_epoch: Some(KeyEpoch::new(1)),
        fingerprint: fingerprint(stamps),
        hashed_at: verified_at,
        verified_at: verified_at.map(EndpointInstant::from_stored_nanos),
    }
}

fn observed(stamps: (i64, i64)) -> Observed {
    Observed {
        kind: EntryKind::File,
        size: 5,
        mode: FileMode::new(0o100_644),
        symlink_target: None,
        fingerprint: fingerprint(stamps),
    }
}

fn fingerprint((mtime_ns, ctime_ns): (i64, i64)) -> StatFingerprint {
    StatFingerprint {
        mtime_ns,
        ctime_ns,
        inode: 1,
        dev: 1,
    }
}

const SECOND: TimestampGranularity = TimestampGranularity::SECOND;
const NANOSECOND: TimestampGranularity = TimestampGranularity::NANOSECOND;

#[test]
fn a_file_written_inside_the_verifying_instants_own_tick_is_never_settled() {
    // The row was proved at 1_500 ns; a coarse volume records both that instant
    // and the write as the same second, so nothing at all is known about their
    // order and the bytes must be read.
    let row = record((1_000, 1_000), Some(1_500));
    assert!(!StatTrust::OutsideRacyWindow(SECOND).settles(&row, &observed((1_000, 1_000))));
    assert!(
        StatTrust::OutsideRacyWindow(NANOSECOND).settles(&row, &observed((1_000, 1_000))),
        "a volume that separates the two instants places the write strictly first"
    );
}

#[test]
fn an_unbounded_gap_never_settles_and_an_unproved_row_never_settles() {
    let row = record((1_000, 1_000), Some(9_000_000_000));
    assert!(!StatTrust::Never.settles(&row, &observed((1_000, 1_000))));
    assert!(
        !StatTrust::OutsideRacyWindow(NANOSECOND)
            .settles(&record((1_000, 1_000), None), &observed((1_000, 1_000))),
        "a row the writing cycle could not prove carries no instant to compare"
    );
}

#[test]
fn a_restored_mtime_never_settles_a_rewrite_the_ctime_reports() {
    // `tar -x`, `rsync -t` and `cp -p` all put an old mtime back over new bytes.
    // The row was proved long ago and the mtime matches; only the ctime knows a
    // write happened, so only the ctime may decide.
    let row = record((1_000, 1_000), Some(2_000));
    assert!(
        !StatTrust::OutsideRacyWindow(NANOSECOND).settles(&row, &observed((1_000, 5_000))),
        "an mtime a tool restored proves nothing about when the bytes changed"
    );
    assert!(
        !StatTrust::OutsideRacyWindow(SECOND).settles(&row, &observed((1_000, 5_000))),
        "and the coarse volume that hides the ctime change still has no proof"
    );
}

#[test]
fn a_matching_fingerprint_is_matched_at_endpoint_resolution() {
    // Two timestamps one second apart are one timestamp to a one-second volume,
    // and two distinct ones to a nanosecond volume. Identity is decided the way
    // the endpoint records it, never more finely than the evidence allows.
    let row = record((1_000_000_000, 1_000_000_000), Some(4_000_000_000));
    assert!(
        StatTrust::OutsideRacyWindow(SECOND)
            .settles(&row, &observed((1_400_000_000, 1_400_000_000)))
    );
    assert!(
        !StatTrust::OutsideRacyWindow(NANOSECOND)
            .settles(&row, &observed((1_400_000_000, 1_400_000_000)))
    );
}

#[test]
fn coarse_proof_rejects_same_size_rewrite_hidden_by_matching_stat() {
    let workspace = TempWorkspace::new("coarse-proof-content").expect("temp workspace");
    let path = WorkspacePath::new("same-size.txt");
    let absolute = workspace.root().join(path.as_str());
    let crypto = WorkspaceCrypto::new("workspace", [7_u8; 32], KeyEpoch::new(1));
    std::fs::write(&absolute, b"old!").expect("write original");

    let ObserveOutcome::Present(original) = observe_classified(workspace.root(), &path) else {
        panic!("original file observation");
    };
    let mut row = record(
        (original.fingerprint.mtime_ns, original.fingerprint.ctime_ns),
        None,
    );
    row.size = original.size;
    row.mode = original.mode;
    row.fingerprint = original.fingerprint;
    row.content_id = Some(crypto.content_id(b"old!"));

    std::fs::write(&absolute, b"new!").expect("same-size rewrite");
    let ObserveOutcome::Present(rewritten) = observe_classified(workspace.root(), &path) else {
        panic!("rewritten file observation");
    };
    // Model a coarse filesystem that reports the rewrite with the same stat
    // identity the row recorded. Shape and stat checks alone necessarily pass.
    row.fingerprint = rewritten.fingerprint;
    let sampled = EndpointInstant::from_stored_nanos(
        SECOND
            .bucket(rewritten.fingerprint.ctime_ns)
            .saturating_add(SECOND.nanos()),
    );

    assert_eq!(
        prove_row(
            workspace.root(),
            SECOND,
            sampled,
            &crypto,
            1024,
            &path,
            &row,
        ),
        None,
        "coarse timestamp proof must validate the bytes, not only matching metadata"
    );
}

#[test]
fn a_bucket_floors_downwards_on_both_sides_of_the_epoch() {
    assert_eq!(SECOND.bucket(1_400_000_000), 1_000_000_000);
    assert_eq!(SECOND.bucket(-1), -1_000_000_000);
    assert_eq!(
        TimestampGranularity::TWO_SECONDS.bucket(3_999_999_999),
        2_000_000_000
    );
    assert!(SECOND.indistinguishable(-1, -1_000_000_000));
    assert!(!SECOND.indistinguishable(-1, 0));
}

#[test]
fn the_endpoint_clock_reads_the_workspace_volume_and_leaves_nothing_behind() {
    let workspace = TempWorkspace::new("endpoint-clock").expect("temp workspace");
    let endpoint = workspace.root().join("view");
    std::fs::create_dir(&endpoint).expect("endpoint root");
    let sampled = sample_endpoint_clock(&endpoint, &endpoint).expect("clock sample");

    // A reading of the volume's own clock, not of the process wall clock: the
    // only thing both must agree on is being a plausible current instant.
    let wall = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .expect("wall clock")
        .as_nanos() as i64;
    assert!(sampled.nanos() > wall - 60_000_000_000);
    assert!(sampled.nanos() < wall + 60_000_000_000);
    assert!(
        std::fs::read_dir(&endpoint)
            .expect("endpoint entries")
            .next()
            .is_none(),
        "the private clock probe leaves nothing behind"
    );
}

#[test]
fn endpoint_clock_does_not_touch_a_project_file_with_the_old_probe_name() {
    let workspace = TempWorkspace::new("endpoint-clock-collision").expect("temp workspace");
    let project_file = workspace.root().join(CLOCK_PROBE_LEAF);
    std::fs::write(&project_file, b"project content").expect("project file");

    sample_endpoint_clock(workspace.root(), workspace.root()).expect("clock sample");

    assert_eq!(
        std::fs::read(project_file).expect("project file remains"),
        b"project content"
    );
}

#[test]
fn a_clock_read_from_another_volume_proves_nothing_about_this_one() {
    let workspace = TempWorkspace::new("endpoint-clock-volume").expect("temp workspace");
    let engine_dir = workspace.root().join(".bowline");
    // `/dev` is a distinct device on both platforms the engine runs on, so this
    // is the work-view shape: engine state parked off the synced volume.
    assert!(
        sample_endpoint_clock(&engine_dir, Path::new("/dev")).is_none(),
        "a reading is only usable for the volume it was taken on"
    );
}

#[test]
fn the_probe_measures_the_workspace_volume() {
    let workspace = TempWorkspace::new("endpoint-probe").expect("temp workspace");
    let engine_dir = workspace.root().join(".bowline");
    let granularity = probe_timestamp_granularity(&engine_dir);
    assert!(
        [
            TimestampGranularity::NANOSECOND,
            TimestampGranularity::SECOND,
            TimestampGranularity::TWO_SECONDS,
        ]
        .contains(&granularity),
        "the probe answers one of the three known granularities"
    );
    assert!(
        !engine_dir.join(TIMESTAMP_PROBE_LEAF).exists(),
        "the probe leaves nothing behind"
    );
}

#[test]
fn reserved_probe_root_is_private_and_stays_on_the_workspace_volume() {
    use std::os::unix::fs::MetadataExt;

    let workspace = TempWorkspace::new("endpoint-reserved-probe").expect("temp workspace");
    let probe_root = prepare_endpoint_probe_root(workspace.root()).expect("prepare probe root");
    assert_eq!(
        fs::metadata(workspace.root())
            .expect("workspace metadata")
            .dev(),
        fs::metadata(&probe_root).expect("probe metadata").dev()
    );
    let relative = probe_root
        .strip_prefix(workspace.root())
        .expect("probe lives beneath workspace")
        .to_string_lossy();
    assert!(crate::policy::is_private_workspace_state_path(&relative));
}

#[test]
fn reserved_probe_root_rejects_a_different_volume() {
    use std::os::unix::fs::MetadataExt;

    let workspace = TempWorkspace::new("endpoint-reserved-probe-volume").expect("temp workspace");
    let workspace_dev = fs::metadata(workspace.root())
        .expect("workspace metadata")
        .dev();
    let foreign = fs::metadata("/dev").expect("/dev metadata");
    assert_ne!(
        workspace_dev,
        foreign.dev(),
        "test requires distinct volumes"
    );
    assert!(validate_endpoint_probe_location(&foreign, workspace_dev).is_err());
}

#[test]
fn reserved_probe_root_never_adopts_an_existing_directory() {
    let workspace = TempWorkspace::new("endpoint-reserved-probe-owner").expect("temp workspace");
    let colliding = workspace
        .root()
        .join(format!("{ENDPOINT_PROBE_STATE_PREFIX}user.tmp"));
    fs::create_dir(&colliding).expect("create colliding directory");
    fs::write(colliding.join("user.txt"), b"user data").expect("write user data");

    let prepared = prepare_endpoint_probe_root(workspace.root()).expect("prepare probe root");
    assert_ne!(prepared, colliding);
    assert_eq!(
        fs::read(colliding.join("user.txt")).expect("read user data"),
        b"user data"
    );
}

#[test]
fn reserved_probe_root_reuses_only_a_valid_owned_directory() {
    let workspace = TempWorkspace::new("endpoint-reserved-probe-reuse").expect("temp workspace");
    let first = prepare_endpoint_probe_root(workspace.root()).expect("prepare first probe root");
    let second = prepare_endpoint_probe_root(workspace.root()).expect("reuse probe root");
    assert_eq!(first, second);
    validate_endpoint_probe_owner(&second).expect("valid owner marker");
}

#[test]
fn the_name_probe_answers_this_volume_and_leaves_nothing_behind() {
    let workspace = TempWorkspace::new("endpoint-name-probe").expect("temp workspace");
    let engine_dir = workspace.root().join(".bowline");
    let folding = probe_name_folding(&engine_dir);

    // The verdict itself is a property of the CI volume, so assert what must hold
    // on every filesystem rather than pinning one answer: the probe classifies
    // both axes, and it cleans up after itself.
    assert!(matches!(
        folding.normalization(),
        NormalizationForm::Insensitive | NormalizationForm::Sensitive
    ));
    assert!(matches!(
        folding.case(),
        CaseForm::Insensitive | CaseForm::Sensitive
    ));
    for leaf in [
        DECOMPOSED_PROBE_LEAF,
        PRECOMPOSED_PROBE_LEAF,
        MIXED_CASE_PROBE_LEAF,
        SWAPPED_CASE_PROBE_LEAF,
    ] {
        assert!(
            !engine_dir.join(leaf).exists(),
            "the name probe leaves nothing behind: {leaf}"
        );
    }
}

/// The probe is an observation plus a mapping. Both directions of each are
/// covered here, so the classification is proven for a folding AND a
/// non-folding endpoint whichever kind of volume the test runs on.
#[test]
fn the_probe_classifies_a_folding_and_a_non_folding_endpoint() {
    let workspace = TempWorkspace::new("endpoint-fold-classify").expect("temp workspace");
    let engine_dir = workspace.root().join(".bowline");

    // The observation, against a real filesystem. Two genuinely different names
    // never resolve to each other — the sensitive endpoint's answer — and one
    // name always resolves to itself, which is what an insensitive endpoint
    // reports for a pair it maps onto a single file.
    assert!(!probe_resolves_as(&engine_dir, "alpha.probe", "beta.probe").expect("probe"));
    assert!(probe_resolves_as(&engine_dir, "alpha.probe", "alpha.probe").expect("probe"));

    // The mapping.
    assert_eq!(
        NormalizationForm::from_folds(true),
        NormalizationForm::Insensitive
    );
    assert_eq!(
        NormalizationForm::from_folds(false),
        NormalizationForm::Sensitive
    );
    assert_eq!(CaseForm::from_folds(true), CaseForm::Insensitive);
    assert_eq!(CaseForm::from_folds(false), CaseForm::Sensitive);
}

/// The composed probe must agree with the same experiment run independently on
/// this volume. Whichever kind of volume CI provides, this pins the verdict to
/// observed behaviour rather than to the OS name.
#[test]
fn the_probe_agrees_with_a_hand_run_experiment_on_this_volume() {
    let workspace = TempWorkspace::new("endpoint-fold-agreement").expect("temp workspace");
    let engine_dir = workspace.root().join(".bowline");
    std::fs::create_dir_all(&engine_dir).expect("engine dir");

    std::fs::write(engine_dir.join("agreement-aa.check"), b"probe").expect("write probe");
    let case_folds = std::fs::symlink_metadata(engine_dir.join("agreement-AA.check")).is_ok();

    let decomposed = format!("agreement-e{}.check", COMBINING_ACUTE);
    let precomposed = format!("agreement-{}.check", PRECOMPOSED_E_ACUTE);
    std::fs::write(engine_dir.join(&decomposed), b"probe").expect("write probe");
    let normalization_folds = std::fs::symlink_metadata(engine_dir.join(&precomposed)).is_ok();

    let probed = probe_name_folding(&engine_dir);
    assert_eq!(probed.case(), CaseForm::from_folds(case_folds));
    assert_eq!(
        probed.normalization(),
        NormalizationForm::from_folds(normalization_folds)
    );
}

#[test]
fn capability_cache_is_partitioned_by_endpoint_directory_identity() {
    let exact = EndpointCapabilities {
        names: NameFolding::EXACT,
        timestamps: TimestampGranularity::NANOSECOND,
    };
    let folding = EndpointCapabilities {
        names: NameFolding::new(NormalizationForm::Insensitive, CaseForm::Insensitive),
        timestamps: TimestampGranularity::TWO_SECONDS,
    };
    let mut cache = std::collections::BTreeMap::new();
    let first_identity = EndpointIdentity {
        volume: 11,
        directory: 101,
    };
    let second_identity = EndpointIdentity {
        volume: 11,
        directory: 202,
    };
    let first = cache_capabilities(&mut cache, first_identity, exact);
    let second = cache_capabilities(&mut cache, second_identity, folding);
    let cached = cache_capabilities(&mut cache, first_identity, folding);

    assert_eq!(first, exact);
    assert_eq!(second, folding);
    assert_eq!(cached, exact);
    assert_eq!(cache.len(), 2);
}

#[test]
fn root_replacement_refresh_overwrites_a_reused_endpoint_identity() {
    let old = EndpointCapabilities {
        names: NameFolding::EXACT,
        timestamps: TimestampGranularity::NANOSECOND,
    };
    let replacement = EndpointCapabilities {
        names: NameFolding::new(NormalizationForm::Insensitive, CaseForm::Insensitive),
        timestamps: TimestampGranularity::TWO_SECONDS,
    };
    let identity = EndpointIdentity {
        volume: 11,
        directory: 101,
    };
    let mut cache = std::collections::BTreeMap::new();
    cache_capabilities(&mut cache, identity, old);

    let refreshed = refresh_cached_capabilities(&mut cache, identity, replacement);
    let cached = cache_capabilities(&mut cache, identity, old);

    assert_eq!(refreshed, replacement);
    assert_eq!(cached, replacement);
}

#[test]
fn capability_probe_uses_a_private_ephemeral_directory_and_leaves_nothing() {
    let workspace = TempWorkspace::new("endpoint-capabilities").expect("temp workspace");
    let endpoint = workspace.root().join("view");
    std::fs::create_dir(&endpoint).expect("endpoint root");

    let capabilities = probe_endpoint_capabilities(&endpoint);
    assert!(matches!(
        capabilities.timestamps,
        TimestampGranularity::NANOSECOND
            | TimestampGranularity::SECOND
            | TimestampGranularity::TWO_SECONDS
    ));
    assert!(
        std::fs::read_dir(&endpoint)
            .expect("endpoint entries")
            .next()
            .is_none(),
        "the endpoint probe directory is removed"
    );
}

#[test]
fn private_probe_cleanup_never_recursively_deletes_unexpected_content() {
    let workspace = TempWorkspace::new("endpoint-probe-injection").expect("temp workspace");
    let injected = with_private_probe_dir(workspace.root(), |probe_dir| {
        std::fs::write(probe_dir.join("unexpected.txt"), b"keep me")?;
        Ok(())
    });

    assert!(
        injected.is_err(),
        "a non-empty probe directory fails closed"
    );
    let probe_dir = std::fs::read_dir(workspace.root())
        .expect("workspace entries")
        .next()
        .expect("preserved probe directory")
        .expect("probe entry")
        .path();
    assert_eq!(
        std::fs::read(probe_dir.join("unexpected.txt")).expect("unexpected content remains"),
        b"keep me"
    );
}

#[test]
fn an_insensitive_endpoint_publishes_one_nfc_spelling_for_both_forms() {
    let folding = NameFolding::new(NormalizationForm::Insensitive, CaseForm::Insensitive);
    let nfc = WorkspacePath::new(CAFE_NFC);
    let nfd = WorkspacePath::new(CAFE_NFD);
    assert_ne!(nfc, nfd, "the two spellings are distinct byte strings");
    assert_eq!(folding.canonical_spelling(&nfd), nfc);
    assert_eq!(
        folding.canonical_spelling(&nfc),
        nfc,
        "an already-NFC path is untouched"
    );
}

#[test]
fn a_sensitive_endpoint_publishes_the_bytes_it_observed() {
    let folding = NameFolding::EXACT;
    let nfd = WorkspacePath::new(CAFE_NFD);
    assert_eq!(
        folding.canonical_spelling(&nfd),
        nfd,
        "rewriting to NFC here would name a file that does not exist"
    );
}

#[test]
fn canonical_spelling_never_folds_case() {
    let folding = NameFolding::new(NormalizationForm::Insensitive, CaseForm::Insensitive);
    let path = WorkspacePath::new("docs/README.md");
    assert_eq!(
        folding.canonical_spelling(&path),
        path,
        "a case-insensitive filesystem is still case-preserving"
    );
}

#[test]
fn the_fold_key_merges_exactly_the_axes_the_endpoint_folds() {
    let both = NameFolding::new(NormalizationForm::Insensitive, CaseForm::Insensitive);
    assert_eq!(both.fold(CAFE_NFD), both.fold(CAFE_NFC));
    assert_eq!(both.fold("Docs/README.md"), both.fold("docs/readme.md"));

    let normalization_only = NameFolding::new(NormalizationForm::Insensitive, CaseForm::Sensitive);
    assert_eq!(
        normalization_only.fold(CAFE_NFD),
        normalization_only.fold(CAFE_NFC)
    );
    assert_ne!(
        normalization_only.fold("Docs/README.md"),
        normalization_only.fold("docs/readme.md")
    );

    let case_only = NameFolding::new(NormalizationForm::Sensitive, CaseForm::Insensitive);
    assert_ne!(case_only.fold(CAFE_NFD), case_only.fold(CAFE_NFC));
    assert_eq!(
        case_only.fold("Docs/README.md"),
        case_only.fold("docs/readme.md")
    );

    assert_ne!(NameFolding::EXACT.fold(CAFE_NFD), CAFE_NFC);
    assert_eq!(NameFolding::EXACT.fold(CAFE_NFC), CAFE_NFC);
}

#[test]
fn an_ascii_path_is_already_nfc_and_is_returned_unchanged() {
    let path = WorkspacePath::new("src/auth.ts");
    assert_eq!(nfc_path(&path), path);
    assert_eq!(
        nfc_path(&WorkspacePath::new(CAFE_NFD)),
        WorkspacePath::new(CAFE_NFC)
    );
}

use super::{
    MAX_WORK_STEPS, MergeBudget, MergePhase, NotTextReason, TextMergeConflictReason,
    TextMergeOutcome, diff::WorkBudget, diff::anchored_diff_changes, merge_text_lines,
};

type MergeCase<'a> = (&'a [u8], &'a [u8], &'a [u8], &'a [u8]);

fn clean_bytes(base: &[u8], local: &[u8], remote: &[u8]) -> Vec<u8> {
    match merge_text_lines(base, local, remote) {
        TextMergeOutcome::Clean { bytes, .. } => bytes,
        outcome => panic!("expected clean merge, got {outcome:?}"),
    }
}

fn assert_conflict(base: &[u8], local: &[u8], remote: &[u8]) {
    match merge_text_lines(base, local, remote) {
        TextMergeOutcome::Conflict { reason, overlaps } => {
            assert_eq!(reason, TextMergeConflictReason::IncompatibleOverlap);
            assert!(!overlaps.is_empty());
        }
        outcome => panic!("expected conflict, got {outcome:?}"),
    }
}

#[test]
fn reviewed_merge_corpus_preserves_exact_bytes() {
    let cases: &[MergeCase<'_>] = &[
        (
            b"one\nmiddle\nlast\n",
            b"ONE\nmiddle\nlast\n",
            b"one\nmiddle\nLAST\n",
            b"ONE\nmiddle\nLAST\n",
        ),
        (
            b"a\nb\nc\nd\n",
            b"a\nB\nc\nd\n",
            b"a\nb\nC\nd\n",
            b"a\nB\nC\nd\n",
        ),
        (
            b"a\nb\nc\n",
            b"a\nlocal\nb\nc\n",
            b"a\nb\nremote\nc\n",
            b"a\nlocal\nb\nremote\nc\n",
        ),
        (
            b"alpha\nomega",
            b"ALPHA\nomega",
            b"alpha\nOMEGA",
            b"ALPHA\nOMEGA",
        ),
        (
            b"a\r\nb\r\nc\r\n",
            b"A\r\nb\r\nc\r\n",
            b"a\r\nb\r\nC\r\n",
            b"A\r\nb\r\nC\r\n",
        ),
        (
            "café\n東京\n🙂\n".as_bytes(),
            "café local\n東京\n🙂\n".as_bytes(),
            "café\n東京 remote\n🙂\n".as_bytes(),
            "café local\n東京 remote\n🙂\n".as_bytes(),
        ),
    ];

    for &(base, local, remote, expected) in cases {
        assert_eq!(clean_bytes(base, local, remote), expected);
    }
}

#[test]
fn divergent_same_point_insertions_conflict_instead_of_concatenating() {
    assert_conflict(b"a\nb\n", b"a\nlocal\nb\n", b"a\nremote\nb\n");
}

#[test]
fn identical_same_point_insertions_apply_once() {
    assert_eq!(
        clean_bytes(b"a\nb\n", b"a\ninserted\nb\n", b"a\ninserted\nb\n"),
        b"a\ninserted\nb\n"
    );
}

#[test]
fn incompatible_overlap_classes_conflict() {
    assert_conflict(b"a\nold\nc\n", b"a\nlocal\nc\n", b"a\nremote\nc\n");
    assert_conflict(b"a\nremove\nc\n", b"a\nc\n", b"a\nedited\nc\n");
    assert_conflict(b"", b"local\n", b"remote\n");
    assert_conflict(
        b"head\nx\nx\ntail\n",
        b"head\nx\ntail\n",
        b"head\ny\nx\ntail\n",
    );
}

#[test]
fn insertions_at_replacement_boundaries_are_disjoint() {
    assert_eq!(
        clean_bytes(b"a\nb\nc\n", b"a\ninserted\nb\nc\n", b"a\nB\nc\n",),
        b"a\ninserted\nB\nc\n"
    );
    assert_eq!(
        clean_bytes(b"a\nb\nc\n", b"a\nb\ninserted\nc\n", b"a\nB\nc\n",),
        b"a\nB\ninserted\nc\n"
    );
}

#[test]
fn insertion_strictly_inside_replacement_conflicts() {
    assert_conflict(
        b"begin\none\ntwo\nend\n",
        b"begin\nONE-TWO\nend\n",
        b"begin\none\ninserted\ntwo\nend\n",
    );
}

#[test]
fn terminator_changes_are_edits_and_never_normalized() {
    assert_eq!(
        clean_bytes(b"a\r\nb\r\nc\r\n", b"A\r\nb\r\nc\r\n", b"a\r\nb\r\nc\n",),
        b"A\r\nb\r\nc\n"
    );
    assert_conflict(b"alpha\nomega", b"alpha\nomega\n", b"alpha\nOMEGA");
}

#[test]
fn input_classification_is_typed() {
    assert_eq!(
        merge_text_lines(b"base\n", b"local\xff\n", b"remote\n"),
        TextMergeOutcome::NotText {
            reason: NotTextReason::InvalidUtf8,
        }
    );
    assert_eq!(
        merge_text_lines(b"base\n", b"local\0\n", b"remote\n"),
        TextMergeOutcome::NotText {
            reason: NotTextReason::BinaryControlByte,
        }
    );
    assert_eq!(
        merge_text_lines(b"base\n", b"local\x07\n", b"remote\n"),
        TextMergeOutcome::NotText {
            reason: NotTextReason::BinaryControlByte,
        }
    );
}

#[test]
fn sparse_edits_merge_above_deleted_matrix_cliff() {
    let base = (0..4_096)
        .map(|index| format!("line-{index:04}\n"))
        .collect::<String>();
    let mut local_lines = base
        .lines()
        .map(|line| format!("{line}\n"))
        .collect::<Vec<_>>();
    let mut remote_lines = local_lines.clone();
    local_lines[64] = "local-0064\n".to_string();
    remote_lines[4_032] = "remote-4032\n".to_string();
    let local = local_lines.concat();
    let remote = remote_lines.concat();
    let merged = String::from_utf8(clean_bytes(
        base.as_bytes(),
        local.as_bytes(),
        remote.as_bytes(),
    ))
    .expect("merged UTF-8");
    assert!(merged.contains("local-0064\n"));
    assert!(merged.contains("remote-4032\n"));
    assert_eq!(merged.lines().count(), 4_096);
}

#[test]
fn pathological_high_distance_diff_stops_at_a_typed_budget() {
    let base = (0..2_048)
        .map(|index| format!("base-{index:04}\n"))
        .collect::<String>();
    let local = (0..2_048)
        .map(|index| format!("local-{index:04}\n"))
        .collect::<String>();
    let remote = (0..2_048)
        .map(|index| format!("remote-{index:04}\n"))
        .collect::<String>();
    assert!(matches!(
        merge_text_lines(base.as_bytes(), local.as_bytes(), remote.as_bytes()),
        TextMergeOutcome::ResourceLimit { .. }
    ));
}

#[test]
fn clean_conflict_classification_is_symmetric() {
    let cases: &[(&[u8], &[u8], &[u8])] = &[
        (b"a\nb\nc\n", b"A\nb\nc\n", b"a\nb\nC\n"),
        (b"a\nb\n", b"a\nlocal\nb\n", b"a\nremote\nb\n"),
        (b"a\nold\nc\n", b"a\nlocal\nc\n", b"a\nremote\nc\n"),
    ];
    for &(base, local, remote) in cases {
        assert_eq!(
            outcome_class(&merge_text_lines(base, local, remote)),
            outcome_class(&merge_text_lines(base, remote, local)),
        );
    }
}

fn outcome_class(outcome: &TextMergeOutcome) -> &'static str {
    match outcome {
        TextMergeOutcome::Clean { .. } => "clean",
        TextMergeOutcome::Conflict { .. } => "conflict",
        TextMergeOutcome::NotText { .. } => "not-text",
        TextMergeOutcome::ResourceLimit { .. } => "resource-limit",
        TextMergeOutcome::InternalError { .. } => "internal-error",
    }
}

#[test]
fn diff_scripts_reproduce_every_small_modified_sequence() {
    let alphabet = ["a\n", "b\n", "{\n"];
    let sequences = small_sequences(&alphabet, 4);
    for base in &sequences {
        for modified in &sequences {
            let mut budget = WorkBudget::new(MAX_WORK_STEPS, 8_000_000, 64);
            let changes =
                anchored_diff_changes(base, modified, &mut budget).expect("bounded small diff");
            let mut rebuilt = Vec::new();
            let mut cursor = 0;
            for change in changes {
                rebuilt.extend_from_slice(&base[cursor..change.base_start]);
                rebuilt.extend(change.replacement);
                cursor = change.base_end;
            }
            rebuilt.extend_from_slice(&base[cursor..]);
            assert_eq!(&rebuilt, modified, "base={base:?} modified={modified:?}");
        }
    }
}

#[test]
fn tiny_deterministic_work_budget_returns_resource_limit() {
    let base = ["a\n", "b\n", "c\n", "d\n"];
    let modified = ["w\n", "x\n", "y\n", "z\n"];
    let mut budget = WorkBudget::new(1, 100, 64);
    assert_eq!(
        anchored_diff_changes(&base, &modified, &mut budget),
        Err(super::diff::DiffFailure::ResourceLimit {
            phase: MergePhase::Anchors,
            budget: MergeBudget::WorkSteps,
        })
    );
}

fn small_sequences<'a>(alphabet: &'a [&'a str], max_len: usize) -> Vec<Vec<&'a str>> {
    let mut sequences = vec![Vec::new()];
    for _ in 0..max_len {
        let previous = sequences.clone();
        for sequence in previous {
            for value in alphabet {
                let mut extended = sequence.clone();
                extended.push(*value);
                sequences.push(extended);
            }
        }
    }
    sequences.sort();
    sequences.dedup();
    sequences
}

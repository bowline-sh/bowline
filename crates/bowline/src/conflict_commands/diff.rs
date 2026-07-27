use std::path::Path;

use bowline_core::commands::DiffUnavailable;
use bowline_local::conflicts::{ConflictAside, ConflictError, ConflictSide, read_conflict_side};

/// Longest file `--diff` will read from either side. Beyond this the answer is
/// "open them in a real diff tool", not a wall of terminal output.
const MAX_DIFF_BYTES: u64 = 1_000_000;

/// What `--diff` has to show for one conflict.
pub(super) enum DiffOutcome {
    Unified(String),
    Unavailable(DiffUnavailable),
}

/// A unified diff from the file to its aside, or the named reason there is none.
///
/// Both sides are read through the engine's no-follow boundary
/// ([`read_conflict_side`]), so an aside-shaped symlink pointing at a file
/// outside the workspace is reported as a symlink and never dereferenced — this
/// output goes straight to a terminal, and following the link would print
/// whatever the user can read.
pub(super) fn unified_diff(
    root: &Path,
    conflict: &ConflictAside,
) -> Result<DiffOutcome, ConflictError> {
    let local = comparable(read_conflict_side(root, &conflict.origin, MAX_DIFF_BYTES)?);
    let remote = comparable(read_conflict_side(root, &conflict.aside, MAX_DIFF_BYTES)?);
    Ok(match (local, remote) {
        (Comparable::Text(local), Comparable::Text(remote)) => {
            DiffOutcome::Unified(render_unified(
                conflict.origin.as_str(),
                &local,
                conflict.aside.as_str(),
                &remote,
            ))
        }
        (Comparable::Refused(reason), _) | (_, Comparable::Refused(reason)) => {
            DiffOutcome::Unavailable(reason)
        }
    })
}

/// One side reduced to the only two answers a diff can use.
enum Comparable {
    Text(String),
    Refused(DiffUnavailable),
}

fn comparable(side: ConflictSide) -> Comparable {
    match side {
        ConflictSide::Text(text) => Comparable::Text(text),
        ConflictSide::Symlink => Comparable::Refused(DiffUnavailable::Symlink),
        ConflictSide::Directory => Comparable::Refused(DiffUnavailable::Directory),
        ConflictSide::Missing => Comparable::Refused(DiffUnavailable::Missing),
        ConflictSide::TooLarge { .. } => Comparable::Refused(DiffUnavailable::TooLarge),
        ConflictSide::Binary => Comparable::Refused(DiffUnavailable::Binary),
        ConflictSide::Unreadable => Comparable::Refused(DiffUnavailable::Unreadable),
    }
}

/// A minimal unified diff over whole lines.
///
/// The common prefix and suffix are elided and everything between them is shown
/// as one replaced block. That is exact (never claims a line changed that did
/// not) without an LCS pass, and a conflict-aside diff is read to decide which
/// side to keep, not to be applied as a patch.
fn render_unified(local_path: &str, local: &str, remote_path: &str, remote: &str) -> String {
    let mut out = format!("--- {local_path}\n+++ {remote_path}\n");
    // Byte equality is answered before splitting: `lines()` drops terminators, so
    // two sides differing only in a final newline — or not differing at all —
    // otherwise reduced to the same sequence and printed as an unexplained
    // "identical apart from trailing bytes", which is precisely the information
    // the reader needed to pick a side.
    if local == remote {
        out.push_str("(identical)\n");
        return out;
    }
    let local_lines = local.lines().collect::<Vec<_>>();
    let remote_lines = remote.lines().collect::<Vec<_>>();
    let prefix = common_prefix(&local_lines, &remote_lines);
    let suffix = common_suffix(&local_lines[prefix..], &remote_lines[prefix..]);

    if prefix == local_lines.len() && prefix == remote_lines.len() {
        // Same lines, different bytes: the only thing left is the final newline.
        // Name which side carries it, the way `diff` does.
        out.push_str(&format!(
            "@@ -{},{} +{},{} @@\n",
            local_lines.len(),
            local_lines.len(),
            remote_lines.len(),
            remote_lines.len(),
        ));
        for (label, text) in [('-', local), ('+', remote)] {
            let last = text.lines().next_back().unwrap_or_default();
            out.push_str(&format!("{label}{last}\n"));
            if !text.ends_with('\n') {
                out.push_str("\\ No newline at end of file\n");
            }
        }
        return out;
    }
    out.push_str(&format!(
        "@@ -{},{} +{},{} @@\n",
        prefix + 1,
        local_lines.len() - prefix - suffix,
        prefix + 1,
        remote_lines.len() - prefix - suffix,
    ));
    for line in &local_lines[prefix..local_lines.len() - suffix] {
        out.push_str(&format!("-{line}\n"));
    }
    if suffix == 0 && !local.ends_with('\n') && !local.is_empty() {
        out.push_str("\\ No newline at end of file\n");
    }
    for line in &remote_lines[prefix..remote_lines.len() - suffix] {
        out.push_str(&format!("+{line}\n"));
    }
    if suffix == 0 && !remote.ends_with('\n') && !remote.is_empty() {
        out.push_str("\\ No newline at end of file\n");
    }
    out
}

fn common_prefix(left: &[&str], right: &[&str]) -> usize {
    left.iter().zip(right).take_while(|(a, b)| a == b).count()
}

fn common_suffix(left: &[&str], right: &[&str]) -> usize {
    left.iter()
        .rev()
        .zip(right.iter().rev())
        .take_while(|(a, b)| a == b)
        .count()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A trailing newline is the whole difference in a real class of conflicts
    /// (an editor that adds one, a generator that does not). `lines()` drops
    /// terminators, so both sides reduced to the same sequence and the reader was
    /// told only that they were "identical apart from trailing bytes" — without
    /// which side held the bytes, which is the one thing they needed.
    #[test]
    fn a_trailing_newline_difference_names_the_side_that_carries_it() {
        let diff = render_unified("a.txt", "one\n", "a.txt.bowline-conflict.abc", "one");

        assert!(diff.contains("\\ No newline at end of file"), "{diff}");
        assert!(!diff.contains("identical"), "{diff}");
    }

    #[test]
    fn byte_identical_sides_are_reported_as_identical() {
        let diff = render_unified("a.txt", "one\n", "a.txt.bowline-conflict.abc", "one\n");

        assert!(diff.contains("(identical)"), "{diff}");
        assert!(!diff.contains("No newline"), "{diff}");
    }

    #[test]
    fn a_changed_line_shows_both_sides_and_no_untouched_context() {
        let diff = render_unified(
            "a.txt",
            "one\ntwo\nthree\n",
            "a.txt.bowline-conflict.abc",
            "one\nTWO\nthree\n",
        );
        assert!(diff.contains("-two\n"), "{diff}");
        assert!(diff.contains("+TWO\n"), "{diff}");
        assert!(!diff.contains("-one\n"), "{diff}");
        assert!(!diff.contains("-three\n"), "{diff}");
    }

    #[test]
    fn an_appended_line_reports_only_the_addition() {
        let diff = render_unified("a.txt", "one\n", "b", "one\ntwo\n");
        assert!(diff.contains("+two\n"), "{diff}");
        assert!(!diff.contains("-one\n"), "{diff}");
    }
}

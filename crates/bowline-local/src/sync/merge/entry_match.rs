use bowline_core::workspace_graph::NamespaceEntry;

pub(super) fn optional_entries_match_for_merge(
    left: Option<&NamespaceEntry>,
    right: Option<&NamespaceEntry>,
) -> bool {
    match (left, right) {
        (None, None) => true,
        (Some(left), Some(right)) => entries_match_for_merge(left, right),
        _ => false,
    }
}

pub(super) fn entries_match_except_executability(
    left: &NamespaceEntry,
    right: &NamespaceEntry,
) -> bool {
    entries_match_for_merge_without_executability(left, right)
        && left.executability != right.executability
}

fn entries_match_for_merge(left: &NamespaceEntry, right: &NamespaceEntry) -> bool {
    entries_match_for_merge_without_executability(left, right)
        && left.executability == right.executability
}

fn entries_match_for_merge_without_executability(
    left: &NamespaceEntry,
    right: &NamespaceEntry,
) -> bool {
    left.path == right.path
        && left.kind == right.kind
        && left.classification == right.classification
        && left.mode == right.mode
        && left.access == right.access
        && left.content_id == right.content_id
        && left.symlink_target == right.symlink_target
        && left.byte_len == right.byte_len
}

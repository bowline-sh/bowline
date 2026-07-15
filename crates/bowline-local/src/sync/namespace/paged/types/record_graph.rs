use std::collections::BTreeSet;

use super::super::{
    codec::NamespacePage,
    layout::{ContentLayoutRecord, SegmentPage, SegmentSequence},
};

pub(super) fn namespace_children(page: &NamespacePage) -> Vec<String> {
    let ids = match page {
        NamespacePage::Leaf { entries, .. } => entries
            .iter()
            .filter_map(|(_, value)| value.content_layout_id.as_ref())
            .map(|id| id.as_str().to_string())
            .collect(),
        NamespacePage::Branch {
            children, value, ..
        } => {
            let mut ids = children
                .iter()
                .map(|(_, id)| id.as_str().to_string())
                .collect::<Vec<_>>();
            if let Some(id) = value
                .as_ref()
                .and_then(|entry| entry.content_layout_id.as_ref())
            {
                ids.push(id.as_str().to_string());
            }
            ids
        }
    };
    ids.into_iter()
        .collect::<BTreeSet<_>>()
        .into_iter()
        .collect()
}

pub(super) fn layout_children(layout: &ContentLayoutRecord) -> Vec<String> {
    match &layout.segments {
        SegmentSequence::Inline(_) => Vec::new(),
        SegmentSequence::Paged { root, .. } => vec![root.as_str().to_string()],
    }
}

pub(super) fn segment_children(page: &SegmentPage) -> Vec<String> {
    let ids = match page {
        SegmentPage::Leaf { .. } => Vec::new(),
        SegmentPage::Index { children, .. } => children
            .iter()
            .map(|child| child.page_id.as_str().to_string())
            .collect(),
    };
    ids.into_iter()
        .collect::<BTreeSet<_>>()
        .into_iter()
        .collect()
}

use std::collections::{BTreeMap, BTreeSet};

use bowline_core::{
    ids::NamespacePageId,
    namespace_snapshot::{NamespaceOperationContext, NamespaceReadError},
};

use super::{
    codec::{
        NamespaceEntryValue, NamespacePage, decode_namespace_page, encode_namespace_page,
        logical_id,
    },
    types::{
        MAX_NAMESPACE_DEPTH, MetadataRecordKind, NAMESPACE_PAGE_TARGET_BYTES, PageStore,
        PagedRecordSource,
    },
};

pub(crate) type KeyedValue = (Vec<u8>, NamespaceEntryValue);

#[path = "tree/mutation.rs"]
mod mutation;
pub(crate) use mutation::{TreeMutation, mutate_tree, paths_for_prefix};

#[derive(Clone, Copy)]
struct TreeSource<'a> {
    workspace_id: &'a str,
    store: &'a dyn PagedRecordSource,
}

struct WalkFrame<'a> {
    base: &'a [u8],
    depth: usize,
}

#[derive(Clone, Copy)]
struct DiffSource<'a> {
    workspace_id: &'a str,
    left: (&'a NamespacePageId, &'a dyn PagedRecordSource),
    right: (&'a NamespacePageId, &'a dyn PagedRecordSource),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum RawNamespaceDiff {
    Added(KeyedValue),
    Removed(KeyedValue),
    Modified {
        before: KeyedValue,
        after: KeyedValue,
    },
}

pub(crate) fn build_tree(
    workspace_id: &str,
    entries: Vec<KeyedValue>,
    store: &mut PageStore,
    context: &mut NamespaceOperationContext<'_>,
) -> Result<NamespacePageId, NamespaceReadError> {
    validate_sorted_entries(&entries)?;
    build_node(workspace_id, entries, store, context, 0)
}

fn build_node(
    workspace_id: &str,
    entries: Vec<KeyedValue>,
    store: &mut PageStore,
    context: &mut NamespaceOperationContext<'_>,
    depth: usize,
) -> Result<NamespacePageId, NamespaceReadError> {
    context.ensure_active()?;
    if depth > MAX_NAMESPACE_DEPTH {
        return Err(NamespaceReadError::CorruptGraph {
            reason: "namespace tree exceeds maximum depth",
        });
    }
    let common_prefix = longest_common_prefix(&entries);
    let leaf = NamespacePage::Leaf {
        common_prefix: common_prefix.clone(),
        entries: entries
            .iter()
            .map(|(key, value)| (key[common_prefix.len()..].to_vec(), value.clone()))
            .collect(),
    };
    match encode_namespace_page(&leaf) {
        Ok(bytes) if bytes.len() <= NAMESPACE_PAGE_TARGET_BYTES || entries.len() <= 1 => {
            return insert_page(workspace_id, bytes, store);
        }
        Err(error) if entries.len() <= 1 => return Err(error),
        Ok(_) | Err(NamespaceReadError::OversizedRecord { .. }) => {}
        Err(error) => return Err(error),
    }

    let mut value = None;
    let mut groups = BTreeMap::<u8, Vec<KeyedValue>>::new();
    for (key, entry_value) in entries {
        let remaining = &key[common_prefix.len()..];
        if remaining.is_empty() {
            value = Some(entry_value);
            continue;
        }
        groups
            .entry(remaining[0])
            .or_default()
            .push((remaining[1..].to_vec(), entry_value));
    }
    let mut children = Vec::with_capacity(groups.len());
    for (edge, child_entries) in groups {
        let child_id = build_node(workspace_id, child_entries, store, context, depth + 1)?;
        children.push((edge, child_id));
    }
    if children.len() == 1 && value.is_none() {
        return Err(NamespaceReadError::CorruptGraph {
            reason: "canonical radix construction failed to compress a single child",
        });
    }
    let page = NamespacePage::Branch {
        common_prefix,
        children,
        value,
    };
    insert_page(workspace_id, encode_namespace_page(&page)?, store)
}

pub(crate) fn insert_page(
    _workspace_id: &str,
    bytes: Vec<u8>,
    store: &mut PageStore,
) -> Result<NamespacePageId, NamespaceReadError> {
    let id = NamespacePageId::new(logical_id("nsp", store.identity_key(), &bytes));
    store.insert_namespace_page(id.clone(), bytes)?;
    Ok(id)
}

pub(crate) fn load_page(
    _workspace_id: &str,
    id: &NamespacePageId,
    store: &dyn PagedRecordSource,
    context: &mut NamespaceOperationContext<'_>,
) -> Result<NamespacePage, NamespaceReadError> {
    let bytes = store
        .load_record(MetadataRecordKind::NamespacePage, id.as_str(), context)?
        .ok_or(NamespaceReadError::MissingRecord {
            record: "namespace page",
        })?;
    context.charge_namespace_page(bytes.len() as u64)?;
    if logical_id("nsp", store.metadata_identity_key(), &bytes) != id.as_str() {
        return Err(NamespaceReadError::CorruptGraph {
            reason: "namespace page logical ID mismatch",
        });
    }
    decode_namespace_page(&bytes)
}

pub(crate) fn get_value(
    workspace_id: &str,
    root: &NamespacePageId,
    store: &dyn PagedRecordSource,
    path: &[u8],
    context: &mut NamespaceOperationContext<'_>,
) -> Result<Option<NamespaceEntryValue>, NamespaceReadError> {
    let mut current = root.clone();
    let mut remaining = path;
    let mut seen = BTreeSet::new();
    for _ in 0..=MAX_NAMESPACE_DEPTH {
        if !seen.insert(current.clone()) {
            return Err(NamespaceReadError::CorruptGraph {
                reason: "cycle in namespace page graph",
            });
        }
        let page = load_page(workspace_id, &current, store, context)?;
        match page {
            NamespacePage::Leaf {
                common_prefix,
                entries,
            } => {
                let Some(suffix) = remaining.strip_prefix(common_prefix.as_slice()) else {
                    return Ok(None);
                };
                return Ok(entries
                    .binary_search_by(|(key, _)| key.as_slice().cmp(suffix))
                    .ok()
                    .map(|index| entries[index].1.clone()));
            }
            NamespacePage::Branch {
                common_prefix,
                children,
                value,
            } => {
                let Some(suffix) = remaining.strip_prefix(common_prefix.as_slice()) else {
                    return Ok(None);
                };
                let Some((&edge, child_suffix)) = suffix.split_first() else {
                    return Ok(value);
                };
                let Ok(index) = children.binary_search_by_key(&edge, |(child_edge, _)| *child_edge)
                else {
                    return Ok(None);
                };
                current = children[index].1.clone();
                remaining = child_suffix;
            }
        }
    }
    Err(NamespaceReadError::CorruptGraph {
        reason: "namespace page graph exceeds maximum depth",
    })
}

pub(crate) fn visit_prefix_values(
    workspace_id: &str,
    root: &NamespacePageId,
    store: &dyn PagedRecordSource,
    prefix: &[u8],
    context: &mut NamespaceOperationContext<'_>,
    visitor: &mut dyn FnMut(
        KeyedValue,
        &mut NamespaceOperationContext<'_>,
    ) -> Result<bool, NamespaceReadError>,
) -> Result<bool, NamespaceReadError> {
    let mut active = BTreeSet::new();
    visit_subtree(
        TreeSource {
            workspace_id,
            store,
        },
        root,
        WalkFrame {
            base: &[],
            depth: 0,
        },
        prefix,
        &mut active,
        context,
        visitor,
    )
}

fn visit_subtree(
    source: TreeSource<'_>,
    id: &NamespacePageId,
    frame: WalkFrame<'_>,
    prefix: &[u8],
    active: &mut BTreeSet<NamespacePageId>,
    context: &mut NamespaceOperationContext<'_>,
    visitor: &mut dyn FnMut(
        KeyedValue,
        &mut NamespaceOperationContext<'_>,
    ) -> Result<bool, NamespaceReadError>,
) -> Result<bool, NamespaceReadError> {
    if frame.depth > MAX_NAMESPACE_DEPTH || !active.insert(id.clone()) {
        return Err(NamespaceReadError::CorruptGraph {
            reason: "cycle or excessive depth in namespace page graph",
        });
    }
    let page = load_page(source.workspace_id, id, source.store, context)?;
    let stopped = match page {
        NamespacePage::Leaf {
            common_prefix,
            entries,
        } => {
            let node_base = joined(frame.base, &common_prefix);
            if !prefix_intersects(&node_base, prefix) {
                false
            } else {
                let mut stopped = false;
                for (suffix, value) in entries {
                    let path = joined(&node_base, &suffix);
                    if component_prefix(&path, prefix) && visitor((path, value), context)? {
                        stopped = true;
                        break;
                    }
                }
                stopped
            }
        }
        NamespacePage::Branch {
            common_prefix,
            children,
            value,
        } => {
            let node_base = joined(frame.base, &common_prefix);
            if !prefix_intersects(&node_base, prefix) {
                false
            } else {
                if let Some(value) = value
                    && component_prefix(&node_base, prefix)
                    && visitor((node_base.clone(), value), context)?
                {
                    active.remove(id);
                    return Ok(true);
                }
                let relevant_children = children
                    .into_iter()
                    .filter_map(|(edge, child)| {
                        let child_base = joined(&node_base, &[edge]);
                        prefix_intersects(&child_base, prefix).then_some((child, child_base))
                    })
                    .collect::<Vec<_>>();
                source.store.prefetch_records(
                    MetadataRecordKind::NamespacePage,
                    &relevant_children
                        .iter()
                        .map(|(child, _)| child.as_str().to_string())
                        .collect::<Vec<_>>(),
                    context,
                )?;
                let mut stopped = false;
                for (child, child_base) in relevant_children {
                    if visit_subtree(
                        source,
                        &child,
                        WalkFrame {
                            base: &child_base,
                            depth: frame.depth + 1,
                        },
                        prefix,
                        active,
                        context,
                        visitor,
                    )? {
                        stopped = true;
                        break;
                    }
                }
                stopped
            }
        }
    };
    active.remove(id);
    Ok(stopped)
}

pub(crate) fn diff_values(
    workspace_id: &str,
    left: (&NamespacePageId, &dyn PagedRecordSource),
    right: (&NamespacePageId, &dyn PagedRecordSource),
    scope: &[u8],
    context: &mut NamespaceOperationContext<'_>,
    visitor: &mut dyn FnMut(
        RawNamespaceDiff,
        &mut NamespaceOperationContext<'_>,
    ) -> Result<(), NamespaceReadError>,
) -> Result<(), NamespaceReadError> {
    let mut active = BTreeSet::new();
    diff_nodes(
        DiffSource {
            workspace_id,
            left,
            right,
        },
        WalkFrame {
            base: &[],
            depth: 0,
        },
        scope,
        &mut active,
        context,
        visitor,
    )
}

fn diff_nodes(
    source: DiffSource<'_>,
    frame: WalkFrame<'_>,
    scope: &[u8],
    active: &mut BTreeSet<(NamespacePageId, NamespacePageId)>,
    context: &mut NamespaceOperationContext<'_>,
    visitor: &mut dyn FnMut(
        RawNamespaceDiff,
        &mut NamespaceOperationContext<'_>,
    ) -> Result<(), NamespaceReadError>,
) -> Result<(), NamespaceReadError> {
    if source.left.0 == source.right.0 {
        return Ok(());
    }
    if frame.depth > MAX_NAMESPACE_DEPTH
        || !active.insert((source.left.0.clone(), source.right.0.clone()))
    {
        return Err(NamespaceReadError::CorruptGraph {
            reason: "cycle or excessive depth during namespace diff",
        });
    }
    let left_page = load_page(source.workspace_id, source.left.0, source.left.1, context)?;
    let right_page = load_page(source.workspace_id, source.right.0, source.right.1, context)?;
    match (&left_page, &right_page) {
        (
            NamespacePage::Branch {
                common_prefix: left_prefix,
                children: left_children,
                value: left_value,
            },
            NamespacePage::Branch {
                common_prefix: right_prefix,
                children: right_children,
                value: right_value,
            },
        ) if left_prefix == right_prefix => {
            let node_base = joined(frame.base, left_prefix);
            compare_optional_values(&node_base, scope, left_value, right_value, context, visitor)?;
            let anticipated_left = left_children
                .iter()
                .filter(|(edge, _)| prefix_intersects(&joined(&node_base, &[*edge]), scope))
                .map(|(_, id)| id.as_str().to_string())
                .collect::<Vec<_>>();
            source.left.1.prefetch_records(
                MetadataRecordKind::NamespacePage,
                &anticipated_left,
                context,
            )?;
            let anticipated_right = right_children
                .iter()
                .filter(|(edge, _)| prefix_intersects(&joined(&node_base, &[*edge]), scope))
                .map(|(_, id)| id.as_str().to_string())
                .collect::<Vec<_>>();
            source.right.1.prefetch_records(
                MetadataRecordKind::NamespacePage,
                &anticipated_right,
                context,
            )?;
            let mut left_index = 0;
            let mut right_index = 0;
            while left_index < left_children.len() || right_index < right_children.len() {
                match (
                    left_children.get(left_index),
                    right_children.get(right_index),
                ) {
                    (Some((left_edge, left_id)), Some((right_edge, right_id)))
                        if left_edge == right_edge =>
                    {
                        let child_base = joined(&node_base, &[*left_edge]);
                        if prefix_intersects(&child_base, scope) {
                            diff_nodes(
                                DiffSource {
                                    workspace_id: source.workspace_id,
                                    left: (left_id, source.left.1),
                                    right: (right_id, source.right.1),
                                },
                                WalkFrame {
                                    base: &child_base,
                                    depth: frame.depth + 1,
                                },
                                scope,
                                active,
                                context,
                                visitor,
                            )?;
                        }
                        left_index += 1;
                        right_index += 1;
                    }
                    (Some((left_edge, left_id)), Some((right_edge, _)))
                        if left_edge < right_edge =>
                    {
                        collect_one_side(
                            TreeSource {
                                workspace_id: source.workspace_id,
                                store: source.left.1,
                            },
                            left_id,
                            &joined(&node_base, &[*left_edge]),
                            scope,
                            true,
                            context,
                            visitor,
                        )?;
                        left_index += 1;
                    }
                    (Some(_), Some((right_edge, right_id))) => {
                        collect_one_side(
                            TreeSource {
                                workspace_id: source.workspace_id,
                                store: source.right.1,
                            },
                            right_id,
                            &joined(&node_base, &[*right_edge]),
                            scope,
                            false,
                            context,
                            visitor,
                        )?;
                        right_index += 1;
                    }
                    (Some((edge, id)), None) => {
                        collect_one_side(
                            TreeSource {
                                workspace_id: source.workspace_id,
                                store: source.left.1,
                            },
                            id,
                            &joined(&node_base, &[*edge]),
                            scope,
                            true,
                            context,
                            visitor,
                        )?;
                        left_index += 1;
                    }
                    (None, Some((edge, id))) => {
                        collect_one_side(
                            TreeSource {
                                workspace_id: source.workspace_id,
                                store: source.right.1,
                            },
                            id,
                            &joined(&node_base, &[*edge]),
                            scope,
                            false,
                            context,
                            visitor,
                        )?;
                        right_index += 1;
                    }
                    (None, None) => break,
                }
            }
        }
        _ => compare_collected(
            source.workspace_id,
            source.left,
            source.right,
            frame.base,
            scope,
            context,
            visitor,
        )?,
    }
    active.remove(&(source.left.0.clone(), source.right.0.clone()));
    Ok(())
}

fn compare_optional_values(
    path: &[u8],
    scope: &[u8],
    left: &Option<NamespaceEntryValue>,
    right: &Option<NamespaceEntryValue>,
    context: &mut NamespaceOperationContext<'_>,
    visitor: &mut dyn FnMut(
        RawNamespaceDiff,
        &mut NamespaceOperationContext<'_>,
    ) -> Result<(), NamespaceReadError>,
) -> Result<(), NamespaceReadError> {
    if !component_prefix(path, scope) {
        return Ok(());
    }
    let path = path.to_vec();
    match (left, right) {
        (Some(before), Some(after)) if before != after => {
            context.charge_diff_entries(1)?;
            visitor(
                RawNamespaceDiff::Modified {
                    before: (path.clone(), before.clone()),
                    after: (path, after.clone()),
                },
                context,
            )?;
        }
        (Some(before), None) => {
            context.charge_diff_entries(1)?;
            visitor(RawNamespaceDiff::Removed((path, before.clone())), context)?
        }
        (None, Some(after)) => {
            context.charge_diff_entries(1)?;
            visitor(RawNamespaceDiff::Added((path, after.clone())), context)?;
        }
        _ => {}
    }
    Ok(())
}

fn collect_one_side(
    source: TreeSource<'_>,
    id: &NamespacePageId,
    base: &[u8],
    scope: &[u8],
    removed: bool,
    context: &mut NamespaceOperationContext<'_>,
    visitor: &mut dyn FnMut(
        RawNamespaceDiff,
        &mut NamespaceOperationContext<'_>,
    ) -> Result<(), NamespaceReadError>,
) -> Result<(), NamespaceReadError> {
    let mut active = BTreeSet::new();
    visit_subtree(
        source,
        id,
        WalkFrame { base, depth: 0 },
        scope,
        &mut active,
        context,
        &mut |value, context| {
            context.charge_diff_entries(1)?;
            visitor(
                if removed {
                    RawNamespaceDiff::Removed(value)
                } else {
                    RawNamespaceDiff::Added(value)
                },
                context,
            )?;
            Ok(false)
        },
    )
    .map(|_| ())
}

fn compare_collected(
    workspace_id: &str,
    left: (&NamespacePageId, &dyn PagedRecordSource),
    right: (&NamespacePageId, &dyn PagedRecordSource),
    base: &[u8],
    scope: &[u8],
    context: &mut NamespaceOperationContext<'_>,
    visitor: &mut dyn FnMut(
        RawNamespaceDiff,
        &mut NamespaceOperationContext<'_>,
    ) -> Result<(), NamespaceReadError>,
) -> Result<(), NamespaceReadError> {
    let left_source = TreeSource {
        workspace_id,
        store: left.1,
    };
    let right_source = TreeSource {
        workspace_id,
        store: right.1,
    };
    let mut active = BTreeSet::new();
    visit_subtree(
        left_source,
        left.0,
        WalkFrame { base, depth: 0 },
        scope,
        &mut active,
        context,
        &mut |(path, before), context| {
            context.charge_diff_entries(1)?;
            let relative = path
                .strip_prefix(base)
                .ok_or(NamespaceReadError::CorruptGraph {
                    reason: "namespace diff path escaped its subtree",
                })?;
            match get_value(workspace_id, right.0, right.1, relative, context)? {
                Some(after) if before != after => visitor(
                    RawNamespaceDiff::Modified {
                        before: (path.clone(), before),
                        after: (path, after),
                    },
                    context,
                )?,
                None => visitor(RawNamespaceDiff::Removed((path, before)), context)?,
                Some(_) => {}
            }
            Ok(false)
        },
    )?;
    visit_subtree(
        right_source,
        right.0,
        WalkFrame { base, depth: 0 },
        scope,
        &mut active,
        context,
        &mut |(path, after), context| {
            let relative = path
                .strip_prefix(base)
                .ok_or(NamespaceReadError::CorruptGraph {
                    reason: "namespace diff path escaped its subtree",
                })?;
            if get_value(workspace_id, left.0, left.1, relative, context)?.is_none() {
                context.charge_diff_entries(1)?;
                visitor(RawNamespaceDiff::Added((path, after)), context)?;
            }
            Ok(false)
        },
    )
    .map(|_| ())
}

fn longest_common_prefix(entries: &[KeyedValue]) -> Vec<u8> {
    let Some((first, _)) = entries.first() else {
        return Vec::new();
    };
    let Some((last, _)) = entries.last() else {
        return Vec::new();
    };
    first
        .iter()
        .zip(last)
        .take_while(|(left, right)| left == right)
        .map(|(byte, _)| *byte)
        .collect()
}

fn validate_sorted_entries(entries: &[KeyedValue]) -> Result<(), NamespaceReadError> {
    let mut previous: Option<&[u8]> = None;
    for (key, _) in entries {
        if previous.is_some_and(|prior| prior >= key.as_slice()) {
            return Err(NamespaceReadError::NonCanonicalOrder { field: "path" });
        }
        previous = Some(key);
    }
    Ok(())
}

fn joined(left: &[u8], right: &[u8]) -> Vec<u8> {
    let mut joined = Vec::with_capacity(left.len() + right.len());
    joined.extend_from_slice(left);
    joined.extend_from_slice(right);
    joined
}

fn prefix_intersects(node: &[u8], prefix: &[u8]) -> bool {
    if node.len() >= prefix.len() {
        component_prefix(node, prefix)
    } else {
        prefix.starts_with(node)
    }
}

fn component_prefix(path: &[u8], prefix: &[u8]) -> bool {
    prefix.is_empty()
        || path == prefix
        || path
            .strip_prefix(prefix)
            .is_some_and(|remaining| remaining.first() == Some(&b'/'))
}

#[cfg(test)]
pub(crate) fn verify_graph_shape_without_identity(
    root: &NamespacePageId,
    store: &PageStore,
) -> Result<(), NamespaceReadError> {
    fn walk(
        id: &NamespacePageId,
        store: &PageStore,
        active: &mut BTreeSet<NamespacePageId>,
        depth: usize,
    ) -> Result<(), NamespaceReadError> {
        if depth > MAX_NAMESPACE_DEPTH || !active.insert(id.clone()) {
            return Err(NamespaceReadError::CorruptGraph {
                reason: "cycle or excessive depth in namespace page graph",
            });
        }
        let bytes = store
            .namespace_page_bytes(id)
            .ok_or(NamespaceReadError::MissingRecord {
                record: "namespace page",
            })?;
        if let NamespacePage::Branch { children, .. } = decode_namespace_page(bytes)? {
            for (_, child) in children {
                walk(&child, store, active, depth + 1)?;
            }
        }
        active.remove(id);
        Ok(())
    }

    walk(root, store, &mut BTreeSet::new(), 0)
}

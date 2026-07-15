use bowline_core::{
    ids::NamespacePageId,
    namespace_snapshot::{NamespaceOperationContext, NamespaceReadError},
};

use super::{
    KeyedValue, build_tree, insert_page, joined, load_page, longest_common_prefix,
    visit_prefix_values,
};
use crate::sync::namespace::paged::{
    codec::{NamespaceEntryValue, NamespacePage, encode_namespace_page},
    types::{MAX_NAMESPACE_DEPTH, NAMESPACE_PAGE_TARGET_BYTES, PageStore},
};

pub(crate) enum TreeMutation {
    Upsert(NamespaceEntryValue),
    Remove,
}

struct BranchMutationRequest<'a> {
    workspace_id: &'a str,
    id: &'a NamespacePageId,
    common_prefix: Vec<u8>,
    children: Vec<(u8, NamespacePageId)>,
    value: Option<NamespaceEntryValue>,
    key: &'a [u8],
    mutation: &'a TreeMutation,
    depth: usize,
}

struct LeafMutationRequest<'a> {
    workspace_id: &'a str,
    id: &'a NamespacePageId,
    common_prefix: Vec<u8>,
    entries: Vec<KeyedValue>,
    key: &'a [u8],
    mutation: &'a TreeMutation,
}

pub(crate) fn mutate_tree(
    workspace_id: &str,
    root: &NamespacePageId,
    key: &[u8],
    mutation: TreeMutation,
    store: &mut PageStore,
    context: &mut NamespaceOperationContext<'_>,
) -> Result<NamespacePageId, NamespaceReadError> {
    let (next, _) = mutate_node(workspace_id, root, key, &mutation, store, context, 0)?;
    match next {
        Some(root) => Ok(root),
        None => build_tree(workspace_id, Vec::new(), store, context),
    }
}

pub(crate) fn paths_for_prefix(
    workspace_id: &str,
    root: &NamespacePageId,
    store: &PageStore,
    prefix: &[u8],
    context: &mut NamespaceOperationContext<'_>,
) -> Result<Vec<Vec<u8>>, NamespaceReadError> {
    let mut paths = Vec::new();
    visit_prefix_values(
        workspace_id,
        root,
        store,
        prefix,
        context,
        &mut |(path, _), context| {
            context.charge_entries(1)?;
            paths.push(path);
            Ok(false)
        },
    )?;
    Ok(paths)
}

fn mutate_node(
    workspace_id: &str,
    id: &NamespacePageId,
    key: &[u8],
    mutation: &TreeMutation,
    store: &mut PageStore,
    context: &mut NamespaceOperationContext<'_>,
    depth: usize,
) -> Result<(Option<NamespacePageId>, bool), NamespaceReadError> {
    if depth > MAX_NAMESPACE_DEPTH {
        return Err(NamespaceReadError::CorruptGraph {
            reason: "namespace mutation exceeds maximum depth",
        });
    }
    let page = load_page(workspace_id, id, store, context)?;
    match page {
        NamespacePage::Leaf {
            common_prefix,
            entries,
        } => mutate_leaf(
            LeafMutationRequest {
                workspace_id,
                id,
                common_prefix,
                entries,
                key,
                mutation,
            },
            store,
            context,
        ),
        NamespacePage::Branch {
            common_prefix,
            children,
            value,
        } => mutate_branch(
            BranchMutationRequest {
                workspace_id,
                id,
                common_prefix,
                children,
                value,
                key,
                mutation,
                depth,
            },
            store,
            context,
        ),
    }
}

fn mutate_leaf(
    request: LeafMutationRequest<'_>,
    store: &mut PageStore,
    context: &mut NamespaceOperationContext<'_>,
) -> Result<(Option<NamespacePageId>, bool), NamespaceReadError> {
    let LeafMutationRequest {
        workspace_id,
        id,
        common_prefix,
        entries,
        key,
        mutation,
    } = request;
    let mut values = entries
        .into_iter()
        .map(|(suffix, value)| (joined(&common_prefix, &suffix), value))
        .collect::<Vec<_>>();
    let position = values.binary_search_by(|(path, _)| path.as_slice().cmp(key));
    let changed = match (position, mutation) {
        (Ok(index), TreeMutation::Upsert(value)) if values[index].1 != *value => {
            values[index].1 = value.clone();
            true
        }
        (Ok(_), TreeMutation::Upsert(_)) | (Err(_), TreeMutation::Remove) => false,
        (Ok(index), TreeMutation::Remove) => {
            values.remove(index);
            true
        }
        (Err(index), TreeMutation::Upsert(value)) => {
            values.insert(index, (key.to_vec(), value.clone()));
            true
        }
    };
    if !changed {
        return Ok((Some(id.clone()), false));
    }
    if values.is_empty() {
        return Ok((None, true));
    }
    Ok((
        Some(build_tree(workspace_id, values, store, context)?),
        true,
    ))
}

fn mutate_branch(
    request: BranchMutationRequest<'_>,
    store: &mut PageStore,
    context: &mut NamespaceOperationContext<'_>,
) -> Result<(Option<NamespacePageId>, bool), NamespaceReadError> {
    let BranchMutationRequest {
        workspace_id,
        id,
        common_prefix,
        mut children,
        mut value,
        key,
        mutation,
        depth,
    } = request;
    let shared = common_prefix_len(&common_prefix, key);
    if shared < common_prefix.len() {
        if matches!(mutation, TreeMutation::Remove) {
            return Ok((Some(id.clone()), false));
        }
        let TreeMutation::Upsert(new_value) = mutation else {
            return Ok((Some(id.clone()), false));
        };
        let old_remaining = &common_prefix[shared..];
        let old_child = rebase_page(workspace_id, id, &old_remaining[1..], store, context)?;
        let new_remaining = &key[shared..];
        let mut branch_value = None;
        let mut branch_children = vec![(old_remaining[0], old_child)];
        if new_remaining.is_empty() {
            branch_value = Some(new_value.clone());
        } else {
            let new_child = build_tree(
                workspace_id,
                vec![(new_remaining[1..].to_vec(), new_value.clone())],
                store,
                context,
            )?;
            branch_children.push((new_remaining[0], new_child));
            branch_children.sort_by_key(|(edge, _)| *edge);
        }
        let branch = NamespacePage::Branch {
            common_prefix: common_prefix[..shared].to_vec(),
            children: branch_children,
            value: branch_value,
        };
        return Ok((
            Some(insert_page(
                workspace_id,
                encode_namespace_page(&branch)?,
                store,
            )?),
            true,
        ));
    }

    let remaining = &key[common_prefix.len()..];
    let changed = if remaining.is_empty() {
        match mutation {
            TreeMutation::Upsert(next) if value.as_ref() != Some(next) => {
                value = Some(next.clone());
                true
            }
            TreeMutation::Remove if value.is_some() => {
                value = None;
                true
            }
            _ => false,
        }
    } else {
        let edge = remaining[0];
        match children.binary_search_by_key(&edge, |(child_edge, _)| *child_edge) {
            Ok(index) => {
                let (next, child_changed) = mutate_node(
                    workspace_id,
                    &children[index].1,
                    &remaining[1..],
                    mutation,
                    store,
                    context,
                    depth + 1,
                )?;
                if child_changed {
                    match next {
                        Some(next) => children[index].1 = next,
                        None => {
                            children.remove(index);
                        }
                    }
                }
                child_changed
            }
            Err(index) => match mutation {
                TreeMutation::Remove => false,
                TreeMutation::Upsert(next) => {
                    let child = build_tree(
                        workspace_id,
                        vec![(remaining[1..].to_vec(), next.clone())],
                        store,
                        context,
                    )?;
                    children.insert(index, (edge, child));
                    true
                }
            },
        }
    };
    if !changed {
        return Ok((Some(id.clone()), false));
    }
    normalize_branch(workspace_id, common_prefix, children, value, store, context)
        .map(|next| (next, true))
}

fn normalize_branch(
    workspace_id: &str,
    common_prefix: Vec<u8>,
    children: Vec<(u8, NamespacePageId)>,
    value: Option<NamespaceEntryValue>,
    store: &mut PageStore,
    context: &mut NamespaceOperationContext<'_>,
) -> Result<Option<NamespacePageId>, NamespaceReadError> {
    if children.is_empty() && value.is_none() {
        return Ok(None);
    }
    if children.len() == 1 && value.is_none() {
        let (edge, child) = &children[0];
        let child_page = load_page(workspace_id, child, store, context)?;
        let combined = joined(&common_prefix, &[*edge]);
        return Ok(Some(prepend_common_prefix(
            workspace_id,
            child_page,
            &combined,
            store,
        )?));
    }
    let branch = NamespacePage::Branch {
        common_prefix,
        children,
        value,
    };
    let branch_id = insert_page(workspace_id, encode_namespace_page(&branch)?, store)?;
    if let Some(leaf_id) = collapse_to_leaf_if_small(workspace_id, &branch_id, store, context)? {
        return Ok(Some(leaf_id));
    }
    Ok(Some(branch_id))
}

fn collapse_to_leaf_if_small(
    workspace_id: &str,
    id: &NamespacePageId,
    store: &mut PageStore,
    context: &mut NamespaceOperationContext<'_>,
) -> Result<Option<NamespacePageId>, NamespaceReadError> {
    let mut values = Vec::new();
    let stopped = visit_prefix_values(workspace_id, id, store, &[], context, &mut |value, _| {
        values.push(value);
        Ok(leaf_bytes(&values).is_none())
    })?;
    if stopped {
        return Ok(None);
    }
    let Some(bytes) = leaf_bytes(&values) else {
        return Ok(None);
    };
    Ok(Some(insert_page(workspace_id, bytes, store)?))
}

fn leaf_bytes(entries: &[KeyedValue]) -> Option<Vec<u8>> {
    let common_prefix = longest_common_prefix(entries);
    let leaf = NamespacePage::Leaf {
        common_prefix: common_prefix.clone(),
        entries: entries
            .iter()
            .map(|(key, value)| (key[common_prefix.len()..].to_vec(), value.clone()))
            .collect(),
    };
    encode_namespace_page(&leaf)
        .ok()
        .filter(|bytes| bytes.len() <= NAMESPACE_PAGE_TARGET_BYTES || entries.len() <= 1)
}

fn rebase_page(
    workspace_id: &str,
    id: &NamespacePageId,
    common_prefix: &[u8],
    store: &mut PageStore,
    context: &mut NamespaceOperationContext<'_>,
) -> Result<NamespacePageId, NamespaceReadError> {
    let page = load_page(workspace_id, id, store, context)?;
    replace_common_prefix(workspace_id, page, common_prefix.to_vec(), store)
}

fn prepend_common_prefix(
    workspace_id: &str,
    page: NamespacePage,
    prefix: &[u8],
    store: &mut PageStore,
) -> Result<NamespacePageId, NamespaceReadError> {
    let common_prefix = match &page {
        NamespacePage::Leaf { common_prefix, .. } | NamespacePage::Branch { common_prefix, .. } => {
            joined(prefix, common_prefix)
        }
    };
    replace_common_prefix(workspace_id, page, common_prefix, store)
}

fn replace_common_prefix(
    workspace_id: &str,
    page: NamespacePage,
    common_prefix: Vec<u8>,
    store: &mut PageStore,
) -> Result<NamespacePageId, NamespaceReadError> {
    let page = match page {
        NamespacePage::Leaf { entries, .. } => NamespacePage::Leaf {
            common_prefix,
            entries,
        },
        NamespacePage::Branch {
            children, value, ..
        } => NamespacePage::Branch {
            common_prefix,
            children,
            value,
        },
    };
    insert_page(workspace_id, encode_namespace_page(&page)?, store)
}

fn common_prefix_len(left: &[u8], right: &[u8]) -> usize {
    left.iter()
        .zip(right)
        .take_while(|(left, right)| left == right)
        .count()
}

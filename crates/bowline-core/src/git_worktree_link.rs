use std::path::{Component, Path};

use crate::workspace_graph::NamespaceEntryKind;

/// In-band marker standing in for "the workspace root on this machine".
pub const WORKSPACE_ROOT_MARKER: &str = "${BOWLINE_WORKSPACE_ROOT}";

const GITDIR_PREFIX: &str = "gitdir: ";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WorktreeLinkFile {
    Gitlink,
    AdminPointer,
}

pub fn is_worktree_gitlink_file(path: &str, kind: NamespaceEntryKind) -> bool {
    kind == NamespaceEntryKind::File
        && path.split('/').rfind(|component| !component.is_empty()) == Some(".git")
}

pub fn is_worktree_admin_pointer(path: &str) -> bool {
    let Some(tail) = git_tail_components(path) else {
        return false;
    };
    matches!(
        tail.as_slice(),
        ["worktrees", name, "gitdir" | "commondir"] if !name.is_empty()
    )
}

pub fn worktree_registration_prefix(path: &str) -> Option<String> {
    let components = path
        .split('/')
        .filter(|component| !component.is_empty())
        .collect::<Vec<_>>();
    let git_index = components
        .iter()
        .position(|component| *component == ".git")?;
    let tail = components.get(git_index + 1..)?;
    match tail {
        ["worktrees", name, ..] if !name.is_empty() => {
            Some(format!("{}/", components[..=git_index + 2].join("/")))
        }
        _ => None,
    }
}

pub fn worktree_link_file(path: &str, kind: NamespaceEntryKind) -> Option<WorktreeLinkFile> {
    if is_worktree_gitlink_file(path, kind) {
        return Some(WorktreeLinkFile::Gitlink);
    }
    if kind == NamespaceEntryKind::File && is_worktree_admin_pointer(path) {
        return Some(WorktreeLinkFile::AdminPointer);
    }
    None
}

pub fn normalize_worktree_link_entry_bytes(
    path: &str,
    kind: NamespaceEntryKind,
    bytes: &[u8],
    root: &Path,
) -> Vec<u8> {
    let Some(file) = worktree_link_file(path, kind) else {
        return bytes.to_vec();
    };
    normalize_worktree_link_bytes(bytes, root, file)
}

pub fn denormalize_worktree_link_entry_bytes(
    path: &str,
    kind: NamespaceEntryKind,
    bytes: &[u8],
    root: &Path,
) -> Vec<u8> {
    let Some(file) = worktree_link_file(path, kind) else {
        return bytes.to_vec();
    };
    denormalize_worktree_link_bytes(bytes, root, file)
}

/// True when bytes are a strict-shape worktree admin pointer whose absolute
/// target cannot follow the workspace root to another machine.
pub fn is_out_of_root_admin_target(bytes: &[u8], root: &Path) -> bool {
    let Ok(text) = std::str::from_utf8(bytes) else {
        return false;
    };
    let Some((line, _)) = split_single_line(text) else {
        return false;
    };
    let Some((_, path_token)) = parse_path_line(line, WorktreeLinkFile::AdminPointer) else {
        return false;
    };
    let path = Path::new(path_token);
    path.is_absolute() && path.strip_prefix(root).is_err()
}

fn normalize_worktree_link_bytes(bytes: &[u8], root: &Path, file: WorktreeLinkFile) -> Vec<u8> {
    transform_worktree_link_bytes(bytes, root, file, normalize_path_token)
}

fn denormalize_worktree_link_bytes(bytes: &[u8], root: &Path, file: WorktreeLinkFile) -> Vec<u8> {
    transform_worktree_link_bytes(bytes, root, file, denormalize_path_token)
}

fn git_tail_components(path: &str) -> Option<Vec<&str>> {
    let components = path
        .split('/')
        .filter(|component| !component.is_empty())
        .collect::<Vec<_>>();
    let git_index = components
        .iter()
        .position(|component| *component == ".git")?;
    Some(components.into_iter().skip(git_index + 1).collect())
}

fn transform_worktree_link_bytes(
    bytes: &[u8],
    root: &Path,
    file: WorktreeLinkFile,
    transform: fn(&str, &Path) -> Option<String>,
) -> Vec<u8> {
    let Ok(text) = std::str::from_utf8(bytes) else {
        return bytes.to_vec();
    };
    let Some((line, trailing_newline)) = split_single_line(text) else {
        return bytes.to_vec();
    };
    let Some((prefix, path_token)) = parse_path_line(line, file) else {
        return bytes.to_vec();
    };
    let Some(transformed_path) = transform(path_token, root) else {
        return bytes.to_vec();
    };
    let mut output = String::with_capacity(prefix.len() + transformed_path.len() + 1);
    output.push_str(prefix);
    output.push_str(&transformed_path);
    if trailing_newline {
        output.push('\n');
    }
    output.into_bytes()
}

fn split_single_line(text: &str) -> Option<(&str, bool)> {
    if let Some(line) = text.strip_suffix('\n') {
        if line.contains('\n') || line.is_empty() {
            return None;
        }
        return Some((line, true));
    }
    if text.contains('\n') || text.is_empty() {
        return None;
    }
    Some((text, false))
}

fn parse_path_line(line: &str, file: WorktreeLinkFile) -> Option<(&str, &str)> {
    match file {
        WorktreeLinkFile::Gitlink => {
            let path_token = line.strip_prefix(GITDIR_PREFIX)?;
            if path_token.is_empty() {
                return None;
            }
            Some((GITDIR_PREFIX, path_token))
        }
        WorktreeLinkFile::AdminPointer => {
            if line.starts_with(GITDIR_PREFIX) {
                return None;
            }
            Some(("", line))
        }
    }
}

fn normalize_path_token(path_token: &str, root: &Path) -> Option<String> {
    normalized_root_text(root)?;
    if has_dot_segment(path_token) {
        return None;
    }
    let path = Path::new(path_token);
    if !path.is_absolute() {
        return None;
    }
    let relative = path.strip_prefix(root).ok()?;
    if !is_clean_relative_path(relative) {
        return None;
    }
    Some(marker_path(relative))
}

fn denormalize_path_token(path_token: &str, root: &Path) -> Option<String> {
    let root_text = normalized_root_text(root)?;
    if path_token == WORKSPACE_ROOT_MARKER {
        return Some(root_text);
    }
    let relative = path_token.strip_prefix(&format!("{WORKSPACE_ROOT_MARKER}/"))?;
    if has_dot_segment(relative) {
        return None;
    }
    if !is_clean_relative_path(Path::new(relative)) {
        return None;
    }
    Some(join_root_text(&root_text, relative))
}

fn normalized_root_text(root: &Path) -> Option<String> {
    let root_text = root.to_str()?;
    if root_text == "/" {
        return Some(root_text.to_string());
    }
    Some(root_text.trim_end_matches('/').to_string())
}

fn marker_path(relative: &Path) -> String {
    let Some(relative_text) = relative.to_str() else {
        return WORKSPACE_ROOT_MARKER.to_string();
    };
    if relative_text.is_empty() {
        return WORKSPACE_ROOT_MARKER.to_string();
    }
    let relative_text = relative_text.trim_start_matches('/');
    format!("{WORKSPACE_ROOT_MARKER}/{relative_text}")
}

fn has_dot_segment(path: &str) -> bool {
    path.split('/')
        .any(|component| component == "." || component == "..")
}

fn is_clean_relative_path(path: &Path) -> bool {
    let mut has_normal_component = false;
    for component in path.components() {
        match component {
            Component::Normal(_) => has_normal_component = true,
            Component::CurDir => {}
            Component::ParentDir | Component::RootDir | Component::Prefix(_) => return false,
        }
    }
    has_normal_component
}

fn join_root_text(root_text: &str, relative: &str) -> String {
    if root_text == "/" {
        format!("/{relative}")
    } else {
        format!("{root_text}/{relative}")
    }
}

#[cfg(test)]
mod tests {
    use std::path::Path;

    use crate::{
        git_worktree_link::{
            WORKSPACE_ROOT_MARKER, WorktreeLinkFile, denormalize_worktree_link_bytes,
            is_out_of_root_admin_target, is_worktree_admin_pointer, is_worktree_gitlink_file,
            normalize_worktree_link_bytes, worktree_link_file, worktree_registration_prefix,
        },
        workspace_graph::NamespaceEntryKind,
    };

    #[test]
    fn normalizes_gitlink_absolute_workspace_path_to_marker() {
        let output = normalize_worktree_link_bytes(
            b"gitdir: /workspace/source/acme/web/.git/worktrees/feat\n",
            Path::new("/workspace/source"),
            WorktreeLinkFile::Gitlink,
        );

        assert_eq!(
            output,
            b"gitdir: ${BOWLINE_WORKSPACE_ROOT}/acme/web/.git/worktrees/feat\n"
        );
    }

    #[test]
    fn denormalizes_gitlink_marker_to_local_root() {
        let output = denormalize_worktree_link_bytes(
            b"gitdir: ${BOWLINE_WORKSPACE_ROOT}/acme/web/.git/worktrees/feat\n",
            Path::new("/workspace/target"),
            WorktreeLinkFile::Gitlink,
        );

        assert_eq!(
            output,
            b"gitdir: /workspace/target/acme/web/.git/worktrees/feat\n"
        );
    }

    #[test]
    fn normalizes_and_denormalizes_bare_gitdir_path() {
        let normalized = normalize_worktree_link_bytes(
            b"/workspace/source/acme/web-wt/.git\n",
            Path::new("/workspace/source"),
            WorktreeLinkFile::AdminPointer,
        );
        assert_eq!(normalized, b"${BOWLINE_WORKSPACE_ROOT}/acme/web-wt/.git\n");

        let denormalized = denormalize_worktree_link_bytes(
            &normalized,
            Path::new("/workspace/target"),
            WorktreeLinkFile::AdminPointer,
        );
        assert_eq!(denormalized, b"/workspace/target/acme/web-wt/.git\n");
    }

    #[test]
    fn leaves_relative_commondir_verbatim() {
        let input = b"../..\n";

        assert_eq!(
            normalize_worktree_link_bytes(
                input,
                Path::new("/workspace/source"),
                WorktreeLinkFile::AdminPointer
            ),
            input
        );
        assert_eq!(
            denormalize_worktree_link_bytes(
                input,
                Path::new("/workspace/source"),
                WorktreeLinkFile::AdminPointer
            ),
            input
        );
    }

    #[test]
    fn detects_out_of_root_absolute_admin_targets() {
        assert!(is_out_of_root_admin_target(
            b"/opt/other/repo/.git\n",
            Path::new("/workspace/source")
        ));
        assert!(!is_out_of_root_admin_target(
            b"/workspace/source/repo-wt/.git\n",
            Path::new("/workspace/source")
        ));
        assert!(!is_out_of_root_admin_target(
            b"../..\n",
            Path::new("/workspace/source")
        ));
    }

    #[test]
    fn ignores_unparseable_admin_targets_for_out_of_root_detection() {
        for input in [
            b"gitdir: /opt/other/repo/.git\n".as_slice(),
            b"/opt/other/repo/.git\nextra\n",
            b"/workspace/source/\xFF\n",
            b"",
        ] {
            assert!(!is_out_of_root_admin_target(
                input,
                Path::new("/workspace/source")
            ));
        }
    }

    #[test]
    fn leaves_outside_root_absolute_path_verbatim() {
        let input = b"gitdir: /opt/other/.git\n";

        assert_eq!(
            normalize_worktree_link_bytes(
                input,
                Path::new("/workspace/source"),
                WorktreeLinkFile::Gitlink
            ),
            input
        );
    }

    #[test]
    fn round_trip_normalized_real_gitlink_is_deterministic() {
        let root = Path::new("/workspace/source");
        let input = b"gitdir: /workspace/source/acme/web/.git/worktrees/feat\n";
        let normalized = normalize_worktree_link_bytes(input, root, WorktreeLinkFile::Gitlink);
        let denormalized =
            denormalize_worktree_link_bytes(&normalized, root, WorktreeLinkFile::Gitlink);

        assert_eq!(
            normalize_worktree_link_bytes(&denormalized, root, WorktreeLinkFile::Gitlink),
            normalized
        );
    }

    #[test]
    fn round_trip_portable_gitlink_is_deterministic() {
        let root = Path::new("/workspace/source");
        let portable = format!("gitdir: {WORKSPACE_ROOT_MARKER}/acme/web/.git/worktrees/feat\n");
        let denormalized =
            denormalize_worktree_link_bytes(portable.as_bytes(), root, WorktreeLinkFile::Gitlink);

        assert_eq!(
            normalize_worktree_link_bytes(&denormalized, root, WorktreeLinkFile::Gitlink),
            portable.as_bytes()
        );
    }

    #[test]
    fn leaves_non_utf8_bytes_verbatim() {
        let input = b"gitdir: /workspace/source/\xFF\n";

        assert_eq!(
            normalize_worktree_link_bytes(
                input,
                Path::new("/workspace/source"),
                WorktreeLinkFile::Gitlink
            ),
            input
        );
    }

    #[test]
    fn leaves_unparseable_line_shapes_verbatim() {
        for input in [
            b"gitdir:/workspace/source/repo/.git\n".as_slice(),
            b"gitdir: /workspace/source/repo/.git\nextra\n",
            b"gitdir: \n",
            b"",
        ] {
            assert_eq!(
                normalize_worktree_link_bytes(
                    input,
                    Path::new("/workspace/source"),
                    WorktreeLinkFile::Gitlink
                ),
                input
            );
        }
    }

    #[test]
    fn leaves_wrong_line_shape_for_named_file_kind_verbatim() {
        let root = Path::new("/workspace/source");
        let gitlink_without_prefix = b"/workspace/source/acme/web/.git/worktrees/feat\n";
        let admin_with_prefix = b"gitdir: /workspace/source/acme/web-wt/.git\n";

        assert_eq!(
            normalize_worktree_link_bytes(gitlink_without_prefix, root, WorktreeLinkFile::Gitlink),
            gitlink_without_prefix
        );
        assert_eq!(
            normalize_worktree_link_bytes(admin_with_prefix, root, WorktreeLinkFile::AdminPointer),
            admin_with_prefix
        );
    }

    #[test]
    fn root_prefix_match_is_component_wise() {
        let input = b"gitdir: /workspace/sourcex/repo/.git\n";

        assert_eq!(
            normalize_worktree_link_bytes(
                input,
                Path::new("/workspace/source"),
                WorktreeLinkFile::Gitlink
            ),
            input
        );
    }

    #[test]
    fn leaves_parent_dir_escape_paths_verbatim() {
        let normalize_input = b"gitdir: /workspace/source/../outside/.git/worktrees/feat\n";
        let denormalize_input =
            b"gitdir: ${BOWLINE_WORKSPACE_ROOT}/../outside/.git/worktrees/feat\n";
        let bare_normalize_input = b"/workspace/source/../outside/.git\n";
        let bare_denormalize_input = b"${BOWLINE_WORKSPACE_ROOT}/../outside/.git\n";

        assert_eq!(
            normalize_worktree_link_bytes(
                normalize_input,
                Path::new("/workspace/source"),
                WorktreeLinkFile::Gitlink
            ),
            normalize_input
        );
        assert_eq!(
            denormalize_worktree_link_bytes(
                denormalize_input,
                Path::new("/workspace/source"),
                WorktreeLinkFile::Gitlink
            ),
            denormalize_input
        );
        assert_eq!(
            normalize_worktree_link_bytes(
                bare_normalize_input,
                Path::new("/workspace/source"),
                WorktreeLinkFile::AdminPointer
            ),
            bare_normalize_input
        );
        assert_eq!(
            denormalize_worktree_link_bytes(
                bare_denormalize_input,
                Path::new("/workspace/source"),
                WorktreeLinkFile::AdminPointer
            ),
            bare_denormalize_input
        );
    }

    #[test]
    fn leaves_dot_segment_paths_verbatim() {
        let normalize_input = b"gitdir: /workspace/source/./repo/.git/worktrees/feat\n";
        let denormalize_input = b"gitdir: ${BOWLINE_WORKSPACE_ROOT}/./repo/.git/worktrees/feat\n";

        assert_eq!(
            normalize_worktree_link_bytes(
                normalize_input,
                Path::new("/workspace/source"),
                WorktreeLinkFile::Gitlink
            ),
            normalize_input
        );
        assert_eq!(
            denormalize_worktree_link_bytes(
                denormalize_input,
                Path::new("/workspace/source"),
                WorktreeLinkFile::Gitlink
            ),
            denormalize_input
        );
    }

    #[test]
    fn detects_only_file_entries_named_dot_git_as_gitlinks() {
        assert!(is_worktree_gitlink_file(".git", NamespaceEntryKind::File));
        assert!(is_worktree_gitlink_file(
            "acme/web-wt/.git",
            NamespaceEntryKind::File
        ));
        assert!(!is_worktree_gitlink_file(
            "acme/web-wt/.git",
            NamespaceEntryKind::Directory
        ));
        assert!(!is_worktree_gitlink_file(
            ".gitignore",
            NamespaceEntryKind::File
        ));
        assert!(!is_worktree_gitlink_file(
            "foo.git",
            NamespaceEntryKind::File
        ));
    }

    #[test]
    fn detects_only_worktree_admin_pointer_paths() {
        assert!(is_worktree_admin_pointer(".git/worktrees/w/gitdir"));
        assert!(is_worktree_admin_pointer("repo/.git/worktrees/w/commondir"));
        assert!(!is_worktree_admin_pointer(".git/worktrees/w/HEAD"));
        assert!(!is_worktree_admin_pointer(".git/config"));
    }

    #[test]
    fn finds_worktree_registration_prefixes() {
        assert_eq!(
            worktree_registration_prefix(".git/worktrees/w/gitdir").as_deref(),
            Some(".git/worktrees/w/")
        );
        assert_eq!(
            worktree_registration_prefix("repo/.git/worktrees/w/refs/heads/feat").as_deref(),
            Some("repo/.git/worktrees/w/")
        );
        assert_eq!(worktree_registration_prefix(".git/config"), None);
    }

    #[test]
    fn classifies_named_worktree_link_files() {
        assert_eq!(
            worktree_link_file("repo-wt/.git", NamespaceEntryKind::File),
            Some(WorktreeLinkFile::Gitlink)
        );
        assert_eq!(
            worktree_link_file(".git/worktrees/w/gitdir", NamespaceEntryKind::File),
            Some(WorktreeLinkFile::AdminPointer)
        );
        assert_eq!(
            worktree_link_file(".git/config", NamespaceEntryKind::File),
            None
        );
    }
}

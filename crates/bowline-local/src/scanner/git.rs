use std::{collections::BTreeSet, fs, io, path::Path};

use bowline_core::workspace_graph::normalize_workspace_path;

use crate::policy::{PathFacts, UserPolicy, classify_path};

use super::{path_to_slash_string, pruned_by_default_policy, syncs_as_workspace_state};

pub(super) fn git_config_has_remote(config: &Path) -> io::Result<bool> {
    match fs::read_to_string(config) {
        Ok(config) => Ok(config
            .lines()
            .any(|line| line.trim_start().starts_with("[remote "))),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(false),
        Err(error) => Err(error),
    }
}

pub(super) fn git_remote_tracking_is_stale(git_dir: &Path) -> io::Result<bool> {
    let Some(branch) = git_head_branch(git_dir)? else {
        return Ok(false);
    };
    let Some(local) = read_git_ref(git_dir, &format!("refs/heads/{branch}"))? else {
        return Ok(false);
    };
    let remotes = git_remote_names(&git_dir.join("config"))?;
    for remote in remotes {
        let Some(remote_ref) = read_git_ref(git_dir, &format!("refs/remotes/{remote}/{branch}"))?
        else {
            continue;
        };
        return Ok(remote_ref != local);
    }
    Ok(false)
}

fn git_head_branch(git_dir: &Path) -> io::Result<Option<String>> {
    let head = match fs::read_to_string(git_dir.join("HEAD")) {
        Ok(head) => head,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(error),
    };
    Ok(head
        .trim()
        .strip_prefix("ref: refs/heads/")
        .map(str::to_string))
}

fn git_remote_names(config: &Path) -> io::Result<Vec<String>> {
    let config = match fs::read_to_string(config) {
        Ok(config) => config,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(error) => return Err(error),
    };
    Ok(config
        .lines()
        .filter_map(|line| {
            line.trim()
                .strip_prefix("[remote \"")
                .and_then(|rest| rest.strip_suffix("\"]"))
                .map(str::to_string)
        })
        .collect())
}

pub(super) fn read_git_ref(git_dir: &Path, reference: &str) -> io::Result<Option<String>> {
    match fs::read_to_string(git_dir.join(reference)) {
        Ok(value) => Ok(Some(value.trim().to_string())),
        Err(error) if error.kind() == io::ErrorKind::NotFound => {
            read_packed_git_ref(git_dir, reference)
        }
        Err(error) => Err(error),
    }
}

fn read_packed_git_ref(git_dir: &Path, reference: &str) -> io::Result<Option<String>> {
    let packed_refs = match fs::read_to_string(git_dir.join("packed-refs")) {
        Ok(packed_refs) => packed_refs,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(error),
    };
    for line in packed_refs.lines() {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') || line.starts_with('^') {
            continue;
        }
        let Some((sha, packed_reference)) = line.split_once(' ') else {
            continue;
        };
        if packed_reference == reference {
            return Ok(Some(sha.to_string()));
        }
    }
    Ok(None)
}

pub(super) struct GitUntrackedRead {
    pub count: u64,
    pub complete: bool,
}

pub(super) fn git_untracked_file_count(repo: &Path) -> io::Result<GitUntrackedRead> {
    let tracked = read_git_index_paths(&repo.join(".git").join("index"))?;
    if !tracked.complete {
        return Ok(GitUntrackedRead {
            count: 0,
            complete: false,
        });
    }
    count_untracked_files(repo, Path::new(""), &tracked.paths).map(|count| GitUntrackedRead {
        count,
        complete: true,
    })
}

fn count_untracked_files(
    repo: &Path,
    relative_dir: &Path,
    tracked: &BTreeSet<String>,
) -> io::Result<u64> {
    let mut count = 0;
    let mut entries =
        crate::fs_access::read_dir(&repo.join(relative_dir))?.collect::<Result<Vec<_>, _>>()?;
    entries.sort_by_key(|entry| entry.file_name());

    for entry in entries {
        let file_type = entry.file_type()?;
        let relative_path = relative_dir.join(entry.file_name());
        let path = normalize_workspace_path(&path_to_slash_string(&relative_path));
        if path == ".git" || path.starts_with(".git/") {
            continue;
        }

        let metadata = crate::fs_access::symlink_metadata(&entry.path())?;
        let is_dir = file_type.is_dir();
        let decision = classify_path(
            &PathFacts {
                relative_path: path.clone(),
                is_dir,
                byte_len: if is_dir { None } else { Some(metadata.len()) },
            },
            &UserPolicy::empty(),
        );

        if is_dir {
            if !pruned_by_default_policy(&decision) {
                count += count_untracked_files(repo, &relative_path, tracked)?;
            }
        } else if !tracked.contains(&path) && syncs_as_workspace_state(&decision) {
            count += 1;
        }
    }

    Ok(count)
}

struct GitIndexRead {
    paths: BTreeSet<String>,
    complete: bool,
}

fn read_git_index_paths(index: &Path) -> io::Result<GitIndexRead> {
    let bytes = match fs::read(index) {
        Ok(bytes) => bytes,
        Err(error) if error.kind() == io::ErrorKind::NotFound => {
            return Ok(GitIndexRead {
                paths: BTreeSet::new(),
                complete: true,
            });
        }
        Err(error) => return Err(error),
    };
    if bytes.len() < 12 || &bytes[0..4] != b"DIRC" {
        return Ok(GitIndexRead {
            paths: BTreeSet::new(),
            complete: false,
        });
    }

    let version = u32::from_be_bytes(bytes[4..8].try_into().expect("slice length"));
    let entry_count = u32::from_be_bytes(bytes[8..12].try_into().expect("slice length"));
    if !matches!(version, 2 | 3) {
        return Ok(GitIndexRead {
            paths: BTreeSet::new(),
            complete: false,
        });
    }

    let mut paths = BTreeSet::new();
    let mut offset = 12usize;
    for _ in 0..entry_count {
        if offset + 62 > bytes.len() {
            break;
        }

        let flags = u16::from_be_bytes([bytes[offset + 60], bytes[offset + 61]]);
        let name_start = offset
            + if version >= 3 && flags & 0x4000 != 0 {
                64
            } else {
                62
            };
        let name_len = (flags & 0x0fff) as usize;
        let Some(name_end) = index_name_end(&bytes, name_start, name_len) else {
            break;
        };

        paths.insert(normalize_workspace_path(&String::from_utf8_lossy(
            &bytes[name_start..name_end],
        )));
        offset += (name_end + 1 - offset).div_ceil(8) * 8;
    }

    Ok(GitIndexRead {
        paths,
        complete: true,
    })
}

fn index_name_end(bytes: &[u8], name_start: usize, name_len: usize) -> Option<usize> {
    if name_len < 0x0fff {
        let end = name_start.checked_add(name_len)?;
        return (end < bytes.len()).then_some(end);
    }

    bytes[name_start..]
        .iter()
        .position(|byte| *byte == 0)
        .map(|nul| name_start + nul)
}

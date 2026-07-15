use std::{
    collections::BTreeMap,
    fs,
    fs::OpenOptions,
    io::{self, Write},
    path::{Path, PathBuf},
    sync::atomic::{AtomicU64, Ordering},
};

#[cfg(unix)]
use std::os::unix::fs::OpenOptionsExt;

use bowline_core::status::{
    StatusAttention, StatusFact, StatusFactAvailabilityImpact, StatusFactScope, StatusLevel,
};
use bowline_daemon::status_projection::DaemonStatusProjection;
use serde::Serialize;

static TEMPORARY_FILE_COUNTER: AtomicU64 = AtomicU64::new(1);

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize)]
enum FinderBadge {
    #[serde(rename = "bowline.synced")]
    Synced,
    #[serde(rename = "bowline.syncing")]
    Syncing,
    #[serde(rename = "bowline.pending")]
    Pending,
    #[serde(rename = "bowline.error")]
    Error,
    #[serde(rename = "bowline.conflict")]
    Conflict,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct FinderBadgeEntry {
    path: String,
    badge: FinderBadge,
    updated_at: String,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct FinderBadgeSnapshot {
    schema_version: u32,
    generated_at: String,
    roots: Vec<String>,
    badges: Vec<FinderBadgeEntry>,
}

pub(super) fn default_snapshot_path() -> Option<PathBuf> {
    #[cfg(test)]
    {
        None
    }
    #[cfg(all(target_os = "macos", not(test)))]
    {
        std::env::var_os("BOWLINE_FINDER_BADGE_SNAPSHOT")
            .filter(|path| !path.is_empty())
            .map(PathBuf::from)
            .or_else(|| {
                std::env::var_os("HOME").map(|home| {
                    PathBuf::from(home)
                        .join("Library")
                        .join("Application Support")
                        .join("bowline")
                        .join("finder-badges.json")
                })
            })
    }
    #[cfg(all(not(target_os = "macos"), not(test)))]
    {
        std::env::var_os("BOWLINE_FINDER_BADGE_SNAPSHOT")
            .filter(|path| !path.is_empty())
            .map(PathBuf::from)
    }
}

pub(super) fn write_snapshot(
    destination: &Path,
    roots: &[PathBuf],
    projection: &DaemonStatusProjection,
    delivered_at: &str,
) -> io::Result<()> {
    let snapshot = snapshot(roots, projection, delivered_at);
    let bytes = serde_json::to_vec(&snapshot).map_err(io::Error::other)?;
    let parent = destination
        .parent()
        .ok_or_else(|| io::Error::other("Finder snapshot path has no parent"))?;
    fs::create_dir_all(parent)?;
    let (temporary, mut file) = create_temporary_file(parent)?;
    let result = (|| {
        file.write_all(&bytes)?;
        file.sync_all()?;
        drop(file);
        fs::rename(&temporary, destination)
    })();
    if result.is_err() {
        let _cleanup = fs::remove_file(&temporary);
    }
    result
}

fn create_temporary_file(parent: &Path) -> io::Result<(PathBuf, fs::File)> {
    for _attempt in 0..16 {
        let counter = TEMPORARY_FILE_COUNTER.fetch_add(1, Ordering::Relaxed);
        let temporary = parent.join(format!(
            ".finder-badges.{}.{}.tmp",
            std::process::id(),
            counter
        ));
        let mut options = OpenOptions::new();
        options.write(true).create_new(true);
        #[cfg(unix)]
        options.mode(0o600);
        match options.open(&temporary) {
            Ok(file) => return Ok((temporary, file)),
            Err(error) if error.kind() == io::ErrorKind::AlreadyExists => continue,
            Err(error) => return Err(error),
        }
    }
    Err(io::Error::new(
        io::ErrorKind::AlreadyExists,
        "could not reserve a Finder snapshot temporary file",
    ))
}

fn snapshot(
    roots: &[PathBuf],
    projection: &DaemonStatusProjection,
    delivered_at: &str,
) -> FinderBadgeSnapshot {
    let mut roots = roots
        .iter()
        .map(|root| root.display().to_string())
        .collect::<Vec<_>>();
    roots.sort();
    roots.dedup();
    let root_badge = match projection.status.status.level {
        StatusLevel::Healthy => FinderBadge::Synced,
        StatusLevel::Attention => FinderBadge::Pending,
        StatusLevel::Limited => FinderBadge::Error,
    };
    let mut badges = roots
        .iter()
        .map(|root| (root.clone(), root_badge))
        .collect::<BTreeMap<_, _>>();
    for fact in &projection.status.status_summary.facts {
        let Some((path, badge)) = badge_for_fact(fact) else {
            continue;
        };
        badges
            .entry(path.to_string())
            .and_modify(|current| *current = (*current).max(badge))
            .or_insert(badge);
    }
    FinderBadgeSnapshot {
        schema_version: 1,
        generated_at: delivered_at.to_string(),
        roots,
        badges: badges
            .into_iter()
            .map(|(path, badge)| FinderBadgeEntry {
                path,
                badge,
                updated_at: delivered_at.to_string(),
            })
            .collect(),
    }
}

fn badge_for_fact(fact: &StatusFact) -> Option<(&str, FinderBadge)> {
    if fact.scope != StatusFactScope::Path {
        return None;
    }
    let path = fact.scope_id.as_deref()?;
    let badge = if matches!(
        fact.kind.as_str(),
        "sync.conflict_unresolved" | "work_view.conflicted"
    ) {
        FinderBadge::Conflict
    } else if fact.availability_impact == StatusFactAvailabilityImpact::Unavailable {
        FinderBadge::Error
    } else if fact.attention_impact == StatusAttention::Required {
        FinderBadge::Pending
    } else if fact.availability_impact == StatusFactAvailabilityImpact::Degraded {
        FinderBadge::Syncing
    } else {
        return None;
    };
    Some((path, badge))
}

#[cfg(test)]
mod tests {
    use std::{fs, time::Duration};

    use bowline_core::status::{
        StatusAttention, StatusFact, StatusFactAvailabilityImpact, StatusFactScope,
    };
    use bowline_daemon::status_projection::{
        DaemonInstanceId, LocalStatusProjectionCollector, ProjectionServiceConfig,
        StatusProjectionService,
    };

    use super::*;

    #[test]
    fn projection_snapshot_atomically_replaces_stale_badges() {
        let temp = std::env::temp_dir().join(format!(
            "bowline-finder-projection-{}-{}",
            std::process::id(),
            time::OffsetDateTime::now_utc().unix_timestamp_nanos()
        ));
        let destination = temp.join("finder-badges.json");
        let collector = LocalStatusProjectionCollector::new(None, None, false)
            .expect("local projection collector");
        let config = ProjectionServiceConfig::new(
            DaemonInstanceId::new("finder-test"),
            Duration::from_secs(60),
        )
        .expect("projection config");
        let service = StatusProjectionService::start(config, vec![Box::new(collector)])
            .expect("projection service");
        let projection_input = service.input();
        let mut projection = (*service.current().expect("projection")).clone();
        let root = temp.join("Code");
        let conflict = root.join("project/conflicted.txt");
        projection.status.status_summary.facts.push(
            StatusFact::new(
                "finder-conflict",
                "sync.conflict_unresolved",
                "finder-test",
                StatusFactScope::Path,
                "2026-07-14T12:00:00Z",
                "finder-conflict",
            )
            .with_scope_id(conflict.display().to_string())
            .with_impacts(
                StatusFactAvailabilityImpact::Degraded,
                StatusAttention::Required,
            ),
        );

        let first_write = write_snapshot(
            &destination,
            std::slice::from_ref(&root),
            &projection,
            projection.generated_at.as_str(),
        );
        projection_input.record_finder_snapshot(first_write.is_ok());
        first_write.expect("write snapshot");
        let first: serde_json::Value =
            serde_json::from_slice(&fs::read(&destination).expect("read snapshot"))
                .expect("decode snapshot");
        assert!(first["badges"].as_array().is_some_and(|badges| {
            badges.iter().any(|badge| {
                badge["path"] == conflict.display().to_string()
                    && badge["badge"] == "bowline.conflict"
            })
        }));

        projection.status.status_summary.facts.clear();
        let second_write = write_snapshot(
            &destination,
            std::slice::from_ref(&root),
            &projection,
            "2026-07-14T12:05:00Z",
        );
        projection_input.record_finder_snapshot(second_write.is_ok());
        second_write.expect("replace snapshot");
        let second: serde_json::Value =
            serde_json::from_slice(&fs::read(&destination).expect("read replaced snapshot"))
                .expect("decode replaced snapshot");
        assert!(!second.to_string().contains("conflicted.txt"));
        assert_eq!(second["generatedAt"], "2026-07-14T12:05:00Z");
        assert!(
            fs::read_dir(&temp)
                .expect("snapshot directory")
                .all(|entry| !entry
                    .expect("snapshot entry")
                    .file_name()
                    .to_string_lossy()
                    .ends_with(".tmp"))
        );
        let metrics = service.metrics().expect("projection metrics");
        assert_eq!(metrics.finder_snapshot_writes, 2);
        assert_eq!(metrics.finder_snapshot_failures, 0);
        let _ = fs::remove_dir_all(temp);
    }
}

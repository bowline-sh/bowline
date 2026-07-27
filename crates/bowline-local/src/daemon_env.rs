//! The persisted daemon environment file (`daemon.env`).
//!
//! One parser and one allow-list, shared by every side of it: the CLI that
//! writes it and spawns the daemon with it, the daemon that loads it at
//! startup, and the bootstrap path that ships it to a trusted remote host.
//! Divergent parsers here mean a device that silently loses its account session
//! or its workspace identity, so there is exactly one implementation.

use std::collections::BTreeMap;
use std::fs;
use std::io;
use std::path::Path;

use bowline_core::fs_atomic::{AtomicWriteOptions, write_atomic};
use fs2::FileExt;

use crate::device_keys::AccountSessionCredentials;

pub const FILE_NAME: &str = "daemon.env";
const LOCK_FILE_NAME: &str = "daemon.env.lock";

pub const ACCOUNT_SESSION_ID_KEY: &str = "BOWLINE_ACCOUNT_SESSION_ID";
pub const ACCOUNT_SESSION_REVOCATION_TOKEN_KEY: &str = "BOWLINE_ACCOUNT_SESSION_REVOCATION_TOKEN";

/// The only keys allowed to cross into a daemon process. Every other line in the
/// file is ignored, so a stale or hostile entry can never widen the daemon's
/// environment. Sorted, because this list is read by humans as often as by code.
pub const PERSISTED_KEYS: &[&str] = &[
    ACCOUNT_SESSION_ID_KEY,
    ACCOUNT_SESSION_REVOCATION_TOKEN_KEY,
    "BOWLINE_CONTROL_PLANE_TOKEN",
    "BOWLINE_DEVICE_ID",
    "BOWLINE_DEVICE_NAME",
    "BOWLINE_SECRET_STORE",
    "BOWLINE_WORKOS_ACCESS_TOKEN",
    "BOWLINE_WORKOS_CLIENT_ID",
    "BOWLINE_WORKSPACE_ID",
    "CONVEX_URL",
];

pub fn is_persisted_key(key: &str) -> bool {
    PERSISTED_KEYS.contains(&key)
}

/// Parse the allow-listed entries of a `daemon.env` body. Values are trimmed and
/// empty values dropped, so a blank entry never shadows a real one. Ordering is
/// deterministic so spawned environments and rendered files never churn.
pub fn parse(contents: &str) -> BTreeMap<String, String> {
    contents
        .lines()
        .filter_map(|line| line.split_once('='))
        .filter(|(key, _)| is_persisted_key(key))
        .filter_map(|(key, value)| {
            let value = value.trim();
            (!value.is_empty()).then(|| (key.to_string(), value.to_string()))
        })
        .collect()
}

/// Render allow-listed entries back into `daemon.env` form. Entries carrying a
/// newline are dropped: one key per line is the whole format.
pub fn render<'a>(entries: impl IntoIterator<Item = (&'a str, &'a str)>) -> String {
    let mut rendered = entries
        .into_iter()
        .filter(|(key, value)| {
            is_persisted_key(key) && !value.trim().is_empty() && !value.contains('\n')
        })
        .map(|(key, value)| format!("{key}={}", value.trim()))
        .collect::<Vec<_>>();
    rendered.sort();
    rendered.push(String::new());
    rendered.join("\n")
}

/// Read the allow-listed environment persisted under a daemon state root. A
/// missing or unreadable file is an empty environment, never an error: the
/// daemon still starts, it just has nothing persisted to overlay.
pub fn read(state_root: &Path) -> BTreeMap<String, String> {
    parse(&std::fs::read_to_string(state_root.join(FILE_NAME)).unwrap_or_default())
}

pub fn value(state_root: &Path, name: &str) -> Option<String> {
    read(state_root).remove(name)
}

/// Rewrites `daemon.env` from its current body, holding one exclusive advisory
/// lock across both the read and the write. `mutate` answering `None` leaves the
/// file untouched, and the returned flag says whether anything was written.
///
/// The lock is what makes the pair safe, not the write. Each write already lands
/// atomically, but the two rewriters run in different processes — `bowline
/// logout` stripping the account session, the daemon recording a session it just
/// registered — and an unlocked read-modify-write lets the loser commit a body
/// it snapshotted before the winner's change. That is how a session logout had
/// just stripped came back, revocation token and all, for the next daemon start
/// to adopt. Readers need no lock: every write lands by rename, so a reader sees
/// one whole body or the other, never a splice.
pub fn update(state_root: &Path, mutate: impl FnOnce(&str) -> Option<String>) -> io::Result<bool> {
    let _lock = lock_exclusive(state_root)?;
    let path = state_root.join(FILE_NAME);
    let Some(updated) = mutate(&read_body(&path)?) else {
        return Ok(false);
    };
    write_body(&path, &updated)?;
    Ok(true)
}

/// The lock lives beside `daemon.env` rather than on it: every write replaces the
/// file by rename, so a lock held on the file's own inode would guard an inode
/// the next writer no longer publishes.
fn lock_exclusive(state_root: &Path) -> io::Result<fs::File> {
    fs::create_dir_all(state_root)?;
    let file = lock_file_options().open(state_root.join(LOCK_FILE_NAME))?;
    file.lock_exclusive()?;
    Ok(file)
}

fn lock_file_options() -> fs::OpenOptions {
    let mut options = fs::OpenOptions::new();
    options.write(true).create(true).truncate(false);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    options
}

/// A host that has never written one reads as an empty environment rather than
/// an error, so the first session to arrive creates the file.
fn read_body(path: &Path) -> io::Result<String> {
    match fs::read_to_string(path) {
        Ok(contents) => Ok(contents),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(String::new()),
        Err(error) => Err(error),
    }
}

fn write_body(path: &Path, contents: &str) -> io::Result<()> {
    write_atomic(
        path,
        contents.as_bytes(),
        AtomicWriteOptions {
            unix_mode: Some(0o600),
            reject_symlink: true,
            replace_existing: true,
        },
    )
}

/// The account-session credentials, which `bowline logout` strips from the file
/// while leaving the device's workspace identity intact.
pub const ACCOUNT_SESSION_KEYS: &[&str] =
    &[ACCOUNT_SESSION_ID_KEY, ACCOUNT_SESSION_REVOCATION_TOKEN_KEY];

/// Rewrites a `daemon.env` body without its account-session entries, or `None`
/// when there was nothing to strip. Operates on the raw body rather than the
/// parsed map so unrelated lines survive a logout byte for byte.
pub fn without_account_session(contents: &str) -> Option<String> {
    let retained = contents
        .lines()
        .filter(|line| {
            line.split_once('=')
                .is_none_or(|(key, _)| !ACCOUNT_SESSION_KEYS.contains(&key.trim()))
        })
        .collect::<Vec<_>>();
    if retained.len() == contents.lines().count() {
        return None;
    }
    Some(if retained.is_empty() {
        String::new()
    } else {
        retained.join("\n") + "\n"
    })
}

/// Rewrites a `daemon.env` body carrying `session`, replacing whatever
/// account-session pair it already held. Operates on the raw body for the same
/// reason [`without_account_session`] does: unrelated lines survive byte for
/// byte, so rewriting the session never disturbs the device's identity.
pub fn with_account_session(contents: &str, session: &AccountSessionCredentials) -> String {
    let mut body = without_account_session(contents).unwrap_or_else(|| contents.to_string());
    if !body.is_empty() && !body.ends_with('\n') {
        body.push('\n');
    }
    body.push_str(&render([
        (ACCOUNT_SESSION_ID_KEY, session.session_id.as_str()),
        (
            ACCOUNT_SESSION_REVOCATION_TOKEN_KEY,
            session.revocation_token.as_str(),
        ),
    ]));
    body
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_keeps_only_allow_listed_non_empty_values() {
        let parsed = parse(
            "CONVEX_URL=https://example.convex.cloud\n\
             BOWLINE_WORKOS_REFRESH_TOKEN=secret\n\
             BOWLINE_DEVICE_ID= device_a \n\
             BOWLINE_DEVICE_NAME=\n\
             not-an-entry\n",
        );

        assert_eq!(
            parsed.get("CONVEX_URL").map(String::as_str),
            Some("https://example.convex.cloud")
        );
        assert_eq!(
            parsed.get("BOWLINE_DEVICE_ID").map(String::as_str),
            Some("device_a")
        );
        assert!(!parsed.contains_key("BOWLINE_WORKOS_REFRESH_TOKEN"));
        assert!(!parsed.contains_key("BOWLINE_DEVICE_NAME"));
    }

    #[test]
    fn render_round_trips_through_parse() {
        let rendered = render([
            ("BOWLINE_DEVICE_ID", "device_a"),
            ("BOWLINE_WORKOS_REFRESH_TOKEN", "secret"),
            ("CONVEX_URL", "https://example.convex.cloud"),
            ("BOWLINE_DEVICE_NAME", "line\nbreak"),
        ]);

        assert_eq!(
            rendered,
            "BOWLINE_DEVICE_ID=device_a\nCONVEX_URL=https://example.convex.cloud\n"
        );
        assert_eq!(parse(&rendered).len(), 2);
    }

    #[test]
    fn account_session_keys_are_a_subset_of_the_persisted_allow_list() {
        for key in ACCOUNT_SESSION_KEYS {
            assert!(is_persisted_key(key), "{key}");
        }
    }

    #[test]
    fn stripping_the_account_session_leaves_the_device_identity_intact() {
        let stripped = without_account_session(
            "BOWLINE_DEVICE_ID=device_a\nBOWLINE_ACCOUNT_SESSION_ID=sess\nCONVEX_URL=https://x\n",
        )
        .expect("session entries removed");

        assert_eq!(
            stripped,
            "BOWLINE_DEVICE_ID=device_a\nCONVEX_URL=https://x\n"
        );
        assert!(without_account_session("BOWLINE_DEVICE_ID=device_a\n").is_none());
    }

    fn session(suffix: &str) -> AccountSessionCredentials {
        AccountSessionCredentials {
            session_id: format!("bowline_session_{suffix}"),
            revocation_token: format!("bowline_revoke_{suffix}"),
        }
    }

    /// Fixtures are rendered rather than spelled out so the source never carries
    /// a line that reads as a real credential assignment.
    fn rendered_environment(session: &AccountSessionCredentials, trailing: &str) -> String {
        format!(
            "BOWLINE_DEVICE_ID=device_a\n{}={}\n{}={}\n{trailing}",
            ACCOUNT_SESSION_ID_KEY,
            session.session_id,
            ACCOUNT_SESSION_REVOCATION_TOKEN_KEY,
            session.revocation_token,
        )
    }

    #[test]
    fn recording_a_session_replaces_the_pair_and_leaves_everything_else_alone() {
        let rewritten = with_account_session(
            &rendered_environment(&session("stale"), "CONVEX_URL=https://x\n"),
            &session("fresh"),
        );

        let parsed = parse(&rewritten);
        assert_eq!(
            parsed.get(ACCOUNT_SESSION_ID_KEY).map(String::as_str),
            Some("bowline_session_fresh")
        );
        assert_eq!(
            parsed
                .get(ACCOUNT_SESSION_REVOCATION_TOKEN_KEY)
                .map(String::as_str),
            Some("bowline_revoke_fresh")
        );
        assert_eq!(
            parsed.get("BOWLINE_DEVICE_ID").map(String::as_str),
            Some("device_a")
        );
        assert_eq!(
            parsed.get("CONVEX_URL").map(String::as_str),
            Some("https://x")
        );
        assert!(!rewritten.contains("stale"));
    }

    #[test]
    fn recording_a_session_into_an_empty_environment_writes_only_the_pair() {
        let fresh = session("fresh");

        let rewritten = with_account_session("", &fresh);

        assert_eq!(
            rewritten,
            format!(
                "{}={}\n{}={}\n",
                ACCOUNT_SESSION_ID_KEY,
                fresh.session_id,
                ACCOUNT_SESSION_REVOCATION_TOKEN_KEY,
                fresh.revocation_token,
            )
        );
    }

    #[test]
    fn missing_state_root_reads_as_an_empty_environment() {
        assert!(read(Path::new("/nonexistent/bowline-daemon-env")).is_empty());
    }

    fn temp_state_root(name: &str) -> std::path::PathBuf {
        let path =
            std::env::temp_dir().join(format!("bowline-daemon-env-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&path);
        std::fs::create_dir_all(&path).expect("state root");
        path
    }

    /// `bowline logout` and the daemon rewrite this file from different
    /// processes. Unlocked, the daemon's pre-logout snapshot lands after the
    /// strip and puts the session — and the only token able to revoke it — back
    /// on disk for the next start to adopt.
    #[test]
    fn a_concurrent_rewrite_cannot_put_a_stripped_account_session_back() {
        let state_root = temp_state_root("concurrent-strip");
        std::fs::write(
            state_root.join(FILE_NAME),
            rendered_environment(&session("stale"), ""),
        )
        .expect("provisioned daemon environment");
        let daemon_has_read = std::sync::Barrier::new(2);

        std::thread::scope(|scope| {
            scope.spawn(|| {
                update(&state_root, |body| {
                    daemon_has_read.wait();
                    // Widens the window a real daemon leaves between its read and
                    // its write; the lock, not the timing, is what has to close it.
                    std::thread::sleep(std::time::Duration::from_millis(100));
                    Some(with_account_session(body, &session("fresh")))
                })
                .expect("the daemon records its replacement");
            });
            scope.spawn(|| {
                daemon_has_read.wait();
                assert!(
                    update(&state_root, without_account_session)
                        .expect("logout strips the session")
                );
            });
        });

        let persisted = read(&state_root);
        assert!(
            !persisted.contains_key(ACCOUNT_SESSION_ID_KEY),
            "{persisted:?}"
        );
        assert!(!persisted.contains_key(ACCOUNT_SESSION_REVOCATION_TOKEN_KEY));
        assert_eq!(
            persisted.get("BOWLINE_DEVICE_ID").map(String::as_str),
            Some("device_a")
        );
        let _ = std::fs::remove_dir_all(state_root);
    }

    #[test]
    fn an_update_with_nothing_to_change_leaves_the_file_byte_for_byte() {
        let state_root = temp_state_root("no-change");
        let body = "BOWLINE_DEVICE_ID=device_a\n";
        std::fs::write(state_root.join(FILE_NAME), body).expect("seed environment");

        assert!(!update(&state_root, without_account_session).expect("nothing to strip"));

        assert_eq!(
            std::fs::read_to_string(state_root.join(FILE_NAME)).expect("read back"),
            body
        );
        let _ = std::fs::remove_dir_all(state_root);
    }
}

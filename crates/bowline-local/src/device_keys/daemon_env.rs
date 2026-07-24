pub(super) fn without_account_session(contents: &str) -> Option<String> {
    let retained = contents
        .lines()
        .filter(|line| {
            line.split_once('=').is_none_or(|(key, _)| {
                !matches!(
                    key,
                    "BOWLINE_ACCOUNT_SESSION_ID" | "BOWLINE_ACCOUNT_SESSION_REVOCATION_TOKEN"
                )
            })
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

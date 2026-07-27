use std::{
    io::IsTerminal,
    process::{Command, Stdio},
};

/// Why a verification URL was not opened for the user. Login is the first
/// command anyone runs, so "we did not open it" must be a stated outcome the
/// caller can render, not a silent no-op.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BrowserLaunch {
    Opened,
    SuppressedByEnv,
    NotATerminal,
    NoOpener,
}

impl BrowserLaunch {
    pub fn opened(self) -> bool {
        self == Self::Opened
    }
}

/// Set to any non-empty value to keep `bowline login` from opening a browser.
const SUPPRESS_ENV: &str = "BOWLINE_NO_BROWSER";

/// Opens the WorkOS verification URL, which already embeds the user code, so
/// the one sanctioned trust step is a browser tab and a confirm button rather
/// than a terminal copy-paste chore.
pub fn open_verification_url(url: &str) -> BrowserLaunch {
    if let Some(refusal) = launch_refusal(
        std::env::var(SUPPRESS_ENV).ok().as_deref(),
        std::io::stdout().is_terminal(),
    ) {
        return refusal;
    }
    for (program, leading_args) in opener_candidates() {
        let mut command = Command::new(program);
        command
            .args(leading_args.iter())
            .arg(url)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null());
        if matches!(command.status(), Ok(status) if status.success()) {
            return BrowserLaunch::Opened;
        }
    }
    BrowserLaunch::NoOpener
}

/// Why launching is refused before any process is spawned, or `None` to try.
fn launch_refusal(suppress_value: Option<&str>, stdout_is_terminal: bool) -> Option<BrowserLaunch> {
    if suppress_value.is_some_and(|value| !value.is_empty()) {
        return Some(BrowserLaunch::SuppressedByEnv);
    }
    if !stdout_is_terminal {
        return Some(BrowserLaunch::NotATerminal);
    }
    None
}

#[cfg(target_os = "macos")]
fn opener_candidates() -> &'static [(&'static str, &'static [&'static str])] {
    &[("/usr/bin/open", &[])]
}

#[cfg(target_os = "linux")]
fn opener_candidates() -> &'static [(&'static str, &'static [&'static str])] {
    &[("xdg-open", &[]), ("gio", &["open"])]
}

#[cfg(not(any(target_os = "macos", target_os = "linux")))]
fn opener_candidates() -> &'static [(&'static str, &'static [&'static str])] {
    &[]
}

#[cfg(test)]
mod tests {
    use super::{BrowserLaunch, launch_refusal};

    #[test]
    fn launching_is_refused_when_suppressed_or_non_interactive() {
        assert_eq!(
            launch_refusal(Some("1"), true),
            Some(BrowserLaunch::SuppressedByEnv)
        );
        assert_eq!(
            launch_refusal(None, false),
            Some(BrowserLaunch::NotATerminal)
        );
        // An empty value is not a suppression request.
        assert_eq!(launch_refusal(Some(""), true), None);
    }
}

use super::*;
use semver::Version;
use serde::Deserialize;
use std::fs;

const DEFAULT_INSTALL_HOST: &str = "https://install.bowline.sh";
const ENV_MANIFEST_URL: &str = "BOWLINE_UPDATE_MANIFEST_URL";
const ENV_CACHE_PATH: &str = "BOWLINE_UPDATE_CACHE";
const ENV_DISABLE_UPDATE_CHECK: &str = "BOWLINE_UPDATE_DISABLE";
const CACHE_TTL: Duration = Duration::from_secs(24 * 60 * 60);
const UPDATE_EXECUTION_DISABLED: &str =
    "Bowline update execution is disabled until release manifests are verified by the CLI";

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub(super) struct ReleaseManifest {
    pub(super) version: String,
    #[serde(default)]
    pub(super) urgency: UpdateUrgency,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub(super) enum UpdateUrgency {
    #[default]
    Normal,
    Required,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct UpdateCheck {
    pub(super) current_version: String,
    pub(super) latest_version: String,
    pub(super) update_available: bool,
    pub(super) urgency: UpdateUrgency,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct UpdateStatusRevision {
    exists: bool,
    len: u64,
    modified: Option<std::time::SystemTime>,
}

pub(super) fn update_status_revision() -> UpdateStatusRevision {
    update_status_revision_at(&cache_path(None))
}

pub(super) fn update_status_revision_at(path: &Path) -> UpdateStatusRevision {
    let metadata = fs::metadata(path).ok();
    UpdateStatusRevision {
        exists: metadata.is_some(),
        len: metadata.as_ref().map_or(0, fs::Metadata::len),
        modified: metadata.as_ref().and_then(|value| value.modified().ok()),
    }
}

pub(super) fn print_update(args: UpdateArgs, json: bool) -> ExitCode {
    let generated_at = generated_at();
    let check = match check_for_update_fresh(args.version.as_deref()) {
        Ok(check) => check,
        Err(error) => {
            print_runtime_error(CommandName::Update, generated_at, &error, json);
            return ExitCode::from(EXIT_RUNTIME);
        }
    };
    let output = update_output(&check, &generated_at, args.version.as_deref());
    if let Err(error) = validate_requested_update_target(&check, args.version.as_deref()) {
        print_runtime_error(CommandName::Update, generated_at, &error, json);
        return ExitCode::from(EXIT_RUNTIME);
    }

    if args.check {
        if json {
            print_json(&output);
        } else {
            print!("{}", render_update_human(&check));
        }
        return ExitCode::SUCCESS;
    }

    if !check.update_available && args.version.is_none() {
        if json {
            print_json(&output);
        } else {
            println!("Bowline is up to date ({CLI_VERSION}).");
        }
        return ExitCode::SUCCESS;
    }

    let message = update_execution_disabled_message(&check);
    print_runtime_error(CommandName::Update, generated_at, &message, json);
    ExitCode::from(EXIT_RUNTIME)
}

pub(super) fn check_for_update(
    version: Option<&str>,
    allow_network: bool,
) -> Result<UpdateCheck, String> {
    check_for_update_with_policy(version, allow_network, false)
}

fn check_for_update_fresh(version: Option<&str>) -> Result<UpdateCheck, String> {
    check_for_update_with_policy(version, true, true)
}

fn check_for_update_with_policy(
    version: Option<&str>,
    allow_network: bool,
    force_fetch: bool,
) -> Result<UpdateCheck, String> {
    let manifest = load_manifest(version, allow_network, force_fetch)?;
    Ok(UpdateCheck {
        current_version: CLI_VERSION.to_string(),
        latest_version: manifest.version.clone(),
        update_available: version_is_newer(&manifest.version, CLI_VERSION),
        urgency: manifest.urgency,
    })
}

pub(super) fn attach_update_status_if_available(
    output: &mut StatusCommandOutput,
    allow_network: bool,
) {
    if env::var(ENV_DISABLE_UPDATE_CHECK).ok().as_deref() == Some("1") {
        return;
    }
    let Ok(check) = check_for_update(None, allow_network) else {
        return;
    };
    if !check.update_available {
        return;
    }

    attach_update_check_status(output, &check);
}

fn attach_update_check_status(output: &mut StatusCommandOutput, check: &UpdateCheck) {
    crate::status_commands::append_status_fact(
        output,
        "client.update_available",
        format!("client-update:{}", check.latest_version),
        "client-update",
        StatusFactScope::Device,
        None,
        None,
    );
    output.items.push(StatusItem {
        kind: StatusItemKind::Update,
        summary: format!(
            "Bowline update available: {} -> {}.",
            check.current_version, check.latest_version
        ),
        subject: Some(StatusSubject {
            kind: StatusSubjectKind::Component,
            id: format!("bowline-update-{}", check.latest_version),
            path: None,
        }),
        path: None,
        classification: None,
        mode: None,
        access: Vec::new(),
        event_id: None,
        event_name: None,
        device_id: None,
        lease_id: None,
        project_id: output.project_id.clone(),
        snapshot_id: None,
        policy_version: None,
        env_record_id: None,
    });
}

fn load_manifest(
    version: Option<&str>,
    allow_network: bool,
    force_fetch: bool,
) -> Result<ReleaseManifest, String> {
    let cache = cache_path(version);
    if allow_network && (force_fetch || should_fetch(&cache)) {
        match curl_text(&manifest_url(version), 2) {
            Ok(text) => {
                let manifest = parse_manifest(&text)?;
                if let Some(parent) = cache.parent() {
                    let _ = fs::create_dir_all(parent);
                }
                let _ = fs::write(&cache, text);
                return Ok(manifest);
            }
            Err(error) if force_fetch => {
                return Err(format!("could not fetch release manifest: {error}"));
            }
            Err(_) => {}
        }
    }
    let text = fs::read_to_string(&cache)
        .map_err(|_| "could not fetch release manifest and no cached manifest is available")?;
    parse_manifest(&text)
}

fn parse_manifest(text: &str) -> Result<ReleaseManifest, String> {
    serde_json::from_str(text).map_err(|error| format!("invalid release manifest: {error}"))
}

fn should_fetch(path: &Path) -> bool {
    let Ok(metadata) = fs::metadata(path) else {
        return true;
    };
    let Ok(modified) = metadata.modified() else {
        return true;
    };
    modified.elapsed().map_or(true, |age| age >= CACHE_TTL)
}

fn curl_text(url: &str, timeout_secs: u64) -> Result<String, String> {
    let output = ProcessCommand::new("curl")
        .args([
            "-fsSL",
            "--retry",
            "1",
            "--max-time",
            &timeout_secs.to_string(),
            url,
        ])
        .output()
        .map_err(|error| error.to_string())?;
    if !output.status.success() {
        return Err(String::from_utf8_lossy(&output.stderr).trim().to_string());
    }
    String::from_utf8(output.stdout).map_err(|error| error.to_string())
}

fn version_is_newer(latest: &str, current: &str) -> bool {
    let Ok(latest) = Version::parse(latest.trim_start_matches('v')) else {
        return false;
    };
    let Ok(current) = Version::parse(current.trim_start_matches('v')) else {
        return false;
    };
    latest > current
}

fn update_output(
    check: &UpdateCheck,
    generated_at: &str,
    requested_version: Option<&str>,
) -> UpdateCommandOutput {
    UpdateCommandOutput {
        contract_version: CONTRACT_VERSION,
        ok: true,
        command: CommandName::Update,
        generated_at: generated_at.to_string(),
        current_version: check.current_version.clone(),
        latest_version: check.latest_version.clone(),
        update_available: check.update_available,
        update_command: update_command(requested_version),
    }
}

fn validate_requested_update_target(
    check: &UpdateCheck,
    requested_version: Option<&str>,
) -> Result<(), String> {
    if requested_version.is_some() && !check.update_available {
        return Err(format!(
            "requested version {} is not newer than current {}",
            check.latest_version, check.current_version
        ));
    }
    Ok(())
}

fn update_execution_disabled_message(check: &UpdateCheck) -> String {
    format!(
        "{UPDATE_EXECUTION_DISABLED}. Latest advisory version: {}.",
        check.latest_version
    )
}

fn render_update_human(check: &UpdateCheck) -> String {
    if check.update_available {
        format!(
            "Bowline update available: {} -> {}\n{UPDATE_EXECUTION_DISABLED}.\n",
            check.current_version, check.latest_version
        )
    } else {
        format!("Bowline is up to date ({})\n", check.current_version)
    }
}

fn update_command(version: Option<&str>) -> String {
    match version {
        Some(version) => format!("bowline update --check --version {version}"),
        None => "bowline update --check".to_string(),
    }
}

fn manifest_url(version: Option<&str>) -> String {
    if let Ok(url) = env::var(ENV_MANIFEST_URL) {
        return url;
    }
    match version {
        Some(version) if version.starts_with('v') => {
            format!("{DEFAULT_INSTALL_HOST}/releases/{version}/release-manifest.json")
        }
        Some(version) => {
            format!("{DEFAULT_INSTALL_HOST}/releases/v{version}/release-manifest.json")
        }
        None => format!("{DEFAULT_INSTALL_HOST}/release-manifest.json"),
    }
}

fn cache_path(version: Option<&str>) -> PathBuf {
    if let Ok(path) = env::var(ENV_CACHE_PATH) {
        return PathBuf::from(path);
    }
    let name = version
        .map(|version| format!("release-manifest-{version}.json"))
        .unwrap_or_else(|| "release-manifest.json".to_string());
    env::var("HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|_| env::temp_dir())
        .join(".local/state/bowline")
        .join(name)
}

#[cfg(test)]
mod tests {
    use super::*;
    use bowline_core::status::StatusLevel;

    #[test]
    fn semver_check_detects_newer_versions() {
        assert!(version_is_newer("0.1.1", "0.1.0"));
        assert!(version_is_newer("v1.0.0", "0.9.9"));
        assert!(version_is_newer("0.2.0", "0.2.0-beta.1"));
        assert!(!version_is_newer("0.1.0", "0.1.0"));
        assert!(!version_is_newer("0.0.9", "0.1.0"));
        assert!(!version_is_newer("0.2.0-beta.1", "0.2.0"));
    }

    #[test]
    fn parses_required_manifest() {
        let manifest = parse_manifest(r#"{"version":"9.0.0","urgency":"required"}"#).unwrap();

        assert_eq!(manifest.version, "9.0.0");
        assert_eq!(manifest.urgency, UpdateUrgency::Required);
    }

    #[test]
    fn unsigned_required_manifest_recommends_update_without_adding_action() {
        let check = update_check("9.0.0", UpdateUrgency::Required);
        let mut output = status_output();

        attach_update_check_status(&mut output, &check);

        assert_eq!(output.status.level, StatusLevel::Attention);
        assert!(output.status.attention_items.is_empty());
        assert_eq!(output.items.len(), 1);
        assert_eq!(
            output.items[0].summary,
            format!("Bowline update available: {} -> 9.0.0.", CLI_VERSION)
        );
        assert!(output.next_actions.is_empty());
    }

    #[test]
    fn optional_manifest_recommends_update_without_action() {
        let check = update_check("9.0.0", UpdateUrgency::Normal);
        let mut output = status_output();

        attach_update_check_status(&mut output, &check);

        assert_eq!(output.status.level, StatusLevel::Attention);
        assert!(output.status.attention_items.is_empty());
        assert_eq!(output.items.len(), 1);
        assert_eq!(
            output.items[0].summary,
            format!("Bowline update available: {} -> 9.0.0.", CLI_VERSION)
        );
        assert!(output.next_actions.is_empty());
    }

    #[test]
    fn pinned_update_rejects_non_newer_version() {
        let check = UpdateCheck {
            current_version: CLI_VERSION.to_string(),
            latest_version: CLI_VERSION.to_string(),
            update_available: false,
            urgency: UpdateUrgency::Normal,
        };

        let error = validate_requested_update_target(&check, Some(CLI_VERSION)).unwrap_err();

        assert_eq!(
            error,
            format!("requested version {CLI_VERSION} is not newer than current {CLI_VERSION}")
        );
    }

    #[test]
    fn pinned_update_allows_newer_version() {
        let check = update_check("9.0.0", UpdateUrgency::Normal);

        assert!(validate_requested_update_target(&check, Some("9.0.0")).is_ok());
    }

    #[test]
    fn update_execution_disabled_message_names_latest_advisory_version() {
        let check = update_check("9.0.0", UpdateUrgency::Normal);

        assert_eq!(
            update_execution_disabled_message(&check),
            "Bowline update execution is disabled until release manifests are verified by the CLI. Latest advisory version: 9.0.0."
        );
    }

    #[test]
    fn update_command_output_points_to_check_only_path() {
        assert_eq!(update_command(None), "bowline update --check");
        assert_eq!(
            update_command(Some("9.0.0")),
            "bowline update --check --version 9.0.0"
        );
    }

    fn update_check(latest_version: &str, urgency: UpdateUrgency) -> UpdateCheck {
        UpdateCheck {
            current_version: CLI_VERSION.to_string(),
            latest_version: latest_version.to_string(),
            update_available: true,
            urgency,
        }
    }

    fn status_output() -> StatusCommandOutput {
        StatusCommandOutput {
            contract_version: CONTRACT_VERSION,
            command: CommandName::Status,
            generated_at: "2026-07-05T12:00:00Z".to_string(),
            workspace_id: WorkspaceId::new("workspace_update_test"),
            project_id: None,
            scope: None,
            requested_path: None,
            resolved_workspace_root: Some("/tmp/workspace".to_string()),
            workspace_summary: None,
            setup_readiness: None,
            sync_queue: None,
            freshness: bowline_core::status::FreshnessVerdict::Unknown,
            stale_bases: Vec::new(),
            status: bowline_core::status::WorkspaceStatus::healthy(),
            status_summary: bowline_core::status::reduce_status_facts(
                Vec::new(),
                1,
                "2026-07-05T12:00:00Z",
            ),
            items: Vec::new(),
            limits: Vec::new(),
            event_watermarks: bowline_core::status::EventWatermarks {
                last_scan_at: None,
                last_event_id: None,
                event_lag_ms: None,
                sync_state: None,
                watcher_state: None,
                network_state: None,
            },
            next_actions: Vec::new(),
            device_approvals: Vec::new(),
        }
    }
}

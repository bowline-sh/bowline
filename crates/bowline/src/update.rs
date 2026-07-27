use super::*;
use semver::Version;
use serde::Deserialize;
use std::collections::BTreeMap;
use std::fmt;
use std::fs;

const DEFAULT_INSTALL_HOST: &str = "https://install.bowline.sh";
const ENV_MANIFEST_URL: &str = "BOWLINE_UPDATE_MANIFEST_URL";
const ENV_CACHE_PATH: &str = "BOWLINE_UPDATE_CACHE";
const ENV_DISABLE_UPDATE_CHECK: &str = "BOWLINE_UPDATE_DISABLE";
const CACHE_TTL: Duration = Duration::from_secs(24 * 60 * 60);

/// The one pinned release key, shared byte-for-byte with `scripts/install.sh`
/// through `scripts/release-signing-key.pub`, so the CLI and the install script
/// can never drift apart. There is deliberately no runtime override.
const RELEASE_SIGNING_PUBKEY: &str = include_str!("../../../scripts/release-signing-key.pub");
const RELEASE_SIGNING_IDENTITY: &str = "bowline-release";
const RELEASE_SIGNING_NAMESPACE: &str = "bowline-release";
/// Manifest key for the release's own install script (`manifestKey` in
/// `scripts/release-assets.mjs`).
const INSTALLER_ARTIFACT_KEY: &str = "installer";
const MANIFEST_FETCH_TIMEOUT_SECONDS: &str = "10";

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub(super) struct ReleaseManifest {
    pub(super) version: String,
    #[serde(default)]
    pub(super) urgency: UpdateUrgency,
    /// Sorted so artifact iteration and error text are deterministic.
    #[serde(default)]
    pub(super) artifacts: BTreeMap<String, ReleaseArtifact>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub(super) struct ReleaseArtifact {
    pub(super) url: String,
    pub(super) sha256: String,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub(super) enum UpdateUrgency {
    #[default]
    Normal,
    Required,
}

/// How far an update check may go for a fresh manifest. `status` is the hottest
/// command in the product, so it reads the cache and never spawns a fetch.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum UpdateCheckNetwork {
    CacheOnly,
    CachedOrFetch,
    Fresh,
}

impl UpdateCheckNetwork {
    fn may_fetch(self) -> bool {
        !matches!(self, Self::CacheOnly)
    }

    fn must_fetch(self) -> bool {
        matches!(self, Self::Fresh)
    }
}

#[derive(Debug)]
pub(super) enum UpdateError {
    ManifestFetch { url: String, detail: String },
    ManifestUnverified { detail: String },
    ManifestInvalid { detail: String },
    ManifestUnavailable,
    MissingArtifact { key: &'static str },
    ArtifactHashMismatch { name: &'static str },
    NotNewer { requested: String, current: String },
    ToolMissing { tool: &'static str },
    InstallFailed { detail: String },
    Workspace { detail: String },
}

impl fmt::Display for UpdateError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::ManifestFetch { url, detail } => {
                write!(formatter, "could not fetch {url}: {detail}")
            }
            Self::ManifestUnverified { detail } => write!(
                formatter,
                "release signature did not verify against the pinned Bowline release key: {detail}"
            ),
            Self::ManifestInvalid { detail } => {
                write!(formatter, "invalid release manifest: {detail}")
            }
            Self::ManifestUnavailable => formatter.write_str(
                "no verified release manifest is available; check network access and retry",
            ),
            Self::MissingArtifact { key } => write!(
                formatter,
                "the release manifest does not publish a `{key}` artifact"
            ),
            Self::ArtifactHashMismatch { name } => write!(
                formatter,
                "{name} does not match the hash pinned by the signed release manifest"
            ),
            Self::NotNewer { requested, current } => write!(
                formatter,
                "requested version {requested} is not newer than current {current}"
            ),
            Self::ToolMissing { tool } => {
                write!(formatter, "{tool} is required to install a Bowline release")
            }
            Self::InstallFailed { detail } => {
                write!(formatter, "the release installer failed: {detail}")
            }
            Self::Workspace { detail } => {
                write!(
                    formatter,
                    "could not prepare a download directory: {detail}"
                )
            }
        }
    }
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
    let requested = args.version.as_deref();
    // A check is cheap and repeatable, so it honours the cache TTL rather than
    // hammering the release host; installing must resolve a fresh manifest.
    let network = if args.check {
        UpdateCheckNetwork::CachedOrFetch
    } else {
        UpdateCheckNetwork::Fresh
    };
    let manifest = match load_manifest(requested, network) {
        Ok(manifest) => manifest,
        Err(error) => {
            return print_runtime_error(
                CommandName::Update,
                generated_at,
                &error.to_string(),
                json,
            )
            .into();
        }
    };
    let check = update_check_for(&manifest);

    if args.check {
        if json {
            print_json(&update_output(&check, &generated_at));
        } else {
            print!("{}", render_update_human(&check));
        }
        return ExitCode::SUCCESS;
    }

    if let Err(error) = validate_requested_update_target(&check, requested) {
        return print_runtime_error(CommandName::Update, generated_at, &error.to_string(), json)
            .into();
    }

    if !check.update_available && requested.is_none() {
        if json {
            print_json(&update_output(&check, &generated_at));
        } else {
            println!("Bowline is up to date ({CLI_VERSION}).");
        }
        return ExitCode::SUCCESS;
    }

    match install_release(&manifest) {
        Ok(()) => {
            if json {
                print_json(&update_output(&installed_check(&check), &generated_at));
            } else {
                println!(
                    "Bowline updated: {} -> {}.",
                    check.current_version, check.latest_version
                );
            }
            ExitCode::SUCCESS
        }
        Err(error) => {
            print_runtime_error(CommandName::Update, generated_at, &error.to_string(), json).into()
        }
    }
}

pub(super) fn check_for_update(
    version: Option<&str>,
    network: UpdateCheckNetwork,
) -> Result<UpdateCheck, UpdateError> {
    Ok(update_check_for(&load_manifest(version, network)?))
}

fn update_check_for(manifest: &ReleaseManifest) -> UpdateCheck {
    UpdateCheck {
        current_version: CLI_VERSION.to_string(),
        latest_version: manifest.version.clone(),
        update_available: version_is_newer(&manifest.version, CLI_VERSION),
        urgency: manifest.urgency,
    }
}

/// The machine view after a successful install describes the Bowline that is now
/// on disk, not the process that performed the swap.
fn installed_check(check: &UpdateCheck) -> UpdateCheck {
    UpdateCheck {
        current_version: check.latest_version.clone(),
        latest_version: check.latest_version.clone(),
        update_available: false,
        urgency: check.urgency,
    }
}

pub(super) fn attach_update_status_if_available(
    output: &mut StatusCommandOutput,
    network: UpdateCheckNetwork,
) {
    if env::var(ENV_DISABLE_UPDATE_CHECK).ok().as_deref() == Some("1") {
        return;
    }
    let Ok(check) = check_for_update(None, network) else {
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
    output.next_actions.push(RepairCommand::mutating(
        "Install the latest Bowline".to_string(),
        Some(update_command(None)),
    ));
}

fn load_manifest(
    version: Option<&str>,
    network: UpdateCheckNetwork,
) -> Result<ReleaseManifest, UpdateError> {
    let cache = cache_path(version);
    let signature = signature_cache_path(&cache);
    if network.may_fetch() && (network.must_fetch() || should_fetch(&cache)) {
        match fetch_manifest(version, &cache, &signature) {
            Ok(manifest) => return Ok(manifest),
            Err(error) if network.must_fetch() => return Err(error),
            Err(_) => {}
        }
    }
    let text = fs::read_to_string(&cache).map_err(|_| UpdateError::ManifestUnavailable)?;
    // The cache is a local file like any other; re-verify it before trusting a
    // version claim, so a tampered cache cannot drive the update surface.
    verify_release_signature(text.as_bytes(), &signature)?;
    parse_manifest(&text)
}

fn fetch_manifest(
    version: Option<&str>,
    cache: &Path,
    signature: &Path,
) -> Result<ReleaseManifest, UpdateError> {
    let url = manifest_url(version);
    let text = curl_text(&url)?;
    let signature_bytes = curl_bytes(&format!("{url}.sig"))?;
    write_cache_file(signature, &signature_bytes)?;
    verify_release_signature(text.as_bytes(), signature)?;
    let manifest = parse_manifest(&text)?;
    write_cache_file(cache, text.as_bytes())?;
    Ok(manifest)
}

fn write_cache_file(path: &Path, bytes: &[u8]) -> Result<(), UpdateError> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).map_err(|error| UpdateError::Workspace {
            detail: error.to_string(),
        })?;
    }
    fs::write(path, bytes).map_err(|error| UpdateError::Workspace {
        detail: error.to_string(),
    })
}

fn signature_cache_path(cache: &Path) -> PathBuf {
    let mut name = cache.as_os_str().to_os_string();
    name.push(".sig");
    PathBuf::from(name)
}

/// Verify signed release bytes with the same `ssh-keygen -Y verify` path
/// `scripts/install.sh` uses, against the same pinned key.
fn verify_release_signature(data: &[u8], signature: &Path) -> Result<(), UpdateError> {
    let workspace = TempWorkspace::new("bowline-update-verify")?;
    let allowed_signers = workspace.path().join("allowed-signers");
    fs::write(
        &allowed_signers,
        format!(
            "{RELEASE_SIGNING_IDENTITY} {}\n",
            RELEASE_SIGNING_PUBKEY.trim()
        ),
    )
    .map_err(|error| UpdateError::Workspace {
        detail: error.to_string(),
    })?;

    let mut child = ProcessCommand::new("ssh-keygen")
        .arg("-Y")
        .arg("verify")
        .arg("-f")
        .arg(&allowed_signers)
        .arg("-I")
        .arg(RELEASE_SIGNING_IDENTITY)
        .arg("-n")
        .arg(RELEASE_SIGNING_NAMESPACE)
        .arg("-s")
        .arg(signature)
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .map_err(|_| UpdateError::ToolMissing { tool: "ssh-keygen" })?;
    if let Some(stdin) = child.stdin.as_mut() {
        stdin
            .write_all(data)
            .map_err(|error| UpdateError::ManifestUnverified {
                detail: error.to_string(),
            })?;
    }
    // Dropping stdin closes the pipe so ssh-keygen sees end of input.
    drop(child.stdin.take());
    let output = child
        .wait_with_output()
        .map_err(|error| UpdateError::ManifestUnverified {
            detail: error.to_string(),
        })?;
    if output.status.success() {
        return Ok(());
    }
    Err(UpdateError::ManifestUnverified {
        detail: process_failure_detail(&output.stderr),
    })
}

/// Download, verify and run the release's own install script. The installer is
/// the single implementation of "put this Bowline on this machine": it verifies
/// every artifact against the pinned key, handles both the CLI archive and the
/// macOS app bundle, and restarts the daemon on the newly installed build.
fn install_release(manifest: &ReleaseManifest) -> Result<(), UpdateError> {
    let artifact =
        manifest
            .artifacts
            .get(INSTALLER_ARTIFACT_KEY)
            .ok_or(UpdateError::MissingArtifact {
                key: INSTALLER_ARTIFACT_KEY,
            })?;
    let workspace = TempWorkspace::new("bowline-update-install")?;
    let installer = workspace.path().join("install.sh");
    curl_download(&artifact.url, &installer)?;
    if sha256_hex(&installer)? != artifact.sha256 {
        return Err(UpdateError::ArtifactHashMismatch { name: "install.sh" });
    }
    run_installer(&installer, &manifest.version)
}

fn run_installer(installer: &Path, version: &str) -> Result<(), UpdateError> {
    let output = ProcessCommand::new("sh")
        .arg(installer)
        .arg("--version")
        .arg(version)
        .output()
        .map_err(|_| UpdateError::ToolMissing { tool: "sh" })?;
    if output.status.success() {
        return Ok(());
    }
    Err(UpdateError::InstallFailed {
        detail: process_failure_detail(&output.stderr),
    })
}

/// Hash tools in the order `scripts/install.sh` probes them, so a host that can
/// run the installer can also verify what the CLI hands it.
const SHA256_TOOLS: &[(&str, &[&str])] = &[("shasum", &["-a", "256"]), ("sha256sum", &[])];

fn sha256_hex(path: &Path) -> Result<String, UpdateError> {
    for (tool, args) in SHA256_TOOLS {
        let Ok(output) = ProcessCommand::new(tool).args(*args).arg(path).output() else {
            continue;
        };
        if !output.status.success() {
            continue;
        }
        if let Some(digest) = String::from_utf8_lossy(&output.stdout)
            .split_whitespace()
            .next()
        {
            return Ok(digest.to_string());
        }
    }
    Err(UpdateError::ToolMissing {
        tool: "shasum or sha256sum",
    })
}

fn process_failure_detail(stderr: &[u8]) -> String {
    let text = String::from_utf8_lossy(stderr);
    let detail = text.trim();
    if detail.is_empty() {
        return "no diagnostic output".to_string();
    }
    detail.to_string()
}

/// A self-cleaning scratch directory: downloads never touch the user's install
/// location until they have been verified against the signed manifest.
struct TempWorkspace {
    path: PathBuf,
}

impl TempWorkspace {
    fn new(prefix: &str) -> Result<Self, UpdateError> {
        let unique = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|elapsed| elapsed.as_nanos())
            .unwrap_or_default();
        let path = env::temp_dir().join(format!("{prefix}-{}-{unique}", std::process::id()));
        fs::create_dir_all(&path).map_err(|error| UpdateError::Workspace {
            detail: error.to_string(),
        })?;
        Ok(Self { path })
    }

    fn path(&self) -> &Path {
        &self.path
    }
}

impl Drop for TempWorkspace {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.path);
    }
}

fn parse_manifest(text: &str) -> Result<ReleaseManifest, UpdateError> {
    serde_json::from_str(text).map_err(|error| UpdateError::ManifestInvalid {
        detail: error.to_string(),
    })
}

fn should_fetch(path: &Path) -> bool {
    let Ok(metadata) = fs::metadata(path) else {
        return true;
    };
    let Ok(modified) = metadata.modified() else {
        return true;
    };
    modified.elapsed().is_ok_and(|age| age >= CACHE_TTL)
}

fn curl_text(url: &str) -> Result<String, UpdateError> {
    String::from_utf8(curl_bytes(url)?).map_err(|error| UpdateError::ManifestFetch {
        url: url.to_string(),
        detail: error.to_string(),
    })
}

fn curl_bytes(url: &str) -> Result<Vec<u8>, UpdateError> {
    let output = ProcessCommand::new("curl")
        .args([
            "-fsSL",
            "--retry",
            "1",
            "--max-time",
            MANIFEST_FETCH_TIMEOUT_SECONDS,
            url,
        ])
        .output()
        .map_err(|_| UpdateError::ToolMissing { tool: "curl" })?;
    if !output.status.success() {
        return Err(UpdateError::ManifestFetch {
            url: url.to_string(),
            detail: process_failure_detail(&output.stderr),
        });
    }
    Ok(output.stdout)
}

fn curl_download(url: &str, destination: &Path) -> Result<(), UpdateError> {
    let output = ProcessCommand::new("curl")
        .arg("-fL")
        .arg("--retry")
        .arg("3")
        .arg("--retry-delay")
        .arg("1")
        .arg("-o")
        .arg(destination)
        .arg(url)
        .output()
        .map_err(|_| UpdateError::ToolMissing { tool: "curl" })?;
    if output.status.success() {
        return Ok(());
    }
    Err(UpdateError::ManifestFetch {
        url: url.to_string(),
        detail: process_failure_detail(&output.stderr),
    })
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

fn update_output(check: &UpdateCheck, generated_at: &str) -> UpdateCommandOutput {
    UpdateCommandOutput {
        contract_version: CONTRACT_VERSION,
        ok: true,
        command: CommandName::Update,
        generated_at: generated_at.to_string(),
        current_version: check.current_version.clone(),
        latest_version: check.latest_version.clone(),
        update_available: check.update_available,
        update_command: update_command(
            check
                .update_available
                .then_some(check.latest_version.as_str()),
        ),
    }
}

fn validate_requested_update_target(
    check: &UpdateCheck,
    requested_version: Option<&str>,
) -> Result<(), UpdateError> {
    if requested_version.is_some() && !check.update_available {
        return Err(UpdateError::NotNewer {
            requested: check.latest_version.clone(),
            current: check.current_version.clone(),
        });
    }
    Ok(())
}

fn render_update_human(check: &UpdateCheck) -> String {
    if check.update_available {
        format!(
            "Bowline update available: {} -> {}\nInstall it: {}\n",
            check.current_version,
            check.latest_version,
            update_command(None)
        )
    } else {
        format!("Bowline is up to date ({})\n", check.current_version)
    }
}

fn update_command(version: Option<&str>) -> String {
    match version {
        Some(version) => format!("bowline update --version {version}"),
        None => "bowline update".to_string(),
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
    fn parses_required_manifest_with_installer_artifact() {
        let manifest = parse_manifest(
            r#"{"version":"9.0.0","urgency":"required","artifacts":{"installer":{"url":"https://install.bowline.sh/releases/v9.0.0/install.sh","sha256":"abc"}}}"#,
        )
        .expect("manifest parses");

        assert_eq!(manifest.version, "9.0.0");
        assert_eq!(manifest.urgency, UpdateUrgency::Required);
        assert_eq!(
            manifest
                .artifacts
                .get(INSTALLER_ARTIFACT_KEY)
                .map(|artifact| artifact.sha256.as_str()),
            Some("abc")
        );
    }

    #[test]
    fn install_refuses_a_manifest_without_an_installer_artifact() {
        let manifest = parse_manifest(r#"{"version":"9.0.0"}"#).expect("manifest parses");

        let error = install_release(&manifest).expect_err("no installer artifact");

        assert!(matches!(
            error,
            UpdateError::MissingArtifact {
                key: INSTALLER_ARTIFACT_KEY
            }
        ));
    }

    #[test]
    fn signature_verification_rejects_unsigned_bytes() {
        let workspace = TempWorkspace::new("bowline-update-signature-test").expect("workspace");
        let signature = workspace.path().join("release-manifest.json.sig");
        fs::write(&signature, b"not a signature").expect("signature file");

        let error = verify_release_signature(br#"{"version":"9.9.9"}"#, &signature)
            .expect_err("unsigned manifests must not verify");

        assert!(matches!(
            error,
            UpdateError::ManifestUnverified { .. } | UpdateError::ToolMissing { .. }
        ));
    }

    #[test]
    fn pinned_release_key_is_the_shared_single_line_key() {
        let key = RELEASE_SIGNING_PUBKEY.trim();

        assert!(key.starts_with("ssh-ed25519 "));
        assert_eq!(key.lines().count(), 1);
    }

    #[test]
    fn update_status_points_at_the_install_verb() {
        let check = update_check("9.0.0", UpdateUrgency::Normal);
        let mut output = status_output();

        attach_update_check_status(&mut output, &check);

        assert_eq!(output.status.level, StatusLevel::Attention);
        assert_eq!(output.items.len(), 1);
        assert_eq!(
            output.items[0].summary,
            format!("Bowline update available: {CLI_VERSION} -> 9.0.0.")
        );
        assert_eq!(
            output
                .next_actions
                .iter()
                .filter_map(|action| action.command.as_deref())
                .collect::<Vec<_>>(),
            vec!["bowline update"]
        );
    }

    #[test]
    fn required_manifest_uses_the_same_actionable_update_path() {
        let check = update_check("9.0.0", UpdateUrgency::Required);
        let mut output = status_output();

        attach_update_check_status(&mut output, &check);

        assert_eq!(output.status.level, StatusLevel::Attention);
        assert_eq!(output.items.len(), 1);
        assert_eq!(
            output
                .next_actions
                .first()
                .and_then(|action| action.command.clone()),
            Some("bowline update".to_string())
        );
    }

    #[test]
    fn pinned_update_rejects_non_newer_version() {
        let check = UpdateCheck {
            current_version: CLI_VERSION.to_string(),
            latest_version: CLI_VERSION.to_string(),
            update_available: false,
            urgency: UpdateUrgency::Normal,
        };

        let error = validate_requested_update_target(&check, Some(CLI_VERSION))
            .expect_err("the same version is not newer");

        assert_eq!(
            error.to_string(),
            format!("requested version {CLI_VERSION} is not newer than current {CLI_VERSION}")
        );
    }

    #[test]
    fn pinned_update_allows_newer_version() {
        let check = update_check("9.0.0", UpdateUrgency::Normal);

        assert!(validate_requested_update_target(&check, Some("9.0.0")).is_ok());
    }

    #[test]
    fn update_command_output_points_at_a_command_that_installs() {
        let check = update_check("9.0.0", UpdateUrgency::Normal);

        let output = update_output(&check, "2026-07-05T12:00:00Z");

        assert_eq!(output.update_command, "bowline update --version 9.0.0");
        assert!(output.update_available);
    }

    #[test]
    fn up_to_date_output_points_at_the_plain_update_verb() {
        let check = UpdateCheck {
            current_version: CLI_VERSION.to_string(),
            latest_version: CLI_VERSION.to_string(),
            update_available: false,
            urgency: UpdateUrgency::Normal,
        };

        let output = update_output(&check, "2026-07-05T12:00:00Z");

        assert_eq!(output.update_command, "bowline update");
    }

    #[test]
    fn installed_check_describes_the_bowline_now_on_disk() {
        let installed = installed_check(&update_check("9.0.0", UpdateUrgency::Normal));

        assert_eq!(installed.current_version, "9.0.0");
        assert_eq!(installed.latest_version, "9.0.0");
        assert!(!installed.update_available);
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
            resolved_project_root: None,
            workspace_summary: None,
            setup_readiness: None,
            sync_queue: None,
            convergence: None,
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
            },
            next_actions: Vec::new(),
            device_approvals: Vec::new(),
            service: None,
            authentication: None,
            sync: None,
        }
    }
}

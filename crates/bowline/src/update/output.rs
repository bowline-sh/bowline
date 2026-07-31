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
    update_output_with_installation(check, generated_at, None)
}

fn installed_update_output(
    check: &UpdateCheck,
    generated_at: &str,
    installation_state: UpdateInstallationState,
) -> UpdateCommandOutput {
    update_output_with_installation(check, generated_at, Some(installation_state))
}

fn update_output_with_installation(
    check: &UpdateCheck,
    generated_at: &str,
    installation_state: Option<UpdateInstallationState>,
) -> UpdateCommandOutput {
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
        installation_state,
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

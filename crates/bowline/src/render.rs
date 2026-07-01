use super::*;

pub(super) fn render_search_human(output: &bowline_core::commands::SearchCommandOutput) -> String {
    let mut lines = Vec::new();
    lines.push(format!(
        "Search: {} results for `{}` ({})",
        output.results.len(),
        output.query,
        output.index.summary
    ));
    for result in &output.results {
        let line = result
            .line_start
            .map(|line| format!(":{line}"))
            .unwrap_or_default();
        lines.push(format!(
            "  {}{}  score {:.1}",
            result.path, line, result.score
        ));
        if let Some(snippet) = &result.snippet {
            lines.push(format!("    {snippet}"));
        }
    }
    if output.results.is_empty() {
        lines.push("  No indexed matches.".to_string());
    }
    lines.push(String::new());
    lines.join("\n")
}

pub(super) fn render_symbols_human(output: &bowline_core::commands::SymbolCommandOutput) -> String {
    let mut lines = Vec::new();
    lines.push(format!(
        "Symbols: {} results for `{}` ({})",
        output.symbols.len(),
        output.query,
        output.index.summary
    ));
    for symbol in &output.symbols {
        lines.push(format!(
            "  {}  {:?} {:?}  {}:{}",
            symbol.name, symbol.kind, symbol.language, symbol.path, symbol.line_start
        ));
    }
    if output.symbols.is_empty() {
        lines.push("  No indexed symbols.".to_string());
    }
    lines.push(String::new());
    lines.join("\n")
}

pub(super) fn render_login_human(output: &bowline_core::commands::LoginCommandOutput) -> String {
    let mut lines = Vec::new();
    match output.account.status {
        bowline_core::devices::AccountLoginStatus::LoginPending => {
            lines.push("Login: waiting for browser approval".to_string());
            if let Some(uri) = &output.account.verification_uri_complete {
                lines.push(format!("Open: {uri}"));
            } else if let Some(uri) = &output.account.verification_uri {
                lines.push(format!("Open: {uri}"));
            }
            if let Some(code) = &output.account.user_code {
                lines.push(format!("Code: {code}"));
            }
        }
        bowline_core::devices::AccountLoginStatus::AccountAuthenticated => {
            lines.push("Login: authenticated".to_string());
            if let Some(account_id) = &output.account.account_id {
                lines.push(format!("Account: {}", account_id.as_str()));
            }
        }
        bowline_core::devices::AccountLoginStatus::Expired => {
            lines.push("Login: expired".to_string());
        }
        bowline_core::devices::AccountLoginStatus::NotLoggedIn => {
            lines.push("Login: not logged in".to_string());
        }
    }
    append_next_actions(&mut lines, &output.next_actions);
    lines.push(String::new());
    lines.join("\n")
}

pub(super) fn render_logout_human(output: &bowline_core::commands::LogoutCommandOutput) -> String {
    let mut lines = Vec::new();
    if output.signed_out {
        lines.push("Logout: signed out".to_string());
    } else {
        lines.push("Logout: already signed out".to_string());
    }
    append_next_actions(&mut lines, &output.next_actions);
    lines.push(String::new());
    lines.join("\n")
}

pub(super) fn render_init_human(output: &bowline_core::commands::InitCommandOutput) -> String {
    let mut lines = vec![
        format!("Root: {}", output.root),
        "State: observed locally; sync has not started".to_string(),
        format!(
            "Observed: {} repos, {} workspace-sync paths, {} env files, {} generated/dependency paths",
            output.scan_summary.repo_count,
            output.scan_summary.workspace_sync_path_count,
            output.scan_summary.env_file_count,
            output.scan_summary.generated_path_count + output.scan_summary.dependency_path_count,
        ),
    ];
    if output.created_root {
        lines.push("Created root directory.".to_string());
    }
    if !output.non_actions.is_empty() {
        lines.push("Did not:".to_string());
        lines.extend(output.non_actions.iter().map(|item| format!("  {item}")));
    }
    if !output.next_actions.is_empty() {
        lines.push("Suggested actions:".to_string());
        lines.extend(
            output
                .next_actions
                .iter()
                .map(|action| match &action.command {
                    Some(command) => format!("  {}: {command}", action.label),
                    None => format!("  {}", action.label),
                }),
        );
    }
    lines.push(String::new());
    lines.join("\n")
}

pub(super) fn render_devices_human(
    output: &bowline_core::commands::DevicesCommandOutput,
) -> String {
    let mut lines = Vec::new();
    match output.action {
        bowline_core::commands::DeviceCommandAction::List => {
            lines.push(format!(
                "Devices: {} trusted, {} pending, {} revoked",
                output.devices.len(),
                output.pending_requests.len(),
                output.revoked_devices.len()
            ));
            lines.extend(output.devices.iter().map(|device| {
                let marker = if device.is_current_device {
                    " (this device)"
                } else {
                    ""
                };
                format!("  {}: trusted{marker}", device.name)
            }));
            lines.extend(output.pending_requests.iter().map(|request| {
                let state = match request.state {
                    bowline_core::devices::DeviceApprovalRequestState::Pending => {
                        "waiting for approval"
                    }
                    bowline_core::devices::DeviceApprovalRequestState::Approved => {
                        "approved, waiting for acceptance"
                    }
                    bowline_core::devices::DeviceApprovalRequestState::Denied => "denied",
                    bowline_core::devices::DeviceApprovalRequestState::Expired => "expired",
                };
                format!(
                    "  {}: {state}, code {} ({})",
                    request.device_name,
                    request.matching_code,
                    request.request_id.as_str()
                )
            }));
        }
        bowline_core::commands::DeviceCommandAction::Request => {
            if let Some(request) = &output.created_request {
                lines.push(format!("Device request: {}", request.request_id.as_str()));
                lines.push(format!("Code: {}", request.matching_code));
                lines.push("State: waiting for approval on an existing trusted device".to_string());
            } else {
                lines.push("Device request created.".to_string());
            }
        }
        bowline_core::commands::DeviceCommandAction::Approve => {
            if let Some(device) = &output.approved_device {
                lines.push(format!("Approved: {}", device.name));
            } else {
                lines.push("Device approved.".to_string());
            }
        }
        bowline_core::commands::DeviceCommandAction::Accept => {
            if let Some(device) = &output.local_device {
                lines.push(format!("Trusted: {}", device.name));
            } else {
                lines.push("Device grant accepted.".to_string());
            }
        }
        bowline_core::commands::DeviceCommandAction::Deny => {
            lines.push("Device request denied.".to_string());
        }
        bowline_core::commands::DeviceCommandAction::Revoke => {
            if let Some(device) = &output.revoked_device {
                lines.push(format!("Revoked: {}", device.name));
            } else {
                lines.push("Device revoked.".to_string());
            }
        }
    }
    append_next_actions(&mut lines, &output.next_actions);
    lines.push(String::new());
    lines.join("\n")
}

pub(super) fn render_recovery_human(output: &recovery::RecoveryRunOutput) -> String {
    let mut lines = Vec::new();
    match output.output.action {
        bowline_core::commands::RecoveryCommandAction::Status => {
            lines.push(format!(
                "Recovery Key: {}",
                recovery_lifecycle_label(output.output.recovery_key.lifecycle)
            ));
        }
        bowline_core::commands::RecoveryCommandAction::Create => {
            lines.push("Recovery Key created.".to_string());
            if let Some(words) = &output.generated_words {
                lines.push("Words:".to_string());
                lines.push(words.to_string());
            }
            lines.push("This is the only time bowline prints these words.".to_string());
        }
        bowline_core::commands::RecoveryCommandAction::Verify => {
            lines.push("Recovery Key verified.".to_string());
        }
        bowline_core::commands::RecoveryCommandAction::Rotate => {
            lines.push("Recovery Key rotated.".to_string());
            if let Some(words) = &output.generated_words {
                lines.push("Words:".to_string());
                lines.push(words.to_string());
            }
            lines.push("This is the only time bowline prints these words.".to_string());
        }
        bowline_core::commands::RecoveryCommandAction::Revoke => {
            lines.push("Recovery Key revoked.".to_string());
        }
        bowline_core::commands::RecoveryCommandAction::Use => {
            lines.push("Recovery Key used.".to_string());
        }
    }
    append_next_actions(&mut lines, &output.output.next_actions);
    lines.push(String::new());
    lines.join("\n")
}

pub(super) fn render_bootstrap_ssh_human(
    output: &bowline_core::commands::BootstrapSshCommandOutput,
) -> String {
    let mut lines = vec![
        format!("Bootstrap SSH: {}:{}", output.host, output.root),
        format!("Trusted: {}", if output.trusted { "yes" } else { "no" }),
    ];
    lines.extend(
        output
            .steps
            .iter()
            .map(|step| format!("  {}: {}", step.name, step.summary)),
    );
    append_next_actions(&mut lines, &output.next_actions);
    lines.push(String::new());
    lines.join("\n")
}

pub(super) fn recovery_lifecycle_label(
    lifecycle: bowline_core::devices::RecoveryKeyLifecycle,
) -> &'static str {
    match lifecycle {
        bowline_core::devices::RecoveryKeyLifecycle::Missing => "missing",
        bowline_core::devices::RecoveryKeyLifecycle::GeneratedUnverified => "generated, unverified",
        bowline_core::devices::RecoveryKeyLifecycle::Active => "active",
        bowline_core::devices::RecoveryKeyLifecycle::Rotated => "rotated",
        bowline_core::devices::RecoveryKeyLifecycle::Revoked => "revoked",
    }
}

pub(super) fn append_next_actions(lines: &mut Vec<String>, next_actions: &[SafeAction]) {
    if next_actions.is_empty() {
        return;
    }
    lines.push("Suggested actions:".to_string());
    lines.extend(next_actions.iter().map(|action| match &action.command {
        Some(command) => format!("  {}: {command}", action.label),
        None => format!("  {}", action.label),
    }));
}

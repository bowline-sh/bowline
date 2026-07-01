use super::*;

pub(super) fn metadata_db_path() -> Option<PathBuf> {
    env::var_os(ENV_METADATA_DB).map(PathBuf::from)
}

pub(super) fn generated_at() -> String {
    env::var(ENV_GENERATED_AT).unwrap_or_else(|_| {
        time::OffsetDateTime::now_utc()
            .format(&time::format_description::well_known::Rfc3339)
            .expect("UTC timestamp should format")
    })
}

pub(super) fn requested_path(explicit: Option<String>) -> Option<String> {
    explicit.map(resolve_explicit_path).or_else(|| {
        env::current_dir()
            .ok()
            .map(|path| path.display().to_string())
    })
}

pub(super) fn resolve_explicit_path(path: String) -> String {
    if path == "~" || path.starts_with("~/") {
        return path;
    }

    let path_buf = PathBuf::from(&path);
    if path_buf.is_absolute() {
        return path;
    }

    env::current_dir()
        .map(|cwd| cwd.join(path_buf).display().to_string())
        .unwrap_or(path)
}

pub(super) fn abbreviate_status_requested_path(output: &mut StatusCommandOutput) {
    output.requested_path = output
        .requested_path
        .as_deref()
        .map(abbreviate_requested_path);
}

pub(super) fn abbreviate_events_requested_path(output: &mut EventsCommandOutput) {
    output.requested_path = output
        .requested_path
        .as_deref()
        .map(abbreviate_requested_path);
}

pub(super) fn abbreviate_requested_path(path: &str) -> String {
    let path_buf = PathBuf::from(path);
    let Some(home) = env::var_os("HOME").map(PathBuf::from) else {
        return path.to_string();
    };
    let Ok(relative) = path_buf.strip_prefix(&home) else {
        return path.to_string();
    };

    if relative.as_os_str().is_empty() {
        return "~".to_string();
    }
    format!("~/{}", relative.display())
}

pub(super) fn print_json(value: &impl serde::Serialize) {
    println!(
        "{}",
        serde_json::to_string(value).expect("command output should serialize")
    );
}

pub(super) fn write_json_line(value: &impl serde::Serialize) -> io::Result<()> {
    let mut stdout = io::stdout().lock();
    serde_json::to_writer(&mut stdout, value)?;
    writeln!(stdout)?;
    stdout.flush()
}

pub(super) fn write_text(text: &str) -> io::Result<()> {
    let mut stdout = io::stdout().lock();
    stdout.write_all(text.as_bytes())?;
    stdout.flush()
}

pub(super) fn write_human_or_exit(
    command: CommandName,
    generated_at: String,
    text: &str,
) -> ExitCode {
    match write_text(text) {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) if error.kind() == io::ErrorKind::BrokenPipe => ExitCode::SUCCESS,
        Err(error) => {
            print_runtime_error(command, generated_at, &error.to_string(), false);
            ExitCode::from(EXIT_RUNTIME)
        }
    }
}

pub(super) fn run_confirmed_tui_command(command_line: &str, socket: &Path) -> ExitCode {
    let child_args = match confirmed_tui_child_args(command_line, socket) {
        Ok(args) => args,
        Err(error) => {
            print_runtime_error(CommandName::Tui, generated_at(), error, false);
            return ExitCode::from(EXIT_RUNTIME);
        }
    };
    let current_exe = match env::current_exe() {
        Ok(path) => path,
        Err(error) => {
            print_runtime_error(CommandName::Tui, generated_at(), &error.to_string(), false);
            return ExitCode::from(EXIT_RUNTIME);
        }
    };
    let status = match ProcessCommand::new(current_exe).args(child_args).status() {
        Ok(status) => status,
        Err(error) => {
            print_runtime_error(CommandName::Tui, generated_at(), &error.to_string(), false);
            return ExitCode::from(EXIT_RUNTIME);
        }
    };
    match status.code() {
        Some(0) => ExitCode::SUCCESS,
        Some(code) => ExitCode::from(code.try_into().unwrap_or(EXIT_RUNTIME)),
        None => ExitCode::from(EXIT_RUNTIME),
    }
}

pub(super) fn confirmed_tui_child_args(
    command_line: &str,
    socket: &Path,
) -> Result<Vec<OsString>, &'static str> {
    let words = split_tui_command_line(command_line)?;
    let Some((program, args)) = words.split_first() else {
        return Err("empty TUI action command");
    };
    if program != "bowline" {
        return Err("TUI action commands must start with `bowline`");
    }

    let mut child_args = vec![
        OsString::from("--socket"),
        socket.as_os_str().to_os_string(),
    ];
    child_args.extend(args.iter().map(OsString::from));
    Ok(child_args)
}

pub(super) fn split_tui_command_line(input: &str) -> Result<Vec<String>, &'static str> {
    let mut words = Vec::new();
    let mut current = String::new();
    let mut chars = input.chars().peekable();
    let mut in_single_quote = false;

    while let Some(ch) = chars.next() {
        if in_single_quote {
            if ch == '\'' {
                in_single_quote = false;
            } else {
                current.push(ch);
            }
            continue;
        }

        match ch {
            '\'' => in_single_quote = true,
            '\\' => {
                if let Some(next) = chars.next() {
                    current.push(next);
                } else {
                    current.push(ch);
                }
            }
            ch if ch.is_whitespace() => {
                if !current.is_empty() {
                    words.push(std::mem::take(&mut current));
                }
            }
            ch => current.push(ch),
        }
    }

    if in_single_quote {
        return Err("unterminated quote in TUI action command");
    }
    if !current.is_empty() {
        words.push(current);
    }
    Ok(words)
}

use super::*;

#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct Handshake {
    pub(super) daemon_version: String,
    pub(super) sync_json: Option<String>,
}

pub(super) struct SocketGuard {
    pub(super) path: PathBuf,
}

impl Drop for SocketGuard {
    fn drop(&mut self) {
        let _ = fs::remove_file(&self.path);
    }
}
pub(super) fn serve(socket: &Path, once: bool, mut runtime: DaemonRuntime) -> io::Result<()> {
    prepare_socket(socket)?;
    let listener = UnixListener::bind(socket)?;
    listener.set_nonblocking(true)?;
    let socket_owner_uid = fs::metadata(socket).ok().map(|metadata| metadata.uid());
    let _guard = SocketGuard {
        path: socket.to_path_buf(),
    };

    loop {
        runtime.poll_sync();
        runtime.poll_notifications();
        match listener.accept() {
            Ok((stream, _)) => {
                let shutdown = match handle_client(stream, &runtime, socket_owner_uid) {
                    Ok(shutdown) => shutdown,
                    Err(error) => {
                        eprintln!("bowline-daemon ignored client error: {error}");
                        false
                    }
                };
                if once || shutdown {
                    break;
                }
            }
            Err(error) if error.kind() == io::ErrorKind::WouldBlock => {
                thread_sleep_short();
            }
            Err(error) => return Err(error),
        }
    }

    Ok(())
}

pub(super) fn thread_sleep_short() {
    std::thread::sleep(Duration::from_millis(20));
}

pub(super) fn prepare_socket(socket: &Path) -> io::Result<()> {
    if let Some(parent) = socket.parent() {
        fs::create_dir_all(parent)?;
    }

    if socket.exists() {
        if UnixStream::connect(socket).is_ok() {
            return Err(io::Error::new(
                io::ErrorKind::AddrInUse,
                "daemon socket is already in use",
            ));
        }
        fs::remove_file(socket)?;
    }

    Ok(())
}

pub(super) fn handle_client(
    mut stream: UnixStream,
    runtime: &DaemonRuntime,
    socket_owner_uid: Option<u32>,
) -> io::Result<bool> {
    stream.set_read_timeout(Some(Duration::from_secs(2)))?;
    stream.set_write_timeout(Some(Duration::from_secs(2)))?;

    let request = read_line(&mut stream)?;
    let mut shutdown = false;
    let response = match daemon_request_type(&request).as_deref() {
        Some("hello") if is_hello_request(&request) => format!(
            "{{\"type\":\"hello_ack\",\"protocol\":\"{PROTOCOL}\",\"version\":{PROTOCOL_VERSION},\"daemonVersion\":{},\"status\":\"ok\"{}}}\n",
            json_string(env!("CARGO_PKG_VERSION")),
            runtime.sync_json_field()
        ),
        Some("shutdown") if is_shutdown_request(&request) => {
            shutdown = true;
            format!(
                "{{\"type\":\"shutdown_ack\",\"protocol\":\"{PROTOCOL}\",\"version\":{PROTOCOL_VERSION},\"status\":\"stopping\"}}\n"
            )
        }
        Some("agent.tool.invoke") => handle_agent_tool_request(
            &request,
            local_peer_credential_checked(&stream, socket_owner_uid),
        ),
        _ => format!(
            "{{\"type\":\"error\",\"protocol\":\"{PROTOCOL}\",\"version\":{PROTOCOL_VERSION},\"error\":{{\"code\":\"unsupported_request\",\"message\":\"supported request types: hello, shutdown, agent.tool.invoke\"}}}}\n"
        ),
    };

    stream.write_all(response.as_bytes())?;
    stream.flush()?;
    Ok(shutdown)
}

pub(super) fn daemon_request_type(request: &str) -> Option<String> {
    let value = serde_json::from_str::<serde_json::Value>(request).ok()?;
    value.get("type")?.as_str().map(ToOwned::to_owned)
}

pub(super) fn is_hello_request(request: &str) -> bool {
    let Ok(value) = serde_json::from_str::<serde_json::Value>(request) else {
        return false;
    };
    value.get("type").and_then(serde_json::Value::as_str) == Some("hello")
        && value.get("protocol").and_then(serde_json::Value::as_str) == Some(PROTOCOL)
        && value.get("version").and_then(serde_json::Value::as_u64) == Some(PROTOCOL_VERSION.into())
}

pub(super) fn is_shutdown_request(request: &str) -> bool {
    let Ok(value) = serde_json::from_str::<serde_json::Value>(request) else {
        return false;
    };
    value.get("type").and_then(serde_json::Value::as_str) == Some("shutdown")
        && value.get("protocol").and_then(serde_json::Value::as_str) == Some(PROTOCOL)
        && value.get("version").and_then(serde_json::Value::as_u64) == Some(PROTOCOL_VERSION.into())
}

pub(super) fn validate_agent_tool_contract(
    request: &AgentToolInvokeRequest,
) -> Result<(), &'static str> {
    if request.message_type != "agent.tool.invoke" {
        return Err("agent tool request type is unsupported");
    }
    if request.protocol_version != CONTRACT_VERSION {
        return Err("agent tool protocol version is unsupported");
    }
    Ok(())
}

pub(super) fn handle_agent_tool_request(request: &str, peer_credential_checked: bool) -> String {
    let request = match serde_json::from_str::<AgentToolInvokeRequest>(request) {
        Ok(request) => request,
        Err(error) => {
            return format!(
                "{{\"type\":\"error\",\"protocol\":\"{PROTOCOL}\",\"version\":{PROTOCOL_VERSION},\"error\":{{\"code\":\"invalid_agent_tool_request\",\"message\":{}}}}}\n",
                json_string(&error.to_string())
            );
        }
    };
    if let Err(message) = validate_agent_tool_contract(&request) {
        return format!(
            "{{\"type\":\"error\",\"protocol\":\"{PROTOCOL}\",\"version\":{PROTOCOL_VERSION},\"error\":{{\"code\":\"unsupported_agent_tool_protocol\",\"message\":{}}}}}\n",
            json_string(message)
        );
    }
    let local_daemon_peer_checked =
        peer_credential_checked && request.authority.transport == AgentToolTransport::LocalDaemon;
    match invoke_agent_tool_from_local_daemon(
        env::var_os(ENV_METADATA_DB).map(PathBuf::from),
        request,
        local_daemon_peer_checked,
        current_timestamp(),
    ) {
        Ok(result) => {
            let result_json = serde_json::to_string(&result).expect("agent result serializes");
            format!(
                "{{\"type\":\"agent.tool.result\",\"protocol\":\"{PROTOCOL}\",\"version\":{PROTOCOL_VERSION},\"result\":{result_json}}}\n"
            )
        }
        Err(error) => format!(
            "{{\"type\":\"error\",\"protocol\":\"{PROTOCOL}\",\"version\":{PROTOCOL_VERSION},\"error\":{{\"code\":\"agent_tool_failed\",\"message\":{}}}}}\n",
            json_string(&error.to_string())
        ),
    }
}

pub(super) fn current_timestamp() -> String {
    format_timestamp(OffsetDateTime::now_utc())
}

pub(super) fn format_timestamp(timestamp: OffsetDateTime) -> String {
    timestamp
        .format(&Rfc3339)
        .unwrap_or_else(|_| "1970-01-01T00:00:00Z".to_string())
}

pub(super) fn local_peer_credential_checked(
    stream: &UnixStream,
    socket_owner_uid: Option<u32>,
) -> bool {
    let Some(socket_owner_uid) = socket_owner_uid else {
        return false;
    };
    stream
        .initial_peer_credentials()
        .is_ok_and(|credentials| credentials.euid() == socket_owner_uid)
}

pub(super) fn handshake(socket: &Path) -> io::Result<Handshake> {
    let mut stream = UnixStream::connect(socket)?;
    stream.set_read_timeout(Some(Duration::from_secs(2)))?;
    stream.set_write_timeout(Some(Duration::from_secs(2)))?;
    stream.write_all(
        format!(
            "{{\"type\":\"hello\",\"protocol\":\"{PROTOCOL}\",\"version\":{PROTOCOL_VERSION}}}\n"
        )
        .as_bytes(),
    )?;
    stream.flush()?;

    let response = read_line(&mut stream)?;
    if !response.contains("\"type\":\"hello_ack\"")
        || !response.contains(&format!("\"protocol\":\"{PROTOCOL}\""))
        || !response.contains(&format!("\"version\":{PROTOCOL_VERSION}"))
    {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "daemon handshake response did not match the expected protocol",
        ));
    }

    Ok(Handshake {
        daemon_version: extract_json_string(&response, "daemonVersion")
            .unwrap_or_else(|| "unknown".to_string()),
        sync_json: extract_json_object(&response, "sync"),
    })
}

pub(super) fn request_shutdown(socket: &Path) -> io::Result<()> {
    let mut stream = UnixStream::connect(socket)?;
    stream.set_read_timeout(Some(Duration::from_secs(2)))?;
    stream.set_write_timeout(Some(Duration::from_secs(2)))?;
    stream.write_all(
        format!(
            "{{\"type\":\"shutdown\",\"protocol\":\"{PROTOCOL}\",\"version\":{PROTOCOL_VERSION}}}\n"
        )
        .as_bytes(),
    )?;
    stream.flush()?;

    let response = read_line(&mut stream)?;
    if !response.contains("\"type\":\"shutdown_ack\"")
        || !response.contains(&format!("\"protocol\":\"{PROTOCOL}\""))
        || !response.contains(&format!("\"version\":{PROTOCOL_VERSION}"))
    {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "daemon shutdown response did not match the expected protocol",
        ));
    }
    Ok(())
}

pub(super) fn read_line(stream: &mut UnixStream) -> io::Result<String> {
    let mut bytes = Vec::new();
    let mut one = [0_u8; 1];
    loop {
        match stream.read(&mut one) {
            Ok(0) => break,
            Ok(_) if one[0] == b'\n' => break,
            Ok(_) => bytes.push(one[0]),
            Err(error) => return Err(error),
        }
    }
    String::from_utf8(bytes).map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error))
}

pub(super) fn extract_json_string(input: &str, key: &str) -> Option<String> {
    let needle = format!("\"{key}\":\"");
    let start = input.find(&needle)? + needle.len();
    let mut value = String::new();
    let mut escaped = false;

    for character in input[start..].chars() {
        if escaped {
            value.push(character);
            escaped = false;
            continue;
        }
        match character {
            '\\' => escaped = true,
            '"' => return Some(value),
            _ => value.push(character),
        }
    }

    None
}

pub(super) fn extract_json_object(input: &str, key: &str) -> Option<String> {
    let needle = format!("\"{key}\":");
    let marker_start = input.find(&needle)?;
    let object_start =
        marker_start + needle.len() + input[marker_start + needle.len()..].find('{')?;
    let mut depth = 0_usize;
    let mut in_string = false;
    let mut escaped = false;

    for (offset, character) in input[object_start..].char_indices() {
        if escaped {
            escaped = false;
            continue;
        }
        match character {
            '\\' if in_string => escaped = true,
            '"' => in_string = !in_string,
            '{' if !in_string => depth += 1,
            '}' if !in_string => {
                depth = depth.saturating_sub(1);
                if depth == 0 {
                    let end = object_start + offset + character.len_utf8();
                    return Some(input[object_start..end].to_string());
                }
            }
            _ => {}
        }
    }

    None
}

pub(super) fn json_string(input: &str) -> String {
    let mut escaped = String::with_capacity(input.len() + 2);
    escaped.push('"');
    for character in input.chars() {
        match character {
            '"' => escaped.push_str("\\\""),
            '\\' => escaped.push_str("\\\\"),
            '\n' => escaped.push_str("\\n"),
            '\r' => escaped.push_str("\\r"),
            '\t' => escaped.push_str("\\t"),
            character if character.is_control() => {
                escaped.push_str(&format!("\\u{:04x}", character as u32));
            }
            character => escaped.push(character),
        }
    }
    escaped.push('"');
    escaped
}

pub(super) fn json_string_array(values: &[String]) -> String {
    format!(
        "[{}]",
        values
            .iter()
            .map(|value| json_string(value))
            .collect::<Vec<_>>()
            .join(",")
    )
}

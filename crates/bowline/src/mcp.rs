use std::{
    fmt,
    io::{BufRead, Write},
    path::Path,
    time::Duration,
};

use bowline_core::{
    commands::{
        AgentToolAuthority, AgentToolInvokeRequest, AgentToolResult, AgentToolResultOutcome,
        AgentToolTransport, CONTRACT_VERSION, CommandExitCode,
    },
    ids::LeaseId,
};
use bowline_daemon_rpc::{ClientOptions, DaemonClient};
use serde::Serialize;
use serde_json::{Value, json};

mod tool_specs;

use tool_specs::{tool_from_name, tools};

const MCP_PROTOCOL_VERSION: &str = "2025-11-25";
const MCP_SUPPORTED_PROTOCOL_VERSIONS: &[&str] = &["2025-11-25", "2025-06-18", "2024-11-05"];
const LEASE_ID_ENV: &str = "BOWLINE_LEASE_ID";
const MCP_TOKEN_FILE_ENV: &str = "BOWLINE_MCP_TOKEN_FILE";

#[derive(Debug, Clone)]
struct McpServerBinding {
    lease_id: Option<LeaseId>,
    token_file: Option<String>,
}

impl McpServerBinding {
    fn from_args(args: crate::cli::McpArgs) -> Self {
        Self::from_sources(
            args,
            std::env::var(LEASE_ID_ENV).ok(),
            std::env::var(MCP_TOKEN_FILE_ENV).ok(),
        )
    }

    fn from_sources(
        args: crate::cli::McpArgs,
        lease_id_env: Option<String>,
        token_file_env: Option<String>,
    ) -> Self {
        let lease_id = binding_value(args.lease_id, lease_id_env).map(LeaseId::new);
        let token_file = binding_value(args.token_file, token_file_env);
        Self {
            lease_id,
            token_file,
        }
    }
}

fn binding_value(flag_value: Option<String>, env_value: Option<String>) -> Option<String> {
    flag_value
        .or(env_value)
        .filter(|value| !value.trim().is_empty())
}

#[derive(Debug, Clone, Copy, Serialize)]
#[serde(rename_all = "camelCase")]
enum McpToolErrorCode {
    InvalidArguments,
    MissingLeaseBinding,
    MissingTokenFileBinding,
    DaemonInvocationFailed,
    ResultSerializationFailed,
}

#[derive(Debug)]
enum McpToolCallError {
    InvalidArguments(&'static str),
    MissingLeaseBinding,
    MissingTokenFileBinding,
    Daemon(DaemonInvokeError),
    ResultSerialization(serde_json::Error),
}

impl McpToolCallError {
    fn code(&self) -> McpToolErrorCode {
        match self {
            Self::InvalidArguments(_) => McpToolErrorCode::InvalidArguments,
            Self::MissingLeaseBinding => McpToolErrorCode::MissingLeaseBinding,
            Self::MissingTokenFileBinding => McpToolErrorCode::MissingTokenFileBinding,
            Self::Daemon(_) => McpToolErrorCode::DaemonInvocationFailed,
            Self::ResultSerialization(_) => McpToolErrorCode::ResultSerializationFailed,
        }
    }

    fn restart_instruction(&self) -> Option<&'static str> {
        match self {
            Self::MissingLeaseBinding => Some(
                "Restart with `bowline mcp --lease <id> --token-file <path>` or set BOWLINE_LEASE_ID and BOWLINE_MCP_TOKEN_FILE.",
            ),
            Self::MissingTokenFileBinding => Some(
                "Restart with `bowline mcp --token-file <path>` or set BOWLINE_MCP_TOKEN_FILE.",
            ),
            _ => None,
        }
    }
}

impl fmt::Display for McpToolCallError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::InvalidArguments(message) => formatter.write_str(message),
            Self::MissingLeaseBinding => formatter
                .write_str("this MCP server has no lease binding; restart it with `--lease <id>`"),
            Self::MissingTokenFileBinding => formatter.write_str(
                "this MCP server has no token-file binding; restart it with `--token-file <path>`",
            ),
            Self::Daemon(error) => write!(formatter, "{error}"),
            Self::ResultSerialization(error) => {
                write!(
                    formatter,
                    "failed to serialize the Bowline tool result: {error}"
                )
            }
        }
    }
}

#[derive(Debug)]
enum DaemonInvokeError {
    Rpc(bowline_daemon_rpc::ClientError),
}

impl fmt::Display for DaemonInvokeError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Rpc(error) => write!(formatter, "Bowline daemon RPC failed: {error}"),
        }
    }
}

pub(super) fn serve_stdio(socket: &Path, args: crate::cli::McpArgs) -> std::process::ExitCode {
    let stdin = std::io::stdin();
    let stdout = std::io::stdout();
    let binding = McpServerBinding::from_args(args);
    let result = serve(stdin.lock(), stdout.lock(), socket, &binding);
    if let Err(error) = &result {
        eprintln!("bowline mcp: {error}");
    }
    exit_code_for_serve_result(&result).into()
}

fn exit_code_for_serve_result(result: &std::io::Result<()>) -> CommandExitCode {
    match result {
        Ok(()) => CommandExitCode::Success,
        Err(_) => CommandExitCode::RetryableRuntimeError,
    }
}

fn serve<R: BufRead, W: Write>(
    reader: R,
    mut writer: W,
    socket: &Path,
    binding: &McpServerBinding,
) -> std::io::Result<()> {
    for line in reader.lines() {
        let line = line?;
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        let response = match serde_json::from_str::<Value>(line) {
            Ok(message) => handle_message(message, socket, binding),
            Err(error) => Some(error_response(Value::Null, -32700, &error.to_string())),
        };
        if let Some(response) = response {
            serde_json::to_writer(&mut writer, &response)?;
            writeln!(writer)?;
            writer.flush()?;
        }
    }
    Ok(())
}

fn handle_message(message: Value, socket: &Path, binding: &McpServerBinding) -> Option<Value> {
    let id = message.get("id").cloned();
    let method = message.get("method").and_then(Value::as_str).unwrap_or("");
    let id = id?;
    Some(match method {
        "initialize" => json!({
            "jsonrpc": "2.0",
            "id": id,
            "result": {
                "protocolVersion": negotiated_protocol_version(&message),
                "capabilities": {"tools": {"listChanged": false}},
                "serverInfo": {"name": "bowline", "version": crate::CLI_VERSION}
            }
        }),
        "ping" => json!({"jsonrpc": "2.0", "id": id, "result": {}}),
        "tools/list" => json!({
            "jsonrpc": "2.0",
            "id": id,
            "result": {"tools": tools()}
        }),
        "tools/call" => handle_tool_call(id, &message, socket, binding),
        "resources/list" => json!({
            "jsonrpc": "2.0",
            "id": id,
            "result": {"resources": []}
        }),
        "prompts/list" => json!({
            "jsonrpc": "2.0",
            "id": id,
            "result": {"prompts": []}
        }),
        _ => error_response(id, -32601, "method not found"),
    })
}

fn negotiated_protocol_version(message: &Value) -> &'static str {
    let Some(requested) = message
        .pointer("/params/protocolVersion")
        .and_then(Value::as_str)
    else {
        return MCP_PROTOCOL_VERSION;
    };
    MCP_SUPPORTED_PROTOCOL_VERSIONS
        .iter()
        .copied()
        .find(|supported| *supported == requested)
        .unwrap_or(MCP_PROTOCOL_VERSION)
}

fn handle_tool_call(
    id: Value,
    message: &Value,
    socket: &Path,
    binding: &McpServerBinding,
) -> Value {
    let name = message
        .pointer("/params/name")
        .and_then(Value::as_str)
        .unwrap_or("");
    let Some(tool) = tool_from_name(name) else {
        return error_response(id, -32602, "unknown bowline agent tool");
    };
    let Some(lease_id) = binding.lease_id.clone() else {
        return mcp_tool_error(id, &McpToolCallError::MissingLeaseBinding);
    };
    let Some(token_file) = binding.token_file.clone() else {
        return mcp_tool_error(id, &McpToolCallError::MissingTokenFileBinding);
    };
    let tool_arguments = match message.pointer("/params/arguments") {
        Some(Value::Object(arguments)) => arguments.clone(),
        None | Some(Value::Null) => serde_json::Map::new(),
        Some(_) => {
            return mcp_tool_error(
                id,
                &McpToolCallError::InvalidArguments("tool arguments must be a JSON object"),
            );
        }
    };
    let request = AgentToolInvokeRequest {
        message_type: "agent.tool.invoke".to_string(),
        protocol_version: CONTRACT_VERSION,
        request_id: request_id_from_mcp_id(&id),
        lease_id,
        tool,
        authority: AgentToolAuthority {
            transport: AgentToolTransport::McpAdapter,
            peer_credential_checked: false,
            nonce_presented: true,
            mcp_token_file: Some(token_file),
        },
        arguments: tool_arguments,
    };
    match invoke_daemon_tool(socket, &request) {
        Ok(result) => mcp_tool_result(id, result),
        Err(error) => mcp_tool_error(id, &McpToolCallError::Daemon(error)),
    }
}

fn error_response(id: Value, code: i64, message: &str) -> Value {
    json!({
        "jsonrpc": "2.0",
        "id": id,
        "error": {"code": code, "message": message}
    })
}

fn mcp_tool_error(id: Value, error: &McpToolCallError) -> Value {
    let message = error.to_string();
    let restart_instruction = error.restart_instruction();
    json!({
        "jsonrpc": "2.0",
        "id": id,
        "result": {
            "content": [{"type": "text", "text": message}],
            "structuredContent": {
                "error": {
                    "code": error.code(),
                    "message": message,
                    "restartInstruction": restart_instruction
                }
            },
            "isError": true
        }
    })
}

fn mcp_tool_result(id: Value, result: AgentToolResult) -> Value {
    let is_error = result.outcome == AgentToolResultOutcome::Denied;
    let text = mcp_tool_result_text(&result);
    let structured_content = match serde_json::to_value(&result) {
        Ok(content) => content,
        Err(error) => {
            return mcp_tool_error(id, &McpToolCallError::ResultSerialization(error));
        }
    };
    json!({
        "jsonrpc": "2.0",
        "id": id,
        "result": {
            "content": [{"type": "text", "text": text}],
            "structuredContent": structured_content,
            "isError": is_error
        }
    })
}

fn mcp_tool_result_text(result: &AgentToolResult) -> String {
    let mut text = result.summary.clone();
    if let Some(denial) = &result.denial {
        text.push('\n');
        text.push_str(&format!("denial: {}", denial.code));
    }
    if let Some(payload) = &result.payload {
        text.push('\n');
        text.push_str(
            &serde_json::to_string_pretty(&payload)
                .unwrap_or_else(|_| "{\"payload\":\"unserializable\"}".to_string()),
        );
    }
    text
}

fn invoke_daemon_tool(
    socket: &Path,
    request: &AgentToolInvokeRequest,
) -> Result<AgentToolResult, DaemonInvokeError> {
    let client =
        DaemonClient::connect(socket, ClientOptions::new("mcp", env!("CARGO_PKG_VERSION")))
            .map_err(DaemonInvokeError::Rpc)?;
    client
        .call("agent.tool.invoke", request, Some(Duration::from_secs(30)))
        .map_err(DaemonInvokeError::Rpc)
}

fn request_id_from_mcp_id(id: &Value) -> String {
    let raw = match id {
        Value::String(value) => value.clone(),
        _ => serde_json::to_string(id).unwrap_or_else(|_| "unknown".to_string()),
    };
    let escaped = raw
        .chars()
        .map(|character| {
            if character.is_ascii_alphanumeric() || character == '_' || character == '-' {
                character
            } else {
                '_'
            }
        })
        .collect::<String>();
    format!("mcp_{escaped}")
}

#[cfg(test)]
mod tests {
    use super::tool_specs::{TOOL_TABLE, tool_name};
    use super::*;
    use std::{
        fs,
        os::unix::net::UnixListener,
        thread,
        time::{SystemTime, UNIX_EPOCH},
    };

    use bowline_core::{
        commands::{
            AgentLeaseBase, AgentMcpGrant, AgentToolDenial, AgentToolInvokeRequest, AgentToolName,
        },
        ids::{DeviceId, ProjectId, WorkspaceId},
        status::RepairCommand,
    };
    use bowline_local::{
        agents::{
            AgentLeaseCreateOptions, AgentMcpTokenIssueOptions, create_agent_lease,
            invoke_agent_tool_from_daemon, issue_agent_mcp_token,
        },
        metadata::MetadataStore,
        workspace::TempWorkspace,
    };
    use serde_json::json;

    fn run(input: &str) -> Vec<Value> {
        run_with_binding(
            input,
            McpServerBinding {
                lease_id: None,
                token_file: None,
            },
        )
    }

    fn run_with_binding(input: &str, binding: McpServerBinding) -> Vec<Value> {
        let mut output = Vec::new();
        serve(
            input.as_bytes(),
            &mut output,
            Path::new("/tmp/bowline-missing-test.sock"),
            &binding,
        )
        .expect("mcp server runs");
        String::from_utf8(output)
            .expect("utf8")
            .lines()
            .map(|line| serde_json::from_str(line).expect("json response"))
            .collect()
    }

    #[test]
    fn stdio_io_failures_use_the_retryable_runtime_exit_code() {
        let success = Ok(());
        let broken_pipe = Err(std::io::Error::new(
            std::io::ErrorKind::BrokenPipe,
            "test broken pipe",
        ));

        assert_eq!(
            exit_code_for_serve_result(&success),
            CommandExitCode::Success
        );
        assert_eq!(
            exit_code_for_serve_result(&broken_pipe),
            CommandExitCode::RetryableRuntimeError
        );
        assert_eq!(
            exit_code_for_serve_result(&broken_pipe).code(),
            3,
            "MCP stdio failures must use the advertised retryable runtime code"
        );
    }

    #[test]
    fn mcp_tool_wire_names_match_agent_tool_serde_contract() {
        let expected = [
            "workspace_status",
            "list_capabilities",
            "resolve_path",
            "list_overlay_changes",
        ];

        assert_eq!(
            TOOL_TABLE
                .iter()
                .map(|spec| tool_name(spec.tool))
                .collect::<Vec<_>>(),
            expected
        );
        for (spec, expected_name) in TOOL_TABLE.iter().zip(expected) {
            assert_eq!(tool_name(spec.tool), expected_name);
        }
    }

    #[test]
    fn mcp_tool_table_round_trips_wire_names() {
        for spec in TOOL_TABLE {
            let name = tool_name(spec.tool);
            assert_eq!(tool_from_name(name), Some(spec.tool));
        }
    }

    #[test]
    fn mcp_denied_result_carries_structured_safe_next_actions() {
        let result = mcp_tool_result(
            json!("call"),
            AgentToolResult {
                request_id: "req_denied".to_string(),
                lease_id: LeaseId::new("lease_test"),
                tool: AgentToolName::ResolvePath,
                outcome: AgentToolResultOutcome::Denied,
                event_id: None,
                receipt_id: None,
                denial: Some(AgentToolDenial {
                    code: "write-scope-denied".to_string(),
                    safe_next_actions: vec![RepairCommand::inspect(
                        "Ask for write scope".to_string(),
                        Some("bowline actions".to_string()),
                    )],
                }),
                summary: "write denied".to_string(),
                payload: None,
            },
        );

        assert_eq!(result["result"]["isError"], true);
        assert_eq!(
            result["result"]["structuredContent"]["denial"]["safeNextActions"][0]["label"],
            "Ask for write scope"
        );
        let text = result["result"]["content"][0]["text"]
            .as_str()
            .expect("text content");
        assert!(text.contains("write denied"));
        assert!(text.contains("denial: write-scope-denied"));
    }

    #[test]
    fn mcp_degraded_result_is_structured_but_not_error() {
        let result = mcp_tool_result(
            json!("call"),
            AgentToolResult {
                request_id: "req_degraded".to_string(),
                lease_id: LeaseId::new("lease_test"),
                tool: AgentToolName::ListOverlayChanges,
                outcome: AgentToolResultOutcome::Degraded,
                event_id: None,
                receipt_id: None,
                denial: None,
                summary: "search degraded".to_string(),
                payload: None,
            },
        );

        assert_eq!(result["result"]["isError"], false);
        assert_eq!(result["result"]["structuredContent"]["outcome"], "degraded");
        assert!(
            result["result"]["structuredContent"]
                .get("degraded")
                .is_none()
        );
    }

    #[test]
    fn mcp_every_schema_property_has_a_description() {
        for tool in tools() {
            let name = tool["name"].as_str().expect("tool name");
            let properties = tool["inputSchema"]["properties"]
                .as_object()
                .expect("properties");
            for (property_name, property) in properties {
                assert!(
                    property["description"]
                        .as_str()
                        .is_some_and(|description| !description.trim().is_empty()),
                    "{name}.{property_name} is missing a description"
                );
            }
        }
    }

    #[test]
    fn mcp_schemas_exclude_server_binding_fields() {
        for tool in tools() {
            let properties = tool["inputSchema"]["properties"]
                .as_object()
                .expect("properties");
            let required = tool["inputSchema"]["required"]
                .as_array()
                .expect("required");
            assert!(!properties.contains_key("leaseId"));
            assert!(!properties.contains_key("mcpTokenFile"));
            assert!(!required.iter().any(|field| field == "leaseId"));
            assert!(!required.iter().any(|field| field == "mcpTokenFile"));
        }
    }

    #[test]
    fn explicit_server_binding_wins_over_environment_fallbacks() {
        let binding = McpServerBinding::from_sources(
            crate::cli::McpArgs {
                lease_id: Some("lease_flag".to_string()),
                token_file: Some("/tmp/token-flag".to_string()),
            },
            Some("lease_env".to_string()),
            Some("/tmp/token-env".to_string()),
        );

        assert_eq!(
            binding.lease_id.as_ref().map(LeaseId::as_str),
            Some("lease_flag")
        );
        assert_eq!(binding.token_file.as_deref(), Some("/tmp/token-flag"));
    }

    #[test]
    fn server_binding_uses_environment_fallbacks() {
        let binding = McpServerBinding::from_sources(
            crate::cli::McpArgs {
                lease_id: None,
                token_file: None,
            },
            Some("lease_env".to_string()),
            Some("/tmp/token-env".to_string()),
        );

        assert_eq!(
            binding.lease_id.as_ref().map(LeaseId::as_str),
            Some("lease_env")
        );
        assert_eq!(binding.token_file.as_deref(), Some("/tmp/token-env"));
    }

    #[test]
    fn initialize_and_list_tools() {
        let responses = run(
            r#"{"jsonrpc":"2.0","id":1,"method":"initialize","params":{}}
{"jsonrpc":"2.0","id":2,"method":"tools/list","params":{}}
"#,
        );

        assert_eq!(responses[0]["result"]["serverInfo"]["name"], "bowline");
        let tools = responses[1]["result"]["tools"].as_array().expect("tools");
        assert_eq!(tools.len(), 4);
        assert_eq!(tools[0]["name"], "workspace_status");
        assert_eq!(tools[3]["name"], "list_overlay_changes");
        // The slim bridge is read-only: no surviving tool requires a request nonce.
        for tool in tools {
            assert!(
                !tool["inputSchema"]["required"]
                    .as_array()
                    .expect("required")
                    .iter()
                    .any(|value| value.as_str() == Some("mcpRequestNonce"))
            );
        }
    }

    #[test]
    fn initialize_echoes_supported_client_protocol_version() {
        let responses = run(
            r#"{"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":"2025-06-18"}}
{"jsonrpc":"2.0","id":2,"method":"initialize","params":{"protocolVersion":"2024-11-05"}}
"#,
        );

        assert_eq!(responses[0]["result"]["protocolVersion"], "2025-06-18");
        assert_eq!(responses[1]["result"]["protocolVersion"], "2024-11-05");
    }

    #[test]
    fn tool_calls_without_a_server_lease_binding_return_restart_guidance() {
        let responses = run(
            r#"{"jsonrpc":"2.0","id":"call","method":"tools/call","params":{"name":"workspace_status","arguments":{}}}
"#,
        );

        assert_eq!(responses[0]["id"], "call");
        assert_eq!(responses[0]["result"]["isError"], true);
        assert_eq!(
            responses[0]["result"]["structuredContent"]["error"]["code"],
            "missingLeaseBinding"
        );
        assert!(
            responses[0]["result"]["structuredContent"]["error"]["restartInstruction"]
                .as_str()
                .expect("restart instruction")
                .contains("--lease <id>")
        );
    }

    #[test]
    fn tool_call_runs_through_stdio_forwarder_and_daemon_path() {
        let (temp, db_path) = seeded_store("mcp-stdio-tool-call");
        let project_path = temp.root().join("Code/apps/web");
        let lease = create_agent_lease(AgentLeaseCreateOptions {
            db_path: Some(db_path.clone()),
            project_path: project_path.display().to_string(),
            task: "mcp stdio success".to_string(),
            base: AgentLeaseBase::LatestWorkspace,
            work_view: true,
            force_stale: false,
            device_id: DeviceId::new("device-test"),
            generated_at: now(),
        })
        .expect("lease created")
        .lease;
        let token = issue_agent_mcp_token(AgentMcpTokenIssueOptions {
            db_path: Some(db_path.clone()),
            lease_id: lease.id.clone(),
            grants: vec![AgentMcpGrant::Read],
            generated_at: now(),
        })
        .expect("mcp token");
        let socket = unique_socket_path();
        let listener = UnixListener::bind(&socket).expect("bind daemon socket");
        let server = thread::spawn(move || -> Result<(), String> {
            let (mut stream, _) = listener.accept().map_err(|error| error.to_string())?;
            use bowline_core::wire::generated::{
                DaemonClientHello, DaemonRpcRequest, DaemonRpcResponse, DaemonServerHello,
                MACHINE_CONTRACT_VERSION,
            };
            let codec = bowline_daemon_rpc::FrameCodec::default();
            codec
                .read_magic(&mut stream)
                .map_err(|error| error.to_string())?;
            let _: DaemonClientHello =
                codec.read(&mut stream).map_err(|error| error.to_string())?;
            codec
                .write(
                    &mut stream,
                    &DaemonServerHello {
                        protocol_version: bowline_daemon_rpc::DAEMON_RPC_PROTOCOL_VERSION,
                        contract_version: MACHINE_CONTRACT_VERSION,
                        schema_hash: bowline_core::wire::generated::WIRE_SCHEMA_HASH.to_string(),
                        daemon_version: "test".to_string(),
                        capabilities: vec!["agent.tool.invoke".to_string()],
                        instance_id: "fake-daemon".to_string(),
                    },
                )
                .map_err(|error| error.to_string())?;
            let rpc_request: DaemonRpcRequest =
                codec.read(&mut stream).map_err(|error| error.to_string())?;
            if rpc_request.method != "agent.tool.invoke" {
                return Err(format!("unexpected daemon method: {}", rpc_request.method));
            }
            let request: AgentToolInvokeRequest =
                serde_json::from_value(rpc_request.params).map_err(|error| error.to_string())?;
            let result = invoke_agent_tool_from_daemon(Some(db_path), request, true, now())
                .map_err(|error| error.to_string())?;
            codec
                .write(
                    &mut stream,
                    &DaemonRpcResponse {
                        request_id: rpc_request.request_id,
                        result: Some(
                            serde_json::to_value(result).map_err(|error| error.to_string())?,
                        ),
                        error: None,
                    },
                )
                .map_err(|error| error.to_string())?;
            Ok(())
        });

        let input = r#"{"jsonrpc":"2.0","id":"call","method":"tools/call","params":{"name":"list_capabilities","arguments":{}}}
"#;
        let binding = McpServerBinding {
            lease_id: Some(lease.id.clone()),
            token_file: Some(token.token_file.clone()),
        };
        let mut output = Vec::new();
        serve(input.as_bytes(), &mut output, &socket, &binding).expect("mcp server runs");

        let response: Value = serde_json::from_slice(&output).expect("json response");
        assert_eq!(response["id"], "call");
        assert_eq!(response["result"]["isError"], false);
        assert_eq!(
            response["result"]["structuredContent"]["leaseId"],
            lease.id.as_str()
        );
        let text = response["result"]["content"][0]["text"]
            .as_str()
            .expect("text result");
        assert!(text.contains("capabilities listed"));
        assert!(text.contains("\"capabilities\""));
        assert!(text.contains("workspace_status"));

        server
            .join()
            .expect("daemon thread")
            .expect("daemon request");
        let _ = fs::remove_file(&socket);
        assert_eq!(
            MetadataStore::open(temp.root().join(".state/local.sqlite3"))
                .expect("store")
                .agent_mcp_token_by_file(&token.token_file)
                .expect("token lookup")
                .expect("stored token")
                .last_used_at
                .as_deref(),
            Some(now().as_str())
        );
    }

    fn seeded_store(name: &str) -> (TempWorkspace, std::path::PathBuf) {
        let temp = TempWorkspace::new(name).expect("temp workspace");
        let code_root = temp.root().join("Code");
        fs::create_dir_all(code_root.join("apps/web")).expect("project dir");
        let db_path = temp.root().join(".state/local.sqlite3");
        let mut store = MetadataStore::open(&db_path).expect("metadata");
        let workspace_id = WorkspaceId::new("ws_code");
        let project_id = ProjectId::new("proj_web");
        store
            .insert_workspace(&workspace_id, "User Code", "2026-06-25T00:00:00Z")
            .expect("workspace");
        store
            .insert_root(
                "root_code",
                &workspace_id,
                &code_root.display().to_string(),
                "2026-06-25T00:00:00Z",
            )
            .expect("root");
        store
            .insert_project(
                &project_id,
                &workspace_id,
                "root_code",
                "apps/web",
                "2026-06-25T00:00:00Z",
            )
            .expect("project");
        bowline_testkit::persist_project_snapshot_fixture(
            &mut store,
            &workspace_id,
            &project_id,
            &code_root,
            "apps/web",
            db_path.parent().expect("metadata state root"),
            "2026-06-25T00:00:00Z",
        );
        (temp, db_path)
    }

    fn now() -> String {
        "2026-06-25T12:00:00Z".to_string()
    }

    fn unique_socket_path() -> std::path::PathBuf {
        let stamp = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("system time")
            .as_nanos();
        std::env::temp_dir().join(format!("bowline-mcp-{stamp}-{}.sock", std::process::id()))
    }
}

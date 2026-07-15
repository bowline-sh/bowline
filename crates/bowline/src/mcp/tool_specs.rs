use bowline_core::commands::AgentToolName;
use serde_json::{Value, json};

#[derive(Clone, Copy)]
pub(super) struct ToolSpec {
    pub(super) tool: AgentToolName,
    description: &'static str,
    schema: SchemaSpec,
}

#[derive(Clone, Copy)]
enum SchemaSpec {
    Object {
        properties: &'static [PropertySpec],
        required: &'static [&'static str],
    },
}

impl SchemaSpec {
    fn input_schema(self) -> Value {
        match self {
            Self::Object {
                properties,
                required,
            } => object_schema(properties, required),
        }
    }
}

#[derive(Clone, Copy)]
struct PropertySpec {
    name: &'static str,
    kind: &'static str,
    description: &'static str,
}

pub(super) const TOOL_TABLE: &[ToolSpec] = &[
    ToolSpec {
        tool: AgentToolName::WorkspaceStatus,
        description: "Return workspace status, host readiness, lease, sync, and trust state. Use this first when an orchestrator needs the current Bowline state.",
        schema: SchemaSpec::Object {
            properties: &[],
            required: &[],
        },
    },
    ToolSpec {
        tool: AgentToolName::ListCapabilities,
        description: "Return the lease-scoped workspace capabilities.",
        schema: SchemaSpec::Object {
            properties: &[],
            required: &[],
        },
    },
    ToolSpec {
        tool: AgentToolName::ResolvePath,
        description: "Resolve a workspace-relative path to its absolute location under the lease read scope.",
        schema: SchemaSpec::Object {
            properties: &[PropertySpec {
                name: "path",
                kind: "string",
                description: "Workspace-relative path to resolve under the lease read scope.",
            }],
            required: &["path"],
        },
    },
    ToolSpec {
        tool: AgentToolName::ListOverlayChanges,
        description: "Return reviewable work-view changes for handoff.",
        schema: SchemaSpec::Object {
            properties: &[],
            required: &[],
        },
    },
];

pub(super) fn tools() -> Vec<Value> {
    TOOL_TABLE
        .iter()
        .map(|spec| {
            json!({
                "name": tool_name(spec.tool),
                "description": spec.description,
                "inputSchema": spec.schema.input_schema()
            })
        })
        .collect()
}

pub(super) fn tool_name(tool: AgentToolName) -> &'static str {
    match tool {
        AgentToolName::WorkspaceStatus => "workspace_status",
        AgentToolName::ListCapabilities => "list_capabilities",
        AgentToolName::ResolvePath => "resolve_path",
        AgentToolName::ListOverlayChanges => "list_overlay_changes",
    }
}

pub(super) fn tool_from_name(name: &str) -> Option<AgentToolName> {
    TOOL_TABLE
        .iter()
        .find(|spec| tool_name(spec.tool) == name)
        .map(|spec| spec.tool)
}

fn object_schema(properties: &[PropertySpec], required: &[&str]) -> Value {
    let mut schema = object_schema_base(properties);
    schema["required"] = json!(required);
    schema
}

fn object_schema_base(properties: &[PropertySpec]) -> Value {
    let properties = properties
        .iter()
        .map(|property| {
            (
                property.name.to_string(),
                json!({"type": property.kind, "description": property.description}),
            )
        })
        .collect::<serde_json::Map<_, _>>();
    json!({
        "type": "object",
        "properties": properties,
        "required": [],
        "additionalProperties": true
    })
}

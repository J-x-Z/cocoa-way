use std::io::{BufRead, Write};
use std::path::{Path, PathBuf};

use serde_json::{Value, json};

#[path = "../control_client.rs"]
mod control_client;
#[allow(dead_code)]
#[path = "../control_protocol.rs"]
mod control_protocol;

use control_protocol::ControlRequest;

const LATEST_PROTOCOL: &str = "2025-11-25";
const SUPPORTED_PROTOCOLS: &[&str] = &["2025-11-25", "2025-06-18", "2025-03-26", "2024-11-05"];

fn main() {
    let socket = parse_socket_argument().unwrap_or_else(|error| fail(&error));
    let stdin = std::io::stdin();
    let mut stdout = std::io::stdout().lock();
    let mut initialized = false;

    for line in stdin.lock().lines() {
        let response = match line {
            Ok(line) if line.trim().is_empty() => None,
            Ok(line) => match serde_json::from_str::<Value>(&line) {
                Ok(message) => handle_message(&message, &socket, &mut initialized),
                Err(error) => Some(json_rpc_error(
                    Value::Null,
                    -32700,
                    format!("Parse error: {error}"),
                )),
            },
            Err(error) => {
                eprintln!("cocoa-way-mcp: failed to read stdin: {error}");
                break;
            }
        };
        if let Some(response) = response {
            match serde_json::to_string(&response) {
                Ok(encoded) => {
                    if writeln!(stdout, "{encoded}").is_err() || stdout.flush().is_err() {
                        break;
                    }
                }
                Err(error) => {
                    eprintln!("cocoa-way-mcp: failed to encode response: {error}");
                    break;
                }
            }
        }
    }
}

fn parse_socket_argument() -> Result<PathBuf, String> {
    let arguments = std::env::args().skip(1).collect::<Vec<_>>();
    if arguments.is_empty() {
        return Ok(control_client::default_socket_path());
    }
    if arguments.len() == 2 && arguments[0] == "--socket" {
        return Ok(PathBuf::from(&arguments[1]));
    }
    Err("usage: cocoa-way-mcp [--socket PATH]".into())
}

fn handle_message(message: &Value, socket: &Path, initialized: &mut bool) -> Option<Value> {
    let Some(object) = message.as_object() else {
        return Some(json_rpc_error(Value::Null, -32600, "Invalid Request"));
    };
    if object.get("jsonrpc").and_then(Value::as_str) != Some("2.0") {
        return Some(json_rpc_error(
            object.get("id").cloned().unwrap_or(Value::Null),
            -32600,
            "Invalid Request",
        ));
    }
    let method = object.get("method").and_then(Value::as_str)?;
    let id = object.get("id").cloned();
    let params = object.get("params").cloned().unwrap_or_else(|| json!({}));

    if id.is_none() {
        if method == "notifications/initialized" {
            *initialized = true;
        }
        return None;
    }
    let id = id.unwrap_or(Value::Null);
    if id.is_null() {
        return Some(json_rpc_error(id, -32600, "Request id must not be null"));
    }

    match method {
        "initialize" => Some(json_rpc_success(id, initialize_result(&params))),
        "ping" => Some(json_rpc_success(id, json!({}))),
        "tools/list" if *initialized => Some(json_rpc_success(id, json!({ "tools": tools() }))),
        "tools/call" if *initialized => match call_tool(&params, socket) {
            Ok(result) => Some(json_rpc_success(id, result)),
            Err(error) => Some(json_rpc_error(id, -32602, error)),
        },
        "tools/list" | "tools/call" => {
            Some(json_rpc_error(id, -32002, "Server is not initialized"))
        }
        _ => Some(json_rpc_error(id, -32601, "Method not found")),
    }
}

fn initialize_result(params: &Value) -> Value {
    let requested = params
        .get("protocolVersion")
        .and_then(Value::as_str)
        .unwrap_or(LATEST_PROTOCOL);
    let protocol = if SUPPORTED_PROTOCOLS.contains(&requested) {
        requested
    } else {
        LATEST_PROTOCOL
    };
    json!({
        "protocolVersion": protocol,
        "capabilities": { "tools": {} },
        "serverInfo": {
            "name": "cocoa-way-mcp",
            "title": "Cocoa-Way Local Control",
            "version": env!("CARGO_PKG_VERSION"),
            "description": "Read-only local diagnostics and guided setup for Cocoa-Way applications, connections, and container resources."
        },
        "instructions": "This server is local and read-only. Use its onboarding and template tools to prepare image, application, and connection settings. Review generated commands in Cocoa-Way; launch, stop, and delete remain explicit user actions."
    })
}

fn tools() -> Vec<Value> {
    vec![
        read_only_tool(
            "cocoa_way_onboarding",
            "Cocoa-Way onboarding",
            "Inspect this Mac and return the next safe setup steps for Apple Container applications or classic Waypipe connections.",
            json!({
                "type": "object",
                "properties": {
                    "goal": {
                        "type": "string",
                        "enum": ["overview", "apple-container", "remote-ssh", "local-socket"],
                        "default": "overview"
                    }
                },
                "additionalProperties": false
            }),
        ),
        read_only_tool(
            "cocoa_way_image_sources",
            "Cocoa-Way image sources",
            "List local images and explain trusted OCI pull, OCI archive import, and bundled GUI-ready image build paths.",
            empty_schema(),
        ),
        read_only_tool(
            "cocoa_way_application_template",
            "Cocoa-Way application template",
            "Generate a reviewable Apple Container application profile and GUI setup steps without saving or launching it.",
            json!({
                "type": "object",
                "properties": {
                    "name": { "type": "string", "minLength": 1, "default": "Linux Application" },
                    "image": { "type": "string", "minLength": 1 },
                    "command": { "type": "string", "minLength": 1 },
                    "presentation": { "type": "string", "enum": ["desktop", "rootless"], "default": "rootless" },
                    "display": { "type": "string", "default": "auto" },
                    "audio": { "type": "boolean", "default": true }
                },
                "required": ["image", "command"],
                "additionalProperties": false
            }),
        ),
        read_only_tool(
            "cocoa_way_connection_template",
            "Cocoa-Way connection template",
            "Generate a saved SSH or local Unix-socket connection block and the equivalent run_waypipe.sh command without connecting.",
            json!({
                "type": "object",
                "properties": {
                    "name": { "type": "string", "minLength": 1, "default": "Linux Machine" },
                    "type": { "type": "string", "enum": ["ssh", "local"], "default": "ssh" },
                    "host": { "type": "string", "description": "Required for SSH." },
                    "user": { "type": "string" },
                    "port": { "type": "integer", "minimum": 1, "maximum": 65535, "default": 22 },
                    "identity": { "type": "string" },
                    "socket": { "type": "string", "description": "Required for a local Unix-socket connection." },
                    "command": { "type": "string", "minLength": 1, "default": "foot" },
                    "display": { "type": "string", "default": "default" },
                    "compression": { "type": "string", "enum": ["none", "lz4", "zstd"], "default": "zstd" }
                },
                "additionalProperties": false
            }),
        ),
        read_only_tool(
            "cocoa_way_status",
            "Cocoa-Way status",
            "Read compositor, transport, performance, and recent activity status.",
            empty_schema(),
        ),
        read_only_tool(
            "cocoa_way_applications",
            "Cocoa-Way applications",
            "List saved application profiles and their active instance/display state.",
            empty_schema(),
        ),
        read_only_tool(
            "cocoa_way_sessions",
            "Cocoa-Way sessions (compatibility)",
            "Compatibility alias for cocoa_way_applications.",
            empty_schema(),
        ),
        read_only_tool(
            "cocoa_way_running",
            "Running Cocoa-Way instances",
            "List active application instances and their runtime processes and display attachments.",
            empty_schema(),
        ),
        read_only_tool(
            "cocoa_way_displays",
            "Cocoa-Way displays",
            "List the default and active dedicated display assignments.",
            empty_schema(),
        ),
        read_only_tool(
            "cocoa_way_images",
            "Cocoa-Way images",
            "List local Apple Container and Docker-compatible images.",
            empty_schema(),
        ),
        read_only_tool(
            "cocoa_way_volumes",
            "Cocoa-Way volumes",
            "List Apple Container and Docker-compatible volumes.",
            empty_schema(),
        ),
        read_only_tool(
            "cocoa_way_runtimes",
            "Cocoa-Way runtimes",
            "Inspect Apple Container, Docker-compatible contexts, and the optional OrbStack provider.",
            empty_schema(),
        ),
        read_only_tool(
            "cocoa_way_tasks",
            "Cocoa-Way tasks",
            "List recent launch, stop, image, volume, and runtime operation tasks.",
            diagnostics_schema(),
        ),
        read_only_tool(
            "cocoa_way_environment",
            "Cocoa-Way environment",
            "Inspect host architecture and local Apple Container, waypipe, Docker, and OrbStack command availability and versions.",
            empty_schema(),
        ),
        read_only_tool(
            "cocoa_way_features",
            "Cocoa-Way feature matrix",
            "Read supported presentation, transport, runtime-control, and automation capabilities with known limitations.",
            empty_schema(),
        ),
        read_only_tool(
            "cocoa_way_diagnostics",
            "Cocoa-Way diagnostics bundle",
            "Collect a redacted, read-only diagnostics snapshot. Optionally include recent logs for one session.",
            diagnostics_schema(),
        ),
        read_only_tool(
            "cocoa_way_issue_draft",
            "Draft a Cocoa-Way issue",
            "Build a redacted Markdown issue draft from local diagnostics without writing a file or posting it anywhere.",
            json!({
                "type": "object",
                "properties": {
                    "summary": { "type": "string", "minLength": 1, "description": "Short factual problem summary." },
                    "session": { "type": "string", "description": "Optional exact session name or zero-based index." },
                    "steps": { "type": "array", "items": { "type": "string" }, "description": "Reproduction steps in order." },
                    "expected": { "type": "string" },
                    "actual": { "type": "string" },
                    "limit": { "type": "integer", "minimum": 1, "maximum": 1000, "default": 100 }
                },
                "required": ["summary"],
                "additionalProperties": false
            }),
        ),
        read_only_tool(
            "cocoa_way_logs",
            "Cocoa-Way session logs",
            "Read recent captured logs for one configured GUI session.",
            json!({
                "type": "object",
                "properties": {
                    "session": {
                        "type": "string",
                        "description": "Exact session name or zero-based session index."
                    },
                    "limit": {
                        "type": "integer",
                        "minimum": 1,
                        "maximum": 1000,
                        "default": 100
                    }
                },
                "required": ["session"],
                "additionalProperties": false
            }),
        ),
    ]
}

fn read_only_tool(name: &str, title: &str, description: &str, input_schema: Value) -> Value {
    json!({
        "name": name,
        "title": title,
        "description": description,
        "inputSchema": input_schema,
        "annotations": {
            "readOnlyHint": true,
            "destructiveHint": false,
            "idempotentHint": true,
            "openWorldHint": false
        }
    })
}

fn empty_schema() -> Value {
    json!({ "type": "object", "additionalProperties": false })
}

fn diagnostics_schema() -> Value {
    json!({
        "type": "object",
        "properties": {
            "session": { "type": "string", "description": "Optional exact session name or zero-based index." },
            "limit": { "type": "integer", "minimum": 1, "maximum": 1000, "default": 100 }
        },
        "additionalProperties": false
    })
}

fn call_tool(params: &Value, socket: &Path) -> Result<Value, String> {
    let name = params
        .get("name")
        .and_then(Value::as_str)
        .ok_or_else(|| "tools/call requires a tool name".to_string())?;
    let arguments = params
        .get("arguments")
        .cloned()
        .unwrap_or_else(|| json!({}));
    if name == "cocoa_way_issue_draft" {
        return issue_draft(&arguments, socket);
    }
    match name {
        "cocoa_way_onboarding" => return onboarding(&arguments, socket),
        "cocoa_way_image_sources" => return image_sources(socket),
        "cocoa_way_application_template" => return application_template(&arguments),
        "cocoa_way_connection_template" => return connection_template(&arguments),
        _ => {}
    }
    let (command, session, limit) = match name {
        "cocoa_way_status" => ("status", None, 10),
        "cocoa_way_applications" => ("applications", None, 100),
        "cocoa_way_sessions" => ("applications", None, 100),
        "cocoa_way_running" => ("running", None, 100),
        "cocoa_way_displays" => ("displays", None, 100),
        "cocoa_way_images" => ("images", None, 100),
        "cocoa_way_volumes" => ("volumes", None, 100),
        "cocoa_way_runtimes" => ("runtimes", None, 100),
        "cocoa_way_tasks" => ("tasks", None, tool_limit(&arguments)),
        "cocoa_way_environment" => ("environment", None, 100),
        "cocoa_way_features" => ("features", None, 100),
        "cocoa_way_diagnostics" => (
            "diagnostics",
            arguments
                .get("session")
                .and_then(Value::as_str)
                .map(str::to_string),
            tool_limit(&arguments),
        ),
        "cocoa_way_logs" => {
            let session = arguments
                .get("session")
                .and_then(Value::as_str)
                .map(str::to_string)
                .ok_or_else(|| "cocoa_way_logs requires a string session argument".to_string())?;
            let limit = tool_limit(&arguments);
            ("logs", Some(session), limit)
        }
        _ => return Err(format!("Unknown tool: {name}")),
    };

    let request = ControlRequest {
        command: command.into(),
        session,
        limit,
    };
    send_control_tool(socket, &request)
}

fn onboarding(arguments: &Value, socket: &Path) -> Result<Value, String> {
    let goal = arguments
        .get("goal")
        .and_then(Value::as_str)
        .unwrap_or("overview");
    let environment = control_data(socket, "environment")?;
    let features = control_data(socket, "features")?;
    let applications = control_data(socket, "applications")?;
    let next_tools = match goal {
        "apple-container" => vec![
            "Confirm Apple's separate Container runtime is installed. If it is missing, install the latest official release from https://github.com/apple/container/releases/latest, then start it from Container > Apple Container.",
            "Call cocoa_way_image_sources and choose a trusted GUI-ready image path.",
            "Call cocoa_way_application_template with the image and application command.",
            "Review CPU, memory, shared memory, platform, and Rosetta in Applications > More... > Resources & Platform...; changes apply to the next launch, not a running VM.",
            "After a runtime update, allow Apple Container to finish kernel/init-image downloads. A bootstrap timeout is not proof that the application image lacks GUI tools; retry Check after the runtime is ready.",
            "Review the generated profile in Applications > New Application, run Check, then launch explicitly.",
        ],
        "remote-ssh" | "local-socket" => vec![
            "Create a managed Display when the default display is already occupied.",
            "Call cocoa_way_connection_template and review the generated saved connection.",
            "Save it through Connections > Connect to Machine, or run the generated run_waypipe.sh command explicitly.",
        ],
        _ => vec![
            "Use Apple Container applications for managed local GUI workloads.",
            "Use saved Connections or run_waypipe.sh for SSH, OrbStack, Docker, and existing local sockets.",
            "Use cocoa_way_diagnostics before reporting a failure; destructive operations are never exposed by MCP.",
        ],
    };
    guided_result(json!({
        "goal": goal,
        "environment": environment,
        "features": features,
        "applications": applications,
        "next_steps": next_tools,
        "safety": "Generated setup is read-only. Review it in Cocoa-Way before launching anything."
    }))
}

fn image_sources(socket: &Path) -> Result<Value, String> {
    let local = control_data(socket, "images")?;
    guided_result(json!({
        "local_images": local,
        "sources": [
            {
                "kind": "oci-registry",
                "action": "Images > Pull from Source...",
                "example": "registry.fedoraproject.org/fedora:<release>",
                "note": "Pull only a registry and tag you trust. A base image is not GUI-ready until it contains Waypipe and the requested application."
            },
            {
                "kind": "oci-archive",
                "action": "Images > Import OCI Archive...",
                "note": "Import an OCI archive produced by another trusted runtime."
            },
            {
                "kind": "bundled-gui-image",
                "action": "Images > Build Example",
                "note": "Build Cocoa-Way's example image locally when a ready-to-test Waypipe, clipboard, audio, and GUI stack is needed."
            }
        ],
        "next_step": "After the image exists locally, call cocoa_way_application_template, create the profile in the GUI, and run Check before Launch."
    }))
}

fn application_template(arguments: &Value) -> Result<Value, String> {
    let image = required_text(arguments, "image")?;
    let command = required_text(arguments, "command")?;
    let name = optional_text(arguments, "name").unwrap_or("Linux Application");
    let presentation = optional_text(arguments, "presentation").unwrap_or("rootless");
    if !matches!(presentation, "desktop" | "rootless") {
        return Err("presentation must be desktop or rootless".into());
    }
    let display = optional_text(arguments, "display").unwrap_or("auto");
    let audio = arguments
        .get("audio")
        .and_then(Value::as_bool)
        .unwrap_or(true);
    let profile = if presentation == "desktop" {
        "desktop"
    } else {
        "single-app"
    };
    let toml = format!(
        "[[session]]\nname = {}\nruntime = \"container\"\nimage = {}\ndisplay = {}\npresentation = {}\nprofile = {}\ncommand = {}\naudio = {}\n",
        toml_string(name),
        toml_string(image),
        toml_string(display),
        toml_string(presentation),
        toml_string(profile),
        toml_string(command),
        audio
    );
    guided_result(json!({
        "profile": toml,
        "fields": {
            "name": name,
            "runtime": "container",
            "image": image,
            "command": command,
            "presentation": presentation,
            "display": display,
            "audio": audio
        },
        "steps": [
            "Confirm the image is local in Images; pull, load, or build it if needed.",
            "Open Applications > New Application and enter these fields, or append the reviewed block to container-sessions.toml.",
            "Review next-launch limits in Applications > More... > Resources & Platform... when needed.",
            "Run Check. Resolve missing Waypipe or application tools in the image; audio is optional. For kernel/init-image download failures, finish runtime setup and retry rather than rebuilding the application image.",
            "Launch explicitly from Cocoa-Way after Check reports Ready."
        ],
        "note": "Use desktop for nested compositors such as niri or Hyprland; use rootless for regular xdg-shell applications such as Foot or Firefox."
    }))
}

fn connection_template(arguments: &Value) -> Result<Value, String> {
    let connection_type = optional_text(arguments, "type").unwrap_or("ssh");
    if !matches!(connection_type, "ssh" | "local") {
        return Err("connection type must be ssh or local".into());
    }
    let name = optional_text(arguments, "name").unwrap_or("Linux Machine");
    let command = optional_text(arguments, "command").unwrap_or("foot");
    let display = optional_text(arguments, "display").unwrap_or("default");
    let compression = optional_text(arguments, "compression").unwrap_or("zstd");
    let mut lines = vec![
        "[[connection]]".to_string(),
        format!("name = {}", toml_string(name)),
        format!("type = {}", toml_string(connection_type)),
    ];
    let run_command = if connection_type == "ssh" {
        let host = required_text(arguments, "host")?;
        let user = optional_text(arguments, "user");
        let target = user
            .map(|user| format!("{user}@{host}"))
            .unwrap_or_else(|| host.to_string());
        let port = arguments.get("port").and_then(Value::as_u64).unwrap_or(22);
        if !(1..=65535).contains(&port) {
            return Err("port must be between 1 and 65535".into());
        }
        lines.push(format!("host = {}", toml_string(host)));
        if let Some(user) = user {
            lines.push(format!("user = {}", toml_string(user)));
        }
        lines.push(format!("port = {port}"));
        if let Some(identity) = optional_text(arguments, "identity") {
            lines.push(format!("identity = {}", toml_string(identity)));
        }
        format!(
            "./run_waypipe.sh --display {} ssh -p {} {} {}",
            shell_quote(display),
            port,
            shell_quote(&target),
            shell_quote(command)
        )
    } else {
        let socket = required_text(arguments, "socket")?;
        lines.push(format!("socket = {}", toml_string(socket)));
        format!(
            "./run_waypipe.sh --display {} --socket {} client",
            shell_quote(display),
            shell_quote(socket)
        )
    };
    lines.push(format!("app = {}", toml_string(command)));
    lines.push(format!("display = {}", toml_string(display)));
    lines.push(format!("compression = {}", toml_string(compression)));
    guided_result(json!({
        "connection": lines.join("\n") + "\n",
        "run_waypipe": run_command,
        "steps": [
            "Start Cocoa-Way and create the requested managed Display if it is not default.",
            "Review and save this entry in Connections > Connect to Machine; passwords are intentionally never stored.",
            "Connect explicitly from the Connections menu, or run the generated command from the repository."
        ]
    }))
}

fn control_data(socket: &Path, command: &str) -> Result<Value, String> {
    let response = control_client::send_request(
        socket,
        &ControlRequest {
            command: command.into(),
            session: None,
            limit: 100,
        },
    )
    .map_err(|error| format!("failed to query Cocoa-Way {command}: {error}"))?;
    if response.ok {
        Ok(response.data)
    } else {
        Err(response
            .error
            .unwrap_or_else(|| format!("Cocoa-Way {command} query failed")))
    }
}

fn guided_result(structured: Value) -> Result<Value, String> {
    let text = serde_json::to_string_pretty(&structured)
        .map_err(|error| format!("failed to format guided setup: {error}"))?;
    Ok(json!({
        "content": [{ "type": "text", "text": text }],
        "structuredContent": structured,
        "isError": false
    }))
}

fn required_text<'a>(arguments: &'a Value, key: &str) -> Result<&'a str, String> {
    optional_text(arguments, key).ok_or_else(|| format!("{key} is required"))
}

fn optional_text<'a>(arguments: &'a Value, key: &str) -> Option<&'a str> {
    arguments
        .get(key)
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|value| !value.is_empty())
}

fn toml_string(value: &str) -> String {
    serde_json::to_string(value).unwrap_or_else(|_| "\"\"".into())
}

fn shell_quote(value: &str) -> String {
    format!("'{}'", value.replace('\'', "'\\''"))
}

fn tool_limit(arguments: &Value) -> usize {
    arguments
        .get("limit")
        .and_then(Value::as_u64)
        .unwrap_or(100)
        .clamp(1, 1000) as usize
}

fn send_control_tool(socket: &Path, request: &ControlRequest) -> Result<Value, String> {
    match control_client::send_request(socket, request) {
        Ok(response) => {
            let structured = serde_json::to_value(&response)
                .map_err(|error| format!("failed to encode Cocoa-Way response: {error}"))?;
            let text = serde_json::to_string_pretty(&structured)
                .map_err(|error| format!("failed to format Cocoa-Way response: {error}"))?;
            Ok(json!({
                "content": [{ "type": "text", "text": text }],
                "structuredContent": structured,
                "isError": !response.ok
            }))
        }
        Err(error) => Ok(json!({
            "content": [{ "type": "text", "text": error }],
            "structuredContent": { "error": error },
            "isError": true
        })),
    }
}

fn issue_draft(arguments: &Value, socket: &Path) -> Result<Value, String> {
    let summary = arguments
        .get("summary")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .ok_or_else(|| "cocoa_way_issue_draft requires a non-empty summary".to_string())?;
    let request = ControlRequest {
        command: "diagnostics".into(),
        session: arguments
            .get("session")
            .and_then(Value::as_str)
            .map(str::to_string),
        limit: tool_limit(arguments),
    };
    let response = control_client::send_request(socket, &request)
        .map_err(|error| format!("failed to collect Cocoa-Way diagnostics: {error}"))?;
    if !response.ok {
        return send_control_tool(socket, &request);
    }

    let steps = arguments
        .get("steps")
        .and_then(Value::as_array)
        .map(|values| {
            values
                .iter()
                .filter_map(Value::as_str)
                .map(str::trim)
                .filter(|value| !value.is_empty())
                .enumerate()
                .map(|(index, value)| format!("{}. {value}", index + 1))
                .collect::<Vec<_>>()
                .join("\n")
        })
        .filter(|value| !value.is_empty())
        .unwrap_or_else(|| "1. Add the exact reproduction steps before submitting.".into());
    let expected = issue_field(arguments, "expected", "Describe the expected result.");
    let actual = issue_field(arguments, "actual", "Describe the actual result.");
    let diagnostics = serde_json::to_string_pretty(&response.data)
        .map_err(|error| format!("failed to format diagnostics: {error}"))?;
    let markdown = format!(
        "## Summary\n\n{summary}\n\n## Steps to reproduce\n\n{steps}\n\n## Expected behavior\n\n{expected}\n\n## Actual behavior\n\n{actual}\n\n## Diagnostics\n\n```json\n{diagnostics}\n```\n\n> Generated locally. Review for private data and remove irrelevant logs before submitting."
    );
    Ok(json!({
        "content": [{ "type": "text", "text": markdown }],
        "structuredContent": { "markdown": markdown, "diagnostics": response.data },
        "isError": false
    }))
}

fn issue_field(arguments: &Value, name: &str, fallback: &str) -> String {
    arguments
        .get(name)
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .unwrap_or(fallback)
        .to_string()
}

fn json_rpc_success(id: Value, result: Value) -> Value {
    json!({ "jsonrpc": "2.0", "id": id, "result": result })
}

fn json_rpc_error(id: Value, code: i64, message: impl Into<String>) -> Value {
    json!({
        "jsonrpc": "2.0",
        "id": id,
        "error": { "code": code, "message": message.into() }
    })
}

fn fail(message: &str) -> ! {
    eprintln!("cocoa-way-mcp: {message}");
    std::process::exit(2);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn initialize_negotiates_the_current_protocol() {
        let mut initialized = false;
        let response = handle_message(
            &json!({
                "jsonrpc": "2.0",
                "id": 1,
                "method": "initialize",
                "params": { "protocolVersion": "2025-11-25" }
            }),
            Path::new("/tmp/missing.sock"),
            &mut initialized,
        )
        .unwrap();
        assert_eq!(response["result"]["protocolVersion"], "2025-11-25");
        assert!(response["result"]["capabilities"]["tools"].is_object());
    }

    #[test]
    fn tool_catalog_is_read_only() {
        let catalog = tools();
        assert!(catalog.iter().all(|tool| {
            tool["annotations"]["readOnlyHint"] == true
                && tool["annotations"]["destructiveHint"] == false
        }));
        assert!(
            catalog.iter().all(|tool| {
                !matches!(tool["name"].as_str(), Some("launch" | "stop" | "delete"))
            })
        );
        for name in [
            "cocoa_way_onboarding",
            "cocoa_way_image_sources",
            "cocoa_way_application_template",
            "cocoa_way_connection_template",
            "cocoa_way_applications",
            "cocoa_way_running",
            "cocoa_way_displays",
            "cocoa_way_images",
            "cocoa_way_volumes",
            "cocoa_way_runtimes",
            "cocoa_way_tasks",
        ] {
            assert!(
                catalog.iter().any(|tool| tool["name"] == name),
                "missing {name}"
            );
        }
    }

    #[test]
    fn issue_draft_is_declared_read_only() {
        let tool = tools()
            .into_iter()
            .find(|tool| tool["name"] == "cocoa_way_issue_draft")
            .unwrap();
        assert_eq!(tool["annotations"]["readOnlyHint"], true);
        assert_eq!(tool["annotations"]["openWorldHint"], false);
        assert_eq!(tool["inputSchema"]["required"][0], "summary");
    }

    #[test]
    fn application_template_keeps_launch_explicit() {
        let result = application_template(&json!({
            "name": "Browser",
            "image": "localhost/example:latest",
            "command": "firefox",
            "presentation": "rootless"
        }))
        .unwrap();
        let structured = &result["structuredContent"];
        assert!(
            structured["profile"]
                .as_str()
                .unwrap()
                .contains("command = \"firefox\"")
        );
        assert_eq!(structured["fields"]["presentation"], "rootless");
        assert!(
            result["content"][0]["text"]
                .as_str()
                .unwrap()
                .contains("Run Check")
        );
    }

    #[test]
    fn connection_template_does_not_store_passwords() {
        let result = connection_template(&json!({
            "name": "Server",
            "type": "ssh",
            "host": "linux.example",
            "user": "guest",
            "command": "foot",
            "display": "display-1"
        }))
        .unwrap();
        let structured = &result["structuredContent"];
        let connection = structured["connection"].as_str().unwrap();
        assert!(connection.contains("host = \"linux.example\""));
        assert!(!connection.contains("password"));
        assert!(
            structured["run_waypipe"]
                .as_str()
                .unwrap()
                .contains("--display 'display-1' ssh")
        );
    }

    #[test]
    fn tools_are_blocked_until_initialized_notification() {
        let mut initialized = false;
        let response = handle_message(
            &json!({ "jsonrpc": "2.0", "id": 2, "method": "tools/list" }),
            Path::new("/tmp/missing.sock"),
            &mut initialized,
        )
        .unwrap();
        assert_eq!(response["error"]["code"], -32002);
    }
}

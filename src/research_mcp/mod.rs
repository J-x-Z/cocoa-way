//! Standalone opt-in research launcher; nothing here is linked into normal GUI
//! startup. Keep the kit fixed and author-reviewed while this process runs.
pub mod children;
#[cfg(unix)]
pub mod kit;
pub mod policy;
pub mod transport;

use serde_json::{Value, json};

use children::Children;
use policy::{Config, Host, LIMITS, Profile, launch_arguments, object_with_keys, stop_arguments};

const PROTOCOLS: &[&str] = &["2025-11-25", "2025-06-18", "2025-03-26", "2024-11-05"];
pub const STATUS: &str = "cocoa_way_research_status";
pub const LAUNCH: &str = "cocoa_way_research_launch";
pub const STOP: &str = "cocoa_way_research_stop";

pub fn run(args: impl IntoIterator<Item = std::ffi::OsString>) -> Result<(), &'static str> {
    let config = Config::parse(args)?;
    transport::serve(Server::new(config, Host::detect()))
}

pub struct Server {
    config: Config,
    host: Host,
    initialized: bool,
    initialize_seen: bool,
    pub closing: bool,
    pub children: Children,
}

impl Server {
    pub fn new(config: Config, host: Host) -> Self {
        Self {
            config,
            host,
            initialized: false,
            initialize_seen: false,
            closing: false,
            children: Children::default(),
        }
    }

    pub fn handle(&mut self, frame: &[u8]) -> Option<Value> {
        if frame.iter().all(u8::is_ascii_whitespace) {
            return None;
        }
        let message: Value = match serde_json::from_slice(frame) {
            Ok(value) => value,
            Err(_) => return Some(error(Value::Null, -32700, "Parse error")),
        };
        let Some(object) = message.as_object() else {
            return Some(error(Value::Null, -32600, "Invalid Request"));
        };
        let id = object.get("id").cloned();
        let valid_id = id.as_ref().is_none_or(|id| {
            id.as_i64().is_some()
                || id.as_u64().is_some()
                || id.as_str().is_some_and(|id| id.len() <= 256)
        });
        if !valid_id
            || object.get("jsonrpc").and_then(Value::as_str) != Some("2.0")
            || !object.get("method").is_some_and(Value::is_string)
        {
            return Some(error(
                if valid_id {
                    id.unwrap_or(Value::Null)
                } else {
                    Value::Null
                },
                -32600,
                "Invalid Request",
            ));
        }
        let method = object["method"].as_str().unwrap();
        let params = object.get("params").cloned().unwrap_or_else(|| json!({}));
        let Some(id) = id else {
            if method == "notifications/initialized" && self.initialize_seen {
                self.initialized = true;
            }
            return None;
        };
        let result = match method {
            "initialize" => {
                self.initialize_seen = true;
                self.initialized = false;
                let requested = params
                    .get("protocolVersion")
                    .and_then(Value::as_str)
                    .unwrap_or(PROTOCOLS[0]);
                let protocol = if PROTOCOLS.contains(&requested) {
                    requested
                } else {
                    PROTOCOLS[0]
                };
                json!({
                    "protocolVersion": protocol, "capabilities": {"tools": {}},
                    "serverInfo": {"name": "cocoa-way-research-mcp", "version": env!("CARGO_PKG_VERSION")},
                    "instructions": format!("{} {}", if self.config.enabled() {
                        "Experimental launch/stop were enabled by process flags. Only three fixed profiles may run, one child per profile and at most three total. Seamless GUI TTL defaults to 600 seconds, maximum 3600; GPU selftest defaults to 60, maximum 180. GUI stop requests same-owned-PID AppKit termination, then allows 2 seconds before forced group cleanup. Acceptance is not completion."
                    } else {
                        "Read-only by default: launch/stop are neither listed nor callable. Only restarting with --allow-experiments --root ABSOLUTE_KIT enables them; tool arguments cannot grant permission."
                    }, LIMITS)
                })
            }
            "ping" => json!({}),
            "tools/list" | "tools/call" if !self.initialized => {
                return Some(error(id, -32002, "Server is not initialized"));
            }
            "tools/list" => {
                if object_with_keys(&params, &[]).is_err() {
                    return Some(error(id, -32602, "No list parameters supported"));
                }
                json!({"tools": catalog(self.config.enabled())})
            }
            "tools/call" => match self.call(&params) {
                Ok(value) => value,
                Err(reason) => return Some(error(id, -32602, reason)),
            },
            _ => return Some(error(id, -32601, "Method not found")),
        };
        Some(json!({"jsonrpc": "2.0", "id": id, "result": result}))
    }

    fn call(&mut self, params: &Value) -> Result<Value, &'static str> {
        let object = object_with_keys(params, &["name", "arguments"])?;
        let name = object
            .get("name")
            .and_then(Value::as_str)
            .ok_or("tool_name_required")?;
        // Check the process-level gate before inspecting arguments. No MCP field
        // can turn this on, including callers that bypass tools/list.
        if (name == LAUNCH || name == STOP) && !self.config.enabled() {
            return Err("experiments_disabled_by_process_configuration");
        }
        let empty = json!({});
        let arguments = object.get("arguments").unwrap_or(&empty);
        match name {
            STATUS => {
                object_with_keys(arguments, &[])?;
                self.children.tick();
                Ok(tool_result(self.status(), false))
            }
            LAUNCH => {
                let (profile, ttl) = launch_arguments(arguments)?;
                let result = if self.closing {
                    Err("server_shutting_down")
                } else {
                    self.host.check().and_then(|_| {
                        self.children.launch(
                            self.config.root.as_deref().ok_or("kit_not_configured")?,
                            profile,
                            ttl,
                        )
                    })
                };
                match result {
                    Ok(()) => Ok(tool_result(
                        json!({"profile": profile.id(), "state": "spawned", "accepted": true, "completed": false, "ttl_seconds": ttl, "sha256_verified_at_launch": true, "acceptance": "unverified; spawning is not native UI, VM, or GPU acceptance"}),
                        false,
                    )),
                    Err(reason) => Ok(tool_result(
                        json!({"profile": profile.id(), "state": "unavailable", "reason": reason}),
                        true,
                    )),
                }
            }
            STOP => {
                let profile = stop_arguments(arguments)?;
                self.children.stop(profile);
                let owned_child = self.children.snapshot(profile);
                let completed = !self.children.is_active(profile)
                    && (owned_child["state"] == "never_started"
                        || (owned_child["leader_reaped"] == true
                            && owned_child["group_cleanup"] != "failed"));
                Ok(tool_result(
                    json!({"profile": profile.id(), "accepted": true, "completed": completed, "owned_child": owned_child,
                        "completion_scope": "owned leader reaped (or no owned child); process-group cleanup signalled, not native UI acceptance. AppKit acceptance alone is not completion; inspect status."}),
                    false,
                ))
            }
            _ => Err("unknown_tool"),
        }
    }

    fn status(&self) -> Value {
        #[cfg(unix)]
        let kit = self
            .host
            .check()
            .and_then(|_| self.config.root.as_deref().ok_or("kit_not_configured"))
            .and_then(kit::Kit::open);
        let profiles: Vec<_> = Profile::ALL.into_iter().map(|profile| {
            #[cfg(unix)]
            let eligibility = match &kit { Ok(kit) => kit.inspect(profile), Err(reason) => Err(*reason) };
            #[cfg(not(unix))]
            let eligibility: Result<(), &str> = Err("unsupported_platform");
            let (default_ttl, max_ttl) = profile.ttl_limits();
            json!({"profile": profile.id(), "availability": if eligibility.is_ok() { "eligible_pending_sha256_check" } else { "unavailable" },
                "default_ttl_seconds": default_ttl, "max_ttl_seconds": max_ttl,
                "reason": eligibility.err(), "owned_child": self.children.snapshot(profile)})
        }).collect();
        json!({"experiments_enabled": self.config.enabled(), "profiles": profiles,
            "gui_stop_grace_ms": policy::GUI_STOP_GRACE_MS, "stop_acceptance_is_not_completion": true,
            "max_owned_children": policy::MAX_CHILDREN, "child_output": "payloads withheld; counters capped at 64 KiB per stream; no output history",
            "limits": LIMITS})
    }
}

fn tool_result(data: Value, is_error: bool) -> Value {
    json!({"content": [{"type": "text", "text": data.to_string()}], "structuredContent": data, "isError": is_error})
}

pub fn error(id: Value, code: i64, message: &str) -> Value {
    json!({"jsonrpc": "2.0", "id": id, "error": {"code": code, "message": message}})
}

pub fn catalog(enabled: bool) -> Vec<Value> {
    let empty = json!({"type": "object", "properties": {}, "additionalProperties": false});
    let mut tools = vec![
        json!({"name": STATUS, "description": "Read fixed-profile availability and redacted owned-child status. No helper execution or permissions probes. Eligibility is not SHA verification, execution, or native acceptance.",
        "inputSchema": empty, "annotations": {"readOnlyHint": true, "destructiveHint": false, "openWorldHint": false, "idempotentHint": true}}),
    ];
    if enabled {
        let profile = json!({"type": "string", "enum": Profile::ALL.map(Profile::id)});
        tools.push(json!({"name": LAUNCH, "description": "EXPERIMENTAL developer-only: open a trusted Seamless host/client GUI with no arguments, or run gpu-vz-probe selftest ONLY. Does not boot a VM or establish GPU acceleration. No native reconnect completion claim. Requires installed owned kit, macOS 27+, matching helper SHA-256. No arbitrary path, argv, environment, shell, network, download, or permission-grant inputs.",
            "inputSchema": {"type": "object", "properties": {"profile": profile, "ttl_seconds": {"type": "integer", "minimum": 1, "maximum": 3600}}, "required": ["profile"], "additionalProperties": false, "oneOf": [
                {"type": "object", "properties": {"profile": {"type": "string", "enum": ["seamless_host", "seamless_client"]}, "ttl_seconds": {"type": "integer", "minimum": 1, "maximum": 3600, "default": 600}}, "required": ["profile"], "additionalProperties": false},
                {"type": "object", "properties": {"profile": {"type": "string", "enum": ["gpu_selftest"]}, "ttl_seconds": {"type": "integer", "minimum": 1, "maximum": 180, "default": 60}}, "required": ["profile"], "additionalProperties": false}
            ]},
            "annotations": {"readOnlyHint": false, "destructiveHint": false, "openWorldHint": true, "idempotentHint": false}}));
        tools.push(json!({"name": STOP, "description": "Request stop only for this launcher's owned child/process group. For GUI, first request same-owned-PID AppKit terminate, then allow 2 seconds before forced group cleanup/reap. Accepted is not completed: inspect status for leader_reaped. Never takes a PID or stops other app instances.",
            "inputSchema": {"type": "object", "properties": {"profile": profile}, "required": ["profile"], "additionalProperties": false},
            "annotations": {"readOnlyHint": false, "destructiveHint": true, "openWorldHint": false, "idempotentHint": true}}));
    }
    tools
}

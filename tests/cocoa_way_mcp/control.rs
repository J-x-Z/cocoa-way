//! Included by the MCP binary's tests; all requests use fake Unix sockets.
use super::*;
use std::io::BufReader;
use std::os::unix::net::UnixListener;
use std::time::{Duration, Instant};

const PROFILE: &str = "0123456789abcdef0123456789abcdef";
const ACTIONS: &[&str] = &[
    "cocoa_way_open_panel",
    "cocoa_way_check",
    "cocoa_way_launch",
    "cocoa_way_stop",
    "cocoa_way_display_create",
    "cocoa_way_display_close",
];

#[test]
fn stdio_frames_are_bounded_before_json_parsing() {
    let mut input = std::io::Cursor::new(b"one\ntwo\n");
    assert_eq!(read_mcp_frame(&mut input).unwrap().as_deref(), Some("one"));
    assert_eq!(read_mcp_frame(&mut input).unwrap().as_deref(), Some("two"));
    assert!(read_mcp_frame(&mut input).unwrap().is_none());
    for newline in [false, true] {
        let mut exact = vec![b'a'; MAX_MCP_FRAME_BYTES];
        if newline {
            exact.push(b'\n');
        }
        assert_eq!(
            read_mcp_frame(&mut std::io::Cursor::new(exact))
                .unwrap()
                .unwrap()
                .len(),
            MAX_MCP_FRAME_BYTES
        );
        let mut oversized = vec![b'a'; MAX_MCP_FRAME_BYTES + 100];
        if newline {
            oversized.push(b'\n');
        }
        assert_eq!(
            read_mcp_frame(&mut std::io::Cursor::new(oversized))
                .unwrap_err()
                .kind(),
            std::io::ErrorKind::InvalidData
        );
    }
    assert!(read_mcp_frame(&mut std::io::Cursor::new([0xff, b'\n'])).is_err());
}

fn rpc(socket: &Path, method: &str, params: Value, allow_control: bool) -> Value {
    handle_message(
        &json!({"jsonrpc": "2.0", "id": 1, "method": method, "params": params}),
        socket,
        &mut true,
        allow_control,
    )
    .unwrap()
}

fn route(name: &str, arguments: Value, allow_control: bool) -> ControlRequest {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("control.sock");
    let listener = UnixListener::bind(&path).unwrap();
    listener.set_nonblocking(true).unwrap();
    let worker = std::thread::spawn(move || {
        let deadline = Instant::now() + Duration::from_secs(3);
        let mut stream = loop {
            match listener.accept() {
                Ok((stream, _)) => break stream,
                Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                    assert!(Instant::now() < deadline, "expected request never arrived");
                    std::thread::sleep(Duration::from_millis(1));
                }
                Err(error) => panic!("fake server: {error}"),
            }
        };
        stream
            .set_read_timeout(Some(Duration::from_secs(2)))
            .unwrap();
        let mut line = String::new();
        BufReader::new(&stream).read_line(&mut line).unwrap();
        let request: ControlRequest = serde_json::from_str(&line).unwrap();
        let response = control_protocol::ControlResponse::success(
            &request.command,
            json!({"accepted": true, "completed": false}),
        );
        writeln!(stream, "{}", serde_json::to_string(&response).unwrap()).unwrap();
        request
    });
    let response = rpc(
        &path,
        "tools/call",
        json!({"name": name, "arguments": arguments}),
        allow_control,
    );
    let request = worker.join().unwrap();
    assert!(response.get("error").is_none(), "{response}");
    assert_eq!(response["result"]["isError"], false);
    assert_eq!(
        response["result"]["structuredContent"]["data"]["completed"],
        false
    );
    request
}

#[test]
fn flag_is_explicit_and_not_inferred_from_other_arguments() {
    assert!(!parse_arguments(Vec::<String>::new()).unwrap().allow_control);
    for args in [
        vec!["--allow-control", "--socket", "/tmp/test.sock"],
        vec!["--socket", "/tmp/test.sock", "--allow-control"],
    ] {
        let options = parse_arguments(args.into_iter().map(str::to_string)).unwrap();
        assert!(options.allow_control);
        assert_eq!(options.socket, Path::new("/tmp/test.sock"));
    }
    for args in [
        vec!["--allow-control=true"],
        vec!["--control"],
        vec!["--socket"],
        vec!["--socket", "--allow-control"],
        vec!["--allow-control", "--allow-control"],
        vec!["--socket", "/a", "--socket", "/b"],
    ] {
        assert!(parse_arguments(args.into_iter().map(str::to_string)).is_err());
    }
}

#[test]
fn catalog_and_initialization_reflect_control_without_weakening_default() {
    let socket = Path::new("/tmp/unused-mcp-control.sock");
    let readonly = rpc(socket, "tools/list", json!({}), false);
    let enabled = rpc(socket, "tools/list", json!({}), true);
    let readonly = readonly["result"]["tools"].as_array().unwrap();
    let enabled = enabled["result"]["tools"].as_array().unwrap();
    assert_eq!(enabled.len(), readonly.len() + ACTIONS.len());
    assert!(
        readonly
            .iter()
            .all(|tool| tool["annotations"]["readOnlyHint"] == true)
    );
    for name in ACTIONS {
        assert!(!readonly.iter().any(|tool| tool["name"] == *name));
        let tool = enabled.iter().find(|tool| tool["name"] == *name).unwrap();
        assert_eq!(tool["annotations"]["readOnlyHint"], false);
        assert_eq!(tool["annotations"]["openWorldHint"], true);
        assert_eq!(tool["annotations"]["idempotentHint"], false);
        assert!(
            tool["description"]
                .as_str()
                .unwrap()
                .contains("Queue acceptance")
        );
        assert_eq!(
            tool["annotations"]["destructiveHint"],
            matches!(*name, "cocoa_way_stop" | "cocoa_way_display_close")
        );
    }
    for tool in enabled {
        let name = tool["name"].as_str().unwrap();
        assert!(!name.contains("delete") && !name.contains("shell") && !name.contains("settings"));
    }
    let default = initialize_result(&json!({}), false);
    let control = initialize_result(&json!({}), true);
    assert!(
        default["instructions"]
            .as_str()
            .unwrap()
            .contains("neither listed nor callable")
    );
    assert!(
        control["instructions"]
            .as_str()
            .unwrap()
            .contains("Queue acceptance is not completion or success")
    );
}

#[test]
fn cached_read_commands_route_without_selectors_in_both_modes() {
    for enabled in [false, true] {
        for (tool, command) in [
            ("cocoa_way_debug_snapshot", "debug-snapshot"),
            ("cocoa_way_performance", "performance"),
            ("cocoa_way_clipboard", "clipboard"),
        ] {
            let request = route(tool, json!({}), enabled);
            assert_eq!(request.command, command);
            assert!(request.session.is_none());
        }
    }
}

#[test]
fn bounded_actions_route_exactly_to_existing_control_protocol() {
    for command in ["check", "launch", "stop"] {
        let request = route(
            &format!("cocoa_way_{command}"),
            json!({"profile_id": PROFILE}),
            true,
        );
        assert_eq!(request.command, command);
        assert_eq!(request.session.as_deref(), Some(PROFILE));
    }
    for page in ["applications", "displays", "diagnostics"] {
        let request = route("cocoa_way_open_panel", json!({"page": page}), true);
        assert_eq!(request.command, "open-panel");
        assert_eq!(request.session.as_deref(), Some(page));
    }
    assert_eq!(
        route("cocoa_way_open_panel", json!({}), true)
            .session
            .as_deref(),
        Some("applications")
    );
    for tool in ["cocoa_way_display_create", "cocoa_way_display_close"] {
        let request = route(tool, json!({"name": "display-2"}), true);
        assert_eq!(request.session.as_deref(), Some("display-2"));
        assert_eq!(
            request.command,
            if tool.ends_with("create") {
                "display-create"
            } else {
                "display-close"
            }
        );
    }
    assert!(
        route("cocoa_way_display_create", json!({}), true)
            .session
            .is_none()
    );
    let request = route(
        "cocoa_way_logs",
        json!({"session": PROFILE, "limit": 1000}),
        false,
    );
    assert_eq!(request.command, "logs");
    assert_eq!(request.limit, 1000);
}

#[test]
fn readonly_bypass_and_invalid_arguments_never_reach_socket() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("control.sock");
    let listener = UnixListener::bind(&path).unwrap();
    listener.set_nonblocking(true).unwrap();
    let reject = |params: Value, enabled: bool| {
        let response = rpc(&path, "tools/call", params.clone(), enabled);
        assert_eq!(
            response["error"]["code"], -32602,
            "accepted {params}: {response}"
        );
        assert!(
            matches!(listener.accept(), Err(error) if error.kind() == std::io::ErrorKind::WouldBlock)
        );
    };
    for name in ACTIONS {
        reject(
            json!({"name": name, "arguments": {"profile_id": PROFILE, "name": "test", "page": "applications"}}),
            false,
        );
        reject(json!({"name": name, "arguments": {}}), false);
    }
    for name in ["cocoa_way_launch", "cocoa_way_check", "cocoa_way_stop"] {
        for selector in [
            "",
            "0",
            "Browser",
            "../profile",
            "$(touch pwned)",
            "0123456789ABCDEF0123456789ABCDEF",
            "0123456789abcdef0123456789abcde",
            "0123456789abcdef0123456789abcdef0",
        ] {
            reject(
                json!({"name": name, "arguments": {"profile_id": selector}}),
                true,
            );
        }
        for args in [
            json!({"profile_id": 0}),
            json!({"profile_id": null}),
            json!({"session": PROFILE}),
            json!({"profile_id": PROFILE, "command": "sh"}),
            json!({"profile_id": PROFILE, "limit": 1}),
        ] {
            reject(json!({"name": name, "arguments": args}), true);
        }
    }
    for tool in ["cocoa_way_display_create", "cocoa_way_display_close"] {
        for name in [
            "",
            "default",
            "auto",
            "dedicated",
            "../x",
            "a/b",
            "Foo",
            "a b",
            "a--b",
            "-a",
            "a-",
            "a\nb",
            "a;sh",
            "a_b",
            "\u{e9}",
            &"a".repeat(65),
        ] {
            reject(json!({"name": tool, "arguments": {"name": name}}), true);
        }
    }
    reject(
        json!({"name": "cocoa_way_display_close", "arguments": {}}),
        true,
    );
    for page in ["settings", "network", "Applications", "../diagnostics"] {
        reject(
            json!({"name": "cocoa_way_open_panel", "arguments": {"page": page}}),
            true,
        );
    }
    for name in [
        "cocoa_way_debug_snapshot",
        "cocoa_way_performance",
        "cocoa_way_clipboard",
        "cocoa_way_status",
    ] {
        for args in [
            json!({"session": PROFILE}),
            json!({"limit": 1}),
            json!({"allow_control": true}),
            json!(null),
            json!([]),
            json!(false),
        ] {
            reject(json!({"name": name, "arguments": args}), false);
        }
    }
    for limit in [
        json!(0),
        json!(1001),
        json!(-1),
        json!(1.5),
        json!("10"),
        json!(null),
        json!(true),
    ] {
        reject(
            json!({"name": "cocoa_way_logs", "arguments": {"session": PROFILE, "limit": limit}}),
            false,
        );
    }
    for params in [
        json!({"name": "cocoa_way_application_template", "arguments": {"image": "a", "command": "b", "audio": "true"}}),
        json!({"name": "cocoa_way_connection_template", "arguments": {"host": "a", "port": "22"}}),
        json!({"name": "cocoa_way_onboarding", "arguments": {"goal": "launch"}}),
        json!({"name": "cocoa_way_issue_draft", "arguments": {"summary": "a", "steps": [1]}}),
        json!({"name": "cocoa_way_issue_draft", "arguments": {"summary": "a", "steps": vec!["step"; 33]}}),
        json!({"name": "cocoa_way_issue_draft", "arguments": {"summary": "a".repeat(4097)}}),
        json!({"name": "cocoa_way_issue_draft", "arguments": {"summary": "a", "steps": vec!["x".repeat(4096); 5]}}),
        json!({"name": "cocoa_way_status", "allow_control": true}),
        json!({"name": "cocoa_way_status", "_meta": false}),
        json!({"name": "cocoa_way_container_delete", "arguments": {}}),
        json!({"name": "launch", "arguments": {"session": PROFILE}}),
    ] {
        reject(params, true);
    }
}

#[test]
fn no_runtime_start_or_process_execution_path_in_mcp_or_client() {
    let source = include_str!("../../src/bin/cocoa-way-mcp.rs");
    let production = source.split("#[cfg(test)]").next().unwrap();
    let client = include_str!("../../src/control_client.rs");
    for code in [production, client] {
        for forbidden in [
            "Command::",
            "std::process::Command",
            "process::Command",
            "posix_spawn",
            "libc::system",
            "libc::exec",
            "libc::fork",
        ] {
            assert!(
                !code.contains(forbidden),
                "unexpected process execution: {forbidden}"
            );
        }
    }
    assert!(production.contains("control_client::send_request"));
}

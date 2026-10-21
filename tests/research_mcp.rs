//! No test launches a helper, GUI, VM, permissions probe, or shell.
#![allow(dead_code, unused_imports)]
#[path = "../src/research_mcp/mod.rs"]
mod research_mcp;

use std::ffi::OsString;

use research_mcp::children::{OUTPUT_CAP, OutputCount};
use research_mcp::policy::*;
use research_mcp::transport::{Frames, MAX_FRAME};
use research_mcp::{LAUNCH, STATUS, STOP, Server, catalog};
use serde_json::{Value, json};

fn config(args: &[&str]) -> Result<Config, &'static str> {
    Config::parse(args.iter().map(|arg| OsString::from(*arg)))
}

fn rpc(server: &mut Server, method: &str, params: Value) -> Value {
    server
        .handle(
            &serde_json::to_vec(
                &json!({"jsonrpc": "2.0", "id": 1, "method": method, "params": params}),
            )
            .unwrap(),
        )
        .unwrap()
}

fn ready(config: Config, host: Host) -> Server {
    let mut server = Server::new(config, host);
    rpc(
        &mut server,
        "initialize",
        json!({"protocolVersion": "2025-11-25"}),
    );
    assert!(
        server
            .handle(br#"{"jsonrpc":"2.0","method":"notifications/initialized"}"#)
            .is_none()
    );
    server
}

fn call(server: &mut Server, name: &str, arguments: Value) -> Value {
    rpc(
        server,
        "tools/call",
        json!({"name": name, "arguments": arguments}),
    )
}

#[test]
fn only_explicit_process_flags_enable_experiments() {
    assert!(!config(&[]).unwrap().enabled());
    assert!(!config(&["--root", "/owned/kit"]).unwrap().enabled());
    for args in [
        ["--allow-experiments", "--root", "/owned/kit"],
        ["--root", "/owned/kit", "--allow-experiments"],
    ] {
        assert!(config(&args).unwrap().enabled());
    }
    for args in [
        vec!["--allow-experiments"],
        vec!["--allow-experiments=true"],
        vec!["--root"],
        vec!["--root", "relative"],
        vec!["--root", "/"],
        vec!["--root", "/owned/../kit"],
        vec!["--root", "/owned/./kit"],
        vec!["--root", "/owned\0kit"],
        vec!["--root", "/a", "--root", "/b"],
        vec!["--allow-experiments", "--allow-experiments", "--root", "/a"],
        vec!["--command", "/bin/sh"],
        vec!["--ttl-seconds", "999"],
    ] {
        assert!(config(&args).is_err(), "{args:?}");
    }
}

#[test]
fn fixed_profiles_have_no_generic_exec_or_vm_boot() {
    assert_eq!(Profile::ALL.len(), MAX_CHILDREN);
    assert_eq!(
        Profile::SeamlessHost.path(),
        "SeamlessHost.app/Contents/MacOS/SeamlessHost"
    );
    assert_eq!(
        Profile::SeamlessClient.path(),
        "SeamlessClient.app/Contents/MacOS/SeamlessClient"
    );
    assert_eq!(Profile::GpuSelftest.path(), "gpu-vz-probe");
    assert!(Profile::SeamlessHost.argv().is_empty());
    assert!(Profile::SeamlessClient.argv().is_empty());
    assert_eq!(Profile::GpuSelftest.argv(), &["selftest"]);
    for id in [
        "host",
        "SeamlessHost",
        "../gpu-vz-probe",
        "/bin/sh",
        "gpu_boot",
        "",
    ] {
        assert!(Profile::parse(id).is_err());
    }
    for profile in Profile::ALL {
        assert_eq!(Profile::parse(profile.id()).unwrap(), profile);
    }
    assert!(Host::Unsupported.check().is_err());
    assert!(Host::MacOs(26).check().is_err());
    assert!(Host::MacOs(27).check().is_ok());
}

#[test]
fn ttl_and_tool_selectors_are_strict_and_bounded() {
    assert_eq!(
        launch_arguments(&json!({"profile": "gpu_selftest"})).unwrap(),
        (Profile::GpuSelftest, 60)
    );
    for profile in Profile::ALL {
        let (default, maximum) = profile.ttl_limits();
        assert_eq!(
            launch_arguments(&json!({"profile": profile.id()})).unwrap(),
            (profile, default)
        );
        for ttl in [1, default, maximum] {
            assert!(
                launch_arguments(&json!({"profile": profile.id(), "ttl_seconds": ttl})).is_ok()
            );
        }
        assert!(
            launch_arguments(&json!({"profile": profile.id(), "ttl_seconds": maximum + 1}))
                .is_err()
        );
    }
    assert_eq!(Profile::SeamlessHost.ttl_limits(), (600, 3600));
    assert_eq!(Profile::SeamlessClient.ttl_limits(), (600, 3600));
    assert_eq!(Profile::GpuSelftest.ttl_limits(), (60, 180));
    for ttl in [
        json!(0),
        json!(181),
        json!(-1),
        json!(1.5),
        json!(60.0),
        json!("60"),
        Value::Null,
        json!(true),
        json!(u64::MAX),
    ] {
        assert!(launch_arguments(&json!({"profile": "gpu_selftest", "ttl_seconds": ttl})).is_err());
    }
    for field in [
        "allow_experiments",
        "root",
        "path",
        "command",
        "argv",
        "pid",
        "env",
        "network",
        "grant_permissions",
    ] {
        let mut args = json!({"profile": "seamless_host"});
        args[field] = json!(true);
        assert!(launch_arguments(&args).is_err());
        assert!(stop_arguments(&args).is_err());
    }
    assert!(stop_arguments(&json!({"profile": "seamless_host", "ttl_seconds": 60})).is_err());
    for args in [json!({}), json!([]), Value::Null, json!({"profile": 1})] {
        assert!(launch_arguments(&args).is_err());
        assert!(stop_arguments(&args).is_err());
    }
}

#[test]
fn graceful_stop_acceptance_does_not_mean_completion() {
    for profile in [Profile::SeamlessHost, Profile::SeamlessClient] {
        assert_eq!(
            stop_action(profile, false, false, None),
            StopAction::RequestAppKit
        );
        // Both an accepted and a rejected AppKit request get bounded grace.
        for accepted in [false, true] {
            assert_eq!(
                stop_action(profile, false, false, Some(accepted)),
                StopAction::Wait
            );
            assert_eq!(
                stop_action(profile, false, true, Some(accepted)),
                StopAction::CleanupGroup
            );
            assert_eq!(
                stop_action(profile, true, false, Some(accepted)),
                StopAction::CleanupGroup
            );
        }
        // Even an unregistered GUI cannot extend the deadline indefinitely.
        assert_eq!(
            stop_action(profile, false, true, None),
            StopAction::CleanupGroup
        );
    }
    assert_eq!(
        stop_action(Profile::GpuSelftest, false, false, None),
        StopAction::CleanupGroup
    );
    assert_eq!(GUI_STOP_GRACE_MS, 2000);
}

#[test]
fn manifest_is_version_one_fixed_id_to_lowercase_sha256_only() {
    let hash = "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad";
    let manifest = Manifest::parse(
        &serde_json::to_vec(&json!({"version": 1, "profiles": {"gpu_selftest": hash}})).unwrap(),
    )
    .unwrap();
    assert!(manifest.0[Profile::GpuSelftest.index()].is_some());
    assert!(manifest.0[Profile::SeamlessHost.index()].is_none());
    assert!(Manifest::parse(br#"{"version":1,"profiles":{}}"#).is_ok());
    for value in [
        json!({"version": 2, "profiles": {}}),
        json!({"version": 1.0, "profiles": {}}),
        json!({"version": 1, "profiles": {}, "root": "/a"}),
        json!({"version": 1}),
        json!({"version": 1, "profiles": {"unknown": hash}}),
        json!({"version": 1, "profiles": {"gpu_selftest": {"sha256": hash, "path": "/bin/sh"}}}),
        json!({"version": 1, "profiles": {"gpu_selftest": hash.to_uppercase()}}),
        json!({"version": 1, "profiles": {"gpu_selftest": "00"}}),
    ] {
        assert!(Manifest::parse(&serde_json::to_vec(&value).unwrap()).is_err());
    }
    assert!(Manifest::parse(&vec![b' '; MAX_MANIFEST + 1]).is_err());
}

#[test]
fn default_catalog_and_direct_calls_both_enforce_gate() {
    for config in [
        Config::default(),
        config(&["--root", "/not/inspected/kit"]).unwrap(),
    ] {
        // A simulated supported host cannot bypass the process-level gate.
        let mut server = ready(config, Host::MacOs(27));
        let response = rpc(&mut server, "tools/list", json!({}));
        let tools = response["result"]["tools"].as_array().unwrap();
        assert_eq!(tools.len(), 1);
        assert_eq!(tools[0]["name"], STATUS);
        assert_eq!(tools[0]["annotations"]["readOnlyHint"], true);
        for name in [LAUNCH, STOP] {
            let response = call(
                &mut server,
                name,
                json!({"profile": "seamless_host", "allow_experiments": true}),
            );
            assert_eq!(
                response["error"]["message"],
                "experiments_disabled_by_process_configuration"
            );
        }
    }
    assert_eq!(catalog(true).len(), 3);
    for tool in catalog(true) {
        assert_eq!(tool["inputSchema"]["additionalProperties"], false);
    }
}

#[test]
fn unsupported_and_closing_servers_never_launch_even_when_enabled() {
    for host in [Host::Unsupported, Host::MacOs(26)] {
        let mut server = ready(
            config(&["--allow-experiments", "--root", "/no/assets/required"]).unwrap(),
            host,
        );
        let response = call(&mut server, LAUNCH, json!({"profile": "gpu_selftest"}));
        assert_eq!(response["result"]["isError"], true);
        assert_eq!(
            response["result"]["structuredContent"]["state"],
            "unavailable"
        );
        let status = call(&mut server, STATUS, json!({}));
        for profile in status["result"]["structuredContent"]["profiles"]
            .as_array()
            .unwrap()
        {
            assert_eq!(profile["availability"], "unavailable");
        }
    }
    let mut server = ready(
        config(&["--allow-experiments", "--root", "/no/assets/required"]).unwrap(),
        Host::MacOs(27),
    );
    server.closing = true;
    let response = call(&mut server, LAUNCH, json!({"profile": "seamless_client"}));
    assert_eq!(
        response["result"]["structuredContent"]["reason"],
        "server_shutting_down"
    );
    assert_eq!(
        call(&mut server, STOP, json!({"profile": "gpu_selftest"}))["result"]["isError"],
        false
    );
}

#[test]
fn protocol_requires_initialization_and_never_echoes_parse_details() {
    let mut server = Server::new(Config::default(), Host::Unsupported);
    assert!(
        server
            .handle(br#"{"jsonrpc":"2.0","method":"notifications/initialized"}"#)
            .is_none()
    );
    assert_eq!(
        rpc(&mut server, "tools/list", json!({}))["error"]["code"],
        -32002
    );
    let response = server
        .handle(b"private clipboard and keys are not JSON")
        .unwrap();
    assert_eq!(response["error"]["message"], "Parse error");
    assert!(!response.to_string().contains("clipboard"));
    for id in [json!(true), json!({}), Value::Null, json!("x".repeat(257))] {
        let frame =
            serde_json::to_vec(&json!({"jsonrpc": "2.0", "id": id, "method": "ping"})).unwrap();
        assert_eq!(server.handle(&frame).unwrap()["error"]["code"], -32600);
    }
    let mut server = ready(Config::default(), Host::Unsupported);
    assert!(
        call(&mut server, STATUS, json!({"window_titles": true}))
            .get("error")
            .is_some()
    );
    assert!(
        call(&mut server, "exec", json!({"command": "anything"}))
            .get("error")
            .is_some()
    );
}

#[test]
fn frames_and_eof_are_bounded_before_json_parsing() {
    let mut frames = Frames::default();
    frames.eof = true;
    assert!(frames.next().unwrap().is_none());
    for newline in [false, true] {
        let mut frames = Frames::default();
        frames.push(&vec![b'a'; MAX_FRAME]).unwrap();
        if newline {
            frames.push(b"\n").unwrap();
        } else {
            frames.eof = true;
        }
        assert_eq!(frames.next().unwrap().unwrap().len(), MAX_FRAME);
        assert!(frames.next().unwrap().is_none());
    }
    let mut frames = Frames::default();
    frames.push(&vec![b'a'; MAX_FRAME + 1]).unwrap();
    assert!(frames.next().is_err());
    assert!(frames.push(b"more").is_err());
    let mut frames = Frames::default();
    frames.push(b"one\ntw").unwrap();
    assert_eq!(frames.next().unwrap().unwrap(), b"one");
    assert!(frames.next().unwrap().is_none());
    frames.push(b"o\n").unwrap();
    assert_eq!(frames.next().unwrap().unwrap(), b"two");
}

#[test]
fn output_accounting_never_retains_bytes_or_grows_unbounded() {
    let mut output = OutputCount::default();
    output.observe(OUTPUT_CAP);
    assert_eq!(output.bytes, OUTPUT_CAP);
    assert!(!output.capped);
    output.observe(usize::MAX);
    assert_eq!(output.bytes, OUTPUT_CAP);
    assert!(output.capped);
}

#[cfg(unix)]
mod filesystem {
    use super::*;
    use research_mcp::kit::Kit;
    use std::fs;
    use std::os::unix::fs::{PermissionsExt, symlink};
    use std::path::Path;

    fn mode(path: &Path, mode: u32) {
        fs::set_permissions(path, fs::Permissions::from_mode(mode)).unwrap();
    }

    fn fixture() -> (tempfile::TempDir, std::path::PathBuf) {
        let temp = tempfile::tempdir().unwrap();
        // macOS /var is itself a symlink; the real trusted root must not be.
        let root = temp.path().canonicalize().unwrap().join("kit");
        fs::create_dir(&root).unwrap();
        mode(&root, 0o700);
        fs::write(root.join("gpu-vz-probe"), b"abc").unwrap();
        mode(&root.join("gpu-vz-probe"), 0o700);
        fs::write(root.join(MANIFEST_NAME), br#"{"version":1,"profiles":{"gpu_selftest":"ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"}}"#).unwrap();
        mode(&root.join(MANIFEST_NAME), 0o600);
        (temp, root)
    }

    #[test]
    fn exact_hash_is_required_and_missing_profiles_are_unavailable() {
        let (_temp, root) = fixture();
        let kit = Kit::open(&root).unwrap();
        assert!(kit.verify(Profile::GpuSelftest, || {}).is_ok());
        assert!(kit.inspect(Profile::SeamlessHost).is_err());
        fs::write(root.join("gpu-vz-probe"), b"abd").unwrap();
        assert!(matches!(
            kit.verify(Profile::GpuSelftest, || {}),
            Err("sha256_mismatch")
        ));
    }

    #[test]
    fn manifest_root_and_helper_symlinks_and_writable_modes_are_refused() {
        let (temp, root) = fixture();
        let alias = temp.path().canonicalize().unwrap().join("alias");
        symlink(&root, &alias).unwrap();
        assert!(Kit::open(&alias).is_err());
        for path in [&root, &root.join(MANIFEST_NAME), &root.join("gpu-vz-probe")] {
            let old_mode = fs::metadata(path).unwrap().permissions().mode();
            mode(path, old_mode | 0o020);
            assert!(
                Kit::open(&root)
                    .and_then(|kit| kit.inspect(Profile::GpuSelftest))
                    .is_err()
            );
            mode(path, old_mode);
        }
        let helper = root.join("gpu-vz-probe");
        fs::rename(&helper, root.join("original")).unwrap();
        symlink(root.join("original"), &helper).unwrap();
        assert!(
            Kit::open(&root)
                .unwrap()
                .inspect(Profile::GpuSelftest)
                .is_err()
        );
        let manifest = root.join(MANIFEST_NAME);
        fs::rename(&manifest, root.join("original-manifest")).unwrap();
        symlink(root.join("original-manifest"), &manifest).unwrap();
        assert!(Kit::open(&root).is_err());
    }

    #[test]
    fn directory_symlinks_and_manifest_mutation_are_refused() {
        let (_temp, root) = fixture();
        let kit = Kit::open(&root).unwrap();
        let helper = kit.verify(Profile::GpuSelftest, || {}).unwrap();
        fs::write(root.join(MANIFEST_NAME), br#"{"version":1,"profiles":{}}"#).unwrap();
        assert!(kit.recheck(Profile::GpuSelftest, &helper).is_err());
        fs::write(
            root.join(MANIFEST_NAME),
            serde_json::to_vec(
                &json!({"version": 1, "profiles": {"seamless_host": "00".repeat(32)}}),
            )
            .unwrap(),
        )
        .unwrap();
        let target = root.join("other");
        fs::create_dir(&target).unwrap();
        mode(&target, 0o700);
        symlink(&target, root.join("SeamlessHost.app")).unwrap();
        assert!(
            Kit::open(&root)
                .unwrap()
                .inspect(Profile::SeamlessHost)
                .is_err()
        );
    }
}

//! Run with rustc --edition=2024 --test; no AppKit, runtime CLI or Cargo required.
#![allow(dead_code)]

#[path = "../src/container_mode/action_targets.rs"]
mod action_targets;

const GUI: &str = include_str!("../src/container_mode.rs");
const MAIN: &str = include_str!("../src/main.rs");
const IDENTITY: &str = include_str!("../src/container_identity.rs");

fn section<'a>(source: &'a str, start: &str, end: &str) -> &'a str {
    source
        .split_once(start)
        .unwrap()
        .1
        .split_once(end)
        .unwrap()
        .0
}

#[test]
fn stop_callbacks_use_live_ids_not_validation_or_profile_resolution() {
    for method in [
        "fn stop_container_session(",
        "fn force_stop_container_session(",
        "fn release_display_attachment(",
    ] {
        let body = section(GUI, method, "#[method(");
        assert!(body.contains("owned_stop_id(sender)"));
        for forbidden in [
            "profile_action_index",
            "session_can_stop",
            "resolve_session",
            "load_sessions",
        ] {
            assert!(!body.contains(forbidden), "{method}: {forbidden}");
        }
    }
    let resolver = section(
        GUI,
        "fn owned_stop_id(",
        "pub(crate) fn control_stoppable_profiles",
    );
    assert!(resolver.contains("resolve_owned_stop"));
    assert!(!resolver.contains("container_sessions::"));
}

#[test]
fn malformed_or_missing_configuration_keeps_recovery_stop_controls() {
    let install = section(
        GUI,
        "unsafe fn install_content(",
        "fn add_sidebar_background(",
    );
    assert!(install.contains("add_owned_recovery_list"));
    assert!(install.contains("config_error.is_some() || sessions.is_empty()"));
    let recovery = section(
        GUI,
        "fn add_owned_recovery_row(",
        "fn add_config_error_list(",
    );
    assert!(recovery.contains("setIdentifier:"));
    assert!(recovery.contains("profile.id.to_string()"));
    assert!(recovery.contains("sel!(stopContainerSession:)"));
    assert!(!recovery.contains("resolve_session"));
}

#[test]
fn queued_stop_dispatch_resolves_active_and_pending_owners_without_config_gate() {
    let dispatch = section(
        MAIN,
        "CompositorMessage::StopContainerSession(i) =>",
        "CompositorMessage::ForceStopContainerSession(i) =>",
    );
    assert!(dispatch.contains("active_container_sessions"));
    assert!(dispatch.contains("pending_profile_operations"));
    assert!(dispatch.contains("request_graceful_container_stop"));
    for forbidden in ["load_sessions", "resolve_session", "config_path"] {
        assert!(!dispatch.contains(forbidden));
    }
}

#[test]
fn confirmation_queues_the_already_captured_docker_identity() {
    let callback = section(
        GUI,
        "fn delete_runtime_container(",
        "#[method(restartRuntimeContainer:)]",
    );
    assert!(callback.contains("docker_target.is_none()"));
    assert!(
        callback.find(".cloned()").unwrap() < callback.find("confirm_delete_resource").unwrap()
    );
    assert!(
        callback.find("confirm_delete_resource").unwrap()
            < callback
                .find("send(CompositorMessage::DeleteRuntimeContainer")
                .unwrap()
    );
    assert!(callback.contains("docker_target"));
    let inventory = section(GUI, "fn docker_container_rows(", "fn orbstack_is_running(");
    for required in [
        "endpoint.host()",
        "--host",
        "--no-trunc",
        "{{.ID}}",
        "DockerDeleteTarget::capture",
    ] {
        assert!(inventory.contains(required));
    }
}

#[test]
fn unpinned_image_volume_and_legacy_container_delete_paths_are_closed() {
    for (start, end) in [
        ("fn spawn_image_delete(", "fn spawn_volume_delete("),
        ("fn spawn_volume_delete(", "fn spawn_volume_create("),
    ] {
        let function = section(MAIN, start, end);
        assert!(function.contains("unpinned_resource_delete_error(&runtime)"));
        assert!(!function.contains("\"docker\" => (\"docker\""));
    }
    let factory = section(
        MAIN,
        "fn runtime_container_command(",
        "fn spawn_apple_runtime_container_restart(",
    );
    assert!(factory.contains("(\"docker\" | \"orb\" | \"orbstack\", _) => None"));
}

#[test]
fn owned_cleanup_pins_endpoint_before_inspect_and_immutable_id_delete() {
    let cleanup = section(IDENTITY, "pub fn cleanup_owned_instance(", "#[cfg(test)]");
    assert!(cleanup.contains("no captured Docker endpoint"));
    assert!(cleanup.contains("DockerEndpoint::pin_environment(&mut inspect)"));
    assert!(cleanup.contains("DockerEndpoint::pin_environment(&mut delete)"));
    assert!(cleanup.contains("\"--host\", endpoint.host(), \"rm\""));
    assert!(cleanup.contains("deletion_target(identity, &output.stdout)"));
}

#[test]
fn ownership_snapshots_are_coalesced_not_cloned_for_each_input_event() {
    let entry = section(
        MAIN,
        "Ok(move |event: Event<()>, target: &ActiveEventLoop| {",
        "let input_trace",
    );
    assert!(!entry.contains("sync_stoppable_profiles"));
    let batch = section(
        MAIN,
        "let mut profile_ownership_changed = false;",
        "        match event {",
    );
    assert!(batch.contains("profile_ownership_changed |= matches!("));
    assert!(batch.contains("if profile_ownership_changed {\n            sync_stoppable_profiles"));
    assert_eq!(batch.matches("sync_stoppable_profiles(").count(), 1);
    for message in [
        "CheckContainerSession",
        "StartContainerSession",
        "ContainerSessionChecked",
        "DedicatedDisplayStarted",
        "DedicatedDisplayFailed",
        "ContainerSessionLaunchFinished",
        "StopContainerSession",
        "ForceStopContainerSession",
        "CreateManagedDisplay",
    ] {
        let marking = batch.split_once("            match msg {").unwrap().0;
        assert!(marking.contains(message));
    }
}

#[test]
fn maintenance_publishes_natural_exits_only_when_owned_instances_change() {
    let maintenance = section(
        MAIN,
        "if now >= next_maintenance {",
        "next_maintenance = now",
    );
    assert!(maintenance.contains("let previous_active_count = active_container_sessions.len()"));
    assert!(maintenance.contains("if active_container_sessions.len() != previous_active_count"));
    assert!(maintenance.contains("sync_stoppable_profiles("));
}

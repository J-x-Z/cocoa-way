use crate::runtime_commands::action_targets::{DockerEndpoint, capture_docker_endpoint};
use serde::Deserialize;
use serde::de::{MapAccess, Visitor};
use std::collections::HashMap;
use std::fmt;
use std::io::Read;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::Duration;

// Apple Container 1.0.0 documents run --label; unsupported versions fail creation, not cleanup safety.
// https://github.com/apple/container/blob/1.0.0/docs/command-reference.md#container-run
pub(super) const OWNER_LABEL: &str = "org.cocoa-way.owner";
const MAX_INSPECT_BYTES: usize = 1024 * 1024;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Runtime {
    Apple,
    Docker,
    OrbStack,
}

/// An attempt's immutable cleanup authority, not proof that container creation succeeded.
#[derive(Clone, Debug)]
pub struct OwnedContainerIdentity {
    runtime: Runtime,
    name: String,
    runtime_binary: PathBuf,
    owner_token: String,
    child_path: String,
    docker_endpoint: Option<DockerEndpoint>,
}

impl OwnedContainerIdentity {
    pub(super) fn new(
        runtime: &str,
        name: &str,
        binary: &Path,
        child_path: &str,
    ) -> Result<Self, String> {
        let mut random = [0u8; 32];
        std::fs::File::open("/dev/urandom")
            .and_then(|mut file| file.read_exact(&mut random))
            .map_err(|error| format!("cannot generate container owner token: {error}"))?;
        let token = random
            .iter()
            .map(|byte| format!("{byte:02x}"))
            .collect::<String>();
        let mut identity = Self::with_token(runtime, name, binary, child_path, &token)?;
        if identity.runtime == Runtime::Docker {
            identity.docker_endpoint = Some(capture_docker_endpoint(
                std::env::var("DOCKER_CONTEXT").ok().as_deref(),
                std::env::var("DOCKER_HOST").ok().as_deref(),
                |args| {
                    let mut command = Command::new(binary);
                    command.env("PATH", child_path).args(args);
                    let output = crate::runtime_commands::capture(command, Duration::from_secs(3))
                        .map_err(|error| error.to_string())?;
                    if !output.status.success() {
                        return Err("Docker endpoint could not be captured; no owned container was started.".into());
                    }
                    Ok(String::from_utf8_lossy(&output.stdout).into_owned())
                },
            )?);
        }
        Ok(identity)
    }

    pub(super) fn with_token(
        runtime: &str,
        name: &str,
        binary: &Path,
        child_path: &str,
        token: &str,
    ) -> Result<Self, String> {
        let runtime = match runtime.trim() {
            "container" | "apple" | "apple-container" => Runtime::Apple,
            "docker" => Runtime::Docker,
            "orb" | "orbstack" => Runtime::OrbStack,
            _ => return Err("unknown owned-container runtime".into()),
        };
        if name.is_empty()
            || name.starts_with('-')
            || !name
                .bytes()
                .all(|byte| byte.is_ascii_alphanumeric() || b"._-".contains(&byte))
        {
            return Err("invalid owned-container name".into());
        }
        if token.len() != 64
            || !token
                .bytes()
                .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
        {
            return Err("invalid container owner token".into());
        }
        if !binary.is_absolute() {
            return Err("owned-container runtime binary must be an absolute captured path".into());
        }
        Ok(Self {
            runtime,
            name: name.to_string(),
            runtime_binary: binary.to_path_buf(),
            owner_token: token.to_string(),
            child_path: child_path.to_string(),
            docker_endpoint: None,
        })
    }

    pub fn runtime(&self) -> &'static str {
        match self.runtime {
            Runtime::Apple => "container",
            Runtime::Docker => "docker",
            Runtime::OrbStack => "orbstack",
        }
    }

    pub fn name(&self) -> &str {
        &self.name
    }

    pub fn runtime_binary(&self) -> &Path {
        &self.runtime_binary
    }

    pub fn owner_token(&self) -> &str {
        &self.owner_token
    }

    pub(super) fn append_owner_label(&self, command: &mut Command) {
        if let Some(endpoint) = &self.docker_endpoint {
            // `run` already occupies argv[1]; use a pinned environment for creation.
            DockerEndpoint::pin_environment(command);
            command.env("DOCKER_HOST", endpoint.host());
        }
        if self.runtime != Runtime::OrbStack {
            command
                .arg("--label")
                .arg(format!("{OWNER_LABEL}={}", self.owner_token));
        }
    }
}

// Refuse duplicate keys as well as non-string label values; ambiguous metadata is not authority.
struct Labels(HashMap<String, String>);

impl<'de> Deserialize<'de> for Labels {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        struct LabelVisitor;
        impl<'de> Visitor<'de> for LabelVisitor {
            type Value = Labels;
            fn expecting(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
                formatter.write_str("a unique string-to-string label dictionary")
            }
            fn visit_map<M: MapAccess<'de>>(self, mut map: M) -> Result<Labels, M::Error> {
                let mut labels = HashMap::new();
                while let Some((key, value)) = map.next_entry::<String, String>()? {
                    if labels.insert(key, value).is_some() {
                        return Err(serde::de::Error::custom("duplicate container label"));
                    }
                }
                Ok(Labels(labels))
            }
        }
        deserializer.deserialize_map(LabelVisitor)
    }
}

#[derive(Deserialize)]
struct AppleConfiguration {
    id: String,
    labels: Labels,
}

#[derive(Deserialize)]
struct AppleInspect {
    #[serde(default, deserialize_with = "present_id")]
    id: Option<String>,
    configuration: AppleConfiguration,
}

fn present_id<'de, D: serde::Deserializer<'de>>(
    deserializer: D,
) -> Result<Option<String>, D::Error> {
    String::deserialize(deserializer).map(Some)
}

#[derive(Deserialize)]
struct DockerConfiguration {
    #[serde(rename = "Labels")]
    labels: Labels,
}

#[derive(Deserialize)]
struct DockerInspect {
    #[serde(rename = "Id")]
    id: String,
    #[serde(rename = "Name")]
    name: String,
    #[serde(rename = "Config")]
    config: DockerConfiguration,
}

fn deletion_target(identity: &OwnedContainerIdentity, bytes: &[u8]) -> Result<String, String> {
    if bytes.len() > MAX_INSPECT_BYTES {
        return Err("owned cleanup refused: inspect JSON exceeds 1 MiB".into());
    }
    let (target, labels) = match identity.runtime {
        Runtime::Apple => {
            // Apple 1.0.0 ManagedContainer and ContainerSnapshot serialize configuration.id/labels.
            // https://github.com/apple/container/blob/1.0.0/Sources/ContainerResource/Container/ManagedContainer.swift
            let mut records: Vec<AppleInspect> =
                serde_json::from_slice(bytes).map_err(|error| {
                    format!("owned cleanup refused: unknown Apple inspect schema: {error}")
                })?;
            if records.len() != 1 {
                return Err("owned cleanup refused: expected one Apple container".into());
            }
            let record = records.pop().unwrap();
            if record.configuration.id != identity.name
                || record.id.as_ref().is_some_and(|id| id != &identity.name)
            {
                return Err(
                    "owned cleanup refused: Apple container ID does not match the attempt".into(),
                );
            }
            (record.configuration.id, record.configuration.labels)
        }
        Runtime::Docker => {
            // https://github.com/moby/moby/blob/master/api/types/container/container.go (InspectResponse)
            // https://github.com/moby/moby/blob/master/api/types/container/config.go (Config.Labels)
            let mut records: Vec<DockerInspect> =
                serde_json::from_slice(bytes).map_err(|error| {
                    format!("owned cleanup refused: unknown Docker inspect schema: {error}")
                })?;
            if records.len() != 1 {
                return Err("owned cleanup refused: expected one Docker container".into());
            }
            let record = records.pop().unwrap();
            if record.name != format!("/{}", identity.name)
                || record.id.len() != 64
                || !record.id.bytes().all(|byte| byte.is_ascii_hexdigit())
            {
                return Err("owned cleanup refused: invalid Docker container identity".into());
            }
            (record.id, record.config.labels)
        }
        Runtime::OrbStack => return Err("owned cleanup does not delete OrbStack machines".into()),
    };
    if labels.0.get(OWNER_LABEL) != Some(&identity.owner_token) {
        return Err(
            "owned cleanup refused: container owner label is missing or belongs to another attempt"
                .into(),
        );
    }
    Ok(target)
}

fn confirmed_missing(identity: &OwnedContainerIdentity, stderr: &[u8]) -> bool {
    if !super::runtime_output_is_not_found(stderr, &identity.name) {
        return false;
    }
    let text = String::from_utf8_lossy(stderr);
    let lines = text.lines().map(str::trim).filter(|line| !line.is_empty());
    let mut matched = false;
    for line in lines {
        let line = line.strip_prefix("Error: ").unwrap_or(line);
        let missing = match identity.runtime {
            Runtime::Apple => {
                // ContainerInspect throws .notFound with this exact single-ID message.
                let line = line.strip_prefix("notFound: ").unwrap_or(line);
                line.trim_matches('"') == format!("container not found: {}", identity.name)
            }
            Runtime::Docker => {
                let line = line
                    .strip_prefix("Error response from daemon: ")
                    .unwrap_or(line);
                line == format!("No such container: {}", identity.name)
                    || line == format!("No such object: {}", identity.name)
            }
            Runtime::OrbStack => false,
        };
        if !missing {
            return false;
        }
        matched = true;
    }
    matched
}

/// Inspect with the captured executable and delete only an exact owner-label match.
/// Apple IDs are reusable names: inspect/delete is not atomic. Docker uses the inspected immutable ID.
pub fn cleanup_owned_instance(identity: &OwnedContainerIdentity) -> Result<(), String> {
    if identity.runtime == Runtime::OrbStack {
        // Preserve the existing no-VM-deletion contract for OrbStack machine sessions.
        return Ok(());
    }
    let mut inspect = Command::new(&identity.runtime_binary);
    inspect.env("PATH", &identity.child_path);
    match identity.runtime {
        Runtime::Apple => {
            inspect.args(["inspect", &identity.name]);
        }
        Runtime::Docker => {
            let endpoint = identity.docker_endpoint.as_ref()
                .ok_or("owned cleanup refused: no captured Docker endpoint; current context will not be used")?;
            DockerEndpoint::pin_environment(&mut inspect);
            inspect.args([
                "--host",
                endpoint.host(),
                "container",
                "inspect",
                &identity.name,
            ]);
        }
        Runtime::OrbStack => unreachable!(),
    }
    let output = crate::runtime_commands::capture(inspect, Duration::from_secs(6))
        .map_err(|error| format!("owned cleanup refused: inspect failed: {error}"))?;
    if !output.status.success() {
        if confirmed_missing(identity, &output.stderr) {
            return Ok(());
        }
        return Err(format!(
            "owned cleanup refused: inspect failed: {}",
            super::runtime_failure_detail(&output)
        ));
    }
    let target = deletion_target(identity, &output.stdout)?;
    let mut delete = Command::new(&identity.runtime_binary);
    delete.env("PATH", &identity.child_path);
    match identity.runtime {
        Runtime::Apple => {
            delete.args(["delete", "--force", &target]);
        }
        Runtime::Docker => {
            let endpoint = identity
                .docker_endpoint
                .as_ref()
                .ok_or("owned cleanup refused: no captured Docker endpoint")?;
            DockerEndpoint::pin_environment(&mut delete);
            delete.args(["--host", endpoint.host(), "rm", "--force", "--", &target]);
        }
        Runtime::OrbStack => unreachable!(),
    }
    let output = crate::runtime_commands::capture(delete, Duration::from_secs(6))
        .map_err(|error| format!("owned container deletion failed: {error}"))?;
    if output.status.success() {
        Ok(())
    } else {
        Err(format!(
            "owned container deletion failed: {}",
            super::runtime_failure_detail(&output)
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::PermissionsExt;

    const NAME: &str = "cocoa-way-owned-test";
    const TOKEN: &str = "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef";
    const DOCKER_ID: &str = "abcdef0123456789abcdef0123456789abcdef0123456789abcdef0123456789";

    struct MockRuntime {
        directory: tempfile::TempDir,
        binary: PathBuf,
    }

    impl MockRuntime {
        fn new() -> Self {
            let directory = tempfile::tempdir().unwrap();
            let binary = directory.path().join("runtime");
            // Derive fixture paths from the captured executable, not global environment.
            std::fs::write(
                &binary,
                r#"#!/bin/sh
root=${0%/*}
printf '%s\n' "$*" >> "$root/commands"
if [ "$1" = --host ]; then shift 2; fi
case "$1" in
    inspect|container)
        /bin/cat "$root/inspect.json"
        if [ -f "$root/inspect-error" ]; then /bin/cat "$root/inspect-error" >&2; fi
        exit "$(/bin/cat "$root/inspect-status")"
        ;;
esac
exit 0
"#,
            )
            .unwrap();
            std::fs::set_permissions(&binary, std::fs::Permissions::from_mode(0o700)).unwrap();
            std::fs::write(directory.path().join("inspect-status"), "0").unwrap();
            Self { directory, binary }
        }

        fn identity(&self, runtime: &str) -> OwnedContainerIdentity {
            let mut identity = OwnedContainerIdentity::with_token(
                runtime,
                NAME,
                &self.binary,
                "/usr/bin:/bin",
                TOKEN,
            )
            .unwrap();
            if identity.runtime == Runtime::Docker {
                identity.docker_endpoint =
                    Some(DockerEndpoint::local("unix:///captured/docker.sock").unwrap());
            }
            identity
        }

        fn inspect(&self, value: &str) {
            std::fs::write(self.directory.path().join("inspect.json"), value).unwrap();
            let _ = std::fs::remove_file(self.directory.path().join("commands"));
        }

        fn commands(&self) -> String {
            std::fs::read_to_string(self.directory.path().join("commands")).unwrap_or_default()
        }
    }

    fn apple_record(labels: serde_json::Value) -> String {
        serde_json::json!([{"id": NAME, "configuration": {"id": NAME, "labels": labels}}])
            .to_string()
    }

    fn docker_record(labels: serde_json::Value) -> String {
        serde_json::json!([{"Id": DOCKER_ID, "Name": format!("/{NAME}"), "Config": {"Labels": labels}}])
            .to_string()
    }

    fn owner_labels() -> serde_json::Value {
        serde_json::json!({OWNER_LABEL: TOKEN})
    }

    #[test]
    fn tokens_are_unique_and_runtime_aliases_are_canonical() {
        let first =
            OwnedContainerIdentity::new("apple", NAME, Path::new("/mock/container"), "").unwrap();
        let second =
            OwnedContainerIdentity::new("container", NAME, Path::new("/mock/container"), "")
                .unwrap();
        assert_ne!(first.owner_token(), second.owner_token());
        assert_eq!(first.owner_token().len(), 64);
        assert_eq!(first.runtime(), "container");
        assert_eq!(first.clone().owner_token(), first.owner_token());
    }

    #[test]
    fn relay_identity_rejects_missing_or_invalid_authority() {
        for token in ["", "not-an-owner-token", &"x".repeat(64)] {
            assert!(
                OwnedContainerIdentity::with_token(
                    "container",
                    NAME,
                    Path::new("/mock/container"),
                    "",
                    token
                )
                .is_err()
            );
        }
        for name in ["", "-other", "name/other", "name\nother"] {
            assert!(
                OwnedContainerIdentity::with_token(
                    "container",
                    name,
                    Path::new("/mock/container"),
                    "",
                    TOKEN
                )
                .is_err()
            );
        }
        assert!(
            OwnedContainerIdentity::with_token(
                "unknown",
                NAME,
                Path::new("/mock/container"),
                "",
                TOKEN
            )
            .is_err()
        );
        assert!(
            OwnedContainerIdentity::with_token("docker", NAME, Path::new("docker"), "", TOKEN)
                .is_err()
        );
    }

    #[test]
    fn matching_apple_label_allows_only_expected_name_deletion() {
        let mock = MockRuntime::new();
        mock.inspect(&apple_record(owner_labels()));
        cleanup_owned_instance(&mock.identity("apple-container")).unwrap();
        assert_eq!(
            mock.commands(),
            format!("inspect {NAME}\ndelete --force {NAME}\n")
        );

        // Verified ContainerSnapshot shape omits the redundant top-level ID.
        mock.inspect(
            &serde_json::json!([{"configuration": {"id": NAME, "labels": owner_labels()}}])
                .to_string(),
        );
        cleanup_owned_instance(&mock.identity("container")).unwrap();
        assert_eq!(
            mock.commands(),
            format!("inspect {NAME}\ndelete --force {NAME}\n")
        );
    }

    #[test]
    fn matching_docker_label_deletes_inspected_immutable_id_not_name() {
        let mock = MockRuntime::new();
        mock.inspect(&docker_record(owner_labels()));
        cleanup_owned_instance(&mock.identity("docker")).unwrap();
        assert_eq!(
            mock.commands(),
            format!(
                "--host unix:///captured/docker.sock container inspect {NAME}\n--host unix:///captured/docker.sock rm --force -- {DOCKER_ID}\n"
            )
        );
    }

    #[test]
    fn missing_mismatched_and_invalid_labels_never_delete() {
        let mock = MockRuntime::new();
        let invalid = [
            serde_json::json!({}),
            serde_json::json!({OWNER_LABEL: "another-attempt"}),
            serde_json::json!({OWNER_LABEL: null}),
            serde_json::json!({OWNER_LABEL: 1}),
            serde_json::json!({OWNER_LABEL: [TOKEN]}),
            serde_json::Value::Null,
            serde_json::json!([]),
        ];
        for runtime in ["container", "docker"] {
            for labels in &invalid {
                let json = if runtime == "container" {
                    apple_record(labels.clone())
                } else {
                    docker_record(labels.clone())
                };
                mock.inspect(&json);
                assert!(
                    cleanup_owned_instance(&mock.identity(runtime)).is_err(),
                    "{runtime}: {json}"
                );
                let expected = if runtime == "container" {
                    format!("inspect {NAME}\n")
                } else {
                    format!("--host unix:///captured/docker.sock container inspect {NAME}\n")
                };
                assert_eq!(mock.commands(), expected);
            }
        }
    }

    #[test]
    fn ambiguous_or_unknown_inspect_schemas_never_delete() {
        let mock = MockRuntime::new();
        let apple_valid = apple_record(owner_labels());
        let docker_valid = docker_record(owner_labels());
        let cases = [
            ("container", "not json".to_string()),
            ("container", "[]".to_string()),
            ("container", "{}".to_string()),
            ("container", apple_valid.replace(NAME, "wrong-name")),
            (
                "container",
                apple_valid.replace(&format!("\"id\":\"{NAME}\""), "\"id\":null"),
            ),
            (
                "container",
                format!(
                    r#"[{{"configuration":{{"id":"{NAME}","labels":{{"{OWNER_LABEL}":"{TOKEN}","{OWNER_LABEL}":"{TOKEN}"}}}}}}]"#
                ),
            ),
            (
                "container",
                format!(r#"[{{"id":"{NAME}","labels":{{"{OWNER_LABEL}":"{TOKEN}"}}}}]"#),
            ),
            (
                "container",
                format!(
                    "[{},{}]",
                    &apple_valid[1..apple_valid.len() - 1],
                    &apple_valid[1..apple_valid.len() - 1]
                ),
            ),
            ("docker", docker_valid.replace(DOCKER_ID, "short-id")),
            ("docker", docker_valid.replace(DOCKER_ID, &"z".repeat(64))),
            ("docker", docker_valid.replace(NAME, "wrong-name")),
            (
                "docker",
                format!(
                    r#"[{{"Name":"/{NAME}","Config":{{"Labels":{{"{OWNER_LABEL}":"{TOKEN}"}}}}}}]"#
                ),
            ),
            (
                "docker",
                format!(
                    r#"[{{"Id":"{DOCKER_ID}","Name":"/{NAME}","Labels":{{"{OWNER_LABEL}":"{TOKEN}"}}}}]"#
                ),
            ),
            (
                "docker",
                format!(
                    r#"[{{"Id":"{DOCKER_ID}","Name":"/{NAME}","Config":{{"Labels":{{"{OWNER_LABEL}":"{TOKEN}","{OWNER_LABEL}":"{TOKEN}"}}}}}}]"#
                ),
            ),
        ];
        for (runtime, json) in cases {
            mock.inspect(&json);
            assert!(
                cleanup_owned_instance(&mock.identity(runtime)).is_err(),
                "{runtime}: {json}"
            );
            assert_eq!(mock.commands().lines().count(), 1, "{runtime}: {json}");
        }
        let identity = mock.identity("container");
        assert!(deletion_target(&identity, &vec![b' '; MAX_INSPECT_BYTES + 1]).is_err());
    }

    #[test]
    fn failed_inspect_and_previous_attempt_authority_never_delete() {
        let mock = MockRuntime::new();
        mock.inspect(&apple_record(owner_labels()));
        std::fs::write(mock.directory.path().join("inspect-status"), "1").unwrap();
        assert!(cleanup_owned_instance(&mock.identity("container")).is_err());
        assert_eq!(mock.commands(), format!("inspect {NAME}\n"));

        std::fs::write(mock.directory.path().join("inspect-status"), "0").unwrap();
        mock.inspect(&apple_record(owner_labels()));
        let old_attempt =
            OwnedContainerIdentity::new("container", NAME, &mock.binary, "/usr/bin:/bin").unwrap();
        assert!(cleanup_owned_instance(&old_attempt).is_err());
        assert_eq!(mock.commands(), format!("inspect {NAME}\n"));
    }

    #[test]
    fn confirmed_auto_removed_container_is_success_but_unknown_inspect_failure_is_not() {
        let mock = MockRuntime::new();
        std::fs::write(mock.directory.path().join("inspect-status"), "1").unwrap();
        for (runtime, diagnostic, success) in [
            (
                "container",
                format!("Error: notFound: container not found: {NAME}\n"),
                true,
            ),
            (
                "container",
                format!("Error: container not found: {NAME}\n"),
                true,
            ),
            (
                "docker",
                format!("Error response from daemon: No such container: {NAME}\n"),
                true,
            ),
            ("docker", format!("Error: No such object: {NAME}\n"), true),
            (
                "docker",
                "Error response from daemon: No such container: other-name\n".into(),
                false,
            ),
            (
                "container",
                "Error: connection to service failed\n".into(),
                false,
            ),
            (
                "container",
                format!("Error: permission denied inspecting {NAME}\n"),
                false,
            ),
            (
                "docker",
                format!("Error: socket for {NAME} not found\n"),
                false,
            ),
            (
                "docker",
                format!("Cannot connect to daemon: No such container: {NAME}\n"),
                false,
            ),
            (
                "docker",
                format!(
                    "permission denied\nError response from daemon: No such container: {NAME}\n"
                ),
                false,
            ),
        ] {
            mock.inspect("[]");
            std::fs::write(mock.directory.path().join("inspect-error"), diagnostic).unwrap();
            let result = cleanup_owned_instance(&mock.identity(runtime));
            assert_eq!(result.is_ok(), success);
            assert_eq!(mock.commands().lines().count(), 1);
        }
    }

    #[test]
    fn inspect_errors_keep_bounded_runtime_diagnostics() {
        let mock = MockRuntime::new();
        mock.inspect("[]");
        std::fs::write(mock.directory.path().join("inspect-status"), "7").unwrap();
        std::fs::write(
            mock.directory.path().join("inspect-error"),
            "runtime service not found\n",
        )
        .unwrap();
        let error = cleanup_owned_instance(&mock.identity("container")).unwrap_err();
        assert!(error.contains("runtime service not found"));
        assert!(error.contains('7'));
        assert_eq!(mock.commands(), format!("inspect {NAME}\n"));
    }

    #[test]
    fn cleanup_uses_captured_binary_and_never_touches_orbstack_machines() {
        let mock = MockRuntime::new();
        mock.inspect(&docker_record(owner_labels()));
        let competing = mock.directory.path().join("docker");
        std::fs::write(&competing, "#!/bin/sh\nexit 99\n").unwrap();
        std::fs::set_permissions(&competing, std::fs::Permissions::from_mode(0o700)).unwrap();
        let mut identity = OwnedContainerIdentity::with_token(
            "docker",
            NAME,
            &mock.binary,
            mock.directory.path().to_str().unwrap(),
            TOKEN,
        )
        .unwrap();
        identity.docker_endpoint =
            Some(DockerEndpoint::local("unix:///captured/docker.sock").unwrap());
        cleanup_owned_instance(&identity).unwrap();
        assert_eq!(
            mock.commands(),
            format!(
                "--host unix:///captured/docker.sock container inspect {NAME}\n--host unix:///captured/docker.sock rm --force -- {DOCKER_ID}\n"
            )
        );

        mock.inspect("not json");
        cleanup_owned_instance(&mock.identity("orbstack")).unwrap();
        assert!(mock.commands().is_empty());
    }
}

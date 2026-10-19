//! Pure action authority shared by inventory, confirmation and the command worker.

use std::path::{Path, PathBuf};
use std::process::Command;

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct DockerEndpoint(String);

impl DockerEndpoint {
    pub(crate) fn local(host: &str) -> Result<Self, String> {
        let socket = host.strip_prefix("unix://").ok_or_else(|| {
            "Delete requires a captured local Unix Docker endpoint; remote/TLS endpoints are not supported for deletion.".to_string()
        })?;
        if !Path::new(socket).is_absolute() || host.chars().any(char::is_control) {
            return Err("Docker endpoint must name an absolute Unix socket.".into());
        }
        Ok(Self(host.into()))
    }

    pub(crate) fn host(&self) -> &str {
        &self.0
    }

    pub(crate) fn pin_environment(command: &mut Command) {
        for name in [
            "DOCKER_CONTEXT",
            "DOCKER_HOST",
            "DOCKER_TLS",
            "DOCKER_TLS_VERIFY",
            "DOCKER_CERT_PATH",
        ] {
            command.env_remove(name);
        }
    }
}

pub(crate) fn capture_docker_endpoint(
    context_override: Option<&str>,
    host_override: Option<&str>,
    mut probe: impl FnMut(&[&str]) -> Result<String, String>,
) -> Result<DockerEndpoint, String> {
    let context_override = context_override.filter(|value| !value.is_empty());
    if context_override.is_none() {
        if let Some(host) = host_override.filter(|value| !value.is_empty()) {
            return DockerEndpoint::local(host);
        }
    }
    let context = match context_override {
        Some(context) => context.to_string(),
        None => probe(&["context", "show"])?.trim().to_string(),
    };
    if context.is_empty() || context.starts_with('-') || context.chars().any(char::is_control) {
        return Err("Docker context identity is unavailable.".into());
    }
    let host = probe(&[
        "context",
        "inspect",
        "--format",
        "{{.Endpoints.docker.Host}}",
        &context,
    ])?;
    DockerEndpoint::local(host.trim())
}

pub(crate) fn unpinned_resource_delete_error(runtime: &str) -> Option<&'static str> {
    matches!(runtime.trim().to_ascii_lowercase().as_str(), "docker" | "orb" | "orbstack")
        .then_some("Docker image and volume deletion is disabled because no endpoint-bound resource identity was captured. Verify the endpoint and resource in Docker's own tools; Cocoa-Way will not delete through the mutable current context.")
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct DockerDeleteTarget {
    executable: PathBuf,
    endpoint: DockerEndpoint,
    id: String,
}

impl DockerDeleteTarget {
    pub(crate) fn capture(
        executable: &Path,
        endpoint: DockerEndpoint,
        id: &str,
    ) -> Result<Self, String> {
        if !executable.is_absolute() {
            return Err("Delete requires a captured absolute Docker executable.".into());
        }
        if id.len() != 64 || !id.bytes().all(|byte| byte.is_ascii_hexdigit()) {
            return Err("Delete requires the full immutable Docker container ID.".into());
        }
        Ok(Self {
            executable: executable.into(),
            endpoint,
            id: id.into(),
        })
    }

    pub(crate) fn endpoint(&self) -> &str {
        self.endpoint.host()
    }

    pub(crate) fn id(&self) -> &str {
        &self.id
    }

    pub(crate) fn command(&self) -> Command {
        self.operation_command("delete")
            .expect("delete is an allowed container operation")
    }

    pub(crate) fn operation_command(&self, operation: &str) -> Result<Command, String> {
        let mut command = Command::new(&self.executable);
        DockerEndpoint::pin_environment(&mut command);
        command.args(["--host", self.endpoint()]);
        match operation {
            "delete" => {
                command.args(["rm", "--force", "--", self.id()]);
            }
            "start" | "stop" | "restart" => {
                command.args([operation, "--", self.id()]);
            }
            _ => return Err("Unsupported captured Docker operation.".into()),
        }
        Ok(command)
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct StoppableProfile<Id> {
    pub(crate) id: Id,
    pub(crate) name: String,
    pub(crate) runtime: String,
    pub(crate) pending: bool,
}

// Deliberately has no configuration loader or validation-state dependency.
pub(crate) fn resolve_owned_stop<'a, Id: Eq>(
    id: &Id,
    owned: &'a [StoppableProfile<Id>],
) -> Option<&'a StoppableProfile<Id>> {
    owned.iter().find(|profile| &profile.id == id)
}

#[cfg(test)]
mod tests {
    use super::*;

    const ID: &str = "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef";

    #[test]
    fn confirmation_target_survives_context_switch_and_same_name_replacement() {
        let endpoint_a = DockerEndpoint::local("unix:///context-a/docker.sock").unwrap();
        let confirmed =
            DockerDeleteTarget::capture(Path::new("/captured/docker"), endpoint_a, ID).unwrap();
        let _current_context = "context-b";
        let _same_name_on_b = "fedcba9876543210".repeat(4);
        let command = confirmed.command();
        assert_eq!(command.get_program(), "/captured/docker");
        assert_eq!(
            command.get_args().collect::<Vec<_>>(),
            [
                "--host",
                "unix:///context-a/docker.sock",
                "rm",
                "--force",
                "--",
                ID
            ]
        );
        for name in [
            "DOCKER_CONTEXT",
            "DOCKER_HOST",
            "DOCKER_TLS",
            "DOCKER_TLS_VERIFY",
            "DOCKER_CERT_PATH",
        ] {
            assert!(
                command
                    .get_envs()
                    .any(|(key, value)| key == name && value.is_none())
            );
        }
    }

    #[test]
    fn deletion_has_no_short_id_name_endpoint_or_executable_fallback() {
        for host in [
            "",
            "unix://relative",
            "unix:///tmp/socket\n",
            "tcp://remote:2376",
            "ssh://server",
        ] {
            assert!(DockerEndpoint::local(host).is_err());
        }
        for id in ["", "my-container", "0123456789ab", &"g".repeat(64)] {
            assert!(
                DockerDeleteTarget::capture(
                    Path::new("/docker"),
                    DockerEndpoint::local("unix:///socket").unwrap(),
                    id
                )
                .is_err()
            );
        }
        assert!(
            DockerDeleteTarget::capture(
                Path::new("docker"),
                DockerEndpoint::local("unix:///socket").unwrap(),
                ID
            )
            .is_err()
        );
    }

    #[test]
    fn tracked_stop_survives_bad_validation_rename_and_malformed_configuration() {
        let owned = vec![StoppableProfile {
            id: 7,
            name: "Name at launch".into(),
            runtime: "container".into(),
            pending: false,
        }];
        let _validation_state = "Invalid";
        let _configuration: Result<(), &str> = Err("malformed TOML");
        let _renamed_profile = "New name";
        assert_eq!(
            resolve_owned_stop(&7, &owned).unwrap().name,
            "Name at launch"
        );
        assert!(resolve_owned_stop(&8, &owned).is_none());
    }

    #[test]
    fn pending_launch_can_be_cancelled_without_a_saved_profile() {
        let owned = vec![StoppableProfile {
            id: 9,
            name: "Pending launch".into(),
            runtime: "docker".into(),
            pending: true,
        }];
        assert!(resolve_owned_stop(&9, &owned).unwrap().pending);
        assert!(resolve_owned_stop(&9, &[]).is_none());
    }

    #[test]
    fn image_and_volume_deletes_cannot_fall_back_to_current_docker_context() {
        for runtime in ["docker", "Docker", "orb", "orbstack"] {
            assert!(unpinned_resource_delete_error(runtime).is_some());
        }
        assert!(unpinned_resource_delete_error("apple").is_none());
    }

    #[test]
    fn endpoint_capture_obeys_context_override_and_pins_its_resolved_host() {
        let endpoint = capture_docker_endpoint(Some("a"), Some("unix:///wrong/socket"), |args| {
            assert_eq!(
                args,
                [
                    "context",
                    "inspect",
                    "--format",
                    "{{.Endpoints.docker.Host}}",
                    "a"
                ]
            );
            Ok("unix:///a/socket\n".into())
        })
        .unwrap();
        assert_eq!(endpoint.host(), "unix:///a/socket");
        assert!(capture_docker_endpoint(None, None, |_| Err("unavailable".into())).is_err());
    }

    #[test]
    fn stopping_or_restarting_runtime_containers_is_also_endpoint_bound() {
        let target = DockerDeleteTarget::capture(
            Path::new("/docker"),
            DockerEndpoint::local("unix:///a/socket").unwrap(),
            ID,
        )
        .unwrap();
        for operation in ["start", "stop", "restart"] {
            let command = target.operation_command(operation).unwrap();
            assert_eq!(
                command.get_args().collect::<Vec<_>>(),
                ["--host", "unix:///a/socket", operation, "--", ID]
            );
        }
        assert!(target.operation_command("prune").is_err());
    }
}

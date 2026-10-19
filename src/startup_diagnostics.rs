//! Explicit, bounded environment inspection. Never starts a runtime or application.

use std::collections::{BTreeMap, HashMap};
use std::process::Command;
use std::time::{Duration, Instant};

use crate::container_sessions::{self, ContainerSession};
use crate::i18n::{tr, tr_format};

pub(crate) const APPLE_SECURITY_BASELINE: (u64, u64, u64) = (1, 4, 1);
const CHECK_TIMEOUT: Duration = Duration::from_secs(3);
const REPORT_TIMEOUT: Duration = Duration::from_secs(25);
const MAX_PROFILES: usize = 24;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Status {
    Pass,
    Warning,
    Failed,
    NotChecked,
}

impl Status {
    fn label(self) -> &'static str {
        match self {
            Self::Pass => "Passed",
            Self::Warning => "Warning",
            Self::Failed => "Failed",
            Self::NotChecked => "Not checked",
        }
    }
}

struct Check {
    title: String,
    translate_title: bool,
    status: Status,
    detail: String,
    hint: &'static str,
}

#[derive(Default)]
pub(crate) struct Report {
    checks: Vec<Check>,
}

impl Report {
    pub fn push(&mut self, title: &str, status: Status, detail: String, hint: &'static str) {
        self.checks.push(Check {
            title: title.into(),
            translate_title: true,
            status,
            detail,
            hint,
        });
    }

    fn profile(&mut self, title: &str, status: Status, detail: String, hint: &'static str) {
        self.push(title, status, detail, hint);
        self.checks.last_mut().unwrap().translate_title = false;
    }

    pub fn text(&self) -> String {
        let mut lines = vec![
            format!("Cocoa-Way {}", env!("CARGO_PKG_VERSION")),
            tr("Read-only checks: no runtime, container, image download or application is started.").into_owned(),
            tr("A passed image inspection does not verify its GUI command or Guest dependencies. Use the application's Run Health Check for those checks.").into_owned(),
        ];
        for check in &self.checks {
            let title = if check.translate_title {
                tr(&check.title)
            } else {
                std::borrow::Cow::Borrowed(check.title.as_str())
            };
            lines.push(format!("\n[{}] {}", tr(check.status.label()), title));
            lines.push(check.detail.chars().take(1600).collect());
            if !check.hint.is_empty() {
                lines.push(tr(check.hint).into_owned());
            }
        }
        crate::process_output::redaction::redact(&lines.join("\n"))
            .chars()
            .take(64 * 1024)
            .collect()
    }
}

struct Probe {
    success: bool,
    stdout: String,
    stderr: String,
}

impl Probe {
    fn detail(&self) -> String {
        format!("{}\n{}", self.stdout.trim(), self.stderr.trim())
            .chars()
            .take(1600)
            .collect()
    }
}

pub(crate) fn collect() -> Report {
    let child_path = crate::runtime_paths::build_child_path();
    let path = container_sessions::config_path();
    // Parser errors can contain source excerpts with credentials. Report the error
    // category and path, never dump the source or process environment.
    let sessions = container_sessions::inspect_sessions(&path)
        .map_err(|error| format!("{}: {:?}", path.display(), error.kind()));
    let deadline = Instant::now() + REPORT_TIMEOUT;
    let mut report = collect_with(sessions, |name, args| {
        if crate::lifecycle_producers::GLOBAL.closing() {
            return Err(
                tr("Diagnostics cancelled because Cocoa-Way is shutting down.").into_owned(),
            );
        }
        let remaining = deadline.saturating_duration_since(Instant::now());
        if remaining.is_zero() {
            return Err(
                tr("Diagnostic time limit reached; rerun to retry incomplete checks.").into_owned(),
            );
        }
        let executable = crate::runtime_paths::resolve_command_path(
            name,
            name.contains('/').then_some(name),
            name,
            &child_path,
        )
        .ok_or_else(|| tr_format("Executable not found: {name}", &[("name", name)]))?;
        let mut command = Command::new(executable);
        command.env("PATH", &child_path).args(args);
        crate::runtime_commands::capture(command, CHECK_TIMEOUT.min(remaining))
            .map(|output| Probe {
                success: output.status.success(),
                stdout: String::from_utf8_lossy(&output.stdout).into_owned(),
                stderr: String::from_utf8_lossy(&output.stderr).into_owned(),
            })
            .map_err(|error| error.to_string())
    });
    report.push(
        "Host environment",
        Status::Pass,
        crate::startup::environment_report(),
        "",
    );
    report
}

fn apple(session: &ContainerSession) -> bool {
    matches!(
        session.runtime.as_str(),
        "container" | "apple" | "apple-container"
    )
}

fn version(text: &str) -> Option<(u64, u64, u64)> {
    text.split_whitespace().find_map(|word| {
        let word = word.trim_matches(|c: char| !c.is_ascii_digit() && c != '.');
        let mut parts = word.split('.');
        let result = (
            parts.next()?.parse().ok()?,
            parts.next()?.parse().ok()?,
            parts.next()?.parse().ok()?,
        );
        parts.next().is_none().then_some(result)
    })
}

fn collect_with(
    sessions: Result<Vec<ContainerSession>, String>,
    mut probe: impl FnMut(&str, &[&str]) -> Result<Probe, String>,
) -> Report {
    let mut report = Report::default();
    let sessions = match sessions {
        Ok(sessions) => {
            report.push(
                "Application configuration",
                Status::Pass,
                tr_format(
                    "{count} saved applications parsed without changing the configuration.",
                    &[("count", &sessions.len().to_string())],
                ),
                "",
            );
            sessions
        }
        Err(error) => {
            report.push("Application configuration", Status::Failed, error,
                "Correct container-sessions.toml syntax, profile IDs or file permissions, then rerun diagnostics. The configuration was not modified.");
            Vec::new()
        }
    };
    let apple_required = sessions.iter().any(apple);
    let mut versions = BTreeMap::<String, Result<Probe, String>>::new();
    let mut valid = Vec::new();
    for session in sessions.iter().take(MAX_PROFILES) {
        match container_sessions::validate_session(session) {
            Ok(()) => valid.push(session),
            Err(error) => report.profile(&session.name, Status::Failed, error.to_string(),
                "Edit this application's profile and correct the reported setting before launching it."),
        }
        let name = session
            .waypipe_path
            .as_deref()
            .filter(|path| !path.trim().is_empty())
            .unwrap_or("waypipe");
        if !versions.contains_key(name) {
            versions.insert(name.into(), probe(name, &["--version"]));
        }
    }
    if versions.is_empty() {
        versions.insert("waypipe".into(), probe("waypipe", &["--version"]));
    }
    for (name, result) in versions {
        match result {
            Ok(output) if output.success => report.push("Waypipe executable", Status::Pass,
                format!("{name}\n{}", output.detail()), ""),
            Ok(output) => report.push("Waypipe executable", Status::Failed,
                format!("{name}\n{}", output.detail()), "Check the configured Waypipe path or reinstall waypipe-darwin. A file existing is not enough; --version must run successfully."),
            Err(error) => report.push("Waypipe executable", Status::Failed, error,
                "Check the configured Waypipe path or reinstall waypipe-darwin. A file existing is not enough; --version must run successfully."),
        }
    }
    let cli = match probe("container", &["--version"]) {
        Ok(output) if output.success => {
            let parsed = version(&output.stdout);
            let status = if parsed.is_some_and(|v| v >= APPLE_SECURITY_BASELINE) {
                Status::Pass
            } else {
                Status::Warning
            };
            report.push("Apple Container version", status, output.detail(),
                if status == Status::Pass { "" } else { "Cocoa-Way's current security baseline is Apple Container 1.4.1. Older or unidentified versions need review; version alone does not prove runtime readiness." });
            parsed
        }
        result => {
            let detail = result
                .map(|output| output.detail())
                .unwrap_or_else(|error| error);
            report.push("Apple Container version", if apple_required { Status::Failed } else { Status::NotChecked }, detail,
                "Apple Container is required only for Apple Container profiles. Install the official runtime separately; Docker and remote SSH connections do not require it.");
            return report;
        }
    };
    let ready = match probe("container", &["system", "status", "--format", "json"]) {
        Ok(output) => match serde_json::from_str::<serde_json::Value>(&output.stdout) {
            Ok(status) => {
                let service = status
                    .get("status")
                    .and_then(serde_json::Value::as_str)
                    .unwrap_or("unknown");
                let ready = output.success && service == "running";
                report.push("Apple Container service", if ready { Status::Pass } else if apple_required { Status::Failed } else { Status::Warning },
                    format!("status: {service}"), if ready { "" } else { "The management service is not ready. Open Apple Container > Start System, or run `container system start`, then rerun diagnostics. This check does not start it." });
                let api = status
                    .pointer("/server/version")
                    .and_then(serde_json::Value::as_str)
                    .or_else(|| {
                        status
                            .get("apiServerVersion")
                            .and_then(serde_json::Value::as_str)
                    })
                    .and_then(version);
                if let Some((cli, api)) = cli.zip(api).filter(|(cli, api)| ready && cli != api) {
                    report.push("Client/API version mismatch", Status::Warning,
                        format!("CLI: {}.{}.{}; API: {}.{}.{}", cli.0, cli.1, cli.2, api.0, api.1, api.2),
                        "The CLI and running API server report different versions. Restart Apple Container when existing work can be stopped, then check again.");
                }
                ready
            }
            Err(_) => {
                report.push("Apple Container service", Status::Failed, output.detail(),
                    "The runtime did not return valid structured status. Check its version, permissions and diagnostic logs; do not assume an image is missing.");
                false
            }
        },
        Err(error) => {
            report.push("Apple Container service", Status::Failed, error,
                "The runtime status check failed or timed out. Check the service and permissions, then retry.");
            false
        }
    };
    match probe("container", &["run", "--help"]) {
        Ok(output) if output.success && output.stdout.contains("--publish-socket") =>
            report.push("Apple Container transport", Status::Pass, "--publish-socket".into(), ""),
        result => report.push("Apple Container transport", Status::Warning,
            result.map(|output| output.detail()).unwrap_or_else(|error| error),
            "Published sockets were not confirmed. Cocoa-Way can use its compatibility relay; Transport V2 needs --publish-socket support. This is separate from service readiness."),
    }
    let mut images = HashMap::<String, bool>::new();
    for session in valid {
        if !apple(session) || !ready {
            report.profile(&session.name, Status::NotChecked, session.image.clone(),
                "Image and Guest checks were skipped. For Apple Container, start the management service first. Docker/OrbStack daemon probes are deliberately not run here because they can wake the provider.");
            continue;
        }
        let present = *images.entry(session.image.clone()).or_insert_with(|| {
            match probe("container", &["image", "inspect", &session.image]) {
                Ok(output) if output.success => true,
                result => {
                    report.push("Application image", Status::Failed,
                        format!("{}\n{}", session.image, result.map(|output| output.detail()).unwrap_or_else(|error| error)),
                        "Image inspection failed. Check the runtime error before pulling or rebuilding; an unavailable service or permission error is not proof that the image is missing.");
                    false
                }
            }
        });
        if present {
            report.profile(&session.name, Status::Pass, session.image.clone(),
                "The local image exists. Run Health Check in Applications to validate the GUI command, Waypipe and Guest dependencies; this diagnostic does not run a Guest probe.");
        }
    }
    if sessions.len() > MAX_PROFILES {
        report.push("Application configuration", Status::NotChecked, format!("{MAX_PROFILES}/{}", sessions.len()),
            "Only the first 24 application profiles were inspected to keep diagnostics bounded. Check other profiles individually in Applications.");
    }
    report
}

#[cfg(test)]
mod tests {
    use super::*;

    fn session() -> ContainerSession {
        toml::from_str("name = 'Example'\nimage = 'local:test'\ncommand = 'foot'").unwrap()
    }

    fn output(text: &str, success: bool) -> Result<Probe, String> {
        Ok(Probe {
            success,
            stdout: text.into(),
            stderr: String::new(),
        })
    }

    #[test]
    fn stopped_service_is_not_a_missing_image_and_never_runs_guest_commands() {
        let mut calls = Vec::new();
        let report = collect_with(Ok(vec![session()]), |name, args| {
            calls.push(format!("{name} {}", args.join(" ")));
            match (name, args) {
                ("waypipe", ["--version"]) => output("waypipe 0.11.0", true),
                ("container", ["--version"]) => output("container CLI version 1.4.1", true),
                ("container", ["system", "status", "--format", "json"]) => {
                    output(r#"{"status":"unregistered"}"#, false)
                }
                ("container", ["run", "--help"]) => output("--publish-socket", true),
                _ => panic!("unexpected command: {name} {args:?}"),
            }
        });
        assert!(
            report
                .checks
                .iter()
                .any(|c| c.title == "Apple Container service" && c.status == Status::Failed)
        );
        assert!(
            !calls
                .iter()
                .any(|c| c.contains("image inspect") || c.contains("system start"))
        );
    }

    #[test]
    fn ready_service_inspects_each_local_image_once_and_checks_custom_waypipe() {
        let mut first = session();
        first.waypipe_path = Some("/custom/waypipe".into());
        let mut second = first.clone();
        second.name = "Other".into();
        let mut image_calls = 0;
        let report = collect_with(Ok(vec![first, second]), |name, args| match (name, args) {
            ("/custom/waypipe", ["--version"]) => output("waypipe 0.11.0", true),
            ("container", ["--version"]) => output("container 1.4.2", true),
            ("container", ["system", "status", "--format", "json"]) => {
                output(r#"{"status":"running","server":{"version":"1.4.1"}}"#, true)
            }
            ("container", ["run", "--help"]) => output("--publish-socket", true),
            ("container", ["image", "inspect", "local:test"]) => {
                image_calls += 1;
                output("{}", true)
            }
            _ => panic!("unexpected command: {name} {args:?}"),
        });
        assert_eq!(image_calls, 1);
        assert!(
            report
                .checks
                .iter()
                .any(|c| c.title == "Client/API version mismatch")
        );
        assert_eq!(
            report
                .checks
                .iter()
                .filter(
                    |c| c.status == Status::Pass && matches!(c.title.as_str(), "Example" | "Other")
                )
                .count(),
            2
        );
    }

    #[test]
    fn malformed_configuration_and_missing_runtime_have_distinct_results() {
        let report = collect_with(Err("profile configuration: InvalidData".into()), |_, _| {
            Err("not installed".into())
        });
        assert!(
            report
                .checks
                .iter()
                .any(|c| c.title == "Application configuration" && c.status == Status::Failed)
        );
        assert!(
            report
                .checks
                .iter()
                .any(|c| c.title == "Apple Container version" && c.status == Status::NotChecked)
        );
    }

    #[test]
    fn docker_profiles_never_trigger_provider_daemon_or_guest_probes() {
        let mut profile = session();
        profile.runtime = "docker".into();
        let mut calls = Vec::new();
        let report = collect_with(Ok(vec![profile]), |name, args| {
            calls.push(format!("{name} {}", args.join(" ")));
            match (name, args) {
                ("waypipe", ["--version"]) => output("waypipe 0.11.0", true),
                ("container", ["--version"]) => Err("not installed".into()),
                _ => panic!("unexpected provider command: {name} {args:?}"),
            }
        });
        assert_eq!(calls.len(), 2);
        assert!(
            !report
                .checks
                .iter()
                .any(|check| check.status == Status::Failed)
        );
    }

    #[test]
    fn profile_titles_remain_user_data_even_when_they_match_translation_keys() {
        let mut report = Report::default();
        report.profile("Applications", Status::Pass, "local:test".into(), "");
        assert!(!report.checks[0].translate_title);
        assert!(report.text().contains("Applications"));
    }

    #[test]
    fn version_parser_does_not_treat_unknown_text_as_a_supported_version() {
        assert_eq!(
            version("container CLI version 1.4.1 (build)"),
            Some((1, 4, 1))
        );
        assert_eq!(version("v1.1.0"), Some((1, 1, 0)));
        assert_eq!(version("version unknown"), None);
        assert_eq!(version("1.4"), None);
    }

    #[test]
    fn rendering_a_report_bounds_details_and_redacts_credentials() {
        let mut report = Report::default();
        report.push(
            "Example",
            Status::Failed,
            format!("password=hidden {}", "x".repeat(8000)),
            "",
        );
        let text = report.text();
        assert!(!text.contains("hidden"));
        assert!(text.len() < 3000);
        assert!(text.contains("Example"));
    }
}

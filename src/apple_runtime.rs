//! Shared read-only Apple Container probes. Execution and UI caching stay outside
//! this module; capability support does not imply that the service is running.

use serde_json::Value;

pub(crate) type Version = (u64, u64, u64);
pub(crate) const MAX_VERSION_OUTPUT: usize = 4096;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Probe {
    Version,
    SystemStatus,
    RunHelp,
    StatsHelp,
    BuildHelp,
}

impl Probe {
    pub(crate) const fn args(self) -> &'static [&'static str] {
        match self {
            Self::Version => &["--version"],
            Self::SystemStatus => &["system", "status", "--format", "json"],
            Self::RunHelp => &["run", "--help"],
            Self::StatsHelp => &["stats", "--help"],
            Self::BuildHelp => &["build", "--help"],
        }
    }
}

pub(crate) fn version_text(version: Version) -> String {
    format!("{}.{}.{}", version.0, version.1, version.2)
}

pub(crate) fn parse_version(text: &str) -> Option<Version> {
    if text.len() > 128 {
        return None;
    }
    let text = text.strip_prefix('v').unwrap_or(text);
    let (core, metadata) = text.split_once('+').unwrap_or((text, ""));
    if text.contains('+')
        && (metadata.is_empty()
            || metadata.split('.').any(|part| {
                part.is_empty()
                    || !part
                        .bytes()
                        .all(|byte| byte.is_ascii_alphanumeric() || byte == b'-')
            }))
    {
        return None;
    }
    let mut parts = core.split('.');
    let mut number = || {
        let part = parts.next()?;
        if part.is_empty()
            || (part.len() > 1 && part.starts_with('0'))
            || !part.bytes().all(|byte| byte.is_ascii_digit())
        {
            return None;
        }
        part.parse().ok()
    };
    let version = (number()?, number()?, number()?);
    parts.next().is_none().then_some(version)
}

fn version_line(text: &str) -> Option<Version> {
    let (version, suffix) = text.split_once(char::is_whitespace).unwrap_or((text, ""));
    let suffix = suffix.trim();
    if !suffix.is_empty()
        && !(suffix.starts_with('(')
            && suffix.ends_with(')')
            && suffix
                .bytes()
                .all(|byte| byte.is_ascii_graphic() || byte == b' '))
    {
        return None;
    }
    parse_version(version)
}

pub(crate) fn confirmed_cli_version(success: bool, stdout: &[u8]) -> Option<Version> {
    if !success || stdout.len() > MAX_VERSION_OUTPUT {
        return None;
    }
    let text = std::str::from_utf8(stdout).ok()?.trim();
    if text.lines().count() != 1 {
        return None;
    }
    let version = text
        .strip_prefix("container CLI version ")
        .or_else(|| text.strip_prefix("container version "))?;
    version_line(version)
}

pub(crate) fn reported_version(text: &str) -> Option<Version> {
    if text.len() > MAX_VERSION_OUTPUT {
        return None;
    }
    let text = text.trim();
    if text.lines().count() != 1 {
        return None;
    }
    let version = text
        .strip_prefix("container-apiserver version ")
        .or_else(|| text.strip_prefix("container CLI version "))
        .or_else(|| text.strip_prefix("container version "))
        .unwrap_or(text);
    version_line(version)
}

pub(crate) fn api_version(status: &Value) -> Option<Version> {
    status
        .pointer("/server/version")
        .and_then(Value::as_str)
        .and_then(reported_version)
        .or_else(|| {
            status
                .get("apiServerVersion")
                .and_then(Value::as_str)
                .and_then(reported_version)
        })
}

pub(crate) fn has_option(help: &str, option: &str) -> bool {
    help.lines().any(|line| {
        let declaration = line.trim_start();
        declaration.starts_with('-')
            && declaration
                .split_whitespace()
                .take_while(|word| word.starts_with('-'))
                .any(|word| word.trim_end_matches(',') == option)
    })
}

pub(crate) fn build_supported(help: &str) -> bool {
    ["--progress", "--file", "--tag"]
        .iter()
        .all(|option| has_option(help, option))
}

pub(crate) fn stats_json_supported(help: &str) -> bool {
    has_option(help, "--format") && has_option(help, "--no-stream")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn probes_only_encode_read_only_commands() {
        assert_eq!(Probe::Version.args(), ["--version"]);
        assert_eq!(
            Probe::SystemStatus.args(),
            ["system", "status", "--format", "json"]
        );
        assert_eq!(Probe::RunHelp.args(), ["run", "--help"]);
        assert_eq!(Probe::StatsHelp.args(), ["stats", "--help"]);
        assert_eq!(Probe::BuildHelp.args(), ["build", "--help"]);
    }

    #[test]
    fn cli_versions_are_not_guessed_from_builds_or_warnings() {
        for text in [
            "build version 1.6.0",
            "warning: container CLI version 1.6.0",
            "container CLI version unknown (build 1.6.0)",
            "container CLI version 1.6.0-rc.1",
            "container CLI version 1.6.0\nwarning",
            "container CLI version 01.6.0",
        ] {
            assert_eq!(confirmed_cli_version(true, text.as_bytes()), None, "{text}");
        }
        assert_eq!(
            confirmed_cli_version(false, b"container CLI version 1.6.0"),
            None
        );
        assert_eq!(
            confirmed_cli_version(
                true,
                b"container CLI version 1.6.0 (build: release, commit: ca4884f)\n"
            ),
            Some((1, 6, 0))
        );
        assert_eq!(
            confirmed_cli_version(true, b"container version v1.1.0+build.12"),
            Some((1, 1, 0))
        );
    }

    #[test]
    fn status_versions_use_server_metadata_not_the_client() {
        for (status, expected) in [
            (
                serde_json::json!({"status":"running", "apiServerVersion":"container-apiserver version 1.0.0"}),
                Some((1, 0, 0)),
            ),
            (
                serde_json::json!({"status":"running", "client":{"version":"1.6.0"}, "server":{"version":"1.5.0"}}),
                Some((1, 5, 0)),
            ),
            (
                serde_json::json!({"status":"unregistered", "server":null}),
                None,
            ),
            (
                serde_json::json!({"status":"running", "server":{"version":"unknown"}, "apiServerVersion":"1.1.0"}),
                Some((1, 1, 0)),
            ),
        ] {
            assert_eq!(api_version(&status), expected);
        }
        for invalid in [
            "unknown build 1.6.0",
            "1.6.0.9",
            "1.6.0-rc1",
            "1.6.0\n1.5.0",
        ] {
            assert_eq!(reported_version(invalid), None);
        }
    }

    #[test]
    fn options_require_exact_declared_names_not_descriptions_or_prefixes() {
        let help = "USAGE: tool [--publish-socket]\n  --publish-socket-file <path>  Not the same flag\n  --other <value>  Mentions --publish-socket and --format\n  --another  Disable --publish-socket\n";
        assert!(!has_option(help, "--publish-socket"));
        assert!(!stats_json_supported(help));
        assert!(has_option(
            "  --publish-socket <spec>  Publish a socket",
            "--publish-socket"
        ));
        assert!(has_option("  -w, --workdir, --cwd <dir>\n", "--cwd"));
        assert!(stats_json_supported(
            "  --format <format> Format\n  --no-stream Disable streaming\n"
        ));
        assert!(build_supported(
            "  --progress <type>\n  -f, --file <path>\n  -t, --tag <tag>\n"
        ));
        assert!(!build_supported(
            "  --progress <type>\n  --file <file>\n description --tag\n"
        ));
    }
}

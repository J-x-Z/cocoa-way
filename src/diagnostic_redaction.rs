//! Best-effort diagnostic sanitization, never a transformation for commands or IPC.
//!
//! Prefer `environment` for environment dumps and `Redactor::with_secrets` when
//! the caller knows credentials. Pattern matching cannot recognize opaque secrets
//! printed without a label. Keep raw configuration/credentials out of diagnostics.

const MASK: &str = "[REDACTED]";

pub fn sensitive_key(key: &str) -> bool {
    let key = key
        .chars()
        .filter(|c| c.is_ascii_alphanumeric())
        .flat_map(char::to_lowercase)
        .collect::<String>();
    [
        "password",
        "passwd",
        "secret",
        "token",
        "credential",
        "authorization",
        "apikey",
        "privatekey",
        "accesskey",
        "clientkey",
        "cookie",
        "username",
    ]
    .iter()
    .any(|part| key.contains(part))
        || matches!(key.as_str(), "pwd" | "auth" | "user" | "orgcocoawayowner")
        || key.starts_with("cocoawaycontainerowner")
}

/// Environment values may be secrets even when the variable has an innocent name.
pub fn environment(entry: &str) -> String {
    let key = entry.split_once('=').map_or(entry, |(key, _)| key);
    format!("{}={MASK}", redact(key))
}

pub fn field(key: &str, value: &str) -> String {
    if sensitive_key(key) {
        MASK.into()
    } else {
        redact(value)
    }
}

pub fn redact(text: &str) -> String {
    Redactor::default().redact(text)
}

/// Sanitize parsed inspect/configuration diagnostics before pretty-printing or
/// truncating them. Environment containers are intentionally masked wholesale.
pub fn json(value: &mut serde_json::Value) {
    json_with_secrets(value, &[]);
}

fn json_with_secrets(value: &mut serde_json::Value, secrets: &[String]) {
    match value {
        serde_json::Value::Object(fields) => {
            for (key, value) in fields {
                let normalized = key.to_ascii_lowercase().replace(['_', '-'], "");
                if sensitive_key(key)
                    || matches!(
                        normalized.as_str(),
                        "env"
                            | "envs"
                            | "environment"
                            | "environmentvariables"
                            | "command"
                            | "cmd"
                            | "args"
                            | "arguments"
                            | "entrypoint"
                    )
                {
                    *value = serde_json::Value::String(MASK.into());
                } else {
                    json_with_secrets(value, secrets);
                }
            }
        }
        serde_json::Value::Array(values) => {
            for value in values {
                json_with_secrets(value, secrets);
            }
        }
        serde_json::Value::String(text) => {
            *text = Redactor::with_secrets(secrets.iter().cloned()).redact(text);
        }
        _ => {}
    }
}

/// A diagnostic view only: never feed this back into command/configuration parsers.
/// Parse structured output before line/width truncation so Env arrays cannot leak.
pub fn output(bytes: &[u8]) -> String {
    output_with_secrets(bytes, &[])
}

pub fn output_with_secrets(bytes: &[u8], secrets: &[String]) -> String {
    if let Ok(mut value) = serde_json::from_slice::<serde_json::Value>(bytes) {
        json_with_secrets(&mut value, secrets);
        serde_json::to_string_pretty(&value).unwrap_or_else(|_| MASK.into())
    } else {
        Redactor::with_secrets(secrets.iter().cloned()).redact(&String::from_utf8_lossy(bytes))
    }
}

/// Logging writers often truncate before finalizing a record. Discard the whole
/// record at the limit: a truncated suffix could have identified URL credentials.
pub fn record(bytes: &[u8], limit: usize) -> Vec<u8> {
    if bytes.len() >= limit {
        b"[diagnostic record truncated]\n".to_vec()
    } else {
        output(bytes).into_bytes()
    }
}

/// State spans lines so PEM bodies cannot escape a line-oriented output reader.
#[derive(Default)]
pub struct Redactor {
    secrets: Vec<String>,
    private_key: bool,
}

impl Redactor {
    pub fn with_secrets(secrets: impl IntoIterator<Item = String>) -> Self {
        let mut secrets = secrets
            .into_iter()
            .filter(|s| !s.is_empty())
            .collect::<Vec<_>>();
        // Replace longer credentials first when values overlap.
        secrets.sort_by_key(|secret| std::cmp::Reverse(secret.len()));
        Self {
            secrets,
            private_key: false,
        }
    }

    pub fn redact(&mut self, text: &str) -> String {
        let text = structured_spans(text, &self.secrets);
        let mut output = String::new();
        for line in text.split_inclusive('\n') {
            let newline = if line.ends_with('\n') { "\n" } else { "" };
            let begin = line.contains("-----BEGIN ") && line.contains("PRIVATE KEY-----");
            if self.private_key || begin {
                self.private_key =
                    !(line.contains("-----END ") && line.contains("PRIVATE KEY-----"));
                output.push_str(MASK);
                output.push_str(newline);
                continue;
            }
            let mut line = line.to_string();
            for secret in &self.secrets {
                line = line.replace(secret, MASK);
            }
            output.push_str(&redact_patterns(&line));
        }
        output
    }
}

fn structured_spans(text: &str, secrets: &[String]) -> String {
    let mut output = String::new();
    let mut copied = 0;
    let mut index = 0;
    let bytes = text.as_bytes();
    let mut attempts = 0;
    while index < bytes.len() {
        if !matches!(bytes[index], b'{' | b'[') {
            index += 1;
            continue;
        }
        let tail = text[index + 1..].trim_start_matches(|c: char| c.is_ascii_whitespace());
        let next = tail.as_bytes().first().copied();
        let plausible = if bytes[index] == b'{' {
            matches!(next, Some(b'"' | b'}'))
        } else {
            matches!(next, Some(b'"' | b'{' | b'[' | b']')) || json_array_scalar_prefix(tail)
        };
        if !plausible {
            index += 1;
            continue;
        }
        output.push_str(&text[copied..index]);
        attempts += 1;
        if attempts > 64 {
            output.push_str("[structured diagnostic omitted]");
            return output;
        }
        let mut stream =
            serde_json::Deserializer::from_str(&text[index..]).into_iter::<serde_json::Value>();
        match stream.next() {
            Some(Ok(mut value)) => {
                json_with_secrets(&mut value, secrets);
                output.push_str(&serde_json::to_string(&value).unwrap_or_else(|_| MASK.into()));
                index += stream.byte_offset();
                copied = index;
            }
            _ => {
                // Do not export an incomplete inspect/configuration object as
                // raw text: an innocent Env key can contain arbitrary secrets.
                output.push_str("[unparseable structured diagnostic omitted]");
                return output;
            }
        }
    }
    output.push_str(&text[copied..]);
    output
}

fn json_array_scalar_prefix(tail: &str) -> bool {
    if !matches!(
        tail.as_bytes().first(),
        Some(b'0'..=b'9' | b'-' | b't' | b'f' | b'n')
    ) {
        return false;
    }
    // Source labels such as [test], [niri::dbus] and [12:34] are not JSON.
    // Require a complete scalar token and an array delimiter before failing
    // closed on malformed structured output farther along in the record.
    let mut scalar = serde_json::Deserializer::from_str(tail).into_iter::<serde_json::Value>();
    matches!(
        scalar.next(),
        Some(Ok(serde_json::Value::Bool(_)
            | serde_json::Value::Null
            | serde_json::Value::Number(_)))
    ) && matches!(
        tail[scalar.byte_offset()..]
            .trim_start_matches(|c: char| c.is_ascii_whitespace())
            .as_bytes()
            .first(),
        Some(b',' | b']')
    )
}

fn identifier(byte: u8) -> bool {
    byte.is_ascii_alphanumeric() || matches!(byte, b'_' | b'-' | b'.')
}

fn quoted_end(bytes: &[u8], start: usize) -> usize {
    let quote = bytes[start];
    let mut index = start + 1;
    while index < bytes.len() {
        if bytes[index] == b'\\' {
            index = (index + 2).min(bytes.len());
        } else if bytes[index] == quote {
            return index + 1;
        } else {
            index += 1;
        }
    }
    bytes.len()
}

fn redact_patterns(text: &str) -> String {
    let bytes = text.as_bytes();
    let mut output = String::new();
    let mut index = 0;
    let mut copied = 0;
    while index < bytes.len() {
        // Preserve the URL host/path, but never its userinfo (including usernames).
        if bytes[index..].starts_with(b"://") {
            let start = index + 3;
            let end = (start..bytes.len())
                .find(|&i| {
                    matches!(bytes[i], b'/' | b'?' | b'#' | b'\'' | b'"')
                        || bytes[i].is_ascii_whitespace()
                })
                .unwrap_or(bytes.len());
            if let Some(at) = (start..end).rfind(|&i| bytes[i] == b'@') {
                output.push_str(&text[copied..start]);
                output.push_str(MASK);
                copied = at;
            }
            index = end;
            continue;
        }
        if !identifier(bytes[index]) {
            index += 1;
            continue;
        }
        let key_start = index;
        while index < bytes.len() && identifier(bytes[index]) {
            index += 1;
        }
        if !sensitive_key(&text[key_start..index]) {
            continue;
        }
        let key_end = index;
        let mut start = index;
        if start < bytes.len() && matches!(bytes[start], b'\'' | b'"') {
            start += 1;
        }
        while start < bytes.len() && matches!(bytes[start], b' ' | b'\t') {
            start += 1;
        }
        if start < bytes.len() && matches!(bytes[start], b'=' | b':') {
            start += 1;
        } else if !text[key_start..key_end].starts_with("--") {
            continue;
        }
        while start < bytes.len() && matches!(bytes[start], b' ' | b'\t') {
            start += 1;
        }
        if start == bytes.len() || matches!(bytes[start], b'\n' | b'\r') {
            continue;
        }
        let key = text[key_start..key_end].to_ascii_lowercase();
        let authorization = key.contains("authorization") || key.contains("cookie");
        let end = if bytes[start..].starts_with(MASK.as_bytes()) {
            start + MASK.len()
        } else if matches!(bytes[start], b'\'' | b'"') {
            quoted_end(bytes, start)
        } else if authorization {
            (start..bytes.len())
                .find(|&i| matches!(bytes[i], b'\n' | b'\r'))
                .unwrap_or(bytes.len())
        } else {
            (start..bytes.len())
                .find(|&i| {
                    bytes[i].is_ascii_whitespace()
                        || matches!(bytes[i], b'&' | b',' | b';' | b'}' | b']' | b'\'' | b'"')
                })
                .unwrap_or(bytes.len())
        };
        output.push_str(&text[copied..start]);
        // Preserve JSON/quoted diagnostic syntax without retaining any value.
        let quote = matches!(bytes[start], b'\'' | b'"').then_some(bytes[start] as char);
        if let Some(quote) = quote {
            output.push(quote);
        }
        output.push_str(MASK);
        if let Some(quote) = quote {
            output.push(quote);
        }
        copied = end;
        index = end;
    }
    output.push_str(&text[copied..]);
    output
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn covers_env_flags_json_headers_and_proxy_urls() {
        for (input, forbidden) in [
            ("API_KEY=env-secret DEBUG=1", "env-secret"),
            ("--password 'spaces and \\'quotes' --verbose", "spaces"),
            (
                r#"{"client_secret":"json-secret","port":80}"#,
                "json-secret",
            ),
            (
                "Authorization: Bearer bearer-secret\nstatus: ok",
                "bearer-secret",
            ),
            (
                "Proxy-Authorization: Basic encoded-secret",
                "encoded-secret",
            ),
            (
                "HTTPS_PROXY=http://alice:p%40ss@localhost:8080/path",
                "alice",
            ),
            (
                "https://host/path?access_token=query-secret&ok=1",
                "query-secret",
            ),
            ("--registry-token=flag-secret", "flag-secret"),
            ("COOKIE=session=secret", "session=secret"),
            ("Cookie: first=one; second=two", "second=two"),
            ("--user=alice:password", "alice"),
        ] {
            let output = redact(input);
            assert!(!output.contains(forbidden), "{output}");
            assert!(output.contains(MASK), "{output}");
        }
        assert!(redact("http://u:p@host:80/path").contains("host:80/path"));
    }

    #[test]
    fn environment_is_fail_closed_and_structured_fields_are_selective() {
        assert_eq!(
            environment("ORDINARY_NAME=opaque"),
            "ORDINARY_NAME=[REDACTED]"
        );
        assert_eq!(field("registry_password", "opaque"), MASK);
        assert_eq!(field("state", "ready"), "ready");
        assert_eq!(
            redact("session 7: ready; путь /tmp/socket"),
            "session 7: ready; путь /tmp/socket"
        );
    }

    #[test]
    fn pem_redaction_survives_separate_lines_and_multiple_secrets() {
        let mut redactor = Redactor::with_secrets(["opaque".into(), "opaque-long".into()]);
        assert_eq!(
            redactor.redact("opaque-long opaque"),
            "[REDACTED] [REDACTED]"
        );
        for line in [
            "-----BEGIN RSA PRIVATE KEY-----",
            "base64body",
            "-----END RSA PRIVATE KEY-----",
        ] {
            assert_eq!(redactor.redact(line), MASK);
        }
        assert_eq!(redactor.redact("ready"), "ready");
        assert!(
            !redact("-----BEGIN PRIVATE KEY-----\nbody\n-----END PRIVATE KEY-----\nready")
                .contains("body")
        );
    }

    #[test]
    fn safe_text_and_utf8_remain_valid_and_redaction_is_idempotent() {
        for input in [
            "",
            "TOKEN=секрет",
            "token=",
            "https://host",
            "PASSWORD=\"unterminated",
            "auth=one&token=two",
        ] {
            let output = redact(input);
            assert_eq!(redact(&output), output);
        }
    }

    #[test]
    fn structured_inspect_masks_all_environment_values_and_nested_credentials() {
        let mut value = serde_json::json!({
            "Config": {"Env": ["ORDINARY=opaque"], "User": "alice"},
            "registry": {"auths": {"host": {"auth": "encoded", "Password": "secret"}}},
            "state": "running", "address": "http://user:pass@localhost/path",
        });
        json(&mut value);
        let output = value.to_string();
        for secret in ["opaque", "alice", "encoded", "secret", "user:pass"] {
            assert!(!output.contains(secret), "{output}");
        }
        assert_eq!(value["state"], "running");
        assert_eq!(value["address"], "http://[REDACTED]@localhost/path");
    }

    #[test]
    fn cleanup_capabilities_are_redacted_but_ordinary_owner_ids_remain_visible() {
        let raw = serde_json::json!({
            "Labels": {"org.cocoa-way.owner": "cleanup-capability"},
            "COCOA_WAY_CONTAINER_OWNER": "owner-capability",
            "COCOA_WAY_CONTAINER_OWNER_TOKEN": "container-capability",
            "COCOA_WAY_CONTAINER_OWNER_NONCE": "nonce-capability",
            "COCOA_WAY_RELAY_OWNER_TOKEN": "relay-capability",
            "owner": "local-account",
            "OwnerUID": 501,
            "owner_uid": 502,
            "org.cocoa-way.owner.uid": 503,
        });
        let sanitized: serde_json::Value =
            serde_json::from_str(&output(raw.to_string().as_bytes())).unwrap();
        assert_eq!(sanitized["Labels"]["org.cocoa-way.owner"], MASK);
        for key in [
            "COCOA_WAY_CONTAINER_OWNER",
            "COCOA_WAY_CONTAINER_OWNER_TOKEN",
            "COCOA_WAY_CONTAINER_OWNER_NONCE",
            "COCOA_WAY_RELAY_OWNER_TOKEN",
        ] {
            assert_eq!(sanitized[key], MASK);
        }
        for key in ["owner", "OwnerUID", "owner_uid", "org.cocoa-way.owner.uid"] {
            assert_eq!(sanitized[key], raw[key]);
        }
        for input in [
            "--label org.cocoa-way.owner=cleanup-capability",
            "COCOA_WAY_CONTAINER_OWNER=owner-capability",
            "COCOA_WAY_CONTAINER_OWNER_NONCE=nonce-capability",
        ] {
            let sanitized = redact(input);
            assert!(!sanitized.contains("capability"), "{sanitized}");
            assert!(sanitized.contains(MASK), "{sanitized}");
        }
        assert_eq!(
            redact("owner_uid=501 owner=local-account"),
            "owner_uid=501 owner=local-account"
        );
        assert_eq!(raw["Labels"]["org.cocoa-way.owner"], "cleanup-capability");
    }

    #[test]
    fn truncated_records_are_fail_closed_and_normal_records_are_sanitized() {
        assert_eq!(
            record(b"https://opaque", 14),
            b"[diagnostic record truncated]\n"
        );
        assert_eq!(record(b"password=opaque\n", 1024), b"password=[REDACTED]\n");
    }

    #[test]
    fn embedded_and_incomplete_inspect_output_never_exports_environment_values() {
        for input in [
            "WARN inspect: {\"Env\":[\"ORDINARY=hidden\"],\"Cmd\":[\"app\",\"opaque\"]}",
            "{\n\"Config\": {\n\"Env\": [\n\"ORDINARY=hidden\"",
            "{\"Env\":[\"ORDINARY=hidden\"]}\n{\"password\":\"also-hidden\"}",
        ] {
            let result = output(input.as_bytes());
            assert!(!result.contains("hidden"), "{result}");
            assert!(!result.contains("opaque"), "{result}");
        }
    }

    #[test]
    fn source_labels_are_preserved_and_embedded_json_is_still_redacted() {
        for prefix in [
            "[test]",
            "[niri::dbus]",
            "[falsehood]",
            "[nullish]",
            "[12:34:56]",
            "[-1ms]",
            "[1.2ms]",
        ] {
            let plain = format!("{prefix} unrelated log");
            assert_eq!(redact(&plain), plain);
            let inspect =
                format!("{prefix} inspect {{\"Env\":[\"ORDINARY=hidden\"],\"state\":\"ready\"}}");
            let result = redact(&inspect);
            assert!(result.starts_with(prefix), "{result}");
            assert!(result.contains("ready"), "{result}");
            assert!(!result.contains("hidden"), "{result}");
        }
        for scalar in ["true", "false", "null", "0", "-1.25e+2"] {
            let inspect = format!("[{scalar}, {{\"Env\":[\"ORDINARY=hidden\"]}}]");
            assert!(!redact(&inspect).contains("hidden"));
            let incomplete = format!("[{scalar}, {{\"Env\":[\"ORDINARY=hidden\"");
            assert!(!redact(&incomplete).contains("hidden"));
        }
    }

    #[test]
    fn secret_reader_handles_escaped_json_echoes_and_does_not_mutate_input() {
        let input = br#"{"message":"opaque\"credential","state":"ready"}"#;
        let original = input.to_vec();
        let result = output_with_secrets(input, &["opaque\"credential".into()]);
        assert!(!result.contains("credential"));
        assert!(result.contains("ready"));
        assert_eq!(input.as_slice(), original);
    }
}

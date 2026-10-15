use std::io::{self, BufRead, BufReader, Read};
use std::sync::{Arc, Mutex, OnceLock};

#[path = "diagnostic_redaction.rs"]
pub mod redaction;

const MAX_LINE_BYTES: usize = 16 * 1024;
const MAX_QUEUED_LINES: usize = 256;
const MAX_QUEUED_BYTES: usize = 4 * 1024 * 1024;

pub fn lines(reader: impl Read) -> BoundedLines<impl BufRead> {
    lines_with_secrets(reader, std::iter::empty())
}

/// Use for credential-bearing commands whose output may echo an unlabeled value.
pub fn lines_with_secrets(
    reader: impl Read,
    secrets: impl IntoIterator<Item = String>,
) -> BoundedLines<impl BufRead> {
    BoundedLines {
        reader: BufReader::new(reader),
        redactor: redaction::Redactor::with_secrets(secrets),
    }
}

/// Captured bytes stay raw for operational parsers. Only this diagnostic view
/// is line-bounded and sanitized, independently for stdout and stderr.
pub fn captured_lines<'a>(
    output: &'a std::process::Output,
    secrets: &'a [String],
) -> impl Iterator<Item = String> + 'a {
    let stdout = redaction::output_with_secrets(&output.stdout, secrets);
    let stderr = redaction::output_with_secrets(&output.stderr, secrets);
    lines_with_secrets(
        std::io::Cursor::new(stdout.into_bytes()),
        secrets.iter().cloned(),
    )
    .chain(lines_with_secrets(
        std::io::Cursor::new(stderr.into_bytes()),
        secrets.iter().cloned(),
    ))
    .filter_map(Result::ok)
    .filter(|line| !line.trim().is_empty())
}

pub struct BoundedLines<R> {
    reader: R,
    redactor: redaction::Redactor,
}

impl<R: BufRead> Iterator for BoundedLines<R> {
    type Item = io::Result<String>;

    fn next(&mut self) -> Option<Self::Item> {
        let mut bytes = Vec::new();
        let mut truncated = false;
        loop {
            let available = match self.reader.fill_buf() {
                Ok(available) => available,
                Err(error) if error.kind() == io::ErrorKind::Interrupted => continue,
                Err(error) => return Some(Err(error)),
            };
            if available.is_empty() {
                if bytes.is_empty() && !truncated {
                    return None;
                }
                break;
            }
            let newline = available.iter().position(|byte| *byte == b'\n');
            let length = newline.unwrap_or(available.len());
            let copied = length.min(MAX_LINE_BYTES - bytes.len());
            bytes.extend_from_slice(&available[..copied]);
            truncated |= copied != length;
            self.reader.consume(length + usize::from(newline.is_some()));
            if newline.is_some() {
                break;
            }
        }
        if bytes.last() == Some(&b'\r') {
            bytes.pop();
        }
        let line = String::from_utf8_lossy(&bytes);
        let line = self.redactor.redact(&line);
        if truncated {
            // A truncated URL/assignment may lose its identifying suffix. Do not
            // expose a credential prefix whose label lies beyond the line limit.
            return Some(Ok("[output truncated]".into()));
        }
        Some(Ok(line))
    }
}

#[derive(Default)]
struct Usage {
    lines: usize,
    bytes: usize,
}

#[derive(Clone, Default)]
struct Budget(Arc<Mutex<Usage>>);

pub struct LogPermit {
    budget: Budget,
    bytes: usize,
}

impl Budget {
    fn reserve(&self, bytes: usize) -> Option<LogPermit> {
        let mut usage = self.0.lock().unwrap_or_else(|error| error.into_inner());
        if usage.lines >= MAX_QUEUED_LINES || bytes > MAX_QUEUED_BYTES.saturating_sub(usage.bytes) {
            return None;
        }
        usage.lines += 1;
        usage.bytes += bytes;
        Some(LogPermit {
            budget: self.clone(),
            bytes,
        })
    }
}

impl Drop for LogPermit {
    fn drop(&mut self) {
        let mut usage = self
            .budget
            .0
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        usage.lines -= 1;
        usage.bytes -= self.bytes;
    }
}

/// Lossy diagnostics must keep draining a noisy child, not block input or the child itself.
pub fn send(
    sender: &crate::event_loop_signal::Sender<crate::messages::CompositorMessage>,
    line_bytes: usize,
    mut message: crate::messages::CompositorMessage,
) {
    use crate::messages::CompositorMessage as Message;
    let bytes = match &mut message {
        Message::ContainerSessionLog { source, line, .. } => {
            *source = redaction::redact(source);
            *line = redaction::redact(line);
            source.capacity().saturating_add(line.capacity())
        }
        Message::ContainerImagePullLog {
            runtime,
            image,
            line,
        } => {
            *line = redaction::redact(line);
            runtime
                .capacity()
                .saturating_add(image.capacity())
                .saturating_add(line.capacity())
        }
        Message::ContainerVolumeDeleteLog {
            runtime,
            volume,
            line,
        }
        | Message::ContainerVolumeCreateLog {
            runtime,
            volume,
            line,
        } => {
            *line = redaction::redact(line);
            runtime
                .capacity()
                .saturating_add(volume.capacity())
                .saturating_add(line.capacity())
        }
        Message::RuntimeContainerActionLog {
            runtime,
            name,
            action,
            line,
        } => {
            *line = redaction::redact(line);
            runtime
                .capacity()
                .saturating_add(name.capacity())
                .saturating_add(action.capacity())
                .saturating_add(line.capacity())
        }
        Message::RuntimeSystemActionLog {
            runtime,
            action,
            line,
        } => {
            *line = redaction::redact(line);
            runtime
                .capacity()
                .saturating_add(action.capacity())
                .saturating_add(line.capacity())
        }
        _ => {
            // This lossy API must never accidentally discard lifecycle/control.
            let _ = sender.send(message);
            return;
        }
    }
    .saturating_add(std::mem::size_of::<Message>())
    .max(line_bytes);
    static BUDGET: OnceLock<Budget> = OnceLock::new();
    if let Some(permit) = BUDGET.get_or_init(Budget::default).reserve(bytes) {
        let _ = sender.try_send(crate::messages::CompositorMessage::ProcessOutput {
            message: Box::new(message),
            _permit: permit,
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn oversized_lines_are_drained_and_next_line_survives() {
        let input = std::io::repeat(b'x')
            .take(2 * 1024 * 1024)
            .chain(&b"\r\nnext\r\n"[..]);
        let output = lines(input).collect::<io::Result<Vec<_>>>().unwrap();
        assert_eq!(output.len(), 2);
        assert_eq!(output[0], "[output truncated]");
        assert_eq!(output[1], "next");
    }

    #[test]
    fn unterminated_invalid_utf8_is_bounded() {
        let output = lines(std::io::repeat(0xff).take(100_000))
            .next()
            .unwrap()
            .unwrap();
        assert!(output.len() <= MAX_LINE_BYTES * 3 + 32);
    }

    #[test]
    fn queue_has_both_line_and_byte_limits_and_releases_credits() {
        let budget = Budget::default();
        let permits = (0..MAX_QUEUED_LINES)
            .map(|_| budget.reserve(1).unwrap())
            .collect::<Vec<_>>();
        assert!(budget.reserve(1).is_none());
        drop(permits);
        let permit = budget.reserve(MAX_QUEUED_BYTES).unwrap();
        assert!(budget.reserve(1).is_none());
        drop(permit);
        assert!(budget.reserve(1).is_some());
    }

    #[test]
    fn process_output_redacts_credentials_and_multiline_private_keys() {
        let output = lines(&b"password=hidden\n-----BEGIN PRIVATE KEY-----\nbody\n-----END PRIVATE KEY-----\nready\n"[..])
            .collect::<io::Result<Vec<_>>>().unwrap();
        assert_eq!(
            output,
            [
                "password=[REDACTED]",
                "[REDACTED]",
                "[REDACTED]",
                "[REDACTED]",
                "ready"
            ]
        );
    }

    #[test]
    fn truncated_url_credentials_are_not_returned_as_an_unlabeled_prefix() {
        let input = format!("https://{}@host\nnext", "s".repeat(MAX_LINE_BYTES * 2));
        assert_eq!(
            lines(input.as_bytes())
                .collect::<io::Result<Vec<_>>>()
                .unwrap(),
            ["[output truncated]", "next"]
        );
    }

    #[test]
    fn known_credentials_are_redacted_without_a_label() {
        let output = lines_with_secrets(
            &b"login rejected: opaque-value\n"[..],
            ["opaque-value".into()],
        )
        .next()
        .unwrap()
        .unwrap();
        assert_eq!(output, "login rejected: [REDACTED]");
    }

    #[test]
    fn captured_diagnostics_redact_structured_and_known_secrets_without_mutating_output() {
        use std::os::unix::process::ExitStatusExt;
        let output = std::process::Output {
            status: std::process::ExitStatus::from_raw(0),
            stdout: br#"{"Env":["ORDINARY=hidden"],"state":"ready","message":"opaque\"secret"}"#
                .to_vec(),
            stderr: b"[test] login rejected: opaque\"secret\n".to_vec(),
        };
        let diagnostic = captured_lines(&output, &["opaque\"secret".into()])
            .collect::<Vec<_>>()
            .join("\n");
        assert!(!diagnostic.contains("hidden"));
        assert!(!diagnostic.contains("secret"));
        assert!(diagnostic.contains("ready"));
        assert!(diagnostic.contains("[test] login rejected:"));
        assert!(String::from_utf8_lossy(&output.stdout).contains("hidden"));
        assert!(String::from_utf8_lossy(&output.stderr).contains("secret"));
    }

    #[test]
    fn diagnostic_log_sanitization_preserves_raw_routing_identifiers() {
        use crate::messages::CompositorMessage as Message;
        let (sender, receiver) = crate::event_loop_signal::channel_with_waker(|| {});
        let runtime = "runtime?token=raw-routing-id".to_string();
        let image = "image?password=raw-image-id".to_string();
        send(
            &sender,
            0,
            Message::ContainerImagePullLog {
                runtime: runtime.clone(),
                image: image.clone(),
                line: "password=hidden".into(),
            },
        );
        let Message::ProcessOutput { message, .. } = receiver.try_recv().unwrap() else {
            panic!("expected bounded log")
        };
        sender.message_received();
        let Message::ContainerImagePullLog {
            runtime: routed_runtime,
            image: routed_image,
            line,
        } = *message
        else {
            panic!("expected image log")
        };
        assert_eq!(routed_runtime, runtime);
        assert_eq!(routed_image, image);
        assert_eq!(line, "password=[REDACTED]");
    }

    #[test]
    fn lossy_logs_do_not_block_a_full_control_queue_or_drop_control() {
        use crate::messages::CompositorMessage as Message;
        let (sender, receiver) = crate::event_loop_signal::channel_with_waker(|| {});
        for _ in 0..crate::event_loop_signal::MAX_PENDING_WORKER_MESSAGES {
            assert!(sender.send(Message::ReloadMenu).is_ok());
        }
        let worker = sender.clone();
        let (done, completed) = std::sync::mpsc::channel();
        let joined = std::thread::spawn(move || {
            send(
                &worker,
                1,
                Message::ContainerImagePullLog {
                    runtime: "apple".into(),
                    image: "test".into(),
                    line: "password=hidden".into(),
                },
            );
            done.send(()).unwrap();
        });
        completed
            .recv_timeout(std::time::Duration::from_secs(1))
            .unwrap();
        joined.join().unwrap();
        // The accidental use of the log API for control is still lossless on UI.
        send(&sender, 0, Message::ReloadMenu);
        let mut count = 0;
        while let Ok(message) = receiver.try_recv() {
            assert!(matches!(message, Message::ReloadMenu));
            sender.message_received();
            count += 1;
        }
        assert_eq!(
            count,
            crate::event_loop_signal::MAX_PENDING_WORKER_MESSAGES + 1
        );
    }
}

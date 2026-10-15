use std::io::{self, BufRead, BufReader, Read};
use std::sync::{Arc, Mutex, OnceLock};

const MAX_LINE_BYTES: usize = 16 * 1024;
const MAX_QUEUED_LINES: usize = 256;
const MAX_QUEUED_BYTES: usize = 4 * 1024 * 1024;

pub fn lines(reader: impl Read) -> BoundedLines<impl BufRead> {
    BoundedLines {
        reader: BufReader::new(reader),
    }
}

pub struct BoundedLines<R> {
    reader: R,
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
        let mut line = String::from_utf8_lossy(&bytes).into_owned();
        if truncated {
            line.push_str(" [output truncated]");
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
    message: crate::messages::CompositorMessage,
) {
    static BUDGET: OnceLock<Budget> = OnceLock::new();
    if let Some(permit) = BUDGET.get_or_init(Budget::default).reserve(line_bytes) {
        let _ = sender.send(crate::messages::CompositorMessage::ProcessOutput {
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
        assert_eq!(
            output[0].len(),
            MAX_LINE_BYTES + " [output truncated]".len()
        );
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
}

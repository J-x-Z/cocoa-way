use std::fmt;
use std::io::{self, Read};
use std::os::unix::process::CommandExt;
use std::process::{Child, Command, Output, Stdio};
use std::sync::mpsc::{self, Receiver};
use std::time::{Duration, Instant};

const MAX_OUTPUT_BYTES: usize = 8 * 1024 * 1024;

#[derive(Debug)]
pub(crate) enum CommandError {
    Io(io::Error),
    TimedOut,
}

impl fmt::Display for CommandError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Io(error) => error.fmt(formatter),
            Self::TimedOut => formatter.write_str("runtime command timed out"),
        }
    }
}

pub(crate) fn capture(mut command: Command, timeout: Duration) -> Result<Output, CommandError> {
    command
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .process_group(0);
    let mut child = command.spawn().map_err(CommandError::Io)?;
    let deadline = Instant::now() + timeout;
    // Drain both pipes while the command runs, including output beyond the capture limit.
    let stdout = drain(child.stdout.take().expect("piped stdout"));
    let stderr = drain(child.stderr.take().expect("piped stderr"));
    let result = (|| {
        let status = loop {
            match child.try_wait().map_err(CommandError::Io)? {
                Some(status) => break status,
                None if Instant::now() >= deadline => return Err(CommandError::TimedOut),
                None => std::thread::sleep(Duration::from_millis(20)),
            }
        };
        Ok(Output {
            status,
            stdout: collect(stdout, deadline)?,
            stderr: collect(stderr, deadline)?,
        })
    })();
    if result.is_err() {
        terminate(&mut child);
    }
    result
}

fn drain(mut pipe: impl Read + Send + 'static) -> Receiver<io::Result<Vec<u8>>> {
    let (sender, receiver) = mpsc::channel();
    std::thread::spawn(move || {
        let result = (|| {
            let mut output = Vec::new();
            let mut buffer = [0u8; 16 * 1024];
            let mut exceeded = false;
            loop {
                let count = match pipe.read(&mut buffer) {
                    Ok(0) => break,
                    Ok(count) => count,
                    Err(error) if error.kind() == io::ErrorKind::Interrupted => continue,
                    Err(error) => return Err(error),
                };
                let retained = count.min(MAX_OUTPUT_BYTES - output.len());
                output.extend_from_slice(&buffer[..retained]);
                exceeded |= retained != count;
            }
            if exceeded {
                Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    "runtime output exceeds 8 MiB",
                ))
            } else {
                Ok(output)
            }
        })();
        let _ = sender.send(result);
    });
    receiver
}

fn collect(
    receiver: Receiver<io::Result<Vec<u8>>>,
    deadline: Instant,
) -> Result<Vec<u8>, CommandError> {
    match receiver.recv_timeout(deadline.saturating_duration_since(Instant::now())) {
        Ok(result) => result.map_err(CommandError::Io),
        Err(mpsc::RecvTimeoutError::Timeout) => Err(CommandError::TimedOut),
        Err(mpsc::RecvTimeoutError::Disconnected) => Err(CommandError::Io(io::Error::other(
            "runtime output reader stopped",
        ))),
    }
}

fn terminate(child: &mut Child) {
    // A grandchild can inherit the pipes after its parent exits; stop the dedicated group too.
    unsafe { libc::kill(-(child.id() as libc::pid_t), libc::SIGKILL) };
    let _ = child.kill();
    let _ = child.wait();
}

#[cfg(test)]
mod tests {
    use super::*;

    fn shell(script: &str) -> Command {
        let mut command = Command::new("/bin/sh");
        command.args(["-c", script]);
        command
    }

    #[test]
    fn drains_large_stdout_and_stderr_without_deadlocking() {
        let output = capture(
            shell("head -c 1048576 /dev/zero; head -c 1048576 /dev/zero >&2"),
            Duration::from_secs(3),
        )
        .unwrap();
        assert!(output.status.success());
        assert_eq!(output.stdout.len(), 1048576);
        assert_eq!(output.stderr.len(), 1048576);
    }

    #[test]
    fn preserves_nonzero_exit_status_and_output() {
        let output = capture(shell("printf failed >&2; exit 7"), Duration::from_secs(3)).unwrap();
        assert_eq!(output.status.code(), Some(7));
        assert_eq!(output.stderr, b"failed");
    }

    #[test]
    fn times_out_when_a_descendant_keeps_the_pipe_open() {
        let started = Instant::now();
        let result = capture(shell("sleep 30 & exit 0"), Duration::from_millis(150));
        assert!(matches!(result, Err(CommandError::TimedOut)));
        assert!(started.elapsed() < Duration::from_secs(2));
    }

    #[test]
    fn rejects_oversized_output_instead_of_returning_truncated_json() {
        let result = capture(shell("head -c 8388609 /dev/zero"), Duration::from_secs(3));
        assert!(
            matches!(result, Err(CommandError::Io(error)) if error.kind() == io::ErrorKind::InvalidData)
        );
    }
}

use std::fmt;
use std::io::{self, Read, Write};
use std::os::fd::AsRawFd;
use std::os::unix::process::CommandExt;
use std::process::{Child, Command, Output, Stdio};
use std::time::{Duration, Instant};

const MAX_OUTPUT_BYTES: usize = 8 * 1024 * 1024;
const MAX_INPUT_BYTES: usize = 1024 * 1024;
pub(crate) const ACTION_TIMEOUT: Duration = Duration::from_secs(60);

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
    if timeout.is_zero() {
        return Err(CommandError::TimedOut);
    }
    command
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .process_group(0);
    capture_child(command.spawn().map_err(CommandError::Io)?, timeout)
}

/// Input remains raw operational data; redaction belongs only at diagnostic sinks.
pub(crate) fn capture_with_input(
    mut command: Command,
    input: Vec<u8>,
    timeout: Duration,
) -> Result<Output, CommandError> {
    if timeout.is_zero() {
        return Err(CommandError::TimedOut);
    }
    if input.len() > MAX_INPUT_BYTES {
        return Err(CommandError::Io(io::Error::new(
            io::ErrorKind::InvalidInput,
            "runtime input exceeds 1 MiB",
        )));
    }
    command
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .process_group(0);
    capture_child_inner(command.spawn().map_err(CommandError::Io)?, input, timeout)
}

/// The caller must spawn in a dedicated process_group(0). Both output pipes must
/// be piped. This function owns reaping/termination, including setup failures.
pub(crate) fn capture_child(child: Child, timeout: Duration) -> Result<Output, CommandError> {
    capture_child_inner(child, Vec::new(), timeout)
}

struct OwnedChild {
    child: Child,
    group: Option<libc::pid_t>,
    finished: bool,
}

impl Drop for OwnedChild {
    fn drop(&mut self) {
        if self.finished {
            return;
        }
        if let Some(group) = self.group {
            // The captured child is the group leader, never the caller's group.
            unsafe {
                libc::kill(-group, libc::SIGKILL);
            }
        }
        let _ = self.child.kill();
        // Do not extend the command deadline with an unbounded wait syscall.
        let reap_until = Instant::now() + Duration::from_millis(200);
        while Instant::now() < reap_until {
            match self.child.try_wait() {
                Ok(Some(_)) => return,
                Ok(None) => std::thread::sleep(Duration::from_millis(5)),
                Err(_) => return,
            }
        }
        // A killed process stuck in the kernel is not allowed to stall callers.
        // Preserve the Child handle for a detached final reaper.
        let pid = self.child.id() as libc::pid_t;
        let _ = std::thread::Builder::new()
            .name("runtime-reap".into())
            .spawn(move || {
                loop {
                    let result = unsafe { libc::waitpid(pid, std::ptr::null_mut(), 0) };
                    if result >= 0
                        || io::Error::last_os_error().kind() != io::ErrorKind::Interrupted
                    {
                        break;
                    }
                }
            });
    }
}

fn nonblocking(pipe: &impl AsRawFd) -> io::Result<()> {
    let fd = pipe.as_raw_fd();
    let flags = unsafe { libc::fcntl(fd, libc::F_GETFL) };
    if flags < 0 || unsafe { libc::fcntl(fd, libc::F_SETFL, flags | libc::O_NONBLOCK) } < 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(())
}

fn retry(error: &io::Error) -> bool {
    matches!(
        error.kind(),
        io::ErrorKind::Interrupted | io::ErrorKind::WouldBlock
    )
}

fn read_pipe<R: Read>(pipe: &mut Option<R>, output: &mut Vec<u8>) -> io::Result<()> {
    let Some(reader) = pipe.as_mut() else {
        return Ok(());
    };
    let mut bytes = [0; 16 * 1024];
    match reader.read(&mut bytes) {
        Ok(0) => {
            *pipe = None;
        }
        Ok(count) => {
            if count > MAX_OUTPUT_BYTES.saturating_sub(output.len()) {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    "runtime output exceeds 8 MiB",
                ));
            }
            output.extend_from_slice(&bytes[..count]);
        }
        Err(error) if retry(&error) => {}
        Err(error) => return Err(error),
    }
    Ok(())
}

fn capture_child_inner(
    child: Child,
    input: Vec<u8>,
    timeout: Duration,
) -> Result<Output, CommandError> {
    let pid = child.id() as libc::pid_t;
    let group = (unsafe { libc::getpgid(pid) } == pid).then_some(pid);
    let mut owned = OwnedChild {
        child,
        group,
        finished: false,
    };
    let deadline = Instant::now()
        .checked_add(timeout)
        .ok_or_else(|| CommandError::Io(io::Error::from(io::ErrorKind::InvalidInput)))?;
    let mut stdout =
        Some(
            owned.child.stdout.take().ok_or_else(|| {
                CommandError::Io(io::Error::other("runtime stdout must be piped"))
            })?,
        );
    let mut stderr =
        Some(
            owned.child.stderr.take().ok_or_else(|| {
                CommandError::Io(io::Error::other("runtime stderr must be piped"))
            })?,
        );
    let mut stdin = owned.child.stdin.take();
    if input.is_empty() {
        stdin = None;
    }
    if !input.is_empty() && stdin.is_none() {
        return Err(CommandError::Io(io::Error::other(
            "runtime stdin must be piped",
        )));
    }
    nonblocking(stdout.as_ref().unwrap()).map_err(CommandError::Io)?;
    nonblocking(stderr.as_ref().unwrap()).map_err(CommandError::Io)?;
    if let Some(stdin) = &stdin {
        nonblocking(stdin).map_err(CommandError::Io)?;
    }
    let mut output = Vec::new();
    let mut errors = Vec::new();
    let mut written = 0;
    loop {
        // Keep the group leader unreaped while descendants can retain pipes.
        // Its PID cannot be reused before timeout cleanup signals that group.
        if stdout.is_none() && stderr.is_none() && stdin.is_none() {
            if let Some(status) = owned.child.try_wait().map_err(CommandError::Io)? {
                owned.finished = true;
                return Ok(Output {
                    status,
                    stdout: output,
                    stderr: errors,
                });
            }
        }
        let remaining = deadline
            .checked_duration_since(Instant::now())
            .filter(|time| !time.is_zero())
            .ok_or(CommandError::TimedOut)?;
        let mut polls = [
            libc::pollfd {
                fd: stdin.as_ref().map_or(-1, AsRawFd::as_raw_fd),
                events: libc::POLLOUT,
                revents: 0,
            },
            libc::pollfd {
                fd: stdout.as_ref().map_or(-1, AsRawFd::as_raw_fd),
                events: libc::POLLIN,
                revents: 0,
            },
            libc::pollfd {
                fd: stderr.as_ref().map_or(-1, AsRawFd::as_raw_fd),
                events: libc::POLLIN,
                revents: 0,
            },
        ];
        if polls.iter().all(|poll| poll.fd < 0) {
            std::thread::sleep(remaining.min(Duration::from_millis(5)));
            continue;
        }
        let wait_ms = remaining
            .as_nanos()
            .div_ceil(1_000_000)
            .min(libc::c_int::MAX as u128) as _;
        if unsafe { libc::poll(polls.as_mut_ptr(), polls.len() as _, wait_ms) } < 0 {
            let error = io::Error::last_os_error();
            if error.kind() == io::ErrorKind::Interrupted {
                continue;
            }
            return Err(CommandError::Io(error));
        }
        if polls.iter().any(|poll| poll.revents & libc::POLLNVAL != 0) {
            return Err(CommandError::Io(io::Error::from(
                io::ErrorKind::InvalidInput,
            )));
        }
        if polls[0].revents & (libc::POLLOUT | libc::POLLERR | libc::POLLHUP) != 0 {
            if let Some(writer) = stdin.as_mut() {
                let end = input.len().min(written + 16 * 1024);
                match writer.write(&input[written..end]) {
                    Ok(0) => return Err(CommandError::Io(io::ErrorKind::WriteZero.into())),
                    Ok(count) => {
                        written += count;
                        if written == input.len() {
                            stdin = None;
                        }
                    }
                    Err(error) if retry(&error) => {}
                    Err(error) => return Err(CommandError::Io(error)),
                }
            }
        }
        if polls[1].revents != 0 {
            read_pipe(&mut stdout, &mut output).map_err(CommandError::Io)?;
        }
        if polls[2].revents != 0 {
            read_pipe(&mut stderr, &mut errors).map_err(CommandError::Io)?;
        }
    }
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

    #[test]
    fn stdin_and_both_outputs_progress_independently_without_changing_operational_bytes() {
        let mut command = shell(
            "head -c 1048576 /dev/zero; head -c 1048576 /dev/zero >&2; printf '%s' \"$ORDINARY\"; cat",
        );
        command.env("ORDINARY", "raw-env-value");
        let input = b"password=raw-input\n".repeat(8192);
        let output = capture_with_input(command, input.clone(), Duration::from_secs(3)).unwrap();
        assert!(output.status.success());
        assert_eq!(output.stderr.len(), 1048576);
        assert_eq!(&output.stdout[1048576..1048576 + 13], b"raw-env-value");
        assert_eq!(&output.stdout[1048576 + 13..], input);
    }

    #[test]
    fn blocked_stdin_has_a_deadline_and_is_reaped() {
        let started = Instant::now();
        let result = capture_with_input(
            shell("sleep 30"),
            vec![1; MAX_INPUT_BYTES],
            Duration::from_millis(100),
        );
        assert!(matches!(result, Err(CommandError::TimedOut)));
        assert!(started.elapsed() < Duration::from_secs(2));
    }

    #[test]
    fn setup_failure_kills_and_reaps_the_owned_child() {
        let child = shell("sleep 30").process_group(0).spawn().unwrap();
        let pid = child.id() as libc::pid_t;
        assert!(capture_child(child, Duration::from_secs(1)).is_err());
        assert_eq!(
            unsafe { libc::waitpid(pid, std::ptr::null_mut(), libc::WNOHANG) },
            -1
        );
        assert_eq!(
            io::Error::last_os_error().raw_os_error(),
            Some(libc::ECHILD)
        );
    }

    #[test]
    fn expired_capture_does_not_spawn_and_oversized_input_is_rejected() {
        assert!(matches!(
            capture(Command::new("/must-not-run"), Duration::ZERO),
            Err(CommandError::TimedOut)
        ));
        let result = capture_with_input(
            Command::new("/must-not-run"),
            vec![0; MAX_INPUT_BYTES + 1],
            Duration::from_secs(1),
        );
        assert!(
            matches!(result, Err(CommandError::Io(error)) if error.kind() == io::ErrorKind::InvalidInput)
        );
    }
}

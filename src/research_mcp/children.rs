use serde_json::{Value, json};

use super::policy::{MAX_CHILDREN, Profile};

pub const OUTPUT_CAP: usize = 64 * 1024;

/// Child bytes are discarded, not partially redacted: even a short prefix can
/// contain clipboard text, key material, or a window title.
#[derive(Clone, Copy, Default)]
pub struct OutputCount {
    pub bytes: usize,
    pub capped: bool,
}

impl OutputCount {
    pub fn observe(&mut self, count: usize) {
        self.capped |= count > OUTPUT_CAP.saturating_sub(self.bytes);
        self.bytes = self.bytes.saturating_add(count).min(OUTPUT_CAP);
    }

    fn value(self) -> Value {
        json!({"bytes_observed_capped": self.bytes, "additional_bytes_discarded": self.capped, "content": "withheld"})
    }
}

#[derive(Clone, Copy)]
struct Summary {
    state: &'static str,
    stdout: OutputCount,
    stderr: OutputCount,
    exit_code: Option<i32>,
    appkit_terminate_accepted: Option<bool>,
    leader_reaped: bool,
    force_kill_used: bool,
    group_cleanup: &'static str,
    group_cleanup_errno: Option<i32>,
    group_membership: &'static str,
}

impl Default for Summary {
    fn default() -> Self {
        Self {
            state: "never_started",
            stdout: OutputCount::default(),
            stderr: OutputCount::default(),
            exit_code: None,
            appkit_terminate_accepted: None,
            leader_reaped: false,
            force_kill_used: false,
            group_cleanup: "not_requested",
            group_cleanup_errno: None,
            group_membership: "not_checked",
        }
    }
}

#[derive(Default)]
pub struct Children {
    summaries: [Summary; MAX_CHILDREN],
    #[cfg(target_os = "macos")]
    active: [Option<mac::Owned>; MAX_CHILDREN],
}

impl Children {
    pub fn snapshot(&self, profile: Profile) -> Value {
        let summary = self.summaries[profile.index()];
        json!({"state": summary.state, "stdout": summary.stdout.value(), "stderr": summary.stderr.value(), "exit_code": summary.exit_code,
            "appkit_terminate_accepted": summary.appkit_terminate_accepted, "leader_reaped": summary.leader_reaped,
            "leader_force_kill_used": summary.force_kill_used, "group_cleanup": summary.group_cleanup,
            "group_cleanup_errno": summary.group_cleanup_errno, "group_membership": summary.group_membership})
    }

    pub fn has_active(&self) -> bool {
        #[cfg(target_os = "macos")]
        {
            self.active.iter().any(Option::is_some)
        }
        #[cfg(not(target_os = "macos"))]
        {
            false
        }
    }

    pub fn is_active(&self, profile: Profile) -> bool {
        #[cfg(target_os = "macos")]
        {
            self.active[profile.index()].is_some()
        }
        #[cfg(not(target_os = "macos"))]
        {
            let _ = profile;
            false
        }
    }

    pub fn launch(
        &mut self,
        root: &std::path::Path,
        profile: Profile,
        ttl: u64,
    ) -> Result<(), &'static str> {
        #[cfg(not(target_os = "macos"))]
        {
            let _ = (root, profile, ttl);
            Err("unsupported_platform")
        }
        #[cfg(target_os = "macos")]
        {
            use std::os::unix::process::CommandExt;
            use std::process::{Command, Stdio};
            use std::time::{Duration, Instant};

            if !(1..=profile.ttl_limits().1).contains(&ttl) {
                return Err("invalid_profile_ttl");
            }
            self.tick();
            if self.active[profile.index()].is_some() {
                return Err("profile_already_running");
            }
            if self.active.iter().flatten().count() >= MAX_CHILDREN {
                return Err("child_limit_reached");
            }
            super::policy::Host::detect().check()?;
            let kit = super::kit::Kit::open(root)?;
            let helper = kit.verify(profile, || self.tick())?;
            if super::transport::shutdown_requested() {
                return Err("server_shutting_down");
            }
            kit.recheck(profile, &helper)?;
            let mut command = Command::new(&helper.path);
            command
                .args(profile.argv())
                .env_clear()
                .env("PATH", "/usr/bin:/bin")
                .env("LANG", "C")
                .stdin(Stdio::null())
                .stdout(Stdio::piped())
                .stderr(Stdio::piped())
                .process_group(0);
            let root_fd = kit.root_fd();
            // Only async-signal-safe fchdir runs between fork and exec. The
            // trusted kit must remain unchanged through pathname-based exec.
            unsafe {
                command.pre_exec(move || {
                    if libc::fchdir(root_fd) == 0 {
                        Ok(())
                    } else {
                        Err(std::io::Error::last_os_error())
                    }
                });
            }
            let child = command.spawn().map_err(|_| "helper_spawn_failed")?;
            // Adopt immediately, before any fallible pipe setup, so every
            // subsequent error goes through owned group cleanup and wait().
            self.active[profile.index()] = Some(mac::Owned {
                child,
                deadline: Instant::now() + Duration::from_secs(ttl),
                stopping: None,
            });
            self.summaries[profile.index()] = Summary {
                state: "running",
                ..Summary::default()
            };
            let owned = self.active[profile.index()].as_ref().unwrap();
            if mac::nonblocking_pipes(owned).is_err() {
                self.request_stop(profile, "pipe_setup_failed");
                return Err("pipe_setup_failed");
            }
            Ok(())
        }
    }

    pub fn stop(&mut self, profile: Profile) {
        #[cfg(target_os = "macos")]
        self.request_stop(profile, "stopped");
        #[cfg(not(target_os = "macos"))]
        let _ = profile;
    }

    pub fn tick(&mut self) {
        #[cfg(target_os = "macos")]
        for profile in Profile::ALL {
            let index = profile.index();
            let Some(owned) = &mut self.active[index] else {
                continue;
            };
            mac::drain(owned, &mut self.summaries[index]);
            match mac::waitable(owned) {
                Ok(true) => {
                    let reason = owned.stopping.as_ref().map_or("exited", |stop| stop.reason);
                    self.finish(index, reason);
                }
                Ok(false) if owned.stopping.is_some() => self.progress_stop(profile),
                Ok(false) if std::time::Instant::now() >= owned.deadline => {
                    self.request_stop(profile, "ttl_expired")
                }
                Ok(false) => {}
                Err(()) => self.finish(index, "ownership_lost"),
            }
        }
    }

    pub fn shutdown(&mut self) {
        #[cfg(target_os = "macos")]
        for profile in Profile::ALL {
            self.request_stop(profile, "server_shutdown");
        }
    }

    #[cfg(target_os = "macos")]
    fn request_stop(&mut self, profile: Profile, reason: &'static str) {
        use std::time::{Duration, Instant};
        let Some(owned) = &mut self.active[profile.index()] else {
            return;
        };
        if owned.stopping.is_none() {
            owned.stopping = Some(mac::Stopping {
                reason,
                deadline: Instant::now() + Duration::from_millis(super::policy::GUI_STOP_GRACE_MS),
            });
            self.summaries[profile.index()].state = "stopping";
        }
        self.progress_stop(profile);
    }

    #[cfg(target_os = "macos")]
    fn progress_stop(&mut self, profile: Profile) {
        use super::policy::{StopAction, stop_action};
        let index = profile.index();
        let Some(owned) = &self.active[index] else {
            return;
        };
        let Some(stop) = &owned.stopping else {
            return;
        };
        let Ok(exited) = mac::waitable(owned) else {
            self.finish(index, "ownership_lost");
            return;
        };
        let reason = stop.reason;
        match stop_action(
            profile,
            exited,
            std::time::Instant::now() >= stop.deadline,
            self.summaries[index].appkit_terminate_accepted,
        ) {
            StopAction::CleanupGroup => self.finish(index, reason),
            StopAction::RequestAppKit => {
                // Lookup and terminate only the still-owned, unreaped leader.
                // A GUI may not yet be registered; retry only during this grace.
                self.summaries[index].appkit_terminate_accepted = mac::terminate(owned);
            }
            StopAction::Wait => {}
        }
    }

    #[cfg(target_os = "macos")]
    fn finish(&mut self, index: usize, reason: &'static str) {
        let Some(mut owned) = self.active[index].take() else {
            return;
        };
        // WNOWAIT preserves the leader's PID even after natural exit, avoiding
        // signalling a reused process group. Never accept a PID from MCP.
        if let Ok(exited) = mac::waitable(&owned) {
            let cleanup = mac::cleanup_group(&owned, exited);
            self.summaries[index].group_cleanup = cleanup.state;
            self.summaries[index].group_cleanup_errno = cleanup.errno;
            self.summaries[index].group_membership = cleanup.membership;
            self.summaries[index].force_kill_used = !exited;
            if !exited {
                let _ = owned.child.kill();
            }
            let status = owned.child.wait();
            self.summaries[index].leader_reaped = status.is_ok();
            self.summaries[index].exit_code = status.ok().and_then(|status| status.code());
            mac::drain(&mut owned, &mut self.summaries[index]);
            self.summaries[index].state = if !self.summaries[index].leader_reaped {
                "reap_failed"
            } else if self.summaries[index].group_cleanup == "failed" {
                "cleanup_failed"
            } else {
                reason
            };
        } else {
            self.summaries[index].state = "ownership_lost";
        }
    }
}

impl Drop for Children {
    fn drop(&mut self) {
        self.shutdown();
        #[cfg(target_os = "macos")]
        while self.has_active() {
            self.tick();
            if self.has_active() {
                // Also drain the same bounded grace on transport errors/signals.
                unsafe {
                    libc::poll(std::ptr::null_mut(), 0, 50);
                }
            }
        }
    }
}

#[cfg(target_os = "macos")]
mod mac {
    use std::io;
    use std::os::fd::AsRawFd;
    use std::process::Child;
    use std::time::Instant;

    use super::{OutputCount, Summary};

    pub struct Owned {
        pub child: Child,
        pub deadline: Instant,
        pub stopping: Option<Stopping>,
    }

    pub struct Stopping {
        pub reason: &'static str,
        pub deadline: Instant,
    }

    pub struct GroupCleanup {
        pub state: &'static str,
        pub errno: Option<i32>,
        pub membership: &'static str,
    }

    pub fn cleanup_group(owned: &Owned, exited: bool) -> GroupCleanup {
        let pid = owned.child.id() as libc::pid_t;
        let result = unsafe { libc::kill(-pid, libc::SIGKILL) };
        // Capture errno before libproc/waitid can overwrite it.
        let errno = if result == 0 {
            None
        } else {
            io::Error::last_os_error().raw_os_error()
        };
        let mut cleanup = GroupCleanup {
            state: "failed",
            errno,
            membership: "not_checked",
        };
        if result == 0 {
            cleanup.state = "signalled";
        } else if errno == Some(libc::ESRCH) {
            cleanup.state = "already_absent";
        } else if errno == Some(libc::EPERM) && exited && waitable(owned) == Ok(true) {
            // Darwin returns EPERM for a group containing only its zombie
            // leader. WNOWAIT still reserves that PID/PGID: do NOT reap first
            // and then retry kill, which could signal a reused process group.
            let mut members = [0 as libc::pid_t; 256];
            let capacity = std::mem::size_of_val(&members) as libc::c_int;
            // proc_listpids returns bytes (unlike proc_listpgrppids, which
            // returns a PID count). PROC_PGRP_ONLY is 2 in proc_info.h.
            let bytes = unsafe {
                libc::proc_listpids(2, pid as u32, members.as_mut_ptr().cast(), capacity)
            };
            if only_exited_leader(pid, &members, bytes) {
                cleanup.state = "no_live_members";
                cleanup.membership = "exited_owned_leader_only";
            } else {
                // This includes live/inaccessible/extra zombie members and any
                // truncated/failed enumeration; EPERM alone is never success.
                cleanup.membership = "additional_or_unverified_members";
            }
        }
        cleanup
    }

    fn only_exited_leader(pid: libc::pid_t, members: &[libc::pid_t], bytes: libc::c_int) -> bool {
        bytes == std::mem::size_of::<libc::pid_t>() as libc::c_int
            && members.len() > 1 // A full one-entry buffer could be truncated.
            && members[0] == pid
    }

    #[cfg(test)]
    mod tests {
        use super::only_exited_leader;

        #[test]
        fn eperm_requires_complete_exited_leader_only_membership() {
            let width = std::mem::size_of::<libc::pid_t>() as libc::c_int;
            assert!(only_exited_leader(42, &[42, 0, 0], width));
            for (members, bytes) in [
                (&[42, 43, 0][..], 2 * width), // Never ignore another member.
                (&[43, 0, 0][..], width),
                (&[42, 0, 0][..], 0),
                (&[42, 0, 0][..], -1),
                (&[42, 0, 0][..], width + 1),
                (&[42, 0, 0][..], 3 * width), // Capacity reached: truncated.
                (&[42][..], width),
                (&[][..], width),
            ] {
                assert!(!only_exited_leader(42, members, bytes));
            }
        }
    }

    pub fn terminate(owned: &Owned) -> Option<bool> {
        use objc2_app_kit::NSRunningApplication;
        if waitable(owned) != Ok(false) {
            return None;
        }
        let pid = owned.child.id() as libc::pid_t;
        objc2::rc::autoreleasepool(|_| unsafe {
            NSRunningApplication::runningApplicationWithProcessIdentifier(pid)
                .filter(|app| app.processIdentifier() == pid)
                .map(|app| app.terminate())
        })
    }

    pub fn nonblocking_pipes(owned: &Owned) -> io::Result<()> {
        let stdout = owned
            .child
            .stdout
            .as_ref()
            .ok_or_else(|| io::Error::other("missing pipe"))?;
        let stderr = owned
            .child
            .stderr
            .as_ref()
            .ok_or_else(|| io::Error::other("missing pipe"))?;
        for fd in [stdout.as_raw_fd(), stderr.as_raw_fd()] {
            super::super::transport::set_nonblocking(fd)?;
        }
        Ok(())
    }

    pub fn drain(owned: &mut Owned, summary: &mut Summary) {
        if let Some(stdout) = owned.child.stdout.as_ref() {
            drain_fd(stdout.as_raw_fd(), &mut summary.stdout);
        }
        if let Some(stderr) = owned.child.stderr.as_ref() {
            drain_fd(stderr.as_raw_fd(), &mut summary.stderr);
        }
    }

    fn drain_fd(fd: libc::c_int, output: &mut OutputCount) {
        let mut bytes = [0u8; 4096];
        // A flooding child cannot monopolize the loop and defer other TTLs.
        for _ in 0..4 {
            let mut ready = libc::pollfd {
                fd,
                events: libc::POLLIN,
                revents: 0,
            };
            if unsafe { libc::poll(&mut ready, 1, 0) } <= 0 {
                break;
            }
            let count = unsafe { libc::read(fd, bytes.as_mut_ptr().cast(), bytes.len()) };
            if count <= 0 {
                break;
            }
            output.observe(count as usize);
        }
    }

    pub fn waitable(owned: &Owned) -> Result<bool, ()> {
        loop {
            let mut info: libc::siginfo_t = unsafe { std::mem::zeroed() };
            let result = unsafe {
                libc::waitid(
                    libc::P_PID,
                    owned.child.id() as libc::id_t,
                    &mut info,
                    libc::WEXITED | libc::WNOHANG | libc::WNOWAIT,
                )
            };
            if result == 0 {
                return Ok(unsafe { info.si_pid() } != 0);
            }
            let error = io::Error::last_os_error();
            if error.kind() != io::ErrorKind::Interrupted {
                // Only ECHILD disproves ownership. No other code reaps our
                // children; on other monitor errors retain the PID/slot and
                // conservatively take the bounded stop path instead of leaking.
                return if error.raw_os_error() == Some(libc::ECHILD) {
                    Err(())
                } else {
                    Ok(false)
                };
            }
        }
    }
}

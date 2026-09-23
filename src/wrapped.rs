//! Runs the user's own statusline command for `statusline --exec`, with a
//! deadline.
//!
//! Claude Code puts no time limit on a statusline command: a script that hangs
//! keeps the status line frozen until the next update happens to cancel it.
//! Wrapping it must not make that any worse, so the command gets
//! [`TIMEOUT`] and is then killed.
//!
//! The command runs in its own process group so the kill reaches everything
//! it started. `sh -c 'a | b'` forks, and a grandchild left alive still holds
//! the inherited stdout, which keeps Claude Code waiting for EOF exactly as if
//! nothing had been killed. A process group of its own also means a signal
//! aimed at this process no longer reaches the command by itself, so
//! [`run`] forwards the usual termination signals to the group.

use std::os::unix::process::CommandExt;
use std::process::{ChildStdin, Command, ExitStatus, Stdio};
use std::sync::atomic::{AtomicI32, Ordering};
use std::time::{Duration, Instant};

/// How long the wrapped command may run. Far longer than any statusline
/// script should take, so only a command that is truly stuck is cut off.
pub const TIMEOUT: Duration = Duration::from_secs(10);

/// How often [`run`] checks whether the command has exited.
const POLL: Duration = Duration::from_millis(5);

/// The process group [`forward_and_die`] kills; zero while there is none.
static GROUP: AtomicI32 = AtomicI32::new(0);

/// Runs `command` under `sh -c` with this process's stdout and stderr,
/// killing its whole process group once `timeout` passes. `feed` writes the
/// command's stdin; the pipe closes when it returns.
///
/// Returns the exit status, or `None` when the command could not be started
/// or had to be killed.
pub fn run(
    command: &str,
    feed: impl FnOnce(&mut ChildStdin) + Send + 'static,
    timeout: Duration,
) -> Option<ExitStatus> {
    // stdout is inherited rather than piped and copied, so the child's bytes
    // reach Claude Code exactly as written — no added newline, no buffering
    // surprises.
    let mut child = Command::new("sh")
        .arg("-c")
        .arg(command)
        .stdin(Stdio::piped())
        .stdout(Stdio::inherit())
        .stderr(Stdio::inherit())
        .process_group(0)
        .spawn()
        .ok()?;
    let group = child.id() as i32;
    forward_termination(group);

    // Fed from another thread: a command that never reads its stdin would
    // otherwise block this one on a full pipe before the deadline is checked.
    // Killing the group breaks the pipe, which ends the write.
    if let Some(mut stdin) = child.stdin.take() {
        std::thread::spawn(move || {
            feed(&mut stdin);
            // Dropping closes the pipe, so a child reading to EOF finishes.
        });
    }

    let deadline = Instant::now() + timeout;
    let status = loop {
        match child.try_wait() {
            Ok(Some(status)) => break Some(status),
            Ok(None) if Instant::now() < deadline => std::thread::sleep(POLL),
            _ => {
                kill_group(group);
                let _ = child.wait();
                break None;
            }
        }
    };
    GROUP.store(0, Ordering::SeqCst);
    status
}

/// Sends `SIGKILL` to every process in `group`. A group that no longer exists
/// is not an error worth reporting.
fn kill_group(group: i32) {
    // SAFETY: `kill` reads no memory; it takes a process group and a signal.
    unsafe { libc::kill(-group, libc::SIGKILL) };
}

/// Makes `SIGTERM`, `SIGINT` and `SIGHUP` kill `group` before they end this
/// process, as they would have if the command still shared its group.
fn forward_termination(group: i32) {
    GROUP.store(group, Ordering::SeqCst);
    for signal in [libc::SIGTERM, libc::SIGINT, libc::SIGHUP] {
        // SAFETY: `forward_and_die` only calls async-signal-safe functions.
        unsafe { libc::signal(signal, forward_and_die as *const () as libc::sighandler_t) };
    }
}

extern "C" fn forward_and_die(signal: libc::c_int) {
    let group = GROUP.load(Ordering::SeqCst);
    if group > 0 {
        kill_group(group);
    }
    // SAFETY: `signal` and `raise` are async-signal-safe. Restoring the
    // default action and raising again ends the process the way the signal
    // would have, exit status included.
    unsafe {
        libc::signal(signal, libc::SIG_DFL);
        libc::raise(signal);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testutil::TempDir;
    use std::io::Write;

    fn bytes(input: Vec<u8>) -> impl FnOnce(&mut ChildStdin) + Send + 'static {
        move |pipe| {
            let _ = pipe.write_all(&input);
        }
    }

    fn alive(pid: i32) -> bool {
        // SAFETY: signal 0 only checks that the process exists.
        unsafe { libc::kill(pid, 0) == 0 }
    }

    #[test]
    fn a_quick_command_gets_the_input_and_its_status_back() {
        let dir = TempDir::new("wrapped-quick");
        let out = dir.path().join("out");
        let command = format!("cat > '{}'; exit 3", out.display());

        let status = run(&command, bytes(b"{\"a\":1}".to_vec()), TIMEOUT).expect("ran");

        assert_eq!(status.code(), Some(3));
        assert_eq!(std::fs::read(&out).expect("output"), b"{\"a\":1}");
    }

    #[test]
    fn a_hung_command_is_killed_at_the_deadline() {
        let started = Instant::now();

        let status = run("sleep 30", bytes(Vec::new()), Duration::from_millis(200));

        assert_eq!(status, None);
        assert!(started.elapsed() < Duration::from_secs(5));
    }

    #[test]
    fn a_command_that_never_reads_a_large_input_does_not_block() {
        let started = Instant::now();

        let status = run(
            "sleep 30",
            bytes(vec![b'x'; 1 << 20]),
            Duration::from_millis(200),
        );

        assert_eq!(status, None);
        assert!(started.elapsed() < Duration::from_secs(5));
    }

    #[test]
    fn the_kill_reaches_what_the_command_started() {
        let dir = TempDir::new("wrapped-grandchild");
        let pid_file = dir.path().join("pid");
        let command = format!("sleep 30 & echo $! > '{}'; wait", pid_file.display());

        assert_eq!(
            run(&command, bytes(Vec::new()), Duration::from_millis(300)),
            None
        );

        let pid: i32 = std::fs::read_to_string(&pid_file)
            .expect("pid written")
            .trim()
            .parse()
            .expect("pid");
        // Killed but possibly not yet reaped by whoever inherited it.
        let deadline = Instant::now() + Duration::from_secs(2);
        while alive(pid) && Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(10));
        }
        assert!(!alive(pid), "background sleep {pid} survived");
    }
}

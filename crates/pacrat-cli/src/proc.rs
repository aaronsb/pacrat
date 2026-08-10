//! Running a child process under a wall-clock bound.
//!
//! Most of pacrat's subprocesses answer or fail promptly, and `Command::output`
//! is the right tool for those. This module exists for the ones that reach the
//! network, where "fail" and "never answer" are different outcomes and only one
//! of them is survivable: a TCP connect to a host that silently drops packets
//! does not return, and the verb that called it does not either.
//!
//! That matters most where nobody is watching. `pacrat updates` is the systemd
//! timer entry point, and a hung timer unit is worse than a failing one — it
//! produces no output, no exit code, and no alert, so the fleet looks calm
//! precisely because the check stopped working.
//!
//! Both pipes are drained on their own threads rather than read after the
//! wait. A child whose output exceeds the pipe buffer would block writing while
//! we blocked waiting, and we would report that deadlock as its timeout — a
//! misdiagnosis that sends the reader looking at the network instead of the
//! output. Each pipe is also capped: the child picks how much it writes, so
//! reading to end unbounded is how a hostile remote becomes pacrat's memory.

use std::io::Read;
use std::process::{Command, ExitStatus, Stdio};
use std::sync::mpsc;
use std::time::{Duration, Instant};

/// How often the wait loop wakes to check on the child. Small enough that the
/// timeout is honoured promptly, large enough not to spin a core.
const POLL: Duration = Duration::from_millis(25);

/// How long to wait for the drain threads after the child is gone. A killed
/// child's pipes close immediately, so this is a backstop, not a budget.
const DRAIN_GRACE: Duration = Duration::from_secs(2);

/// Per-pipe ceiling. Generous for anything pacrat asks a child to say, and
/// finite, which is the point.
const PIPE_LIMIT: u64 = 1 << 20;

pub struct Ran {
    pub status: ExitStatus,
    pub stdout: Vec<u8>,
    pub stderr: Vec<u8>,
    /// The child overran `timeout` and was killed. Its output, if any, is
    /// whatever it managed to write first and is not a complete answer.
    pub timed_out: bool,
}

/// Run `cmd` to completion or to `timeout`, whichever comes first.
///
/// stdin is `/dev/null` so a child that decides to prompt fails fast rather
/// than blocking on a terminal that is not listening — the timeout is the
/// backstop for that case, not the plan.
pub fn run_with_timeout(mut cmd: Command, timeout: Duration) -> Result<Ran, String> {
    let started = Instant::now();
    let mut child = cmd
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|e| format!("{e}"))?;

    let (tx, rx) = mpsc::channel::<(bool, Vec<u8>)>();
    for (is_err, pipe) in [
        (false, child.stdout.take().map(Pipe::Out)),
        (true, child.stderr.take().map(Pipe::Err)),
    ] {
        let Some(pipe) = pipe else { continue };
        let tx = tx.clone();
        std::thread::spawn(move || {
            let mut buf = Vec::new();
            let _ = pipe.take(PIPE_LIMIT).read_to_end(&mut buf);
            let _ = tx.send((is_err, buf));
        });
    }
    drop(tx);

    let mut timed_out = false;
    let status = loop {
        match child.try_wait().map_err(|e| format!("{e}"))? {
            Some(status) => break status,
            None if started.elapsed() >= timeout => {
                let _ = child.kill();
                timed_out = true;
                break child.wait().map_err(|e| format!("{e}"))?;
            }
            None => std::thread::sleep(POLL),
        }
    };

    let deadline = Instant::now() + DRAIN_GRACE;
    let (mut stdout, mut stderr) = (Vec::new(), Vec::new());
    for _ in 0..2 {
        match rx.recv_timeout(deadline.saturating_duration_since(Instant::now())) {
            Ok((true, buf)) => stderr = buf,
            Ok((false, buf)) => stdout = buf,
            Err(_) => break,
        }
    }

    Ok(Ran {
        status,
        stdout,
        stderr,
        timed_out,
    })
}

/// The two pipe types differ only in name; this lets one thread body read
/// either without boxing a trait object.
enum Pipe {
    Out(std::process::ChildStdout),
    Err(std::process::ChildStderr),
}

impl Read for Pipe {
    fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
        match self {
            Pipe::Out(p) => p.read(buf),
            Pipe::Err(p) => p.read(buf),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_prompt_child_is_waited_for_not_timed_out() {
        let mut cmd = Command::new("sh");
        cmd.args(["-c", "printf out; printf err >&2; exit 3"]);
        let ran = run_with_timeout(cmd, Duration::from_secs(30)).unwrap();
        assert!(!ran.timed_out);
        assert_eq!(ran.status.code(), Some(3));
        assert_eq!(ran.stdout, b"out");
        assert_eq!(ran.stderr, b"err");
    }

    #[test]
    fn a_child_that_never_answers_is_killed_and_says_so() {
        let started = Instant::now();
        let mut cmd = Command::new("sh");
        cmd.args(["-c", "sleep 60"]);
        let ran = run_with_timeout(cmd, Duration::from_millis(300)).unwrap();
        assert!(ran.timed_out);
        assert!(!ran.status.success());
        // The bound is a bound: the call returns near the timeout, not near
        // the child's own idea of how long it wanted.
        assert!(
            started.elapsed() < Duration::from_secs(10),
            "{:?}",
            started.elapsed()
        );
    }

    #[test]
    fn output_larger_than_a_pipe_buffer_does_not_deadlock() {
        // 256 KiB is several times the 64 KiB pipe buffer: read after the
        // wait rather than concurrently and this test hangs forever.
        let mut cmd = Command::new("sh");
        cmd.args(["-c", "yes 0123456789abcdef | head -c 262144"]);
        let ran = run_with_timeout(cmd, Duration::from_secs(30)).unwrap();
        assert!(!ran.timed_out, "drained output was misreported as a hang");
        assert_eq!(ran.stdout.len(), 262_144);
    }

    #[test]
    fn a_child_that_cannot_be_spawned_is_an_error_not_a_panic() {
        let cmd = Command::new("/nonexistent/pacrat-test-binary");
        assert!(run_with_timeout(cmd, Duration::from_secs(1)).is_err());
    }

    #[test]
    fn stdin_is_closed_so_a_reader_gets_eof_rather_than_blocking() {
        let mut cmd = Command::new("sh");
        cmd.args(["-c", "cat; echo done"]);
        let ran = run_with_timeout(cmd, Duration::from_secs(30)).unwrap();
        assert!(!ran.timed_out);
        assert_eq!(ran.stdout, b"done\n");
    }
}

//! Bounded child-process execution for benchmark recording.
//!
//! # Contents
//! - [`run_command`] executes one argv vector with optional timeout enforcement
//!   and RSS sampling.
//! - [`RecordedOutput`] retains the real child status and captured byte streams.
//!
//! # Invariants
//! - With neither timeout nor RSS sampling enabled, execution stays on the
//!   direct [`std::process::Command::output`] path.
//! - Timeout polling and RSS sampling use independent cadences.
//! - Stdout and stderr are drained concurrently so a verbose child cannot
//!   deadlock on full pipes.
//! - Descendants which inherit the pipes cannot keep a bounded invocation
//!   waiting past its deadline.
//!
//! # See also
//! - [`crate::BenchmarkOutcome`] for the semantic classification built from a
//!   recorded process result.

use std::io::Read;
use std::process::{Command, ExitStatus, Stdio};
use std::sync::mpsc::{self, Receiver, RecvTimeoutError};
use std::time::{Duration, Instant};

/// Exact process state captured by [`run_command`].
#[derive(Debug)]
pub struct RecordedOutput {
    /// Real child-process exit status after completion or timeout termination.
    pub status: ExitStatus,
    /// Captured stdout bytes, empty when the bounded pipe read times out.
    pub stdout: Vec<u8>,
    /// Captured stderr bytes, empty when the bounded pipe read times out.
    pub stderr: Vec<u8>,
    /// Peak resident bytes when RSS sampling was enabled and observed a value.
    pub peak_rss_bytes: Option<u64>,
    /// Whether the child or its inherited output pipes exceeded the timeout.
    pub timed_out: bool,
}

type OutputReceiver = Receiver<std::io::Result<Vec<u8>>>;

fn output_reader<R>(mut input: R) -> OutputReceiver
where
    R: Read + Send + 'static,
{
    let (sender, receiver) = mpsc::channel();
    std::thread::spawn(move || {
        let mut output = Vec::new();
        let result = input.read_to_end(&mut output).map(|_| output);
        let _ = sender.send(result);
    });
    receiver
}

fn remaining_timeout(started: Instant, timeout_ms: u64) -> Option<Duration> {
    Duration::from_millis(timeout_ms).checked_sub(started.elapsed())
}

fn receive_output(
    receiver: &OutputReceiver,
    started: Instant,
    timeout_ms: Option<u64>,
) -> std::io::Result<Option<Vec<u8>>> {
    match timeout_ms {
        Some(timeout_ms) => {
            let Some(remaining) = remaining_timeout(started, timeout_ms) else {
                return Ok(None);
            };
            match receiver.recv_timeout(remaining) {
                Ok(output) => output.map(Some),
                Err(RecvTimeoutError::Timeout) => Ok(None),
                Err(RecvTimeoutError::Disconnected) => {
                    Err(std::io::Error::other("output reader disconnected"))
                }
            }
        }
        None => receiver
            .recv()
            .map_err(|_| std::io::Error::other("output reader disconnected"))?
            .map(Some),
    }
}

fn poll_interval_ms(timeout_ms: Option<u64>, rss_sample_ms: u64) -> u64 {
    match (timeout_ms, rss_sample_ms) {
        (Some(_), rss) if rss > 0 => rss.min(5),
        (Some(_), _) => 5,
        (None, rss) if rss > 0 => rss,
        (None, _) => 10,
    }
}

/// Run one child command with optional timeout enforcement and RSS sampling.
///
/// `command` is an argv vector whose first element is the executable. A
/// `timeout_ms` of `None` leaves execution unbounded. An `rss_sample_ms` of
/// zero disables RSS polling and, when no timeout is requested, preserves the
/// direct `Command::output` path.
pub fn run_command(
    command: &[String],
    timeout_ms: Option<u64>,
    rss_sample_ms: u64,
) -> std::io::Result<RecordedOutput> {
    if timeout_ms.is_none() && rss_sample_ms == 0 {
        return Command::new(&command[0])
            .args(&command[1..])
            .output()
            .map(|output| RecordedOutput {
                status: output.status,
                stdout: output.stdout,
                stderr: output.stderr,
                peak_rss_bytes: None,
                timed_out: false,
            });
    }

    let mut child = Command::new(&command[0])
        .args(&command[1..])
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()?;
    let stdout_reader = output_reader(child.stdout.take().expect("piped stdout"));
    let stderr_reader = output_reader(child.stderr.take().expect("piped stderr"));

    #[cfg(feature = "rss")]
    let pid = sysinfo::Pid::from_u32(child.id());
    #[cfg(feature = "rss")]
    let mut system = sysinfo::System::new();
    #[cfg(feature = "rss")]
    let mut peak_rss_bytes = 0u64;
    #[cfg(not(feature = "rss"))]
    let peak_rss_bytes = 0u64;
    let started = Instant::now();
    let poll_ms = poll_interval_ms(timeout_ms, rss_sample_ms);
    #[cfg(feature = "rss")]
    let rss_interval = (rss_sample_ms > 0).then(|| Duration::from_millis(rss_sample_ms));
    #[cfg(feature = "rss")]
    let mut next_rss_sample = rss_interval.map(|_| started);
    let mut timed_out = false;
    let status = loop {
        #[cfg(feature = "rss")]
        if let (Some(interval), Some(next_sample)) = (rss_interval, next_rss_sample)
            && Instant::now() >= next_sample
        {
            system.refresh_processes(sysinfo::ProcessesToUpdate::Some(&[pid]), true);
            if let Some(process) = system.process(pid) {
                peak_rss_bytes = peak_rss_bytes.max(process.memory());
            }
            next_rss_sample = Instant::now().checked_add(interval);
        }
        if let Some(status) = child.try_wait()? {
            break status;
        }
        if timeout_ms.is_some_and(|limit| started.elapsed() >= Duration::from_millis(limit)) {
            timed_out = true;
            if let Err(error) = child.kill() {
                if let Some(status) = child.try_wait()? {
                    timed_out = false;
                    break status;
                }
                return Err(error);
            }
            break child.wait()?;
        }
        let poll_interval = Duration::from_millis(poll_ms);
        let sleep_for = timeout_ms
            .and_then(|limit| remaining_timeout(started, limit))
            .map_or(poll_interval, |remaining| remaining.min(poll_interval));
        if !sleep_for.is_zero() {
            std::thread::sleep(sleep_for);
        }
    };

    #[cfg(feature = "rss")]
    if rss_sample_ms > 0 {
        system.refresh_processes(sysinfo::ProcessesToUpdate::Some(&[pid]), true);
        if let Some(process) = system.process(pid) {
            peak_rss_bytes = peak_rss_bytes.max(process.memory());
        }
    }

    let stdout = if timed_out {
        Vec::new()
    } else {
        match receive_output(&stdout_reader, started, timeout_ms)? {
            Some(output) => output,
            None => {
                timed_out = true;
                Vec::new()
            }
        }
    };
    let stderr = if timed_out {
        Vec::new()
    } else {
        match receive_output(&stderr_reader, started, timeout_ms)? {
            Some(output) => output,
            None => {
                timed_out = true;
                Vec::new()
            }
        }
    };
    Ok(RecordedOutput {
        status,
        stdout,
        stderr,
        peak_rss_bytes: (peak_rss_bytes > 0).then_some(peak_rss_bytes),
        timed_out,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn poll_interval_preserves_rss_cadence_without_a_timeout() {
        assert_eq!(poll_interval_ms(None, 250), 250);
        assert_eq!(poll_interval_ms(Some(1_000), 250), 5);
        assert_eq!(poll_interval_ms(Some(1_000), 2), 2);
    }

    #[cfg(unix)]
    #[test]
    fn timeout_does_not_wait_for_descendant_held_pipes() {
        let command = vec![
            "sh".to_owned(),
            "-c".to_owned(),
            "sleep 2 & exit 0".to_owned(),
        ];
        let started = Instant::now();
        let output = run_command(&command, Some(50), 0).unwrap();
        assert!(output.timed_out);
        assert!(started.elapsed() < Duration::from_millis(1_000));
    }
}

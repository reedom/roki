//! Idle-stdout watchdog. Used by both `CommandPhaseExecutor` (per
//! invocation) and `SessionSupervisor` (per cycle).
//!
//! Contract: callers update `tick_stdout()` on every byte that arrives on
//! stdout. `run` polls the elapsed-since-last-byte interval and signals the
//! child if it exceeds `stall_seconds`. SIGTERM is sent first; after a
//! fixed `GRACE_PERIOD` (5 s) SIGKILL is sent if the process is still alive.

#![allow(dead_code)]

use std::sync::Arc;
use std::sync::atomic::{AtomicU32, AtomicU64, Ordering};
use std::time::Duration;

use nix::sys::signal::{Signal, kill};
use nix::unistd::Pid;
use tokio::process::Child;
use tokio::time::Instant;

/// Hard-coded grace period between SIGTERM and SIGKILL. Per fr:04 §126
/// ("waits up to a fixed grace period").
pub const GRACE_PERIOD: Duration = Duration::from_secs(5);

/// Polling interval for the watchdog. 250 ms is fine-grained enough that an
/// operator stall window of `1 s` (the validated minimum) still terminates
/// the child within ~250 ms of the boundary.
const POLL_INTERVAL: Duration = Duration::from_millis(250);

/// Outcome of the watchdog's `run` loop.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StallOutcome {
    /// Subprocess exited cleanly (the watchdog observed `try_wait` reporting
    /// an exit before any stall fired).
    Healthy,
    /// Stall fired; the watchdog signalled SIGTERM (and SIGKILL after grace
    /// if the child did not exit). Caller should treat the phase as
    /// `FailureKind::Stall`.
    StalledThenTerminated,
}

#[derive(Clone)]
pub struct Watchdog {
    last_stdout_ms: Arc<AtomicU64>,
    stall_seconds: Arc<AtomicU32>,
    started: Instant,
}

impl Watchdog {
    pub fn new(stall_seconds: u32) -> Self {
        Self {
            last_stdout_ms: Arc::new(AtomicU64::new(0)),
            stall_seconds: Arc::new(AtomicU32::new(stall_seconds)),
            started: Instant::now(),
        }
    }

    /// Update the last-stdout-byte timestamp. Called by the stdout reader
    /// on every byte (or every line — bytes-per-line granularity is fine
    /// because the resolution is far below `stall_seconds`).
    pub fn tick_stdout(&self) {
        let elapsed = self.started.elapsed().as_millis() as u64;
        self.last_stdout_ms.store(elapsed, Ordering::Relaxed);
    }

    /// Mutate the stall window mid-flight. Used by `SessionSupervisor` when
    /// the active phase carries a per-file `stall_seconds` override.
    pub fn set_stall_seconds(&self, seconds: u32) {
        self.stall_seconds.store(seconds, Ordering::Relaxed);
    }

    /// Returns `true` if the elapsed-since-last-byte interval exceeds the
    /// configured stall window. Used by `SessionSupervisor`'s stall task to
    /// poll without taking ownership of the child.
    pub fn is_stalled(&self) -> bool {
        let stall_ms = (self.stall_seconds.load(Ordering::Relaxed) as u64) * 1000;
        let elapsed_ms = self.started.elapsed().as_millis() as u64;
        let last = self.last_stdout_ms.load(Ordering::Relaxed);
        let idle_ms = elapsed_ms.saturating_sub(last);
        idle_ms > stall_ms
    }

    /// Run the watchdog until either the child exits cleanly (`Healthy`) or
    /// the stall window elapses and the watchdog terminates the child
    /// (`StalledThenTerminated`).
    pub async fn run(&self, child: &mut Child) -> StallOutcome {
        let mut interval = tokio::time::interval(POLL_INTERVAL);
        interval.tick().await; // first tick is instant; skip
        loop {
            interval.tick().await;
            if let Ok(Some(_)) = child.try_wait() {
                return StallOutcome::Healthy;
            }

            let stall_ms = (self.stall_seconds.load(Ordering::Relaxed) as u64) * 1000;
            let elapsed_ms = self.started.elapsed().as_millis() as u64;
            let last = self.last_stdout_ms.load(Ordering::Relaxed);
            let idle_ms = elapsed_ms.saturating_sub(last);
            if idle_ms > stall_ms {
                terminate_child(child).await;
                return StallOutcome::StalledThenTerminated;
            }
        }
    }
}

pub(crate) async fn terminate_child(child: &mut Child) {
    if let Some(pid) = child.id() {
        let _ = kill(Pid::from_raw(pid as i32), Signal::SIGTERM);
    }
    let deadline = Instant::now() + GRACE_PERIOD;
    loop {
        if let Ok(Some(_)) = child.try_wait() {
            return;
        }
        if Instant::now() >= deadline {
            break;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    let _ = child.kill().await;
    let _ = child.wait().await;
}

/// Public alias for callers outside this module.
pub async fn terminate_child_external(child: &mut Child) {
    terminate_child(child).await
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::process::Command;

    fn sleep_child(seconds: u64) -> Child {
        Command::new("sh")
            .arg("-c")
            .arg(format!("sleep {seconds}"))
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped())
            .spawn()
            .expect("spawn sleep")
    }

    #[tokio::test]
    async fn watchdog_stalls_on_idle_child() {
        let wd = Watchdog::new(1);
        let mut child = sleep_child(30);
        let outcome = wd.run(&mut child).await;
        assert_eq!(outcome, StallOutcome::StalledThenTerminated);
    }

    #[tokio::test]
    async fn watchdog_healthy_when_child_exits_first() {
        let wd = Watchdog::new(30);
        let mut child = Command::new("sh")
            .arg("-c")
            .arg("sleep 0.2")
            .stdout(std::process::Stdio::piped())
            .spawn()
            .unwrap();
        let outcome = wd.run(&mut child).await;
        assert_eq!(outcome, StallOutcome::Healthy);
    }

    #[tokio::test]
    async fn watchdog_resets_on_stdout_byte() {
        let wd = Watchdog::new(1);
        let mut child = Command::new("sh")
            .arg("-c")
            .arg("sleep 2")
            .stdout(std::process::Stdio::piped())
            .spawn()
            .unwrap();
        let wd_clone = wd.clone();
        let ticker = tokio::spawn(async move {
            let deadline = Instant::now() + Duration::from_millis(1900);
            while Instant::now() < deadline {
                wd_clone.tick_stdout();
                tokio::time::sleep(Duration::from_millis(200)).await;
            }
        });
        let outcome = wd.run(&mut child).await;
        let _ = ticker.await;
        assert_eq!(outcome, StallOutcome::Healthy);
    }

    #[tokio::test]
    async fn watchdog_set_stall_seconds_takes_effect() {
        let wd = Watchdog::new(30);
        let mut child = sleep_child(30);
        wd.set_stall_seconds(1);
        let outcome = wd.run(&mut child).await;
        assert_eq!(outcome, StallOutcome::StalledThenTerminated);
    }
}

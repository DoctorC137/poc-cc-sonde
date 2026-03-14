use std::process::Output;
use std::time::Duration;
use tokio::process::Command;
use tracing::{debug, error, info, warn};
#[cfg(unix)]
extern crate libc;

/// RAII guard: sends SIGKILL to the entire process group on drop, if armed.
/// Armed by default; disarm on normal completion to preserve intentional background jobs.
/// Fires on timeout or cancellation (abort()) to clean up leaked processes.
#[cfg(unix)]
struct ProcessGroupKillOnDrop {
    pgid: libc::pid_t,
    armed: bool,
}

#[cfg(unix)]
impl ProcessGroupKillOnDrop {
    fn new(pgid: libc::pid_t) -> Self {
        Self { pgid, armed: true }
    }
    fn disarm(&mut self) {
        self.armed = false;
    }
}

#[cfg(unix)]
impl Drop for ProcessGroupKillOnDrop {
    fn drop(&mut self) {
        if self.armed {
            // SAFETY: kill(2) with a negative PID targets the process group.
            // ESRCH is silently ignored if the group already exited.
            unsafe { libc::kill(-(self.pgid), libc::SIGKILL) };
        }
    }
}

pub async fn execute_command(
    command: &str,
    timeout_seconds: u64,
    log_output: bool,
) -> Result<Output, Box<dyn std::error::Error>> {
    // Log at debug only: the command string may contain tokens or passwords
    debug!(
        command = %command,
        timeout_seconds = timeout_seconds,
        "Executing command"
    );

    if command.trim().is_empty() {
        return Err("Empty command".into());
    }

    // Spawn through shell to support &&, ||, ;, pipes, etc.
    // kill_on_drop(true) ensures the shell is killed when the Child handle is dropped.
    // process_group(0) places the child in its own process group so that SIGKILL on the
    // group also reaches grandchildren (pipelines, sub-shells) on timeout.
    let mut cmd = Command::new("sh");
    cmd.args(["-c", command]).kill_on_drop(true);
    #[cfg(unix)]
    cmd.process_group(0);
    let child = cmd.spawn().map_err(|e| {
        error!(error = %e, "Failed to spawn command");
        e
    })?;

    // Capture PGID before moving `child` into wait_with_output.
    // On Unix with process_group(0), PGID == child PID.
    #[cfg(unix)]
    let pgid = child.id();

    // Guard: kills the entire process group on drop, regardless of how the future ends
    // (normal completion, timeout, or abort() cancellation).
    #[cfg(unix)]
    let mut _pgkill = pgid.map(|pid| ProcessGroupKillOnDrop::new(pid as libc::pid_t));

    let output = match tokio::time::timeout(
        Duration::from_secs(timeout_seconds),
        child.wait_with_output(),
    )
    .await
    {
        Ok(Ok(output)) => {
            // Normal completion — disarm to preserve intentional background jobs (e.g. daemonised via &)
            #[cfg(unix)]
            if let Some(ref mut g) = _pgkill {
                g.disarm();
            }
            output
        }
        Ok(Err(e)) => {
            error!(error = %e, "Failed to wait for command");
            return Err(e.into());
        }
        Err(_) => {
            error!(
                timeout_seconds = timeout_seconds,
                "Command execution timed out"
            );
            // _pgkill guard fires SIGKILL to the process group via Drop
            return Err("Command execution timed out".into());
        }
    };

    let exit_code = output.status.code().unwrap_or(-1);

    if output.status.success() {
        if log_output {
            info!(exit_code = exit_code, "Command executed successfully");
        }
    } else if log_output {
        // stderr only — stdout may contain sensitive data.
        // Cap at MAX_STDERR_LOG_BYTES to prevent log flooding.
        const MAX_STDERR_LOG_BYTES: usize = 512;
        let stderr_raw = &output.stderr;
        let capped = if stderr_raw.len() > MAX_STDERR_LOG_BYTES {
            &stderr_raw[..MAX_STDERR_LOG_BYTES]
        } else {
            stderr_raw.as_slice()
        };
        let stderr = String::from_utf8_lossy(capped);
        warn!(
            exit_code = exit_code,
            stderr = %stderr.trim(),
            stderr_truncated = output.stderr.len() > MAX_STDERR_LOG_BYTES,
            "Command executed with non-zero exit code"
        );
    }

    Ok(output)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    #[cfg(unix)]
    async fn test_grandchild_killed_on_task_abort() {
        use std::time::Duration;

        // The shell itself blocks (sleeps 60s after writing the grandchild PID),
        // so execute_command never returns Ok(Ok(_)) before select! cancels it.
        // Cancellation drops the future, which drops _pgkill with armed=true → SIGKILL.
        let pid_file = format!("/tmp/test_pgkill_abort_{}", std::process::id());
        let cmd = format!("sleep 60 & echo $! > {}; sleep 60", pid_file);

        tokio::select! {
            _ = execute_command(&cmd, 30, false) => {}
            // Give the grandchild time to start and write its PID, then cancel
            _ = tokio::time::sleep(Duration::from_millis(200)) => {}
        }

        // Wait for SIGKILL to propagate
        tokio::time::sleep(Duration::from_millis(200)).await;

        if let Ok(content) = std::fs::read_to_string(&pid_file) {
            let _ = std::fs::remove_file(&pid_file);
            let pid: libc::pid_t = content.trim().parse().unwrap_or(-1);
            if pid > 0 {
                // kill -0 checks process existence without sending a signal
                let alive = unsafe { libc::kill(pid, 0) == 0 };
                assert!(!alive, "Grandchild (PID {}) must be dead after cancellation", pid);
            }
        }
    }

    #[tokio::test]
    #[cfg(unix)]
    async fn test_grandchild_alive_after_normal_completion() {
        use std::time::Duration;

        // The shell exits immediately after backgrounding sleep and writing the PID.
        // execute_command returns Ok(Ok(_)) → guard is disarmed → grandchild survives.
        let pid_file = format!("/tmp/test_pgkill_normal_{}", std::process::id());
        let cmd = format!("sleep 60 & echo $! > {}", pid_file);

        let result = execute_command(&cmd, 5, false).await;
        assert!(result.is_ok(), "execute_command should succeed");

        // Short delay to ensure the PID file is flushed
        tokio::time::sleep(Duration::from_millis(50)).await;

        let content = std::fs::read_to_string(&pid_file)
            .expect("PID file must exist after normal completion");
        let _ = std::fs::remove_file(&pid_file);
        let pid: libc::pid_t = content.trim().parse().expect("PID file must contain a valid PID");

        let alive = unsafe { libc::kill(pid, 0) == 0 };
        assert!(alive, "Grandchild (PID {}) must still be alive after normal completion", pid);

        // Cleanup: kill the grandchild we intentionally left running
        unsafe { libc::kill(pid, libc::SIGKILL) };
    }
}

use crate::config::Probe;
use crate::executor;
use crate::healthcheck_probe;
use crate::persistence::{self, PersistenceBackend, ProbeState};
use std::sync::Arc;
use std::time::Duration;
use tokio::time;
use tracing::{debug, error, info};

pub async fn schedule_probe(probe: Probe, backend: Arc<dyn PersistenceBackend>) {
    info!(
        probe_name = %probe.name,
        interval_seconds = probe.interval_seconds,
        delay_after_success = probe.delay_after_success_seconds,
        delay_after_failure = probe.delay_after_failure_seconds,
        delay_after_command_success = probe.delay_after_command_success_seconds,
        delay_after_command_failure = probe.delay_after_command_failure_seconds,
        "Starting probe scheduler"
    );

    // Load previous state if exists
    let previous_state = backend.load_state(&probe.name).await.ok().flatten();

    let mut next_delay = match &previous_state {
        Some(state) => {
            let now = persistence::current_timestamp();
            if state.next_check_timestamp > now {
                let remaining = state.next_check_timestamp - now;
                info!(
                    probe_name = %probe.name,
                    remaining_seconds = remaining,
                    last_success = state.last_check_success,
                    consecutive_failures = state.consecutive_failures,
                    "Resuming from saved state"
                );
                remaining
            } else {
                info!(
                    probe_name = %probe.name,
                    consecutive_failures = state.consecutive_failures,
                    "Saved state expired, starting immediately"
                );
                0
            }
        }
        None => {
            info!(
                probe_name = %probe.name,
                "No previous state found, starting immediately"
            );
            0
        }
    };

    let mut consecutive_failures = previous_state
        .as_ref()
        .map(|s| s.consecutive_failures)
        .unwrap_or(0);

    loop {
        // Wait for the calculated delay
        if next_delay > 0 {
            debug!(
                probe_name = %probe.name,
                delay_seconds = next_delay,
                "Waiting before next execution"
            );
            time::sleep(Duration::from_secs(next_delay)).await;
        }

        info!(
            probe_name = %probe.name,
            "Executing scheduled probe"
        );

        let check_timestamp = persistence::current_timestamp();
        let (success, command_executed, command_succeeded) = match healthcheck_probe::execute_probe(&probe).await {
            Ok(_) => {
                info!(
                    probe_name = %probe.name,
                    "Probe succeeded"
                );
                // Reset consecutive failures on success
                consecutive_failures = 0;
                (true, false, false)
            }
            Err(failure) => {
                // Increment consecutive failures
                consecutive_failures += 1;

                error!(
                    probe_name = %probe.name,
                    failure = %failure,
                    consecutive_failures = consecutive_failures,
                    "Probe failed"
                );

                let mut command_executed = false;
                let mut command_succeeded = false;

                // Execute failure command if configured and threshold reached
                if let Some(ref command) = probe.on_failure_command {
                    let retry_threshold = probe.get_failure_retries_before_command();

                    if consecutive_failures > retry_threshold {
                        command_executed = true;

                        // Substitute ${APP_ID} if an app is configured
                        let app_id = probe.apps.first().map(|a| a.id.as_str());
                        let command = if let Some(id) = app_id {
                            command.replace("${APP_ID}", id)
                        } else {
                            command.clone()
                        };

                        info!(
                            probe_name = %probe.name,
                            command = %command,
                            consecutive_failures = consecutive_failures,
                            threshold = retry_threshold,
                            "Failure threshold reached, executing command"
                        );

                        match executor::execute_command(&command, probe.command_timeout_seconds).await {
                            Ok(output) => {
                                if output.status.success() {
                                    command_succeeded = true;
                                    info!(
                                        probe_name = %probe.name,
                                        "Failure command completed successfully"
                                    );
                                } else {
                                    error!(
                                        probe_name = %probe.name,
                                        exit_code = output.status.code().unwrap_or(-1),
                                        "Failure command completed with errors"
                                    );
                                }
                            }
                            Err(e) => {
                                error!(
                                    probe_name = %probe.name,
                                    error = %e,
                                    "Failed to execute failure command"
                                );
                            }
                        }
                    } else {
                        info!(
                            probe_name = %probe.name,
                            consecutive_failures = consecutive_failures,
                            threshold = retry_threshold,
                            remaining_retries = retry_threshold.saturating_sub(consecutive_failures),
                            "Failure threshold not reached, retrying without command"
                        );
                    }
                }
                (false, command_executed, command_succeeded)
            }
        };

        // Calculate next delay based on success/failure and command execution
        next_delay = if success {
            probe.get_delay_after_success()
        } else if command_executed {
            if command_succeeded {
                probe.get_delay_after_command_success()
            } else {
                probe.get_delay_after_command_failure()
            }
        } else {
            probe.get_delay_after_failure()
        };

        // Save state
        let state = ProbeState {
            probe_name: probe.name.clone(),
            last_check_timestamp: check_timestamp,
            last_check_success: success,
            next_check_timestamp: check_timestamp + next_delay,
            consecutive_failures,
        };

        if let Err(e) = backend.save_state(&state).await {
            error!(
                probe_name = %probe.name,
                error = %e,
                "Failed to save state"
            );
        }

        debug!(
            probe_name = %probe.name,
            next_delay_seconds = next_delay,
            success = success,
            "Scheduled next execution"
        );
    }
}

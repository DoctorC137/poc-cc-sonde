use crate::config::Probe;
use crate::executor;
use crate::persistence::{self, PersistenceBackend, ProbeState};
use crate::probe;
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
        "Starting probe scheduler"
    );

    // Load previous state if exists
    let mut next_delay = match backend.load_state(&probe.name).await {
        Ok(Some(state)) => {
            let now = persistence::current_timestamp();
            if state.next_check_timestamp > now {
                let remaining = state.next_check_timestamp - now;
                info!(
                    probe_name = %probe.name,
                    remaining_seconds = remaining,
                    last_success = state.last_check_success,
                    "Resuming from saved state"
                );
                remaining
            } else {
                info!(
                    probe_name = %probe.name,
                    "Saved state expired, starting immediately"
                );
                0
            }
        }
        Ok(None) => {
            info!(
                probe_name = %probe.name,
                "No previous state found, starting immediately"
            );
            0
        }
        Err(e) => {
            error!(
                probe_name = %probe.name,
                error = %e,
                "Failed to load state, starting immediately"
            );
            0
        }
    };

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
        let success = match probe::execute_probe(&probe).await {
            Ok(_) => {
                info!(
                    probe_name = %probe.name,
                    "Probe succeeded"
                );
                true
            }
            Err(failure) => {
                error!(
                    probe_name = %probe.name,
                    failure = %failure,
                    "Probe failed"
                );

                // Execute failure command if configured
                if let Some(ref command) = probe.on_failure_command {
                    info!(
                        probe_name = %probe.name,
                        command = %command,
                        "Executing failure command"
                    );

                    match executor::execute_command(command, probe.command_timeout_seconds).await {
                        Ok(output) => {
                            if output.status.success() {
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
                }
                false
            }
        };

        // Calculate next delay based on success/failure
        next_delay = if success {
            probe.get_delay_after_success()
        } else {
            probe.get_delay_after_failure()
        };

        // Save state
        let state = ProbeState {
            probe_name: probe.name.clone(),
            last_check_timestamp: check_timestamp,
            last_check_success: success,
            next_check_timestamp: check_timestamp + next_delay,
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

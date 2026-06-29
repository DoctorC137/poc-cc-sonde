use crate::config::Probe;
use crate::executor;
use crate::healthcheck_probe;
use crate::persistence::{self, PersistenceBackend, ProbeState};
use std::sync::Arc;
use std::time::Duration;
use tokio::time;
use tracing::{debug, error, info, warn};

pub async fn schedule_probe(
    probe: Probe,
    backend: Arc<dyn PersistenceBackend>,
    dry_run: bool,
    multi_instance: bool,
) {
    // Build the HTTP client once and reuse across all iterations
    let client = match healthcheck_probe::build_client() {
        Ok(c) => c,
        Err(e) => {
            error!(probe_name = %probe.name, error = %e, "Failed to build HTTP client");
            return;
        }
    };
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
    let previous_state = match backend.load_state(&probe.name).await {
        Ok(state) => state,
        Err(e) => {
            warn!(probe_name = %probe.name, error = %e,
                  "Failed to load initial state, starting fresh");
            None
        }
    };

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

        let lock_key = format!("poc-sonde:lock:probe:{}", probe.name);
        let ttl_ms = (probe.interval_seconds
            + probe.get_request_timeout()
            + probe.command_timeout_seconds
            + 10) * 1000;

        let lock_token = match backend.acquire_lock(&lock_key, ttl_ms).await {
            Ok(None) => {
                debug!(probe_name = %probe.name, "Lock held by another instance, skipping cycle");
                next_delay = probe.get_delay_after_success();
                continue;
            }
            Err(e) => {
                if multi_instance {
                    // Fail-closed: skipping this cycle preserves mutual exclusion guarantee
                    error!(probe_name = %probe.name, error = %e,
                           "Lock acquisition failed in multi-instance mode, skipping cycle");
                    next_delay = probe.get_delay_after_success();
                    continue;
                }
                // Single-instance: fail-open (backward-compatible)
                warn!(probe_name = %probe.name, error = %e,
                      "Lock acquisition failed, proceeding without lock");
                None
            }
            Ok(Some(token)) => Some(token),
        };

        // Refresh state from Redis after acquiring lock to sync with other instances.
        // `Box<dyn Error>` (!Send) is converted to String before any await.
        let mut refresh_failed_multi = false;
        let mut skip_with_delay: Option<u64> = None;
        match backend.load_state(&probe.name).await {
            Ok(Some(fresh_state)) => {
                consecutive_failures = fresh_state.consecutive_failures;
                let now = persistence::current_timestamp();
                if fresh_state.next_check_timestamp > now {
                    skip_with_delay = Some(fresh_state.next_check_timestamp - now);
                }
            }
            Ok(None) => {}
            Err(e) => {
                let e_str = e.to_string();
                if multi_instance {
                    error!(probe_name = %probe.name, error = %e_str,
                           "Failed to refresh state from Redis, skipping cycle (fail-close)");
                    refresh_failed_multi = true;
                } else {
                    warn!(probe_name = %probe.name, error = %e_str,
                          "Failed to refresh state, proceeding with cached values");
                }
            }
        }
        if refresh_failed_multi {
            if let Some(ref t) = lock_token {
                let _ = backend.release_lock(&lock_key, t).await;
            }
            next_delay = probe.get_delay_after_failure();
            continue;
        }
        if let Some(remaining) = skip_with_delay {
            // Another instance ran more recently; respect its scheduled next check.
            debug!(
                probe_name = %probe.name,
                remaining_seconds = remaining,
                "State refreshed: another instance holds a future check timestamp, releasing lock"
            );
            if let Some(ref t) = lock_token {
                let _ = backend.release_lock(&lock_key, t).await;
            }
            next_delay = remaining;
            continue;
        }

        info!(
            probe_name = %probe.name,
            "Executing scheduled probe"
        );

        let check_timestamp = persistence::current_timestamp();
        let (success, command_executed, command_succeeded) = match healthcheck_probe::execute_probe(
            &probe, &client,
        )
        .await
        {
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

                        warn!(
                            probe_name = %probe.name,
                            consecutive_failures = consecutive_failures,
                            threshold = retry_threshold,
                            "Failure threshold reached, executing command"
                        );
                        debug!(command = %command, "Failure command detail");

                        if dry_run {
                            warn!(
                                probe_name = %probe.name,
                                "DRY RUN: skipping failure command"
                            );
                            debug!(command = %command, "DRY RUN command detail");
                            command_succeeded = true;
                        } else {
                            match executor::execute_command(&command, probe.command_timeout_seconds, !probe.suppress_command_output)
                                .await
                            {
                                Ok(output) => {
                                    if output.status.success() {
                                        command_succeeded = true;
                                        warn!(
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
            next_check_timestamp: persistence::current_timestamp() + next_delay,
            consecutive_failures,
        };

        if let Err(e) = backend.save_state(&state).await {
            error!(
                probe_name = %probe.name,
                error = %e,
                "Failed to save state"
            );
        }

        if let Some(ref token) = lock_token {
            if let Err(e) = backend.release_lock(&lock_key, token).await {
                debug!(probe_name = %probe.name, error = %e, "Failed to release lock (will expire via TTL)");
            }
        }

        debug!(
            probe_name = %probe.name,
            next_delay_seconds = next_delay,
            success = success,
            "Scheduled next execution"
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::Checks;
    use crate::persistence::FailingLockBackend;
    use std::sync::Arc;

    fn test_probe(url: String) -> Probe {
        Probe {
            name: "lock-test".to_string(),
            url: Some(url),
            interval_seconds: 1,
            checks: Checks {
                expected_status: Some(200),
                expected_body_contains: None,
                expected_body_regex: None,
                expected_header: None,
                compiled_body_regex: None,
            },
            on_failure_command: None,
            command_timeout_seconds: 5,
            delay_after_success_seconds: None,
            delay_after_failure_seconds: None,
            delay_after_command_success_seconds: None,
            delay_after_command_failure_seconds: None,
            failure_retries_before_command: None,
            request_timeout_seconds: Some(1),
            suppress_command_output: false,
            apps: vec![],
        }
    }

    // multi_instance=true + lock error → probe skipped, 0 HTTP requests
    #[tokio::test]
    async fn test_lock_error_skips_cycle_when_multi_instance() {
        let mut server = mockito::Server::new_async().await;
        let mock = server.mock("GET", "/").with_status(200).create_async().await;

        let backend: Arc<dyn PersistenceBackend> = Arc::new(FailingLockBackend::new());
        let handle = tokio::spawn(schedule_probe(
            test_probe(server.url()),
            backend,
            false,
            true, // multi_instance=true
        ));
        tokio::time::sleep(std::time::Duration::from_millis(300)).await;
        handle.abort();

        mock.expect(0).assert_async().await;
    }

    // multi_instance=false + lock error → probe executed (fail-open), ≥1 HTTP requests
    #[tokio::test]
    async fn test_lock_error_proceeds_when_single_instance() {
        let mut server = mockito::Server::new_async().await;
        let mock = server
            .mock("GET", "/")
            .with_status(200)
            .expect_at_least(1)
            .create_async()
            .await;

        let backend: Arc<dyn PersistenceBackend> = Arc::new(FailingLockBackend::new());
        let handle = tokio::spawn(schedule_probe(
            test_probe(server.url()),
            backend,
            false,
            false, // multi_instance=false
        ));
        tokio::time::sleep(std::time::Duration::from_millis(300)).await;
        handle.abort();

        mock.assert_async().await;
    }
}

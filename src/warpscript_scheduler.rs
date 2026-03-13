use crate::config::WarpScriptProbe;
use crate::executor;
use crate::persistence::{self, PersistenceBackend, WarpScriptProbeState};
use crate::warpscript_probe;
use std::env;
use std::fs;
use std::sync::Arc;
use std::time::Duration;
use tokio::time;
use tracing::{debug, error, info, warn};

/// Execute a command with app_id substitution.
/// Returns `true` if the command succeeded, `false` otherwise.
pub(crate) async fn execute_scaling_command(
    probe_name: &str,
    command: &str,
    app_id: Option<&str>,
    timeout_seconds: u64,
    action: &str, // "upscale" or "downscale"
) -> bool {
    // Substitute ${APP_ID} if present
    let cmd = if let Some(id) = app_id {
        command.replace("${APP_ID}", id)
    } else {
        command.to_string()
    };

    warn!(probe_name = %probe_name, action = %action, "Executing {} command", action);
    debug!(command = %cmd, "Scaling command detail");

    match executor::execute_command(&cmd, timeout_seconds).await {
        Ok(output) if output.status.success() => {
            warn!(
                probe_name = %probe_name,
                "{} command completed successfully", action
            );
            true
        }
        Ok(output) => {
            error!(
                probe_name = %probe_name,
                exit_code = output.status.code().unwrap_or(-1),
                "{} command completed with errors", action
            );
            false
        }
        Err(e) => {
            error!(
                probe_name = %probe_name,
                error = %e,
                "Failed to execute {} command", action
            );
            false
        }
    }
}

pub async fn schedule_warpscript_probe(
    probe: WarpScriptProbe,
    backend: Arc<dyn PersistenceBackend>,
    dry_run: bool,
    multi_instance: bool,
) {
    // Build the HTTP client once and reuse across all iterations
    let client = match warpscript_probe::build_client() {
        Ok(c) => c,
        Err(e) => {
            error!(probe_name = %probe.name, error = %e, "Failed to build HTTP client");
            return;
        }
    };

    info!(
        probe_name = %probe.name,
        interval_seconds = probe.interval_seconds,
        levels_count = probe.levels.len(),
        min_level = probe.min_level(),
        max_level = probe.max_level(),
        "Starting WarpScript probe scheduler"
    );

    // Validate levels
    if probe.levels.is_empty() {
        error!(
            probe_name = %probe.name,
            "No levels defined for WarpScript probe"
        );
        return;
    }

    // Load previous state if exists
    let previous_state = backend
        .load_warpscript_state(&probe.name)
        .await
        .ok()
        .flatten();

    let (mut current_level, mut next_delay) = match &previous_state {
        Some(state) => {
            let now = persistence::current_timestamp();
            let delay = if state.next_check_timestamp > now {
                let remaining = state.next_check_timestamp - now;
                info!(
                    probe_name = %probe.name,
                    remaining_seconds = remaining,
                    current_level = state.current_level,
                    last_value = state.last_value,
                    "Resuming WarpScript probe from saved state"
                );
                remaining
            } else {
                info!(
                    probe_name = %probe.name,
                    current_level = state.current_level,
                    last_value = state.last_value,
                    "Saved state expired, starting immediately"
                );
                0
            };
            let loaded_level = state.current_level;
            let current_level = if probe.get_level(loaded_level).is_some() {
                loaded_level
            } else {
                let clamped = probe.min_level();
                warn!(
                    probe_name = %probe.name,
                    loaded = loaded_level,
                    clamped,
                    "Loaded level not in config, resetting to min"
                );
                clamped
            };
            (current_level, delay)
        }
        None => {
            // Start at minimum level
            let initial_level = probe.min_level();
            info!(
                probe_name = %probe.name,
                initial_level = initial_level,
                "No previous state found, starting immediately"
            );
            (initial_level, 0)
        }
    };

    let mut consecutive_failures: u32 = previous_state
        .as_ref()
        .map(|s| s.consecutive_failures)
        .unwrap_or(0);

    let mut last_value: f64 = previous_state
        .as_ref()
        .map(|s| s.last_value)
        .unwrap_or(0.0);

    // Resolve environment variables once before the loop (stable for process lifetime)
    let endpoint = match env::var("WARP_ENDPOINT") {
        Ok(v) => v,
        Err(_) => {
            error!(probe_name = %probe.name, "WARP_ENDPOINT environment variable not set");
            return;
        }
    };
    let fallback_token = env::var("WARP_TOKEN").ok();

    // Read the WarpScript file once before the loop to avoid repeated disk I/O.
    // Retry on transient errors (file not yet mounted, wrong permissions at startup).
    let script_content = loop {
        match fs::read_to_string(&probe.warpscript_file) {
            Ok(content) => break content,
            Err(e) => {
                error!(
                    probe_name = %probe.name,
                    file = %probe.warpscript_file,
                    error = %e,
                    retry_in_seconds = probe.interval_seconds,
                    "Failed to read WarpScript file, will retry"
                );
                time::sleep(Duration::from_secs(probe.interval_seconds)).await;
            }
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

        let lock_key = format!("poc-sonde:lock:warpscript:{}", probe.name);
        // TTL = HTTP timeout + remote execution timeout + safety margin.
        // Using the probe's actual HTTP timeout avoids the lock expiring mid-request
        // when interval_seconds is shorter than request_timeout_seconds.
        let ttl_ms = (probe.get_request_timeout() + probe.command_timeout_seconds + 10) * 1000;

        let lock_token = match backend.acquire_lock(&lock_key, ttl_ms).await {
            Ok(None) => {
                debug!(probe_name = %probe.name, "Lock held by another instance, skipping cycle");
                next_delay = probe.get_delay_after_scale();
                continue;
            }
            Err(e) => {
                if multi_instance {
                    // Fail-closed: skipping this cycle preserves mutual exclusion guarantee
                    error!(probe_name = %probe.name, error = %e,
                           "Lock acquisition failed in multi-instance mode, skipping cycle");
                    next_delay = probe.interval_seconds;
                    continue;
                }
                // Single-instance: fail-open (backward-compatible)
                warn!(probe_name = %probe.name, error = %e,
                      "Lock acquisition failed, proceeding without lock");
                None
            }
            Ok(Some(token)) => Some(token),
        };

        info!(
            probe_name = %probe.name,
            current_level = current_level,
            "Executing WarpScript probe"
        );

        let check_timestamp = persistence::current_timestamp();

        // Get app (should have exactly one if expanded correctly)
        let app = probe.apps.first();
        let app_id = app.map(|a| a.id.as_str());

        // Resolve effective token: per-app override takes precedence over env fallback
        let token = match app.and_then(|a| a.warp_token.as_deref()).or(fallback_token.as_deref()) {
            Some(t) => t,
            None => {
                error!(
                    probe_name = %probe.name,
                    "No Warp token available (neither app warp_token nor WARP_TOKEN env var set)"
                );
                if let Some(ref token) = lock_token {
                    if let Err(e) = backend.release_lock(&lock_key, token).await {
                        debug!(probe_name = %probe.name, error = %e, "Failed to release lock (will expire via TTL)");
                    }
                }
                next_delay = probe.interval_seconds;
                continue;
            }
        };

        // Execute WarpScript and get value
        let value = match warpscript_probe::execute_warpscript(
            &probe.name,
            &script_content,
            app_id,
            token,
            &endpoint,
            probe.get_request_timeout(),
            &client,
        )
        .await
        {
            Ok(v) => {
                info!(
                    probe_name = %probe.name,
                    value = v,
                    current_level = current_level,
                    "WarpScript execution successful"
                );
                consecutive_failures = 0;
                last_value = v;
                v
            }
            Err(e) => {
                consecutive_failures += 1;
                error!(
                    probe_name = %probe.name,
                    error = %e,
                    consecutive_failures,
                    "WarpScript execution failed"
                );

                next_delay = probe.interval_seconds;

                if let Some(ref command) = probe.on_failure_command {
                    let threshold = probe.get_failure_retries_before_command();
                    if consecutive_failures > threshold {
                        let cmd = if let Some(id) = app_id {
                            command.replace("${APP_ID}", id)
                        } else {
                            command.clone()
                        };
                        warn!(
                            probe_name = %probe.name,
                            consecutive_failures,
                            threshold,
                            "Failure threshold reached, executing command"
                        );
                        if dry_run {
                            warn!(
                                probe_name = %probe.name,
                                command = %cmd,
                                "DRY RUN: skipping failure command"
                            );
                            next_delay = probe.get_delay_after_command_success();
                        } else {
                            match executor::execute_command(&cmd, probe.command_timeout_seconds).await {
                                Ok(output) if output.status.success() => {
                                    warn!(probe_name = %probe.name, "Failure command completed successfully");
                                    next_delay = probe.get_delay_after_command_success();
                                }
                                Ok(_) => {
                                    error!(probe_name = %probe.name, "Failure command completed with errors");
                                    next_delay = probe.get_delay_after_command_failure();
                                }
                                Err(e) => {
                                    error!(probe_name = %probe.name, error = %e, "Failed to execute failure command");
                                    next_delay = probe.get_delay_after_command_failure();
                                }
                            }
                        }
                    } else {
                        info!(
                            probe_name = %probe.name,
                            consecutive_failures,
                            threshold,
                            remaining = threshold - consecutive_failures,
                            "Failure threshold not reached, retrying without command"
                        );
                    }
                }

                let state = WarpScriptProbeState {
                    probe_name: probe.name.clone(),
                    last_check_timestamp: check_timestamp,
                    current_level,
                    last_value,
                    next_check_timestamp: check_timestamp + next_delay,
                    consecutive_failures,
                };
                if let Err(e) = backend.save_warpscript_state(&state).await {
                    error!(probe_name = %probe.name, error = %e, "Failed to save WarpScript state");
                }

                if let Some(ref token) = lock_token {
                    if let Err(e) = backend.release_lock(&lock_key, token).await {
                        debug!(probe_name = %probe.name, error = %e, "Failed to release lock (will expire via TTL)");
                    }
                }
                continue;
            }
        };

        // Check if we should scale up
        if probe.should_scale_up(current_level, value) {
            let new_level = current_level + 1;
            warn!(
                probe_name = %probe.name,
                from_level = current_level,
                to_level = new_level,
                value = value,
                "Scaling UP detected"
            );

            let command_ok = if let Some(level_config) = probe.get_level(current_level) {
                if let Some(ref cmd) = level_config.upscale_command {
                    if dry_run {
                        warn!(
                            probe_name = %probe.name,
                            command = %cmd,
                            from_level = current_level,
                            to_level = new_level,
                            "DRY RUN: skipping upscale command"
                        );
                        true
                    } else {
                        execute_scaling_command(
                            &probe.name,
                            cmd,
                            app_id,
                            probe.command_timeout_seconds,
                            "upscale",
                        )
                        .await
                    }
                } else {
                    warn!(
                        probe_name = %probe.name,
                        level = current_level,
                        "No upscale command defined for this level — level transition skipped"
                    );
                    false
                }
            } else {
                true
            };

            if command_ok {
                current_level = new_level;
                next_delay = probe.get_delay_after_scale();
            } else {
                warn!(
                    probe_name = %probe.name,
                    current_level,
                    "Scaling command failed — level not updated, will retry at next interval"
                );
                next_delay = probe.interval_seconds;
            }
        }
        // Check if we should scale down
        else if probe.should_scale_down(current_level, value) {
            let new_level = current_level - 1;
            warn!(
                probe_name = %probe.name,
                from_level = current_level,
                to_level = new_level,
                value = value,
                "Scaling DOWN detected"
            );

            let command_ok = if let Some(level_config) = probe.get_level(current_level) {
                if let Some(ref cmd) = level_config.downscale_command {
                    if dry_run {
                        warn!(
                            probe_name = %probe.name,
                            command = %cmd,
                            from_level = current_level,
                            to_level = new_level,
                            "DRY RUN: skipping downscale command"
                        );
                        true
                    } else {
                        execute_scaling_command(
                            &probe.name,
                            cmd,
                            app_id,
                            probe.command_timeout_seconds,
                            "downscale",
                        )
                        .await
                    }
                } else {
                    warn!(
                        probe_name = %probe.name,
                        level = current_level,
                        "No downscale command defined for this level — level transition skipped"
                    );
                    false
                }
            } else {
                true
            };

            if command_ok {
                current_level = new_level;
                next_delay = probe.get_delay_after_scale();
            } else {
                warn!(
                    probe_name = %probe.name,
                    current_level,
                    "Scaling command failed — level not updated, will retry at next interval"
                );
                next_delay = probe.interval_seconds;
            }
        }
        // No scaling needed
        else {
            debug!(
                probe_name = %probe.name,
                level = current_level,
                value = value,
                "No scaling action needed, level unchanged"
            );
            next_delay = probe.interval_seconds;
        }

        // Save state
        let state = WarpScriptProbeState {
            probe_name: probe.name.clone(),
            last_check_timestamp: check_timestamp,
            current_level,
            last_value: value,
            next_check_timestamp: check_timestamp + next_delay,
            consecutive_failures,
        };

        if let Err(e) = backend.save_warpscript_state(&state).await {
            error!(
                probe_name = %probe.name,
                error = %e,
                "Failed to save WarpScript state"
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
            level = current_level,
            "Scheduled next execution"
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn test_scaling_command_failure_returns_false() {
        let ok = execute_scaling_command("test", "exit 1", None, 5, "upscale").await;
        assert!(!ok);
    }

    #[tokio::test]
    async fn test_scaling_command_success_returns_true() {
        let ok = execute_scaling_command("test", "true", None, 5, "upscale").await;
        assert!(ok);
    }

    #[tokio::test]
    async fn test_scaling_command_spawn_error_returns_false() {
        let ok = execute_scaling_command("test", "nonexistent_xyz_cmd_42", None, 5, "upscale").await;
        assert!(!ok);
    }
}

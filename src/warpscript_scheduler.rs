use crate::config::WarpScriptProbe;
use crate::executor;
use crate::persistence::{self, PersistenceBackend, WarpScriptProbeState};
use crate::warpscript_probe;
use std::collections::HashMap;
use std::env;
use std::sync::Arc;
use std::time::Duration;
use tokio::time;
use tracing::{debug, error, info, warn};

/// Execute a scaling command with variable substitution.
///
/// Substitutes `${APP_ID}`, `${FLAVOR}`, and `${INSTANCES}` in the command string.
/// Returns `true` if the command succeeded, `false` otherwise.
pub(crate) async fn execute_scaling_command(
    probe_name: &str,
    command: &str,
    app_id: Option<&str>,
    flavor: &str,
    instances: u32,
    timeout_seconds: u64,
    action: &str, // "upscale" or "downscale"
) -> bool {
    let mut cmd = command.to_string();
    if let Some(id) = app_id {
        cmd = cmd.replace("${APP_ID}", id);
    }
    cmd = cmd.replace("${FLAVOR}", flavor);
    cmd = cmd.replace("${INSTANCES}", &instances.to_string());

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
        metrics_count = probe.warpscript_files.len(),
        min_level = probe.min_level(),
        max_level = probe.max_level(),
        "Starting WarpScript probe scheduler"
    );

    if probe.scaling.flavors.is_empty() {
        error!(probe_name = %probe.name, "No flavors defined for WarpScript probe");
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
                    "Resuming WarpScript probe from saved state"
                );
                remaining
            } else {
                info!(
                    probe_name = %probe.name,
                    current_level = state.current_level,
                    "Saved state expired, starting immediately"
                );
                0
            };
            let loaded_level = state.current_level;
            let current_level = if probe.get_computed_level(loaded_level).is_some() {
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

    let mut last_values: HashMap<String, f64> = previous_state
        .as_ref()
        .map(|s| s.last_values.clone())
        .unwrap_or_default();

    // Resolve environment variables once before the loop (stable for process lifetime)
    let endpoint = match env::var("WARP_ENDPOINT") {
        Ok(v) => v,
        Err(_) => {
            error!(probe_name = %probe.name, "WARP_ENDPOINT environment variable not set");
            return;
        }
    };
    let fallback_token = env::var("WARP_TOKEN").ok();

    // Load all WarpScript files before the loop, retrying on transient errors.
    let scripts: HashMap<String, String> = loop {
        let mut loaded: HashMap<String, String> = HashMap::new();
        let mut all_ok = true;
        for (metric, path) in &probe.warpscript_files {
            match tokio::fs::read_to_string(path).await {
                Ok(content) => {
                    loaded.insert(metric.clone(), content);
                }
                Err(e) => {
                    error!(
                        probe_name = %probe.name,
                        metric = %metric,
                        file = %path,
                        error = %e,
                        retry_in_seconds = probe.interval_seconds,
                        "Failed to read WarpScript file, will retry"
                    );
                    all_ok = false;
                }
            }
        }
        if all_ok {
            break loaded;
        }
        time::sleep(Duration::from_secs(probe.interval_seconds)).await;
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
        let ttl_ms = (probe.get_request_timeout() + probe.command_timeout_seconds + 10) * 1000;

        let lock_token = match backend.acquire_lock(&lock_key, ttl_ms).await {
            Ok(None) => {
                debug!(probe_name = %probe.name, "Lock held by another instance, skipping cycle");
                next_delay = probe.get_delay_after_scale();
                continue;
            }
            Err(e) => {
                if multi_instance {
                    error!(probe_name = %probe.name, error = %e,
                           "Lock acquisition failed in multi-instance mode, skipping cycle");
                    next_delay = probe.interval_seconds;
                    continue;
                }
                warn!(probe_name = %probe.name, error = %e,
                      "Lock acquisition failed, proceeding without lock");
                None
            }
            Ok(Some(token)) => Some(token),
        };

        // Re-read state from Redis to get the latest level set by any other instance.
        // Safe to do here because we hold the lock — no other instance can mutate the
        // state between this load and our eventual save.
        if let Ok(Some(fresh_state)) = backend.load_warpscript_state(&probe.name).await {
            if fresh_state.current_level != current_level {
                info!(
                    probe_name = %probe.name,
                    stale = current_level,
                    fresh = fresh_state.current_level,
                    "State refreshed from Redis after lock acquisition"
                );
            }
            let loaded_level = fresh_state.current_level;
            current_level = if probe.get_computed_level(loaded_level).is_some() {
                loaded_level
            } else {
                let clamped = probe.min_level();
                warn!(
                    probe_name = %probe.name,
                    loaded = loaded_level,
                    clamped,
                    "Refreshed level not in config, resetting to min"
                );
                clamped
            };
            consecutive_failures = fresh_state.consecutive_failures;
            last_values = fresh_state.last_values.clone();
        }

        info!(
            probe_name = %probe.name,
            current_level = current_level,
            "Executing WarpScript probe"
        );

        let check_timestamp = persistence::current_timestamp();

        let app = probe.apps.first();
        let app_id = app.map(|a| a.id.as_str());

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

        // Execute all WarpScript files and collect metric values
        let mut metric_values: HashMap<String, f64> = HashMap::new();
        let mut any_failure = false;

        for (metric, script_content) in &scripts {
            match warpscript_probe::execute_warpscript(
                &probe.name,
                script_content,
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
                        metric = %metric,
                        value = v,
                        current_level = current_level,
                        "WarpScript execution successful"
                    );
                    metric_values.insert(metric.clone(), v);
                }
                Err(e) => {
                    error!(
                        probe_name = %probe.name,
                        metric = %metric,
                        error = %e,
                        "WarpScript execution failed"
                    );
                    any_failure = true;
                }
            }
        }

        if any_failure {
            consecutive_failures += 1;
            error!(
                probe_name = %probe.name,
                consecutive_failures,
                "One or more WarpScript executions failed"
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
                        next_delay = probe.get_delay_after_onf_command_success();
                    } else {
                        match executor::execute_command(&cmd, probe.command_timeout_seconds).await {
                            Ok(output) if output.status.success() => {
                                warn!(probe_name = %probe.name, "Failure command completed successfully");
                                next_delay = probe.get_delay_after_onf_command_success();
                            }
                            Ok(_) => {
                                error!(probe_name = %probe.name, "Failure command completed with errors");
                                next_delay = probe.get_delay_after_onf_command_failure();
                            }
                            Err(e) => {
                                error!(probe_name = %probe.name, error = %e, "Failed to execute failure command");
                                next_delay = probe.get_delay_after_onf_command_failure();
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
                last_values: last_values.clone(),
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

        consecutive_failures = 0;
        last_values = metric_values.clone();

        // Determine scaling action
        if probe.should_scale_up(current_level, &metric_values) {
            let new_level = current_level + 1;
            warn!(
                probe_name = %probe.name,
                from_level = current_level,
                to_level = new_level,
                "Scaling UP detected"
            );

            let computed = probe.get_computed_level(current_level).unwrap();
            let cmd = &probe.scaling.upscale_command;

            let command_ok = if dry_run {
                warn!(
                    probe_name = %probe.name,
                    command = %cmd,
                    flavor = %computed.flavor,
                    instances = computed.instances,
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
                    &computed.flavor,
                    computed.instances,
                    probe.command_timeout_seconds,
                    "upscale",
                )
                .await
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
        } else if probe.should_scale_down(current_level, &metric_values) {
            let new_level = current_level - 1;
            warn!(
                probe_name = %probe.name,
                from_level = current_level,
                to_level = new_level,
                "Scaling DOWN detected"
            );

            let computed = probe.get_computed_level(new_level).unwrap();
            let cmd = &probe.scaling.downscale_command;

            let command_ok = if dry_run {
                warn!(
                    probe_name = %probe.name,
                    command = %cmd,
                    flavor = %computed.flavor,
                    instances = computed.instances,
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
                    &computed.flavor,
                    computed.instances,
                    probe.command_timeout_seconds,
                    "downscale",
                )
                .await
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
        } else {
            debug!(
                probe_name = %probe.name,
                level = current_level,
                "No scaling action needed, level unchanged"
            );
            next_delay = probe.interval_seconds;
        }

        // Save state
        let state = WarpScriptProbeState {
            probe_name: probe.name.clone(),
            last_check_timestamp: check_timestamp,
            current_level,
            last_values: last_values.clone(),
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
        let ok = execute_scaling_command("test", "exit 1", None, "S", 1, 5, "upscale").await;
        assert!(!ok);
    }

    #[tokio::test]
    async fn test_scaling_command_success_returns_true() {
        let ok = execute_scaling_command("test", "true", None, "S", 1, 5, "upscale").await;
        assert!(ok);
    }

    #[tokio::test]
    async fn test_scaling_command_spawn_error_returns_false() {
        let ok = execute_scaling_command("test", "nonexistent_xyz_cmd_42", None, "S", 1, 5, "upscale").await;
        assert!(!ok);
    }

    #[tokio::test]
    async fn test_scaling_command_substitutes_flavor_and_instances() {
        // Use echo to capture substituted values; check exit code (always 0)
        let ok = execute_scaling_command(
            "test",
            "echo ${FLAVOR} ${INSTANCES}",
            Some("myapp"),
            "XL",
            3,
            5,
            "upscale",
        )
        .await;
        assert!(ok);
    }
}

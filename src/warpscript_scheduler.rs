use crate::config::WarpScriptProbe;
use crate::executor;
use crate::persistence::{self, PersistenceBackend, WarpScriptProbeState};
use crate::warpscript_probe;
use std::collections::HashMap;
use std::env;
use std::sync::Arc;
use std::time::Duration;
use tokio::task::JoinSet;
use tokio::time;
use tracing::{debug, error, info, warn};

pub(crate) struct ScalingCommandArgs<'a> {
    pub probe_name: &'a str,
    pub command: &'a str,
    pub app_id: Option<&'a str>,
    pub flavor: &'a str,
    pub instances: u32,
    pub timeout_seconds: u64,
    /// `"upscale"` or `"downscale"`
    pub action: &'a str,
    pub log_output: bool,
}

/// Execute a scaling command with variable substitution.
///
/// Substitutes `${APP_ID}`, `${FLAVOR}`, and `${INSTANCES}` in the command string.
/// Returns `true` if the command succeeded, `false` otherwise.
pub(crate) async fn execute_scaling_command(args: ScalingCommandArgs<'_>) -> bool {
    let ScalingCommandArgs {
        probe_name,
        command,
        app_id,
        flavor,
        instances,
        timeout_seconds,
        action,
        log_output,
    } = args;

    let mut cmd = command.to_string();
    if let Some(id) = app_id {
        cmd = cmd.replace("${APP_ID}", id);
    }
    cmd = cmd.replace("${FLAVOR}", flavor);
    cmd = cmd.replace("${INSTANCES}", &instances.to_string());

    warn!(probe_name = %probe_name, action = %action, "Executing {} command", action);
    debug!(command = %cmd, "Scaling command detail");

    match executor::execute_command(&cmd, timeout_seconds, log_output).await {
        Ok(output) if output.status.success() => {
            if log_output {
                warn!(
                    probe_name = %probe_name,
                    "{} command completed successfully", action
                );
            }
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

    let metrics_count = probe.warpscript_files.len();
    if probe.is_stateless() {
        info!(
            probe_name = %probe.name,
            interval_seconds = probe.interval_seconds,
            metrics_count,
            "Starting WarpScript probe scheduler (stateless mode)"
        );
    } else {
        info!(
            probe_name = %probe.name,
            interval_seconds = probe.interval_seconds,
            metrics_count,
            min_level = probe.min_level(),
            max_level = probe.max_level(),
            "Starting WarpScript probe scheduler"
        );
    }

    // Load previous state if exists
    let previous_state = match backend.load_warpscript_state(&probe.name).await {
        Ok(state) => state,
        Err(e) => {
            warn!(probe_name = %probe.name, error = %e,
                  "Failed to load initial state, starting fresh");
            None
        }
    };

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
            let current_level = if probe.is_stateless() {
                probe.min_level()
            } else if probe.get_computed_level(loaded_level).is_some() {
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

    let mut upscale_blocked_until: u64 = previous_state
        .as_ref()
        .map(|s| s.upscale_blocked_until)
        .unwrap_or(0);
    let mut downscale_blocked_until: u64 = previous_state
        .as_ref()
        .map(|s| s.downscale_blocked_until)
        .unwrap_or(0);
    let mut consecutive_scaling_failures: u32 = previous_state
        .as_ref()
        .map(|s| s.consecutive_scaling_failures)
        .unwrap_or(0);

    // Resolve environment variables once before the loop (stable for process lifetime)
    let endpoint = match env::var("WARP_ENDPOINT") {
        Ok(v) => v,
        Err(_) => {
            error!(probe_name = %probe.name, "WARP_ENDPOINT environment variable not set");
            return;
        }
    };
    let fallback_token = env::var("WARP_TOKEN").ok().filter(|t| !t.is_empty());

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
        // With parallel metric execution, all N metrics complete in ~1× request_timeout.
        // Adding command_timeout covers the optional on_failure / scaling command.
        let ttl_ms = (probe.get_request_timeout() + probe.command_timeout_seconds + 10) * 1000;

        let lock_token = match backend.acquire_lock(&lock_key, ttl_ms).await {
            Ok(None) => {
                debug!(probe_name = %probe.name, "Lock held by another instance, skipping cycle");
                next_delay = probe.interval_seconds;
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
        //
        // `Box<dyn Error>` (!Send) is converted to String immediately in the Err arm so
        // it is never held across any await point, keeping the future Send.
        let mut new_upscale_blocked = upscale_blocked_until;
        let mut new_downscale_blocked = downscale_blocked_until;
        let mut refresh_failed_multi = false;
        let mut skip_with_delay: Option<u64> = None;
        match backend.load_warpscript_state(&probe.name).await {
            Ok(Some(fresh_state)) => {
                if fresh_state.current_level != current_level {
                    info!(
                        probe_name = %probe.name,
                        stale = current_level,
                        fresh = fresh_state.current_level,
                        "State refreshed from Redis after lock acquisition"
                    );
                }
                let loaded_level = fresh_state.current_level;
                current_level = if probe.is_stateless() {
                    probe.min_level()
                } else if probe.get_computed_level(loaded_level).is_some() {
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
                consecutive_scaling_failures = fresh_state.consecutive_scaling_failures;
                last_values = fresh_state.last_values.clone();
                new_upscale_blocked = fresh_state.upscale_blocked_until;
                new_downscale_blocked = fresh_state.downscale_blocked_until;
                // Respect a future next_check_timestamp set by another instance
                // (e.g., the delay after on_failure_command).
                let now_ts = persistence::current_timestamp();
                if fresh_state.next_check_timestamp > now_ts {
                    skip_with_delay = Some(fresh_state.next_check_timestamp - now_ts);
                }
            }
            Ok(None) => {
                // First start — no state in Redis yet, keep in-memory values
            }
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
        upscale_blocked_until = new_upscale_blocked;
        downscale_blocked_until = new_downscale_blocked;

        if refresh_failed_multi {
            if let Some(ref t) = lock_token {
                let _ = backend.release_lock(&lock_key, t).await;
            }
            next_delay = probe.interval_seconds;
            continue;
        }

        if let Some(delay) = skip_with_delay {
            debug!(
                probe_name = %probe.name,
                remaining_seconds = delay,
                "Another instance scheduled a future check; releasing lock"
            );
            if let Some(ref t) = lock_token {
                let _ = backend.release_lock(&lock_key, t).await;
            }
            next_delay = delay;
            continue;
        }

        // Respect the per-direction cooldowns set by whichever instance last acted.
        // Only skip the cycle when BOTH directions are still blocked; if only one
        // direction is blocked the probe will proceed and apply per-direction checks
        // in the scaling branches below.
        let now = persistence::current_timestamp();
        if now < upscale_blocked_until && now < downscale_blocked_until {
            debug!(
                probe_name = %probe.name,
                upscale_remaining = upscale_blocked_until - now,
                downscale_remaining = downscale_blocked_until - now,
                "Both scaling directions still in cooldown, releasing lock and waiting"
            );
            if let Some(ref token) = lock_token {
                if let Err(e) = backend.release_lock(&lock_key, token).await {
                    debug!(probe_name = %probe.name, error = %e, "Failed to release lock (will expire via TTL)");
                }
            }
            next_delay = upscale_blocked_until.min(downscale_blocked_until).saturating_sub(now);
            continue;
        }

        info!(
            probe_name = %probe.name,
            current_level = current_level,
            "Executing WarpScript probe"
        );

        let check_timestamp = persistence::current_timestamp();

        let app = probe.apps.first();
        let app_id = app.map(|a| a.id.as_str());

        let token = match app.and_then(|a| a.warp_token.as_deref().filter(|t| !t.is_empty())).or(fallback_token.as_deref()) {
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

        // Substitute ${WARP_TOKEN} and ${APP_ID} once per cycle (stable for all metrics in this cycle).
        let substituted_scripts: HashMap<String, String> = scripts
            .iter()
            .map(|(k, v)| {
                let s = v.replace("${WARP_TOKEN}", token);
                let s = if let Some(id) = app_id { s.replace("${APP_ID}", id) } else { s };
                (k.clone(), s)
            })
            .collect();

        // Execute all WarpScript files in parallel and collect metric values.
        // Parallel execution ensures all N metrics complete in ~1× request_timeout
        // rather than N× request_timeout, keeping the lock TTL safe.
        let mut join_set: JoinSet<(String, Result<f64, warpscript_probe::WarpScriptError>)> =
            JoinSet::new();

        for (metric, script_content) in &substituted_scripts {
            let probe_name = probe.name.clone();
            let script = script_content.clone();
            let app_id_owned = app_id.map(|s| s.to_string());
            let token_owned = token.to_string();
            let endpoint_own = endpoint.clone();
            let timeout = probe.get_request_timeout();
            let client_clone = client.clone();
            let metric_own = metric.clone();

            join_set.spawn(async move {
                let result = warpscript_probe::execute_warpscript(
                    &probe_name,
                    &script,
                    app_id_owned.as_deref(),
                    &token_owned,
                    &endpoint_own,
                    timeout,
                    &client_clone,
                )
                .await;
                (metric_own, result)
            });
        }

        let mut metric_values: HashMap<String, f64> = HashMap::new();
        let mut any_failure = false;

        while let Some(join_result) = join_set.join_next().await {
            match join_result {
                Ok((metric, Ok(v))) => {
                    info!(probe_name = %probe.name, metric = %metric, value = v,
                          current_level, "WarpScript execution successful");
                    metric_values.insert(metric, v);
                }
                Ok((metric, Err(e))) => {
                    error!(probe_name = %probe.name, metric = %metric, error = %e,
                           "WarpScript execution failed");
                    any_failure = true;
                }
                Err(e) => {
                    error!(probe_name = %probe.name, error = %e, "Metric task panicked");
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
                            "DRY RUN: skipping failure command"
                        );
                        debug!(command = %cmd, "DRY RUN command detail");
                        next_delay = probe.get_delay_after_onf_command_success();
                    } else {
                        match executor::execute_command(&cmd, probe.command_timeout_seconds, !probe.suppress_command_output).await {
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
                upscale_blocked_until,
                downscale_blocked_until,
                consecutive_scaling_failures,
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
            let now = check_timestamp;
            if now < upscale_blocked_until {
                debug!(
                    probe_name = %probe.name,
                    remaining_seconds = upscale_blocked_until - now,
                    "Upscale cooldown active, skipping upscale"
                );
                next_delay = probe.interval_seconds;
            } else {
                let cmd = &probe.scaling.upscale_command;

                let command_ok = if probe.is_stateless() {
                    warn!(probe_name = %probe.name, "Scaling UP detected (stateless)");
                    if dry_run {
                        warn!(probe_name = %probe.name, "DRY RUN: skipping upscale command");
                        debug!(command = %cmd, "DRY RUN command detail");
                        true
                    } else {
                        execute_scaling_command(ScalingCommandArgs {
                            probe_name: &probe.name,
                            command: cmd,
                            app_id,
                            flavor: "",
                            instances: 0,
                            timeout_seconds: probe.command_timeout_seconds,
                            action: "upscale",
                            log_output: !probe.suppress_command_output,
                        })
                        .await
                    }
                } else {
                    let new_level = current_level + 1;
                    warn!(
                        probe_name = %probe.name,
                        from_level = current_level,
                        to_level = new_level,
                        "Scaling UP detected"
                    );
                    let computed = probe.get_computed_level(new_level).unwrap();
                    if dry_run {
                        warn!(
                            probe_name = %probe.name,
                            flavor = %computed.flavor,
                            instances = computed.instances,
                            from_level = current_level,
                            to_level = new_level,
                            "DRY RUN: skipping upscale command"
                        );
                        debug!(command = %cmd, "DRY RUN command detail");
                        true
                    } else {
                        execute_scaling_command(ScalingCommandArgs {
                            probe_name: &probe.name,
                            command: cmd,
                            app_id,
                            flavor: &computed.flavor,
                            instances: computed.instances,
                            timeout_seconds: probe.command_timeout_seconds,
                            action: "upscale",
                            log_output: !probe.suppress_command_output,
                        })
                        .await
                    }
                };

                if command_ok {
                    consecutive_scaling_failures = 0;
                    if !probe.is_stateless() {
                        current_level += 1;
                    }
                    let now = persistence::current_timestamp();
                    upscale_blocked_until   = now + probe.delay_after_upscale_then_upscale();
                    downscale_blocked_until = now + probe.delay_after_upscale_then_downscale();
                    next_delay = upscale_blocked_until.min(downscale_blocked_until).saturating_sub(now);
                } else {
                    consecutive_scaling_failures += 1;
                    warn!(
                        probe_name = %probe.name,
                        current_level,
                        consecutive_scaling_failures,
                        "Scaling command failed — level not updated"
                    );
                    next_delay = probe.interval_seconds;
                }
            }
        } else if probe.should_scale_down(current_level, &metric_values) {
            let now = check_timestamp;
            if now < downscale_blocked_until {
                debug!(
                    probe_name = %probe.name,
                    remaining_seconds = downscale_blocked_until - now,
                    "Downscale cooldown active, skipping downscale"
                );
                next_delay = probe.interval_seconds;
            } else {
                let cmd = &probe.scaling.downscale_command;

                let command_ok = if probe.is_stateless() {
                    warn!(probe_name = %probe.name, "Scaling DOWN detected (stateless)");
                    if dry_run {
                        warn!(probe_name = %probe.name, "DRY RUN: skipping downscale command");
                        debug!(command = %cmd, "DRY RUN command detail");
                        true
                    } else {
                        execute_scaling_command(ScalingCommandArgs {
                            probe_name: &probe.name,
                            command: cmd,
                            app_id,
                            flavor: "",
                            instances: 0,
                            timeout_seconds: probe.command_timeout_seconds,
                            action: "downscale",
                            log_output: !probe.suppress_command_output,
                        })
                        .await
                    }
                } else {
                    let new_level = current_level - 1;
                    warn!(
                        probe_name = %probe.name,
                        from_level = current_level,
                        to_level = new_level,
                        "Scaling DOWN detected"
                    );
                    let computed = probe.get_computed_level(new_level).unwrap();
                    if dry_run {
                        warn!(
                            probe_name = %probe.name,
                            flavor = %computed.flavor,
                            instances = computed.instances,
                            from_level = current_level,
                            to_level = new_level,
                            "DRY RUN: skipping downscale command"
                        );
                        debug!(command = %cmd, "DRY RUN command detail");
                        true
                    } else {
                        execute_scaling_command(ScalingCommandArgs {
                            probe_name: &probe.name,
                            command: cmd,
                            app_id,
                            flavor: &computed.flavor,
                            instances: computed.instances,
                            timeout_seconds: probe.command_timeout_seconds,
                            action: "downscale",
                            log_output: !probe.suppress_command_output,
                        })
                        .await
                    }
                };

                if command_ok {
                    consecutive_scaling_failures = 0;
                    if !probe.is_stateless() {
                        current_level -= 1;
                    }
                    let now = persistence::current_timestamp();
                    downscale_blocked_until = now + probe.delay_after_downscale_then_downscale();
                    upscale_blocked_until   = now + probe.delay_after_downscale_then_upscale();
                    next_delay = upscale_blocked_until.min(downscale_blocked_until).saturating_sub(now);
                } else {
                    consecutive_scaling_failures += 1;
                    warn!(
                        probe_name = %probe.name,
                        current_level,
                        consecutive_scaling_failures,
                        "Scaling command failed — level not updated"
                    );
                    next_delay = probe.interval_seconds;
                }
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
            upscale_blocked_until,
            downscale_blocked_until,
            consecutive_scaling_failures,
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
        let ok = execute_scaling_command(ScalingCommandArgs {
            probe_name: "test",
            command: "exit 1",
            app_id: None,
            flavor: "S",
            instances: 1,
            timeout_seconds: 5,
            action: "upscale",
            log_output: true,
        })
        .await;
        assert!(!ok);
    }

    #[tokio::test]
    async fn test_scaling_command_success_returns_true() {
        let ok = execute_scaling_command(ScalingCommandArgs {
            probe_name: "test",
            command: "true",
            app_id: None,
            flavor: "S",
            instances: 1,
            timeout_seconds: 5,
            action: "upscale",
            log_output: true,
        })
        .await;
        assert!(ok);
    }

    #[tokio::test]
    async fn test_scaling_command_spawn_error_returns_false() {
        let ok = execute_scaling_command(ScalingCommandArgs {
            probe_name: "test",
            command: "nonexistent_xyz_cmd_42",
            app_id: None,
            flavor: "S",
            instances: 1,
            timeout_seconds: 5,
            action: "upscale",
            log_output: true,
        })
        .await;
        assert!(!ok);
    }

    #[tokio::test]
    async fn test_scaling_command_substitutes_flavor_and_instances() {
        // Use echo to capture substituted values; check exit code (always 0)
        let ok = execute_scaling_command(ScalingCommandArgs {
            probe_name: "test",
            command: "echo ${FLAVOR} ${INSTANCES}",
            app_id: Some("myapp"),
            flavor: "XL",
            instances: 3,
            timeout_seconds: 5,
            action: "upscale",
            log_output: true,
        })
        .await;
        assert!(ok);
    }
}

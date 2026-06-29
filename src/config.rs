use serde::Deserialize;
use std::collections::HashMap;
use std::fs;
use std::path::Path;

#[derive(Debug, Deserialize, Clone)]
pub struct Config {
    #[serde(default)]
    pub healthcheck_probes: Vec<Probe>,
    #[serde(default)]
    pub warpscript_probes: Vec<WarpScriptProbe>,
}

#[derive(Debug, Deserialize, Clone)]
pub struct Probe {
    pub name: String,
    /// Direct URL (used when no apps are defined)
    pub url: Option<String>,
    pub interval_seconds: u64,
    pub checks: Checks,
    pub on_failure_command: Option<String>,
    #[serde(default = "default_timeout")]
    pub command_timeout_seconds: u64,
    /// Delay in seconds before next execution after a successful check (defaults to interval_seconds)
    pub delay_after_success_seconds: Option<u64>,
    /// Delay in seconds before next execution after a failed check (defaults to interval_seconds)
    pub delay_after_failure_seconds: Option<u64>,
    /// Delay in seconds before next execution after command succeeds (defaults to delay_after_failure_seconds)
    pub delay_after_command_success_seconds: Option<u64>,
    /// Delay in seconds before next execution after command fails (defaults to delay_after_failure_seconds)
    pub delay_after_command_failure_seconds: Option<u64>,
    /// Number of consecutive failures before executing the failure command (defaults to 0 = execute immediately)
    pub failure_retries_before_command: Option<u32>,
    /// HTTP request timeout in seconds for this probe (defaults to 30)
    pub request_timeout_seconds: Option<u64>,
    /// Suppress command output logging (exit code, stderr) on success and failure (defaults to false)
    #[serde(default)]
    pub suppress_command_output: bool,
    /// Applications to monitor (each app creates an independent probe instance)
    #[serde(default)]
    pub apps: Vec<HealthCheckApp>,
}

#[derive(Debug, Deserialize, Clone)]
pub struct HealthCheckApp {
    /// Application ID (substituted as ${APP_ID} in commands)
    pub id: String,
    /// Health check URL for this specific app
    pub url: String,
}

impl Probe {
    pub fn get_delay_after_success(&self) -> u64 {
        self.delay_after_success_seconds
            .unwrap_or(self.interval_seconds)
    }

    pub fn get_delay_after_failure(&self) -> u64 {
        self.delay_after_failure_seconds
            .unwrap_or(self.interval_seconds)
    }

    pub fn get_delay_after_command_success(&self) -> u64 {
        self.delay_after_command_success_seconds
            .unwrap_or_else(|| self.get_delay_after_failure())
    }

    pub fn get_delay_after_command_failure(&self) -> u64 {
        self.delay_after_command_failure_seconds
            .unwrap_or_else(|| self.get_delay_after_failure())
    }

    pub fn get_failure_retries_before_command(&self) -> u32 {
        self.failure_retries_before_command.unwrap_or(0)
    }

    pub fn get_request_timeout(&self) -> u64 {
        self.request_timeout_seconds.unwrap_or(30)
    }
}

fn default_timeout() -> u64 {
    30
}

#[derive(Debug, Deserialize, Clone)]
pub struct Checks {
    pub expected_status: Option<u16>,
    pub expected_body_contains: Option<String>,
    pub expected_body_regex: Option<String>,
    pub expected_header: Option<HashMap<String, String>>,
    /// Pre-compiled version of `expected_body_regex`. Populated by `Config::validate`.
    /// Skipped during (de)serialisation; `None` for probes built in tests.
    #[serde(skip)]
    pub compiled_body_regex: Option<regex::Regex>,
}

// WarpScript Probe Configuration
#[derive(Debug, Deserialize, Clone)]
pub struct WarpScriptProbe {
    pub name: String,
    /// Map of metric name → WarpScript file path (inline TOML table)
    #[serde(rename = "warpscript_file")]
    pub warpscript_files: HashMap<String, String>,
    pub interval_seconds: u64,
    #[serde(default = "default_timeout")]
    pub command_timeout_seconds: u64,
    /// HTTP request timeout in seconds for WarpScript calls (defaults to 30)
    pub request_timeout_seconds: Option<u64>,
    /// Shell command to execute after repeated probe failures
    pub on_failure_command: Option<String>,
    /// Consecutive failures required before executing on_failure_command (default 0 = first failure)
    pub failure_retries_before_command: Option<u32>,
    /// Delay in seconds before next execution after on_failure_command succeeds (defaults to interval_seconds)
    pub delay_after_command_success_seconds: Option<u64>,
    /// Delay in seconds before next execution after on_failure_command fails (defaults to interval_seconds)
    pub delay_after_command_failure_seconds: Option<u64>,
    /// Suppress command output logging (exit code, stderr) on success and failure (defaults to false)
    #[serde(default)]
    pub suppress_command_output: bool,
    /// Applications to manage (each with optional warp_token)
    #[serde(default)]
    pub apps: Vec<WarpScriptApp>,
    /// Scaling configuration (flavors, instances, thresholds, commands)
    pub scaling: ScalingConfig,
    /// Pre-computed level list. Populated by `Config::validate`; empty for probes built in tests.
    #[serde(skip)]
    pub computed_levels: Vec<ComputedLevel>,
}

#[derive(Debug, Deserialize, Clone)]
pub struct WarpScriptApp {
    /// Application ID
    pub id: String,
    /// Optional Warp token for this specific app (overrides WARP_TOKEN env var)
    pub warp_token: Option<String>,
}

#[derive(Debug, Deserialize, Clone)]
pub struct InstanceRange {
    pub min: u32,
    /// If absent, effective_max = min (no instance scaling, only flavor scaling)
    pub max: Option<u32>,
}

#[derive(Debug, Deserialize, Clone, Default)]
pub struct ScalingDelay {
    pub downscale: Option<u64>,
    pub upscale: Option<u64>,
}

impl InstanceRange {
    pub fn effective_max(&self) -> u32 {
        self.max.unwrap_or(self.min)
    }
}

#[derive(Debug, Deserialize, Clone)]
pub struct ScalingConfig {
    #[serde(default)]
    pub instances: Option<InstanceRange>,
    /// Ordered list of flavors (e.g. ["S", "M", "L"])
    #[serde(default)]
    pub flavors: Vec<String>,
    /// Thresholds to trigger upscale — scale up if ANY metric exceeds its threshold
    pub scale_up_threshold: HashMap<String, f64>,
    /// Thresholds to trigger downscale — scale down if ALL metrics are below their thresholds
    pub scale_down_threshold: HashMap<String, f64>,
    pub upscale_command: String,
    pub downscale_command: String,
    /// Delay after scaling up or down (fallback of last resort if matrix fields are absent)
    pub delay_after_scale_seconds: Option<u64>,
    /// Directional cooldown matrix applied after an upscale
    pub delay_after_upscale: Option<ScalingDelay>,
    /// Directional cooldown matrix applied after a downscale
    pub delay_after_downscale: Option<ScalingDelay>,
}

/// A computed scaling level (not deserialised, generated at runtime from ScalingConfig).
#[derive(Debug, Clone)]
pub struct ComputedLevel {
    pub level: u32,
    pub instances: u32,
    pub flavor: String,
}

impl WarpScriptProbe {
    pub fn get_request_timeout(&self) -> u64 {
        self.request_timeout_seconds.unwrap_or(30)
    }

    pub fn delay_after_upscale_then_upscale(&self) -> u64 {
        self.scaling.delay_after_upscale.as_ref()
            .and_then(|d| d.upscale)
            .or(self.scaling.delay_after_scale_seconds)
            .unwrap_or(self.interval_seconds)
    }

    pub fn delay_after_upscale_then_downscale(&self) -> u64 {
        self.scaling.delay_after_upscale.as_ref()
            .and_then(|d| d.downscale)
            .or(self.scaling.delay_after_scale_seconds)
            .unwrap_or(self.interval_seconds)
    }

    pub fn delay_after_downscale_then_downscale(&self) -> u64 {
        self.scaling.delay_after_downscale.as_ref()
            .and_then(|d| d.downscale)
            .or(self.scaling.delay_after_scale_seconds)
            .unwrap_or(self.interval_seconds)
    }

    pub fn delay_after_downscale_then_upscale(&self) -> u64 {
        self.scaling.delay_after_downscale.as_ref()
            .and_then(|d| d.upscale)
            .or(self.scaling.delay_after_scale_seconds)
            .unwrap_or(self.interval_seconds)
    }

    pub fn get_failure_retries_before_command(&self) -> u32 {
        self.failure_retries_before_command.unwrap_or(0)
    }

    pub fn get_delay_after_onf_command_success(&self) -> u64 {
        self.delay_after_command_success_seconds
            .unwrap_or(self.interval_seconds)
    }

    pub fn get_delay_after_onf_command_failure(&self) -> u64 {
        self.delay_after_command_failure_seconds
            .unwrap_or(self.interval_seconds)
    }

    /// Returns true when neither `flavors` nor `instances` are configured.
    /// In stateless mode the command fires every cycle the threshold is crossed,
    /// without any level tracking.
    pub fn is_stateless(&self) -> bool {
        self.scaling.flavors.is_empty() && self.scaling.instances.is_none()
    }

    /// Generate the ordered list of computed levels from flavors and instance range.
    ///
    /// Algorithm:
    /// - Phase 1: each flavor except the last gets one level at `instances.min`
    /// - Phase 2: the last flavor gets one level per instance count from `min` to `effective_max`
    ///
    /// Examples:
    ///   flavors=["S","M","L"], min=1, max=3 → (1,S,1),(2,M,1),(3,L,1),(4,L,2),(5,L,3)
    ///   flavors=["S"],         min=1, max=3 → (1,S,1),(2,S,2),(3,S,3)
    ///   flavors=["S","M"],     min=1, max=None → (1,S,1),(2,M,1)
    pub fn compute_levels(&self) -> Vec<ComputedLevel> {
        let sc = &self.scaling;
        let Some(ref inst) = sc.instances else { return vec![] };
        let min_inst = inst.min;
        let max_inst = inst.effective_max();
        let flavors = &sc.flavors;
        let mut levels = Vec::new();
        let mut level_num = 1u32;

        // Phase 1: all flavors except the last, at min instances
        for flavor in flavors.iter().take(flavors.len().saturating_sub(1)) {
            levels.push(ComputedLevel {
                level: level_num,
                instances: min_inst,
                flavor: flavor.clone(),
            });
            level_num += 1;
        }

        // Phase 2: last flavor, from min to max instances (inclusive)
        if let Some(last_flavor) = flavors.last() {
            for inst in min_inst..=max_inst {
                levels.push(ComputedLevel {
                    level: level_num,
                    instances: inst,
                    flavor: last_flavor.clone(),
                });
                level_num += 1;
            }
        }

        levels
    }

    /// Minimum level is always 1.
    pub fn min_level(&self) -> u32 {
        1
    }

    /// Maximum level derived from the pre-computed cache. Returns 0 in stateless mode.
    pub fn max_level(&self) -> u32 {
        self.computed_levels.last().map(|l| l.level).unwrap_or(0)
    }

    /// Look up a computed level by number using the pre-computed cache.
    pub fn get_computed_level(&self, n: u32) -> Option<&ComputedLevel> {
        self.computed_levels.iter().find(|l| l.level == n)
    }

    /// Scale up if ANY metric value exceeds its configured threshold.
    pub fn should_scale_up(&self, current_level: u32, values: &HashMap<String, f64>) -> bool {
        if !self.is_stateless() && current_level >= self.max_level() {
            return false;
        }
        self.scaling
            .scale_up_threshold
            .iter()
            .any(|(metric, &threshold)| {
                values.get(metric).is_some_and(|&v| v > threshold)
            })
    }

    /// Scale down if ALL metric values are below their configured thresholds.
    pub fn should_scale_down(&self, current_level: u32, values: &HashMap<String, f64>) -> bool {
        if !self.is_stateless() && current_level <= self.min_level() {
            return false;
        }
        let thresholds = &self.scaling.scale_down_threshold;
        if thresholds.is_empty() {
            return false;
        }
        thresholds.iter().all(|(metric, &threshold)| {
            values.get(metric).is_some_and(|&v| v < threshold)
        })
    }
}

/// Validates that an app ID contains only safe characters (alphanumeric, `-`, `_`, `.`).
/// This prevents shell injection when APP_ID is substituted into sh -c commands.
fn validate_app_id(probe_name: &str, id: &str) -> Result<(), Box<dyn std::error::Error>> {
    if id.is_empty() {
        return Err(format!("Probe '{}': app id cannot be empty", probe_name).into());
    }
    if !id
        .chars()
        .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_' || c == '.')
    {
        return Err(format!(
            "Probe '{}': app id '{}' contains invalid characters (only alphanumeric, '-', '_', '.' allowed)",
            probe_name, id
        ).into());
    }
    Ok(())
}

impl Config {
    pub fn from_file<P: AsRef<Path>>(path: P) -> Result<Self, Box<dyn std::error::Error>> {
        let contents = fs::read_to_string(path)?;
        let mut config: Config = toml::from_str(&contents)?;
        config.validate()?;
        Ok(config)
    }

    fn validate(&mut self) -> Result<(), Box<dyn std::error::Error>> {
        if self.healthcheck_probes.is_empty() && self.warpscript_probes.is_empty() {
            return Err(
                "Configuration must contain at least one probe \
                 (healthcheck_probes or warpscript_probes)"
                    .into(),
            );
        }

        // Validate uniqueness of effective probe names (post-app expansion)
        let mut seen_names: std::collections::HashSet<String> = std::collections::HashSet::new();

        for probe in &mut self.healthcheck_probes {
            if probe.name.is_empty() {
                return Err("Probe name cannot be empty".into());
            }
            // Either url or apps must be specified
            if probe.url.is_none() && probe.apps.is_empty() {
                return Err(format!(
                    "Probe '{}' must have either 'url' or 'apps' configured",
                    probe.name
                )
                .into());
            }
            if probe.url.is_some() && !probe.apps.is_empty() {
                return Err(format!(
                    "Probe '{}' cannot have both 'url' and 'apps' configured",
                    probe.name
                )
                .into());
            }
            if probe.interval_seconds == 0 {
                return Err(
                    format!("Probe '{}' has invalid interval (must be > 0)", probe.name).into(),
                );
            }

            // Validate that at least one check is configured
            if probe.checks.expected_status.is_none()
                && probe.checks.expected_body_contains.is_none()
                && probe.checks.expected_body_regex.is_none()
                && probe.checks.expected_header.is_none()
            {
                return Err(format!("Probe '{}' has no checks configured", probe.name).into());
            }

            if probe.on_failure_command.as_deref() == Some("") {
                return Err(format!(
                    "Probe '{}': on_failure_command cannot be empty; omit the field to disable it",
                    probe.name
                ).into());
            }

            // Validate and pre-compile regex patterns if present
            if let Some(ref pattern) = probe.checks.expected_body_regex {
                let compiled = regex::Regex::new(pattern)
                    .map_err(|e| format!("Probe '{}' has invalid regex: {}", probe.name, e))?;
                probe.checks.compiled_body_regex = Some(compiled);
            }

            // Validate effective names (after app expansion) are unique
            let effective_names: Vec<String> = if probe.apps.is_empty() {
                vec![probe.name.clone()]
            } else {
                probe
                    .apps
                    .iter()
                    .map(|a| {
                        validate_app_id(&probe.name, &a.id)?;
                        Ok(format!("{} - {}", probe.name, a.id))
                    })
                    .collect::<Result<Vec<_>, Box<dyn std::error::Error>>>()?
            };
            for name in effective_names {
                if !seen_names.insert(name.clone()) {
                    return Err(format!("Duplicate effective probe name: '{}'", name).into());
                }
            }
        }

        for probe in &mut self.warpscript_probes {
            if probe.name.is_empty() {
                return Err("WarpScript probe name cannot be empty".into());
            }
            if probe.interval_seconds == 0 {
                return Err(format!(
                    "WarpScript probe '{}' has invalid interval (must be > 0)",
                    probe.name
                )
                .into());
            }
            if probe.warpscript_files.is_empty() {
                return Err(format!(
                    "WarpScript probe '{}': warpscript_file must define at least one metric",
                    probe.name
                )
                .into());
            }

            let sc = &probe.scaling;
            let is_stateless = probe.is_stateless();
            let has_flavors = !sc.flavors.is_empty();
            let has_instances = sc.instances.is_some();

            if has_flavors != has_instances {
                return Err(format!(
                    "WarpScript probe '{}': 'flavors' and 'instances' must both be present or both absent",
                    probe.name
                )
                .into());
            }

            if !is_stateless {
                let inst = sc.instances.as_ref().unwrap();
                if inst.min < 1 {
                    return Err(format!(
                        "WarpScript probe '{}': instances.min must be >= 1",
                        probe.name
                    )
                    .into());
                }
                if let Some(max) = inst.max {
                    if max < inst.min {
                        return Err(format!(
                            "WarpScript probe '{}': instances.max ({}) must be >= instances.min ({})",
                            probe.name, max, inst.min
                        )
                        .into());
                    }
                }
            }

            // Threshold keys must be a subset of warpscript_files keys
            for key in sc.scale_up_threshold.keys() {
                if !probe.warpscript_files.contains_key(key) {
                    return Err(format!(
                        "WarpScript probe '{}': scale_up_threshold key '{}' not found in warpscript_file",
                        probe.name, key
                    )
                    .into());
                }
            }
            for key in sc.scale_down_threshold.keys() {
                if !probe.warpscript_files.contains_key(key) {
                    return Err(format!(
                        "WarpScript probe '{}': scale_down_threshold key '{}' not found in warpscript_file",
                        probe.name, key
                    )
                    .into());
                }
            }

            if sc.upscale_command.is_empty() {
                return Err(format!(
                    "WarpScript probe '{}': upscale_command cannot be empty",
                    probe.name
                )
                .into());
            }
            if sc.downscale_command.is_empty() {
                return Err(format!(
                    "WarpScript probe '{}': downscale_command cannot be empty",
                    probe.name
                )
                .into());
            }

            if is_stateless {
                for placeholder in &["${FLAVOR}", "${INSTANCES}"] {
                    if sc.upscale_command.contains(placeholder) {
                        return Err(format!(
                            "WarpScript probe '{}': stateless probe cannot use {} in upscale_command \
                             (no flavor/instances are configured)",
                            probe.name, placeholder
                        ).into());
                    }
                    if sc.downscale_command.contains(placeholder) {
                        return Err(format!(
                            "WarpScript probe '{}': stateless probe cannot use {} in downscale_command \
                             (no flavor/instances are configured)",
                            probe.name, placeholder
                        ).into());
                    }
                }
            }

            for app in &probe.apps {
                if app.warp_token.as_deref() == Some("") {
                    return Err(format!(
                        "WarpScript probe '{}': app '{}' has an empty warp_token; \
                         omit the field to fall back to the WARP_TOKEN environment variable",
                        probe.name, app.id
                    ).into());
                }
            }

            if probe.on_failure_command.as_deref() == Some("") {
                return Err(format!(
                    "WarpScript probe '{}': on_failure_command cannot be empty; omit the field to disable it",
                    probe.name
                ).into());
            }

            // Validate effective names (after app expansion) are unique
            let effective_names: Vec<String> = if probe.apps.is_empty() {
                vec![probe.name.clone()]
            } else {
                probe
                    .apps
                    .iter()
                    .map(|a| {
                        validate_app_id(&probe.name, &a.id)?;
                        Ok(format!("{} - {}", probe.name, a.id))
                    })
                    .collect::<Result<Vec<_>, Box<dyn std::error::Error>>>()?
            };
            for name in effective_names {
                if !seen_names.insert(name.clone()) {
                    return Err(format!("Duplicate effective probe name: '{}'", name).into());
                }
            }

            // Pre-compute and cache the level list (used by get_computed_level and max_level)
            let levels = probe.compute_levels();
            probe.computed_levels = levels;
        }

        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_valid_config() {
        let toml_content = r#"
            [[healthcheck_probes]]
            name = "Test Probe"
            url = "https://example.com"
            interval_seconds = 60

            [healthcheck_probes.checks]
            expected_status = 200
        "#;

        let mut config: Config = toml::from_str(toml_content).unwrap();
        assert!(config.validate().is_ok());
    }

    #[test]
    fn test_empty_probes() {
        // Both arrays empty → must be rejected
        let toml_content = r#"
            healthcheck_probes = []
            warpscript_probes = []
        "#;

        let mut config: Config = toml::from_str(toml_content).unwrap();
        assert!(config.validate().is_err());
    }

    #[test]
    fn test_warpscript_only() {
        // Config with only warpscript_probes (no healthcheck_probes) must be accepted
        let toml_content = r#"
            [[warpscript_probes]]
            name = "ws-only"
            warpscript_file = {cpu = "test.mc2"}
            interval_seconds = 60

            [warpscript_probes.scaling]
            instances = {min = 1, max = 2}
            flavors = ["S", "M"]
            scale_up_threshold = {cpu = 70.0}
            scale_down_threshold = {cpu = 40.0}
            upscale_command = "scale up"
            downscale_command = "scale down"
        "#;

        let mut config: Config = toml::from_str(toml_content).unwrap();
        assert!(config.validate().is_ok());
    }

    fn warpscript_probe_toml(scaling_block: &str) -> String {
        format!(
            r#"
            [[healthcheck_probes]]
            name = "hp"
            url = "https://example.com"
            interval_seconds = 60
            [healthcheck_probes.checks]
            expected_status = 200

            [[warpscript_probes]]
            name = "ws"
            warpscript_file = {{cpu = "test.mc2"}}
            interval_seconds = 60
            {}
            "#,
            scaling_block
        )
    }

    #[test]
    fn test_compute_levels_multi_flavor() {
        // flavors=["S","M","L"], min=1, max=3 → 5 levels
        let toml = warpscript_probe_toml(
            r#"
            [warpscript_probes.scaling]
            instances = {min = 1, max = 3}
            flavors = ["S", "M", "L"]
            scale_up_threshold = {cpu = 70.0}
            scale_down_threshold = {cpu = 40.0}
            upscale_command = "scale up"
            downscale_command = "scale down"
            "#,
        );
        let mut config: Config = toml::from_str(&toml).unwrap();
        assert!(config.validate().is_ok());

        let probe = &config.warpscript_probes[0];
        let levels = probe.compute_levels();
        assert_eq!(levels.len(), 5);
        assert_eq!(levels[0].level, 1); assert_eq!(levels[0].instances, 1); assert_eq!(levels[0].flavor, "S");
        assert_eq!(levels[1].level, 2); assert_eq!(levels[1].instances, 1); assert_eq!(levels[1].flavor, "M");
        assert_eq!(levels[2].level, 3); assert_eq!(levels[2].instances, 1); assert_eq!(levels[2].flavor, "L");
        assert_eq!(levels[3].level, 4); assert_eq!(levels[3].instances, 2); assert_eq!(levels[3].flavor, "L");
        assert_eq!(levels[4].level, 5); assert_eq!(levels[4].instances, 3); assert_eq!(levels[4].flavor, "L");
        assert_eq!(probe.min_level(), 1);
        assert_eq!(probe.max_level(), 5);
    }

    #[test]
    fn test_compute_levels_mono_flavor() {
        // flavors=["S"], min=1, max=3 → 3 levels
        let toml = warpscript_probe_toml(
            r#"
            [warpscript_probes.scaling]
            instances = {min = 1, max = 3}
            flavors = ["S"]
            scale_up_threshold = {cpu = 70.0}
            scale_down_threshold = {cpu = 40.0}
            upscale_command = "scale up"
            downscale_command = "scale down"
            "#,
        );
        let mut config: Config = toml::from_str(&toml).unwrap();
        assert!(config.validate().is_ok());

        let probe = &config.warpscript_probes[0];
        let levels = probe.compute_levels();
        assert_eq!(levels.len(), 3);
        assert_eq!(levels[0].level, 1); assert_eq!(levels[0].instances, 1); assert_eq!(levels[0].flavor, "S");
        assert_eq!(levels[1].level, 2); assert_eq!(levels[1].instances, 2); assert_eq!(levels[1].flavor, "S");
        assert_eq!(levels[2].level, 3); assert_eq!(levels[2].instances, 3); assert_eq!(levels[2].flavor, "S");
        assert_eq!(probe.max_level(), 3);
    }

    #[test]
    fn test_compute_levels_no_max() {
        // flavors=["S","M"], min=1, max absent → 2 levels (no instance scaling)
        let toml = warpscript_probe_toml(
            r#"
            [warpscript_probes.scaling]
            instances = {min = 1}
            flavors = ["S", "M"]
            scale_up_threshold = {cpu = 70.0}
            scale_down_threshold = {cpu = 40.0}
            upscale_command = "scale up"
            downscale_command = "scale down"
            "#,
        );
        let mut config: Config = toml::from_str(&toml).unwrap();
        assert!(config.validate().is_ok());

        let probe = &config.warpscript_probes[0];
        let levels = probe.compute_levels();
        assert_eq!(levels.len(), 2);
        assert_eq!(levels[0].flavor, "S"); assert_eq!(levels[0].instances, 1);
        assert_eq!(levels[1].flavor, "M"); assert_eq!(levels[1].instances, 1);
        assert_eq!(probe.max_level(), 2);
    }

    #[test]
    fn test_should_scale_up_any() {
        let toml_content = r#"
            [[warpscript_probes]]
            name = "ws"
            warpscript_file = {cpu = "test.mc2", memory = "mem.mc2"}
            interval_seconds = 60

            [warpscript_probes.scaling]
            instances = {min = 1, max = 3}
            flavors = ["S", "M", "L"]
            scale_up_threshold = {cpu = 70.0, memory = 80.0}
            scale_down_threshold = {cpu = 40.0, memory = 40.0}
            upscale_command = "scale up"
            downscale_command = "scale down"
        "#;
        let mut config: Config = toml::from_str(toml_content).unwrap();
        config.validate().unwrap();
        let probe = &config.warpscript_probes[0];

        // Only cpu exceeds threshold → should scale up (ANY)
        let mut values = HashMap::new();
        values.insert("cpu".to_string(), 75.0);
        values.insert("memory".to_string(), 50.0);
        assert!(probe.should_scale_up(1, &values));

        // Neither exceeds threshold → no scale up
        values.insert("cpu".to_string(), 60.0);
        values.insert("memory".to_string(), 60.0);
        assert!(!probe.should_scale_up(1, &values));

        // At max level → no scale up even if above threshold
        values.insert("cpu".to_string(), 90.0);
        assert!(!probe.should_scale_up(probe.max_level(), &values));
    }

    #[test]
    fn test_should_scale_down_all() {
        let toml_content = r#"
            [[warpscript_probes]]
            name = "ws"
            warpscript_file = {cpu = "test.mc2", memory = "mem.mc2"}
            interval_seconds = 60

            [warpscript_probes.scaling]
            instances = {min = 1, max = 3}
            flavors = ["S", "M", "L"]
            scale_up_threshold = {cpu = 70.0, memory = 80.0}
            scale_down_threshold = {cpu = 40.0, memory = 40.0}
            upscale_command = "scale up"
            downscale_command = "scale down"
        "#;
        let mut config: Config = toml::from_str(toml_content).unwrap();
        config.validate().unwrap();
        let probe = &config.warpscript_probes[0];

        // Both below threshold → should scale down (ALL)
        let mut values = HashMap::new();
        values.insert("cpu".to_string(), 30.0);
        values.insert("memory".to_string(), 35.0);
        assert!(probe.should_scale_down(2, &values));

        // Only one below threshold → no scale down
        values.insert("cpu".to_string(), 30.0);
        values.insert("memory".to_string(), 50.0);
        assert!(!probe.should_scale_down(2, &values));

        // At min level → no scale down even if all below threshold
        values.insert("cpu".to_string(), 20.0);
        values.insert("memory".to_string(), 20.0);
        assert!(!probe.should_scale_down(probe.min_level(), &values));
    }

    #[test]
    fn test_stateless_valid_config() {
        // No flavors and no instances → stateless mode is valid
        let toml = warpscript_probe_toml(
            r#"
            [warpscript_probes.scaling]
            scale_up_threshold = {cpu = 70.0}
            scale_down_threshold = {cpu = 40.0}
            upscale_command = "alert up"
            downscale_command = "alert down"
            "#,
        );
        let mut config: Config = toml::from_str(&toml).unwrap();
        assert!(config.validate().is_ok());
        assert!(config.warpscript_probes[0].is_stateless());
    }

    #[test]
    fn test_stateless_flavors_without_instances_error() {
        // flavors present but instances absent → error
        let toml = warpscript_probe_toml(
            r#"
            [warpscript_probes.scaling]
            flavors = ["S", "M"]
            scale_up_threshold = {cpu = 70.0}
            scale_down_threshold = {cpu = 40.0}
            upscale_command = "scale up"
            downscale_command = "scale down"
            "#,
        );
        let mut config: Config = toml::from_str(&toml).unwrap();
        assert!(config.validate().is_err());
    }

    #[test]
    fn test_stateless_instances_without_flavors_error() {
        // instances present but flavors absent → error
        let toml = warpscript_probe_toml(
            r#"
            [warpscript_probes.scaling]
            instances = {min = 1, max = 3}
            scale_up_threshold = {cpu = 70.0}
            scale_down_threshold = {cpu = 40.0}
            upscale_command = "scale up"
            downscale_command = "scale down"
            "#,
        );
        let mut config: Config = toml::from_str(&toml).unwrap();
        assert!(config.validate().is_err());
    }

    #[test]
    fn test_stateless_should_scale_up_every_cycle() {
        // In stateless mode, should_scale_up fires unconditionally when threshold crossed
        let toml = warpscript_probe_toml(
            r#"
            [warpscript_probes.scaling]
            scale_up_threshold = {cpu = 70.0}
            scale_down_threshold = {cpu = 40.0}
            upscale_command = "alert up"
            downscale_command = "alert down"
            "#,
        );
        let mut config: Config = toml::from_str(&toml).unwrap();
        config.validate().unwrap();
        let probe = &config.warpscript_probes[0];
        assert!(probe.is_stateless());

        let mut values = HashMap::new();
        values.insert("cpu".to_string(), 80.0);
        // Fires at any "level" since there are no level bounds
        assert!(probe.should_scale_up(1, &values));
        assert!(probe.should_scale_up(100, &values));
        assert!(probe.should_scale_up(0, &values));

        // Below threshold → no trigger
        values.insert("cpu".to_string(), 60.0);
        assert!(!probe.should_scale_up(1, &values));
    }

    #[test]
    fn test_stateless_should_scale_down_every_cycle() {
        let toml = warpscript_probe_toml(
            r#"
            [warpscript_probes.scaling]
            scale_up_threshold = {cpu = 70.0}
            scale_down_threshold = {cpu = 40.0}
            upscale_command = "alert up"
            downscale_command = "alert down"
            "#,
        );
        let mut config: Config = toml::from_str(&toml).unwrap();
        config.validate().unwrap();
        let probe = &config.warpscript_probes[0];

        let mut values = HashMap::new();
        values.insert("cpu".to_string(), 30.0);
        // Fires at any "level"
        assert!(probe.should_scale_down(1, &values));
        assert!(probe.should_scale_down(0, &values));

        // Above threshold → no trigger
        values.insert("cpu".to_string(), 50.0);
        assert!(!probe.should_scale_down(1, &values));
    }

    #[test]
    fn test_stateless_compute_levels_empty() {
        let toml = warpscript_probe_toml(
            r#"
            [warpscript_probes.scaling]
            scale_up_threshold = {cpu = 70.0}
            scale_down_threshold = {cpu = 40.0}
            upscale_command = "alert up"
            downscale_command = "alert down"
            "#,
        );
        let mut config: Config = toml::from_str(&toml).unwrap();
        config.validate().unwrap();
        let probe = &config.warpscript_probes[0];
        assert!(probe.compute_levels().is_empty());
        assert_eq!(probe.max_level(), 0);
    }

    #[test]
    fn test_no_checks() {
        let toml_content = r#"
            [[healthcheck_probes]]
            name = "Test"
            url = "https://example.com"
            interval_seconds = 60

            [healthcheck_probes.checks]
        "#;

        let mut config: Config = toml::from_str(toml_content).unwrap();
        assert!(config.validate().is_err());
    }

    #[test]
    fn test_validation_threshold_key_not_in_files() {
        // scale_up_threshold references "ram" but warpscript_file only has "cpu"
        let toml = warpscript_probe_toml(
            r#"
            [warpscript_probes.scaling]
            instances = {min = 1, max = 2}
            flavors = ["S"]
            scale_up_threshold = {ram = 70.0}
            scale_down_threshold = {cpu = 40.0}
            upscale_command = "scale up"
            downscale_command = "scale down"
            "#,
        );
        let mut config: Config = toml::from_str(&toml).unwrap();
        assert!(config.validate().is_err());
    }

    #[test]
    fn test_validation_instances_max_less_than_min() {
        let toml = warpscript_probe_toml(
            r#"
            [warpscript_probes.scaling]
            instances = {min = 3, max = 1}
            flavors = ["S"]
            scale_up_threshold = {cpu = 70.0}
            scale_down_threshold = {cpu = 40.0}
            upscale_command = "scale up"
            downscale_command = "scale down"
            "#,
        );
        let mut config: Config = toml::from_str(&toml).unwrap();
        assert!(config.validate().is_err());
    }

    #[test]
    fn test_multi_metric_warpscript_file() {
        let toml_content = r#"
            [[warpscript_probes]]
            name = "Multi-Metric"
            warpscript_file = {memory = "warpscript/ram.mc2", cpu = "warpscript/cpu.mc2"}
            interval_seconds = 60

            [warpscript_probes.scaling]
            instances = {min = 1, max = 3}
            flavors = ["S", "M", "L"]
            scale_up_threshold = {memory = 60.0, cpu = 60.0}
            scale_down_threshold = {memory = 40.0, cpu = 40.0}
            upscale_command = "clever scale --app ${APP_ID} --flavor ${FLAVOR} --instances ${INSTANCES}"
            downscale_command = "clever scale --app ${APP_ID} --flavor ${FLAVOR} --instances ${INSTANCES}"
        "#;
        let mut config: Config = toml::from_str(toml_content).unwrap();
        assert!(config.validate().is_ok());
        assert_eq!(config.warpscript_probes[0].warpscript_files.len(), 2);
    }

    fn probe_with_delay_toml(extra_scaling: &str) -> WarpScriptProbe {
        let toml = warpscript_probe_toml(&format!(
            r#"
            [warpscript_probes.scaling]
            scale_up_threshold = {{cpu = 70.0}}
            scale_down_threshold = {{cpu = 40.0}}
            upscale_command = "up"
            downscale_command = "down"
            {}
            "#,
            extra_scaling
        ));
        let mut config: Config = toml::from_str(&toml).unwrap();
        config.validate().unwrap();
        config.warpscript_probes.remove(0)
    }

    #[test]
    fn test_delay_none_falls_back_to_interval() {
        // No delay fields set → all four fall back to interval_seconds (60)
        let probe = probe_with_delay_toml("");
        assert_eq!(probe.delay_after_upscale_then_upscale(), 60);
        assert_eq!(probe.delay_after_upscale_then_downscale(), 60);
        assert_eq!(probe.delay_after_downscale_then_downscale(), 60);
        assert_eq!(probe.delay_after_downscale_then_upscale(), 60);
    }

    #[test]
    fn test_delay_after_scale_fallback() {
        // Only delay_after_scale_seconds → all four use it
        let probe = probe_with_delay_toml("delay_after_scale_seconds = 120");
        assert_eq!(probe.delay_after_upscale_then_upscale(), 120);
        assert_eq!(probe.delay_after_upscale_then_downscale(), 120);
        assert_eq!(probe.delay_after_downscale_then_downscale(), 120);
        assert_eq!(probe.delay_after_downscale_then_upscale(), 120);
    }

    #[test]
    fn test_delay_matrix_full() {
        // Full matrix: after upscale block upscale=120 downscale=30; after downscale block downscale=120 upscale=30
        let probe = probe_with_delay_toml(
            "delay_after_upscale = {upscale = 120, downscale = 30}\ndelay_after_downscale = {downscale = 120, upscale = 30}"
        );
        assert_eq!(probe.delay_after_upscale_then_upscale(), 120);
        assert_eq!(probe.delay_after_upscale_then_downscale(), 30);
        assert_eq!(probe.delay_after_downscale_then_downscale(), 120);
        assert_eq!(probe.delay_after_downscale_then_upscale(), 30);
    }

    #[test]
    fn test_delay_matrix_partial_fallback() {
        // Partial matrix: only downscale field set in delay_after_upscale → upscale side falls back
        let probe = probe_with_delay_toml(
            "delay_after_upscale = {downscale = 120}\ndelay_after_scale_seconds = 45"
        );
        // upscale field absent → falls back to delay_after_scale_seconds
        assert_eq!(probe.delay_after_upscale_then_upscale(), 45);
        // downscale field present → uses it
        assert_eq!(probe.delay_after_upscale_then_downscale(), 120);
        // delay_after_downscale not set → falls back to delay_after_scale_seconds
        assert_eq!(probe.delay_after_downscale_then_downscale(), 45);
        assert_eq!(probe.delay_after_downscale_then_upscale(), 45);
    }

    #[test]
    fn test_stateless_upscale_command_with_instances_placeholder_error() {
        let toml = warpscript_probe_toml(
            r#"
            [warpscript_probes.scaling]
            scale_up_threshold = {cpu = 70.0}
            scale_down_threshold = {cpu = 40.0}
            upscale_command = "scale --instances ${INSTANCES}"
            downscale_command = "alert down"
            "#,
        );
        let mut config: Config = toml::from_str(&toml).unwrap();
        assert!(config.validate().is_err());
    }

    #[test]
    fn test_stateless_downscale_command_with_flavor_placeholder_error() {
        let toml = warpscript_probe_toml(
            r#"
            [warpscript_probes.scaling]
            scale_up_threshold = {cpu = 70.0}
            scale_down_threshold = {cpu = 40.0}
            upscale_command = "alert up"
            downscale_command = "scale --flavor ${FLAVOR}"
            "#,
        );
        let mut config: Config = toml::from_str(&toml).unwrap();
        assert!(config.validate().is_err());
    }

    #[test]
    fn test_empty_warp_token_error() {
        let toml_content = r#"
            [[warpscript_probes]]
            name = "ws"
            warpscript_file = {cpu = "test.mc2"}
            interval_seconds = 60

            [[warpscript_probes.apps]]
            id = "myapp"
            warp_token = ""

            [warpscript_probes.scaling]
            scale_up_threshold = {cpu = 70.0}
            scale_down_threshold = {cpu = 40.0}
            upscale_command = "alert up"
            downscale_command = "alert down"
        "#;
        let mut config: Config = toml::from_str(toml_content).unwrap();
        assert!(config.validate().is_err());
    }

    #[test]
    fn test_empty_on_failure_command_healthcheck_error() {
        let toml_content = r#"
            [[healthcheck_probes]]
            name = "hp"
            url = "https://example.com"
            interval_seconds = 60
            on_failure_command = ""

            [healthcheck_probes.checks]
            expected_status = 200
        "#;
        let mut config: Config = toml::from_str(toml_content).unwrap();
        assert!(config.validate().is_err());
    }

    #[test]
    fn test_empty_on_failure_command_warpscript_error() {
        let toml = warpscript_probe_toml(
            r#"
            on_failure_command = ""

            [warpscript_probes.scaling]
            scale_up_threshold = {cpu = 70.0}
            scale_down_threshold = {cpu = 40.0}
            upscale_command = "alert up"
            downscale_command = "alert down"
            "#,
        );
        let mut config: Config = toml::from_str(&toml).unwrap();
        assert!(config.validate().is_err());
    }

    #[test]
    fn test_delay_matrix_overrides_scale_fallback() {
        // Matrix fields win over delay_after_scale_seconds
        let probe = probe_with_delay_toml(
            "delay_after_scale_seconds = 999\ndelay_after_upscale = {upscale = 300, downscale = 30}"
        );
        assert_eq!(probe.delay_after_upscale_then_upscale(), 300);
        assert_eq!(probe.delay_after_upscale_then_downscale(), 30);
        // delay_after_downscale not set → falls back to delay_after_scale_seconds
        assert_eq!(probe.delay_after_downscale_then_downscale(), 999);
        assert_eq!(probe.delay_after_downscale_then_upscale(), 999);
    }
}

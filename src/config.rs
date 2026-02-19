use serde::Deserialize;
use std::collections::HashMap;
use std::fs;
use std::path::Path;

#[derive(Debug, Deserialize, Clone)]
pub struct Config {
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
        self.delay_after_success_seconds.unwrap_or(self.interval_seconds)
    }

    pub fn get_delay_after_failure(&self) -> u64 {
        self.delay_after_failure_seconds.unwrap_or(self.interval_seconds)
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
}

// WarpScript Probe Configuration
#[derive(Debug, Deserialize, Clone)]
pub struct WarpScriptProbe {
    pub name: String,
    pub warpscript_file: String,
    pub interval_seconds: u64,
    #[serde(default = "default_timeout")]
    pub command_timeout_seconds: u64,
    /// Delay after scaling up or down
    pub delay_after_scale_seconds: Option<u64>,
    /// Applications to manage (each with optional warp_token)
    #[serde(default)]
    pub apps: Vec<WarpScriptApp>,
    /// Scaling levels (must be ordered by level number)
    pub levels: Vec<ScalingLevel>,
}

#[derive(Debug, Deserialize, Clone)]
pub struct WarpScriptApp {
    /// Application ID
    pub id: String,
    /// Optional Warp token for this specific app (overrides WARP_TOKEN env var)
    pub warp_token: Option<String>,
}

#[derive(Debug, Deserialize, Clone)]
pub struct ScalingLevel {
    /// Level number (1, 2, 3, etc.)
    pub level: u32,
    /// Threshold to trigger upscale (move to level+1)
    pub scale_up_threshold: Option<f64>,
    /// Threshold to trigger downscale (move to level-1)
    pub scale_down_threshold: Option<f64>,
    /// Command to execute when scaling up FROM this level
    pub upscale_command: Option<String>,
    /// Command to execute when scaling down FROM this level
    pub downscale_command: Option<String>,
}

impl WarpScriptProbe {
    pub fn get_delay_after_scale(&self) -> u64 {
        self.delay_after_scale_seconds.unwrap_or(self.interval_seconds)
    }

    /// Get level configuration by level number
    pub fn get_level(&self, level_num: u32) -> Option<&ScalingLevel> {
        self.levels.iter().find(|l| l.level == level_num)
    }

    /// Get minimum level number
    pub fn min_level(&self) -> u32 {
        self.levels.iter().map(|l| l.level).min().unwrap_or(1)
    }

    /// Get maximum level number
    pub fn max_level(&self) -> u32 {
        self.levels.iter().map(|l| l.level).max().unwrap_or(1)
    }

    /// Determine if we should scale up based on current level and value
    pub fn should_scale_up(&self, current_level: u32, value: f64) -> bool {
        if current_level >= self.max_level() {
            return false; // Already at max, can't scale up
        }

        if let Some(level_config) = self.get_level(current_level) {
            if let Some(threshold) = level_config.scale_up_threshold {
                return value > threshold;
            }
        }
        false
    }

    /// Determine if we should scale down based on current level and value
    pub fn should_scale_down(&self, current_level: u32, value: f64) -> bool {
        if current_level <= self.min_level() {
            return false; // Already at min, can't scale down
        }

        if let Some(level_config) = self.get_level(current_level) {
            if let Some(threshold) = level_config.scale_down_threshold {
                return value < threshold;
            }
        }
        false
    }
}

impl Config {
    pub fn from_file<P: AsRef<Path>>(path: P) -> Result<Self, Box<dyn std::error::Error>> {
        let contents = fs::read_to_string(path)?;
        let config: Config = toml::from_str(&contents)?;
        config.validate()?;
        Ok(config)
    }

    fn validate(&self) -> Result<(), Box<dyn std::error::Error>> {
        if self.healthcheck_probes.is_empty() {
            return Err("Configuration must contain at least one probe".into());
        }

        for probe in &self.healthcheck_probes {
            if probe.name.is_empty() {
                return Err("Probe name cannot be empty".into());
            }
            // Either url or apps must be specified
            if probe.url.is_none() && probe.apps.is_empty() {
                return Err(format!("Probe '{}' must have either 'url' or 'apps' configured", probe.name).into());
            }
            if probe.url.is_some() && !probe.apps.is_empty() {
                return Err(format!("Probe '{}' cannot have both 'url' and 'apps' configured", probe.name).into());
            }
            if probe.interval_seconds == 0 {
                return Err(format!("Probe '{}' has invalid interval (must be > 0)", probe.name).into());
            }

            // Validate that at least one check is configured
            if probe.checks.expected_status.is_none()
                && probe.checks.expected_body_contains.is_none()
                && probe.checks.expected_body_regex.is_none()
                && probe.checks.expected_header.is_none()
            {
                return Err(format!("Probe '{}' has no checks configured", probe.name).into());
            }

            // Validate regex patterns if present
            if let Some(ref pattern) = probe.checks.expected_body_regex {
                regex::Regex::new(pattern)
                    .map_err(|e| format!("Probe '{}' has invalid regex: {}", probe.name, e))?;
            }
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

        let config: Config = toml::from_str(toml_content).unwrap();
        assert!(config.validate().is_ok());
    }

    #[test]
    fn test_empty_probes() {
        let toml_content = r#"
            healthcheck_probes = []
        "#;

        let config: Config = toml::from_str(toml_content).unwrap();
        assert!(config.validate().is_err());
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

        let config: Config = toml::from_str(toml_content).unwrap();
        assert!(config.validate().is_err());
    }
}

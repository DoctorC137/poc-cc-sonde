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
    #[serde(deserialize_with = "deserialize_levels")]
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

#[derive(Debug, Deserialize)]
struct ScalingLevelRaw {
    /// Singular form: level = N (backwards-compatible)
    level: Option<u32>,
    /// Plural form: levels = [N, M, ...] (new)
    #[serde(default)]
    levels: Vec<u32>,
    scale_up_threshold: Option<f64>,
    scale_down_threshold: Option<f64>,
    upscale_command: Option<String>,
    downscale_command: Option<String>,
}

fn deserialize_levels<'de, D>(deserializer: D) -> Result<Vec<ScalingLevel>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    use serde::de::Error;

    let raw_entries: Vec<ScalingLevelRaw> = Vec::deserialize(deserializer)?;
    let mut result: Vec<ScalingLevel> = Vec::new();

    for raw in raw_entries {
        let level_nums: Vec<u32> = match (raw.level, raw.levels.is_empty()) {
            (Some(n), true) => vec![n],
            (None, false) => raw.levels,
            (Some(_), false) => {
                return Err(D::Error::custom(
                    "a scaling level entry cannot specify both `level` and `levels`",
                ));
            }
            (None, true) => {
                return Err(D::Error::custom(
                    "a scaling level entry must specify either `level = N` or `levels = [N, ...]`",
                ));
            }
        };

        for n in level_nums {
            result.push(ScalingLevel {
                level: n,
                scale_up_threshold: raw.scale_up_threshold,
                scale_down_threshold: raw.scale_down_threshold,
                upscale_command: raw.upscale_command.clone(),
                downscale_command: raw.downscale_command.clone(),
            });
        }
    }

    result.sort_by_key(|l| l.level);
    Ok(result)
}

impl WarpScriptProbe {
    pub fn get_delay_after_scale(&self) -> u64 {
        self.delay_after_scale_seconds
            .unwrap_or(self.interval_seconds)
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

        for probe in &self.warpscript_probes {
            if probe.name.is_empty() {
                return Err("WarpScript probe name cannot be empty".into());
            }
            if probe.levels.is_empty() {
                return Err(format!(
                    "WarpScript probe '{}' must define at least one level",
                    probe.name
                )
                .into());
            }
            let mut seen = std::collections::HashSet::new();
            for level in &probe.levels {
                if !seen.insert(level.level) {
                    return Err(format!(
                        "WarpScript probe '{}' has duplicate level number {}",
                        probe.name, level.level
                    )
                    .into());
                }
            }

            // Levels are sorted after deserialisation; check for gaps
            let min = probe.min_level();
            let max = probe.max_level();
            let expected_count = (max - min + 1) as usize;
            if probe.levels.len() != expected_count {
                return Err(format!(
                    "WarpScript probe '{}' levels must be contiguous (found gaps between {} and {})",
                    probe.name, min, max
                )
                .into());
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
            warpscript_file = "test.mc2"
            interval_seconds = 60

            [[warpscript_probes.levels]]
            level = 1
            scale_up_threshold = 70.0
            upscale_command = "scale up"

            [[warpscript_probes.levels]]
            level = 2
            scale_down_threshold = 50.0
            downscale_command = "scale down"
        "#;

        let mut config: Config = toml::from_str(toml_content).unwrap();
        assert!(config.validate().is_ok());
    }

    fn warpscript_probe_toml(levels_block: &str) -> String {
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
            warpscript_file = "test.mc2"
            interval_seconds = 60
            {}
            "#,
            levels_block
        )
    }

    #[test]
    fn test_warpscript_level_singular() {
        let toml = warpscript_probe_toml(
            r#"
            [[warpscript_probes.levels]]
            level = 1
            scale_up_threshold = 70.0
            upscale_command = "scale up"

            [[warpscript_probes.levels]]
            level = 2
            scale_down_threshold = 50.0
            downscale_command = "scale down"
            "#,
        );
        let mut config: Config = toml::from_str(&toml).unwrap();
        assert!(config.validate().is_ok());
        assert_eq!(config.warpscript_probes[0].levels.len(), 2);
        assert_eq!(config.warpscript_probes[0].levels[0].level, 1);
        assert_eq!(config.warpscript_probes[0].levels[1].level, 2);
    }

    #[test]
    fn test_warpscript_levels_plural_expands() {
        let toml = warpscript_probe_toml(
            r#"
            [[warpscript_probes.levels]]
            level = 1
            scale_up_threshold = 70.0
            upscale_command = "scale up"

            [[warpscript_probes.levels]]
            levels = [2, 3]
            scale_down_threshold = 45.0
            downscale_command = "clever scale --app ${APP_ID} --flavor XS"
            "#,
        );
        let mut config: Config = toml::from_str(&toml).unwrap();
        assert!(config.validate().is_ok());
        let levels = &config.warpscript_probes[0].levels;
        assert_eq!(levels.len(), 3);
        assert_eq!(levels[1].level, 2);
        assert_eq!(levels[2].level, 3);
        assert_eq!(levels[1].scale_down_threshold, Some(45.0));
        assert_eq!(levels[2].scale_down_threshold, Some(45.0));
        assert_eq!(
            levels[1].downscale_command.as_deref(),
            Some("clever scale --app ${APP_ID} --flavor XS")
        );
        assert_eq!(
            levels[2].downscale_command.as_deref(),
            Some("clever scale --app ${APP_ID} --flavor XS")
        );
    }

    #[test]
    fn test_warpscript_level_and_levels_both_rejected() {
        let toml = warpscript_probe_toml(
            r#"
            [[warpscript_probes.levels]]
            level = 1
            levels = [1, 2]
            "#,
        );
        assert!(toml::from_str::<Config>(&toml).is_err());
    }

    #[test]
    fn test_warpscript_neither_level_nor_levels_rejected() {
        let toml = warpscript_probe_toml(
            r#"
            [[warpscript_probes.levels]]
            scale_up_threshold = 70.0
            "#,
        );
        assert!(toml::from_str::<Config>(&toml).is_err());
    }

    #[test]
    fn test_warpscript_duplicate_level_rejected_by_validate() {
        let toml = warpscript_probe_toml(
            r#"
            [[warpscript_probes.levels]]
            level = 1

            [[warpscript_probes.levels]]
            levels = [1, 2]
            "#,
        );
        // Deserialisation succeeds (duplicates not detected there), validate() catches it
        let mut config: Config = toml::from_str(&toml).unwrap();
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

        let mut config: Config = toml::from_str(toml_content).unwrap();
        assert!(config.validate().is_err());
    }

    #[test]
    fn test_warpscript_non_contiguous_levels_rejected() {
        let toml = warpscript_probe_toml(
            r#"
            [[warpscript_probes.levels]]
            level = 1
            scale_up_threshold = 70.0

            [[warpscript_probes.levels]]
            level = 3
            scale_down_threshold = 50.0
            "#,
        );
        let mut config: Config = toml::from_str(&toml).unwrap();
        assert!(config.validate().is_err());
    }

    #[test]
    fn test_warpscript_contiguous_levels_accepted() {
        let toml = warpscript_probe_toml(
            r#"
            [[warpscript_probes.levels]]
            level = 2
            scale_up_threshold = 70.0

            [[warpscript_probes.levels]]
            level = 3
            scale_down_threshold = 50.0
            "#,
        );
        let mut config: Config = toml::from_str(&toml).unwrap();
        assert!(config.validate().is_ok());
    }
}

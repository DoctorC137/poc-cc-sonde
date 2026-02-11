use serde::Deserialize;
use std::collections::HashMap;
use std::fs;
use std::path::Path;

#[derive(Debug, Deserialize, Clone)]
pub struct Config {
    pub probes: Vec<Probe>,
}

#[derive(Debug, Deserialize, Clone)]
pub struct Probe {
    pub name: String,
    pub url: String,
    pub interval_seconds: u64,
    pub checks: Checks,
    pub on_failure_command: Option<String>,
    #[serde(default = "default_timeout")]
    pub command_timeout_seconds: u64,
    /// Delay in seconds before next execution after a successful check (defaults to interval_seconds)
    pub delay_after_success_seconds: Option<u64>,
    /// Delay in seconds before next execution after a failed check (defaults to interval_seconds)
    pub delay_after_failure_seconds: Option<u64>,
}

impl Probe {
    pub fn get_delay_after_success(&self) -> u64 {
        self.delay_after_success_seconds.unwrap_or(self.interval_seconds)
    }

    pub fn get_delay_after_failure(&self) -> u64 {
        self.delay_after_failure_seconds.unwrap_or(self.interval_seconds)
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

impl Config {
    pub fn from_file<P: AsRef<Path>>(path: P) -> Result<Self, Box<dyn std::error::Error>> {
        let contents = fs::read_to_string(path)?;
        let config: Config = toml::from_str(&contents)?;
        config.validate()?;
        Ok(config)
    }

    fn validate(&self) -> Result<(), Box<dyn std::error::Error>> {
        if self.probes.is_empty() {
            return Err("Configuration must contain at least one probe".into());
        }

        for probe in &self.probes {
            if probe.name.is_empty() {
                return Err("Probe name cannot be empty".into());
            }
            if probe.url.is_empty() {
                return Err(format!("Probe '{}' has empty URL", probe.name).into());
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
            [[probes]]
            name = "Test Probe"
            url = "https://example.com"
            interval_seconds = 60

            [probes.checks]
            expected_status = 200
        "#;

        let config: Config = toml::from_str(toml_content).unwrap();
        assert!(config.validate().is_ok());
    }

    #[test]
    fn test_empty_probes() {
        let toml_content = r#"
            probes = []
        "#;

        let config: Config = toml::from_str(toml_content).unwrap();
        assert!(config.validate().is_err());
    }

    #[test]
    fn test_no_checks() {
        let toml_content = r#"
            [[probes]]
            name = "Test"
            url = "https://example.com"
            interval_seconds = 60

            [probes.checks]
        "#;

        let config: Config = toml::from_str(toml_content).unwrap();
        assert!(config.validate().is_err());
    }
}

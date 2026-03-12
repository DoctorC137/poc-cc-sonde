use crate::config::Probe;
use regex::Regex;
use reqwest::Client;
use std::time::Instant;
use tracing::{error, info, warn};

pub fn build_client() -> Result<Client, reqwest::Error> {
    Client::builder().build()
}

#[derive(Debug)]
pub enum CheckFailure {
    Status {
        expected: u16,
        actual: u16,
    },
    BodyContains {
        expected: String,
        body: String,
    },
    BodyRegex {
        pattern: String,
        body: String,
    },
    Header {
        key: String,
        expected: String,
        actual: Option<String>,
    },
    RequestError {
        error: String,
    },
}

impl std::fmt::Display for CheckFailure {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            CheckFailure::Status { expected, actual } => {
                write!(
                    f,
                    "Status check failed: expected {}, got {}",
                    expected, actual
                )
            }
            CheckFailure::BodyContains { expected, body } => {
                write!(
                    f,
                    "Body contains check failed: expected '{}' in body (length: {})",
                    expected,
                    body.len()
                )
            }
            CheckFailure::BodyRegex { pattern, body } => {
                write!(
                    f,
                    "Body regex check failed: pattern '{}' not found in body (length: {})",
                    pattern,
                    body.len()
                )
            }
            CheckFailure::Header {
                key,
                expected,
                actual,
            } => {
                write!(
                    f,
                    "Header check failed: expected '{}' = '{}', got {:?}",
                    key, expected, actual
                )
            }
            CheckFailure::RequestError { error } => {
                write!(f, "Request error: {}", error)
            }
        }
    }
}

pub async fn execute_probe(probe: &Probe, client: &Client) -> Result<bool, CheckFailure> {
    let url = probe.url.as_deref().unwrap_or("");
    let start = Instant::now();
    info!(
        probe_name = %probe.name,
        url = %url,
        "Starting HTTP probe"
    );

    // Execute HTTP request
    let timeout = std::time::Duration::from_secs(probe.get_request_timeout());
    let response = match client.get(url).timeout(timeout).send().await {
        Ok(resp) => resp,
        Err(e) => {
            error!(
                probe_name = %probe.name,
                url = %url,
                error = %e,
                "HTTP request failed"
            );
            return Err(CheckFailure::RequestError {
                error: e.to_string(),
            });
        }
    };

    let duration = start.elapsed();
    let status = response.status().as_u16();
    // Save headers before consuming the response body — avoids a second HTTP request
    let headers = response.headers().clone();

    info!(
        probe_name = %probe.name,
        url = %url,
        status = status,
        duration_ms = duration.as_millis(),
        "Received HTTP response"
    );

    // Check status code
    if let Some(expected_status) = probe.checks.expected_status {
        if status != expected_status {
            warn!(
                probe_name = %probe.name,
                expected = expected_status,
                actual = status,
                "Status code check failed"
            );
            return Err(CheckFailure::Status {
                expected: expected_status,
                actual: status,
            });
        }
        info!(
            probe_name = %probe.name,
            status = status,
            "Status code check passed"
        );
    }

    // Get response body for body checks
    let body = if probe.checks.expected_body_contains.is_some()
        || probe.checks.expected_body_regex.is_some()
    {
        match response.text().await {
            Ok(text) => text,
            Err(e) => {
                error!(
                    probe_name = %probe.name,
                    error = %e,
                    "Failed to read response body"
                );
                return Err(CheckFailure::RequestError {
                    error: format!("Failed to read body: {}", e),
                });
            }
        }
    } else {
        String::new()
    };

    // Check body contains
    if let Some(ref expected_contains) = probe.checks.expected_body_contains {
        if !body.contains(expected_contains) {
            warn!(
                probe_name = %probe.name,
                expected = %expected_contains,
                body_preview = %&body[..body.len().min(100)],
                "Body contains check failed"
            );
            return Err(CheckFailure::BodyContains {
                expected: expected_contains.clone(),
                body: body.clone(),
            });
        }
        info!(
            probe_name = %probe.name,
            "Body contains check passed"
        );
    }

    // Check body regex — use pre-compiled version when available (normal path),
    // fall back to on-the-fly compilation for probes built manually in tests.
    if let Some(ref pattern) = probe.checks.expected_body_regex {
        let fallback;
        let re: &Regex = match probe.checks.compiled_body_regex.as_ref() {
            Some(r) => r,
            None => {
                fallback = Regex::new(pattern).map_err(|e| CheckFailure::RequestError {
                    error: format!("Invalid regex: {}", e),
                })?;
                &fallback
            }
        };

        if !re.is_match(&body) {
            warn!(
                probe_name = %probe.name,
                pattern = %pattern,
                body_preview = %&body[..body.len().min(100)],
                "Body regex check failed"
            );
            return Err(CheckFailure::BodyRegex {
                pattern: pattern.clone(),
                body: body.clone(),
            });
        }
        info!(
            probe_name = %probe.name,
            pattern = %pattern,
            "Body regex check passed"
        );
    }

    // Check headers from the saved first response — no second HTTP request needed
    if let Some(ref expected_headers) = probe.checks.expected_header {
        for (key, expected_value) in expected_headers {
            let actual_value = headers
                .get(key)
                .and_then(|v| v.to_str().ok())
                .map(|s| s.to_string());

            match actual_value {
                Some(ref actual) if actual == expected_value => {
                    info!(
                        probe_name = %probe.name,
                        header = %key,
                        value = %expected_value,
                        "Header check passed"
                    );
                }
                actual => {
                    warn!(
                        probe_name = %probe.name,
                        header = %key,
                        expected = %expected_value,
                        actual = ?actual,
                        "Header check failed"
                    );
                    return Err(CheckFailure::Header {
                        key: key.clone(),
                        expected: expected_value.clone(),
                        actual,
                    });
                }
            }
        }
    }

    info!(
        probe_name = %probe.name,
        duration_ms = duration.as_millis(),
        "All checks passed"
    );

    Ok(true)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::{Checks, Probe};

    #[tokio::test]
    async fn test_successful_probe() {
        let mut server = mockito::Server::new_async().await;
        let mock = server
            .mock("GET", "/")
            .with_status(200)
            .with_body(r#"{"status":"ok"}"#)
            .create_async()
            .await;

        let probe = Probe {
            name: "Test".to_string(),
            url: Some(server.url()),
            interval_seconds: 1,
            checks: Checks {
                expected_status: Some(200),
                expected_body_contains: Some("ok".to_string()),
                expected_body_regex: None,
                expected_header: None,
                compiled_body_regex: None,
            },
            on_failure_command: None,
            command_timeout_seconds: 30,
            delay_after_success_seconds: None,
            delay_after_failure_seconds: None,
            delay_after_command_success_seconds: None,
            delay_after_command_failure_seconds: None,
            failure_retries_before_command: None,
            request_timeout_seconds: None,
            apps: vec![],
        };

        let client = build_client().unwrap();
        let result = execute_probe(&probe, &client).await;
        assert!(result.is_ok());
        mock.assert_async().await;
    }
}

use reqwest::Client;
use tracing::{debug, info};

/// Maximum bytes read from a WarpScript response body (1 MiB).
const MAX_WARP_BODY_BYTES: usize = 1024 * 1024;

pub fn build_client() -> Result<Client, reqwest::Error> {
    Client::builder().build()
}

#[derive(Debug)]
pub enum WarpScriptError {
    RequestError(String),
    ParseError(String),
    NoScalarValue,
}

impl std::fmt::Display for WarpScriptError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            WarpScriptError::RequestError(e) => write!(f, "Warp API request failed: {}", e),
            WarpScriptError::ParseError(e) => write!(f, "Failed to parse Warp response: {}", e),
            WarpScriptError::NoScalarValue => write!(f, "No scalar value returned from WarpScript"),
        }
    }
}

impl std::error::Error for WarpScriptError {}

/// Execute a WarpScript and return the scalar value.
/// `script` is the script content (read once by the caller).
/// `token`, `endpoint`, and `timeout_seconds` are resolved once by the caller before the loop.
pub async fn execute_warpscript(
    probe_name: &str,
    script: &str,
    app_id: Option<&str>,
    token: &str,
    endpoint: &str,
    timeout_seconds: u64,
    client: &Client,
) -> Result<f64, WarpScriptError> {
    // Substitute ${WARP_TOKEN} with actual token
    let script = script.replace("${WARP_TOKEN}", token);

    // Substitute ${APP_ID} with the app_id value
    let script = if let Some(id) = app_id {
        script.replace("${APP_ID}", id)
    } else {
        script
    };

    debug!(
        probe_name = %probe_name,
        script_length = script.len(),
        app_id = ?app_id,
        "Token and app_id substitution completed"
    );

    debug!(
        probe_name = %probe_name,
        endpoint = %crate::utils::sanitize_url_for_log(endpoint),
        "Executing WarpScript"
    );

    let mut response = client
        .post(endpoint)
        .header("Content-Type", "text/plain")
        .timeout(std::time::Duration::from_secs(timeout_seconds))
        .body(script)
        .send()
        .await
        .map_err(|e| WarpScriptError::RequestError(e.to_string()))?;

    let status = response.status();

    if !status.is_success() {
        let error_body = {
            let mut buf: Vec<u8> = Vec::new();
            loop {
                match response.chunk().await {
                    Ok(Some(chunk)) => {
                        let remaining = MAX_WARP_BODY_BYTES.saturating_sub(buf.len());
                        if remaining == 0 {
                            break;
                        }
                        buf.extend_from_slice(&chunk[..chunk.len().min(remaining)]);
                        if buf.len() >= MAX_WARP_BODY_BYTES {
                            break;
                        }
                    }
                    Ok(None) => break,
                    Err(_) => break,
                }
            }
            String::from_utf8_lossy(&buf).into_owned()
        };
        // Redact token from error body before logging (Warp may echo the submitted script)
        let safe_body = error_body.replace(token, "****");
        return Err(WarpScriptError::RequestError(format!(
            "HTTP {}: {}",
            status, safe_body
        )));
    }

    let body = {
        let mut buf: Vec<u8> = Vec::new();
        loop {
            match response.chunk().await {
                Ok(Some(chunk)) => {
                    let remaining = MAX_WARP_BODY_BYTES.saturating_sub(buf.len());
                    if remaining == 0 {
                        break;
                    }
                    buf.extend_from_slice(&chunk[..chunk.len().min(remaining)]);
                    if buf.len() >= MAX_WARP_BODY_BYTES {
                        break;
                    }
                }
                Ok(None) => break,
                Err(e) => {
                    return Err(WarpScriptError::RequestError(e.to_string()));
                }
            }
        }
        String::from_utf8_lossy(&buf).into_owned()
    };

    debug!(
        probe_name = %probe_name,
        response_length = body.len(),
        "Received WarpScript response"
    );

    // Parse response to extract scalar value
    // Warp returns JSON array, we need the last element which should be a number
    let value = parse_warp_response(&body)?;

    info!(
        probe_name = %probe_name,
        value = value,
        "WarpScript execution successful"
    );

    Ok(value)
}

/// Parse Warp response to extract scalar value
/// Warp returns a JSON array, we take the last element
fn parse_warp_response(body: &str) -> Result<f64, WarpScriptError> {
    let json: serde_json::Value =
        serde_json::from_str(body).map_err(|e| WarpScriptError::ParseError(e.to_string()))?;

    // Response should be an array
    let array = json
        .as_array()
        .ok_or_else(|| WarpScriptError::ParseError("Response is not an array".to_string()))?;

    if array.is_empty() {
        return Err(WarpScriptError::NoScalarValue);
    }

    // Get the last element (top of stack)
    let last = &array[array.len() - 1];

    // Try to extract as number
    if let Some(num) = last.as_f64() {
        return Ok(num);
    }

    // Try to extract as integer
    if let Some(num) = last.as_i64() {
        return Ok(num as f64);
    }

    Err(WarpScriptError::ParseError(format!(
        "Last element is not a number: {:?}",
        last
    )))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_parse_warp_response_simple() {
        let response = "[42.5]";
        let value = parse_warp_response(response).unwrap();
        assert_eq!(value, 42.5);
    }

    #[test]
    fn test_parse_warp_response_multiple() {
        let response = "[1, 2, 3, 85.7]";
        let value = parse_warp_response(response).unwrap();
        assert_eq!(value, 85.7);
    }

    #[test]
    fn test_parse_warp_response_integer() {
        let response = "[100]";
        let value = parse_warp_response(response).unwrap();
        assert_eq!(value, 100.0);
    }

    #[test]
    fn test_parse_warp_response_empty() {
        let response = "[]";
        let result = parse_warp_response(response);
        assert!(result.is_err());
    }

    #[test]
    fn test_parse_warp_response_invalid() {
        let response = "[\"not a number\"]";
        let result = parse_warp_response(response);
        assert!(result.is_err());
    }
}

use std::env;
use std::fs;
use tracing::{debug, error, info};

#[derive(Debug)]
pub enum WarpScriptError {
    FileReadError(std::io::Error),
    MissingEndpoint,
    MissingToken,
    RequestError(String),
    ParseError(String),
    NoScalarValue,
}

impl std::fmt::Display for WarpScriptError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            WarpScriptError::FileReadError(e) => write!(f, "Failed to read WarpScript file: {}", e),
            WarpScriptError::MissingEndpoint => write!(f, "WARP_ENDPOINT environment variable not set"),
            WarpScriptError::MissingToken => write!(f, "warp_token not configured for this app and WARP_TOKEN environment variable not set"),
            WarpScriptError::RequestError(e) => write!(f, "Warp API request failed: {}", e),
            WarpScriptError::ParseError(e) => write!(f, "Failed to parse Warp response: {}", e),
            WarpScriptError::NoScalarValue => write!(f, "No scalar value returned from WarpScript"),
        }
    }
}

impl std::error::Error for WarpScriptError {}

/// Execute a WarpScript file and return the scalar value
pub async fn execute_warpscript(
    probe_name: &str,
    warpscript_file: &str,
    app_id: Option<&str>,
    custom_token: Option<&str>,
) -> Result<f64, WarpScriptError> {
    // Read environment variables
    let endpoint = env::var("WARP_ENDPOINT")
        .map_err(|_| WarpScriptError::MissingEndpoint)?;

    // Use custom token if provided, otherwise fallback to WARP_TOKEN env var
    let token = if let Some(t) = custom_token {
        t.to_string()
    } else {
        env::var("WARP_TOKEN")
            .map_err(|_| WarpScriptError::MissingToken)?
    };

    // Read WarpScript file
    let script = fs::read_to_string(warpscript_file)
        .map_err(WarpScriptError::FileReadError)?;

    debug!(
        probe_name = %probe_name,
        file = %warpscript_file,
        "Read WarpScript file"
    );

    // Substitute ${WARP_TOKEN} with actual token
    let script = script.replace("${WARP_TOKEN}", &token);

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

    // Execute WarpScript via HTTP POST
    let client = reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(30))
        .build()
        .map_err(|e| WarpScriptError::RequestError(e.to_string()))?;

    info!(
        probe_name = %probe_name,
        endpoint = %endpoint,
        "Executing WarpScript"
    );

    let response = client
        .post(&endpoint)
        .header("Content-Type", "text/plain")
        .body(script)
        .send()
        .await
        .map_err(|e| WarpScriptError::RequestError(e.to_string()))?;

    let status = response.status();

    if !status.is_success() {
        let error_body = response.text().await.unwrap_or_else(|_| "Unknown error".to_string());
        error!(
            probe_name = %probe_name,
            status = %status,
            error = %error_body,
            "WarpScript execution failed"
        );
        return Err(WarpScriptError::RequestError(format!("HTTP {}: {}", status, error_body)));
    }

    let body = response
        .text()
        .await
        .map_err(|e| WarpScriptError::RequestError(e.to_string()))?;

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
    let json: serde_json::Value = serde_json::from_str(body)
        .map_err(|e| WarpScriptError::ParseError(e.to_string()))?;

    // Response should be an array
    let array = json.as_array()
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

    Err(WarpScriptError::ParseError(
        format!("Last element is not a number: {:?}", last)
    ))
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

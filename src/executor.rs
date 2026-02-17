use std::process::Output;
use std::time::Duration;
use tokio::process::Command;
use tracing::{error, info, warn};

pub async fn execute_command(
    command: &str,
    timeout_seconds: u64,
) -> Result<Output, Box<dyn std::error::Error>> {
    info!(
        command = %command,
        timeout_seconds = timeout_seconds,
        "Executing command"
    );

    if command.trim().is_empty() {
        return Err("Empty command".into());
    }

    // Execute through shell to support &&, ||, ;, pipes, etc.
    let child = Command::new("sh").args(["-c", command]).output();

    let output = match tokio::time::timeout(Duration::from_secs(timeout_seconds), child).await {
        Ok(Ok(output)) => output,
        Ok(Err(e)) => {
            error!(
                command = %command,
                error = %e,
                "Failed to execute command"
            );
            return Err(e.into());
        }
        Err(_) => {
            error!(
                command = %command,
                timeout_seconds = timeout_seconds,
                "Command execution timed out"
            );
            return Err("Command execution timed out".into());
        }
    };

    let exit_code = output.status.code().unwrap_or(-1);
    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);

    if output.status.success() {
        info!(
            command = %command,
            exit_code = exit_code,
            stdout = %stdout.trim(),
            "Command executed successfully"
        );
    } else {
        warn!(
            command = %command,
            exit_code = exit_code,
            stdout = %stdout.trim(),
            stderr = %stderr.trim(),
            "Command executed with non-zero exit code"
        );
    }

    Ok(output)
}

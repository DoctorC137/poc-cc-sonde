# POC Sonde - HTTP Monitoring Application

A Rust-based HTTP monitoring application that periodically checks HTTP endpoints and executes shell commands when checks fail.

## Features

- **Periodic HTTP Monitoring**: Configure multiple probes with custom intervals
- **Flexible Checks**: Support for multiple verification methods:
  - HTTP status code validation
  - Response body content matching (substring)
  - Response body regex pattern matching
  - HTTP header validation
- **Failure Actions**: Execute shell commands when checks fail
- **Concurrent Execution**: Run multiple probes simultaneously
- **Health Check Endpoint**: Optional HTTP server for monitoring the application itself
- **Structured Logging**: Detailed logging with configurable levels
- **TOML Configuration**: Simple, human-readable configuration format

## Installation

### Prerequisites

- Rust 1.70 or later
- Cargo (comes with Rust)

### Building from Source

```bash
# Clone or navigate to the repository
cd poc-sonde

# Build in release mode for optimal performance
cargo build --release

# Build with Redis persistence support
cargo build --release --features redis-persistence

# The binary will be available at ./target/release/poc-sonde
```

**Features:**
- Default build: In-memory persistence (no external dependencies)
- `redis-persistence`: Enables Redis-based state persistence for production deployments

## Configuration

Create a `config.toml` file with your probe configurations. See the example below:

```toml
[[probes]]
name = "API Health Check"
url = "https://api.example.com/health"
interval_seconds = 60
on_failure_command = "systemctl restart my-service"
command_timeout_seconds = 30  # Optional, defaults to 30

[probes.checks]
expected_status = 200
expected_body_contains = "\"status\":\"ok\""

[[probes]]
name = "Service with Header Check"
url = "https://service.example.com/status"
interval_seconds = 30

[probes.checks]
expected_status = 200

[probes.checks.expected_header]
"X-Service-Status" = "healthy"
"Content-Type" = "application/json"

[[probes]]
name = "Regex Pattern Check"
url = "https://api.example.com/version"
interval_seconds = 120

[probes.checks]
expected_status = 200
expected_body_regex = "\"version\":\\s*\"\\d+\\.\\d+\\.\\d+\""
```

### Configuration Parameters

#### Probe Configuration

- `name` (required): A descriptive name for the probe
- `url` (required): The HTTP endpoint to monitor
- `interval_seconds` (required): Default interval for running the probe (in seconds)
- `on_failure_command` (optional): Shell command to execute when checks fail
- `command_timeout_seconds` (optional): Timeout for command execution (default: 30)
- `delay_after_success_seconds` (optional): Delay before next execution after successful check (defaults to `interval_seconds`)
- `delay_after_failure_seconds` (optional): Delay before next execution after failed check (defaults to `interval_seconds`)

#### Check Types

At least one check must be configured for each probe:

- `expected_status`: Expected HTTP status code (e.g., 200, 404)
- `expected_body_contains`: String that must be present in response body
- `expected_body_regex`: Regex pattern that must match the response body
- `expected_header`: Key-value pairs of expected HTTP headers

All configured checks must pass for the probe to succeed.

## Usage

### Running the Application

```bash
# Use default config.toml in current directory
cargo run --release

# Or specify a custom configuration file
cargo run --release -- /path/to/config.toml

# Or run the compiled binary directly
./target/release/poc-sonde config.toml

# Enable health check endpoint on default port 8080
./target/release/poc-sonde --healthcheck

# Enable health check endpoint on custom port
./target/release/poc-sonde --healthcheck --healthcheck-port 9090

# Combine with custom config
./target/release/poc-sonde myconfig.toml --healthcheck --healthcheck-port 3000
```

### Command Line Options

```
Usage: poc-sonde [OPTIONS] [CONFIG]

Arguments:
  [CONFIG]  Configuration file path [default: config.toml]

Options:
      --healthcheck
          Enable health check HTTP server
      --healthcheck-port <HEALTHCHECK_PORT>
          Port for health check server (requires --healthcheck) [default: 8080]
  -h, --help
          Print help
  -V, --version
          Print version
```

### Health Check Endpoint

When enabled with `--healthcheck`, the application starts an HTTP server that responds to all requests with:
- **Status**: 200 OK
- **Body**: "Probe is running"
- **Port**: Configurable via `--healthcheck-port` (default: 8080)

This endpoint can be used to monitor the health of the monitoring application itself (meta-monitoring).

```bash
# Start with health check on port 8080
./target/release/poc-sonde --healthcheck

# Test the health check endpoint
curl http://localhost:8080
# Output: Probe is running
```

### Retry Strategies

Configure different delays after success or failure to implement retry strategies:

```toml
[[probes]]
name = "Critical API"
url = "https://api.example.com/health"
interval_seconds = 300              # Default interval (5 minutes)
delay_after_success_seconds = 300   # Continue checking every 5 minutes when healthy
delay_after_failure_seconds = 30    # Fast retry: check every 30 seconds when unhealthy
on_failure_command = "echo 'API down, checking every 30s'"

[probes.checks]
expected_status = 200
```

**Use cases:**
- **Fast failure detection**: Set short `delay_after_failure_seconds` to quickly detect recovery
- **Reduced load when healthy**: Set longer `delay_after_success_seconds` to reduce monitoring overhead
- **Exponential backoff**: Increase `delay_after_failure_seconds` to avoid overwhelming failing services

### Redis Persistence (Optional)

Enable Redis persistence to maintain probe state across restarts:

```bash
# Set Redis URL environment variable
export REDIS_URL="redis://localhost:6379"

# Build with Redis support
cargo build --release --features redis-persistence

# Run the application
./target/release/poc-sonde
```

**Without Redis URL**: The application uses in-memory persistence (state is lost on restart).

**With Redis URL**: Probe states are persisted to Redis:
- Last execution timestamp
- Success/failure status
- Next scheduled execution time
- On restart, probes resume from their saved state

**Benefits:**
- No duplicate checks immediately after restart
- Maintains retry schedules across deployments
- Enables horizontal scaling (future feature)

### Configuring Log Levels

Use the `RUST_LOG` environment variable to control logging verbosity:

```bash
# Info level (default)
RUST_LOG=info cargo run

# Debug level (detailed)
RUST_LOG=debug cargo run

# Trace level (very detailed)
RUST_LOG=trace cargo run

# Filter specific modules
RUST_LOG=poc_sonde::probe=debug cargo run
```

### Graceful Shutdown

Press `Ctrl+C` to gracefully shut down the application. All running probes will be terminated.

## Log Format

The application produces structured logs with the following information:

```
2024-01-15T10:30:45.123456Z  INFO poc_sonde: Starting HTTP monitoring application
2024-01-15T10:30:45.234567Z  INFO poc_sonde: Loading configuration config_path="config.toml"
2024-01-15T10:30:45.345678Z  INFO poc_sonde::probe: Starting HTTP probe probe_name="API Health Check" url="https://api.example.com/health"
2024-01-15T10:30:45.456789Z  INFO poc_sonde::probe: Received HTTP response probe_name="API Health Check" status=200 duration_ms=111
2024-01-15T10:30:45.567890Z  INFO poc_sonde::probe: All checks passed probe_name="API Health Check" duration_ms=222
```

## Examples

### Example 1: Monitor API Health

```toml
[[probes]]
name = "Production API"
url = "https://api.myapp.com/health"
interval_seconds = 30
on_failure_command = "curl -X POST https://hooks.slack.com/... -d '{\"text\":\"API is down!\"}'"

[probes.checks]
expected_status = 200
expected_body_contains = "healthy"
```

### Example 2: Monitor Service with Auto-Restart

```toml
[[probes]]
name = "Backend Service"
url = "http://localhost:8080/status"
interval_seconds = 60
on_failure_command = "systemctl restart myservice"

[probes.checks]
expected_status = 200
```

### Example 3: Validate API Response Format

```toml
[[probes]]
name = "User API"
url = "https://api.myapp.com/users/1"
interval_seconds = 120

[probes.checks]
expected_status = 200
expected_body_regex = "\\{\"id\":\\s*\\d+,\\s*\"name\":\\s*\".+\"\\}"
```

## Testing

```bash
# Run all tests
cargo test

# Run tests with output
cargo test -- --nocapture

# Run specific test
cargo test test_valid_config
```

## Troubleshooting

### Configuration Errors

If you see "Configuration must contain at least one probe":
- Ensure your `config.toml` has at least one `[[probes]]` section

If you see "Probe has no checks configured":
- Add at least one check type to `[probes.checks]`

### Network Errors

If probes fail with connection errors:
- Verify the URLs are accessible from your machine
- Check firewall settings
- Ensure DNS resolution is working

### Command Execution Errors

If failure commands don't execute:
- Verify the command exists and is in your PATH
- Check permissions for the command
- Review logs with `RUST_LOG=debug` for detailed error messages

### Timeout Issues

If commands are timing out:
- Increase `command_timeout_seconds` in the probe configuration
- Ensure the command isn't hanging or waiting for input

## Performance Considerations

- Each probe runs in its own async task
- HTTP requests have a 30-second timeout
- Shell commands respect the configured timeout
- The application is designed to handle dozens of probes concurrently

## Security Notes

- Shell commands are executed with the same privileges as the application
- Be cautious with commands that require elevated permissions
- Validate URLs to prevent unintended network access
- Consider using specific commands rather than shell scripts for better security

## Improvements and Future Features

Potential enhancements:
- HTTPS with custom certificates
- Prometheus metrics export
- Webhook notifications
- Configuration hot-reload
- Web UI for status visualization
- Multiple notification channels
- Retry logic with exponential backoff
- Health check history and statistics

## License

This is a proof-of-concept (POC) project. Use at your own discretion.

## Contributing

This is a POC project. Feel free to fork and modify for your needs.

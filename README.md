# POC Sonde - HTTP Monitoring Application

A Rust-based HTTP monitoring application that periodically checks HTTP endpoints and executes shell commands when checks fail.

## Features

- **Periodic HTTP Monitoring**: Configure multiple probes with custom intervals
- **Flexible Checks**: Support for multiple verification methods:
  - HTTP status code validation
  - Response body content matching (substring)
  - Response body regex pattern matching
  - HTTP header validation
- **WarpScript Probes**: Execute WarpScript queries and monitor scalar values with level-based auto-scaling
  - Multi-level scaling (1, 2, 3, ...N levels)
  - Automatic level transitions based on metric thresholds
  - Execute scale up/down commands per level
  - Manage multiple applications with a single configuration (apps)
  - Custom Warp token per application
  - Token and app ID substitution in WarpScript and commands
  - Each app instance maintains independent state
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
[[healthcheck_probes]]
name = "API Health Check"
url = "https://api.example.com/health"
interval_seconds = 60
on_failure_command = "systemctl restart my-service"
command_timeout_seconds = 30  # Optional, defaults to 30

[healthcheck_probes.checks]
expected_status = 200
expected_body_contains = "\"status\":\"ok\""

[[healthcheck_probes]]
name = "Service with Header Check"
url = "https://service.example.com/status"
interval_seconds = 30

[healthcheck_probes.checks]
expected_status = 200

[healthcheck_probes.checks.expected_header]
"X-Service-Status" = "healthy"
"Content-Type" = "application/json"

[[healthcheck_probes]]
name = "Regex Pattern Check"
url = "https://api.example.com/version"
interval_seconds = 120

[healthcheck_probes.checks]
expected_status = 200
expected_body_regex = "\"version\":\\s*\"\\d+\\.\\d+\\.\\d+\""

# Monitor multiple apps with the same configuration
[[healthcheck_probes]]
name = "App Monitor"
interval_seconds = 60
on_failure_command = "clever restart --app ${APP_ID}"
failure_retries_before_command = 1

[healthcheck_probes.checks]
expected_status = 200

[[healthcheck_probes.apps]]
id = "app_frontend"
url = "https://frontend.example.com/health"

[[healthcheck_probes.apps]]
id = "app_backend"
url = "https://backend.example.com/health"
```

### Configuration Parameters

#### Probe Configuration

- `name` (required): A descriptive name for the probe
- `url` (optional): The HTTP endpoint to monitor — required if `apps` is not specified
- `apps` (optional): List of applications to monitor — required if `url` is not specified
- `interval_seconds` (required): Default interval for running the probe (in seconds)
- `on_failure_command` (optional): Shell command to execute when checks fail (supports `${APP_ID}`, `&&`, pipes)
- `command_timeout_seconds` (optional): Timeout for command execution (default: 30)
- `delay_after_success_seconds` (optional): Delay before next execution after successful check (defaults to `interval_seconds`)
- `delay_after_failure_seconds` (optional): Delay before next execution after failed check (defaults to `interval_seconds`)
- `failure_retries_before_command` (optional): Number of consecutive failures before executing the failure command (default: 0 = execute immediately)

**Note:** `url` and `apps` are mutually exclusive. Use `url` for a single endpoint, `apps` to monitor multiple apps with the same configuration.

**App Configuration:**
- `id` (required): Application identifier — substituted as `${APP_ID}` in `on_failure_command`
- `url` (required): Health check URL for this specific app

#### Check Types

At least one check must be configured for each probe:

- `expected_status`: Expected HTTP status code (e.g., 200, 404)
- `expected_body_contains`: String that must be present in response body
- `expected_body_regex`: Regex pattern that must match the response body
- `expected_header`: Key-value pairs of expected HTTP headers

The same checks apply to all apps within a probe.

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
[[healthcheck_probes]]
name = "Critical API"
url = "https://api.example.com/health"
interval_seconds = 300              # Default interval (5 minutes)
delay_after_success_seconds = 300   # Continue checking every 5 minutes when healthy
delay_after_failure_seconds = 30    # Fast retry: check every 30 seconds when unhealthy
on_failure_command = "echo 'API down, checking every 30s'"

[healthcheck_probes.checks]
expected_status = 200
```

**Use cases:**
- **Fast failure detection**: Set short `delay_after_failure_seconds` to quickly detect recovery
- **Reduced load when healthy**: Set longer `delay_after_success_seconds` to reduce monitoring overhead
- **Exponential backoff**: Increase `delay_after_failure_seconds` to avoid overwhelming failing services

### Failure Retry Threshold

Avoid false alerts by requiring multiple consecutive failures before executing the failure command:

```toml
[[healthcheck_probes]]
name = "API with Transient Issues"
url = "https://api.example.com/health"
interval_seconds = 60
delay_after_failure_seconds = 10            # Retry every 10 seconds on failure
failure_retries_before_command = 3          # Only execute command after 3 consecutive failures
on_failure_command = "alert-admin.sh"

[healthcheck_probes.checks]
expected_status = 200
```

**How it works:**
- The probe retries failed checks according to `delay_after_failure_seconds`
- Consecutive failures are counted and persisted
- The failure command is executed only when `consecutive_failures > failure_retries_before_command`
- Counter resets to 0 on first successful check

**Configuration examples:**
- `failure_retries_before_command = 0` (default): Execute command immediately on first failure
- `failure_retries_before_command = 3`: Wait for 3 failures before alerting (good for services with occasional hiccups)
- `failure_retries_before_command = 10`: High tolerance (good for non-critical or flaky services)

**Benefits:**
- Reduces false alerts from transient network issues
- Gives services time to self-recover before intervention
- Prevents command/alert spam during outages
- Different tolerance levels per probe

### WarpScript Probes - Auto-Scaling

Monitor metrics from Warp 10 and automatically scale your applications based on numeric thresholds.

#### Prerequisites

Set the required environment variable:
```bash
# Required: Warp API endpoint
export WARP_ENDPOINT="https://warp.example.com/api/v0/exec"

# Optional: Default Warp token (fallback for apps without warp_token)
export WARP_TOKEN="YOUR_READ_TOKEN"
```

**Note:** Each app can have its own `warp_token` in the configuration. If an app doesn't specify `warp_token`, it will use the `WARP_TOKEN` environment variable as fallback.

#### Configuration Example

```toml
[[warpscript_probes]]
name = "CPU Auto-Scaler"
warpscript_file = "warpscript/cpu_usage.mc2"
interval_seconds = 60
delay_after_scale_seconds = 120  # Wait 2min after scaling

# Define apps with optional custom tokens
[[warpscript_probes.apps]]
id = "app_frontend"
warp_token = "READ_TOKEN_FRONTEND"  # Optional: custom token

[[warpscript_probes.apps]]
id = "app_backend"
# warp_token not specified: uses WARP_TOKEN env var

# Level 1: Minimum scale (1 replica)
[[warpscript_probes.levels]]
level = 1
scale_up_threshold = 70.0          # If CPU > 70%, scale up
upscale_command = "clever scale --app ${APP_ID} --min-instances 2"

# Level 2: Medium scale (2 replicas)
[[warpscript_probes.levels]]
level = 2
scale_up_threshold = 85.0          # If CPU > 85%, scale up
scale_down_threshold = 50.0        # If CPU < 50%, scale down
upscale_command = "clever scale --app ${APP_ID} --min-instances 3"
downscale_command = "clever scale --app ${APP_ID} --min-instances 1"

# Level 3: Maximum scale (3 replicas)
[[warpscript_probes.levels]]
level = 3
scale_down_threshold = 60.0        # If CPU < 60%, scale down
downscale_command = "clever scale --app ${APP_ID} --min-instances 2"
```

#### Configuration Parameters

**WarpScript Probe Configuration:**
- `name` (required): A descriptive name for the probe
- `warpscript_file` (required): Path to the WarpScript file to execute
- `interval_seconds` (required): Interval between executions (in seconds)
- `command_timeout_seconds` (optional): Timeout for command execution (default: 30)
- `delay_after_scale_seconds` (optional): Delay after scaling up or down (defaults to `interval_seconds`)
- `apps` (optional): List of applications to manage (default: empty list)

**App Configuration:**
- `id` (required): Application identifier
- `warp_token` (optional): Warp read token for this app (uses WARP_TOKEN env var if not specified)

**Level Configuration:**
- `level` (required): Level number (1, 2, 3, etc.) - must be unique and ordered
- `scale_up_threshold` (optional): Value threshold to trigger upscale (move to level+1)
- `scale_down_threshold` (optional): Value threshold to trigger downscale (move to level-1)
- `upscale_command` (optional): Shell command to execute when scaling up from this level
- `downscale_command` (optional): Shell command to execute when scaling down from this level

**Notes:**
- At least one level must be defined
- Level 1 is considered the minimum level (downscale ignored)
- Highest level number is considered the maximum level (upscale ignored)
- At least one threshold per level should be defined (except at boundaries)
- Commands support `${APP_ID}` placeholder for per-app execution

#### WarpScript File Example

```warpscript
// warpscript/cpu_usage.mc2
// ${WARP_TOKEN} is automatically replaced with the env var value
// ${APP_ID} is replaced with the specific app_id for this probe instance

'${WARP_TOKEN}' 'token' STORE
'${APP_ID}' 'app' STORE

// Fetch CPU metric for this specific app (last 5 minutes)
[
  $token
  'os.cpu'
  { 'app_id' $app }  // Filter by this specific app_id
  NOW 5 m -
  NOW
]
FETCH

// Calculate average
[ SWAP bucketizer.mean 0 1 0 ] BUCKETIZE

// Return single value
0 GET VALUES 0 GET 0 GET

// Top of stack must be a number (e.g., 75.5)
```

#### How It Works

1. **Probe Expansion**: If `apps` is specified, each app creates an independent probe instance
   - Example: 3 apps = 3 separate probes with independent states
2. **Execution**: WarpScript file is executed via HTTP POST to `WARP_ENDPOINT`
3. **Token Substitution**: `${WARP_TOKEN}` in the file is replaced with:
   - App's custom `warp_token` if specified
   - Otherwise, `WARP_TOKEN` environment variable
4. **App ID Substitution**: `${APP_ID}` is replaced with the specific app id for this probe instance
   - In WarpScript files
   - In scaling commands
5. **Value Extraction**: Last element from JSON response array is used as the metric value
6. **Scaling Logic**: Based on **current level** and value:
   - If `value > scale_up_threshold` → **UPSCALE** (level + 1)
     - Execute `upscale_command` of current level
     - Increment level (unless already at max)
   - If `value < scale_down_threshold` → **DOWNSCALE** (level - 1)
     - Execute `downscale_command` of current level
     - Decrement level (unless already at min)
7. **Boundary Protection**:
   - At **minimum level**: downscale is ignored
   - At **maximum level**: upscale is ignored
8. **State Persistence**: Current level and last value are persisted per probe instance (Redis or memory)

#### Scaling Strategy Tips

- **Hysteresis**: Set `scale_down_threshold` lower than `scale_up_threshold` to avoid flapping
  - Example: Up at 70%, Down at 50% (20% hysteresis)
- **Cooldown**: Use `delay_after_scale_seconds` to stabilize after scaling actions
- **Gradual**: Define progressive thresholds (level 1→2 at 70%, level 2→3 at 85%)

#### Managing Multiple Applications

You can manage multiple applications with a single configuration using the `apps` array to avoid duplication:

**In configuration:**
```toml
[[warpscript_probes]]
name = "Multi-App Scaler"
warpscript_file = "warpscript/metrics.mc2"

[[warpscript_probes.apps]]
id = "app_frontend"
warp_token = "TOKEN_FRONTEND"  # Optional custom token

[[warpscript_probes.apps]]
id = "app_backend"
warp_token = "TOKEN_BACKEND"

[[warpscript_probes.apps]]
id = "app_worker"
# No warp_token: uses WARP_TOKEN env var

[[warpscript_probes.levels]]
level = 1
scale_up_threshold = 70.0
upscale_command = "clever scale --app ${APP_ID} --min-instances 2"
```

**How it works:**
- **Probe Expansion**: The configuration above creates **3 independent probes**:
  - "Multi-App Scaler - app_frontend" (uses TOKEN_FRONTEND)
  - "Multi-App Scaler - app_backend" (uses TOKEN_BACKEND)
  - "Multi-App Scaler - app_worker" (uses WARP_TOKEN env var)
- **Independent State**: Each probe has its own:
  - Current scaling level
  - Last metric value
  - State persistence
  - Optional custom Warp token
- **Substitution**: In each probe instance:
  - `${APP_ID}` in WarpScript → replaced with specific app id (e.g., `app_frontend`)
  - `${APP_ID}` in commands → replaced with specific app id
  - `${WARP_TOKEN}` in WarpScript → replaced with app's custom token or WARP_TOKEN env var

**Benefits:**
- Avoid configuration duplication for similar apps
- Each app scales independently based on its own metrics
- Each app can use its own Warp token (for multi-tenant scenarios)
- Consistent scaling policies across multiple apps
- Each app can be at a different scaling level

#### Benefits

- Automatic horizontal/vertical scaling based on real metrics
- Gradual scale up/down to prevent over-provisioning
- State persistence ensures correct level after restarts
- Flexible WarpScript queries for any Warp 10 metric
- Works with any platform (Kubernetes, Clever Cloud, etc.)

### Redis Persistence (Optional)

Enable Redis persistence to maintain probe state across restarts.

#### Configuration Options

**Option 1: Using REDIS_URL**
```bash
export REDIS_URL="redis://localhost:6379"
# With password:
export REDIS_URL="redis://:mypassword@localhost:6379"
```

**Option 2: Using separate environment variables**
```bash
export REDIS_HOST="localhost"
export REDIS_PORT="6379"           # Optional, defaults to 6379
export REDIS_PASSWORD="mypassword" # Optional
```

**Priority**: `REDIS_URL` takes precedence over individual variables.

#### Building and Running

```bash
# Build with Redis support
cargo build --release --features redis-persistence

# Run the application
./target/release/poc-sonde
```

#### Behavior

**Without Redis configuration**: The application uses in-memory persistence (state is lost on restart).

**With Redis configuration**: Probe states are persisted to Redis:
- Last execution timestamp
- Success/failure status
- Next scheduled execution time
- On restart, probes resume from their saved state

**Benefits:**
- No duplicate checks immediately after restart
- Maintains retry schedules across deployments
- Enables horizontal scaling (future feature)

#### Examples

```bash
# Development: Local Redis without password
export REDIS_HOST="localhost"
./target/release/poc-sonde

# Production: Redis with authentication
export REDIS_HOST="redis.example.com"
export REDIS_PORT="6379"
export REDIS_PASSWORD="prod-secret-password"
./target/release/poc-sonde

# Docker/Kubernetes: Using REDIS_URL
export REDIS_URL="redis://:${REDIS_PASS}@redis-service:6379"
./target/release/poc-sonde

# Cloud Redis (e.g., AWS ElastiCache, Google Cloud Memorystore)
export REDIS_URL="redis://my-redis.abc123.cache.amazonaws.com:6379"
./target/release/poc-sonde
```

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
[[healthcheck_probes]]
name = "Production API"
url = "https://api.myapp.com/health"
interval_seconds = 30
on_failure_command = "curl -X POST https://hooks.slack.com/... -d '{\"text\":\"API is down!\"}'"

[healthcheck_probes.checks]
expected_status = 200
expected_body_contains = "healthy"
```

### Example 2: Monitor Service with Auto-Restart

```toml
[[healthcheck_probes]]
name = "Backend Service"
url = "http://localhost:8080/status"
interval_seconds = 60
on_failure_command = "systemctl restart myservice"

[healthcheck_probes.checks]
expected_status = 200
```

### Example 3: Validate API Response Format

```toml
[[healthcheck_probes]]
name = "User API"
url = "https://api.myapp.com/users/1"
interval_seconds = 120

[healthcheck_probes.checks]
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
- Ensure your `config.toml` has at least one `[[healthcheck_probes]]` section

If you see "Probe has no checks configured":
- Add at least one check type to `[healthcheck_probes.checks]`

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

## Command Execution

All commands (`on_failure_command`, `upscale_command`, `downscale_command`) are executed via `sh -c`, which means:
- Shell operators are supported: `&&`, `||`, `;`, pipes (`|`)
- `${APP_ID}` is substituted with the app identifier before execution

```toml
# Simple command
on_failure_command = "clever restart --app ${APP_ID}"

# Chained commands with &&
on_failure_command = "clever scale --app ${APP_ID} --flavor S && clever restart --app ${APP_ID}"

# With pipe
on_failure_command = "echo 'Alert' | mail -s 'App down' ops@example.com"
```

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

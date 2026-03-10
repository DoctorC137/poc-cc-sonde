# cc-sonde — HTTP Monitoring & Auto-Scaling Application

A Rust application that periodically checks HTTP endpoints and executes shell commands on failure, and optionally drives level-based auto-scaling from Warp 10 metrics.

---

## Table of Contents

- [Features](#features)
- [Installation](#installation)
- [Usage](#usage)
- [Configuration](#configuration)
  - [Healthcheck Probes](#healthcheck-probes)
  - [WarpScript Probes](#warpscript-probes-auto-scaling)
- [Environment Variables](#environment-variables)
- [Command Execution](#command-execution)
- [State Persistence](#state-persistence)
- [Graceful Shutdown](#graceful-shutdown)
- [Liveness Endpoint](#liveness-endpoint)
- [Logging](#logging)
- [Testing](#testing)
- [Troubleshooting](#troubleshooting)
- [Security Notes](#security-notes)

---

## Features

- **Periodic HTTP Monitoring** — multiple probes, each with its own interval
- **Flexible Checks** — any combination of:
  - HTTP status code
  - Response body substring
  - Response body regex (pre-compiled at startup)
  - HTTP response header key/value
- **Failure Actions** — execute a shell command when checks fail repeatedly
- **Retry Threshold** — tolerate N consecutive failures before triggering the command
- **Configurable Delays** — independent wait times after success, failure, command success, command failure
- **Multiple Apps per Probe** — expand one probe definition into N independent instances via `apps`; `${APP_ID}` is substituted in commands and WarpScript files
- **WarpScript Probes** — query a Warp 10 platform, compare the numeric result to configurable thresholds, and fire scale-up / scale-down shell commands
  - Multi-level scaling (1 … N levels, contiguous, no gaps)
  - Per-level thresholds and commands
  - `levels = [N, M, …]` shorthand to share identical config across multiple levels
  - WarpScript file read once at startup then cached; retry loop if the file is not yet available at launch
  - `${WARP_TOKEN}` and `${APP_ID}` substitution inside the WarpScript file
  - Per-app optional `warp_token`; falls back to the `WARP_TOKEN` environment variable
  - `WARP_ENDPOINT` and `WARP_TOKEN` resolved once per probe task before the polling loop
- **Process Group Cleanup** — on timeout, the entire process group is killed (Linux/macOS), including pipelines and sub-shells
- **Concurrent Execution** — each probe instance runs as an independent async task
- **Graceful Shutdown** — handles `SIGTERM` (containers, systemd) and `SIGINT` (Ctrl+C)
- **Liveness Endpoint** — optional HTTP server for meta-monitoring
- **State Persistence** — in-memory (default) or Redis; survives restarts
- **Structured Logging** — `tracing`-based, configurable via `RUST_LOG`
- **TOML Configuration** — human-readable, validated at startup

---

## Installation

**Prerequisites:** Rust 1.70+ and Cargo.

```bash
# Default build (in-memory persistence)
cargo build --release

# With Redis persistence support
cargo build --release --features redis-persistence

# Binary location
./target/release/cc-sonde
```

---

## Usage

```bash
# Read config.toml in the current directory
./target/release/cc-sonde

# Custom config file
./target/release/cc-sonde --config /path/to/config.toml

# Enable liveness endpoint (default port 8080)
./target/release/cc-sonde --healthcheck

# Custom port
./target/release/cc-sonde --healthcheck --healthcheck-port 9090
```

### Command-Line Options

```
Usage: cc-sonde [OPTIONS]

Options:
      --config <CONFIG>
          Configuration file path [default: config.toml]
      --healthcheck
          Enable health check HTTP server
      --healthcheck-port <HEALTHCHECK_PORT>
          Port for health check server (requires --healthcheck) [default: 8080]
  -h, --help
          Print help
  -V, --version
          Print version
```

---

## Configuration

The configuration file is TOML. Both `healthcheck_probes` and `warpscript_probes` are optional, but **at least one probe of either type must be present**.

A minimal valid config with a single healthcheck probe:

```toml
[[healthcheck_probes]]
name = "My API"
url = "https://api.example.com/health"
interval_seconds = 60

[healthcheck_probes.checks]
expected_status = 200
```

A minimal valid config with only WarpScript probes (no healthcheck probes):

```toml
[[warpscript_probes]]
name = "CPU Scaler"
warpscript_file = "cpu.mc2"
interval_seconds = 60

[[warpscript_probes.levels]]
level = 1
scale_up_threshold = 70.0
upscale_command = "kubectl scale deployment myapp --replicas=2"

[[warpscript_probes.levels]]
level = 2
scale_down_threshold = 50.0
downscale_command = "kubectl scale deployment myapp --replicas=1"
```

See `config.example.toml` and `config-warpscript-example.toml` for complete annotated examples.

---

### Healthcheck Probes

```toml
[[healthcheck_probes]]
name = "API Health Check"
url = "https://api.example.com/health"
interval_seconds = 60
on_failure_command = "systemctl restart my-service"
command_timeout_seconds = 30
delay_after_success_seconds = 300
delay_after_failure_seconds = 30
delay_after_command_success_seconds = 120
delay_after_command_failure_seconds = 30
failure_retries_before_command = 2

[healthcheck_probes.checks]
expected_status = 200
expected_body_contains = "\"status\":\"ok\""
expected_body_regex = "version\":\\s*\"\\d+\\.\\d+"

[healthcheck_probes.checks.expected_header]
"X-Service" = "my-api"
```

#### Probe Parameters

| Key | Required | Default | Description |
|-----|----------|---------|-------------|
| `name` | yes | — | Unique descriptive name |
| `url` | yes* | — | HTTP endpoint to monitor. Required if `apps` is not set. Mutually exclusive with `apps`. |
| `apps` | yes* | — | List of apps to monitor. Required if `url` is not set. Mutually exclusive with `url`. |
| `interval_seconds` | yes | — | Default interval between executions. Must be > 0. |
| `on_failure_command` | no | — | Shell command to execute when the failure threshold is reached |
| `command_timeout_seconds` | no | `30` | Maximum execution time for the failure command (seconds) |
| `delay_after_success_seconds` | no | `interval_seconds` | Wait time after a successful check |
| `delay_after_failure_seconds` | no | `interval_seconds` | Wait time after a failed check (below threshold) |
| `delay_after_command_success_seconds` | no | `delay_after_failure_seconds` | Wait time after the failure command succeeds |
| `delay_after_command_failure_seconds` | no | `delay_after_failure_seconds` | Wait time after the failure command fails |
| `failure_retries_before_command` | no | `0` | Number of consecutive failures tolerated before executing the command |

**Delay resolution order:**

| Situation | Delay used |
|-----------|-----------|
| Check succeeded | `delay_after_success_seconds` → `interval_seconds` |
| Check failed (below threshold) | `delay_after_failure_seconds` → `interval_seconds` |
| Failure command succeeded | `delay_after_command_success_seconds` → `delay_after_failure_seconds` → `interval_seconds` |
| Failure command failed | `delay_after_command_failure_seconds` → `delay_after_failure_seconds` → `interval_seconds` |

#### `failure_retries_before_command` Semantics

The counter increments on each consecutive failure and resets to zero on any success.

| Value | Command triggered on |
|-------|----------------------|
| `0` (default) | 1st consecutive failure |
| `1` | 2nd consecutive failure |
| `N` | (N+1)th consecutive failure |

The command is triggered exactly once per threshold crossing — on the cycle where the counter first reaches the threshold — and then again if the counter keeps growing (i.e., every cycle once the threshold is met and the probe keeps failing).

#### Check Types

At least one check must be configured per probe. All configured checks must pass.

| Key | Description |
|-----|-------------|
| `expected_status` | Expected HTTP status code (integer) |
| `expected_body_contains` | Substring that must be present in the response body |
| `expected_body_regex` | Regular expression that must match the response body. Compiled once at startup. |
| `expected_header` | Inline TOML table of `"Header-Name" = "expected-value"` pairs, all of which must be present |

#### Multiple Apps (Healthcheck)

Use `apps` to apply one probe template to multiple endpoints. Each app creates an independent probe instance named `"<probe name> - <app id>"`, with its own failure counter, delay state, and persistence key.

```toml
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

App fields:

| Key | Required | Description |
|-----|----------|-------------|
| `id` | yes | Identifier substituted as `${APP_ID}` in `on_failure_command`. Only alphanumeric, `-`, `_`, `.` allowed. |
| `url` | yes | Health check URL for this app |

---

### WarpScript Probes (Auto-Scaling)

Execute a WarpScript query against a Warp 10 platform and automatically scale applications based on the returned numeric value.

#### Required Environment Variables

```bash
# Required when any warpscript_probes are configured
export WARP_ENDPOINT="https://warp.example.com/api/v0/exec"

# Optional: fallback token for apps without a warp_token
export WARP_TOKEN="your-read-token"
```

`WARP_ENDPOINT` is validated at startup and logged only at `debug` level. `WARP_TOKEN` and per-app `warp_token` values are never logged. Both are resolved once per probe task before the polling loop starts.

#### Configuration Example

```toml
[[warpscript_probes]]
name = "CPU Auto-Scaler"
warpscript_file = "warpscript/cpu_usage.mc2"
interval_seconds = 60
command_timeout_seconds = 45
delay_after_scale_seconds = 120

[[warpscript_probes.apps]]
id = "app_frontend"
warp_token = "READ_TOKEN_FRONTEND"   # Overrides WARP_TOKEN env var for this app

[[warpscript_probes.apps]]
id = "app_backend"
# No warp_token: uses WARP_TOKEN env var

[[warpscript_probes.levels]]
level = 1
scale_up_threshold = 70.0
upscale_command = "clever scale --app ${APP_ID} --min-instances 2"

[[warpscript_probes.levels]]
level = 2
scale_up_threshold = 85.0
scale_down_threshold = 50.0
upscale_command = "clever scale --app ${APP_ID} --min-instances 3"
downscale_command = "clever scale --app ${APP_ID} --min-instances 1"

[[warpscript_probes.levels]]
level = 3
scale_down_threshold = 60.0
downscale_command = "clever scale --app ${APP_ID} --min-instances 2"
```

#### Probe Parameters

| Key | Required | Default | Description |
|-----|----------|---------|-------------|
| `name` | yes | — | Unique descriptive name |
| `warpscript_file` | yes | — | Path to the `.mc2` file. Read once at startup (with retry); restart required to pick up changes. |
| `interval_seconds` | yes | — | Default interval between executions |
| `command_timeout_seconds` | no | `30` | Maximum execution time for scaling commands (seconds) |
| `delay_after_scale_seconds` | no | `interval_seconds` | Wait time after any scaling action (up or down) |
| `apps` | no | `[]` | List of apps to manage; each creates an independent probe instance |

#### App Parameters (WarpScript)

| Key | Required | Description |
|-----|----------|-------------|
| `id` | yes | Identifier substituted as `${APP_ID}` in the script and commands. Only alphanumeric, `-`, `_`, `.` allowed. |
| `warp_token` | no | Per-app read token. Overrides the `WARP_TOKEN` env var. If neither is set, the cycle is skipped with an error log. |

#### Level Parameters

At least one level must be defined. Level numbers must be unique and contiguous (no gaps).

| Key | Required | Description |
|-----|----------|-------------|
| `level` | yes* | Single level number — use `level = N` |
| `levels` | yes* | Multiple level numbers — use `levels = [N, M, …]` for levels sharing identical config |
| `scale_up_threshold` | no | If the metric value exceeds this, scale up. Ignored at the maximum level. |
| `scale_down_threshold` | no | If the metric value drops below this, scale down. Ignored at the minimum level. |
| `upscale_command` | no | Shell command executed when scaling up from this level |
| `downscale_command` | no | Shell command executed when scaling down from this level |

*`level` and `levels` are mutually exclusive; exactly one must be present per entry.

#### Sharing Config Across Multiple Levels

When consecutive levels share identical thresholds and commands, use `levels = [N, M, …]`:

```toml
# Before (verbose — two identical blocks)
[[warpscript_probes.levels]]
level = 2
scale_down_threshold = 45.0
downscale_command = "clever scale --app ${APP_ID} --flavor XS"

[[warpscript_probes.levels]]
level = 3
scale_down_threshold = 45.0
downscale_command = "clever scale --app ${APP_ID} --flavor XS"

# After (compact)
[[warpscript_probes.levels]]
levels = [2, 3]
scale_down_threshold = 45.0
downscale_command = "clever scale --app ${APP_ID} --flavor XS"
```

Level entries are sorted by level number after deserialization, regardless of declaration order.

#### How Scaling Works

1. `WARP_ENDPOINT`, `WARP_TOKEN`, and the WarpScript file are resolved once per probe task before the polling loop.
   - If the script file is not readable at startup (e.g., not yet mounted), the probe retries every `interval_seconds` until it succeeds. The task does not die.
2. At each interval, `${WARP_TOKEN}` and `${APP_ID}` are substituted into the cached script, and it is sent via HTTP POST to `WARP_ENDPOINT`.
3. The last element of the JSON response array is used as the metric value (must be a number).
4. The value is compared against the **current level's** thresholds:
   - `value > scale_up_threshold` → execute `upscale_command`, increment level
   - `value < scale_down_threshold` → execute `downscale_command`, decrement level
   - Otherwise → no action, wait `interval_seconds`
5. Boundaries: upscale is ignored at max level; downscale is ignored at min level.
6. After any scaling action, wait `delay_after_scale_seconds` before the next check.
7. On WarpScript execution error, the current level is kept and the probe retries after `interval_seconds`.
8. The current level is persisted and restored on restart. If the persisted level is no longer valid in the current config (e.g., max level was reduced), it is clamped to the minimum and a `WARN` is logged.

#### Token Resolution

For each polling cycle:

1. If the current app has `warp_token` → use it.
2. Else if `WARP_TOKEN` env var is set → use it.
3. Else → log an error and skip the cycle; retry at the next interval.

#### WarpScript File Format

The script is a standard WarpScript (`.mc2`) file. Two substitutions are performed before each execution:

| Placeholder | Replaced with |
|-------------|---------------|
| `${WARP_TOKEN}` | The effective token for this app (per-app or env fallback) |
| `${APP_ID}` | The app identifier |

The script must leave exactly one numeric value on the stack; the last element of the returned JSON array is used.

```warpscript
// warpscript/cpu_usage.mc2
'${WARP_TOKEN}' 'token' STORE
'${APP_ID}'     'app'   STORE

[
  $token
  'os.cpu'
  { 'app_id' $app }
  NOW 5 m -
  NOW
] FETCH

[ SWAP bucketizer.mean 0 1 0 ] BUCKETIZE

// Return a single numeric value (e.g., 75.5)
0 GET VALUES 0 GET 0 GET
```

#### Scaling Strategy Tips

- **Hysteresis**: keep `scale_down_threshold` meaningfully below `scale_up_threshold` to avoid flapping (e.g., up at 70%, down at 50%).
- **Cooldown**: use `delay_after_scale_seconds` to let the system stabilize before re-evaluating.
- **Progressive thresholds**: use higher up-thresholds at higher levels (e.g., 70% → level 2, 85% → level 3).
- **Script changes**: the WarpScript file is read once at startup. Restart the application to pick up edits.

---

## Environment Variables

| Variable | Used by | Required | Description |
|----------|---------|----------|-------------|
| `WARP_ENDPOINT` | WarpScript probes | yes (if any WarpScript probe) | URL of the Warp 10 exec API. Validated at startup; never logged at `info`. |
| `WARP_TOKEN` | WarpScript probes | no | Fallback read token for apps without a per-app `warp_token`. Never logged. |
| `REDIS_URL` | Persistence | no | Full Redis connection URL (takes precedence over individual vars). |
| `REDIS_HOST` | Persistence | no | Redis hostname (used only if `REDIS_URL` is not set). |
| `REDIS_PORT` | Persistence | no | Redis port (default: `6379`). |
| `REDIS_PASSWORD` | Persistence | no | Redis password (masked in logs). |
| `RUST_LOG` | Logging | no | Log level filter (default: `info`). See [Logging](#logging). |

---

## Command Execution

All commands (`on_failure_command`, `upscale_command`, `downscale_command`) are run via `sh -c`, so shell operators work:

```toml
on_failure_command = "clever scale --app ${APP_ID} --flavor S && clever restart --app ${APP_ID}"
on_failure_command = "echo 'Alert' | mail -s 'App down' ops@example.com"
```

`${APP_ID}` is substituted before the command is passed to the shell.

### Timeout and Process Group Cleanup

If a command exceeds `command_timeout_seconds`, the **entire process group** is killed (`SIGKILL` on the group) on Linux/macOS. This ensures that pipelines, sub-shells, and grandchildren are all terminated, not just the top-level `sh` process.

On non-Unix platforms, only the direct child process is killed via `kill_on_drop`.

### Logging Behaviour

- Command strings are logged only at `debug` level (they may contain tokens or passwords).
- On non-zero exit, `stderr` is logged at `warn`. `stdout` is never logged (it may contain sensitive output).
- Exit code is always logged.

---

## State Persistence

Each probe instance saves its state after every execution. The state includes:

| Field | Healthcheck | WarpScript |
|-------|------------|------------|
| Last execution timestamp | ✓ | ✓ |
| Next scheduled execution timestamp | ✓ | ✓ |
| Last check success | ✓ | — |
| Consecutive failure counter | ✓ | — |
| Current scaling level | — | ✓ |
| Last metric value | — | ✓ |

On startup, if a saved state is found and `next_check_timestamp` is in the future, the probe waits out the remaining delay before its first execution. This prevents duplicate checks immediately after a restart.

### In-Memory Backend (Default)

No external dependencies. State is lost when the process exits. Suitable for single-instance deployments where missing one cycle on restart is acceptable.

### Redis Backend (Optional)

Build with `--features redis-persistence`.

```bash
# Option 1: full URL (takes precedence)
export REDIS_URL="redis://:mypassword@localhost:6379"

# Option 2: individual variables
export REDIS_HOST="localhost"
export REDIS_PORT="6379"           # optional, default 6379
export REDIS_PASSWORD="mypassword" # optional
```

The Redis URL (including any embedded password) is **never written to logs**. Only a masked form is logged at startup.

Redis keys used:
- `poc-sonde:probe:<probe-name>` — healthcheck probe state
- `poc-sonde:warpscript:<probe-name>` — WarpScript probe state

If the Redis connection fails at startup, the application falls back to in-memory persistence with an `error` log entry.

### Level Validation on Restart

When a WarpScript probe restores its level from state and that level is no longer present in the current config (e.g., the maximum level was reduced), the level is automatically clamped to the configured minimum and a `warn` log entry is emitted. No manual cleanup is required.

---

## Graceful Shutdown

The application handles:

- **`SIGTERM`** — sent by container orchestrators (`docker stop`, Kubernetes) and systemd
- **`SIGINT`** — Ctrl+C in a terminal

On either signal, all probe tasks are aborted and the process exits cleanly.

---

## Liveness Endpoint

When `--healthcheck` is passed, an HTTP server starts on `0.0.0.0:<port>` (default `8080`).

Every request receives:
- **Status**: `200 OK`
- **Body**: `Probe is running`

```bash
curl http://localhost:8080
# Probe is running
```

This is useful for meta-monitoring the monitoring application itself (e.g., as a Kubernetes liveness probe).

---

## Logging

Control verbosity with `RUST_LOG`:

```bash
RUST_LOG=info  ./target/release/cc-sonde   # default
RUST_LOG=debug ./target/release/cc-sonde   # detailed, includes WARP_ENDPOINT and command strings
RUST_LOG=cc_sonde::warpscript_scheduler=debug ./target/release/cc-sonde  # module-level
```

Log format:

```
2024-01-15T10:30:45.123Z  INFO cc_sonde: Starting HTTP monitoring application
2024-01-15T10:30:45.234Z  INFO cc_sonde: Loading configuration config_path="config.toml"
2024-01-15T10:30:45.345Z  INFO cc_sonde::healthcheck_probe: All checks passed probe_name="API Health Check" duration_ms=111
```

**What is logged at each level:**

| Information | Level | Notes |
|-------------|-------|-------|
| Application startup, configuration summary | `info` | |
| Probe results (success / failure / scaling decisions) | `info` | |
| Redis URL (masked) | `info` | Password replaced with `****` |
| Command exit codes | `info` / `warn` | `info` on success, `warn` on non-zero |
| Command stderr (on non-zero exit) | `warn` | |
| `WARP_ENDPOINT` value | `debug` | Not emitted at `info` |
| Command strings | `debug` | May contain tokens or passwords |
| Per-execution scheduling details | `debug` | |
| State load/save operations | `debug` | |

---

## Testing

```bash
cargo test

# With stdout
cargo test -- --nocapture

# Single test
cargo test test_warpscript_levels_plural_expands

# With Redis feature
cargo test --features redis-persistence
```

```bash
cargo clippy -- -D warnings
```

---

## Troubleshooting

| Symptom | Likely cause |
|---------|-------------|
| `Configuration must contain at least one probe (healthcheck_probes or warpscript_probes)` | Both `healthcheck_probes` and `warpscript_probes` are empty or absent |
| `Probe '…' has no checks configured` | No key defined under `[healthcheck_probes.checks]` |
| `Probe '…' must have either 'url' or 'apps' configured` | Neither `url` nor `apps` specified for a healthcheck probe |
| `Probe '…' cannot have both 'url' and 'apps' configured` | Both `url` and `apps` are set on the same probe |
| `Probe '…': app id '…' contains invalid characters` | `id` contains characters other than alphanumeric, `-`, `_`, `.` |
| `a scaling level entry must specify either level = N or levels = [N, …]` | WarpScript level block is missing both `level` and `levels` |
| `a scaling level entry cannot specify both level and levels` | Both `level` and `levels` are present in the same level entry |
| `WarpScript probe '…' has duplicate level number N` | Same level defined twice (including via `levels = [N, N]`) |
| `WarpScript probe '…' levels must be contiguous` | Level numbers have gaps (e.g., `1` and `3` without `2`) |
| `WARP_ENDPOINT environment variable not set` | Required env var missing when WarpScript probes are configured |
| `No Warp token available …` | App has no `warp_token` and `WARP_TOKEN` env var is not set; that cycle is skipped |
| `Failed to read WarpScript file, will retry` | File not found or permission error; the probe retries every `interval_seconds` |
| WarpScript changes not reflected | The script is read once at startup; restart the application after editing the `.mc2` file |
| Scaling level reset to minimum after restart | The previously persisted level is not in the current config; expected behaviour after reducing the number of levels |
| Command times out but child processes keep running | Should not happen on Linux/macOS — the whole process group is killed. On other platforms, only `sh` is killed. |
| `Redis URL provided but redis-persistence feature is not enabled` | Rebuild with `cargo build --features redis-persistence` |
| Redis connection fails at startup | Application falls back to in-memory persistence; check `REDIS_URL` / `REDIS_HOST` and network reachability |

---

## Security Notes

- **Privileges**: commands execute with the same OS privileges as the application. Consider running as a dedicated low-privilege user.
- **`${APP_ID}` validation**: app IDs are validated at startup — only alphanumeric characters, `-`, `_`, and `.` are allowed. This prevents shell injection via the `${APP_ID}` placeholder.
- **Shell execution surface**: all commands are run via `sh -c` to support pipes and shell operators. The **content** of `on_failure_command`, `upscale_command`, and `downscale_command` is not otherwise validated and is the responsibility of the administrator who writes the configuration file. Only trusted administrators should have write access to the TOML file.
- **Tokens in configuration**: prefer the `WARP_TOKEN` environment variable over inline `warp_token` values in the TOML file. Environment variables are not stored on disk and are less likely to be accidentally committed to version control or exposed in file system backups. Use inline `warp_token` only for local development or when environment variable injection is not available.
- **Command logging**: command strings are only logged at `debug` level, as they may contain tokens or passwords. Run with `RUST_LOG=info` (the default) in production to avoid exposing them.
- **`WARP_ENDPOINT` logging**: logged only at `debug` level, since query parameters or credentials may be embedded in the URL.
- **Redis passwords**: masked before being logged; the raw URL is never written to any log output.
- **stdout**: command stdout is never logged (it may contain sensitive data). Only stderr is logged, and only on non-zero exit.

---

## License

MIT — use at your own discretion.

# cc-sonde — HTTP Monitoring & Auto-Scaling Application

A Rust application that periodically checks HTTP endpoints and executes shell commands on failure, and optionally drives level-based auto-scaling from Warp 10 metrics.

---

## Table of Contents

- [Features](#features)
- [Installation](#installation)
- [Usage](#usage)
- [Dry Run Mode](#dry-run-mode)
- [Multi-Instance Mode](#multi-instance-mode)
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
  - **Multi-metric support** — one probe can query several WarpScript files simultaneously; scale UP if ANY metric exceeds its threshold, scale DOWN if ALL metrics are below their thresholds
  - **Computed levels** — levels are derived automatically from a `flavors` list and an `instances` range; no need to enumerate level entries manually
  - **Flavor + instance scaling** — each level maps to a (flavor, instances) pair; `${flavor}` and `${instances}` are substituted in commands
  - WarpScript files read once at startup then cached; retry loop if a file is not yet available at launch
  - `${WARP_TOKEN}` and `${APP_ID}` substitution inside WarpScript files
  - Per-app optional `warp_token`; falls back to the `WARP_TOKEN` environment variable
  - `WARP_ENDPOINT` and `WARP_TOKEN` resolved once per probe task before the polling loop
- **Process Group Cleanup** — on timeout, the entire process group is killed (Linux/macOS), including pipelines and sub-shells
- **Concurrent Execution** — each probe instance runs as an independent async task
- **Graceful Shutdown** — handles `SIGTERM` (containers, systemd) and `SIGINT` (Ctrl+C)
- **Liveness Endpoint** — optional HTTP server for meta-monitoring
- **State Persistence** — in-memory (default) or Redis; survives restarts
- **Dry Run Mode** — `--dry-run` flag executes all probes and persists state, but skips all remediation commands; safe for config validation and threshold tuning
- **Multi-Instance Mode** — `--multi-instance` (or `MULTI_INSTANCE=true`) enforces that Redis is available; the process exits fatally on connection failure instead of silently falling back to in-memory, preventing split-brain across replicas; after acquiring the distributed lock, each instance refreshes its state from Redis before making scaling decisions, guaranteeing convergence
- **Bounded Response Body Reads** — HTTP responses are read chunk-by-chunk and capped at 1 MiB; a gigantic response body never causes unbounded memory consumption
- **Credential Sanitisation in Logs** — all URLs (Redis, HTTP endpoints) are sanitised before logging; `://user:PASSWORD@host` credentials are masked as `****` regardless of scheme
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

# Dry run: execute all probes but skip remediation commands
./target/release/cc-sonde --dry-run

# Multi-instance mode: require Redis; exit fatally if Redis is unavailable
./target/release/cc-sonde --multi-instance
MULTI_INSTANCE=true ./target/release/cc-sonde
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
      --dry-run
          Dry run mode: probe checks are executed but remediation commands are not
      --multi-instance
          Multi-instance mode: Redis is required for distributed locking.
          Can also be set via the MULTI_INSTANCE environment variable.
  -h, --help
          Print help
  -V, --version
          Print version
```

---

## Dry Run Mode

The `--dry-run` flag lets you run cc-sonde with full probe execution — HTTP checks, WarpScript queries, state persistence, failure counters — while **skipping all external remediation commands**.

```bash
./target/release/cc-sonde --config config.toml --dry-run
```

At startup, a `WARN` entry is emitted to make the mode visible:

```
WARN cc_sonde: Dry run mode enabled: remediation commands will not be executed
```

### What runs vs. what is skipped

| Action | Dry run |
|--------|---------|
| HTTP health check (GET to monitored service) | Executed normally |
| WarpScript query (POST to Warp 10 endpoint) | Executed normally |
| `on_failure_command` | **Skipped** — logged instead |
| `upscale_command` / `downscale_command` | **Skipped** — logged instead |
| Internal state (failure counter, scaling level, timestamps) | Updated normally |
| Persistence (in-memory / Redis) | Saved normally |

When a command would have been executed, a `WARN` log is emitted instead:

```
WARN probe_name="my-api" command="systemctl restart my-service" DRY RUN: skipping failure command
WARN probe_name="cpu-scaler" command="clever scale --app app1 --flavor M --instances 2" from_level=1 to_level=2 DRY RUN: skipping upscale command
```

### Behaviour details

- When a failure command is skipped, the scheduler treats it as if the command had **succeeded**: the next delay will be `delay_after_command_success_seconds`. This simulates the nominal recovery path.
- When a scaling command is skipped, `current_level` is still updated. The probe tracks the level it *would* be at, so threshold logic remains coherent across cycles.
- Removing `--dry-run` resumes normal operation with no further changes required.

### Typical use cases

- Validate a new configuration file against a real environment without side effects.
- Tune scaling thresholds by observing which levels would be triggered.
- Test the monitoring pipeline in a staging environment that shares infrastructure with production.

---

## Multi-Instance Mode

When multiple replicas share a Redis backend, the distributed lock mechanism guarantees that **exactly one instance** executes a given probe during each cycle. If one instance holds the lock, others skip that cycle and wait for the next interval.

By default, if the Redis connection fails at startup, the application falls back to in-memory persistence silently. In a multi-instance deployment this is dangerous: each replica would run independently, producing duplicate remediation commands and inconsistent state — a **split-brain** scenario.

Use `--multi-instance` (or `MULTI_INSTANCE=true`) to make the process exit fatally instead of silently degrading:

```bash
# Binary flag
./target/release/cc-sonde --multi-instance

# Environment variable (useful in container deployments)
MULTI_INSTANCE=true ./target/release/cc-sonde
```

At startup a `WARN` entry is emitted:

```
WARN cc_sonde: Multi-instance mode enabled: Redis is required for distributed locking
```

If Redis is unreachable, the process prints an error message and exits with code 1:

```
Fatal: Redis connection failed in multi-instance mode: …
```

Additionally, if Redis is configured but `--multi-instance` is not set, a `WARN` is logged to encourage the operator to opt in:

```
WARN Redis is configured but --multi-instance is not set. If running multiple replicas, add --multi-instance …
```

### State synchronisation across instances

After each successful lock acquisition, the winner **re-reads its state from Redis** before making any scaling decision. This prevents the stale-read problem that would arise if instance A scaled to level 3 and saved it while instance B still held level 2 in local memory. Observable in logs:

```
INFO probe_name="cpu-scaler" stale=2 fresh=3 State refreshed from Redis after lock acquisition
```

The refresh is best-effort: if Redis is temporarily unreachable at that precise moment, the instance continues with its local state (degraded but non-blocking).

### Requirements and constraints

| Condition | `--multi-instance` absent | `--multi-instance` present |
|-----------|--------------------------|---------------------------|
| No Redis config | In-memory (normal) | In-memory (normal) |
| Redis config, connection OK | Redis backend | Redis backend |
| Redis config, connection fails | Falls back to in-memory (error log) | **Fatal exit (code 1)** |
| Redis config, feature not compiled in | Warning, in-memory | **Fatal exit (code 1)** |

Single-instance deployments do not need this flag. Use it in any Kubernetes / Docker Swarm / ECS configuration where several pods or containers share the same Redis instance.

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

A minimal valid config with only a WarpScript probe:

```toml
[[warpscript_probes]]
name = "CPU Scaler"
warpscript_file = {cpu = "warpscript/cpu.mc2"}
interval_seconds = 60

[warpscript_probes.scaling]
instances  = {min = 1, max = 2}
flavors    = ["S", "M"]
scale_up_threshold   = {cpu = 70.0}
scale_down_threshold = {cpu = 40.0}
upscale_command   = "kubectl scale deployment myapp --replicas=${instances}"
downscale_command = "kubectl scale deployment myapp --replicas=${instances}"
```

See `config.example.toml` for annotated healthcheck examples.

---

### Healthcheck Probes

```toml
[[healthcheck_probes]]
name = "API Health Check"
url = "https://api.example.com/health"
interval_seconds = 60
on_failure_command = "systemctl restart my-service"
request_timeout_seconds = 10
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
| `request_timeout_seconds` | no | `30` | HTTP request timeout for this probe (seconds). The probe fails with a `RequestError` if the server does not respond within this duration. |
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

Execute WarpScript queries against a Warp 10 platform and automatically scale applications based on the returned numeric values.

#### Required Environment Variables

```bash
# Required when any warpscript_probes are configured
export WARP_ENDPOINT="https://warp.example.com/api/v0/exec"

# Optional: fallback token for apps without a warp_token
export WARP_TOKEN="your-read-token"
```

`WARP_ENDPOINT` is validated at startup and logged only at `debug` level. `WARP_TOKEN` and per-app `warp_token` values are never logged. Both are resolved once per probe task before the polling loop starts.

#### Configuration Overview

A WarpScript probe is structured around three keys:

1. **`warpscript_file`** — inline TOML table mapping metric names to `.mc2` file paths
2. **`[warpscript_probes.scaling]`** — block defining how levels are computed and which commands to run
3. **`[[warpscript_probes.apps]]`** — optional list of app instances sharing this probe's configuration

```toml
[[warpscript_probes]]
name = "CPU Auto-Scaler"

# Inline table: metric_name = "path/to/file.mc2"
# Single metric:
warpscript_file = {cpu = "warpscript/cpu.mc2"}
# Multiple metrics (scale up on ANY, scale down on ALL):
# warpscript_file = {cpu = "warpscript/cpu.mc2", memory = "warpscript/mem.mc2"}

interval_seconds = 60
command_timeout_seconds = 45
request_timeout_seconds = 30
delay_after_scale_seconds = 120

# Optional failure handling
on_failure_command = "curl -s https://ops.example.com/alert?probe=${APP_ID}"
failure_retries_before_command = 2
delay_after_command_success_seconds = 300
delay_after_command_failure_seconds = 60

[[warpscript_probes.apps]]
id = "app_frontend"
warp_token = "READ_TOKEN_FRONTEND"   # Overrides WARP_TOKEN env var for this app

[[warpscript_probes.apps]]
id = "app_backend"
# No warp_token: uses WARP_TOKEN env var

[warpscript_probes.scaling]
# Instance range for the last flavor
instances = {min = 1, max = 3}

# Ordered list of flavors (smallest to largest)
flavors = ["S", "M", "L"]

# Scale UP if ANY of these values exceed their threshold
scale_up_threshold = {cpu = 70.0}

# Scale DOWN if ALL of these values are below their threshold
scale_down_threshold = {cpu = 40.0}

# ${flavor} and ${instances} are substituted from the computed level
# ${APP_ID} is substituted if apps are configured
upscale_command   = "clever scale --app ${APP_ID} --flavor ${flavor} --instances ${instances}"
downscale_command = "clever scale --app ${APP_ID} --flavor ${flavor} --instances ${instances}"
```

#### Level Computation

Levels are derived automatically from `flavors` and `instances`. There is no explicit `levels` array to maintain.

**Algorithm:**
- **Phase 1** — each flavor except the last gets one level at `instances.min`
- **Phase 2** — the last flavor gets one level per instance count from `instances.min` to `instances.max` (inclusive)

**Examples:**

| `flavors` | `instances` | Computed levels |
|-----------|-------------|-----------------|
| `["S","M","L"]` | `min=1, max=3` | 5 levels: (S,1) → (M,1) → (L,1) → (L,2) → (L,3) |
| `["S"]` | `min=1, max=3` | 3 levels: (S,1) → (S,2) → (S,3) |
| `["S","M"]` | `min=1` (no max) | 2 levels: (S,1) → (M,1) — no instance scaling |

At level N, the `${flavor}` and `${instances}` placeholders in commands resolve to the corresponding computed values.

#### Probe Parameters

| Key | Required | Default | Description |
|-----|----------|---------|-------------|
| `name` | yes | — | Unique descriptive name |
| `warpscript_file` | yes | — | Inline TOML table mapping metric names to `.mc2` file paths. Must define at least one metric. |
| `interval_seconds` | yes | — | Default interval between executions. Must be > 0. |
| `request_timeout_seconds` | no | `30` | HTTP request timeout for WarpScript API calls (seconds) |
| `command_timeout_seconds` | no | `30` | Maximum execution time for scaling and failure commands (seconds) |
| `delay_after_scale_seconds` | no | `interval_seconds` | Wait time after any scaling action (up or down) |
| `on_failure_command` | no | — | Shell command to execute when the failure threshold is reached. `${APP_ID}` is substituted if `apps` is configured. |
| `failure_retries_before_command` | no | `0` | Consecutive WarpScript failures tolerated before executing `on_failure_command` |
| `delay_after_command_success_seconds` | no | `interval_seconds` | Wait time after `on_failure_command` exits 0 |
| `delay_after_command_failure_seconds` | no | `interval_seconds` | Wait time after `on_failure_command` exits non-zero or fails to spawn |
| `apps` | no | `[]` | List of apps to manage; each creates an independent probe instance |

#### Scaling Parameters (`[warpscript_probes.scaling]`)

| Key | Required | Description |
|-----|----------|-------------|
| `instances.min` | yes | Minimum instance count. Must be ≥ 1. |
| `instances.max` | no | Maximum instance count for the last flavor. If absent, defaults to `min` (no instance scaling, only flavor scaling). Must be ≥ `min`. |
| `flavors` | yes | Ordered list of flavor names (e.g. `["S", "M", "L"]`). Must not be empty. |
| `scale_up_threshold` | no | Inline TOML table `{metric = value, …}`. Scale UP if ANY metric value exceeds its threshold. Keys must be present in `warpscript_file`. |
| `scale_down_threshold` | no | Inline TOML table `{metric = value, …}`. Scale DOWN if ALL metric values are below their thresholds. Keys must be present in `warpscript_file`. |
| `upscale_command` | yes | Shell command executed when scaling up. Must not be empty. |
| `downscale_command` | yes | Shell command executed when scaling down. Must not be empty. |

#### App Parameters (WarpScript)

| Key | Required | Description |
|-----|----------|-------------|
| `id` | yes | Identifier substituted as `${APP_ID}` in the script and commands. Only alphanumeric, `-`, `_`, `.` allowed. |
| `warp_token` | no | Per-app read token. Overrides the `WARP_TOKEN` env var. If neither is set, the cycle is skipped with an error log. |

#### How Scaling Works

1. `WARP_ENDPOINT`, `WARP_TOKEN`, and all WarpScript files are resolved once per probe task before the polling loop.
   - If any script file is not readable at startup (e.g., not yet mounted), the probe retries every `interval_seconds` until all files are loaded. The task does not die.
2. At each interval, the probe acquires the distributed lock (if Redis is configured). Only one instance proceeds; others skip the cycle.
3. **After acquiring the lock**, the instance re-reads its state from Redis to get the latest level saved by any previous holder. This prevents stale-level decisions in multi-instance deployments.
4. `${WARP_TOKEN}` and `${APP_ID}` are substituted into each cached script, and it is sent via HTTP POST to `WARP_ENDPOINT`.
5. The last element of the JSON response array is used as the metric value (must be a number).
6. The values are compared against the thresholds:
   - ANY `value > scale_up_threshold[metric]` → execute `upscale_command`, increment level (if the command succeeds)
   - ALL `value < scale_down_threshold[metric]` (and the threshold map is non-empty) → execute `downscale_command`, decrement level (if the command succeeds)
   - Otherwise → no action, wait `interval_seconds`
7. Boundaries: upscale is ignored at max level; downscale is ignored at min level (level 1).
8. After any scaling action, wait `delay_after_scale_seconds` before the next check.
9. On WarpScript execution error, the current level is kept and the consecutive failure counter is incremented. If `on_failure_command` is set and `consecutive_failures > failure_retries_before_command`, the command is executed.
10. The current level is persisted and restored on restart. If the persisted level is no longer valid in the current config (e.g., `flavors` was shortened), it is clamped to level 1 and a `WARN` is logged.

#### Token Resolution

For each polling cycle:

1. If the current app has `warp_token` → use it.
2. Else if `WARP_TOKEN` env var is set → use it.
3. Else → log an error and skip the cycle; retry at the next interval.

#### Command Substitution

The following placeholders are substituted in `upscale_command` and `downscale_command`:

| Placeholder | Replaced with |
|-------------|---------------|
| `${APP_ID}` | The app identifier (only when `apps` is configured) |
| `${flavor}` | The flavor name at the **current** level (upscale) or **target** level (downscale) |
| `${instances}` | The instance count at the **current** level (upscale) or **target** level (downscale) |

For `on_failure_command`, only `${APP_ID}` is substituted.

#### Multi-Metric Example

```toml
[[warpscript_probes]]
name = "Multi-Metric Scaler"
warpscript_file = {cpu = "warpscript/cpu.mc2", memory = "warpscript/mem.mc2"}
interval_seconds = 60

[warpscript_probes.scaling]
instances = {min = 1, max = 3}
flavors   = ["S", "M", "L"]

# Scale UP if cpu > 70% OR memory > 80%
scale_up_threshold = {cpu = 70.0, memory = 80.0}

# Scale DOWN only if BOTH cpu < 40% AND memory < 50%
scale_down_threshold = {cpu = 40.0, memory = 50.0}

upscale_command   = "clever scale --app ${APP_ID} --flavor ${flavor} --instances ${instances}"
downscale_command = "clever scale --app ${APP_ID} --flavor ${flavor} --instances ${instances}"
```

#### WarpScript File Format

Each script is a standard WarpScript (`.mc2`) file. Two substitutions are performed before each execution:

| Placeholder | Replaced with |
|-------------|---------------|
| `${WARP_TOKEN}` | The effective token for this app (per-app or env fallback) |
| `${APP_ID}` | The app identifier |

Each script must leave exactly one numeric value on the stack; the last element of the returned JSON array is used.

```warpscript
// warpscript/cpu.mc2
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

- **Hysteresis**: keep `scale_down_threshold` meaningfully below `scale_up_threshold` to avoid flapping (e.g., up at 70%, down at 40%).
- **Cooldown**: use `delay_after_scale_seconds` to let the system stabilize before re-evaluating.
- **Multi-metric AND logic for downscale**: requiring all metrics to be low before downscaling is conservative and avoids premature scale-down when only one dimension recovers.
- **Script changes**: WarpScript files are read once at startup. Restart the application to pick up edits.
- **Instance-only scaling** (single flavor): set `flavors = ["M"]` with `instances = {min = 1, max = 4}` to scale only instance count.
- **Flavor-only scaling** (fixed instances): set `instances = {min = 2}` (no `max`) with multiple flavors.

---

## Environment Variables

| Variable | Used by | Required | Description |
|----------|---------|----------|-------------|
| `WARP_ENDPOINT` | WarpScript probes | yes (if any WarpScript probe) | URL of the Warp 10 exec API. Validated at startup; logged only at `debug` with credentials masked. |
| `WARP_TOKEN` | WarpScript probes | no | Fallback read token for apps without a per-app `warp_token`. Never logged. |
| `REDIS_URL` | Persistence | no | Full Redis connection URL (takes precedence over individual vars). |
| `REDIS_HOST` | Persistence | no | Redis hostname (used only if `REDIS_URL` is not set). |
| `REDIS_PORT` | Persistence | no | Redis port (default: `6379`). |
| `REDIS_PASSWORD` | Persistence | no | Redis password. Percent-encoded automatically to handle special characters (`@`, `:`, `/`, …). Masked in logs. |
| `MULTI_INSTANCE` | Startup | no | Set to `true` to enable multi-instance mode (equivalent to `--multi-instance`). |
| `RUST_LOG` | Logging | no | Log level filter (default: `info`). See [Logging](#logging). |

---

## Command Execution

All commands (`on_failure_command`, `upscale_command`, `downscale_command`) are run via `sh -c`, so shell operators work:

```toml
on_failure_command = "clever scale --app ${APP_ID} --flavor S && clever restart --app ${APP_ID}"
on_failure_command = "echo 'Alert' | mail -s 'App down' ops@example.com"
```

`${APP_ID}` is substituted before the command is passed to the shell. In scaling commands, `${flavor}` and `${instances}` are also substituted.

### Timeout and Process Group Cleanup

On Linux/macOS, every spawned command is placed in its own **process group**. A RAII guard (`ProcessGroupKillOnDrop`) targets the entire group, not just the top-level `sh` process:

| Outcome | Guard state | Effect |
|---------|-------------|--------|
| Command times out (`command_timeout_seconds` exceeded) | Armed | `SIGKILL` sent to the whole process group on drop — pipelines and sub-shells terminated |
| Task cancelled / probe aborted (e.g. `SIGTERM` received) | Armed | `SIGKILL` sent to the whole process group on drop |
| Command exits normally (`sh` returns) | **Disarmed** | Process group is **not** killed — background jobs started with `&` (intentional daemonisation) survive |

This means that a command such as `my-daemon &` will leave the daemon running after normal completion, which is the expected behaviour. Only timeout or cancellation triggers the group kill.

On non-Unix platforms, only the direct child process is killed via Tokio's `kill_on_drop`.

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
| Consecutive failure counter | ✓ | ✓ |
| Current scaling level | — | ✓ |
| Last metric values | — | ✓ |

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

The Redis URL (including any embedded password) is **never written to logs**. Only a masked form (`://user:****@host`) is logged at startup.

`REDIS_PASSWORD` may contain any character. Special characters (`@`, `:`, `/`, `#`, `?`) are percent-encoded before the URL is constructed, so an unencoded password in the environment variable always produces a valid Redis URL.

Redis keys used:
- `poc-sonde:probe:<probe-name>` — healthcheck probe state
- `poc-sonde:warpscript:<probe-name>` — WarpScript probe state
- `poc-sonde:lock:warpscript:<probe-name>` — distributed lock (WarpScript probes)
- `poc-sonde:lock:healthcheck:<probe-name>` — distributed lock (healthcheck probes)

**Distributed lock token**: each lock acquisition generates a **UUID v4** token. The compare-and-delete release script only removes the key if the stored token matches the caller's token. UUID v4 tokens are globally unique regardless of process PID or wall-clock time, which prevents a replica with PID 1 (common in containers) from accidentally stealing or releasing another instance's lock when two replicas start within the same second.

**Multi-instance state synchronisation**: after acquiring the lock, the winning instance refreshes its in-memory state from Redis before evaluating thresholds. This guarantees that all instances converge on the same `current_level` even if one of them was dormant for several cycles. A `WARN` log is emitted if the fresh level differs from the stale local value.

**Connection failure behaviour:**

| Mode | Redis fails at startup |
|------|------------------------|
| Default (no flag) | Falls back to in-memory — `error` log, continues running |
| `--multi-instance` | **Fatal exit (code 1)** — prevents split-brain |

When running with the Redis backend, the distributed lock TTL is computed as `request_timeout_seconds + command_timeout_seconds + 10` seconds, ensuring the lock outlives the longest possible execution even when `interval_seconds` is shorter than the HTTP timeout.

### Level Validation on Restart

When a WarpScript probe restores its level from state and that level is no longer valid in the current config (e.g., `flavors` was shortened), the level is automatically clamped to level 1 and a `warn` log entry is emitted. No manual cleanup is required. The same clamping is applied when the level is refreshed from Redis mid-run.

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
| Probe results (success / failure) | `info` | |
| Redis URL (masked) | `info` | Password replaced with `****` |
| HTTP probe URLs | `info` / `debug` | Credentials masked if present (`://user:****@host`) |
| State refreshed from Redis (level changed) | `info` | Emitted when fresh level ≠ stale local level |
| Remediation actions (threshold reached, scaling detected, commands executed) | `warn` | Visible with `RUST_LOG=warn` |
| Command exit codes on non-zero | `warn` | |
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
cargo test test_compute_levels_multi_flavor

# With Redis feature
cargo test --features redis-persistence
```

```bash
cargo clippy -- -D warnings
```

### Manual Multi-Instance Test

```bash
# Terminal 1
REDIS_URL=redis://localhost:6379 ./target/release/cc-sonde --config config.toml --multi-instance

# Terminal 2
REDIS_URL=redis://localhost:6379 ./target/release/cc-sonde --config config.toml --multi-instance
```

Observe that:
- Only one instance logs `Executing WarpScript probe` per cycle (the lock winner)
- After a scaling action by instance A, instance B logs `State refreshed from Redis after lock acquisition` with the updated level when it next wins the lock

---

## Troubleshooting

| Symptom | Likely cause |
|---------|-------------|
| `Configuration must contain at least one probe (healthcheck_probes or warpscript_probes)` | Both `healthcheck_probes` and `warpscript_probes` are empty or absent |
| `Probe '…' has no checks configured` | No key defined under `[healthcheck_probes.checks]` |
| `Probe '…' must have either 'url' or 'apps' configured` | Neither `url` nor `apps` specified for a healthcheck probe |
| `Probe '…' cannot have both 'url' and 'apps' configured` | Both `url` and `apps` are set on the same probe |
| `Probe '…': app id '…' contains invalid characters` | `id` contains characters other than alphanumeric, `-`, `_`, `.` |
| `WarpScript probe '…': warpscript_file must define at least one metric` | `warpscript_file` is an empty table `{}` |
| `WarpScript probe '…': scale_up_threshold key '…' not found in warpscript_file` | A threshold key references a metric not present in `warpscript_file` |
| `WarpScript probe '…': flavors must not be empty` | `flavors = []` or the key is absent |
| `WarpScript probe '…': instances.min must be >= 1` | `instances.min = 0` is not allowed |
| `WarpScript probe '…': instances.max (N) must be >= instances.min (M)` | `max` is set to a value smaller than `min` |
| `WarpScript probe '…': upscale_command cannot be empty` | `upscale_command = ""` is not allowed; use `"true"` for a deliberate no-op |
| `WarpScript probe '…': downscale_command cannot be empty` | `downscale_command = ""` is not allowed; use `"true"` for a deliberate no-op |
| `WarpScript probe '…' has invalid interval (must be > 0)` | `interval_seconds = 0` is not allowed; set a positive value |
| `WARP_ENDPOINT environment variable not set` | Required env var missing when WarpScript probes are configured |
| `No Warp token available …` | App has no `warp_token` and `WARP_TOKEN` env var is not set; that cycle is skipped |
| `Failed to read WarpScript file, will retry` | File not found or permission error; the probe retries every `interval_seconds` |
| WarpScript changes not reflected | The script is read once at startup; restart the application after editing the `.mc2` file |
| Scaling level reset to minimum after restart | The previously persisted level is not in the current config; expected behaviour after reducing `flavors` or `instances.max` |
| `${flavor}` / `${instances}` not substituted in command | Verify the placeholders are spelled exactly as shown; only `upscale_command` and `downscale_command` receive these substitutions |
| Two instances diverge on `current_level` | Expected without Redis; with Redis and `--multi-instance`, state is synced after each lock acquisition — check logs for `State refreshed from Redis` |
| Command times out but child processes keep running | Should not happen on Linux/macOS — the whole process group is killed. On other platforms, only `sh` is killed. |
| `Redis URL provided but redis-persistence feature is not enabled` | Rebuild with `cargo build --features redis-persistence` |
| Redis connection fails at startup | Without `--multi-instance`: falls back to in-memory (error log). With `--multi-instance`: fatal exit. Check `REDIS_URL` / `REDIS_HOST` and network reachability. |
| `Fatal: Redis connection failed in multi-instance mode: …` | `--multi-instance` is active but Redis is unreachable. Fix the Redis config or remove `--multi-instance` for single-instance deployments. |
| `REDIS_PASSWORD` with special characters breaks the Redis URL | Handled automatically via percent-encoding; ensure you are running the current build |
| Background process started with `&` is killed immediately after command exits | Should not happen — the process group SIGKILL guard is disarmed on normal command completion. |

---

## Security Notes

- **Privileges**: commands execute with the same OS privileges as the application. Consider running as a dedicated low-privilege user.
- **`${APP_ID}` validation**: app IDs are validated at startup — only alphanumeric characters, `-`, `_`, and `.` are allowed. This prevents shell injection via the `${APP_ID}` placeholder.
- **`${flavor}` and `${instances}`**: these are derived from the TOML config (flavor strings and integer counts), not from external inputs, so they do not introduce injection risk.
- **Shell execution surface**: all commands are run via `sh -c` to support pipes and shell operators. The **content** of `on_failure_command`, `upscale_command`, and `downscale_command` is not otherwise validated and is the responsibility of the administrator who writes the configuration file. Only trusted administrators should have write access to the TOML file.
- **Tokens in configuration**: prefer the `WARP_TOKEN` environment variable over inline `warp_token` values in the TOML file. Environment variables are not stored on disk and are less likely to be accidentally committed to version control or exposed in file system backups.
- **Command logging**: command strings are only logged at `debug` level, as they may contain tokens or passwords. Run with `RUST_LOG=info` (the default) in production to avoid exposing them.
- **URL credential sanitisation**: all URLs — Redis, WarpScript endpoints, and HTTP probe endpoints — are sanitised before being passed to the logger. Any `://user:PASSWORD@host` authority is masked as `://user:****@host`, regardless of scheme.
- **`WARP_ENDPOINT` logging**: logged only at `debug` level and sanitised; never logged at `info`.
- **Redis passwords**: percent-encoded at URL construction time (so special characters do not corrupt the URL), then masked in all log output. The raw password is never written anywhere.
- **stdout**: command stdout is never logged (it may contain sensitive data). Only stderr is logged, and only on non-zero exit.
- **Multi-instance fail-safe**: without `--multi-instance`, a Redis failure causes a silent fallback to in-memory. In a multi-replica deployment, enable `--multi-instance` so that the process exits rather than producing split-brain behaviour.

---

## License

MIT — use at your own discretion.

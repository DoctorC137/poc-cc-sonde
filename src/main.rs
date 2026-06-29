mod config;
mod executor;
mod healthcheck;
mod healthcheck_probe;
mod healthcheck_scheduler;
mod persistence;
mod utils;
mod warpscript_probe;
mod warpscript_scheduler;

use clap::Parser;
use percent_encoding::{utf8_percent_encode, NON_ALPHANUMERIC};
use std::env;
use std::time::Duration;
use tracing::{debug, info, warn};
use tracing_subscriber::{layer::SubscriberExt, util::SubscriberInitExt};

#[derive(Parser, Debug)]
#[command(author, version, about, long_about = None)]
struct Args {
    /// Configuration file path
    #[arg(long, default_value = "config.toml")]
    config: String,

    /// Enable health check HTTP server
    #[arg(long, default_value_t = false)]
    healthcheck: bool,

    /// Port for health check server (requires --healthcheck)
    #[arg(long, default_value_t = 8080)]
    healthcheck_port: u16,

    /// Host/address to bind the health check server on (requires --healthcheck)
    #[arg(long, default_value = "0.0.0.0", env = "HEALTHCHECK_HOST")]
    healthcheck_host: String,

    /// Dry run mode: probe checks are executed but remediation commands are not
    #[arg(long, default_value_t = false)]
    dry_run: bool,

    /// Multi-instance mode: Redis is required for distributed locking.
    /// Can also be set via the MULTI_INSTANCE environment variable.
    #[arg(long, default_value_t = false, env = "MULTI_INSTANCE")]
    multi_instance: bool,

    /// Maximum time (seconds) to wait for tasks to finish after shutdown signal.
    /// If exceeded, the process exits immediately. Default: 2.
    /// Can also be set via the SHUTDOWN_TIMEOUT environment variable.
    #[arg(long, default_value_t = 2, env = "SHUTDOWN_TIMEOUT")]
    shutdown_timeout: u64,
}

/// Get Redis URL from environment variables
/// Priority: REDIS_URL > (REDIS_HOST + REDIS_PORT + REDIS_PASSWORD)
fn get_redis_url() -> Option<String> {
    // First, try REDIS_URL
    if let Ok(url) = env::var("REDIS_URL") {
        return Some(url);
    }

    // Otherwise, try to build from components
    if let Ok(host) = env::var("REDIS_HOST") {
        let port = env::var("REDIS_PORT").unwrap_or_else(|_| "6379".to_string());
        let password = env::var("REDIS_PASSWORD").ok();

        let url = if let Some(pwd) = password {
            let encoded_pwd = utf8_percent_encode(&pwd, NON_ALPHANUMERIC).to_string();
            format!("redis://:{}@{}:{}", encoded_pwd, host, port)
        } else {
            format!("redis://{}:{}", host, port)
        };

        return Some(url);
    }

    None
}


#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    // Initialize tracing/logging
    tracing_subscriber::registry()
        .with(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "cc_sonde=warn".into()),
        )
        .with(tracing_subscriber::fmt::layer())
        .init();

    // Parse command line arguments
    let args = Args::parse();

    info!("Starting HTTP monitoring application");

    if args.dry_run {
        warn!("Dry run mode enabled: remediation commands will not be executed");
    }

    if args.multi_instance {
        warn!("Multi-instance mode enabled: Redis is required for distributed locking");
    }

    // Load and validate configuration
    info!(config_path = %args.config, "Loading configuration");
    let config = config::Config::from_file(&args.config)?;

    info!(
        http_probe_count = config.healthcheck_probes.len(),
        warpscript_probe_count = config.warpscript_probes.len(),
        "Configuration loaded successfully"
    );

    // Check WarpScript environment variables if WarpScript probes are configured
    if !config.warpscript_probes.is_empty() {
        // WARP_ENDPOINT is always required
        let endpoint = env::var("WARP_ENDPOINT").map_err(|_| {
            "WARP_ENDPOINT environment variable not set, but WarpScript probes are configured"
        })?;

        debug!(
            warp_endpoint = %utils::sanitize_url_for_log(&endpoint),
            "WarpScript environment configured"
        );
    }

    // Initialize persistence backend
    let redis_url = get_redis_url();
    if let Some(ref url) = redis_url {
        let masked_url = utils::sanitize_url_for_log(url);
        info!(redis_url = %masked_url, "Redis configuration detected");
    } else {
        info!("No Redis configuration found, using in-memory persistence");
    }

    let backend = persistence::create_backend(redis_url.clone(), args.multi_instance)
        .await
        .unwrap_or_else(|e| {
            eprintln!("Fatal: Redis connection failed in multi-instance mode: {}", e);
            std::process::exit(1);
        });

    if redis_url.is_some() && !args.multi_instance {
        warn!(
            "Redis is configured but --multi-instance is not set. \
             If running multiple replicas, add --multi-instance (or MULTI_INSTANCE=true) \
             to enable distributed locking."
        );
    }

    // Bind health check server before spawning so startup failures are caught immediately
    if args.healthcheck {
        info!(host = %args.healthcheck_host, port = args.healthcheck_port, "Starting health check server");
        let listener = healthcheck::bind_healthcheck_server(&args.healthcheck_host, args.healthcheck_port)?;
        tokio::spawn(async move { healthcheck::serve_healthcheck(listener).await });
    }

    // Spawn a task for each healthcheck probe
    // If apps is specified, create one probe instance per app
    let mut handles = vec![];

    for probe in config.healthcheck_probes {
        if probe.apps.is_empty() {
            // No apps: single probe with direct url
            info!(
                probe_name = %probe.name,
                url = %utils::sanitize_url_for_log(probe.url.as_deref().unwrap_or("")),
                interval_seconds = probe.interval_seconds,
                "Spawning healthcheck probe task"
            );

            let backend_clone = backend.clone();
            let handle = tokio::spawn(healthcheck_scheduler::schedule_probe(probe, backend_clone, args.dry_run, args.multi_instance));
            handles.push(handle);
        } else {
            // With apps: create one probe instance per app
            let apps_count = probe.apps.len();
            info!(
                probe_name = %probe.name,
                apps_count = apps_count,
                "Expanding healthcheck probe for each app"
            );

            for app in &probe.apps {
                let mut probe_instance = probe.clone();
                probe_instance.name = format!("{} - {}", probe.name, app.id);
                probe_instance.url = Some(app.url.clone());
                probe_instance.apps = vec![app.clone()];

                info!(
                    probe_name = %probe_instance.name,
                    app_id = %app.id,
                    url = %utils::sanitize_url_for_log(&app.url),
                    interval_seconds = probe_instance.interval_seconds,
                    "Spawning healthcheck probe instance"
                );

                let backend_clone = backend.clone();
                let handle = tokio::spawn(healthcheck_scheduler::schedule_probe(
                    probe_instance,
                    backend_clone,
                    args.dry_run,
                    args.multi_instance,
                ));
                handles.push(handle);
            }
        }
    }

    // Spawn a task for each WarpScript probe
    // If apps is specified, create one probe instance per app
    for probe in config.warpscript_probes {
        if probe.apps.is_empty() {
            // No apps: create a single probe as-is
            info!(
                probe_name = %probe.name,
                interval_seconds = probe.interval_seconds,
                metrics_count = probe.warpscript_files.len(),
                "Spawning WarpScript probe task"
            );

            let backend_clone = backend.clone();
            let handle = tokio::spawn(warpscript_scheduler::schedule_warpscript_probe(
                probe,
                backend_clone,
                args.dry_run,
                args.multi_instance,
            ));
            handles.push(handle);
        } else {
            // With apps: create one probe instance per app
            let apps_count = probe.apps.len();
            info!(
                probe_name = %probe.name,
                apps_count = apps_count,
                "Expanding WarpScript probe for each app"
            );

            for app in &probe.apps {
                let mut probe_instance = probe.clone();
                // Update probe name to include app_id
                probe_instance.name = format!("{} - {}", probe.name, app.id);
                // Keep only this app
                probe_instance.apps = vec![app.clone()];

                info!(
                    probe_name = %probe_instance.name,
                    app_id = %app.id,
                    has_custom_token = app.warp_token.is_some(),
                    interval_seconds = probe_instance.interval_seconds,
                    metrics_count = probe_instance.warpscript_files.len(),
                    "Spawning WarpScript probe instance"
                );

                let backend_clone = backend.clone();
                let handle = tokio::spawn(warpscript_scheduler::schedule_warpscript_probe(
                    probe_instance,
                    backend_clone,
                    args.dry_run,
                    args.multi_instance,
                ));
                handles.push(handle);
            }
        }
    }

    info!("All probe tasks spawned, waiting for shutdown signal");

    #[cfg(unix)]
    {
        use tokio::signal::unix::{signal, SignalKind};
        let mut sigterm = signal(SignalKind::terminate())?;
        tokio::select! {
            _ = tokio::signal::ctrl_c() => {}
            _ = sigterm.recv() => {}
        }
    }
    #[cfg(not(unix))]
    {
        tokio::signal::ctrl_c().await?;
    }

    info!("Shutdown signal received, terminating...");

    // Watchdog : garantit la sortie dans `shutdown_timeout` secondes,
    // même si le runtime Tokio est bloqué par getaddrinfo() ou un connect TCP.
    let watchdog_timeout = args.shutdown_timeout;
    std::thread::spawn(move || {
        std::thread::sleep(Duration::from_secs(watchdog_timeout));
        warn!(
            shutdown_timeout = watchdog_timeout,
            "Shutdown timeout reached, forcing exit"
        );
        std::process::exit(1);
    });

    for handle in &handles {
        handle.abort();
    }
    // Chemin rapide : si les tâches s'annulent normalement (runtime non bloqué)
    for handle in handles {
        let _ = handle.await;
    }
    info!("All tasks terminated");
    std::process::exit(0);
}

mod config;
mod executor;
mod healthcheck;
mod healthcheck_probe;
mod healthcheck_scheduler;
mod persistence;
mod warpscript_probe;
mod warpscript_scheduler;

use clap::Parser;
use std::env;
use tracing::info;
use tracing_subscriber::{layer::SubscriberExt, util::SubscriberInitExt};

#[derive(Parser, Debug)]
#[command(author, version, about, long_about = None)]
struct Args {
    /// Configuration file path
    #[arg(default_value = "config.toml")]
    config: String,

    /// Enable health check HTTP server
    #[arg(long, default_value_t = false)]
    healthcheck: bool,

    /// Port for health check server (requires --healthcheck)
    #[arg(long, default_value_t = 8080)]
    healthcheck_port: u16,
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
            format!("redis://:{}@{}:{}", pwd, host, port)
        } else {
            format!("redis://{}:{}", host, port)
        };

        return Some(url);
    }

    None
}

/// Mask password in Redis URL for safe logging
fn mask_redis_password(url: &str) -> String {
    // Match pattern: redis://:PASSWORD@host:port
    if let Some(idx) = url.find("://:") {
        if let Some(end_idx) = url[idx + 4..].find('@') {
            let mut masked = url.to_string();
            masked.replace_range(idx + 4..idx + 4 + end_idx, "****");
            return masked;
        }
    }
    url.to_string()
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    // Initialize tracing/logging
    tracing_subscriber::registry()
        .with(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "poc_sonde=info".into()),
        )
        .with(tracing_subscriber::fmt::layer())
        .init();

    // Parse command line arguments
    let args = Args::parse();

    info!("Starting HTTP monitoring application");

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
        let endpoint = env::var("WARP_ENDPOINT")
            .map_err(|_| "WARP_ENDPOINT environment variable not set, but WarpScript probes are configured")?;

        info!(
            warp_endpoint = %endpoint,
            "WarpScript environment configured"
        );
    }

    // Initialize persistence backend
    let redis_url = get_redis_url();
    if let Some(ref url) = redis_url {
        // Mask password in logs
        let masked_url = mask_redis_password(url);
        info!(redis_url = %masked_url, "Redis configuration detected");
    } else {
        info!("No Redis configuration found, using in-memory persistence");
    }

    let backend = persistence::create_backend(redis_url).await;

    // Spawn health check server if enabled
    if args.healthcheck {
        info!(
            port = args.healthcheck_port,
            "Starting health check server"
        );
        tokio::spawn(async move {
            if let Err(e) = healthcheck::start_healthcheck_server(args.healthcheck_port).await {
                tracing::error!(error = %e, "Health check server failed");
            }
        });
    }

    // Spawn a task for each healthcheck probe
    // If apps is specified, create one probe instance per app
    let mut handles = vec![];

    for probe in config.healthcheck_probes {
        if probe.apps.is_empty() {
            // No apps: single probe with direct url
            info!(
                probe_name = %probe.name,
                url = %probe.url.as_deref().unwrap_or(""),
                interval_seconds = probe.interval_seconds,
                "Spawning healthcheck probe task"
            );

            let backend_clone = backend.clone();
            let handle = tokio::spawn(healthcheck_scheduler::schedule_probe(probe, backend_clone));
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
                    url = %app.url,
                    interval_seconds = probe_instance.interval_seconds,
                    "Spawning healthcheck probe instance"
                );

                let backend_clone = backend.clone();
                let handle = tokio::spawn(healthcheck_scheduler::schedule_probe(probe_instance, backend_clone));
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
                warpscript_file = %probe.warpscript_file,
                interval_seconds = probe.interval_seconds,
                levels_count = probe.levels.len(),
                "Spawning WarpScript probe task"
            );

            let backend_clone = backend.clone();
            let handle = tokio::spawn(warpscript_scheduler::schedule_warpscript_probe(probe, backend_clone));
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
                    warpscript_file = %probe_instance.warpscript_file,
                    interval_seconds = probe_instance.interval_seconds,
                    levels_count = probe_instance.levels.len(),
                    "Spawning WarpScript probe instance"
                );

                let backend_clone = backend.clone();
                let handle = tokio::spawn(warpscript_scheduler::schedule_warpscript_probe(probe_instance, backend_clone));
                handles.push(handle);
            }
        }
    }

    info!("All probe tasks spawned, waiting for shutdown signal");

    // Wait for Ctrl+C
    tokio::signal::ctrl_c().await?;

    info!("Shutdown signal received, terminating...");

    Ok(())
}

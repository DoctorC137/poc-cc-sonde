mod config;
mod executor;
mod healthcheck;
mod persistence;
mod probe;
mod scheduler;

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
        probe_count = config.probes.len(),
        "Configuration loaded successfully"
    );

    // Initialize persistence backend
    let redis_url = env::var("REDIS_URL").ok();
    if let Some(ref url) = redis_url {
        info!(redis_url = %url, "Redis URL detected");
    } else {
        info!("No REDIS_URL environment variable, using in-memory persistence");
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

    // Spawn a task for each probe
    let mut handles = vec![];

    for probe in config.probes {
        info!(
            probe_name = %probe.name,
            url = %probe.url,
            interval_seconds = probe.interval_seconds,
            "Spawning probe task"
        );

        let backend_clone = backend.clone();
        let handle = tokio::spawn(scheduler::schedule_probe(probe, backend_clone));
        handles.push(handle);
    }

    info!("All probe tasks spawned, waiting for shutdown signal");

    // Wait for Ctrl+C
    tokio::signal::ctrl_c().await?;

    info!("Shutdown signal received, terminating...");

    Ok(())
}

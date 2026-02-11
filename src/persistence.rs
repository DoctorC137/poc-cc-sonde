use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};
use tokio::sync::Mutex;
#[cfg(feature = "redis-persistence")]
use tracing::{debug, error, info};
#[cfg(not(feature = "redis-persistence"))]
use tracing::{debug, info, warn};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ProbeState {
    pub probe_name: String,
    pub last_check_timestamp: u64,
    pub last_check_success: bool,
    pub next_check_timestamp: u64,
}

#[async_trait::async_trait]
pub trait PersistenceBackend: Send + Sync {
    async fn save_state(&self, state: &ProbeState) -> Result<(), Box<dyn std::error::Error>>;
    async fn load_state(&self, probe_name: &str) -> Result<Option<ProbeState>, Box<dyn std::error::Error>>;
}

// In-memory implementation (default)
pub struct InMemoryBackend {
    states: Arc<Mutex<HashMap<String, ProbeState>>>,
}

impl InMemoryBackend {
    pub fn new() -> Self {
        Self {
            states: Arc::new(Mutex::new(HashMap::new())),
        }
    }
}

#[async_trait::async_trait]
impl PersistenceBackend for InMemoryBackend {
    async fn save_state(&self, state: &ProbeState) -> Result<(), Box<dyn std::error::Error>> {
        let mut states = self.states.lock().await;
        states.insert(state.probe_name.clone(), state.clone());
        debug!(
            probe_name = %state.probe_name,
            last_success = state.last_check_success,
            next_check = state.next_check_timestamp,
            "State saved to memory"
        );
        Ok(())
    }

    async fn load_state(&self, probe_name: &str) -> Result<Option<ProbeState>, Box<dyn std::error::Error>> {
        let states = self.states.lock().await;
        Ok(states.get(probe_name).cloned())
    }
}

// Redis implementation (optional)
#[cfg(feature = "redis-persistence")]
pub struct RedisBackend {
    client: redis::Client,
}

#[cfg(feature = "redis-persistence")]
impl RedisBackend {
    pub async fn new(redis_url: &str) -> Result<Self, Box<dyn std::error::Error>> {
        info!(redis_url = %redis_url, "Connecting to Redis");
        let client = redis::Client::open(redis_url)?;

        // Test connection
        let mut con = client.get_multiplexed_async_connection().await?;
        redis::cmd("PING").query_async::<String>(&mut con).await?;

        info!("Successfully connected to Redis");
        Ok(Self { client })
    }

    fn get_key(probe_name: &str) -> String {
        format!("poc-sonde:probe:{}", probe_name)
    }
}

#[cfg(feature = "redis-persistence")]
#[async_trait::async_trait]
impl PersistenceBackend for RedisBackend {
    async fn save_state(&self, state: &ProbeState) -> Result<(), Box<dyn std::error::Error>> {
        let mut con = self.client.get_multiplexed_async_connection().await?;
        let key = Self::get_key(&state.probe_name);
        let value = serde_json::to_string(state)?;

        redis::cmd("SET")
            .arg(&key)
            .arg(&value)
            .query_async::<()>(&mut con)
            .await?;

        debug!(
            probe_name = %state.probe_name,
            last_success = state.last_check_success,
            next_check = state.next_check_timestamp,
            "State saved to Redis"
        );
        Ok(())
    }

    async fn load_state(&self, probe_name: &str) -> Result<Option<ProbeState>, Box<dyn std::error::Error>> {
        let mut con = self.client.get_multiplexed_async_connection().await?;
        let key = Self::get_key(probe_name);

        let value: Option<String> = redis::cmd("GET")
            .arg(&key)
            .query_async(&mut con)
            .await?;

        match value {
            Some(json) => {
                let state: ProbeState = serde_json::from_str(&json)?;
                debug!(
                    probe_name = %probe_name,
                    last_success = state.last_check_success,
                    "State loaded from Redis"
                );
                Ok(Some(state))
            }
            None => {
                debug!(probe_name = %probe_name, "No state found in Redis");
                Ok(None)
            }
        }
    }
}

pub async fn create_backend(redis_url: Option<String>) -> Arc<dyn PersistenceBackend> {
    #[cfg(feature = "redis-persistence")]
    {
        if let Some(url) = redis_url {
            match RedisBackend::new(&url).await {
                Ok(backend) => {
                    info!("Using Redis persistence backend");
                    return Arc::new(backend);
                }
                Err(e) => {
                    error!(
                        error = %e,
                        "Failed to connect to Redis, falling back to in-memory backend"
                    );
                }
            }
        }
    }

    #[cfg(not(feature = "redis-persistence"))]
    {
        if redis_url.is_some() {
            warn!("Redis URL provided but redis-persistence feature is not enabled");
            warn!("Rebuild with: cargo build --features redis-persistence");
        }
    }

    info!("Using in-memory persistence backend");
    Arc::new(InMemoryBackend::new())
}

pub fn current_timestamp() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_secs()
}

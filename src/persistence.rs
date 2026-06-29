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
    #[serde(default)]
    pub consecutive_failures: u32,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WarpScriptProbeState {
    pub probe_name: String,
    pub last_check_timestamp: u64,
    pub current_level: u32,
    #[serde(default)]
    pub last_values: HashMap<String, f64>,
    pub next_check_timestamp: u64,
    #[serde(default)]
    pub consecutive_failures: u32,
    #[serde(default)]
    pub upscale_blocked_until: u64,
    #[serde(default)]
    pub downscale_blocked_until: u64,
    #[serde(default)]
    pub consecutive_scaling_failures: u32,
}

#[async_trait::async_trait]
pub trait PersistenceBackend: Send + Sync {
    async fn save_state(&self, state: &ProbeState) -> Result<(), Box<dyn std::error::Error>>;
    async fn load_state(
        &self,
        probe_name: &str,
    ) -> Result<Option<ProbeState>, Box<dyn std::error::Error>>;

    async fn save_warpscript_state(
        &self,
        state: &WarpScriptProbeState,
    ) -> Result<(), Box<dyn std::error::Error>>;
    async fn load_warpscript_state(
        &self,
        probe_name: &str,
    ) -> Result<Option<WarpScriptProbeState>, Box<dyn std::error::Error>>;

    async fn acquire_lock(
        &self,
        key: &str,
        ttl_ms: u64,
    ) -> Result<Option<String>, Box<dyn std::error::Error>>;
    async fn release_lock(&self, key: &str, token: &str) -> Result<(), Box<dyn std::error::Error>>;
}

// In-memory implementation (default)
pub struct InMemoryBackend {
    states: Arc<Mutex<HashMap<String, ProbeState>>>,
    warpscript_states: Arc<Mutex<HashMap<String, WarpScriptProbeState>>>,
}

impl InMemoryBackend {
    pub fn new() -> Self {
        Self {
            states: Arc::new(Mutex::new(HashMap::new())),
            warpscript_states: Arc::new(Mutex::new(HashMap::new())),
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

    async fn load_state(
        &self,
        probe_name: &str,
    ) -> Result<Option<ProbeState>, Box<dyn std::error::Error>> {
        let states = self.states.lock().await;
        Ok(states.get(probe_name).cloned())
    }

    async fn save_warpscript_state(
        &self,
        state: &WarpScriptProbeState,
    ) -> Result<(), Box<dyn std::error::Error>> {
        let mut states = self.warpscript_states.lock().await;
        states.insert(state.probe_name.clone(), state.clone());
        debug!(
            probe_name = %state.probe_name,
            current_level = %state.current_level,
            "WarpScript state saved to memory"
        );
        Ok(())
    }

    async fn load_warpscript_state(
        &self,
        probe_name: &str,
    ) -> Result<Option<WarpScriptProbeState>, Box<dyn std::error::Error>> {
        let states = self.warpscript_states.lock().await;
        Ok(states.get(probe_name).cloned())
    }

    async fn acquire_lock(
        &self,
        _key: &str,
        _ttl_ms: u64,
    ) -> Result<Option<String>, Box<dyn std::error::Error>> {
        Ok(Some("inmemory".to_string()))
    }

    async fn release_lock(&self, _key: &str, _token: &str) -> Result<(), Box<dyn std::error::Error>> {
        Ok(())
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
        info!("Connecting to Redis");
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

    #[cfg(test)]
    pub(crate) fn client(&self) -> &redis::Client {
        &self.client
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

    async fn load_state(
        &self,
        probe_name: &str,
    ) -> Result<Option<ProbeState>, Box<dyn std::error::Error>> {
        let mut con = self.client.get_multiplexed_async_connection().await?;
        let key = Self::get_key(probe_name);

        let value: Option<String> = redis::cmd("GET").arg(&key).query_async(&mut con).await?;

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

    async fn save_warpscript_state(
        &self,
        state: &WarpScriptProbeState,
    ) -> Result<(), Box<dyn std::error::Error>> {
        let mut con = self.client.get_multiplexed_async_connection().await?;
        let key = format!("poc-sonde:warpscript:{}", state.probe_name);
        let value = serde_json::to_string(state)?;

        redis::cmd("SET")
            .arg(&key)
            .arg(&value)
            .query_async::<()>(&mut con)
            .await?;

        debug!(
            probe_name = %state.probe_name,
            current_level = %state.current_level,
            "WarpScript state saved to Redis"
        );
        Ok(())
    }

    async fn load_warpscript_state(
        &self,
        probe_name: &str,
    ) -> Result<Option<WarpScriptProbeState>, Box<dyn std::error::Error>> {
        let mut con = self.client.get_multiplexed_async_connection().await?;
        let key = format!("poc-sonde:warpscript:{}", probe_name);

        let value: Option<String> = redis::cmd("GET").arg(&key).query_async(&mut con).await?;

        match value {
            Some(json) => {
                let state: WarpScriptProbeState = serde_json::from_str(&json)?;
                debug!(
                    probe_name = %probe_name,
                    current_level = %state.current_level,
                    "WarpScript state loaded from Redis"
                );
                Ok(Some(state))
            }
            None => {
                debug!(probe_name = %probe_name, "No WarpScript state found in Redis");
                Ok(None)
            }
        }
    }

    async fn acquire_lock(
        &self,
        key: &str,
        ttl_ms: u64,
    ) -> Result<Option<String>, Box<dyn std::error::Error>> {
        let mut con = self.client.get_multiplexed_async_connection().await?;
        let token = uuid::Uuid::new_v4().to_string();
        let result: Option<String> = redis::cmd("SET")
            .arg(key)
            .arg(&token)
            .arg("NX")
            .arg("PX")
            .arg(ttl_ms as usize)
            .query_async(&mut con)
            .await?;
        if result.is_some() {
            Ok(Some(token))
        } else {
            Ok(None)
        }
    }

    async fn release_lock(&self, key: &str, token: &str) -> Result<(), Box<dyn std::error::Error>> {
        let mut con = self.client.get_multiplexed_async_connection().await?;
        redis::Script::new(
            "if redis.call('get', KEYS[1]) == ARGV[1] then return redis.call('del', KEYS[1]) else return 0 end"
        )
        .key(key)
        .arg(token)
        .invoke_async::<i64>(&mut con)
        .await?;
        Ok(())
    }
}

/// Create the persistence backend.
///
/// When `redis_required` is `true` (i.e. multi-instance mode) and the Redis connection
/// fails, this function returns `Err` so the caller can exit fatally rather than silently
/// falling back to in-memory (which would cause split-brain across replicas).
pub async fn create_backend(
    redis_url: Option<String>,
    redis_required: bool,
) -> Result<Arc<dyn PersistenceBackend>, Box<dyn std::error::Error>> {
    #[cfg(feature = "redis-persistence")]
    {
        if let Some(url) = redis_url {
            match RedisBackend::new(&url).await {
                Ok(backend) => {
                    info!("Using Redis persistence backend");
                    return Ok(Arc::new(backend));
                }
                Err(e) => {
                    if redis_required {
                        return Err(e);
                    }
                    error!(
                        error = %e,
                        "Failed to connect to Redis, falling back to in-memory backend"
                    );
                }
            }
        } else if redis_required {
            return Err("Multi-instance mode requires a Redis URL".into());
        }
    }

    #[cfg(not(feature = "redis-persistence"))]
    {
        if redis_url.is_some() {
            if redis_required {
                return Err(
                    "Multi-instance mode requires Redis, but redis-persistence feature is not \
                     compiled in. Rebuild with: cargo build --features redis-persistence"
                        .into(),
                );
            }
            warn!("Redis URL provided but redis-persistence feature is not enabled");
            warn!("Rebuild with: cargo build --features redis-persistence");
        }
    }

    info!("Using in-memory persistence backend");
    Ok(Arc::new(InMemoryBackend::new()))
}

/// A persistence backend for tests that always fails lock acquisition.
/// All state operations are delegated to an inner `InMemoryBackend`.
#[cfg(test)]
pub(crate) struct FailingLockBackend {
    inner: InMemoryBackend,
}

#[cfg(test)]
impl FailingLockBackend {
    pub(crate) fn new() -> Self {
        Self { inner: InMemoryBackend::new() }
    }
}

#[cfg(test)]
#[async_trait::async_trait]
impl PersistenceBackend for FailingLockBackend {
    async fn save_state(&self, state: &ProbeState) -> Result<(), Box<dyn std::error::Error>> {
        self.inner.save_state(state).await
    }
    async fn load_state(&self, probe_name: &str) -> Result<Option<ProbeState>, Box<dyn std::error::Error>> {
        self.inner.load_state(probe_name).await
    }
    async fn save_warpscript_state(&self, state: &WarpScriptProbeState) -> Result<(), Box<dyn std::error::Error>> {
        self.inner.save_warpscript_state(state).await
    }
    async fn load_warpscript_state(&self, probe_name: &str) -> Result<Option<WarpScriptProbeState>, Box<dyn std::error::Error>> {
        self.inner.load_warpscript_state(probe_name).await
    }
    async fn acquire_lock(&self, _key: &str, _ttl_ms: u64) -> Result<Option<String>, Box<dyn std::error::Error>> {
        Err("Redis unavailable (test)".into())
    }
    async fn release_lock(&self, _key: &str, _token: &str) -> Result<(), Box<dyn std::error::Error>> {
        Ok(())
    }
}

pub fn current_timestamp() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_secs()
}

#[cfg(all(test, feature = "redis-persistence"))]
mod tests {
    use super::{PersistenceBackend, RedisBackend};
    use std::sync::Arc;
    use std::time::{Duration, SystemTime, UNIX_EPOCH};
    use tokio::time::sleep;

    // Skip le test si Redis n'est pas disponible
    macro_rules! skip_without_redis {
        ($opt:expr) => {
            match $opt {
                Some(b) => b,
                None => {
                    eprintln!("Skipping: no Redis available");
                    return;
                }
            }
        };
    }

    // Lit REDIS_URL ou REDIS_HOST, tente la connexion (PING inclus dans new())
    async fn get_test_backend() -> Option<RedisBackend> {
        let url = if let Ok(u) = std::env::var("REDIS_URL") {
            u
        } else if let Ok(host) = std::env::var("REDIS_HOST") {
            format!("redis://{}:6379", host)
        } else {
            eprintln!("Skipping Redis tests: REDIS_URL and REDIS_HOST not set");
            return None;
        };
        match RedisBackend::new(&url).await {
            Ok(b) => Some(b),
            Err(e) => {
                eprintln!("Skipping Redis tests: connection failed: {}", e);
                None
            }
        }
    }

    // Supprime la clé de test après chaque scénario
    async fn cleanup(client: &redis::Client, key: &str) {
        if let Ok(mut con) = client.get_multiplexed_async_connection().await {
            let _: Result<(), _> = redis::cmd("DEL").arg(key).query_async(&mut con).await;
        }
    }

    // Clé unique par invocation (résolution nanoseconde)
    fn unique_key() -> String {
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        format!("test:lock:{:x}", nanos)
    }

    // 1. Acquisition sur clé libre → Some(token)
    #[tokio::test]
    async fn test_acquire_free_lock() {
        let backend = skip_without_redis!(get_test_backend().await);
        let key = unique_key();
        let result = backend.acquire_lock(&key, 5000).await.unwrap();
        assert!(result.is_some(), "Expected Some(token) for a free key");
        cleanup(backend.client(), &key).await;
    }

    // 2. Double acquisition → la seconde retourne None
    #[tokio::test]
    async fn test_acquire_held_lock() {
        let backend = skip_without_redis!(get_test_backend().await);
        let key = unique_key();
        let first = backend.acquire_lock(&key, 5000).await.unwrap();
        assert!(first.is_some(), "First acquire should succeed");
        let second = backend.acquire_lock(&key, 5000).await.unwrap();
        assert!(second.is_none(), "Second acquire should fail while key is held");
        cleanup(backend.client(), &key).await;
    }

    // 3. Release avec bon token → clé supprimée → re-acquisition possible
    #[tokio::test]
    async fn test_release_correct_token_allows_reacquire() {
        let backend = skip_without_redis!(get_test_backend().await);
        let key = unique_key();
        let token = backend.acquire_lock(&key, 5000).await.unwrap()
            .expect("First acquire must succeed");
        backend.release_lock(&key, &token).await.unwrap();
        let second = backend.acquire_lock(&key, 5000).await.unwrap();
        assert!(second.is_some(), "Re-acquire after correct release should succeed");
        cleanup(backend.client(), &key).await;
    }

    // 4. Release avec mauvais token → Lua no-op → clé toujours présente
    #[tokio::test]
    async fn test_release_wrong_token_leaves_lock() {
        let backend = skip_without_redis!(get_test_backend().await);
        let key = unique_key();
        let _token = backend.acquire_lock(&key, 5000).await.unwrap()
            .expect("Acquire must succeed");
        backend.release_lock(&key, "wrong_token").await.unwrap();
        let second = backend.acquire_lock(&key, 5000).await.unwrap();
        assert!(second.is_none(), "Lock must still be held after wrong-token release");
        cleanup(backend.client(), &key).await;
    }

    // 5. Expiration TTL → re-acquisition possible après délai
    #[tokio::test]
    async fn test_lock_ttl_expiry() {
        let backend = skip_without_redis!(get_test_backend().await);
        let key = unique_key();
        let first = backend.acquire_lock(&key, 100).await.unwrap();
        assert!(first.is_some(), "Initial acquire must succeed");
        sleep(Duration::from_millis(200)).await;
        let second = backend.acquire_lock(&key, 5000).await.unwrap();
        assert!(second.is_some(), "Acquire after TTL expiry must succeed");
        cleanup(backend.client(), &key).await;
    }

    // 6. Deux tâches concurrentes → exactement une gagne
    #[tokio::test]
    async fn test_concurrent_acquire_one_winner() {
        let backend = Arc::new(skip_without_redis!(get_test_backend().await));
        let key = unique_key();
        let b1 = Arc::clone(&backend);
        let b2 = Arc::clone(&backend);
        let k1 = key.clone();
        let k2 = key.clone();
        let t1 = tokio::spawn(async move { b1.acquire_lock(&k1, 5000).await.map_err(|e| e.to_string()) });
        let t2 = tokio::spawn(async move { b2.acquire_lock(&k2, 5000).await.map_err(|e| e.to_string()) });
        let (r1, r2) = tokio::join!(t1, t2);
        let r1 = r1.expect("task 1 panicked").expect("task 1 returned Err");
        let r2 = r2.expect("task 2 panicked").expect("task 2 returned Err");
        let winners = [r1.is_some(), r2.is_some()].iter().filter(|&&w| w).count();
        assert_eq!(winners, 1, "Exactly one task must win the lock; got {}", winners);
        cleanup(backend.client(), &key).await;
    }
}

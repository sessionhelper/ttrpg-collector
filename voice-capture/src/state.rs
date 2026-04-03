use std::collections::{HashMap, HashSet};
use std::sync::Arc;

use tokio::sync::Mutex;
use tokio::task::JoinHandle;

use crate::config::Config;
use crate::consent::ConsentManager;
use crate::storage::{S3Uploader, SessionBundle};

pub struct AppState {
    pub config: Config,
    pub consent: Arc<Mutex<ConsentManager>>,
    pub bundles: Arc<Mutex<HashMap<u64, SessionBundle>>>,
    pub ssrc_maps: Arc<Mutex<HashMap<u64, Arc<Mutex<HashMap<u32, u64>>>>>>,
    pub consented_users: Arc<Mutex<HashMap<u64, Arc<Mutex<HashSet<u64>>>>>>,
    pub s3: Arc<S3Uploader>,
    pub db: sqlx::PgPool,
    /// Per-guild audio handles for clean shutdown on /stop.
    pub audio_handles: Arc<Mutex<HashMap<u64, crate::voice::AudioHandle>>>,
    /// Cleanup tasks for ephemeral license button messages (per guild).
    /// Aborted on session end so stale messages get cleaned up immediately.
    pub license_cleanup_tasks: Arc<Mutex<HashMap<u64, Vec<JoinHandle<()>>>>>,
}

impl AppState {
    pub fn new(config: Config, db: sqlx::PgPool) -> Self {
        let s3 = Arc::new(S3Uploader::new(&config));
        Self {
            config,
            consent: Arc::new(Mutex::new(ConsentManager::new())),
            bundles: Arc::new(Mutex::new(HashMap::new())),
            ssrc_maps: Arc::new(Mutex::new(HashMap::new())),
            consented_users: Arc::new(Mutex::new(HashMap::new())),
            s3,
            db,
            license_cleanup_tasks: Arc::new(Mutex::new(HashMap::new())),
            audio_handles: Arc::new(Mutex::new(HashMap::new())),
        }
    }

    /// Abort all pending license cleanup tasks for a guild.
    pub async fn abort_license_cleanups(&self, guild_id: u64) {
        let mut tasks = self.license_cleanup_tasks.lock().await;
        if let Some(handles) = tasks.remove(&guild_id) {
            for handle in handles {
                handle.abort();
            }
        }
    }
}

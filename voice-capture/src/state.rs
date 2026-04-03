use std::sync::Arc;

use tokio::sync::Mutex;

use crate::config::Config;
use crate::session::SessionManager;
use crate::storage::S3Uploader;

pub struct AppState {
    pub config: Config,
    pub sessions: Arc<Mutex<SessionManager>>,
    pub s3: Arc<S3Uploader>,
    pub db: sqlx::PgPool,
}

impl AppState {
    pub fn new(config: Config, db: sqlx::PgPool) -> Self {
        let s3 = Arc::new(S3Uploader::new(&config));
        Self {
            config,
            sessions: Arc::new(Mutex::new(SessionManager::new())),
            s3,
            db,
        }
    }
}

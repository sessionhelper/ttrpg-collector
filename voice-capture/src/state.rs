use std::collections::{HashMap, HashSet};
use std::sync::Arc;

use tokio::sync::Mutex;

use crate::config::Config;
use crate::consent::ConsentManager;
use crate::storage::{S3Uploader, SessionBundle};

pub struct AppState {
    pub config: Config,
    pub consent: Arc<Mutex<ConsentManager>>,
    pub bundles: Arc<Mutex<HashMap<u64, SessionBundle>>>,
    pub ssrc_maps: Arc<Mutex<HashMap<u64, Arc<Mutex<HashMap<u32, u64>>>>>>,
    pub consented_users: Arc<Mutex<HashMap<u64, Arc<Mutex<HashSet<u64>>>>>>,
    pub s3: S3Uploader,
}

impl AppState {
    pub fn new(config: Config) -> Self {
        let s3 = S3Uploader::new(&config);
        Self {
            config,
            consent: Arc::new(Mutex::new(ConsentManager::new())),
            bundles: Arc::new(Mutex::new(HashMap::new())),
            ssrc_maps: Arc::new(Mutex::new(HashMap::new())),
            consented_users: Arc::new(Mutex::new(HashMap::new())),
            s3,
        }
    }
}

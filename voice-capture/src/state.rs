//! Global shared application state.

use std::sync::Arc;

use tokio::sync::Mutex;

use crate::api_client::DataApiClient;
use crate::config::Config;
use crate::session::SessionManager;

/// Shared state injected into all event handlers via `Arc<AppState>`.
pub struct AppState {
    pub config: Config,
    pub sessions: Arc<Mutex<SessionManager>>,
    pub api: Arc<DataApiClient>,
}

impl AppState {
    /// Build a new AppState with the given Data API client.
    pub fn new(config: Config, api: DataApiClient) -> Self {
        Self {
            config,
            sessions: Arc::new(Mutex::new(SessionManager::new())),
            api: Arc::new(api),
        }
    }
}

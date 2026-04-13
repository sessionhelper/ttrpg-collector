//! Global shared application state.

use std::sync::Arc;

use dashmap::DashMap;
use serenity::all::Context;
use tokio::sync::OnceCell;

use crate::api_client::DataApiClient;
use crate::config::Config;
use crate::session::actor::SessionHandle;
use crate::session::restart::RestartBudget;

/// Shared state injected into event handlers and the harness.
pub struct AppState {
    pub config: Config,
    /// Per-guild actor handles. Lock-free shard-per-key lookups.
    pub sessions: Arc<DashMap<u64, SessionHandle>>,
    pub api: Arc<DataApiClient>,
    /// Serenity `Context`, populated once in `Handler::ready`. The harness
    /// uses this to reach the songbird manager and HTTP client.
    pub ctx: Arc<OnceCell<Context>>,
    /// Per-guild catastrophic-restart budget tracker (1 restart/hour).
    pub restart_budget: Arc<RestartBudget>,
}

impl AppState {
    pub fn new(config: Config, api: DataApiClient) -> Self {
        Self {
            config,
            sessions: Arc::new(DashMap::new()),
            api: Arc::new(api),
            ctx: Arc::new(OnceCell::new()),
            restart_budget: Arc::new(RestartBudget::new()),
        }
    }
}

//! Global shared application state.

use std::sync::Arc;

use dashmap::DashMap;
use serenity::all::Context;
use tokio::sync::OnceCell;

use crate::api_client::DataApiClient;
use crate::config::Config;
use crate::session::actor::SessionHandle;

/// Shared state injected into all event handlers via `Arc<AppState>`.
pub struct AppState {
    pub config: Config,
    /// Per-guild actor handles. `DashMap` gives lock-free per-key access:
    /// inserting a new session for guild A never blocks a concurrent lookup
    /// for guild B, and the actors themselves never touch this map except
    /// to remove their own entry on exit.
    pub sessions: Arc<DashMap<u64, SessionHandle>>,
    pub api: Arc<DataApiClient>,
    /// Serenity Context populated on `Handler::ready` so non-event-handler
    /// code paths (specifically the dev-only E2E harness HTTP endpoint) can
    /// access the gateway cache, songbird manager, and HTTP client.
    ///
    /// `None` before the bot is ready; `Some(ctx)` after the first `ready`
    /// event fires. The harness blocks its incoming requests until this
    /// `OnceCell` is populated.
    pub ctx: Arc<OnceCell<Context>>,
}

impl AppState {
    /// Build a new AppState with the given Data API client.
    pub fn new(config: Config, api: DataApiClient) -> Self {
        Self {
            config,
            sessions: Arc::new(DashMap::new()),
            api: Arc::new(api),
            ctx: Arc::new(OnceCell::new()),
        }
    }
}

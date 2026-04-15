//! Per-participant consent record + state-machine labels.

use chrono::{DateTime, Utc};
use serenity::all::UserId;

use super::ConsentScope;

/// Per-participant consent record.
#[derive(Debug, Clone)]
pub struct ParticipantConsent {
    pub user_id: UserId,
    pub display_name: String,
    pub scope: Option<ConsentScope>,
    pub consented_at: Option<DateTime<Utc>>,
    pub mid_session_join: bool,
    pub participant_uuid: Option<uuid::Uuid>,
    pub state: ParticipantState,
}

impl ParticipantConsent {
    pub fn new(user_id: UserId, display_name: String, mid_session_join: bool) -> Self {
        Self {
            user_id,
            display_name,
            scope: None,
            consented_at: None,
            mid_session_join,
            participant_uuid: None,
            state: ParticipantState::Pending,
        }
    }
}

/// Per-participant state machine:
///
/// ```text
///   Pending
///     │
///     ├──(Accept click)──▶ Accepted
///     └──(Decline click or session end without response)──▶ Declined
/// ```
///
/// Accepted means "cached chunks have flushed (or will flush) and live chunks
/// go direct-to-api." Declined means "cache deleted, pipeline dropped."
/// Pending means "chunks cache to disk awaiting a click."
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ParticipantState {
    Pending,
    Accepted,
    Declined,
}

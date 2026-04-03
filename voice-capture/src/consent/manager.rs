use std::collections::HashMap;

use chrono::{DateTime, Utc};
use serenity::all::{ChannelId, MessageId, UserId};

#[derive(Debug, Clone, Copy, PartialEq)]
pub enum SessionState {
    AwaitingConsent,
    Recording,
    Finalizing,
    Complete,
    Cancelled,
}

#[derive(Debug, Clone, Copy, PartialEq, serde::Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ConsentScope {
    Full,
    DeclineAudio,
    Decline,
}

#[derive(Debug, Clone)]
pub struct ParticipantConsent {
    pub user_id: UserId,
    pub display_name: String,
    pub scope: Option<ConsentScope>,
    pub consented_at: Option<DateTime<Utc>>,
    pub mid_session_join: bool,
}

#[derive(Debug)]
pub struct ConsentSession {
    pub guild_id: u64,
    pub channel_id: u64,
    pub text_channel_id: u64,
    pub initiator_id: UserId,
    pub session_id: String,
    pub state: SessionState,
    pub participants: HashMap<UserId, ParticipantConsent>,
    pub min_participants: usize,
    pub require_all: bool,
    /// Message ID of the consent embed, so we can clean it up on session end
    pub consent_message: Option<(ChannelId, MessageId)>,
    /// Interaction tokens + message IDs for ephemeral license followups, so we can edit them on session end
    pub license_followups: Vec<(String, MessageId)>,
}

impl ConsentSession {
    pub fn new(
        guild_id: u64,
        channel_id: u64,
        text_channel_id: u64,
        initiator_id: UserId,
        session_id: String,
        min_participants: usize,
        require_all: bool,
    ) -> Self {
        Self {
            guild_id,
            channel_id,
            text_channel_id,
            initiator_id,
            session_id,
            state: SessionState::AwaitingConsent,
            participants: HashMap::new(),
            min_participants,
            require_all,
            consent_message: None,
            license_followups: Vec::new(),
        }
    }

    pub fn add_participant(&mut self, user_id: UserId, display_name: String, mid_session: bool) {
        self.participants.entry(user_id).or_insert(ParticipantConsent {
            user_id,
            display_name,
            scope: None,
            consented_at: None,
            mid_session_join: mid_session,
        });
    }

    pub fn record_consent(&mut self, user_id: UserId, scope: ConsentScope) {
        if let Some(p) = self.participants.get_mut(&user_id) {
            p.scope = Some(scope);
            p.consented_at = Some(Utc::now());
        }
    }

    pub fn remove_pending(&mut self, user_id: UserId) {
        if let Some(p) = self.participants.get(&user_id) {
            if p.scope.is_none() {
                self.participants.remove(&user_id);
            }
        }
    }

    pub fn all_responded(&self) -> bool {
        self.participants.values().all(|p| p.scope.is_some())
    }

    pub fn has_decline(&self) -> bool {
        self.participants
            .values()
            .any(|p| p.scope == Some(ConsentScope::Decline))
    }

    pub fn consented_user_ids(&self) -> Vec<UserId> {
        self.participants
            .values()
            .filter(|p| p.scope == Some(ConsentScope::Full))
            .map(|p| p.user_id)
            .collect()
    }

    pub fn pending_user_ids(&self) -> Vec<UserId> {
        self.participants
            .values()
            .filter(|p| p.scope.is_none())
            .map(|p| p.user_id)
            .collect()
    }

    pub fn evaluate_quorum(&self) -> bool {
        if self.require_all && self.has_decline() {
            return false;
        }
        self.consented_user_ids().len() >= self.min_participants
    }
}

/// Manages consent sessions across guilds. One active session per guild.
pub struct ConsentManager {
    sessions: HashMap<u64, ConsentSession>,
}

impl ConsentManager {
    pub fn new() -> Self {
        Self {
            sessions: HashMap::new(),
        }
    }

    pub fn create_session(&mut self, session: ConsentSession) {
        self.sessions.insert(session.guild_id, session);
    }

    pub fn get_session(&self, guild_id: u64) -> Option<&ConsentSession> {
        self.sessions.get(&guild_id)
    }

    pub fn get_session_mut(&mut self, guild_id: u64) -> Option<&mut ConsentSession> {
        self.sessions.get_mut(&guild_id)
    }

    pub fn remove_session(&mut self, guild_id: u64) -> Option<ConsentSession> {
        self.sessions.remove(&guild_id)
    }

    pub fn has_active_session(&self, guild_id: u64) -> bool {
        self.sessions.get(&guild_id).is_some_and(|s| {
            matches!(
                s.state,
                SessionState::AwaitingConsent | SessionState::Recording | SessionState::Finalizing
            )
        })
    }
}

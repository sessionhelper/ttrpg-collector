//! Unified session state.
//!
//! Each active recording session lives inside its own [`actor`] task.
//! External handlers send [`actor::SessionCmd`] messages and await oneshot
//! replies. This module owns the plain data (Session, Phase, ParticipantConsent)
//! and the per-phase helpers (`phases`, `participant`, `stabilization`, `heal`,
//! `restart`). The run loop is in [`actor`].

pub mod actor;
pub mod heal;
pub mod participant;
pub mod phases;
pub mod restart;
pub mod stabilization;

use std::collections::HashMap;

use chrono::{DateTime, Utc};
use serde::Serialize;
use serenity::all::{
    ButtonStyle, ChannelId, CreateActionRow, CreateButton, CreateEmbed, MessageId, UserId,
};

use crate::storage::bundle::{
    AudioFormat, ConsentEntry, ConsentRecord, ParticipantMeta, SessionMeta,
};
use crate::storage::pseudonymize;

pub use participant::{ParticipantConsent, ParticipantState};
pub use phases::Phase;

/// What a participant chose when presented with the consent prompt.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ConsentScope {
    Full,
    Decline,
}

/// A single recording session. One per guild, owns all session state.
///
/// Phase transitions happen in place via the [`phases`] helpers. The audio
/// pipeline is spawned per-participant and torn down via the actor, not
/// directly on this struct.
pub struct Session {
    // Identity
    pub id: String,
    pub guild_id: u64,
    pub phase: Phase,

    // Channel + initiator
    pub channel_id: u64,
    pub text_channel_id: u64,
    pub initiator_id: UserId,

    // Participants + consent records
    pub participants: HashMap<UserId, ParticipantConsent>,

    // Quorum
    pub min_participants: usize,
    pub require_all: bool,

    // Timestamps
    pub started_at: DateTime<Utc>,
    pub ended_at: Option<DateTime<Utc>>,

    // Discord UI refs (for cleanup)
    pub consent_message: Option<(ChannelId, MessageId)>,
}

impl Session {
    pub fn new(
        guild_id: u64,
        channel_id: u64,
        text_channel_id: u64,
        initiator_id: UserId,
        min_participants: usize,
        require_all: bool,
    ) -> Self {
        Self {
            id: uuid::Uuid::new_v4().to_string(),
            guild_id,
            phase: Phase::AwaitingStabilization {
                entered_at: Utc::now(),
            },
            channel_id,
            text_channel_id,
            initiator_id,
            participants: HashMap::new(),
            min_participants,
            require_all,
            started_at: Utc::now(),
            ended_at: None,
            consent_message: None,
        }
    }

    /// Build a session for a catastrophic-restart with a pre-built participant
    /// map carried forward. Consent records transfer as-is: Accepted stays
    /// Accepted, Declined stays Declined, Pending → Decline (the spec's rule).
    pub fn new_carry_forward(
        guild_id: u64,
        channel_id: u64,
        text_channel_id: u64,
        initiator_id: UserId,
        min_participants: usize,
        require_all: bool,
        carried: HashMap<UserId, ParticipantConsent>,
    ) -> Self {
        let mut s = Self::new(
            guild_id,
            channel_id,
            text_channel_id,
            initiator_id,
            min_participants,
            require_all,
        );
        s.participants = carried
            .into_iter()
            .map(|(uid, mut pc)| {
                if pc.scope.is_none() {
                    pc.scope = Some(ConsentScope::Decline);
                    pc.state = ParticipantState::Declined;
                }
                (uid, pc)
            })
            .collect();
        s
    }

    // ------------------------------------------------------------------
    // Participant management
    // ------------------------------------------------------------------

    pub fn add_participant(&mut self, user_id: UserId, display_name: String, mid_session: bool) {
        self.participants
            .entry(user_id)
            .or_insert_with(|| ParticipantConsent::new(user_id, display_name, mid_session));
    }

    pub fn set_participant_uuid(&mut self, user_id: UserId, pid: uuid::Uuid) {
        if let Some(p) = self.participants.get_mut(&user_id) {
            p.participant_uuid = Some(pid);
        }
    }

    pub fn participant_uuid(&self, user_id: UserId) -> Option<uuid::Uuid> {
        self.participants.get(&user_id).and_then(|p| p.participant_uuid)
    }

    pub fn record_consent(&mut self, user_id: UserId, scope: ConsentScope) {
        if let Some(p) = self.participants.get_mut(&user_id) {
            p.scope = Some(scope);
            p.consented_at = Some(Utc::now());
            p.state = match scope {
                ConsentScope::Full => ParticipantState::Accepted,
                ConsentScope::Decline => ParticipantState::Declined,
            };
        }
    }

    // ------------------------------------------------------------------
    // Consent queries
    // ------------------------------------------------------------------

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

    pub fn evaluate_quorum(&self) -> bool {
        if self.require_all && self.has_decline() {
            return false;
        }
        self.consented_user_ids().len() >= self.min_participants
    }

    // ------------------------------------------------------------------
    // UI — consent embed
    // ------------------------------------------------------------------

    pub fn consent_embed(&self) -> CreateEmbed {
        let mut pending = Vec::new();
        let mut accepted = Vec::new();
        let mut declined = Vec::new();
        for p in self.participants.values() {
            match p.scope {
                None => pending.push(p.display_name.as_str()),
                Some(ConsentScope::Full) => accepted.push(p.display_name.as_str()),
                Some(ConsentScope::Decline) => declined.push(p.display_name.as_str()),
            }
        }
        let mut embed = CreateEmbed::new()
            .title("Session Recording — Open Dataset")
            .description(CONSENT_TEXT)
            .color(0x5A5A5A);
        if !pending.is_empty() {
            embed = embed.field("Waiting for", pending.join("\n"), true);
        }
        if !accepted.is_empty() {
            embed = embed.field("Accepted", accepted.join("\n"), true);
        }
        if !declined.is_empty() {
            embed = embed.field("Declined", declined.join("\n"), true);
        }
        embed
    }

    // ------------------------------------------------------------------
    // Metadata serialization
    // ------------------------------------------------------------------

    pub fn meta_json(&self) -> Vec<u8> {
        let duration = self
            .ended_at
            .map(|e| (e - self.started_at).num_milliseconds() as f64 / 1000.0)
            .unwrap_or(0.0);
        let participants: Vec<ParticipantMeta> = self
            .participants
            .values()
            .map(|p| {
                let pseudo = pseudonymize(p.user_id.get());
                ParticipantMeta {
                    pseudo_id: pseudo.clone(),
                    track_file: if p.scope == Some(ConsentScope::Full) {
                        Some(format!("audio/{}/", pseudo))
                    } else {
                        None
                    },
                    consent_scope: p.scope,
                }
            })
            .collect();
        let meta = SessionMeta {
            session_id: self.id.clone(),
            started_at: self.started_at,
            ended_at: self.ended_at,
            duration_seconds: duration,
            game_system: None,
            campaign_name: None,
            session_number: None,
            participant_count: self.participants.len(),
            consented_audio_count: self.consented_user_ids().len(),
            collector_version: env!("CARGO_PKG_VERSION").to_string(),
            audio_format: AudioFormat {
                sample_rate: 48000,
                bit_depth: 16,
                channels: 2,
                codec: "pcm_s16le".to_string(),
                container: "raw".to_string(),
            },
            participants,
        };
        serde_json::to_string_pretty(&meta)
            .expect("serialize meta")
            .into_bytes()
    }

    pub fn consent_json(&self) -> Vec<u8> {
        let mut participants = std::collections::HashMap::new();
        for p in self.participants.values() {
            let pseudo = pseudonymize(p.user_id.get());
            participants.insert(
                pseudo,
                ConsentEntry {
                    consented_at: p.consented_at.map(|t| t.to_rfc3339()),
                    scope: p.scope,
                    audio_release: p.scope == Some(ConsentScope::Full),
                    mid_session_join: p.mid_session_join,
                },
            );
        }
        let record = ConsentRecord {
            session_id: self.id.clone(),
            consent_version: "1.0".to_string(),
            license: "CC BY-SA 4.0".to_string(),
            participants,
        };
        serde_json::to_string_pretty(&record)
            .expect("serialize consent")
            .into_bytes()
    }
}

const CONSENT_TEXT: &str = "\
This session will be recorded for the **Open Voice Project**.\n\n\
Audio capture starts immediately; chunks are cached locally until you respond.\n\n\
On Accept your cached chunks flush to storage. On Decline they're deleted.";

pub fn consent_buttons() -> CreateActionRow {
    CreateActionRow::Buttons(vec![
        CreateButton::new("consent_accept")
            .label("Accept")
            .style(ButtonStyle::Success),
        CreateButton::new("consent_decline")
            .label("Decline")
            .style(ButtonStyle::Danger),
    ])
}

#[cfg(test)]
mod tests {
    use super::*;
    use serenity::all::UserId;

    fn user(id: u64) -> UserId {
        UserId::new(id)
    }

    fn session_with_three(min: usize, require_all: bool) -> Session {
        let mut s = Session::new(111, 222, 333, user(1), min, require_all);
        s.add_participant(user(1), "alice".into(), false);
        s.add_participant(user(2), "bob".into(), false);
        s.add_participant(user(3), "carol".into(), false);
        s
    }

    #[test]
    fn add_participant_is_idempotent() {
        let mut s = session_with_three(2, true);
        let before = s.participants.len();
        s.add_participant(user(1), "renamed".into(), false);
        assert_eq!(s.participants.len(), before);
        assert_eq!(s.participants[&user(1)].display_name, "alice");
    }

    #[test]
    fn record_consent_moves_participant_state_forward() {
        let mut s = session_with_three(2, true);
        s.record_consent(user(1), ConsentScope::Full);
        assert_eq!(s.participants[&user(1)].state, ParticipantState::Accepted);
        s.record_consent(user(2), ConsentScope::Decline);
        assert_eq!(s.participants[&user(2)].state, ParticipantState::Declined);
    }

    #[test]
    fn quorum_passes_when_min_accepts_and_no_decline() {
        let mut s = session_with_three(2, true);
        s.record_consent(user(1), ConsentScope::Full);
        s.record_consent(user(2), ConsentScope::Full);
        s.record_consent(user(3), ConsentScope::Full);
        assert!(s.evaluate_quorum());
    }

    #[test]
    fn quorum_fails_when_require_all_and_someone_declines() {
        let mut s = session_with_three(2, true);
        s.record_consent(user(1), ConsentScope::Full);
        s.record_consent(user(2), ConsentScope::Full);
        s.record_consent(user(3), ConsentScope::Decline);
        assert!(!s.evaluate_quorum());
    }

    #[test]
    fn carry_forward_promotes_pending_to_decline() {
        let mut s = session_with_three(2, true);
        s.record_consent(user(1), ConsentScope::Full);
        s.record_consent(user(2), ConsentScope::Decline);
        // user(3) stays pending.
        let carried = s.participants.clone();
        let s2 = Session::new_carry_forward(111, 222, 333, user(1), 2, true, carried);
        assert_eq!(s2.participants[&user(1)].state, ParticipantState::Accepted);
        assert_eq!(s2.participants[&user(2)].state, ParticipantState::Declined);
        assert_eq!(s2.participants[&user(3)].state, ParticipantState::Declined);
    }

    #[test]
    fn meta_json_round_trips_to_valid_json() {
        let s = session_with_three(2, true);
        let bytes = s.meta_json();
        let parsed: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(parsed["session_id"], s.id);
        assert_eq!(parsed["participant_count"], 3);
    }

    #[test]
    fn consent_json_encodes_pseudonymous_participants() {
        let mut s = session_with_three(2, true);
        s.record_consent(user(1), ConsentScope::Full);
        s.record_consent(user(2), ConsentScope::Decline);
        let bytes = s.consent_json();
        let parsed: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(parsed["license"], "CC BY-SA 4.0");
        let participants = parsed["participants"].as_object().unwrap();
        assert_eq!(participants.len(), 3);
        for k in participants.keys() {
            assert_eq!(k.len(), 16);
        }
    }
}

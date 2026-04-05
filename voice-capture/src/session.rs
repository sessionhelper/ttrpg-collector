//! Unified session state machine.
//!
//! All per-session data lives here in one struct, with state transitions
//! enforced via enum variants. No more scattered HashMaps in AppState.

use std::collections::{HashMap, HashSet};
use std::sync::{Arc, Mutex as StdMutex};

use chrono::{DateTime, Utc};
use serde::Serialize;
use serenity::all::{ChannelId, MessageId, UserId};
use tokio::sync::{mpsc, Mutex};
use tokio::task::JoinHandle;

use serenity::all::{CreateEmbed, CreateButton, ButtonStyle, CreateActionRow};

use crate::api_client::DataApiClient;
use crate::voice::{AudioHandle, AudioPacket, AudioReceiver};
use crate::storage::pseudonymize;
use crate::storage::bundle::{
    SessionMeta, AudioFormat, ParticipantMeta, ConsentRecord, ConsentEntry,
};

/// What a participant chose when presented with the consent prompt.
/// This is session-domain data; the serde derive is here so it can be
/// embedded directly in the JSON payloads that ship to S3 via meta.json
/// and consent.json.
#[derive(Debug, Clone, Copy, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ConsentScope {
    /// Participant consented to full audio capture and release.
    Full,
    /// Participant declined all recording.
    Decline,
}

/// Per-participant consent record, tracking who they are and what they chose.
#[derive(Debug, Clone)]
pub struct ParticipantConsent {
    pub user_id: UserId,
    pub display_name: String,
    pub scope: Option<ConsentScope>,
    pub consented_at: Option<DateTime<Utc>>,
    pub mid_session_join: bool,
}

/// All possible session states. Each variant carries only the data relevant to that phase.
pub enum Phase {
    /// Waiting for all participants to accept/decline.
    AwaitingConsent,

    /// Bot is in voice, actively capturing audio.
    Recording {
        audio_tx: mpsc::Sender<AudioPacket>,
        audio_handle: AudioHandle,
        ssrc_map: Arc<StdMutex<HashMap<u32, u64>>>,
        consented_users: Arc<Mutex<HashSet<u64>>>,
    },

    /// /stop called, flushing buffers and uploading metadata.
    Finalizing,

    /// Session complete — kept briefly for cleanup, then removed.
    Complete,
}

/// A single recording session. One per guild, owns all session state.
pub struct Session {
    // Identity
    pub id: String,
    pub guild_id: u64,
    pub phase: Phase,

    // Consent
    pub channel_id: u64,
    pub text_channel_id: u64,
    pub initiator_id: UserId,
    pub participants: HashMap<UserId, ParticipantConsent>,
    pub min_participants: usize,
    pub require_all: bool,

    // Timestamps
    pub started_at: DateTime<Utc>,
    pub ended_at: Option<DateTime<Utc>>,

    // Discord UI references (for cleanup)
    pub consent_message: Option<(ChannelId, MessageId)>,
    pub license_followups: Vec<(String, MessageId)>,
    pub license_cleanup_tasks: Vec<JoinHandle<()>>,

    // Audio config
    pub audio_received: Arc<std::sync::atomic::AtomicBool>,

    /// Cooperative cancellation for the startup pipeline (voice join, DAVE
    /// wait, recording_started). /stop and auto_stop flip this to `true`
    /// before tearing the session down, so a concurrently-running startup
    /// task can check the flag at each await point and bail out cleanly
    /// instead of continuing to mutate a session that's being finalized.
    ///
    /// The flag is tied to this specific Session instance — it is a
    /// separate `Arc` so the startup task can hold a clone that stays
    /// valid even after the session is removed from the manager.
    pub startup_cancelled: Arc<std::sync::atomic::AtomicBool>,
}

impl Session {
    /// Create a new session in AwaitingConsent phase.
    pub fn new(
        guild_id: u64,
        channel_id: u64,
        text_channel_id: u64,
        initiator_id: UserId,
        min_participants: usize,
        require_all: bool,
    ) -> Self {
        let id = uuid::Uuid::new_v4().to_string();
        Self {
            id,
            guild_id,
            phase: Phase::AwaitingConsent,
            channel_id,
            text_channel_id,
            initiator_id,
            participants: HashMap::new(),
            min_participants,
            require_all,
            started_at: Utc::now(),
            ended_at: None,
            consent_message: None,
            license_followups: Vec::new(),
            license_cleanup_tasks: Vec::new(),
            audio_received: Arc::new(std::sync::atomic::AtomicBool::new(false)),
            startup_cancelled: Arc::new(std::sync::atomic::AtomicBool::new(false)),
        }
    }

    // --- Participant management ---

    /// Add a participant (idempotent — skips if already present).
    pub fn add_participant(&mut self, user_id: UserId, display_name: String, mid_session: bool) {
        self.participants.entry(user_id).or_insert(ParticipantConsent {
            user_id,
            display_name,
            scope: None,
            consented_at: None,
            mid_session_join: mid_session,
        });
    }

    /// Record a participant's consent choice.
    pub fn record_consent(&mut self, user_id: UserId, scope: ConsentScope) {
        if let Some(p) = self.participants.get_mut(&user_id) {
            p.scope = Some(scope);
            p.consented_at = Some(Utc::now());
        }
    }

    /// True if every participant has responded (accepted or declined).
    pub fn all_responded(&self) -> bool {
        self.participants.values().all(|p| p.scope.is_some())
    }

    /// True if any participant declined.
    pub fn has_decline(&self) -> bool {
        self.participants.values().any(|p| p.scope == Some(ConsentScope::Decline))
    }

    /// User IDs of participants who gave full consent.
    pub fn consented_user_ids(&self) -> Vec<UserId> {
        self.participants
            .values()
            .filter(|p| p.scope == Some(ConsentScope::Full))
            .map(|p| p.user_id)
            .collect()
    }

    /// Check whether enough participants consented to start recording.
    pub fn evaluate_quorum(&self) -> bool {
        if self.require_all && self.has_decline() {
            return false;
        }
        self.consented_user_ids().len() >= self.min_participants
    }

    // --- Phase transitions ---

    /// Transition from AwaitingConsent -> Recording.
    /// Creates the audio pipeline and returns the channel sender for DAVE retries.
    #[tracing::instrument(skip_all, fields(session_id = %self.id, guild_id = self.guild_id))]
    pub fn start_recording(
        &mut self,
        call: &mut songbird::Call,
        api: Arc<DataApiClient>,
    ) -> mpsc::Sender<AudioPacket> {
        let consented_set: HashSet<u64> = self.consented_user_ids()
            .into_iter()
            .map(|uid| uid.get())
            .collect();
        let consented_users = Arc::new(Mutex::new(consented_set));

        // Parse session UUID for the API client
        let session_uuid = uuid::Uuid::parse_str(&self.id)
            .expect("session id is always a valid UUID");

        // Create pipeline once — single buffer task for the session
        let (audio_tx, audio_handle) = AudioReceiver::create_pipeline(
            api, session_uuid,
        );

        // Attach to the Songbird Call
        let ssrc_map = AudioReceiver::attach(
            call, audio_tx.clone(), consented_users.clone(), self.audio_received.clone(),
        );

        self.phase = Phase::Recording {
            audio_tx: audio_tx.clone(),
            audio_handle,
            ssrc_map,
            consented_users,
        };

        audio_tx
    }

    /// Re-attach audio receiver after a DAVE retry (new Songbird Call).
    /// Reuses the existing pipeline — same channel, same buffer task.
    pub fn reattach_audio(&mut self, call: &mut songbird::Call) {
        if let Phase::Recording { audio_tx, consented_users, ssrc_map, .. } = &mut self.phase {
            let new_ssrc_map = AudioReceiver::attach(
                call, audio_tx.clone(), consented_users.clone(), self.audio_received.clone(),
            );
            *ssrc_map = new_ssrc_map;
        }
    }

    /// Check if DAVE is delivering audio.
    pub fn has_audio(&self) -> bool {
        self.audio_received.load(std::sync::atomic::Ordering::Relaxed)
    }

    /// Check if any SSRC has been mapped (DAVE connection alive, even if silent).
    pub fn has_ssrc(&self) -> bool {
        if let Phase::Recording { ssrc_map, .. } = &self.phase {
            let map = ssrc_map.lock().expect("ssrc_map poisoned");
            !map.is_empty()
        } else {
            false
        }
    }

    /// Transition from Recording -> Finalizing. Shuts down audio pipeline.
    #[tracing::instrument(skip_all, fields(session_id = %self.id))]
    pub async fn finalize(&mut self) {
        self.ended_at = Some(Utc::now());

        // Take the audio handle out of the phase and shut it down
        let old_phase = std::mem::replace(&mut self.phase, Phase::Finalizing);
        if let Phase::Recording { audio_handle, .. } = old_phase {
            audio_handle.shutdown().await;
        }
    }

    /// Mark session as complete.
    pub fn complete(&mut self) {
        self.phase = Phase::Complete;
    }

    // --- Cleanup ---

    /// Abort all pending license button cleanup tasks.
    pub fn abort_license_cleanups(&mut self) {
        for handle in self.license_cleanup_tasks.drain(..) {
            handle.abort();
        }
    }

    /// Full cleanup — abort tasks, mark complete. Call before removing from SessionManager.
    pub async fn cleanup(&mut self) {
        self.abort_license_cleanups();
        // If still recording (e.g. error path), shut down audio
        let old_phase = std::mem::replace(&mut self.phase, Phase::Complete);
        if let Phase::Recording { audio_handle, .. } = old_phase {
            audio_handle.shutdown().await;
        }
    }

    // --- UI (consent embed) ---

    /// Build the Discord embed showing consent status for each participant.
    /// Separates participants into pending/accepted/declined columns.
    pub fn consent_embed(&self) -> CreateEmbed {
        let mut pending = Vec::new();
        let mut accepted = Vec::new();
        let mut declined = Vec::new();

        for p in self.participants.values() {
            match p.scope {
                None => pending.push(p.display_name.as_str()),
                Some(ConsentScope::Full) => accepted.push(p.display_name.as_str()),
                Some(ConsentScope::Decline) => {
                    declined.push(p.display_name.clone())
                }
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
            let declined_strs: Vec<&str> = declined.iter().map(|s| s.as_str()).collect();
            embed = embed.field("Declined", declined_strs.join("\n"), true);
        }

        embed
    }

    // --- Metadata serialization (moved from SessionBundle) ---

    /// Serialize session metadata to JSON bytes for S3 upload.
    /// Contains timing, participant info, and audio format details.
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
            .expect("Failed to serialize meta")
            .into_bytes()
    }

    /// Serialize consent records to JSON bytes for S3 upload.
    /// Each participant is keyed by pseudonymized ID for privacy.
    pub fn consent_json(&self) -> Vec<u8> {
        let mut participants = std::collections::HashMap::new();
        for p in self.participants.values() {
            let pseudo = pseudonymize(p.user_id.get());
            participants.insert(
                pseudo,
                ConsentEntry {
                    consented_at: p.consented_at.map(|t: DateTime<Utc>| t.to_rfc3339()),
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
            .expect("Failed to serialize consent")
            .into_bytes()
    }
}

/// Consent prompt text shown in the embed.
const CONSENT_TEXT: &str = "\
This session will be recorded for the **Open Voice Project**.\n\n\
After accepting, you can set restrictions on how your audio is used.\n\n\
You can decline without leaving the voice channel.";

/// Build the standard consent Accept/Decline button row.
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

/// Manages sessions across guilds. One active session per guild.
pub struct SessionManager {
    sessions: HashMap<u64, Session>,
}

impl SessionManager {
    /// Create an empty session manager.
    pub fn new() -> Self {
        Self { sessions: HashMap::new() }
    }

    /// Atomically reserve a slot for a guild: insert the session if and only
    /// if there is no existing active session for that guild. Returns `Ok(())`
    /// on successful reservation, or `Err(session)` handing the session back
    /// in a Box (Session is ~272 bytes; the Box keeps the Result variant
    /// sizes balanced per clippy::result_large_err).
    ///
    /// This is the only supported insertion path. A raw check-then-insert
    /// sequence was previously racy because the slow work between the two
    /// (Data API calls, Discord response) released the mutex and let a
    /// concurrent /record slip through.
    pub fn try_insert(&mut self, session: Session) -> Result<(), Box<Session>> {
        if self.has_active(session.guild_id) {
            return Err(Box::new(session));
        }
        self.sessions.insert(session.guild_id, session);
        Ok(())
    }

    /// Look up a session by guild ID.
    pub fn get(&self, guild_id: u64) -> Option<&Session> {
        self.sessions.get(&guild_id)
    }

    /// Look up a session mutably by guild ID.
    pub fn get_mut(&mut self, guild_id: u64) -> Option<&mut Session> {
        self.sessions.get_mut(&guild_id)
    }

    /// Remove and return a session by guild ID.
    pub fn remove(&mut self, guild_id: u64) -> Option<Session> {
        self.sessions.remove(&guild_id)
    }

    /// True if the guild has a session in a non-terminal phase.
    pub fn has_active(&self, guild_id: u64) -> bool {
        self.sessions.get(&guild_id).is_some_and(|s| {
            matches!(s.phase, Phase::AwaitingConsent | Phase::Recording { .. } | Phase::Finalizing)
        })
    }
}

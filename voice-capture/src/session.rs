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
use crate::voice::{AudioHandle, AudioPacket, AudioReceiver, Op5Event};
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
    /// Cached Data API participant row UUID, populated at add_participant
    /// time from the server's response. When present, consent / license
    /// click handlers can call the _by_id fast path (1 HTTP round trip)
    /// instead of the 3-hop find_participant fallback
    /// (upsert user + list participants + filter by user_id).
    pub participant_uuid: Option<uuid::Uuid>,
    /// Local mirror of the Data API license flags, updated from batch-add
    /// responses and license-toggle PATCH responses. Lets license button
    /// clicks compute the toggle + patch in a single round trip without
    /// a read-back.
    pub no_llm_training: bool,
    pub no_public_release: bool,
}

/// Fields owned by both `StartingRecording` and `Recording` phases. The audio
/// pipeline is created at the StartingRecording transition and kept alive
/// through to Finalizing — splitting these fields into their own struct lets
/// both variants share the exact same shape without code duplication.
pub struct RecordingPipeline {
    pub audio_tx: mpsc::Sender<AudioPacket>,
    pub audio_handle: AudioHandle,
    pub ssrc_map: Arc<StdMutex<HashMap<u32, u64>>>,
    pub consented_users: Arc<Mutex<HashSet<u64>>>,
    /// SSRCs that have appeared in VoiceTick with decoded audio. The DAVE
    /// heal task checks this to confirm OP5-announced speakers are decoded.
    pub ssrcs_seen: Arc<StdMutex<HashSet<u32>>>,
    /// Receiver for OP5 events from SpeakingTracker. The DAVE heal task
    /// takes ownership of this to detect new speakers and start timers.
    pub op5_rx: Option<mpsc::UnboundedReceiver<Op5Event>>,
}

/// Full session lifecycle.
///
/// ```text
///   AwaitingConsent
///     │
///     │ (quorum met, begin_startup)
///     ▼
///   StartingRecording ─────────────────┐ (/stop or DAVE failed)
///     │                                │
///     │ (DAVE audio confirmed,         ▼
///     │  confirm_recording)         Cancelled ──▶ removed
///     ▼
///   Recording
///     │
///     │ (/stop or auto_stop, finalize)
///     ▼
///   Finalizing
///     │
///     │ (metadata uploaded, complete)
///     ▼
///   Complete ──▶ removed
/// ```
///
/// The key property: a phase transition IS the cancellation mechanism. If
/// /stop fires during StartingRecording, the startup pipeline sees the phase
/// change on its next lock acquisition and bails cleanly. No separate flag,
/// no polling of an AtomicBool, no unwrap() panics when concurrent cleanup
/// removes the session out from under a running task.
pub enum Phase {
    /// Waiting for all participants to accept/decline.
    AwaitingConsent,

    /// Quorum met. Voice channel joined, audio pipeline created, DAVE
    /// handshake in progress. Carries the pipeline even though no audio is
    /// flowing yet — the `audio_received` flag flips to true inside VoiceTick
    /// when DAVE starts delivering decoded frames.
    StartingRecording(RecordingPipeline),

    /// DAVE confirmed. Audio flowing, chunks uploading.
    Recording(RecordingPipeline),

    /// /stop or auto_stop fired during Recording. Metadata is uploading.
    Finalizing,

    /// /stop fired during StartingRecording, or DAVE gave up after retries.
    /// No metadata to upload; Data API row marked `abandoned`.
    Cancelled,

    /// Terminal state. Session about to be removed from the manager.
    Complete,
}

impl Phase {
    /// True if /stop can act on this phase (i.e. a session visibly exists
    /// from the user's perspective).
    pub fn is_stoppable(&self) -> bool {
        matches!(
            self,
            Self::AwaitingConsent | Self::StartingRecording(_) | Self::Recording(_)
        )
    }

    /// True if this phase means the session is on its way out — no further
    /// state mutation should happen except the final remove. Used by
    /// `pipeline_aborted` to detect concurrent /stop preemption.
    pub fn is_terminal(&self) -> bool {
        matches!(self, Self::Finalizing | Self::Cancelled | Self::Complete)
    }
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

    /// Pending auto-stop timer task, if any. Set when the voice channel
    /// becomes empty; aborted on rejoin (rapid leave/rejoin churn) or on
    /// finalization. At most one pending timer per session at a time.
    pub auto_stop_task: Option<JoinHandle<()>>,

    // Audio config
    pub audio_received: Arc<std::sync::atomic::AtomicBool>,
    /// Set to true once the DAVE heal check passes or heal completes.
    /// The harness polls this via GET /status to know when feeders can
    /// start transmitting. In production, the start announcement serves
    /// the same purpose (it plays after heal settles).
    pub recording_stable: Arc<std::sync::atomic::AtomicBool>,
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
            auto_stop_task: None,
            audio_received: Arc::new(std::sync::atomic::AtomicBool::new(false)),
            recording_stable: Arc::new(std::sync::atomic::AtomicBool::new(false)),
        }
    }

    /// Abort a pending auto-stop timer if any. Called when the channel
    /// re-fills (rapid leave/rejoin) and during finalization.
    pub fn abort_auto_stop(&mut self) {
        if let Some(h) = self.auto_stop_task.take() {
            h.abort();
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
            participant_uuid: None,
            no_llm_training: false,
            no_public_release: false,
        });
    }

    /// Update the cached license flags for a participant from a Data API
    /// PATCH response. Called after each license toggle so the next toggle
    /// can be computed locally without a read-back.
    pub fn set_license_flags(
        &mut self,
        user_id: UserId,
        no_llm_training: bool,
        no_public_release: bool,
    ) {
        if let Some(p) = self.participants.get_mut(&user_id) {
            p.no_llm_training = no_llm_training;
            p.no_public_release = no_public_release;
        }
    }

    /// Current cached license flags for a participant, or (false, false)
    /// as the default. Used by the license button click handler to pass
    /// "current" flags into toggle_license_flag_by_id.
    pub fn license_flags(&self, user_id: UserId) -> (bool, bool) {
        self.participants
            .get(&user_id)
            .map(|p| (p.no_llm_training, p.no_public_release))
            .unwrap_or((false, false))
    }

    /// Record a participant's consent choice.
    pub fn record_consent(&mut self, user_id: UserId, scope: ConsentScope) {
        if let Some(p) = self.participants.get_mut(&user_id) {
            p.scope = Some(scope);
            p.consented_at = Some(Utc::now());
        }
    }

    /// Cache the Data API's participant row UUID for a local user. Called
    /// after `add_participants_batch` returns so subsequent consent/license
    /// clicks can skip the find_participant lookup.
    ///
    /// Idempotent: if the user already has a uuid, it's overwritten. If the
    /// user isn't in the participants map, this is a no-op (mirrors
    /// record_consent's tolerance for stale clicks).
    pub fn set_participant_uuid(&mut self, user_id: UserId, participant_uuid: uuid::Uuid) {
        if let Some(p) = self.participants.get_mut(&user_id) {
            p.participant_uuid = Some(participant_uuid);
        }
    }

    /// Look up the cached Data API participant UUID for a local user.
    /// Returns None if the user isn't in the participants map OR if the
    /// cache hasn't been populated yet (e.g. bot restarted mid-session).
    pub fn participant_uuid(&self, user_id: UserId) -> Option<uuid::Uuid> {
        self.participants.get(&user_id).and_then(|p| p.participant_uuid)
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

    /// AwaitingConsent → StartingRecording. Creates the audio pipeline and
    /// joins the Songbird call, but leaves the session in the transient
    /// "starting" phase until DAVE delivers decoded audio. Returns the audio
    /// channel sender for DAVE retry reattachment.
    #[tracing::instrument(skip_all, fields(session_id = %self.id, guild_id = self.guild_id))]
    pub fn begin_startup(
        &mut self,
        call: &mut songbird::Call,
        api: Arc<DataApiClient>,
    ) -> mpsc::Sender<AudioPacket> {
        let consented_set: HashSet<u64> = self
            .consented_user_ids()
            .into_iter()
            .map(|uid| uid.get())
            .collect();
        let consented_users = Arc::new(Mutex::new(consented_set));

        let session_uuid =
            uuid::Uuid::parse_str(&self.id).expect("session id is always a valid UUID");

        let ssrcs_seen = Arc::new(StdMutex::new(HashSet::new()));
        let (op5_tx, op5_rx) = mpsc::unbounded_channel();

        let (audio_tx, audio_handle) = AudioReceiver::create_pipeline(api, session_uuid);
        let ssrc_map = AudioReceiver::attach(
            call,
            audio_tx.clone(),
            consented_users.clone(),
            self.audio_received.clone(),
            ssrcs_seen.clone(),
            op5_tx,
        );

        self.phase = Phase::StartingRecording(RecordingPipeline {
            audio_tx: audio_tx.clone(),
            audio_handle,
            ssrc_map,
            consented_users,
            ssrcs_seen,
            op5_rx: Some(op5_rx),
        });

        audio_tx
    }

    /// StartingRecording → Recording, once DAVE has delivered decoded audio.
    /// Idempotent: if called while already in Recording (e.g. after a retry
    /// path), does nothing. If called in any other phase, does nothing —
    /// the session was preempted between the audio-detected check and here.
    pub fn confirm_recording(&mut self) {
        let placeholder = Phase::AwaitingConsent;
        let current = std::mem::replace(&mut self.phase, placeholder);
        self.phase = match current {
            Phase::StartingRecording(data) => Phase::Recording(data),
            other => other,
        };
    }

    /// Re-attach the audio receiver to a new Songbird Call after a DAVE
    /// retry (leave + rejoin). Reuses the existing pipeline — same channel,
    /// same buffer task. Only valid during StartingRecording; silently
    /// ignored in other phases.
    pub fn reattach_audio(&mut self, call: &mut songbird::Call) {
        let phase = match &mut self.phase {
            Phase::StartingRecording(data) => data,
            Phase::Recording(data) => data,
            _ => return,
        };
        // Create a fresh OP5 channel for the new Call's SpeakingTracker.
        // The old op5_tx is dropped with the old Call's event handlers.
        let (op5_tx, op5_rx) = mpsc::unbounded_channel();
        let new_ssrc_map = AudioReceiver::attach(
            call,
            phase.audio_tx.clone(),
            phase.consented_users.clone(),
            self.audio_received.clone(),
            phase.ssrcs_seen.clone(),
            op5_tx,
        );
        phase.ssrc_map = new_ssrc_map;
        phase.op5_rx = Some(op5_rx);
    }

    /// Check if DAVE is delivering audio. Returns the underlying atomic flag
    /// that VoiceTick flips to true on the first decoded packet.
    pub fn has_audio(&self) -> bool {
        self.audio_received
            .load(std::sync::atomic::Ordering::Relaxed)
    }

    /// True once the DAVE heal check has passed or heal has completed.
    /// Polled by the harness GET /status endpoint.
    pub fn is_stable(&self) -> bool {
        self.recording_stable
            .load(std::sync::atomic::Ordering::Relaxed)
    }

    /// Check if any SSRC has been mapped (DAVE connection alive, even if
    /// silent). Valid during StartingRecording and Recording.
    pub fn has_ssrc(&self) -> bool {
        match &self.phase {
            Phase::StartingRecording(data) | Phase::Recording(data) => {
                let map = data.ssrc_map.lock().expect("ssrc_map poisoned");
                !map.is_empty()
            }
            _ => false,
        }
    }

    /// Recording → Finalizing. Shuts down the audio pipeline and marks the
    /// session as ended. Called by /stop and auto_stop on the happy path.
    ///
    /// If called in StartingRecording (e.g. /stop fires before DAVE confirmed)
    /// this still shuts the pipeline down but the caller should treat the
    /// session as cancelled, not finalized — prefer `cancel_startup` in that
    /// case so metadata upload is skipped.
    #[tracing::instrument(skip_all, fields(session_id = %self.id))]
    pub async fn finalize(&mut self) {
        self.ended_at = Some(Utc::now());
        let old_phase = std::mem::replace(&mut self.phase, Phase::Finalizing);
        match old_phase {
            Phase::Recording(data) | Phase::StartingRecording(data) => {
                data.audio_handle.shutdown().await;
            }
            _ => {}
        }
    }

    /// StartingRecording → Cancelled. Called when /stop fires before DAVE
    /// confirms, or when DAVE gives up after retries. Shuts the pipeline
    /// down but the caller skips metadata upload — there is nothing to
    /// upload.
    #[tracing::instrument(skip_all, fields(session_id = %self.id))]
    pub async fn cancel_startup(&mut self) {
        self.ended_at = Some(Utc::now());
        let old_phase = std::mem::replace(&mut self.phase, Phase::Cancelled);
        if let Phase::StartingRecording(data) = old_phase {
            data.audio_handle.shutdown().await;
        }
    }

    /// Finalizing → Complete. Last step before the session is removed.
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

    /// Abort all background tasks owned by this session: license cleanup
    /// followups and the auto-stop timer. Called during every terminal
    /// transition.
    pub fn abort_all_background_tasks(&mut self) {
        self.abort_license_cleanups();
        self.abort_auto_stop();
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

    /// True if the guild has a session the user can still act on.
    /// Matches every non-terminal phase (AwaitingConsent, StartingRecording,
    /// Recording). Finalizing / Cancelled / Complete are terminal and not
    /// counted as "active" — a new /record is allowed once we're past the
    /// audio capture stage.
    pub fn has_active(&self, guild_id: u64) -> bool {
        self.sessions
            .get(&guild_id)
            .is_some_and(|s| s.phase.is_stoppable())
    }
}

// ---------------------------------------------------------------------------
// Unit tests — pure logic only (no Songbird Call, no Data API client, no
// Discord).
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use serenity::all::UserId;

    fn user(id: u64) -> UserId {
        UserId::new(id)
    }

    /// Build a test session with three pending participants.
    fn session_with_three_participants(min: usize, require_all: bool) -> Session {
        let mut s = Session::new(
            /* guild_id */ 111,
            /* channel_id */ 222,
            /* text_channel_id */ 333,
            user(1),
            min,
            require_all,
        );
        s.add_participant(user(1), "alice".into(), false);
        s.add_participant(user(2), "bob".into(), false);
        s.add_participant(user(3), "carol".into(), false);
        s
    }

    // --- Phase state ---

    #[test]
    fn phase_is_stoppable_for_non_terminal_states() {
        let s = session_with_three_participants(2, true);
        assert!(s.phase.is_stoppable()); // AwaitingConsent
    }

    #[test]
    fn phase_is_terminal_for_finalized_states() {
        assert!(Phase::Finalizing.is_terminal());
        assert!(Phase::Cancelled.is_terminal());
        assert!(Phase::Complete.is_terminal());
    }

    #[test]
    fn phase_awaiting_consent_is_not_terminal() {
        // Regression test: pipeline_aborted in consent.rs uses is_terminal
        // to decide whether the startup pipeline should bail. If
        // AwaitingConsent were mistakenly reported as terminal, the FIRST
        // abort check — which fires BEFORE begin_startup transitions the
        // session to StartingRecording — would return true and every
        // recording would bail immediately after voice_joined. This
        // actually happened in the initial state-machine rework. Keep
        // this assertion to lock in the fix.
        let s = session_with_three_participants(2, true);
        assert!(!s.phase.is_terminal());
    }

    #[test]
    fn phase_is_not_stoppable_for_terminal_states() {
        assert!(!Phase::Finalizing.is_stoppable());
        assert!(!Phase::Cancelled.is_stoppable());
        assert!(!Phase::Complete.is_stoppable());
    }

    // --- add_participant / record_consent ---

    #[test]
    fn add_participant_is_idempotent() {
        let mut s = session_with_three_participants(2, true);
        let before = s.participants.len();
        s.add_participant(user(1), "alice renamed".into(), false);
        assert_eq!(s.participants.len(), before);
        // First write wins — subsequent adds don't overwrite.
        assert_eq!(s.participants[&user(1)].display_name, "alice");
    }

    #[test]
    fn add_participant_defaults_uuid_to_none() {
        let s = session_with_three_participants(2, true);
        assert_eq!(s.participants[&user(1)].participant_uuid, None);
    }

    #[test]
    fn set_participant_uuid_stores_value() {
        let mut s = session_with_three_participants(2, true);
        let pid = uuid::Uuid::new_v4();
        s.set_participant_uuid(user(1), pid);
        assert_eq!(s.participant_uuid(user(1)), Some(pid));
    }

    #[test]
    fn set_participant_uuid_ignores_unknown_user() {
        let mut s = session_with_three_participants(2, true);
        s.set_participant_uuid(user(999), uuid::Uuid::new_v4());
        // Does NOT silently insert a ghost participant row.
        assert_eq!(s.participants.len(), 3);
        assert_eq!(s.participant_uuid(user(999)), None);
    }

    #[test]
    fn participant_uuid_returns_none_for_unknown() {
        let s = session_with_three_participants(2, true);
        assert_eq!(s.participant_uuid(user(42)), None);
    }

    #[test]
    fn license_flags_default_to_false_false() {
        let s = session_with_three_participants(2, true);
        assert_eq!(s.license_flags(user(1)), (false, false));
    }

    #[test]
    fn set_license_flags_round_trips() {
        let mut s = session_with_three_participants(2, true);
        s.set_license_flags(user(1), true, false);
        assert_eq!(s.license_flags(user(1)), (true, false));
        s.set_license_flags(user(1), true, true);
        assert_eq!(s.license_flags(user(1)), (true, true));
        s.set_license_flags(user(1), false, true);
        assert_eq!(s.license_flags(user(1)), (false, true));
    }

    #[test]
    fn set_license_flags_ignores_unknown_user() {
        let mut s = session_with_three_participants(2, true);
        s.set_license_flags(user(999), true, true);
        // No ghost insertion.
        assert_eq!(s.participants.len(), 3);
        assert_eq!(s.license_flags(user(999)), (false, false));
    }

    #[test]
    fn set_participant_uuid_survives_record_consent() {
        // Ordering sanity: caching the UUID then recording consent
        // should leave both fields populated.
        let mut s = session_with_three_participants(2, true);
        let pid = uuid::Uuid::new_v4();
        s.set_participant_uuid(user(1), pid);
        s.record_consent(user(1), ConsentScope::Full);
        let p = &s.participants[&user(1)];
        assert_eq!(p.participant_uuid, Some(pid));
        assert_eq!(p.scope, Some(ConsentScope::Full));
    }

    #[test]
    fn record_consent_sets_scope_and_timestamp() {
        let mut s = session_with_three_participants(2, true);
        s.record_consent(user(1), ConsentScope::Full);
        let p = &s.participants[&user(1)];
        assert_eq!(p.scope, Some(ConsentScope::Full));
        assert!(p.consented_at.is_some());
    }

    #[test]
    fn record_consent_ignores_unknown_user() {
        let mut s = session_with_three_participants(2, true);
        s.record_consent(user(999), ConsentScope::Full);
        // No panic, no new participant.
        assert_eq!(s.participants.len(), 3);
    }

    // --- Response tracking ---

    #[test]
    fn all_responded_false_when_some_pending() {
        let mut s = session_with_three_participants(2, true);
        s.record_consent(user(1), ConsentScope::Full);
        assert!(!s.all_responded());
    }

    #[test]
    fn all_responded_true_when_every_participant_replied() {
        let mut s = session_with_three_participants(2, true);
        s.record_consent(user(1), ConsentScope::Full);
        s.record_consent(user(2), ConsentScope::Full);
        s.record_consent(user(3), ConsentScope::Decline);
        assert!(s.all_responded());
    }

    #[test]
    fn has_decline_flags_any_decliner() {
        let mut s = session_with_three_participants(2, true);
        assert!(!s.has_decline());
        s.record_consent(user(1), ConsentScope::Full);
        assert!(!s.has_decline());
        s.record_consent(user(2), ConsentScope::Decline);
        assert!(s.has_decline());
    }

    #[test]
    fn consented_user_ids_lists_only_full_scope() {
        let mut s = session_with_three_participants(2, true);
        s.record_consent(user(1), ConsentScope::Full);
        s.record_consent(user(2), ConsentScope::Decline);
        s.record_consent(user(3), ConsentScope::Full);
        let ids = s.consented_user_ids();
        assert_eq!(ids.len(), 2);
        assert!(ids.contains(&user(1)));
        assert!(ids.contains(&user(3)));
    }

    // --- Quorum evaluation ---

    #[test]
    fn quorum_passes_when_min_accepts_and_no_decline() {
        let mut s = session_with_three_participants(2, true);
        s.record_consent(user(1), ConsentScope::Full);
        s.record_consent(user(2), ConsentScope::Full);
        s.record_consent(user(3), ConsentScope::Full);
        assert!(s.evaluate_quorum());
    }

    #[test]
    fn quorum_fails_when_require_all_and_someone_declines() {
        let mut s = session_with_three_participants(2, true);
        s.record_consent(user(1), ConsentScope::Full);
        s.record_consent(user(2), ConsentScope::Full);
        s.record_consent(user(3), ConsentScope::Decline);
        assert!(!s.evaluate_quorum());
    }

    #[test]
    fn quorum_passes_when_not_require_all_and_min_met() {
        let mut s = session_with_three_participants(2, false);
        s.record_consent(user(1), ConsentScope::Full);
        s.record_consent(user(2), ConsentScope::Full);
        s.record_consent(user(3), ConsentScope::Decline);
        assert!(s.evaluate_quorum());
    }

    #[test]
    fn quorum_fails_when_below_min_even_without_declines() {
        let mut s = session_with_three_participants(3, false);
        s.record_consent(user(1), ConsentScope::Full);
        s.record_consent(user(2), ConsentScope::Full);
        assert!(!s.evaluate_quorum());
    }

    // --- Metadata serialization ---

    #[test]
    fn meta_json_round_trips_to_valid_json() {
        let s = session_with_three_participants(2, true);
        let bytes = s.meta_json();
        let parsed: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(parsed["session_id"], s.id);
        assert_eq!(parsed["participant_count"], 3);
        assert_eq!(parsed["audio_format"]["sample_rate"], 48000);
        assert_eq!(parsed["audio_format"]["bit_depth"], 16);
    }

    #[test]
    fn consent_json_includes_every_participant_keyed_by_pseudo() {
        let mut s = session_with_three_participants(2, true);
        s.record_consent(user(1), ConsentScope::Full);
        s.record_consent(user(2), ConsentScope::Decline);
        let bytes = s.consent_json();
        let parsed: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(parsed["license"], "CC BY-SA 4.0");
        let participants = parsed["participants"].as_object().unwrap();
        assert_eq!(participants.len(), 3);
        // Every key is a 16-char hex pseudonym
        for k in participants.keys() {
            assert_eq!(k.len(), 16);
            assert!(k.chars().all(|c| c.is_ascii_hexdigit()));
        }
    }

    #[test]
    fn meta_json_audio_count_reflects_full_consent_only() {
        let mut s = session_with_three_participants(2, true);
        s.record_consent(user(1), ConsentScope::Full);
        s.record_consent(user(2), ConsentScope::Full);
        s.record_consent(user(3), ConsentScope::Decline);
        let bytes = s.meta_json();
        let parsed: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(parsed["consented_audio_count"], 2);
        assert_eq!(parsed["participant_count"], 3);
    }

    // --- SessionManager atomicity ---
    //
    // Session isn't Debug (carries non-Debug songbird/tokio handles), so
    // `.unwrap()` / `.unwrap_err()` on try_insert's Result<(), Box<Session>>
    // won't compile. We use an explicit `if is_err { panic }` helper and
    // match for the Err-inspecting test.

    fn try_insert_or_panic(mgr: &mut SessionManager, s: Session) {
        if mgr.try_insert(s).is_err() {
            panic!("try_insert failed unexpectedly");
        }
    }

    #[test]
    fn session_manager_try_insert_ok_when_empty() {
        let mut mgr = SessionManager::new();
        try_insert_or_panic(&mut mgr, session_with_three_participants(2, true));
        assert!(mgr.has_active(111));
    }

    #[test]
    fn session_manager_try_insert_rejects_duplicate_guild() {
        let mut mgr = SessionManager::new();
        try_insert_or_panic(&mut mgr, session_with_three_participants(2, true));
        let second = session_with_three_participants(2, true);
        let second_id = second.id.clone();
        match mgr.try_insert(second) {
            Err(rejected) => assert_eq!(rejected.id, second_id),
            Ok(()) => panic!("expected try_insert to reject duplicate guild"),
        }
    }

    #[test]
    fn session_manager_try_insert_ok_after_remove() {
        let mut mgr = SessionManager::new();
        try_insert_or_panic(&mut mgr, session_with_three_participants(2, true));
        mgr.remove(111);
        assert!(!mgr.has_active(111));
        try_insert_or_panic(&mut mgr, session_with_three_participants(2, true));
    }

    #[test]
    fn session_manager_has_active_false_for_empty() {
        let mgr = SessionManager::new();
        assert!(!mgr.has_active(111));
    }
}

//! Per-session actor.
//!
//! Runs as its own `tokio::spawn`ed task, owns its `Session` exclusively.
//! External handlers send [`SessionCmd`]s over an mpsc and await oneshot
//! replies.
//!
//! Two-phase capture flow (F2, F6, F8):
//!
//! 1. **Pre-gate** (`AwaitingStabilization`): every participant's voice
//!    frames land in a disk cache keyed by `(session_id, pseudo_id)`. The
//!    mix track is NOT computed incrementally. No data-api traffic at all.
//! 2. **Gate-open**: the actor
//!    - runs blocklist checks for the enrolled participants,
//!    - creates the `chronicle-data-api` session row,
//!    - batch-inserts surviving (non-Declined) participants,
//!    - deletes Declined participants' per-speaker caches,
//!    - renders a one-shot mix from the surviving per-speaker caches,
//!    - flushes every cache (per-speaker + mix) to the data-api concurrently,
//!    - switches per-speaker pipelines from CACHE to DIRECT mode,
//!    - spawns the live mix task.
//! 3. **Post-gate** (`Recording`): per-speaker pipelines upload direct;
//!    the live mix task tick-mixes accepted speakers and uploads rollover
//!    chunks. Late Accept after gate makes the speaker's backlog flush at
//!    Accept time (not the gate), and adds them to the live mix from
//!    Accept forward.
//!
//! The spec's gate-open invariant is sharp: if the gate never opens
//! (timeout, Decline-triggered quorum failure, /stop before gate), the
//! data-api is never told this session existed. The whole session cache
//! dir is deleted; no abandon-row is written.

use std::collections::{HashMap, HashSet};
use std::sync::{Arc, Mutex as StdMutex};
use std::time::Duration;

use chrono::{DateTime, TimeZone, Utc};
use dashmap::DashMap;
use serenity::all::*;
use tokio::sync::{mpsc, oneshot};
use tokio::task::{JoinHandle, JoinSet};
use tokio_util::sync::CancellationToken;
use tracing::{error, info, warn, Instrument};

use crate::api_client::DataApiClient;
use crate::session::participant::ParticipantState;
use crate::session::phases::Phase;
use crate::session::restart::carry_forward;
use crate::session::stabilization::{self, GateInputs, StreakStatus, StreakTracker};
use crate::session::{consent_buttons, ConsentScope, Session};
use crate::state::AppState;
use crate::storage::pseudonymize;
use crate::voice::buffer::{
    self, pcm_bytes_to_duration_ms, BufferRoot, FlushError, ParticipantCache, MIXED_PSEUDO_ID,
};
use crate::voice::mixer::{
    render_mix_from_caches, write_mix_chunks_to_cache, LiveMixAccum, MixChunk, PerSpeakerSource,
    SourceChunk,
};
use crate::voice::{AudioHandle, AudioObservables, AudioPacket, AudioReceiver, PacketSink};

// ---------------------------------------------------------------------------
// Errors + command types
// ---------------------------------------------------------------------------

#[derive(Debug, thiserror::Error)]
pub enum SessionError {
    #[error("user is not a participant in this session")]
    NotParticipant,
    #[error("user has already responded")]
    AlreadyResponded,
    #[error("only the initiator can stop the session")]
    NotInitiator,
    #[error("session is not in a stoppable state")]
    NotStoppable,
    #[error("session actor is gone")]
    ActorGone,
}

#[derive(Debug, Clone, Copy)]
pub enum LicenseField {
    NoLlmTraining,
    NoPublicRelease,
}

#[derive(Debug)]
pub enum ConsentOutcome {
    Ack { embed: CreateEmbed },
    QuorumFailed { embed: CreateEmbed },
}

#[derive(Debug, Clone)]
#[allow(dead_code)]
pub struct SessionSnapshot {
    pub session_id: String,
    pub guild_id: u64,
    pub recording: bool,
    pub stable: bool,
    pub phase_label: &'static str,
    pub participant_count: usize,
}

#[allow(dead_code)]
pub enum SessionCmd {
    RecordConsent {
        user: UserId,
        scope: ConsentScope,
        reply: oneshot::Sender<Result<ConsentOutcome, SessionError>>,
    },
    SetLicense {
        user: UserId,
        field: LicenseField,
        value: bool,
        reply: oneshot::Sender<Result<(), SessionError>>,
    },
    Stop {
        initiator: UserId,
        reply: oneshot::Sender<Result<(), SessionError>>,
    },
    AutoStop,
    VoiceStateChange {
        old_channel: Option<ChannelId>,
        new_channel: Option<ChannelId>,
        user_id: UserId,
        is_bot: bool,
        display_name: String,
    },
    SetConsentMessage {
        channel_id: ChannelId,
        message_id: MessageId,
    },
    /// Harness `POST /enrol`: add a participant after session spawn.
    Enrol {
        user_id: UserId,
        display_name: String,
        is_bot: bool,
        reply: oneshot::Sender<Result<uuid::Uuid, SessionError>>,
    },
    GetSnapshot {
        reply: oneshot::Sender<SessionSnapshot>,
    },
}

// ---------------------------------------------------------------------------
// Handle
// ---------------------------------------------------------------------------

#[derive(Clone)]
#[allow(dead_code)]
pub struct SessionHandle {
    tx: mpsc::Sender<SessionCmd>,
    pub session_id: String,
    pub shutting_down: Arc<std::sync::atomic::AtomicBool>,
}

impl SessionHandle {
    pub async fn send(&self, cmd: SessionCmd) -> Result<(), SessionError> {
        self.tx.send(cmd).await.map_err(|_| SessionError::ActorGone)
    }

    pub fn is_shutting_down(&self) -> bool {
        self.shutting_down
            .load(std::sync::atomic::Ordering::Relaxed)
    }
}

pub async fn request<T, F>(handle: &SessionHandle, build: F) -> Result<T, SessionError>
where
    F: FnOnce(oneshot::Sender<T>) -> SessionCmd,
{
    let (tx, rx) = oneshot::channel();
    handle.send(build(tx)).await?;
    rx.await.map_err(|_| SessionError::ActorGone)
}

// ---------------------------------------------------------------------------
// Spawn
// ---------------------------------------------------------------------------

pub fn spawn_session(
    state: Arc<AppState>,
    ctx: Context,
    session: Session,
) -> Result<SessionHandle, Box<Session>> {
    let guild_id = session.guild_id;
    let session_id = session.id.clone();
    let (tx, rx) = mpsc::channel::<SessionCmd>(64);
    let shutting_down = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let handle = SessionHandle {
        tx,
        session_id: session_id.clone(),
        shutting_down: shutting_down.clone(),
    };

    match state.sessions.entry(guild_id) {
        dashmap::mapref::entry::Entry::Occupied(_) => return Err(Box::new(session)),
        dashmap::mapref::entry::Entry::Vacant(vac) => {
            vac.insert(handle.clone());
        }
    }

    let span = tracing::info_span!(
        "session_actor",
        session_id = %session_id,
        guild_id = guild_id,
    );
    let cancel = CancellationToken::new();
    tokio::spawn(run_actor(state, ctx, session, rx, shutting_down, cancel).instrument(span));
    Ok(handle)
}

// ---------------------------------------------------------------------------
// Per-participant capture task
// ---------------------------------------------------------------------------

pub enum ParticipantCmd {
    Packet(AudioPacket),
    /// Pre-gate: drop this participant's cache; no upload. Used when the
    /// participant Declines before gate-open.
    DropCache,
    /// Gate opened: participant survived (Accepted or still Pending). From
    /// this command forward we ignore the pre-gate cache dir and either
    /// upload directly (Accepted) or keep caching post-gate (Pending).
    GateOpened {
        accepted: bool,
        session_uuid: uuid::Uuid,
        mix_tx: Option<mpsc::Sender<MixerCmd>>,
    },
    /// Post-gate late accept: flush the post-gate cache (if any) + switch
    /// to direct upload + add to live mix.
    LateAccept,
    /// Post-gate late decline: drop the post-gate cache + drop pipeline.
    LateDecline,
    /// Cancellation mid-flight.
    Shutdown,
}

const CHUNK_SIZE_BYTES: usize = 2 * 1024 * 1024;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum PMode {
    /// Pre-gate: caching to disk. Every participant sits here until the gate.
    Cache,
    /// Post-gate Accepted: direct-upload each rollover chunk + feed the mix.
    Direct,
    /// Post-gate still Pending: keep caching to disk (new post-gate dir
    /// separate from the pre-gate one).
    PendingPostGate,
    /// Declined or gate failed: drop everything.
    Dropping,
}

struct ParticipantChannel {
    tx: mpsc::Sender<ParticipantCmd>,
    join: JoinHandle<()>,
    pseudo_id: String,
}

struct ParticipantCtx {
    session_id: String,
    pseudo_id: String,
    user_id: UserId,
    buffer_root: BufferRoot,
    api: Arc<DataApiClient>,
    cancel: CancellationToken,
}

fn spawn_participant_task(pctx: ParticipantCtx) -> ParticipantChannel {
    let (tx, mut rx) = mpsc::channel::<ParticipantCmd>(1024);
    let pseudo_id = pctx.pseudo_id.clone();
    let span = tracing::info_span!(
        "participant_capture",
        pseudo_id = %pctx.pseudo_id,
        user_id = %pctx.user_id,
    );

    let join: JoinHandle<()> = tokio::spawn(
        async move {
            run_participant_task(pctx, &mut rx).await;
        }
        .instrument(span),
    );

    ParticipantChannel {
        tx,
        join,
        pseudo_id,
    }
}

async fn run_participant_task(pctx: ParticipantCtx, rx: &mut mpsc::Receiver<ParticipantCmd>) {
    let mut cache = open_cache_or_warn(&pctx.buffer_root, &pctx.session_id, &pctx.pseudo_id);
    let mut mode = PMode::Cache;
    let mut accum: Vec<u8> = Vec::with_capacity(CHUNK_SIZE_BYTES);
    let mut accum_started: Option<DateTime<Utc>> = None;
    let mut session_uuid: Option<uuid::Uuid> = None;
    let mut mix_tx: Option<mpsc::Sender<MixerCmd>> = None;
    let mut seq: u32 = 0;

    loop {
        tokio::select! {
            _ = pctx.cancel.cancelled() => break,
            maybe = rx.recv() => {
                let Some(cmd) = maybe else { break };
                match cmd {
                    ParticipantCmd::Packet(p) => {
                        if mode == PMode::Dropping { continue; }
                        if accum_started.is_none() {
                            accum_started = Some(Utc::now());
                        }
                        let bytes = samples_to_bytes(p.samples);
                        // Live mix gets a copy of the samples in Direct mode.
                        if mode == PMode::Direct && let Some(tx) = &mix_tx {
                            let _ = tx.try_send(MixerCmd::Packet {
                                now: Utc::now(),
                                samples_bytes: bytes.clone(),
                            });
                        }
                        accum.extend_from_slice(&bytes);
                        if accum.len() >= CHUNK_SIZE_BYTES {
                            let started = accum_started.take().unwrap_or_else(Utc::now);
                            let payload = std::mem::take(&mut accum);
                            emit_chunk(
                                &mut cache,
                                mode,
                                &pctx.api,
                                &pctx.pseudo_id,
                                session_uuid,
                                seq,
                                payload,
                                started,
                            ).await;
                            seq += 1;
                        }
                    }
                    ParticipantCmd::DropCache => {
                        // Pre-gate decline path: unlink the cache dir and stop.
                        if let Some(c) = cache.as_ref()
                            && let Err(e) = c.delete() {
                            warn!(error = %e, "participant_cache_delete_failed");
                        }
                        cache = None;
                        accum.clear();
                        accum_started = None;
                        mode = PMode::Dropping;
                    }
                    ParticipantCmd::GateOpened { accepted, session_uuid: sid, mix_tx: mtx } => {
                        // Gate-open: mode transition. For Accepted participants
                        // the actor has already flushed the pre-gate cache +
                        // deleted the dir, so we start fresh; for Pending we
                        // keep caching (a new dir may have been created after
                        // delete).
                        session_uuid = Some(sid);
                        mode = if accepted { PMode::Direct } else { PMode::PendingPostGate };
                        mix_tx = mtx;
                        // Reopen cache so the pre-gate flush-driven delete doesn't leave us holding a dead handle.
                        cache = open_cache_or_warn(&pctx.buffer_root, &pctx.session_id, &pctx.pseudo_id);
                        // Reset accum (pre-gate partial chunk has already been cached by the flush path).
                        accum.clear();
                        accum_started = None;
                        seq = 0;
                    }
                    ParticipantCmd::LateAccept => {
                        // Post-gate Accept: flush post-gate cache dir, switch to Direct.
                        if let (Some(c), Some(sid)) = (cache.as_ref(), session_uuid) {
                            let dir = c.dir().to_path_buf();
                            match buffer::flush_and_delete(pctx.api.clone(), sid, &pctx.pseudo_id, dir).await {
                                Ok(n) => info!(uploaded = n, "late_accept_cache_flushed"),
                                Err(e) => error!(error = %e, "late_accept_cache_flush_failed"),
                            }
                        }
                        // Reopen a fresh (empty) cache handle in case Direct mode
                        // races; Direct won't write to it in the happy path.
                        cache = open_cache_or_warn(&pctx.buffer_root, &pctx.session_id, &pctx.pseudo_id);
                        mode = PMode::Direct;
                    }
                    ParticipantCmd::LateDecline => {
                        if let Some(c) = cache.as_ref()
                            && let Err(e) = c.delete() {
                            warn!(error = %e, "late_decline_cache_delete_failed");
                        }
                        cache = None;
                        accum.clear();
                        accum_started = None;
                        mode = PMode::Dropping;
                    }
                    ParticipantCmd::Shutdown => break,
                }
            }
        }
    }

    // On exit, flush any remaining accum.
    if !accum.is_empty() {
        let started = accum_started.unwrap_or_else(Utc::now);
        let payload = std::mem::take(&mut accum);
        emit_chunk(
            &mut cache,
            mode,
            &pctx.api,
            &pctx.pseudo_id,
            session_uuid,
            seq,
            payload,
            started,
        )
        .await;
    }
}

fn open_cache_or_warn(
    buffer_root: &BufferRoot,
    session_id: &str,
    pseudo_id: &str,
) -> Option<ParticipantCache> {
    match ParticipantCache::open(buffer_root, session_id, pseudo_id) {
        Ok(c) => Some(c),
        Err(e) => {
            error!(error = %e, "participant_cache_open_failed");
            None
        }
    }
}

#[allow(clippy::too_many_arguments)]
async fn emit_chunk(
    cache: &mut Option<ParticipantCache>,
    mode: PMode,
    api: &DataApiClient,
    pseudo_id: &str,
    session_uuid: Option<uuid::Uuid>,
    seq: u32,
    payload: Vec<u8>,
    capture_started_at: DateTime<Utc>,
) {
    match mode {
        PMode::Cache | PMode::PendingPostGate => {
            if let Some(c) = cache.as_mut()
                && let Err(e) = c.append_chunk(&payload, capture_started_at) {
                warn!(error = %e, "chunk_cache_write_failed");
            }
        }
        PMode::Direct => {
            let Some(sid) = session_uuid else {
                warn!("direct_upload_missing_session_uuid — dropping chunk");
                return;
            };
            upload_direct(api, sid, pseudo_id, seq, payload, capture_started_at).await;
        }
        PMode::Dropping => {}
    }
}

fn samples_to_bytes(samples: Vec<i16>) -> Vec<u8> {
    let mut v = Vec::with_capacity(samples.len() * 2);
    for s in samples {
        v.extend_from_slice(&s.to_le_bytes());
    }
    v
}

async fn upload_direct(
    api: &DataApiClient,
    session_uuid: uuid::Uuid,
    pseudo_id: &str,
    seq: u32,
    bytes: Vec<u8>,
    capture_started_at: DateTime<Utc>,
) {
    let size = bytes.len();
    let start = std::time::Instant::now();
    let client_chunk_id = format!("{session_uuid}:{pseudo_id}:{seq}");
    let duration_ms = pcm_bytes_to_duration_ms(size);
    match api
        .upload_chunk_with_retry(
            session_uuid,
            pseudo_id,
            bytes,
            capture_started_at,
            duration_ms,
            &client_chunk_id,
        )
        .await
    {
        Ok(_) => {
            metrics::histogram!("chronicle_audio_chunk_upload_seconds")
                .record(start.elapsed().as_secs_f64());
            metrics::counter!("chronicle_audio_chunks_uploaded").increment(1);
            metrics::counter!(
                "chronicle_uploads_total",
                "type" => "chunk",
                "outcome" => "success",
            )
            .increment(1);
            info!(size, "chunk_uploaded");
        }
        Err(e) => {
            metrics::counter!(
                "chronicle_uploads_total",
                "type" => "chunk",
                "outcome" => "failure",
            )
            .increment(1);
            error!(error = %e, size, "chunk_upload_lost");
        }
    }
}

// ---------------------------------------------------------------------------
// Live mix task
// ---------------------------------------------------------------------------

pub enum MixerCmd {
    Packet {
        now: DateTime<Utc>,
        samples_bytes: Vec<u8>,
    },
    Shutdown,
}

struct MixerChannel {
    tx: mpsc::Sender<MixerCmd>,
    join: JoinHandle<()>,
}

fn spawn_mix_task(
    session_uuid: uuid::Uuid,
    api: Arc<DataApiClient>,
    cancel: CancellationToken,
) -> MixerChannel {
    let (tx, mut rx) = mpsc::channel::<MixerCmd>(1024);
    let span = tracing::info_span!("mix_task", session_uuid = %session_uuid);
    let join: JoinHandle<()> = tokio::spawn(
        async move {
            run_mix_task(session_uuid, api, cancel, &mut rx).await;
        }
        .instrument(span),
    );
    MixerChannel { tx, join }
}

async fn run_mix_task(
    session_uuid: uuid::Uuid,
    api: Arc<DataApiClient>,
    cancel: CancellationToken,
    rx: &mut mpsc::Receiver<MixerCmd>,
) {
    let mut accum = LiveMixAccum::new();
    let mut drain_tick = tokio::time::interval(Duration::from_millis(200));
    drain_tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);

    loop {
        tokio::select! {
            _ = cancel.cancelled() => break,
            _ = drain_tick.tick() => {
                if let Some(chunk) = accum.drain_ready(Utc::now()) {
                    upload_mix_chunk(&api, session_uuid, chunk).await;
                }
            }
            maybe = rx.recv() => {
                let Some(cmd) = maybe else { break };
                match cmd {
                    MixerCmd::Packet { now, samples_bytes } => {
                        let samples = bytes_to_samples(&samples_bytes);
                        if let Some(chunk) = accum.submit(now, &samples) {
                            upload_mix_chunk(&api, session_uuid, chunk).await;
                        }
                    }
                    MixerCmd::Shutdown => break,
                }
            }
        }
    }

    // Flush remaining.
    if let Some(chunk) = accum.finish() {
        upload_mix_chunk(&api, session_uuid, chunk).await;
    }
}

async fn upload_mix_chunk(api: &DataApiClient, session_uuid: uuid::Uuid, chunk: MixChunk) {
    upload_direct(
        api,
        session_uuid,
        MIXED_PSEUDO_ID,
        chunk.seq,
        chunk.bytes,
        chunk.capture_started_at,
    )
    .await;
}

fn bytes_to_samples(bytes: &[u8]) -> Vec<i16> {
    bytes
        .chunks_exact(2)
        .map(|b| i16::from_le_bytes([b[0], b[1]]))
        .collect()
}

// ---------------------------------------------------------------------------
// Actor run loop
// ---------------------------------------------------------------------------

struct ActorEnv {
    state: Arc<AppState>,
    ctx: Context,
    cancel: CancellationToken,
    buffer_root: BufferRoot,
    obs: AudioObservables,
    participants: HashMap<UserId, ParticipantChannel>,
    expected_user_ids: Arc<StdMutex<HashSet<u64>>>,
    #[allow(dead_code)]
    op5_rx: Option<mpsc::UnboundedReceiver<crate::voice::Op5Event>>,
    audio_handle: Option<AudioHandle>,
    pending_flush: JoinSet<Result<(), FlushError>>,
    /// Set post-gate only. `None` pre-gate means there is no data-api row.
    session_uuid: Option<uuid::Uuid>,
    mixer: Option<MixerChannel>,
    /// Humans currently in the voice channel (excluding the bot). Drives
    /// auto-stop (F7).
    humans_in_channel: HashSet<UserId>,
    /// Outstanding auto-stop grace timer; cancel on rejoin, fire on expiry.
    empty_channel_timer: Option<JoinHandle<()>>,
}

/// Auto-stop grace after the channel empties (F7).
pub const AUTO_STOP_GRACE: Duration = Duration::from_secs(30);

/// Compute the set of "accepting humans still present" (intersection of
/// in-channel humans and non-Declined session participants). Pure fn, split
/// out for test coverage.
pub fn accepting_humans(
    humans_in_channel: &HashSet<UserId>,
    session: &Session,
) -> HashSet<UserId> {
    humans_in_channel
        .iter()
        .filter(|uid| {
            session
                .participants
                .get(uid)
                .map(|p| p.state != ParticipantState::Declined)
                .unwrap_or(true)
        })
        .copied()
        .collect()
}

struct Outcome {
    restart: bool,
}

#[allow(clippy::too_many_arguments)]
async fn run_actor(
    state: Arc<AppState>,
    ctx: Context,
    mut session: Session,
    rx: mpsc::Receiver<SessionCmd>,
    shutting_down: Arc<std::sync::atomic::AtomicBool>,
    cancel: CancellationToken,
) {
    let buffer_root = BufferRoot::new(
        state.config.local_buffer_dir(),
        state.config.local_buffer_max_secs,
    );
    let expected = {
        let mut s = HashSet::new();
        for (uid, p) in &session.participants {
            if p.state != ParticipantState::Declined {
                s.insert(uid.get());
            }
        }
        Arc::new(StdMutex::new(s))
    };
    // Seed humans_in_channel with enrolled participants — they're the ones
    // in the voice channel at /record time.
    let humans_in_channel: HashSet<UserId> = session.participants.keys().copied().collect();

    let mut env = ActorEnv {
        state: state.clone(),
        ctx: ctx.clone(),
        cancel: cancel.clone(),
        buffer_root,
        obs: AudioObservables::new(),
        participants: HashMap::new(),
        expected_user_ids: expected,
        op5_rx: None,
        audio_handle: None,
        pending_flush: JoinSet::new(),
        session_uuid: None,
        mixer: None,
        humans_in_channel,
        empty_channel_timer: None,
    };

    let outcome = match join_voice_and_attach(&mut env, &mut session).await {
        Ok(()) => drive_session(&mut env, &mut session, rx).await,
        Err(e) => {
            error!(error = %e, "voice_join_failed");
            metrics::counter!("chronicle_sessions_total", "outcome" => "failed").increment(1);
            Outcome { restart: false }
        }
    };

    shutting_down.store(true, std::sync::atomic::Ordering::Relaxed);
    env.cancel.cancel();

    if outcome.restart {
        finalize_before_restart(&mut env, &session).await;
        spawn_restart(state.clone(), ctx.clone(), &session);
    } else {
        finalize_normal(&mut env, &mut session).await;
    }

    if let Some(h) = &env.audio_handle {
        h.shutdown();
    }
    state.sessions.remove(&session.guild_id);
    info!(phase = session.phase.label(), "session_actor_exit");
}

async fn join_voice_and_attach(env: &mut ActorEnv, session: &mut Session) -> Result<(), String> {
    let manager = songbird::get(&env.ctx).await.ok_or("songbird manager missing")?;
    let guild_id = GuildId::new(session.guild_id);
    let channel_id = ChannelId::new(session.channel_id);
    let call = manager
        .join(guild_id, channel_id)
        .await
        .map_err(|e| e.to_string())?;

    for (uid, p) in &session.participants {
        if p.state == ParticipantState::Declined {
            continue;
        }
        let pseudo = pseudonymize(uid.get());
        let ch = spawn_participant_task(ParticipantCtx {
            session_id: session.id.clone(),
            pseudo_id: pseudo,
            user_id: *uid,
            buffer_root: env.buffer_root.clone(),
            api: env.state.api.clone(),
            cancel: env.cancel.clone(),
        });
        env.participants.insert(*uid, ch);
    }

    let mut call_lock = call.lock().await;
    let (sink, _drop_rx) = build_sink_and_rx(&env.participants);
    let (op5_tx, op5_rx_new) = mpsc::unbounded_channel();
    let audio_handle = AudioReceiver::attach(&mut call_lock, sink, env.obs.clone(), op5_tx);
    drop(call_lock);

    env.audio_handle = Some(audio_handle);
    env.op5_rx = Some(op5_rx_new);
    Ok(())
}

fn build_sink_and_rx(
    participants: &HashMap<UserId, ParticipantChannel>,
) -> (PacketSink, mpsc::UnboundedReceiver<crate::voice::Op5Event>) {
    let routes: Arc<DashMap<u64, mpsc::Sender<ParticipantCmd>>> = Arc::new(DashMap::new());
    for (uid, ch) in participants {
        routes.insert(uid.get(), ch.tx.clone());
    }
    let sink: PacketSink = {
        let routes = routes.clone();
        Arc::new(move |pkt: AudioPacket| {
            if let Some(sender) = routes.get(&pkt.user_id) {
                let _ = sender.try_send(ParticipantCmd::Packet(pkt));
            }
        })
    };
    let (_tx, rx) = mpsc::unbounded_channel();
    (sink, rx)
}

async fn drive_session(
    env: &mut ActorEnv,
    session: &mut Session,
    mut rx: mpsc::Receiver<SessionCmd>,
) -> Outcome {
    // --- Stabilization gate ---
    let gate_inputs = GateInputs {
        ssrcs_seen: env.obs.ssrcs_seen.clone(),
        ssrc_map: env.obs.ssrc_map.clone(),
        expected_user_ids: env.expected_user_ids.clone(),
    };
    let gate_required = Duration::from_secs(env.state.config.stabilization_gate_secs);
    match wait_for_gate(env, session, &mut rx, &gate_inputs, gate_required).await {
        GateResult::Opened => {
            if let Err(e) = open_the_gate(env, session).await {
                warn!(error = %e, "gate_open_failed — session aborted");
                session.phase = Phase::Cancelled;
                return Outcome { restart: false };
            }
            session.phase = Phase::Recording {
                gate_opened_at: Utc::now(),
            };
            play_announcement(&env.ctx, session.guild_id, "recording_started").await;
            info!("recording_started");
            metrics::gauge!("chronicle_sessions_active").increment(1.0);
            send_consent_embed(&env.ctx, session).await;
        }
        GateResult::Stopped => {
            session.phase = Phase::Cancelled;
            return Outcome { restart: false };
        }
        GateResult::TimedOut => {
            warn!("stabilization_timeout — cancelling session");
            session.phase = Phase::Cancelled;
            return Outcome { restart: false };
        }
    }

    // --- Recording loop ---
    let mut heal_tick = tokio::time::interval(crate::session::heal::HEAL_TICK);
    heal_tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    heal_tick.tick().await;
    let mut heal_failures: u32 = 0;
    let restart_budget = env.state.restart_budget.clone();

    loop {
        tokio::select! {
            biased;
            _ = env.cancel.cancelled() => {
                info!("actor_cancelled");
                return Outcome { restart: false };
            }
            maybe_cmd = rx.recv() => {
                let Some(cmd) = maybe_cmd else {
                    return Outcome { restart: false };
                };
                if handle_cmd(env, session, cmd).await {
                    return Outcome { restart: false };
                }
            }
            _ = heal_tick.tick() => {
                let Some(manager) = songbird::get(&env.ctx).await else { continue };
                let verdict = crate::session::heal::check_and_heal(
                    &manager,
                    GuildId::new(session.guild_id),
                    ChannelId::new(session.channel_id),
                    &GateInputs {
                        ssrcs_seen: env.obs.ssrcs_seen.clone(),
                        ssrc_map: env.obs.ssrc_map.clone(),
                        expected_user_ids: env.expected_user_ids.clone(),
                    },
                    &mut heal_failures,
                ).await;
                match verdict {
                    crate::session::heal::HealVerdict::BudgetExhausted => {
                        if restart_budget.try_consume(session.guild_id) {
                            session.phase = Phase::Restarting;
                            return Outcome { restart: true };
                        } else {
                            send_unrecoverable(&env.ctx, session).await;
                            return Outcome { restart: false };
                        }
                    }
                    crate::session::heal::HealVerdict::Healed => {
                        env.obs.reset();
                        reattach_audio(env, session).await;
                    }
                    crate::session::heal::HealVerdict::Healthy => {}
                }
            }
            Some(joined) = env.pending_flush.join_next() => {
                match joined {
                    Ok(Ok(())) => {}
                    Ok(Err(e)) => warn!(error = %e, "pending_flush_error"),
                    Err(e) => warn!(error = %e, "pending_flush_join_error"),
                }
            }
        }
    }
}

enum GateResult {
    Opened,
    Stopped,
    TimedOut,
}

async fn wait_for_gate(
    env: &mut ActorEnv,
    session: &mut Session,
    rx: &mut mpsc::Receiver<SessionCmd>,
    gate_inputs: &GateInputs,
    required: Duration,
) -> GateResult {
    let mut tracker = StreakTracker::new(required);
    let mut poll = tokio::time::interval(Duration::from_millis(250));
    let started = std::time::Instant::now();
    loop {
        tokio::select! {
            biased;
            _ = env.cancel.cancelled() => {
                return GateResult::Stopped;
            }
            maybe_cmd = rx.recv() => {
                let Some(cmd) = maybe_cmd else { return GateResult::Stopped };
                if handle_cmd(env, session, cmd).await {
                    return GateResult::Stopped;
                }
            }
            _ = poll.tick() => {
                let verdict = stabilization::evaluate(gate_inputs);
                match tracker.observe(verdict) {
                    StreakStatus::GateOpens { total_secs } => {
                        metrics::histogram!("chronicle_stabilization_gate_secs")
                            .record(total_secs.as_secs_f64());
                        return GateResult::Opened;
                    }
                    StreakStatus::TimedOut => {
                        metrics::histogram!("chronicle_stabilization_gate_secs")
                            .record(started.elapsed().as_secs_f64());
                        return GateResult::TimedOut;
                    }
                    StreakStatus::Streaking | StreakStatus::Unhealthy => {}
                }
            }
        }
    }
}

// ---------------------------------------------------------------------------
// Gate-open transition — the heart of delta 1 + delta 2.
// ---------------------------------------------------------------------------

async fn open_the_gate(env: &mut ActorEnv, session: &mut Session) -> Result<(), String> {
    let session_uuid = uuid::Uuid::parse_str(&session.id).map_err(|e| e.to_string())?;

    // Run blocklist checks in parallel before touching the data-api.
    let allowed_ids = blocklist_filter(&env.state.api, &session.participants).await;

    // Mark blocklisted participants as Dropping + delete caches.
    let blocklisted: Vec<UserId> = session
        .participants
        .keys()
        .copied()
        .filter(|uid| !allowed_ids.contains(uid))
        .collect();
    for uid in &blocklisted {
        info!(user_id = %uid, "participant_blocklisted — dropping from session");
        session.participants.remove(uid);
        if let Some(ch) = env.participants.remove(uid) {
            let _ = ch.tx.send(ParticipantCmd::DropCache).await;
            let _ = ch.tx.send(ParticipantCmd::Shutdown).await;
            let _ = ch.join.await;
        }
    }

    // Create the data-api session row. This is the FIRST data-api write for
    // this session; if the gate never opened, it would never happen.
    let s3_prefix = format!("sessions/{}/{}", session.guild_id, session.id);
    env.state
        .api
        .create_session(
            session_uuid,
            session.guild_id as i64,
            session.started_at,
            None,
            None,
            Some(s3_prefix),
        )
        .await
        .map_err(|e| format!("create_session: {e}"))?;

    // Batch-add surviving (non-Declined, non-blocklisted) participants.
    let to_enrol: Vec<(u64, bool, Option<String>)> = session
        .participants
        .values()
        .filter(|p| p.state != ParticipantState::Declined)
        .map(|p| (p.user_id.get(), p.mid_session_join, Some(p.display_name.clone())))
        .collect();

    let rows = env
        .state
        .api
        .add_participants_batch(session_uuid, &to_enrol)
        .await
        .map_err(|e| format!("add_participants_batch: {e}"))?;

    // Propagate participant UUIDs back into the local Session.
    let uid_order: Vec<UserId> = to_enrol
        .iter()
        .map(|(uid, _, _)| UserId::new(*uid))
        .collect();
    for (uid, row) in uid_order.iter().zip(rows.iter()) {
        session.set_participant_uuid(*uid, row.id);
    }

    // Delete any per-speaker cache for an already-Declined participant and
    // drop their task. Collect the surviving participants' cache dirs for
    // mix rendering.
    let declined: Vec<UserId> = session
        .participants
        .iter()
        .filter(|(_, p)| p.state == ParticipantState::Declined)
        .map(|(uid, _)| *uid)
        .collect();
    for uid in declined {
        if let Some(ch) = env.participants.remove(&uid) {
            let _ = ch.tx.send(ParticipantCmd::DropCache).await;
            let _ = ch.tx.send(ParticipantCmd::Shutdown).await;
            let _ = ch.join.await;
        }
    }

    // Enumerate surviving per-speaker cache dirs. Do this on a blocking
    // thread since it's disk I/O.
    let session_id_str = session.id.clone();
    let surviving_pseudos: Vec<String> = env
        .participants
        .values()
        .map(|ch| ch.pseudo_id.clone())
        .collect();
    let buffer_root = env.buffer_root.clone();
    let mix_sources_and_accepted = tokio::task::spawn_blocking(move || {
        let mut sources = Vec::new();
        for pseudo in &surviving_pseudos {
            let dir = buffer_root.participant_dir(&session_id_str, pseudo);
            let chunks = collect_source_chunks(&dir);
            if !chunks.is_empty() {
                sources.push(PerSpeakerSource { chunks });
            }
        }
        sources
    })
    .await
    .map_err(|e| format!("mix_sources_join: {e}"))?;

    // Render mix + write to cache before flushing anything.
    let mix_chunks = render_mix_from_caches(&mix_sources_and_accepted);
    if !mix_chunks.is_empty() {
        let buffer_root = env.buffer_root.clone();
        let session_id_cloned = session.id.clone();
        tokio::task::spawn_blocking(move || {
            write_mix_chunks_to_cache(&buffer_root, &session_id_cloned, &mix_chunks)
        })
        .await
        .map_err(|e| format!("mix_write_join: {e}"))?
        .map_err(|e| format!("mix_write: {e}"))?;
    }

    // Flush all surviving per-speaker caches + mix cache in parallel.
    let mut flushes: JoinSet<Result<(usize, String), FlushError>> = JoinSet::new();
    let surviving_pseudos: Vec<String> = env
        .participants
        .values()
        .map(|ch| ch.pseudo_id.clone())
        .collect();
    for pseudo in surviving_pseudos {
        let dir = env.buffer_root.participant_dir(&session.id, &pseudo);
        let api = env.state.api.clone();
        flushes.spawn(async move {
            let n = buffer::flush_and_delete(api, session_uuid, &pseudo, dir).await?;
            Ok::<(usize, String), FlushError>((n, pseudo))
        });
    }
    // Mix flush.
    {
        let dir = env
            .buffer_root
            .participant_dir(&session.id, MIXED_PSEUDO_ID);
        let api = env.state.api.clone();
        flushes.spawn(async move {
            let n = buffer::flush_and_delete(api, session_uuid, MIXED_PSEUDO_ID, dir).await?;
            Ok::<(usize, String), FlushError>((n, MIXED_PSEUDO_ID.to_string()))
        });
    }
    while let Some(joined) = flushes.join_next().await {
        match joined {
            Ok(Ok((n, pseudo))) => {
                info!(uploaded = n, pseudo = %pseudo, "gate_flush_completed");
            }
            Ok(Err(e)) => warn!(error = %e, "gate_flush_errored"),
            Err(e) => warn!(error = %e, "gate_flush_join_error"),
        }
    }

    // Spawn the live mix task.
    let mixer = spawn_mix_task(session_uuid, env.state.api.clone(), env.cancel.clone());

    // Tell each surviving per-speaker task its mode for post-gate.
    let accepted_uids: HashSet<UserId> = session
        .participants
        .iter()
        .filter(|(_, p)| p.state == ParticipantState::Accepted)
        .map(|(uid, _)| *uid)
        .collect();
    for (uid, ch) in &env.participants {
        let accepted = accepted_uids.contains(uid);
        let mix_tx = if accepted { Some(mixer.tx.clone()) } else { None };
        let _ = ch
            .tx
            .send(ParticipantCmd::GateOpened {
                accepted,
                session_uuid,
                mix_tx,
            })
            .await;
    }

    env.session_uuid = Some(session_uuid);
    env.mixer = Some(mixer);
    Ok(())
}

async fn blocklist_filter(
    api: &Arc<DataApiClient>,
    participants: &HashMap<UserId, crate::session::ParticipantConsent>,
) -> HashSet<UserId> {
    let uids: Vec<UserId> = participants.keys().copied().collect();
    let mut set: JoinSet<(UserId, bool)> = JoinSet::new();
    for uid in &uids {
        let api = api.clone();
        let uid = *uid;
        set.spawn(async move {
            let blocked = api.check_blocklist(uid.get()).await.unwrap_or(false);
            (uid, blocked)
        });
    }
    let mut allowed = HashSet::new();
    while let Some(res) = set.join_next().await {
        if let Ok((uid, blocked)) = res
            && !blocked
        {
            allowed.insert(uid);
        }
    }
    allowed
}

fn collect_source_chunks(dir: &std::path::Path) -> Vec<SourceChunk> {
    let mut out = Vec::new();
    if !dir.exists() {
        return out;
    }
    let mut entries: Vec<_> = match std::fs::read_dir(dir) {
        Ok(r) => r.flatten().collect(),
        Err(_) => return out,
    };
    // Sort by filename so seq is preserved.
    entries.sort_by_key(|e| e.file_name());
    for entry in entries {
        let name = entry.file_name();
        let name = name.to_string_lossy().to_string();
        let Some(stripped) = name
            .strip_prefix("chunk_")
            .and_then(|s| s.strip_suffix(".pcm"))
        else {
            continue;
        };
        let mut parts = stripped.splitn(2, '_');
        let _seq = parts.next().and_then(|s| s.parse::<u32>().ok());
        let Some(ms) = parts.next().and_then(|s| s.parse::<i64>().ok()) else {
            continue;
        };
        let Some(bytes) = std::fs::read(entry.path()).ok() else {
            continue;
        };
        let Some(ts) = chrono::Utc.timestamp_millis_opt(ms).single() else {
            continue;
        };
        out.push(SourceChunk {
            capture_started_at: ts,
            bytes,
        });
    }
    out
}

async fn reattach_audio(env: &mut ActorEnv, session: &Session) {
    let Some(manager) = songbird::get(&env.ctx).await else { return };
    let Some(call) = manager.get(GuildId::new(session.guild_id)) else { return };
    let (sink, _drop_rx) = build_sink_and_rx(&env.participants);
    let (op5_tx, op5_rx) = mpsc::unbounded_channel();
    let mut call_lock = call.lock().await;
    let new_handle = AudioReceiver::attach(&mut call_lock, sink, env.obs.clone(), op5_tx);
    drop(call_lock);
    env.audio_handle = Some(new_handle);
    env.op5_rx = Some(op5_rx);
}

// ---------------------------------------------------------------------------
// Command dispatch
// ---------------------------------------------------------------------------

async fn handle_cmd(env: &mut ActorEnv, session: &mut Session, cmd: SessionCmd) -> bool {
    match cmd {
        SessionCmd::RecordConsent { user, scope, reply } => {
            let result = apply_consent(env, session, user, scope).await;
            let failed = matches!(result, Ok(ConsentOutcome::QuorumFailed { .. }));
            let _ = reply.send(result);
            if failed {
                env.cancel.cancel();
            }
            failed
        }
        SessionCmd::SetLicense { user, field, value, reply } => {
            let result = apply_license(&env.state, session, user, field, value).await;
            let _ = reply.send(result);
            false
        }
        SessionCmd::Stop { initiator, reply } => {
            if session.initiator_id != initiator {
                let _ = reply.send(Err(SessionError::NotInitiator));
                return false;
            }
            if !session.phase.is_stoppable() {
                let _ = reply.send(Err(SessionError::NotStoppable));
                return false;
            }
            let _ = reply.send(Ok(()));
            env.cancel.cancel();
            true
        }
        SessionCmd::AutoStop => {
            if !session.phase.is_stoppable() {
                return false;
            }
            env.cancel.cancel();
            true
        }
        SessionCmd::VoiceStateChange { old_channel, new_channel, user_id, is_bot, display_name } => {
            apply_voice_state(env, session, old_channel, new_channel, user_id, is_bot, display_name).await;
            false
        }
        SessionCmd::SetConsentMessage { channel_id, message_id } => {
            session.consent_message = Some((channel_id, message_id));
            false
        }
        SessionCmd::Enrol { user_id, display_name, is_bot: _, reply } => {
            let result = apply_enrol(env, session, user_id, display_name).await;
            let _ = reply.send(result);
            false
        }
        SessionCmd::GetSnapshot { reply } => {
            let snap = SessionSnapshot {
                session_id: session.id.clone(),
                guild_id: session.guild_id,
                recording: session.phase.is_recording(),
                stable: session.phase.is_recording(),
                phase_label: session.phase.label(),
                participant_count: session.participants.len(),
            };
            let _ = reply.send(snap);
            false
        }
    }
}

async fn apply_consent(
    env: &mut ActorEnv,
    session: &mut Session,
    user: UserId,
    scope: ConsentScope,
) -> Result<ConsentOutcome, SessionError> {
    if !session.participants.contains_key(&user) {
        return Err(SessionError::NotParticipant);
    }
    if session.participants[&user].scope.is_some() {
        return Err(SessionError::AlreadyResponded);
    }
    session.record_consent(user, scope);

    let session_uuid = env.session_uuid;
    let cached_pid = session.participant_uuid(user);
    let post_gate = session.phase.is_recording();

    // Consent PATCH — only meaningful post-gate (pre-gate there's no row to PATCH).
    if post_gate {
        let api = env.state.api.clone();
        let scope_str = match scope {
            ConsentScope::Full => "full",
            ConsentScope::Decline => "decline",
        };
        let scope_owned = scope_str.to_string();
        tokio::spawn(async move {
            let res = if let Some(pid) = cached_pid {
                api.record_consent_by_id(pid, &scope_owned).await
            } else if let Some(sid) = session_uuid {
                api.record_consent(sid, user.get(), &scope_owned).await
            } else {
                Ok(())
            };
            if let Err(e) = res {
                error!("record_consent api call failed: {e}");
            }
        });
    }

    // Drive the participant task.
    if let Some(ch) = env.participants.get(&user) {
        let cmd = match (post_gate, scope) {
            // Pre-gate: cache stays with the task; just note state. Decline
            // pre-gate means we'll exclude them at gate-open (DropCache sent
            // there).
            (false, _) => None,
            // Post-gate Accept: late-accept flush + switch to direct + mix.
            (true, ConsentScope::Full) => Some(ParticipantCmd::LateAccept),
            // Post-gate Decline: drop the post-gate cache.
            (true, ConsentScope::Decline) => Some(ParticipantCmd::LateDecline),
        };
        if let Some(c) = cmd {
            let _ = ch.tx.send(c).await;
        }
        // Also update mix routing for post-gate Accept so the task starts
        // feeding the mix.
        if post_gate
            && scope == ConsentScope::Full
            && let Some(mixer) = env.mixer.as_ref()
        {
            let _ = ch
                .tx
                .send(ParticipantCmd::GateOpened {
                    accepted: true,
                    session_uuid: session_uuid.expect("post_gate implies session_uuid"),
                    mix_tx: Some(mixer.tx.clone()),
                })
                .await;
        }
    }

    // F4: on Accept, immediately PATCH default license flags (post-gate only).
    if post_gate && scope == ConsentScope::Full && let Some(pid) = cached_pid {
        let api = env.state.api.clone();
        tokio::spawn(async move {
            if let Err(e) = api
                .set_license_flags_by_id(pid, Some(false), Some(false))
                .await
            {
                error!("set_license_flags (F4 defaults) failed: {e}");
            }
        });
    }

    // Declined users are no longer expected to produce audio.
    if scope == ConsentScope::Decline
        && let Ok(mut g) = env.expected_user_ids.lock()
    {
        g.remove(&user.get());
    }

    // Pre-gate Decline: tell the task to drop its cache now. (The gate loop
    // will also skip them.)
    if !post_gate && scope == ConsentScope::Decline
        && let Some(ch) = env.participants.get(&user)
    {
        let _ = ch.tx.send(ParticipantCmd::DropCache).await;
    }

    // Recompute the channel-human set after a decline (it reflects
    // "accepting humans" for the F7 auto-stop).
    recompute_humans_and_auto_stop_timer(env, session);

    let embed = session.consent_embed();
    let outcome = if session.all_responded() && !session.evaluate_quorum() {
        ConsentOutcome::QuorumFailed { embed }
    } else {
        ConsentOutcome::Ack { embed }
    };
    Ok(outcome)
}

async fn apply_license(
    state: &Arc<AppState>,
    session: &Session,
    user: UserId,
    field: LicenseField,
    value: bool,
) -> Result<(), SessionError> {
    let pid = session
        .participant_uuid(user)
        .ok_or(SessionError::NotParticipant)?;
    let (no_llm, no_public) = match field {
        LicenseField::NoLlmTraining => (Some(value), None),
        LicenseField::NoPublicRelease => (None, Some(value)),
    };
    state
        .api
        .set_license_flags_by_id(pid, no_llm, no_public)
        .await
        .map_err(|_| SessionError::NotParticipant)?;
    Ok(())
}

async fn apply_enrol(
    env: &mut ActorEnv,
    session: &mut Session,
    user_id: UserId,
    display_name: String,
) -> Result<uuid::Uuid, SessionError> {
    session.add_participant(user_id, display_name.clone(), false);

    // Pre-gate: local-only enrolment. No data-api call.
    let session_uuid_opt = env.session_uuid;
    let post_gate = session_uuid_opt.is_some();

    let participant_uuid = if post_gate {
        let sid = session_uuid_opt.unwrap();
        let row = env
            .state
            .api
            .add_participant(sid, user_id.get(), true, Some(display_name))
            .await
            .map_err(|_| SessionError::NotParticipant)?;
        session.set_participant_uuid(user_id, row.id);
        row.id
    } else {
        // Placeholder UUID for the pre-gate harness path; the real UUID is
        // assigned at gate-open when the participant is batch-inserted.
        uuid::Uuid::nil()
    };

    if let Ok(mut g) = env.expected_user_ids.lock() {
        g.insert(user_id.get());
    }

    let pseudo = pseudonymize(user_id.get());
    let ch = spawn_participant_task(ParticipantCtx {
        session_id: session.id.clone(),
        pseudo_id: pseudo,
        user_id,
        buffer_root: env.buffer_root.clone(),
        api: env.state.api.clone(),
        cancel: env.cancel.clone(),
    });
    env.participants.insert(user_id, ch);

    recompute_humans_and_auto_stop_timer(env, session);
    Ok(participant_uuid)
}

// ---------------------------------------------------------------------------
// F7 — auto-stop when the channel empties.
// ---------------------------------------------------------------------------

async fn apply_voice_state(
    env: &mut ActorEnv,
    session: &Session,
    _old_channel: Option<ChannelId>,
    new_channel: Option<ChannelId>,
    user_id: UserId,
    is_bot: bool,
    _display_name: String,
) {
    if is_bot {
        return;
    }
    let session_channel = ChannelId::new(session.channel_id);
    if new_channel == Some(session_channel) {
        env.humans_in_channel.insert(user_id);
    } else {
        env.humans_in_channel.remove(&user_id);
    }
    recompute_humans_and_auto_stop_timer(env, session);
}

fn recompute_humans_and_auto_stop_timer(env: &mut ActorEnv, session: &Session) {
    let accepting = accepting_humans(&env.humans_in_channel, session);

    if accepting.is_empty() {
        // Start the grace timer if one isn't already running.
        if env.empty_channel_timer.is_none() {
            let cancel = env.cancel.clone();
            let session_tx = env.state.sessions.get(&session.guild_id).map(|e| e.clone());
            let handle = tokio::spawn(async move {
                tokio::select! {
                    biased;
                    _ = cancel.cancelled() => {}
                    _ = tokio::time::sleep(AUTO_STOP_GRACE) => {
                        if let Some(handle) = session_tx {
                            let _ = handle.send(SessionCmd::AutoStop).await;
                        }
                    }
                }
            });
            env.empty_channel_timer = Some(handle);
        }
    } else if let Some(h) = env.empty_channel_timer.take() {
        h.abort();
    }
}

// ---------------------------------------------------------------------------
// Finalization
// ---------------------------------------------------------------------------

async fn finalize_normal(env: &mut ActorEnv, session: &mut Session) {
    let was_recording = session.phase.is_recording();
    let had_gate_open = env.session_uuid.is_some();
    session.phase = Phase::Finalizing;
    session.ended_at = Some(Utc::now());

    if was_recording {
        play_announcement(&env.ctx, session.guild_id, "recording_stopped").await;
    }

    if let Some(manager) = songbird::get(&env.ctx).await {
        let _ = manager.leave(GuildId::new(session.guild_id)).await;
    }

    // Shut down the mix task + drain.
    if let Some(mixer) = env.mixer.take() {
        let _ = mixer.tx.send(MixerCmd::Shutdown).await;
        let _ = mixer.join.await;
    }

    // Shut down per-participant tasks concurrently.
    let mut drains = JoinSet::new();
    for (_, ch) in env.participants.drain() {
        drains.spawn(async move {
            let _ = ch.tx.send(ParticipantCmd::Shutdown).await;
            let _ = ch.join.await;
        });
    }
    while drains.join_next().await.is_some() {}

    if had_gate_open {
        if let Some(sid) = env.session_uuid {
            let meta_bytes = session.meta_json();
            let consent_bytes = session.consent_json();
            let meta_val: Option<serde_json::Value> = serde_json::from_slice(&meta_bytes).ok();
            let consent_val: Option<serde_json::Value> = serde_json::from_slice(&consent_bytes).ok();
            let api = env.state.api.clone();
            let participant_count = session.participants.len() as i32;
            let (meta_res, final_res) = tokio::join!(
                api.write_metadata(sid, meta_val, consent_val),
                async {
                    let api = env.state.api.clone();
                    api.finalize_session(sid, Utc::now(), participant_count).await
                }
            );
            if let Err(e) = meta_res {
                error!("write_metadata failed: {e}");
            }
            if let Err(e) = final_res {
                error!("finalize_session failed: {e}");
            }
            metrics::gauge!("chronicle_sessions_active").decrement(1.0);
        }
    } else {
        // Pre-gate exit: no data-api row was ever created. Drop the buffer
        // dir and post a "session abandoned" message. Per F2 in the spec,
        // no abandon_session call either.
        post_abandoned_message(&env.ctx, session).await;
    }

    if let Err(e) = buffer::delete_session(&env.buffer_root, &session.id) {
        warn!(error = %e, "buffer_dir_cleanup_failed");
    }
    cleanup_consent_embed(&env.ctx, session).await;
}

async fn finalize_before_restart(env: &mut ActorEnv, session: &Session) {
    if let Some(manager) = songbird::get(&env.ctx).await {
        let _ = manager.leave(GuildId::new(session.guild_id)).await;
    }
    // The data-api row was created at gate-open, so mark it abandoned here.
    if let Some(sid) = env.session_uuid
        && let Err(e) = env.state.api.abandon_session(sid).await
    {
        error!("abandon_session (restart) failed: {e}");
    }
    if let Err(e) = buffer::delete_session(&env.buffer_root, &session.id) {
        warn!(error = %e, "buffer_dir_cleanup_failed (restart)");
    }
    ChannelId::new(session.text_channel_id)
        .say(
            &env.ctx.http,
            "Something went catastrophically wrong, restarting session.",
        )
        .await
        .ok();
}

fn spawn_restart(state: Arc<AppState>, ctx: Context, failed: &Session) {
    let carried = carry_forward(&failed.participants);
    let next = Session::new_carry_forward(
        failed.guild_id,
        failed.channel_id,
        failed.text_channel_id,
        failed.initiator_id,
        state.config.min_participants,
        state.config.require_all_consent,
        carried,
    );
    match spawn_session(state, ctx, next) {
        Ok(_) => info!("catastrophic_restart_spawned"),
        Err(_) => {
            warn!("catastrophic_restart_insert_conflict — guild already had an active session")
        }
    }
}

async fn send_unrecoverable(ctx: &Context, session: &Session) {
    ChannelId::new(session.text_channel_id)
        .say(&ctx.http, "Session unrecoverable — please `/record` again.")
        .await
        .ok();
}

async fn post_abandoned_message(ctx: &Context, session: &Session) {
    ChannelId::new(session.text_channel_id)
        .say(&ctx.http, "Session abandoned before recording started.")
        .await
        .ok();
}

async fn play_announcement(ctx: &Context, guild_id: u64, label: &str) {
    let Some(manager) = songbird::get(ctx).await else { return };
    let Some(call) = manager.get(GuildId::new(guild_id)) else { return };
    let path = match label {
        "recording_started" => "/assets/recording_started.wav",
        "recording_stopped" => "/assets/recording_stopped.wav",
        _ => return,
    };
    let mut handler = call.lock().await;
    let source = songbird::input::File::new(path);
    let _ = handler.play_input(source.into());
    drop(handler);
    tokio::time::sleep(Duration::from_secs(2)).await;
}

async fn send_consent_embed(ctx: &Context, session: &Session) {
    if session.consent_message.is_some() {
        return;
    }
    let embed = session.consent_embed();
    let msg = serenity::all::CreateMessage::new()
        .embed(embed)
        .components(vec![consent_buttons()]);
    let _ = ChannelId::new(session.text_channel_id)
        .send_message(&ctx.http, msg)
        .await;
}

async fn cleanup_consent_embed(ctx: &Context, session: &Session) {
    if let Some((channel_id, msg_id)) = session.consent_message {
        let edit = EditMessage::new()
            .embed(
                CreateEmbed::new()
                    .title("Session complete")
                    .description("Recording has ended. Thank you for contributing!")
                    .color(0x8b949e),
            )
            .components(vec![]);
        let _ = ctx
            .http
            .edit_message(channel_id, msg_id, &edit, vec![])
            .await;
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use crate::session::ConsentScope;

    #[test]
    fn session_error_variants_have_distinct_messages() {
        let msgs: Vec<String> = vec![
            SessionError::NotParticipant.to_string(),
            SessionError::AlreadyResponded.to_string(),
            SessionError::NotInitiator.to_string(),
            SessionError::NotStoppable.to_string(),
            SessionError::ActorGone.to_string(),
        ];
        let unique: std::collections::HashSet<&String> = msgs.iter().collect();
        assert_eq!(unique.len(), msgs.len());
    }

    fn demo_session() -> Session {
        let mut s = Session::new(1, 2, 3, UserId::new(1), 1, true);
        s.add_participant(UserId::new(10), "alice".into(), false);
        s.add_participant(UserId::new(11), "bob".into(), false);
        s
    }

    #[test]
    fn accepting_humans_counts_only_non_declined_channel_humans() {
        let mut s = demo_session();
        s.record_consent(UserId::new(11), ConsentScope::Decline);
        let mut in_channel = HashSet::new();
        in_channel.insert(UserId::new(10));
        in_channel.insert(UserId::new(11));
        in_channel.insert(UserId::new(99)); // not a session participant — counted
        let acc = accepting_humans(&in_channel, &s);
        assert!(acc.contains(&UserId::new(10)));
        assert!(!acc.contains(&UserId::new(11)));
        assert!(acc.contains(&UserId::new(99)));
    }

    #[test]
    fn accepting_humans_empty_when_only_declined_present() {
        let mut s = demo_session();
        s.record_consent(UserId::new(10), ConsentScope::Decline);
        s.record_consent(UserId::new(11), ConsentScope::Decline);
        let mut in_channel = HashSet::new();
        in_channel.insert(UserId::new(10));
        in_channel.insert(UserId::new(11));
        assert!(accepting_humans(&in_channel, &s).is_empty());
    }

    /// F7: a 30s grace timer fires once the channel is empty, but is
    /// cancelled when a human rejoins before it expires. We model this as
    /// a pure primitive with tokio's paused clock.
    #[tokio::test(start_paused = true)]
    async fn auto_stop_timer_fires_after_grace_period() {
        let fired = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let f_clone = fired.clone();
        let cancel = CancellationToken::new();
        let handle = {
            let cancel = cancel.clone();
            tokio::spawn(async move {
                tokio::select! {
                    biased;
                    _ = cancel.cancelled() => {}
                    _ = tokio::time::sleep(AUTO_STOP_GRACE) => {
                        f_clone.store(true, std::sync::atomic::Ordering::SeqCst);
                    }
                }
            })
        };
        tokio::time::advance(AUTO_STOP_GRACE + Duration::from_secs(1)).await;
        let _ = handle.await;
        assert!(fired.load(std::sync::atomic::Ordering::SeqCst));
        drop(cancel);
    }

    #[tokio::test(start_paused = true)]
    async fn auto_stop_timer_cancels_on_rejoin_before_expiry() {
        let fired = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let f_clone = fired.clone();
        let cancel = CancellationToken::new();
        let handle = {
            let cancel = cancel.clone();
            tokio::spawn(async move {
                tokio::select! {
                    biased;
                    _ = cancel.cancelled() => {}
                    _ = tokio::time::sleep(AUTO_STOP_GRACE) => {
                        f_clone.store(true, std::sync::atomic::Ordering::SeqCst);
                    }
                }
            })
        };
        // Simulate a rejoin at T=5s: cancel the grace timer.
        tokio::time::advance(Duration::from_secs(5)).await;
        cancel.cancel();
        let _ = handle.await;
        // Advance past the grace window — timer should never fire.
        tokio::time::advance(AUTO_STOP_GRACE).await;
        assert!(!fired.load(std::sync::atomic::Ordering::SeqCst));
    }

    /// Delta 1: verify that when a participant Declines pre-gate, their
    /// per-speaker cache directory is deleted and cannot survive into the
    /// gate-flush stage.
    #[tokio::test]
    async fn decline_before_gate_deletes_participant_cache() {
        use crate::voice::buffer::{BufferRoot, ParticipantCache};
        use tempfile::TempDir;
        let td = TempDir::new().unwrap();
        let root = BufferRoot::new(td.path().to_path_buf(), 7200);
        let session_id = "sess-1";
        let pseudo = "pseudo-xyz";
        let mut cache = ParticipantCache::open(&root, session_id, pseudo).unwrap();
        cache
            .append_chunk(b"hello", Utc::now())
            .unwrap();
        assert!(cache.dir().exists());
        // Simulate the actor's Decline-before-gate path: delete the cache.
        cache.delete().unwrap();
        assert!(!cache.dir().exists());
        // And confirm a subsequent gate-flush collect yields no chunks.
        let chunks = collect_source_chunks(&root.participant_dir(session_id, pseudo));
        assert!(chunks.is_empty());
    }

    /// Delta 2: the mix rendered at gate-open excludes declined
    /// participants by virtue of only getting passed surviving sources.
    #[test]
    fn mix_render_excludes_declined_participants() {
        use crate::voice::buffer::{BufferRoot, ParticipantCache};
        use crate::voice::mixer::{render_mix_from_caches, PerSpeakerSource};
        use tempfile::TempDir;
        let td = TempDir::new().unwrap();
        let root = BufferRoot::new(td.path().to_path_buf(), 7200);

        // Two participants — one survives, one declines.
        let mut alice = ParticipantCache::open(&root, "s", "alice").unwrap();
        let mut bob = ParticipantCache::open(&root, "s", "bob").unwrap();
        let ts = Utc.timestamp_millis_opt(1_700_000_000_000).unwrap();
        alice.append_chunk(&vec![100i16.to_le_bytes()[0]; 4], ts).unwrap();
        bob.append_chunk(&vec![100i16.to_le_bytes()[0]; 4], ts).unwrap();
        // Bob declines → their cache is deleted.
        bob.delete().unwrap();

        // Collect surviving (alice only).
        let alice_chunks = collect_source_chunks(&root.participant_dir("s", "alice"));
        let bob_chunks = collect_source_chunks(&root.participant_dir("s", "bob"));
        assert_eq!(bob_chunks.len(), 0);
        assert_eq!(alice_chunks.len(), 1);

        let sources = vec![PerSpeakerSource { chunks: alice_chunks }];
        let mix = render_mix_from_caches(&sources);
        // Exactly one source → mix output non-empty.
        assert!(!mix.is_empty());
    }

    /// Delta 1: the timestamp encoded on disk by `append_chunk` is the one
    /// that's preserved into `capture_started_at` when collected. This is
    /// what the gate-flush relies on to carry the right header value.
    #[test]
    fn collect_source_chunks_preserves_capture_timestamp() {
        use crate::voice::buffer::{BufferRoot, ParticipantCache};
        use tempfile::TempDir;
        let td = TempDir::new().unwrap();
        let root = BufferRoot::new(td.path().to_path_buf(), 7200);
        let t_a = Utc.timestamp_millis_opt(1_700_000_000_000).unwrap();
        let t_b = Utc.timestamp_millis_opt(1_700_000_000_500).unwrap();
        let t_c = Utc.timestamp_millis_opt(1_700_000_001_200).unwrap();
        let mut c = ParticipantCache::open(&root, "s", "p").unwrap();
        c.append_chunk(b"chunk_a", t_a).unwrap();
        c.append_chunk(b"chunk_b", t_b).unwrap();
        c.append_chunk(b"chunk_c", t_c).unwrap();
        let src = collect_source_chunks(&root.participant_dir("s", "p"));
        assert_eq!(src.len(), 3);
        assert_eq!(src[0].capture_started_at, t_a);
        assert_eq!(src[1].capture_started_at, t_b);
        assert_eq!(src[2].capture_started_at, t_c);
    }
}

//! Audio capture pipeline: VoiceTick handler, per-speaker buffering, and Data API upload.
//!
//! The hot path (VoiceTick) is kept lock-free by sending audio packets through
//! an mpsc channel to a background buffer task. The buffer task accumulates
//! per-speaker PCM data and uploads chunks via the Data API when they reach the threshold.

use std::collections::{HashMap, HashSet};
use std::sync::{Arc, Mutex as StdMutex};

use serenity::async_trait;
use songbird::events::context_data::VoiceTick;
use songbird::{CoreEvent, Event, EventContext, EventHandler as VoiceEventHandler};
use tokio::sync::{mpsc, Mutex};
use tracing::info;
use uuid::Uuid;

use crate::api_client::DataApiClient;
use crate::storage::pseudonymize;

/// 5MB chunk size — ~26 seconds of 48kHz stereo s16le audio per speaker
const CHUNK_SIZE: usize = 5 * 1024 * 1024;

/// Audio data sent from the VoiceTick handler to the buffer task via channel.
/// This keeps the hot path lock-free.
pub struct AudioPacket {
    ssrc: u32,
    user_id: u64,
    data: Vec<i16>,
}

/// Per-speaker buffer that uploads 5MB chunks via the Data API when full.
struct SpeakerBuffer {
    pseudo_id: String,
    buffer: Vec<u8>,
    chunk_seq: u32,
    total_bytes: u64,
}

impl SpeakerBuffer {
    fn new(pseudo_id: String) -> Self {
        Self {
            pseudo_id,
            buffer: Vec::with_capacity(CHUNK_SIZE),
            chunk_seq: 0,
            total_bytes: 0,
        }
    }

    /// Append PCM bytes. Returns a chunk to upload if buffer is full.
    fn write(&mut self, bytes: &[u8]) -> Option<ChunkToUpload> {
        self.buffer.extend_from_slice(bytes);
        self.total_bytes += bytes.len() as u64;

        if self.buffer.len() >= CHUNK_SIZE {
            Some(self.drain_chunk())
        } else {
            None
        }
    }

    fn drain_chunk(&mut self) -> ChunkToUpload {
        let data = std::mem::take(&mut self.buffer);
        let seq = self.chunk_seq;
        self.chunk_seq += 1;
        self.buffer = Vec::with_capacity(CHUNK_SIZE);
        ChunkToUpload {
            pseudo_id: self.pseudo_id.clone(),
            seq,
            data,
        }
    }

    /// Flush remaining data as the final (possibly smaller) chunk.
    fn flush(&mut self) -> Option<ChunkToUpload> {
        if self.buffer.is_empty() {
            None
        } else {
            Some(self.drain_chunk())
        }
    }
}

/// A buffered PCM chunk ready for upload.
#[allow(dead_code)]
struct ChunkToUpload {
    pseudo_id: String,
    seq: u32,
    data: Vec<u8>,
}

/// Handle returned from register() — used by /stop to cleanly shut down audio capture.
pub struct AudioHandle {
    /// Close this to signal the buffer task to drain and flush
    receiver_close: Arc<tokio::sync::Notify>,
    /// Await this after signaling close to wait for final flush + upload
    task: Option<tokio::task::JoinHandle<()>>,
}

impl AudioHandle {
    /// Shut down audio capture: signal channel close, wait for final flush.
    pub async fn shutdown(mut self) {
        // Signal the buffer task to stop accepting new audio and flush
        self.receiver_close.notify_one();
        // Wait for the buffer task to finish uploading
        if let Some(task) = self.task.take() {
            let _ = task.await;
        }
    }
}

pub struct AudioReceiver {
    /// Channel sender — VoiceTick pushes audio here, no buffer lock needed
    tx: mpsc::Sender<AudioPacket>,
    /// SSRC → discord user id mapping. std::sync::Mutex because holds are
    /// always non-blocking (single HashMap op) and the VoiceTick hot path
    /// should not be awaiting.
    ssrc_to_user: Arc<StdMutex<HashMap<u32, u64>>>,
    consented_users: Arc<Mutex<HashSet<u64>>>,
    /// Shared flag: set to true once decoded audio arrives (DAVE watchdog)
    pub audio_received: Arc<std::sync::atomic::AtomicBool>,
}

impl AudioReceiver {
    /// Create the audio pipeline: channel + buffer task + handle.
    /// Call this ONCE per session. Use `attach()` to wire it to a Songbird Call
    /// (can be called multiple times for DAVE retries).
    pub fn create_pipeline(
        api: Arc<DataApiClient>,
        session_id: Uuid,
    ) -> (mpsc::Sender<AudioPacket>, AudioHandle) {
        let (tx, rx) = mpsc::channel::<AudioPacket>(1000);
        let close_signal = Arc::new(tokio::sync::Notify::new());

        let task = tokio::spawn(buffer_task(rx, api, session_id, close_signal.clone()));

        let handle = AudioHandle {
            receiver_close: close_signal,
            task: Some(task),
        };

        (tx, handle)
    }

    /// Attach audio receiver and speaking tracker to a Songbird Call.
    /// Can be called multiple times (DAVE retries) with the same tx channel —
    /// all retries feed into the single buffer task.
    pub fn attach(
        call: &mut songbird::Call,
        tx: mpsc::Sender<AudioPacket>,
        consented_users: Arc<Mutex<HashSet<u64>>>,
        audio_received: Arc<std::sync::atomic::AtomicBool>,
    ) -> Arc<StdMutex<HashMap<u32, u64>>> {
        let ssrc_map = Arc::new(StdMutex::new(HashMap::new()));

        let receiver = Self {
            tx,
            ssrc_to_user: ssrc_map.clone(),
            consented_users,
            audio_received,
        };
        call.add_global_event(CoreEvent::VoiceTick.into(), receiver);

        let tracker = SpeakingTracker {
            ssrc_to_user: ssrc_map.clone(),
        };
        call.add_global_event(CoreEvent::SpeakingStateUpdate.into(), tracker);

        ssrc_map
    }
}

#[async_trait]
impl VoiceEventHandler for AudioReceiver {
    async fn act(&self, ctx: &EventContext<'_>) -> Option<Event> {
        if let EventContext::VoiceTick(VoiceTick { speaking, .. }) = ctx {
            // Acquire the tokio mutex (async) first so the std mutex guard
            // never needs to cross an await point — std MutexGuard is !Send.
            let consented = self.consented_users.lock().await;
            let ssrc_map = self.ssrc_to_user.lock().expect("ssrc_map poisoned");

            for (ssrc, data) in speaking {
                // Only capture audio for consented users
                let user_id = match ssrc_map.get(ssrc) {
                    Some(uid) if consented.contains(uid) => *uid,
                    _ => continue,
                };

                if let Some(decoded) = &data.decoded_voice {
                    // Signal DAVE is working
                    self.audio_received
                        .store(true, std::sync::atomic::Ordering::Relaxed);

                    metrics::counter!("ttrpg_audio_packets_received").increment(1);

                    // Send to buffer task via channel — no lock contention
                    let _ = self.tx.try_send(AudioPacket {
                        ssrc: *ssrc,
                        user_id,
                        data: decoded.clone(),
                    });
                }
            }
        }
        None
    }
}

/// Background task that receives audio from the channel, buffers per-speaker,
/// and uploads 5MB chunks via the Data API. Exits when signaled via close_signal.
async fn buffer_task(
    mut rx: mpsc::Receiver<AudioPacket>,
    api: Arc<DataApiClient>,
    session_id: Uuid,
    close_signal: Arc<tokio::sync::Notify>,
) {
    let mut buffers: HashMap<u32, SpeakerBuffer> = HashMap::new();

    let mut packet_count: u64 = 0;
    let mut total_bytes: u64 = 0;
    let mut last_status = tokio::time::Instant::now();

    loop {
        tokio::select! {
            // Process incoming audio packets
            packet = rx.recv() => {
                let Some(packet) = packet else { break };
                let pkt_bytes = (packet.data.len() * 2) as u64;
                packet_count += 1;
                total_bytes += pkt_bytes;
                process_packet(&mut buffers, &api, session_id, packet);

                // Log status every 10 seconds
                if last_status.elapsed() >= std::time::Duration::from_secs(10) {
                    let buffered: u64 = buffers.values().map(|b| b.buffer.len() as u64).sum();
                    metrics::gauge!("ttrpg_audio_bytes_buffered").set(buffered as f64);
                    info!(
                        packets = packet_count,
                        total_bytes = total_bytes,
                        buffered_bytes = buffered,
                        speakers = buffers.len(),
                        "audio_status"
                    );
                    last_status = tokio::time::Instant::now();
                }
            }
            // Shutdown signal from AudioHandle
            _ = close_signal.notified() => {
                // Drain any remaining packets in the channel before flushing
                while let Ok(packet) = rx.try_recv() {
                    process_packet(&mut buffers, &api, session_id, packet);
                }
                break;
            }
        }
    }

    // Flush all remaining speaker buffers
    metrics::gauge!("ttrpg_audio_bytes_buffered").set(0.0);
    info!(speakers = buffers.len(), "flushing_audio_buffers");
    for (ssrc, buffer) in buffers.iter_mut() {
        if let Some(chunk) = buffer.flush() {
            let size = chunk.data.len();
            let upload_start = std::time::Instant::now();
            match api.upload_chunk(session_id, &chunk.pseudo_id, chunk.data).await {
                Ok(_) => {
                    let elapsed = upload_start.elapsed().as_secs_f64();
                    metrics::histogram!("ttrpg_audio_chunk_upload_seconds").record(elapsed);
                    metrics::counter!("ttrpg_audio_chunks_uploaded").increment(1);
                    metrics::counter!("ttrpg_uploads_total", "type" => "chunk", "outcome" => "success").increment(1);
                    info!(pseudo_id = %chunk.pseudo_id, size = size, ssrc = ssrc, upload_secs = elapsed, "final_chunk_uploaded");
                }
                Err(e) => {
                    metrics::counter!("ttrpg_uploads_total", "type" => "chunk", "outcome" => "failure").increment(1);
                    tracing::error!(pseudo_id = %chunk.pseudo_id, error = %e, "final_chunk_upload_failed");
                }
            }
        }
    }
}

/// Buffer a single audio packet — upload a chunk via the Data API if the buffer is full.
fn process_packet(
    buffers: &mut HashMap<u32, SpeakerBuffer>,
    api: &Arc<DataApiClient>,
    session_id: Uuid,
    packet: AudioPacket,
) {
    let buffer = buffers.entry(packet.ssrc).or_insert_with(|| {
        let pseudo_id = pseudonymize(packet.user_id);
        info!(ssrc = packet.ssrc, pseudo_id = %pseudo_id, "new_speaker");
        SpeakerBuffer::new(pseudo_id)
    });

    // Reinterpret the decoded i16 PCM samples as little-endian bytes for
    // upload. bytemuck::cast_slice is zero-copy and statically checks that
    // i16→u8 is a sound reinterpretation (alignment + size), so we don't
    // need unsafe. Host endianness matters: x86/ARM64 are LE, matching the
    // pcm_s16le format we advertise in meta.json.
    let bytes: &[u8] = bytemuck::cast_slice(&packet.data);

    if let Some(chunk) = buffer.write(bytes) {
        let size = chunk.data.len();
        let api_clone = api.clone();
        let pseudo_id = chunk.pseudo_id.clone();
        tokio::spawn(async move {
            let upload_start = std::time::Instant::now();
            match api_clone.upload_chunk(session_id, &pseudo_id, chunk.data).await {
                Ok(_) => {
                    let elapsed = upload_start.elapsed().as_secs_f64();
                    metrics::histogram!("ttrpg_audio_chunk_upload_seconds").record(elapsed);
                    metrics::counter!("ttrpg_audio_chunks_uploaded").increment(1);
                    metrics::counter!("ttrpg_uploads_total", "type" => "chunk", "outcome" => "success").increment(1);
                    info!(pseudo_id = %pseudo_id, size = size, upload_secs = elapsed, "chunk_uploaded");
                }
                Err(e) => {
                    metrics::counter!("ttrpg_uploads_total", "type" => "chunk", "outcome" => "failure").increment(1);
                    tracing::error!(pseudo_id = %pseudo_id, error = %e, "chunk_upload_failed");
                }
            }
        });
    }
}

/// Tracks SSRC → user_id mappings from Discord speaking events.
struct SpeakingTracker {
    ssrc_to_user: Arc<StdMutex<HashMap<u32, u64>>>,
}

#[async_trait]
impl VoiceEventHandler for SpeakingTracker {
    async fn act(&self, ctx: &EventContext<'_>) -> Option<Event> {
        if let EventContext::SpeakingStateUpdate(speaking) = ctx
            && let Some(uid) = speaking.user_id
        {
            let mut map = self.ssrc_to_user.lock().expect("ssrc_map poisoned");
            if !map.contains_key(&speaking.ssrc) {
                info!(ssrc = speaking.ssrc, user_id = %uid, "ssrc_mapped");
            }
            map.insert(speaking.ssrc, uid.0);
        }
        None
    }
}

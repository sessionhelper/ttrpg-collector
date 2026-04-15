//! Audio capture pipeline: VoiceTick handler + per-participant routing.
//!
//! The hot path (`VoiceTick`) is lock-free: it forwards decoded PCM to a
//! per-SSRC participant task via an mpsc channel, so the tick handler never
//! touches per-participant state directly. Per-participant tasks — spawned
//! by the session actor — own their own disk cache and drive the
//! `Pending → Accepted/Declined` sub-state-machine (F3).

use std::collections::{HashMap, HashSet};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex as StdMutex};

use serenity::async_trait;
use songbird::events::context_data::VoiceTick;
use songbird::{CoreEvent, Event, EventContext, EventHandler as VoiceEventHandler};
use tokio::sync::mpsc;
use tracing::{debug, info, warn};

/// PCM frame for one speaker on one tick. Sent from the VoiceTick handler to
/// the per-participant router.
pub struct AudioPacket {
    #[allow(dead_code)]
    pub ssrc: u32,
    pub user_id: u64,
    pub samples: Vec<i16>,
}

/// OP5 speaking-state event — used by the stabilization gate to confirm
/// SSRC→user_id mappings exist.
#[derive(Debug, Clone, Copy)]
#[allow(dead_code)]
pub struct Op5Event {
    pub ssrc: u32,
    pub user_id: u64,
}

/// Handle returned from `AudioReceiver::attach` — close it to drop the
/// VoiceTick forwarding.
pub struct AudioHandle {
    _close: Arc<tokio::sync::Notify>,
}

impl AudioHandle {
    pub fn shutdown(&self) {
        self._close.notify_waiters();
    }
}

/// Observability handles that the actor reads for the stabilization gate.
#[derive(Clone)]
pub struct AudioObservables {
    pub ssrc_map: Arc<StdMutex<HashMap<u32, u64>>>,
    pub ssrcs_seen: Arc<StdMutex<HashSet<u32>>>,
}

impl AudioObservables {
    pub fn new() -> Self {
        Self {
            ssrc_map: Arc::new(StdMutex::new(HashMap::new())),
            ssrcs_seen: Arc::new(StdMutex::new(HashSet::new())),
        }
    }

    /// Reset all observables — used on heal-driven re-attach so stale SSRCs
    /// don't mask a genuinely broken new connection.
    pub fn reset(&self) {
        if let Ok(mut m) = self.ssrc_map.lock() {
            m.clear();
        }
        if let Ok(mut s) = self.ssrcs_seen.lock() {
            s.clear();
        }
    }
}

impl Default for AudioObservables {
    fn default() -> Self {
        Self::new()
    }
}

/// Signature of the packet sink: the session actor hands us a closure that
/// routes each [`AudioPacket`] to the right per-participant task.
pub type PacketSink = Arc<dyn Fn(AudioPacket) + Send + Sync + 'static>;

/// Diagnostic counters the VoiceTick handler bumps. Visible at INFO-level
/// rollups (every 100 ticks or 5s, whichever comes first) so DAVE-class
/// failures stop being invisible.
#[derive(Default)]
struct TickCounters {
    ticks_total: AtomicU64,
    ticks_with_speaking: AtomicU64,
    ticks_empty_speaking: AtomicU64,
    decoded_packets: AtomicU64,
    silent_packets: AtomicU64,
    unmapped_ssrc: AtomicU64,
    packets_forwarded: AtomicU64,
    last_log_tick: AtomicU64,
}

/// The session actor calls this once per heal-cycle attach. `ssrc_map` and
/// `ssrcs_seen` are Arc'd so the gate-watcher task can observe them in
/// parallel with the VoiceTick handler.
pub struct AudioReceiver {
    sink: PacketSink,
    obs: AudioObservables,
    counters: Arc<TickCounters>,
}

impl AudioReceiver {
    pub fn attach(
        call: &mut songbird::Call,
        sink: PacketSink,
        obs: AudioObservables,
        op5_tx: mpsc::UnboundedSender<Op5Event>,
    ) -> AudioHandle {
        let close = Arc::new(tokio::sync::Notify::new());
        let counters = Arc::new(TickCounters::default());
        let receiver = Self {
            sink,
            obs: obs.clone(),
            counters: counters.clone(),
        };
        call.add_global_event(CoreEvent::VoiceTick.into(), receiver);
        let tracker = SpeakingTracker {
            ssrc_to_user: obs.ssrc_map.clone(),
            op5_tx,
        };
        call.add_global_event(CoreEvent::SpeakingStateUpdate.into(), tracker);
        info!("audio_receiver_attached");
        AudioHandle { _close: close }
    }
}

impl TickCounters {
    fn bump_tick(&self, speaking_empty: bool) -> u64 {
        let tick = self.ticks_total.fetch_add(1, Ordering::Relaxed);
        let bucket = if speaking_empty { &self.ticks_empty_speaking } else { &self.ticks_with_speaking };
        bucket.fetch_add(1, Ordering::Relaxed);
        tick
    }

    fn snapshot(&self) -> (u64, u64, u64, u64, u64, u64) {
        (
            self.ticks_total.load(Ordering::Relaxed),
            self.ticks_with_speaking.load(Ordering::Relaxed),
            self.decoded_packets.load(Ordering::Relaxed),
            self.silent_packets.load(Ordering::Relaxed),
            self.unmapped_ssrc.load(Ordering::Relaxed),
            self.packets_forwarded.load(Ordering::Relaxed),
        )
    }
}

impl AudioReceiver {
    /// Log a rollup snapshot every 250 ticks (~5s at 50 Hz). INFO level so
    /// it surfaces without bumping the whole crate to debug.
    fn maybe_log_rollup(&self, tick: u64) {
        if tick == 0 || !tick.is_multiple_of(250) {
            return;
        }
        let (ticks, with_speaking, decoded, silent, unmapped, forwarded) = self.counters.snapshot();
        let ssrc_map_len = self.obs.ssrc_map.lock().map(|m| m.len()).unwrap_or(0);
        let ssrcs_seen = self.obs.ssrcs_seen.lock().map(|s| s.len()).unwrap_or(0);
        info!(
            ticks, with_speaking, decoded, silent, unmapped, forwarded,
            ssrc_map_len, ssrcs_seen,
            "voice_rx_rollup"
        );
    }

}

#[async_trait]
impl VoiceEventHandler for AudioReceiver {
    async fn act(&self, ctx: &EventContext<'_>) -> Option<Event> {
        let EventContext::VoiceTick(VoiceTick { speaking, .. }) = ctx else {
            return None;
        };

        let tick = self.counters.bump_tick(speaking.is_empty());
        self.maybe_log_rollup(tick);

        if speaking.is_empty() {
            return None;
        }
        let Ok(ssrc_map) = self.obs.ssrc_map.lock().map(|g| g.clone()) else {
            return None;
        };

        // Inlined: songbird's SpeakingData is not re-exported under a
        // nameable path, and extracting into a helper requires naming it.
        // The vertical flow below reads linearly; each `continue` is a
        // named failure mode.
        for (ssrc, data) in speaking {
            let ssrc = *ssrc;
            let Some(decoded) = data.decoded_voice.as_ref() else {
                let n = self.counters.silent_packets.fetch_add(1, Ordering::Relaxed) + 1;
                if n <= 3 {
                    warn!(ssrc, "voice_tick_no_decoded (DAVE decrypt fail or silent)");
                }
                continue;
            };
            self.counters.decoded_packets.fetch_add(1, Ordering::Relaxed);
            if let Ok(mut s) = self.obs.ssrcs_seen.lock()
                && s.insert(ssrc)
            {
                info!(ssrc, samples = decoded.len(), "ssrc_first_audio");
            }
            let Some(&user_id) = ssrc_map.get(&ssrc) else {
                let n = self.counters.unmapped_ssrc.fetch_add(1, Ordering::Relaxed) + 1;
                if n <= 3 {
                    warn!(ssrc, "voice_tick_ssrc_not_mapped (OP5 missing)");
                }
                continue;
            };
            self.counters.packets_forwarded.fetch_add(1, Ordering::Relaxed);
            metrics::counter!("chronicle_audio_packets_received").increment(1);
            (self.sink)(AudioPacket { ssrc, user_id, samples: decoded.clone() });
        }
        None
    }
}

struct SpeakingTracker {
    ssrc_to_user: Arc<StdMutex<HashMap<u32, u64>>>,
    op5_tx: mpsc::UnboundedSender<Op5Event>,
}

#[async_trait]
impl VoiceEventHandler for SpeakingTracker {
    async fn act(&self, ctx: &EventContext<'_>) -> Option<Event> {
        if let EventContext::SpeakingStateUpdate(s) = ctx
            && let Some(uid) = s.user_id
        {
            let is_new = if let Ok(mut map) = self.ssrc_to_user.lock() {
                map.insert(s.ssrc, uid.0).is_none()
            } else {
                false
            };
            if is_new {
                info!(ssrc = s.ssrc, user_id = uid.0, "op5_ssrc_mapped");
            } else {
                debug!(ssrc = s.ssrc, user_id = uid.0, "op5_ssrc_remapped");
            }
            let _ = self.op5_tx.send(Op5Event {
                ssrc: s.ssrc,
                user_id: uid.0,
            });
        }
        None
    }
}

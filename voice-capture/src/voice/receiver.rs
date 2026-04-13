//! Audio capture pipeline: VoiceTick handler + per-participant routing.
//!
//! The hot path (`VoiceTick`) is lock-free: it forwards decoded PCM to a
//! per-SSRC participant task via an mpsc channel, so the tick handler never
//! touches per-participant state directly. Per-participant tasks — spawned
//! by the session actor — own their own disk cache and drive the
//! `Pending → Accepted/Declined` sub-state-machine (F3).

use std::collections::{HashMap, HashSet};
use std::sync::{Arc, Mutex as StdMutex};

use serenity::async_trait;
use songbird::events::context_data::VoiceTick;
use songbird::{CoreEvent, Event, EventContext, EventHandler as VoiceEventHandler};
use tokio::sync::mpsc;

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

/// The session actor calls this once per heal-cycle attach. `ssrc_map` and
/// `ssrcs_seen` are Arc'd so the gate-watcher task can observe them in
/// parallel with the VoiceTick handler.
pub struct AudioReceiver {
    sink: PacketSink,
    obs: AudioObservables,
}

impl AudioReceiver {
    pub fn attach(
        call: &mut songbird::Call,
        sink: PacketSink,
        obs: AudioObservables,
        op5_tx: mpsc::UnboundedSender<Op5Event>,
    ) -> AudioHandle {
        let close = Arc::new(tokio::sync::Notify::new());
        let receiver = Self {
            sink,
            obs: obs.clone(),
        };
        call.add_global_event(CoreEvent::VoiceTick.into(), receiver);
        let tracker = SpeakingTracker {
            ssrc_to_user: obs.ssrc_map.clone(),
            op5_tx,
        };
        call.add_global_event(CoreEvent::SpeakingStateUpdate.into(), tracker);
        AudioHandle { _close: close }
    }
}

#[async_trait]
impl VoiceEventHandler for AudioReceiver {
    async fn act(&self, ctx: &EventContext<'_>) -> Option<Event> {
        if let EventContext::VoiceTick(VoiceTick { speaking, .. }) = ctx {
            if speaking.is_empty() {
                return None;
            }
            let ssrc_map = match self.obs.ssrc_map.lock() {
                Ok(g) => g.clone(),
                Err(_) => return None,
            };

            for (ssrc, data) in speaking {
                let Some(decoded) = data.decoded_voice.as_ref() else {
                    continue;
                };
                // Record SSRC as "seen" regardless of mapping — the gate
                // watcher cares about any audio.
                if let Ok(mut s) = self.obs.ssrcs_seen.lock() {
                    s.insert(*ssrc);
                }
                let Some(&user_id) = ssrc_map.get(ssrc) else {
                    continue;
                };
                metrics::counter!("chronicle_audio_packets_received").increment(1);
                (self.sink)(AudioPacket {
                    ssrc: *ssrc,
                    user_id,
                    samples: decoded.clone(),
                });
            }
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
            if let Ok(mut map) = self.ssrc_to_user.lock() {
                map.insert(s.ssrc, uid.0);
            }
            let _ = self.op5_tx.send(Op5Event {
                ssrc: s.ssrc,
                user_id: uid.0,
            });
        }
        None
    }
}

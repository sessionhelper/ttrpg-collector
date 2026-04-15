//! Session phase state machine.
//!
//! ```text
//!   AwaitingStabilization
//!     │
//!     ├───── gate opens (N healthy seconds) ─────▶ Recording
//!     │
//!     └───── 180s timeout OR Cancel cmd ─────▶ Cancelled
//!
//!   Recording
//!     │
//!     ├───── /stop OR AutoStop ─────▶ Finalizing ─────▶ (actor exits)
//!     │
//!     └───── catastrophic failure ─────▶ Restarting ─────▶ (spawn new session)
//! ```
//!
//! The actor owns a single `Phase` and mutates it in place; external code
//! observes via `SessionCmd::GetSnapshot`.

use chrono::{DateTime, Utc};

/// Session phase.
#[derive(Debug, Clone, Copy)]
#[allow(dead_code)]
pub enum Phase {
    /// Voice pipeline attached, DAVE settling, consent messages posted, per-
    /// participant tasks caching chunks to disk. Gate has not yet opened.
    AwaitingStabilization { entered_at: DateTime<Utc> },

    /// Gate opened. Announcement played. Chunks flow live (cached for Pending,
    /// direct-to-api for Accepted). Heal events are silent.
    Recording { gate_opened_at: DateTime<Utc> },

    /// /stop or auto-stop fired from Recording. Draining pending uploads and
    /// writing metadata.
    Finalizing,

    /// Catastrophic failure during Recording. The actor posts a text message
    /// and spawns a successor actor with a new session_id. The old actor
    /// exits once this finishes.
    Restarting,

    /// AwaitingStabilization timed out, or the caller cancelled. Session row
    /// marked `abandoned`; no announcement was ever played.
    Cancelled,
}

#[allow(dead_code)]
impl Phase {
    pub fn label(&self) -> &'static str {
        match self {
            Phase::AwaitingStabilization { .. } => "awaiting_stabilization",
            Phase::Recording { .. } => "recording",
            Phase::Finalizing => "finalizing",
            Phase::Restarting => "restarting",
            Phase::Cancelled => "cancelled",
        }
    }

    pub fn is_stoppable(&self) -> bool {
        matches!(self, Phase::AwaitingStabilization { .. } | Phase::Recording { .. })
    }

    pub fn is_recording(&self) -> bool {
        matches!(self, Phase::Recording { .. })
    }

    pub fn is_terminal(&self) -> bool {
        matches!(self, Phase::Finalizing | Phase::Cancelled | Phase::Restarting)
    }

    pub fn time_in_phase(&self) -> chrono::Duration {
        match self {
            Phase::AwaitingStabilization { entered_at } => Utc::now() - *entered_at,
            Phase::Recording { gate_opened_at } => Utc::now() - *gate_opened_at,
            _ => chrono::Duration::zero(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn phase_labels_are_distinct() {
        let labels = [
            Phase::AwaitingStabilization { entered_at: Utc::now() }.label(),
            Phase::Recording { gate_opened_at: Utc::now() }.label(),
            Phase::Finalizing.label(),
            Phase::Restarting.label(),
            Phase::Cancelled.label(),
        ];
        let unique: std::collections::HashSet<_> = labels.iter().collect();
        assert_eq!(unique.len(), labels.len());
    }

    #[test]
    fn is_stoppable_only_for_pre_terminal() {
        assert!(Phase::AwaitingStabilization { entered_at: Utc::now() }.is_stoppable());
        assert!(Phase::Recording { gate_opened_at: Utc::now() }.is_stoppable());
        assert!(!Phase::Finalizing.is_stoppable());
        assert!(!Phase::Cancelled.is_stoppable());
        assert!(!Phase::Restarting.is_stoppable());
    }
}

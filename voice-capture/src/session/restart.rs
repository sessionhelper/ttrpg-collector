//! Catastrophic-recovery (Restarting phase).
//!
//! After the stabilization gate has opened, if the actor hits a catastrophic
//! failure (supervisor-caught panic, heal budget exhausted, data-api 5xx past
//! the retry budget) it transitions to the Restarting phase and spawns a
//! successor actor with a fresh `session_id` carrying the existing participant
//! list forward.
//!
//! Budget: at most **1 restart per guild per hour**. A second restart within
//! the window posts `"Session unrecoverable — please /record again"` and exits.

use std::collections::HashMap;
use std::time::{Duration, Instant};

use dashmap::DashMap;
use serenity::all::UserId;
use tracing::warn;

use super::ParticipantConsent;

/// One restart allowed per hour.
pub const RESTART_WINDOW: Duration = Duration::from_secs(60 * 60);
pub const RESTART_BUDGET_PER_WINDOW: u32 = 1;

/// Per-guild budget tracker. Each guild_id maps to `(window_start, used_count)`.
#[derive(Default)]
pub struct RestartBudget {
    inner: DashMap<u64, BudgetEntry>,
}

#[derive(Clone, Copy)]
struct BudgetEntry {
    window_start: Instant,
    used: u32,
}

impl RestartBudget {
    pub fn new() -> Self {
        Self::default()
    }

    /// Record a restart attempt. Returns `true` if the restart is allowed,
    /// `false` if the budget is exhausted.
    pub fn try_consume(&self, guild_id: u64) -> bool {
        let mut entry = self.inner.entry(guild_id).or_insert(BudgetEntry {
            window_start: Instant::now(),
            used: 0,
        });
        if entry.window_start.elapsed() >= RESTART_WINDOW {
            entry.window_start = Instant::now();
            entry.used = 0;
        }
        if entry.used >= RESTART_BUDGET_PER_WINDOW {
            warn!(
                guild_id,
                used = entry.used,
                "restart_budget_exhausted"
            );
            return false;
        }
        entry.used += 1;
        true
    }
}

/// Carry forward participant records from a failed session to its successor.
pub fn carry_forward(
    participants: &HashMap<UserId, ParticipantConsent>,
) -> HashMap<UserId, ParticipantConsent> {
    participants.clone()
}

#[cfg(test)]
mod tests {
    use super::*;

    use crate::session::participant::ParticipantState;
    use crate::session::{ConsentScope, Session};
    use serenity::all::UserId;

    #[test]
    fn restart_budget_allows_one_per_hour() {
        let budget = RestartBudget::new();
        assert!(budget.try_consume(1));
        assert!(!budget.try_consume(1));
        // Different guild still gets its own fresh budget.
        assert!(budget.try_consume(2));
    }

    #[test]
    fn carry_forward_clones_records_verbatim() {
        let mut s = Session::new(1, 2, 3, UserId::new(1), 1, true);
        s.add_participant(UserId::new(1), "alice".into(), false);
        s.add_participant(UserId::new(2), "bob".into(), false);
        s.record_consent(UserId::new(1), ConsentScope::Full);
        // bob stays pending.
        let carried = carry_forward(&s.participants);
        assert_eq!(carried.len(), 2);
        assert_eq!(
            carried[&UserId::new(1)].state,
            ParticipantState::Accepted
        );
        assert_eq!(carried[&UserId::new(2)].state, ParticipantState::Pending);
    }

    #[test]
    fn restart_yields_fresh_session_id() {
        let mut original = Session::new(1, 2, 3, UserId::new(1), 1, true);
        original.add_participant(UserId::new(1), "alice".into(), false);
        original.record_consent(UserId::new(1), ConsentScope::Full);
        let carried = carry_forward(&original.participants);
        let next = Session::new_carry_forward(1, 2, 3, UserId::new(1), 1, true, carried);
        assert_ne!(original.id, next.id, "restart must mint a new session_id");
        // Accepted carries over, but pending promotes to decline on the
        // new session (spec rule).
        assert_eq!(
            next.participants[&UserId::new(1)].state,
            ParticipantState::Accepted
        );
    }
}

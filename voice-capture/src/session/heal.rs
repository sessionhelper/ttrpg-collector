//! Post-gate silent heal loop.
//!
//! After the stabilization gate opens, DAVE/SSRC churn can still happen.
//! This loop heals silently — no announcement replay, no user-visible
//! disruption. If heal repeatedly fails past the budget, the actor escalates
//! to [`super::restart`].

use std::sync::Arc;
use std::time::Duration;

use serenity::all::{ChannelId, GuildId};
use songbird::Songbird;
use tracing::{error, info, warn};

use super::stabilization::{evaluate, GateInputs, GateVerdict};

/// How often the heal loop wakes to check the gate's health-indicator set.
pub const HEAL_TICK: Duration = Duration::from_secs(5);

/// Maximum consecutive heal failures before the actor escalates to Restarting.
pub const HEAL_FAIL_BUDGET: u32 = 3;

pub enum HealVerdict {
    /// Everything's healthy — no action needed.
    Healthy,
    /// Healed successfully this cycle. Continue.
    Healed,
    /// Heal exhausted its budget — escalate to restart.
    BudgetExhausted,
}

pub async fn check_and_heal(
    manager: &Arc<Songbird>,
    guild_id: GuildId,
    channel_id: ChannelId,
    gate: &GateInputs,
    consecutive_failures: &mut u32,
) -> HealVerdict {
    if evaluate(gate) == GateVerdict::Healthy {
        *consecutive_failures = 0;
        return HealVerdict::Healthy;
    }

    warn!("post_gate_heal_triggered");
    let _ = manager.leave(guild_id).await;
    tokio::time::sleep(Duration::from_secs(2)).await;
    match manager.join(guild_id, channel_id).await {
        Ok(_) => {
            info!("post_gate_heal_rejoined");
            // Give DAVE a moment to resume.
            tokio::time::sleep(Duration::from_secs(3)).await;
            if evaluate(gate) == GateVerdict::Healthy {
                *consecutive_failures = 0;
                HealVerdict::Healed
            } else {
                *consecutive_failures += 1;
                if *consecutive_failures >= HEAL_FAIL_BUDGET {
                    error!(
                        failures = *consecutive_failures,
                        budget = HEAL_FAIL_BUDGET,
                        "post_gate_heal_budget_exhausted"
                    );
                    HealVerdict::BudgetExhausted
                } else {
                    HealVerdict::Healed
                }
            }
        }
        Err(e) => {
            *consecutive_failures += 1;
            error!(error = %e, failures = *consecutive_failures, "post_gate_heal_rejoin_failed");
            if *consecutive_failures >= HEAL_FAIL_BUDGET {
                HealVerdict::BudgetExhausted
            } else {
                HealVerdict::Healed
            }
        }
    }
}

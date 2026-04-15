//! Stabilization gate (F6).
//!
//! Opens when every expected-audio SSRC has been healthy for N continuous
//! seconds (default 3, configured via `STABILIZATION_GATE_SECS`).
//! Hard cap: 180s from gate entry — if it hasn't opened by then the session
//! goes to Cancelled.

use std::collections::HashSet;
use std::sync::{Arc, Mutex as StdMutex};
use std::time::{Duration, Instant};

/// Hard cap on AwaitingStabilization. Spec-locked at 180s.
pub const STABILIZATION_TIMEOUT: Duration = Duration::from_secs(180);

/// Shared state read by the gate polling loop.
#[derive(Clone)]
pub struct GateInputs {
    /// SSRCs that have produced decoded audio since the last reset.
    pub ssrcs_seen: Arc<StdMutex<HashSet<u32>>>,
    /// SSRC → user_id mappings from OP5.
    pub ssrc_map: Arc<StdMutex<std::collections::HashMap<u32, u64>>>,
    /// Set of user IDs we expect audio from (consented + pending — anyone
    /// who might produce audio we care about).
    pub expected_user_ids: Arc<StdMutex<HashSet<u64>>>,
}

/// Gate verdict at a point in time.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum GateVerdict {
    /// No expected users → gate can open the moment some SSRC is mapped.
    /// With expected users: every expected user must have an SSRC mapping
    /// AND each mapped SSRC must have decoded audio.
    Healthy,
    /// Something was off (missing mapping, missing audio, or no speakers).
    Unhealthy,
}

/// Evaluate the gate right now.
pub fn evaluate(inputs: &GateInputs) -> GateVerdict {
    let expected = {
        let guard = match inputs.expected_user_ids.lock() {
            Ok(g) => g,
            Err(_) => return GateVerdict::Unhealthy,
        };
        guard.clone()
    };
    let ssrc_map = match inputs.ssrc_map.lock() {
        Ok(g) => g.clone(),
        Err(_) => return GateVerdict::Unhealthy,
    };
    let seen = match inputs.ssrcs_seen.lock() {
        Ok(g) => g.clone(),
        Err(_) => return GateVerdict::Unhealthy,
    };

    // Empty-channel edge case: no expected users means gate opens as soon as
    // at least one SSRC is mapped AND seen (bot + one human scenarios).
    if expected.is_empty() {
        let any_mapped_and_seen = ssrc_map.keys().any(|ssrc| seen.contains(ssrc));
        return if any_mapped_and_seen {
            GateVerdict::Healthy
        } else {
            GateVerdict::Unhealthy
        };
    }

    // Every expected user must have an SSRC whose audio has been seen.
    let mapped_users: HashSet<u64> = ssrc_map
        .iter()
        .filter(|(ssrc, _uid)| seen.contains(ssrc))
        .map(|(_ssrc, uid)| *uid)
        .collect();

    if expected.is_subset(&mapped_users) {
        GateVerdict::Healthy
    } else {
        GateVerdict::Unhealthy
    }
}

/// Track "continuous healthy" streak at 250ms cadence. Returns Ok once the
/// healthy streak hits `required`, returns Err on `STABILIZATION_TIMEOUT`.
///
/// The loop polls at 250ms to keep latency low without thrashing; the stabilization
/// invariant is "N seconds of healthy", which this measures by resetting the
/// streak whenever the verdict flips back to Unhealthy.
pub struct StreakTracker {
    entered: Instant,
    healthy_since: Option<Instant>,
    required: Duration,
}

impl StreakTracker {
    pub fn new(required: Duration) -> Self {
        Self {
            entered: Instant::now(),
            healthy_since: None,
            required,
        }
    }

    /// Feed in a new verdict. Returns the current streak status.
    pub fn observe(&mut self, verdict: GateVerdict) -> StreakStatus {
        match verdict {
            GateVerdict::Healthy => {
                let since = self.healthy_since.get_or_insert_with(Instant::now);
                if since.elapsed() >= self.required {
                    StreakStatus::GateOpens {
                        total_secs: self.entered.elapsed(),
                    }
                } else {
                    StreakStatus::Streaking
                }
            }
            GateVerdict::Unhealthy => {
                self.healthy_since = None;
                if self.entered.elapsed() >= STABILIZATION_TIMEOUT {
                    StreakStatus::TimedOut
                } else {
                    StreakStatus::Unhealthy
                }
            }
        }
    }
}

pub enum StreakStatus {
    /// First-ever healthy tick, or still accumulating toward `required`.
    Streaking,
    /// Gate opens now.
    GateOpens { total_secs: Duration },
    /// Still unhealthy, still under the 180s cap.
    Unhealthy,
    /// 180s cap hit — session must go to Cancelled.
    TimedOut,
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;

    fn fresh_inputs() -> GateInputs {
        GateInputs {
            ssrcs_seen: Arc::new(StdMutex::new(HashSet::new())),
            ssrc_map: Arc::new(StdMutex::new(HashMap::new())),
            expected_user_ids: Arc::new(StdMutex::new(HashSet::new())),
        }
    }

    #[test]
    fn gate_unhealthy_when_no_users_and_no_ssrcs() {
        let inputs = fresh_inputs();
        assert_eq!(evaluate(&inputs), GateVerdict::Unhealthy);
    }

    #[test]
    fn gate_healthy_with_all_expected_users_mapped_and_seen() {
        let inputs = fresh_inputs();
        inputs
            .expected_user_ids
            .lock()
            .unwrap()
            .extend([10u64, 11]);
        inputs
            .ssrc_map
            .lock()
            .unwrap()
            .extend([(100u32, 10u64), (101, 11)]);
        inputs.ssrcs_seen.lock().unwrap().extend([100u32, 101]);
        assert_eq!(evaluate(&inputs), GateVerdict::Healthy);
    }

    #[test]
    fn gate_unhealthy_when_one_user_missing_audio() {
        let inputs = fresh_inputs();
        inputs
            .expected_user_ids
            .lock()
            .unwrap()
            .extend([10u64, 11]);
        inputs
            .ssrc_map
            .lock()
            .unwrap()
            .extend([(100u32, 10u64), (101, 11)]);
        inputs.ssrcs_seen.lock().unwrap().insert(100);
        // User 11 is mapped but their SSRC hasn't produced audio → unhealthy.
        assert_eq!(evaluate(&inputs), GateVerdict::Unhealthy);
    }

    #[test]
    fn streak_requires_continuous_healthy_window() {
        let mut tracker = StreakTracker::new(Duration::from_millis(50));
        assert!(matches!(tracker.observe(GateVerdict::Healthy), StreakStatus::Streaking));
        // Blip unhealthy → streak resets.
        assert!(matches!(tracker.observe(GateVerdict::Unhealthy), StreakStatus::Unhealthy));
        assert!(matches!(tracker.observe(GateVerdict::Healthy), StreakStatus::Streaking));
        std::thread::sleep(Duration::from_millis(60));
        assert!(matches!(
            tracker.observe(GateVerdict::Healthy),
            StreakStatus::GateOpens { .. }
        ));
    }

    #[test]
    fn gate_opens_after_simulated_voice_frames() {
        // Full integration: seed expected users, then simulate OP5 +
        // VoiceTick arrivals by populating ssrc_map and ssrcs_seen. The
        // gate evaluator must return Healthy, and a tracker with a short
        // window must observe GateOpens after sustained healthy ticks.
        let inputs = fresh_inputs();
        inputs.expected_user_ids.lock().unwrap().extend([100u64, 101]);

        // Before OP5 fires: gate is unhealthy.
        assert_eq!(evaluate(&inputs), GateVerdict::Unhealthy);

        // OP5 maps SSRCs → users. Still unhealthy until audio seen.
        inputs
            .ssrc_map
            .lock()
            .unwrap()
            .extend([(5000u32, 100u64), (5001, 101)]);
        assert_eq!(evaluate(&inputs), GateVerdict::Unhealthy);

        // VoiceTick adds both SSRCs to `seen` → gate is healthy.
        inputs.ssrcs_seen.lock().unwrap().extend([5000u32, 5001]);
        assert_eq!(evaluate(&inputs), GateVerdict::Healthy);

        // Streak tracker with 50ms required: opens after >=50ms sustained
        // healthy ticks.
        let mut tracker = StreakTracker::new(Duration::from_millis(50));
        assert!(matches!(
            tracker.observe(evaluate(&inputs)),
            StreakStatus::Streaking
        ));
        std::thread::sleep(Duration::from_millis(60));
        let verdict = evaluate(&inputs);
        assert!(matches!(
            tracker.observe(verdict),
            StreakStatus::GateOpens { .. }
        ));
    }

    #[test]
    fn gate_does_not_reopen_after_first_open() {
        // Property: once the tracker reports GateOpens, subsequent healthy
        // observations also say GateOpens (the tracker's healthy_since is
        // sticky). The ACTOR's responsibility is to stop observing after
        // the first open — verified in actor.rs::drive_session which
        // matches StreakStatus::GateOpens and `return`s.
        let mut tracker = StreakTracker::new(Duration::from_millis(10));
        // First observation primes healthy_since; need another after the
        // required window passes to actually "open."
        assert!(matches!(
            tracker.observe(GateVerdict::Healthy),
            StreakStatus::Streaking
        ));
        std::thread::sleep(Duration::from_millis(15));
        assert!(matches!(
            tracker.observe(GateVerdict::Healthy),
            StreakStatus::GateOpens { .. }
        ));
        // Subsequent tick: still says GateOpens (tracker is stateless past
        // the threshold). The actor has already returned from the gate
        // loop so it never polls again.
        assert!(matches!(
            tracker.observe(GateVerdict::Healthy),
            StreakStatus::GateOpens { .. }
        ));
    }
}

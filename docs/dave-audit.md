# DAVE Protocol Audit — Application-Level Findings

Audit date: 2026-04-07
Scope: `chronicle-bot/voice-capture/src/` only. No songbird or davey changes proposed.

---

## 1. Findings

### F1. OP5 heal timer (2s) races with epoch key retention (10s)

**Files:** `consent.rs:997`, `consent.rs:880`

The continuous monitoring path waits 2 seconds after an OP5 event before declaring an SSRC "broken" and triggering a heal. The DAVE protocol spec says receivers retain previous epoch keys for **up to 10 seconds** during transitions. A normal epoch transition (e.g. a human joining the channel, triggering MLS renegotiation) will temporarily make a speaker's SSRC disappear from `ssrcs_seen` and reappear once the new epoch's keys are applied — but this can take several seconds, well within the 10s retention window.

With a 2s timer, the bot will misinterpret a healthy epoch transition as a DAVE failure and force a heal (leave + rejoin), which itself triggers *another* epoch transition for every other member in the channel. In a channel with multiple humans joining in quick succession, this creates a cascade: each join triggers a renegotiation, the bot heals prematurely, the heal triggers another renegotiation, and so on.

**Risk:** HIGH — this is the most likely cause of unnecessary reconnects in production. Every human join during a recording could trigger a spurious heal.

**Recommended fix:** Increase the OP5-to-VoiceTick confirmation window from 2s to 10s in the continuous monitoring path (line 997). The initial startup check (line 880) already uses a 2s grace *after* a 5s sleep, totaling 7s, which is close enough. But the continuous monitor at line 997 should use 10s to match the protocol's epoch key retention period.

---

### F2. DAVE heal is limited to one reconnect per session

**Files:** `consent.rs:1005-1012`, `consent.rs:824`

The comment at line 824 says "the heal is still one-shot — only one reconnect per session." After the first heal, the `healed` flag is set to `true` and all subsequent broken-SSRC detections are logged but ignored (line 1009: "already healed once, cannot retry"). The fallback interval also skips when `healed` is true (line 1042).

This means if DAVE breaks a second time during a long recording session (e.g. a second human joins 30 minutes later and triggers another MLS renegotiation failure), the bot has no recovery path. Audio silently degrades for the rest of the session.

**Risk:** MEDIUM — depends on session length and how often the channel membership changes. Short sessions with stable membership are fine. Multi-hour sessions where people come and go are at risk.

**Recommended fix:** Replace the boolean `healed` flag with a counter (e.g. `heal_count`) and allow up to N heals (say, 3) with exponential backoff. After each heal, wait progressively longer (5s, 10s, 20s) before declaring stable. Add a cooldown (e.g. 60s) between heals to prevent cascading reconnects from F1.

---

### F3. No `ssrcs_seen` / `audio_received` reset after heal reconnect

**Files:** `session.rs:384-403` (`reattach_audio`)

When `reattach_audio` is called during a heal, it creates a new `ssrc_map` and `op5_rx` channel, but it reuses the existing `ssrcs_seen` and `audio_received` without clearing them. After a heal reconnect:

- `audio_received` remains `true` from before the heal, so `wait_for_dave`-style checks would instantly pass even if the new connection hasn't established DAVE yet.
- `ssrcs_seen` still contains SSRCs from the old connection. Since the heal task checks `ssrcs_seen.contains(&evt.ssrc)` to skip known SSRCs (line 990-993), a re-announced SSRC after reconnect will be silently skipped, defeating the heal's purpose.

This is somewhat mitigated because the continuous monitor already consumed the OP5 event that triggered the heal, and after heal it swaps to the new `op5_rx`. But if the same SSRC fires OP5 again on the new connection, the stale `ssrcs_seen` entry causes it to be skipped.

**Risk:** MEDIUM — the heal may appear to succeed (logs say "dave_heal_complete") but the new connection may not actually be decrypting that speaker. The 5s post-heal sleep (line 1146) is meant to cover this, but there's no verification.

**Recommended fix:** Clear `ssrcs_seen` in `reattach_audio` after a heal. Optionally reset `audio_received` to `false` and verify it flips back within a timeout after heal. This gives a positive confirmation that the new connection is working.

---

### F4. No handling of voice gateway close code 4017 (DAVE required)

**Files:** `main.rs` (entire EventHandler impl), `voice/events.rs`

Close code 4017 means the server requires DAVE but the client didn't negotiate it. The bot has no handler for `DriverDisconnect` or any voice gateway close codes. If songbird exposes this event via `CoreEvent::DriverDisconnect`, the bot doesn't subscribe to it. If songbird silently drops the connection, the bot's only detection path is the 30s dead-connection fallback in the heal task (line 1055: "no SSRCs seen 30s after stable").

**Risk:** LOW — songbird's `next` branch likely handles DAVE negotiation internally via the Identify payload. But if something goes wrong (e.g. songbird doesn't set `max_dave_protocol_version`), the bot would sit silent for 30 seconds before detecting the problem, and then the one-shot heal limit (F2) means only one recovery attempt.

**Recommended fix:** Subscribe to `CoreEvent::DriverDisconnect` in the `AudioReceiver::attach` call. Log the close code. If it's 4017, immediately trigger a heal rather than waiting for the 30s fallback. This requires checking whether songbird exposes close codes through the event context.

---

### F5. No audio discard window after reconnect

**Files:** `voice/receiver.rs:247-287` (VoiceTick handler)

After a DAVE heal reconnect, the bot immediately starts buffering all decoded audio. During the first few seconds of a new connection, songbird may deliver frames that were decrypted with stale epoch keys (the protocol retains old keys for up to 10s). These frames could contain garbage audio — correctly decrypted with the wrong key, producing plausible-looking but corrupted PCM.

There is no way to distinguish "correctly decrypted real audio" from "correctly decrypted with stale key = garbage" at the application level, since both produce valid i16 PCM samples. However, the RMS energy diagnostic (lines 210-224) could be used heuristically: a sudden shift from ~0 RMS to high RMS after reconnect is normal, but a burst of abnormally high-energy noise could indicate stale-key decryption.

**Risk:** LOW-MEDIUM — depends on whether songbird correctly invalidates old decryptors during reconnect. If it does, this is a non-issue. If it doesn't, the first few seconds of audio after every heal contain garbage that gets uploaded to S3 and fed to the pipeline.

**Recommended fix:** Add a configurable "mute window" after reconnect — discard all audio packets for the first N seconds (e.g. 3s) after `reattach_audio` is called. This can be implemented as a timestamp on the `AudioReceiver` that's checked in the VoiceTick handler: if `now - reattach_time < MUTE_WINDOW`, skip the packet. The start announcement (which plays after the 5s post-heal sleep) naturally provides a gap, so this mainly protects against edge cases.

---

### F6. Mid-session human joins don't trigger DAVE health verification

**Files:** `voice/events.rs:60-69` (`handle_mid_session_join`)

When a human joins during an active recording, the code posts a consent prompt but does **not** verify that DAVE renegotiation succeeded. A new member joining the channel triggers an MLS proposal+commit cycle. If davey's `process_proposals()` bug (clearing `pending_commit` on new proposals, as documented in davey issue #15) is hit, the new member's audio may never decrypt — but the existing members' decryptors could also break.

The bot has no detection path for this except the continuous OP5 monitor, which relies on the new member starting to transmit (OP5) and then failing to appear in `ssrcs_seen` within 2s. But if the new member doesn't transmit immediately (they're listening first), the breakage in existing members' decryptors goes undetected until someone else triggers OP5.

**Risk:** HIGH — this is the primary real-world scenario. A player joins 10 minutes into a session, the MLS commit pipeline races, and audio for one or more existing speakers silently drops to silence. The heal task won't detect it because OP5 won't re-fire for already-speaking members.

**Recommended fix:** After a voice_state_update that adds a new member to the active channel during Recording phase, start a 10s timer. After 10s, verify that all previously-healthy SSRCs in `ssrcs_seen` are still delivering decoded audio (non-zero RMS). If any have gone silent, trigger a heal. This is a "post-join health check" — a new concept the current code doesn't have.

---

### F7. Songbird DecodeConfig uses defaults — no explicit DAVE tuning

**Files:** `main.rs:189`

```rust
SongbirdConfig::default().decode_mode(DecodeMode::Decode(DecodeConfig::default()));
```

`DecodeConfig::default()` is used without any explicit DAVE-related configuration. Songbird's `next` branch added DAVE support, but the bot doesn't verify or configure:
- Whether `max_dave_protocol_version` is set in the Identify payload (songbird likely handles this, but it's not confirmed)
- Whether the encryption mode is optimal for the use case
- Whether PLC (Packet Loss Concealment) behavior during DAVE key transitions is appropriate

**Risk:** LOW — songbird's defaults are likely correct for DAVE. But the lack of explicit configuration means a songbird update could change defaults and silently break DAVE support.

**Recommended fix:** Pin the DecodeConfig explicitly with the fields that matter:
```rust
DecodeConfig {
    plc: true,                    // PLC during transitions
    ..DecodeConfig::default()
}
```
Add a comment documenting which songbird commit/version the DAVE support was validated against.

---

### F8. SpeakingStateUpdate (OP5) may not fire for all speakers

**Files:** `voice/receiver.rs:174`, `consent.rs:910-941`

Songbird PR #291 notes: "SpeakingStateUpdate events are apparently not fired consistently." The heal task's fallback check (lines 910-941) partially addresses this — it compares `ssrc_map` size against `consented_count` and detects when OP5 was edge-triggered away. But this check only runs during initial startup, not during continuous monitoring after `healed` is set.

The continuous fallback interval (line 1041) does run every 10s, but its check at line 1048 compares `seen_count > 0 && mapped_count < consented_count`, which uses the stale `consented_count` captured at task start. If a mid-session joiner was added to `consented_users`, the `consented_count` is now wrong, and the check may not trigger.

**Risk:** MEDIUM — if a speaker's OP5 never fires (songbird bug or Discord quirk) AND the fallback's `consented_count` is stale, that speaker's DAVE failure goes undetected.

**Recommended fix:** Re-read `consented_users.len()` on each fallback tick instead of using the stale `consented_count` from task start. This is a one-line fix: capture `consented_users` Arc at task start, lock it on each tick.

---

### F9. Songbird log level set to `error` — DAVE diagnostics invisible

**Files:** `main.rs:129`

```rust
.add_directive("songbird=error".parse().unwrap())
.add_directive("songbird::driver::tasks::udp_rx=off".parse().unwrap())
```

Songbird's DAVE-related log messages (key ratchet events, epoch transitions, decryptor failures) are at `debug` or `info` level. Setting songbird to `error` means none of these are visible in production logs. The `udp_rx=off` directive further silences the receive path entirely.

During a DAVE failure investigation, the first thing you'd want to see is songbird's internal DAVE state — but it's all filtered out.

**Risk:** LOW (operational, not functional) — doesn't cause bugs, but makes debugging DAVE issues significantly harder.

**Recommended fix:** Add a `SONGBIRD_LOG_LEVEL` env var (defaulting to `error`) so DAVE debugging can be enabled without a code change. In the tracing subscriber:
```rust
.add_directive(format!("songbird={}", std::env::var("SONGBIRD_LOG_LEVEL").unwrap_or("error".into())).parse().unwrap())
```

---

### F10. No DAVE-specific metrics for epoch transitions

**Files:** All metrics are in `receiver.rs` and `consent.rs`

The existing metrics (`chronicle_dave_attempts_total`, `chronicle_audio_packets_received`) track initial DAVE establishment and packet flow, but there are no metrics for:
- Epoch transitions (how often MLS renegotiates during a session)
- Temporary decryption gaps (periods where `decoded_voice` is `None` for an active SSRC)
- Heal trigger reasons (OP5 timeout vs. fallback vs. dead connection)
- Post-heal recovery time

**Risk:** LOW (operational) — doesn't cause bugs, but makes it hard to assess DAVE health across sessions.

**Recommended fix:** Add counters for `ttrpg_dave_heal_trigger` with a `reason` label (op5_timeout, fallback_check, dead_connection), and a histogram for `ttrpg_dave_epoch_gap_seconds` tracking periods where a previously-active SSRC stops delivering decoded audio.

---

## 2. Risk Summary

| # | Finding | Risk | Effort |
|---|---------|------|--------|
| F1 | 2s OP5 timer vs 10s epoch retention | HIGH | Small (change one constant) |
| F2 | One-shot heal limit | MEDIUM | Medium (refactor heal state) |
| F3 | No ssrcs_seen/audio_received reset after heal | MEDIUM | Small (add `.clear()` calls) |
| F4 | No close code 4017 handling | LOW | Medium (new event subscription) |
| F5 | No audio discard window after reconnect | LOW-MEDIUM | Medium (new timestamp check) |
| F6 | Mid-session joins don't verify DAVE health | HIGH | Medium (new post-join timer) |
| F7 | DecodeConfig uses defaults | LOW | Small (pin config fields) |
| F8 | Stale consented_count in fallback check | MEDIUM | Small (re-read on each tick) |
| F9 | Songbird logs filtered to error | LOW | Small (env var) |
| F10 | No epoch transition metrics | LOW | Small (add counters) |

---

## 3. Quick Wins (minimal risk, can ship immediately)

### QW1. Increase OP5 confirmation window to 10s (F1)

`consent.rs:997` — change `Duration::from_secs(2)` to `Duration::from_secs(10)` in the continuous monitoring path. This is a single constant change. The initial startup check at line 880 can stay at 2s since it already has a 5s grace period before it.

### QW2. Clear ssrcs_seen in reattach_audio (F3)

`session.rs:384` — add `phase.ssrcs_seen.lock().expect("ssrcs_seen poisoned").clear();` at the top of `reattach_audio`, before attaching the new receiver. Two lines of code.

### QW3. Fix stale consented_count (F8)

`consent.rs:840-852` — capture the `consented_users` Arc (not just its `.len()`) at task start. On each fallback tick (line 1048), lock it and read `.len()` fresh. Alternatively, pass the Arc into the task and call `.lock().await.len()` in the fallback branch.

### QW4. Add configurable songbird log level (F9)

`main.rs:129` — replace the hardcoded `"songbird=error"` with a runtime-configurable directive from an env var. Five lines of code.

### QW5. Add heal trigger reason labels (F10)

`consent.rs:949`, `consent.rs:1017`, `consent.rs:1053`, `consent.rs:1060` — add a `reason` label to the existing `metrics::counter!` calls so each heal trigger path is distinguishable in dashboards.

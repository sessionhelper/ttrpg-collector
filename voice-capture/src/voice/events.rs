//! Voice state change dispatch.
//!
//! Post-refactor: the actor owns mid-session-join and auto-stop logic.
//! This module computes the cache data the actor needs and forwards the event
//! via `SessionCmd::VoiceStateChange`. No bypass, no `state.sessions.lock()`.

use std::sync::Arc;

use serenity::all::*;

use crate::session::actor::SessionCmd;
use crate::state::AppState;

pub async fn handle_voice_state_update(
    ctx: Context,
    old: Option<VoiceState>,
    new: VoiceState,
    state: Arc<AppState>,
) {
    let Some(guild_id) = new.guild_id else { return };
    let Some(entry) = state.sessions.get(&guild_id.get()) else {
        return;
    };
    if entry.is_shutting_down() {
        return;
    }
    let handle = entry.clone();

    let guild = match ctx.cache.guild(guild_id) {
        Some(g) => g.clone(),
        None => return,
    };

    // `is_bot` here means "is this voice-state event about OUR OWN bot"
    // — i.e. something the session actor should ignore (our own join
    // when we enter voice, our own leave on finalize). It does NOT mean
    // "this user account has Discord's bot flag set"; other Discord bot
    // accounts (including the harness feeder fleet) are legitimate test
    // users whose audio we need to capture, so they must flow through
    // the normal auto-enrol path. Matching on our own user_id is the
    // only condition that warrants ignoring a voice-state.
    let is_bot = new.user_id == ctx.cache.current_user().id;
    let display_name = guild
        .members
        .get(&new.user_id)
        .map(|m| m.display_name().to_string())
        .unwrap_or_else(|| format!("User {}", new.user_id));

    let _ = handle
        .send(SessionCmd::VoiceStateChange {
            old_channel: old.as_ref().and_then(|o| o.channel_id),
            new_channel: new.channel_id,
            user_id: new.user_id,
            is_bot,
            display_name,
        })
        .await;
}

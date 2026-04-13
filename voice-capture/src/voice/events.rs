//! Voice state change dispatch.
//!
//! Post-refactor: the actor owns mid-session-join and auto-stop logic.
//! This module computes the tiny amount of cache data the actor needs
//! (joining user's display name + bot flag) and forwards the event via
//! `SessionCmd::VoiceStateChange`. No `state.sessions.lock()` here.

use std::sync::Arc;

use serenity::all::*;

use crate::session::actor::SessionCmd;
use crate::state::AppState;

/// Entry point called from `Handler::voice_state_update`.
pub async fn handle_voice_state_update(
    ctx: Context,
    old: Option<VoiceState>,
    new: VoiceState,
    state: Arc<AppState>,
) {
    let guild_id = match new.guild_id {
        Some(id) => id,
        None => return,
    };

    let handle = match state.sessions.get(&guild_id.get()) {
        Some(entry) if !entry.is_shutting_down() => entry.clone(),
        _ => return,
    };

    let guild = match ctx.cache.guild(guild_id) {
        Some(g) => g.clone(),
        None => return,
    };

    let is_bot = guild
        .members
        .get(&new.user_id)
        .is_some_and(|m| m.user.bot);
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

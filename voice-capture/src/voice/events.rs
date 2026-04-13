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

    let is_bot = guild.members.get(&new.user_id).is_some_and(|m| m.user.bot);
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

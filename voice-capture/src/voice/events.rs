//! Voice state change handling: mid-session joins and auto-stop.
//!
//! Serenity's `voice_state_update` event fires whenever someone joins,
//! leaves, mutes, deafens, or otherwise changes voice state. We care about
//! two cases:
//!
//!   1. **Mid-session join**: a new user enters the voice channel while a
//!      recording is active. Their audio is captured only after they
//!      explicitly consent via the per-user prompt posted to the text
//!      channel. Handled in [`handle_mid_session_join`].
//!
//!   2. **Channel empty**: the last human left the voice channel. After a
//!      30-second grace period (to absorb brief disconnects), we finalize
//!      the session. Handled in [`schedule_auto_stop`].
//!
//! The top-level entry point [`handle_voice_state_update`] is called from
//! `main.rs`'s EventHandler impl and dispatches to the two helpers.

use std::sync::Arc;

use serenity::all::*;
use tracing::{error, info, warn};

use crate::commands;
use crate::session::{consent_buttons, Phase};
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

    // Only act on guilds that have an active Recording session. Startup
    // and terminal phases are ignored — mid-session joiners don't make
    // sense before DAVE confirms, and auto_stop shouldn't fire on a
    // session that's already finalizing.
    let (channel_id, text_channel_id) = {
        let sessions = state.sessions.lock().await;
        match sessions.get(guild_id.get()) {
            Some(s) if matches!(s.phase, Phase::Recording(_)) => (
                ChannelId::new(s.channel_id),
                ChannelId::new(s.text_channel_id),
            ),
            _ => return,
        }
    };

    let joined = new.channel_id == Some(channel_id);
    let was_in = old
        .as_ref()
        .is_some_and(|o| o.channel_id == Some(channel_id));

    if joined && !was_in {
        handle_mid_session_join(
            &ctx,
            &state,
            guild_id,
            new.user_id,
            channel_id,
            text_channel_id,
        )
        .await;
    }

    // Auto-stop arming: if the channel is now empty, schedule a timer.
    // If someone just rejoined and the channel isn't empty any more, abort
    // any pending timer so we don't auto-stop on them after they leave
    // again via a different path.
    match channel_humans(&ctx, guild_id, channel_id) {
        Some(0) => schedule_auto_stop(ctx.clone(), state.clone(), guild_id, channel_id).await,
        Some(_) => {
            let mut sessions = state.sessions.lock().await;
            if let Some(s) = sessions.get_mut(guild_id.get()) {
                s.abort_auto_stop();
            }
        }
        None => {}
    }
}

// ---------------------------------------------------------------------------
// Mid-session join
// ---------------------------------------------------------------------------

async fn handle_mid_session_join(
    ctx: &Context,
    state: &Arc<AppState>,
    guild_id: GuildId,
    user_id: UserId,
    _channel_id: ChannelId,
    text_channel_id: ChannelId,
) {
    let guild = match ctx.cache.guild(guild_id) {
        Some(g) => g.clone(),
        None => return,
    };
    if guild.members.get(&user_id).is_some_and(|m| m.user.bot) {
        return;
    }

    let already_participant = {
        let sessions = state.sessions.lock().await;
        sessions
            .get(guild_id.get())
            .is_some_and(|s| s.participants.contains_key(&user_id))
    };
    if already_participant {
        return;
    }

    // Blocklist check — if the user has globally opted out, skip the
    // consent prompt entirely. Errors fall through and allow the prompt;
    // better to ask than to silently drop a joiner.
    match state.api.check_blocklist(user_id.get()).await {
        Ok(true) => {
            info!(user_id = %user_id, "mid_session_joiner_blocked — user opted out globally");
            return;
        }
        Err(e) => {
            error!("API call failed (check_blocklist): {e}");
        }
        _ => {}
    }

    let display_name = guild
        .members
        .get(&user_id)
        .map(|m| m.display_name().to_string())
        .unwrap_or_else(|| format!("User {user_id}"));

    // Register locally and push to Data API. Cache the returned UUID on
    // the local Session so the joiner's eventual consent click hits the
    // fast path instead of find_participant.
    let session_id_str = {
        let mut sessions = state.sessions.lock().await;
        sessions.get_mut(guild_id.get()).map(|session| {
            session.add_participant(user_id, display_name.clone(), true);
            session.id.clone()
        })
    };

    if let Some(sid_str) = &session_id_str
        && let Ok(sid) = uuid::Uuid::parse_str(sid_str)
    {
        match state.api.add_participant(sid, user_id.get(), true).await {
            Ok(row) => {
                let mut sessions = state.sessions.lock().await;
                if let Some(s) = sessions.get_mut(guild_id.get()) {
                    s.set_participant_uuid(user_id, row.id);
                }
            }
            Err(e) => error!("API call failed (add_participant mid-session): {e}"),
        }
    }

    info!(user_id = %user_id, name = %display_name, "mid_session_joiner");

    let embed = {
        let sessions = state.sessions.lock().await;
        match sessions.get(guild_id.get()) {
            Some(s) => s.consent_embed(),
            None => return,
        }
    };

    let msg = CreateMessage::new()
        .content(format!(
            "<@{user_id}> joined the voice channel during recording. \
             Their audio is **not being captured** until they consent."
        ))
        .embed(embed)
        .components(vec![consent_buttons()]);

    if let Err(e) = text_channel_id.send_message(&ctx.http, msg).await {
        warn!(error = %e, "failed to send mid-session consent prompt");
    }
}

// ---------------------------------------------------------------------------
// Auto-stop
// ---------------------------------------------------------------------------

/// Count non-bot users currently in the given voice channel. Returns None
/// if the guild isn't cached.
fn channel_humans(ctx: &Context, guild_id: GuildId, channel_id: ChannelId) -> Option<usize> {
    let guild = ctx.cache.guild(guild_id)?.clone();
    Some(
        guild
            .voice_states
            .values()
            .filter(|vs| vs.channel_id == Some(channel_id))
            .filter(|vs| {
                guild
                    .members
                    .get(&vs.user_id)
                    .is_none_or(|m| !m.user.bot)
            })
            .count(),
    )
}

/// Arm the auto-stop timer. If a previous timer is already pending for
/// this session (e.g. rapid leave/rejoin/leave sequence), abort it first
/// so we don't accumulate duplicate timers racing each other. The new
/// handle is stored on the Session so finalization / cleanup can abort
/// it too.
async fn schedule_auto_stop(
    ctx: Context,
    state: Arc<AppState>,
    guild_id: GuildId,
    channel_id: ChannelId,
) {
    let state_inner = state.clone();
    let ctx_inner = ctx.clone();
    let handle = tokio::spawn(async move {
        tokio::time::sleep(std::time::Duration::from_secs(30)).await;

        if channel_humans(&ctx_inner, guild_id, channel_id).is_none_or(|n| n > 0) {
            return;
        }

        info!(guild_id = %guild_id, "auto_stop — channel empty for 30s");

        let manager = songbird::get(&ctx_inner).await.unwrap();
        if let Some(call) = manager.get(guild_id) {
            let mut handler = call.lock().await;
            let source = songbird::input::File::new("/assets/recording_stopped.wav");
            let _ = handler.play_input(source.into());
            drop(handler);
            tokio::time::sleep(std::time::Duration::from_secs(2)).await;
        }

        let _ = manager.leave(guild_id).await;
        commands::stop::auto_stop(&ctx_inner, guild_id.get(), &state_inner).await;
    });

    // Replace any existing pending timer; abort the old handle.
    let mut sessions = state.sessions.lock().await;
    if let Some(s) = sessions.get_mut(guild_id.get()) {
        s.abort_auto_stop();
        s.auto_stop_task = Some(handle);
    } else {
        // Session vanished between the outer lock and here — drop the
        // handle; the timer task will no-op on its check.
        handle.abort();
    }
}

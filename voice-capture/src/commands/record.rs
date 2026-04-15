//! `/record` slash command.
//!
//! Per the locked spec (F2/F6), the bot does not talk to the data-api at all
//! until the stabilization gate opens. This handler's job is narrow:
//!
//! 1. Validate the invoker is in a voice channel.
//! 2. Build a `Session` with the voice-channel humans pre-enrolled as
//!    Pending participants.
//! 3. Spawn the per-guild actor.
//! 4. Post the consent embed into the text channel.
//!
//! Blocklist checks, the `POST /internal/sessions` row insert, and the
//! `participants/batch` insert all move into the actor's gate-open path.
//! If the gate never opens (decline, timeout, /stop before gate), the
//! data-api never hears about the session at all — that is intentional.

use std::sync::Arc;

use serenity::all::*;
use tracing::{error, info};

use crate::commands::respond::{respond, InteractionReply};
use crate::session::actor::{spawn_session, SessionCmd};
use crate::session::Session;
use crate::state::AppState;

#[tracing::instrument(skip(ctx, command, state), fields(guild_id = %command.guild_id.map(|g| g.get()).unwrap_or(0)))]
pub async fn handle_record(
    ctx: &Context,
    command: &CommandInteraction,
    state: &Arc<AppState>,
) -> Result<(), serenity::Error> {
    respond(ctx, command, || record_inner(ctx, command, state)).await
}

async fn record_inner(
    ctx: &Context,
    command: &CommandInteraction,
    state: &Arc<AppState>,
) -> InteractionReply {
    let Some(guild_id) = command.guild_id else {
        return InteractionReply::Edit(
            EditInteractionResponse::new().content("Run this in a server."),
        );
    };

    // Extract voice-channel members INSIDE a block so the `CacheRef` drops
    // before we `.await` — the dashmap guard it carries is `!Send`.
    let (channel_id, members) = {
        let Some(guild_arc) = ctx.cache.guild(guild_id) else {
            return InteractionReply::Edit(
                EditInteractionResponse::new().content("Guild cache not populated yet."),
            );
        };
        let guild = guild_arc;
        let Some(channel_id) = guild
            .voice_states
            .get(&command.user.id)
            .and_then(|vs| vs.channel_id)
        else {
            return InteractionReply::Edit(
                EditInteractionResponse::new().content("You need to be in a voice channel."),
            );
        };
        // Known bot UIDs in this channel — our own bot + any bot whose
        // member cache entry we actually have. Can't rely *only* on the
        // member cache because it's sometimes not hydrated yet at /record
        // time (observed empirically on a fresh guild).
        let our_bot_id = ctx.cache.current_user().id;
        let members: Vec<(UserId, String)> = guild
            .voice_states
            .iter()
            .filter(|(_, vs)| vs.channel_id == Some(channel_id))
            .map(|(uid, _)| *uid)
            .filter(|uid| *uid != our_bot_id)
            .map(|uid| {
                let display_name = guild
                    .members
                    .get(&uid)
                    .map(|m| m.display_name().to_string())
                    .unwrap_or_else(|| format!("user-{}", uid.get()));
                (uid, display_name)
            })
            // Drop any member-cache-confirmed bot (non-self). Cache-miss
            // defaults to treating the UID as human — safer than dropping
            // a real user because their cache hadn't filled.
            .filter(|(uid, _)| {
                guild.members.get(uid).map(|m| !m.user.bot).unwrap_or(true)
            })
            .collect();
        (channel_id, members)
    };

    let mut session = Session::new(
        guild_id.get(),
        channel_id.get(),
        command.channel_id.get(),
        command.user.id,
        state.config.min_participants,
        state.config.require_all_consent,
    );
    for (uid, name) in &members {
        session.add_participant(*uid, name.clone(), false);
    }
    let session_id = session.id.clone();

    let handle = match spawn_session(state.clone(), ctx.clone(), session) {
        Ok(h) => h,
        Err(_) => {
            return InteractionReply::Edit(
                EditInteractionResponse::new()
                    .content("A recording session is already active in this server."),
            );
        }
    };

    // Render the first consent embed. The actor has its own copy of the
    // Session; we build a display-only one here so we don't reach into the
    // actor's state from the handler.
    let mut display_session = Session::new(
        guild_id.get(),
        channel_id.get(),
        command.channel_id.get(),
        command.user.id,
        state.config.min_participants,
        state.config.require_all_consent,
    );
    for (uid, name) in &members {
        display_session.add_participant(*uid, name.clone(), false);
    }
    let embed = display_session.consent_embed();
    let mentions: String = members
        .iter()
        .map(|(uid, _)| format!("<@{}>", uid))
        .collect::<Vec<_>>()
        .join(" ");

    match command
        .edit_response(
            &ctx.http,
            EditInteractionResponse::new()
                .content(mentions)
                .embed(embed)
                .components(vec![crate::session::consent_buttons()]),
        )
        .await
    {
        Ok(msg) => {
            metrics::counter!("chronicle_sessions_total", "outcome" => "started").increment(1);
            let _ = handle
                .send(SessionCmd::SetConsentMessage {
                    channel_id: msg.channel_id,
                    message_id: msg.id,
                })
                .await;
            info!(session_id = %session_id, "session_spawned");
        }
        Err(e) => {
            error!(error = %e, "edit_response_failed — tearing down");
            let _ = handle.send(SessionCmd::AutoStop).await;
        }
    }

    InteractionReply::Silent
}

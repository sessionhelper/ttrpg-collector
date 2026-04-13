//! `/record` slash command.
//!
//! Spawns a per-guild actor, kicks off `Session::new`, and returns as soon as
//! the handle is in the DashMap. The actor handles voice-join, stabilization,
//! announcement, consent and finalization — this handler only assembles the
//! initial Session + posts the consent embed via the [`respond`] wrapper.

use std::sync::Arc;

use serenity::all::*;
use tracing::{error, info};

use crate::commands::respond::{respond, InteractionReply};
use crate::session::actor::{spawn_session, SessionCmd, SessionHandle};
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

    // Extract everything we need from the cache INSIDE a block so the
    // `CacheRef` drops before our first `.await`. `CacheRef` contains a
    // `dashmap` internal guard which is `!Send`.
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
        let members: Vec<(UserId, String)> = guild
            .voice_states
            .iter()
            .filter(|(_, vs)| vs.channel_id == Some(channel_id))
            .filter_map(|(uid, _)| {
                let member = guild.members.get(uid)?;
                if member.user.bot {
                    return None;
                }
                Some((*uid, member.display_name().to_string()))
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

    if let Err(e) = create_session_row(state, &session_id, guild_id.get()).await {
        error!(error = %e, "create_session_failed");
        let _ = handle.send(SessionCmd::AutoStop).await;
        return InteractionReply::Edit(
            EditInteractionResponse::new()
                .content("Couldn't reach storage backend. Try `/record` again."),
        );
    }

    if let Err(e) = enrol_members(state, &handle, &session_id, &members).await {
        error!(error = %e, "enrol_members_failed");
    }

    // Render the first consent embed.
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

async fn create_session_row(
    state: &Arc<AppState>,
    session_id: &str,
    guild_id: u64,
) -> Result<(), String> {
    let session_uuid = uuid::Uuid::parse_str(session_id).map_err(|e| e.to_string())?;
    let s3_prefix = format!("sessions/{}/{}", guild_id, session_id);
    state
        .api
        .create_session(
            session_uuid,
            guild_id as i64,
            chrono::Utc::now(),
            None,
            None,
            Some(s3_prefix),
        )
        .await
        .map_err(|e| e.to_string())?;
    Ok(())
}

async fn enrol_members(
    state: &Arc<AppState>,
    handle: &SessionHandle,
    session_id: &str,
    members: &[(UserId, String)],
) -> Result<(), String> {
    if members.is_empty() {
        return Ok(());
    }
    let session_uuid = uuid::Uuid::parse_str(session_id).map_err(|e| e.to_string())?;

    // Async follow-up #1: parallel blocklist checks via JoinSet.
    let mut set: tokio::task::JoinSet<(usize, Result<bool, String>)> = tokio::task::JoinSet::new();
    for (i, (uid, _)) in members.iter().enumerate() {
        let api = state.api.clone();
        let uid = uid.get();
        set.spawn(async move {
            (
                i,
                api.check_blocklist(uid).await.map_err(|e| e.to_string()),
            )
        });
    }
    let mut blocked = vec![false; members.len()];
    while let Some(join_res) = set.join_next().await {
        let (i, res) = match join_res {
            Ok(v) => v,
            Err(e) => return Err(format!("blocklist_join_error: {e}")),
        };
        blocked[i] = res?;
    }

    let allowed: Vec<(UserId, String)> = members
        .iter()
        .enumerate()
        .filter_map(|(i, (uid, name))| {
            if blocked[i] {
                info!(user_id = %uid, "participant_blocklisted — skip");
                None
            } else {
                Some((*uid, name.clone()))
            }
        })
        .collect();

    let batch_input: Vec<(u64, bool, Option<String>)> = allowed
        .iter()
        .map(|(uid, name)| (uid.get(), false, Some(name.clone())))
        .collect();
    match state.api.add_participants_batch(session_uuid, &batch_input).await {
        Ok(rows) => {
            for ((uid, _), row) in allowed.iter().zip(rows.iter()) {
                let _ = handle
                    .send(SessionCmd::SetParticipantUuid {
                        user_id: *uid,
                        participant_uuid: row.id,
                    })
                    .await;
            }
        }
        Err(e) => return Err(e.to_string()),
    }
    Ok(())
}

//! `/record` slash command handler.

use serenity::all::*;
use tracing::error;

use crate::session::{consent_buttons, Session};
use crate::state::AppState;

/// Handle the /record slash command.
///
/// Flow (structured to avoid the TOCTOU race between active-session check
/// and session insert):
///   1. Cheap in-process validation — voice channel, min participants
///   2. Build the Session struct (no network calls)
///   3. Atomically reserve the guild's slot via SessionManager::try_insert;
///      if a session already exists, reply and bail
///   4. Defer the interaction — now we have 15 minutes for the slow work
///   5. Persist to Data API (create_session, add_participant per member)
///   6. Edit the deferred response with the consent embed
///   7. If step 5 or 6 fails, roll back by removing the session from the
///      manager and best-effort marking the Data API row abandoned
#[tracing::instrument(skip(ctx, command, state), fields(guild_id = %command.guild_id.unwrap()))]
pub async fn handle_record(
    ctx: &Context,
    command: &CommandInteraction,
    state: &AppState,
) -> Result<(), serenity::Error> {
    let guild_id = command.guild_id.unwrap();

    // Find the user's voice channel (cache lookup, instant)
    let guild = ctx.cache.guild(guild_id).unwrap().clone();
    let channel_id = guild
        .voice_states
        .get(&command.user.id)
        .and_then(|vs| vs.channel_id);

    let channel_id = match channel_id {
        Some(id) => id,
        None => {
            command
                .create_response(
                    &ctx.http,
                    CreateInteractionResponse::Message(
                        CreateInteractionResponseMessage::new()
                            .content("You need to be in a voice channel.")
                            .ephemeral(true),
                    ),
                )
                .await?;
            return Ok(());
        }
    };

    // Gather non-bot members in the voice channel (cache, instant)
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

    if members.len() < state.config.min_participants {
        command
            .create_response(
                &ctx.http,
                CreateInteractionResponse::Message(
                    CreateInteractionResponseMessage::new()
                        .content(format!(
                            "Need at least {} people in the voice channel.",
                            state.config.min_participants
                        ))
                        .ephemeral(true),
                ),
            )
            .await?;
        return Ok(());
    }

    // Build the session locally (no network calls in the constructor)
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

    // Atomically reserve the guild's slot. If another /record is in flight
    // for this guild, bail instantly — no Data API rows created, no Discord
    // side-effects, no lock held across await points.
    {
        let mut sessions = state.sessions.lock().await;
        if let Err(_rejected) = sessions.try_insert(session) {
            drop(sessions);
            command
                .create_response(
                    &ctx.http,
                    CreateInteractionResponse::Message(
                        CreateInteractionResponseMessage::new()
                            .content("A recording session is already active in this server.")
                            .ephemeral(true),
                    ),
                )
                .await?;
            return Ok(());
        }
    }

    // Slot reserved. Defer the Discord interaction so the slow work below
    // can run without fighting the 3-second response window.
    command
        .create_response(
            &ctx.http,
            CreateInteractionResponse::Defer(CreateInteractionResponseMessage::new()),
        )
        .await?;

    // Persist session and participants to Data API. Track the first hard
    // failure so we can roll back cleanly — individual add_participant
    // failures are logged but not treated as fatal.
    let session_uuid = uuid::Uuid::parse_str(&session_id).ok();
    let mut create_failed = false;
    if let Some(sid) = session_uuid {
        let s3_prefix = format!("sessions/{}/{}", guild_id.get(), session_id);
        if let Err(e) = state
            .api
            .create_session(
                sid,
                guild_id.get() as i64,
                chrono::Utc::now(),
                None,
                None,
                Some(s3_prefix),
            )
            .await
        {
            error!("API call failed (create_session): {e}");
            create_failed = true;
        }

        if !create_failed {
            // Blocklist check per user first. Users who've opted out
            // globally are dropped from the batch before it's sent.
            let mut accepted: Vec<(UserId, u64)> = Vec::with_capacity(members.len());
            for (uid, _name) in &members {
                match state.api.check_blocklist(uid.get()).await {
                    Ok(true) => {
                        tracing::info!(
                            user_id = %uid,
                            "participant_blocked — user opted out globally, skipping"
                        );
                        continue;
                    }
                    Err(e) => {
                        error!("API call failed (check_blocklist): {e}");
                    }
                    _ => {}
                }
                accepted.push((*uid, uid.get()));
            }

            // Single batch insert + single response → one HTTP round trip
            // regardless of party size. Cache each returned UUID on the
            // local Session so consent/license clicks hit the fast path.
            let batch_input: Vec<(u64, bool)> = accepted
                .iter()
                .map(|(_, raw)| (*raw, false))
                .collect();
            match state.api.add_participants_batch(sid, &batch_input).await {
                Ok(rows) => {
                    let count = rows.len();
                    let mut sessions = state.sessions.lock().await;
                    if let Some(s) = sessions.get_mut(guild_id.get()) {
                        for ((user_id, _), row) in accepted.iter().zip(rows.iter()) {
                            s.set_participant_uuid(*user_id, row.id);
                        }
                    }
                    tracing::info!(
                        participants = count,
                        "participants_batched_and_cached",
                    );
                }
                Err(e) => {
                    error!("API call failed (add_participants_batch): {e}");
                }
            }
        }
    }

    // If the Data API session row couldn't be created, roll back the local
    // reservation so a retry isn't blocked by a zombie entry.
    if create_failed {
        let mut sessions = state.sessions.lock().await;
        sessions.remove(guild_id.get());
        drop(sessions);
        let _ = command
            .edit_response(
                &ctx.http,
                EditInteractionResponse::new()
                    .content("Couldn't reach the storage backend. Try `/record` again in a moment."),
            )
            .await;
        return Ok(());
    }

    // Build the consent embed now that the reserved session has participants.
    // Re-borrow the reserved session to read the embed state.
    let (embed, buttons) = {
        let sessions = state.sessions.lock().await;
        match sessions.get(guild_id.get()) {
            Some(s) => (s.consent_embed(), consent_buttons()),
            None => {
                // The session was removed between reservation and here —
                // e.g. by /stop or auto_stop. Nothing more to do.
                let _ = command
                    .edit_response(
                        &ctx.http,
                        EditInteractionResponse::new()
                            .content("Session was cancelled. Try `/record` again."),
                    )
                    .await;
                return Ok(());
            }
        }
    };

    let mentions: String = members
        .iter()
        .map(|(uid, _)| format!("<@{}>", uid))
        .collect::<Vec<_>>()
        .join(" ");

    // Fill in the deferred response with the consent embed. On failure,
    // roll back the reservation and (best-effort) mark the Data API session
    // abandoned so we don't leak a row.
    match command
        .edit_response(
            &ctx.http,
            EditInteractionResponse::new()
                .content(mentions)
                .embed(embed)
                .components(vec![buttons]),
        )
        .await
    {
        Ok(msg) => {
            metrics::counter!("ttrpg_sessions_total", "outcome" => "started").increment(1);
            let mut sessions = state.sessions.lock().await;
            if let Some(s) = sessions.get_mut(guild_id.get()) {
                s.consent_message = Some((msg.channel_id, msg.id));
            }
        }
        Err(e) => {
            error!(error = %e, "edit_response_failed — rolling back reservation");
            let mut sessions = state.sessions.lock().await;
            sessions.remove(guild_id.get());
            drop(sessions);
            if let Some(sid) = session_uuid
                && let Err(api_e) = state.api.update_session_state(sid, "abandoned").await
            {
                error!("API call failed (update_session_state=abandoned): {api_e}");
            }
            return Err(e);
        }
    }

    Ok(())
}

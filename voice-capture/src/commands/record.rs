//! `/record` slash command handler.

use serenity::all::*;
use tracing::error;

use crate::session::{consent_buttons, Session};
use crate::state::AppState;

/// Handle the /record slash command.
/// Creates a new Session in AwaitingConsent phase, adds all voice channel
/// participants, and sends the consent embed. The session is only stored
/// after Discord accepts the response — if the Discord call fails, no
/// orphaned state is left behind.
#[tracing::instrument(skip(ctx, command, state), fields(guild_id = %command.guild_id.unwrap()))]
pub async fn handle_record(
    ctx: &Context,
    command: &CommandInteraction,
    state: &AppState,
) -> Result<(), serenity::Error> {
    let guild_id = command.guild_id.unwrap();

    // Reject if there's already an active session for this guild
    {
        let sessions = state.sessions.lock().await;
        if sessions.has_active(guild_id.get()) {
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

    // Find the user's voice channel
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

    // Gather non-bot members in the voice channel
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

    // Create the unified session — all state lives here
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

    // Persist session and participants to Data API (non-blocking, best-effort)
    if let Ok(session_uuid) = uuid::Uuid::parse_str(&session.id) {
        let s3_prefix = format!("sessions/{}/{}", guild_id.get(), session.id);
        if let Err(e) = state
            .api
            .create_session(
                session_uuid,
                guild_id.get() as i64,
                chrono::Utc::now(),
                None,
                None,
                Some(s3_prefix),
            )
            .await
        {
            error!("API call failed (create_session): {e}");
        }

        // Add each participant, checking blocklist first
        for (uid, _name) in &members {
            match state.api.check_blocklist(uid.get()).await {
                Ok(true) => {
                    tracing::info!(user_id = %uid, "participant_blocked — user opted out globally, skipping");
                    continue;
                }
                Err(e) => {
                    error!("API call failed (check_blocklist): {e}");
                }
                _ => {}
            }

            if let Err(e) = state
                .api
                .add_participant(session_uuid, uid.get(), false)
                .await
            {
                error!("API call failed (add_participant): {e}");
            }
        }
    }

    let embed = session.consent_embed();
    let buttons = consent_buttons();

    let mentions: String = members
        .iter()
        .map(|(uid, _)| format!("<@{}>", uid))
        .collect::<Vec<_>>()
        .join(" ");

    // Respond to Discord FIRST — if this fails, don't store the session
    let response_result = command
        .create_response(
            &ctx.http,
            CreateInteractionResponse::Message(
                CreateInteractionResponseMessage::new()
                    .content(mentions)
                    .embed(embed)
                    .components(vec![buttons]),
            ),
        )
        .await;

    match response_result {
        Ok(_) => {
            metrics::counter!("ttrpg_sessions_total", "outcome" => "started").increment(1);
            // Store the consent message ID so we can clean it up on session end
            if let Ok(msg) = command.get_response(&ctx.http).await {
                session.consent_message = Some((msg.channel_id, msg.id));
            }
            // Only store the session after Discord accepted the response
            let mut sessions = state.sessions.lock().await;
            sessions.insert(session);
        }
        Err(e) => {
            // No cleanup needed — session was never stored
            return Err(e);
        }
    }

    Ok(())
}

use serenity::all::*;
use tracing::error;

use crate::consent::*;
use crate::consent::embeds::consent_buttons;
use crate::db;
use crate::state::AppState;

pub async fn handle_record(
    ctx: &Context,
    command: &CommandInteraction,
    state: &AppState,
) -> Result<(), serenity::Error> {
    let guild_id = command.guild_id.unwrap();

    // Check for active session
    {
        let manager = state.consent.lock().await;
        if manager.has_active_session(guild_id.get()) {
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

    // Get members in the voice channel
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

    // Create session
    let session_id = uuid::Uuid::new_v4().to_string();
    let mut session = ConsentSession::new(
        guild_id.get(),
        channel_id.get(),
        command.channel_id.get(),
        command.user.id,
        session_id.clone(),
        state.config.min_participants,
        state.config.require_all_consent,
    );

    for (uid, name) in &members {
        session.add_participant(*uid, name.clone(), false);
    }

    // Store game info
    {
        let mut bundles = state.bundles.lock().await;
        let mut bundle = crate::storage::SessionBundle::new(
            session_id.clone(),
            &state.config.local_buffer_dir,
            guild_id.get(),
        );
        bundles.insert(guild_id.get(), bundle);
    }

    // Write Point 1: Create session and add participants in Postgres
    if let Ok(session_uuid) = uuid::Uuid::parse_str(&session_id) {
        // Read game info from the bundle we just stored
        let new_session = db::NewSession {
            id: session_uuid,
            guild_id: guild_id.get() as i64,
            started_at: chrono::Utc::now(),
            game_system: None,
            campaign_name: None,
        };

        if let Err(e) = db::create_session(&state.db, &new_session).await {
            error!("DB write failed (create_session): {e}");
        }

        // Add each participant, checking blocklist first
        for (uid, _name) in &members {
            match db::check_blocklist(&state.db, uid.get()).await {
                Ok(true) => {
                    tracing::info!(user_id = %uid, "participant_blocked — user opted out globally, skipping");
                    continue;
                }
                Err(e) => {
                    error!("DB read failed (check_blocklist): {e}");
                    // Continue — allow participant if DB is down
                }
                _ => {}
            }

            let participant = db::NewParticipant {
                session_id: session_uuid,
                discord_user_id: uid.get(),
                mid_session_join: false,
            };
            if let Err(e) = db::add_participant(&state.db, &participant).await {
                error!("DB write failed (add_participant): {e}");
            }
        }
    }

    let embed = build_consent_embed(&session);
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
            // Store the consent message ID so we can clean it up on session end
            if let Ok(msg) = command.get_response(&ctx.http).await {
                session.consent_message = Some((msg.channel_id, msg.id));
            }
            // Only store the session after Discord accepted the response
            let mut manager = state.consent.lock().await;
            manager.create_session(session);
        }
        Err(e) => {
            // Clean up the bundle since we won't have a session
            let mut bundles = state.bundles.lock().await;
            bundles.remove(&guild_id.get());
            return Err(e);
        }
    }

    Ok(())
}

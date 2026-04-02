use serenity::all::*;

use crate::consent::*;
use crate::consent::embeds::consent_buttons;
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

    // Parse optional command options
    let system = command
        .data
        .options
        .iter()
        .find(|o| o.name == "system")
        .and_then(|o| o.value.as_str())
        .map(String::from);

    let campaign = command
        .data
        .options
        .iter()
        .find(|o| o.name == "campaign")
        .and_then(|o| o.value.as_str())
        .map(String::from);

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
        bundle.game_system = system;
        bundle.campaign_name = campaign;
        bundles.insert(guild_id.get(), bundle);
    }

    let embed = build_consent_embed(&session);
    let buttons = consent_buttons();

    let mentions: String = members
        .iter()
        .map(|(uid, _)| format!("<@{}>", uid))
        .collect::<Vec<_>>()
        .join(" ");

    {
        let mut manager = state.consent.lock().await;
        manager.create_session(session);
    }

    command
        .create_response(
            &ctx.http,
            CreateInteractionResponse::Message(
                CreateInteractionResponseMessage::new()
                    .content(mentions)
                    .embed(embed)
                    .components(vec![buttons]),
            ),
        )
        .await?;

    Ok(())
}

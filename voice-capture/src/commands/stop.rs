//! `/stop` slash command handler.
//!
//! Post-refactor: the actor owns the finalization pipeline. This handler
//! sends `SessionCmd::Stop` to the per-guild actor; the actor cancels any
//! in-flight startup via its cancellation watch, runs the phase-specific
//! tear-down (finalize → meta upload, or cancel → abandon), and removes
//! itself from the DashMap. No `state.sessions.lock()` here.

use std::sync::Arc;

use serenity::all::*;
use tracing::warn;

use crate::session::actor::{request, SessionCmd, SessionError};
use crate::state::AppState;

#[tracing::instrument(skip(ctx, command, state), fields(guild_id = %command.guild_id.unwrap()))]
pub async fn handle_stop(
    ctx: &Context,
    command: &CommandInteraction,
    state: &Arc<AppState>,
) -> Result<(), serenity::Error> {
    let guild_id = command.guild_id.unwrap();

    // Defer IMMEDIATELY. Discord's 3-second ack window is unforgiving;
    // even the DashMap lookup below is cheap but deferring first pushes
    // every subsequent await onto the 15-minute followup clock.
    command
        .create_response(
            &ctx.http,
            CreateInteractionResponse::Defer(
                CreateInteractionResponseMessage::new().ephemeral(true),
            ),
        )
        .await?;

    let handle = match state.sessions.get(&guild_id.get()) {
        Some(entry) if !entry.is_shutting_down() => entry.clone(),
        _ => {
            command
                .edit_response(
                    &ctx.http,
                    EditInteractionResponse::new()
                        .content("No active recording in this server."),
                )
                .await?;
            return Ok(());
        }
    };

    let result = request(&handle, |reply| SessionCmd::Stop {
        initiator: command.user.id,
        reply,
    })
    .await;

    // Double-result: outer = send/reply succeeded, inner = actor verdict.
    let flattened = match result {
        Ok(inner) => inner,
        Err(e) => Err(e),
    };

    match flattened {
        Ok(()) => {
            command
                .edit_response(
                    &ctx.http,
                    EditInteractionResponse::new().content("Wrapping up..."),
                )
                .await?;
            command
                .channel_id
                .say(&ctx.http, "Thanks for contributing!")
                .await?;
            Ok(())
        }
        Err(SessionError::NotInitiator) => {
            command
                .edit_response(
                    &ctx.http,
                    EditInteractionResponse::new().content(
                        "Only the person who started the recording can stop it.",
                    ),
                )
                .await?;
            Ok(())
        }
        Err(SessionError::NotStoppable | SessionError::ActorGone) => {
            command
                .edit_response(
                    &ctx.http,
                    EditInteractionResponse::new()
                        .content("No active recording in this server."),
                )
                .await?;
            Ok(())
        }
        Err(e) => {
            warn!(error = %e, "stop_send_failed");
            command
                .edit_response(
                    &ctx.http,
                    EditInteractionResponse::new()
                        .content("Couldn't stop the recording — try again."),
                )
                .await?;
            Ok(())
        }
    }
}

//! `/stop` slash command.

use std::sync::Arc;

use serenity::all::*;
use tracing::warn;

use crate::commands::respond::{respond, InteractionReply};
use crate::session::actor::{request, SessionCmd, SessionError};
use crate::state::AppState;

#[tracing::instrument(skip(ctx, command, state), fields(guild_id = %command.guild_id.map(|g| g.get()).unwrap_or(0)))]
pub async fn handle_stop(
    ctx: &Context,
    command: &CommandInteraction,
    state: &Arc<AppState>,
) -> Result<(), serenity::Error> {
    respond(ctx, command, || stop_inner(ctx, command, state)).await
}

async fn stop_inner(
    _ctx: &Context,
    command: &CommandInteraction,
    state: &Arc<AppState>,
) -> InteractionReply {
    let Some(guild_id) = command.guild_id else {
        return InteractionReply::Edit(
            EditInteractionResponse::new().content("Run this in a server."),
        );
    };

    let handle = match state.sessions.get(&guild_id.get()) {
        Some(entry) if !entry.is_shutting_down() => entry.clone(),
        _ => {
            return InteractionReply::Edit(
                EditInteractionResponse::new().content("No active recording in this server."),
            );
        }
    };

    let result = request(&handle, |reply| SessionCmd::Stop {
        initiator: command.user.id,
        reply,
    })
    .await;

    let flattened = match result {
        Ok(inner) => inner,
        Err(e) => Err(e),
    };

    match flattened {
        Ok(()) => InteractionReply::Edit(
            EditInteractionResponse::new().content("Wrapping up…"),
        ),
        Err(SessionError::NotInitiator) => InteractionReply::Edit(
            EditInteractionResponse::new().content(
                "Only the person who started the recording can stop it.",
            ),
        ),
        Err(SessionError::NotStoppable | SessionError::ActorGone) => InteractionReply::Edit(
            EditInteractionResponse::new().content("No active recording in this server."),
        ),
        Err(e) => {
            warn!(error = %e, "stop_send_failed");
            InteractionReply::Edit(
                EditInteractionResponse::new().content("Couldn't stop the recording — try again."),
            )
        }
    }
}

//! Consent button handler (`/record`'s Accept / Decline flow).
//!
//! Post-refactor, this module is intentionally thin: it routes a click to
//! the per-guild actor via `SessionCmd::RecordConsent` and renders the
//! actor's response back to Discord. The heavy lifting (consent validation,
//! quorum evaluation, voice-join + DAVE-handshake pipeline) lives inside
//! the actor in `crate::session::actor`. No `state.sessions.lock()` here.

use std::sync::Arc;

use serenity::all::*;
use tracing::warn;

use crate::commands::license;
use crate::session::actor::{
    request, ConsentOutcome, SessionCmd, SessionError, SessionHandle,
    StartRecordingOutcome,
};
use crate::session::{consent_buttons, ConsentScope};
use crate::state::AppState;

/// Handle a consent Accept/Decline click, or delegate to the license-button
/// handler if the click was actually on a license preference followup.
#[tracing::instrument(
    skip_all,
    fields(
        guild_id = component.guild_id.map(|g| g.get()),
        user_id = %component.user.id,
    )
)]
pub async fn handle_consent_button(
    ctx: &Context,
    component: &ComponentInteraction,
    state: &Arc<AppState>,
) -> Result<(), serenity::Error> {
    let scope = match component.data.custom_id.as_str() {
        "consent_accept" => ConsentScope::Full,
        "consent_decline" => ConsentScope::Decline,
        "license_no_llm" | "license_no_public" => {
            return license::handle_license_button(ctx, component, state).await;
        }
        _ => return Ok(()),
    };

    let scope_label = match scope {
        ConsentScope::Full => "full",
        ConsentScope::Decline => "decline",
    };
    metrics::counter!("chronicle_consent_responses_total", "scope" => scope_label).increment(1);

    let guild_id = match component.guild_id {
        Some(g) => g.get(),
        None => return Ok(()),
    };
    let user_id = component.user.id;

    // Look up the actor handle. If it's gone or already shutting down,
    // ephemeral-reply and bail.
    let handle = match state.sessions.get(&guild_id) {
        Some(entry) if !entry.is_shutting_down() => entry.clone(),
        _ => {
            return ack_ephemeral(
                ctx,
                component,
                "There's no active recording to consent to.",
            )
            .await;
        }
    };

    let outcome = request(&handle, |reply| SessionCmd::RecordConsent {
        user: user_id,
        scope,
        reply,
    })
    .await;

    // `request` returns Result<T, SessionError> where T here is
    // Result<ConsentOutcome, SessionError>. Flatten both layers.
    let outcome = match outcome {
        Ok(Ok(o)) => o,
        Ok(Err(SessionError::NotParticipant)) => {
            return ack_ephemeral(
                ctx,
                component,
                "You're not in the voice channel for this session.",
            )
            .await;
        }
        Ok(Err(SessionError::AlreadyResponded)) => {
            return ack_ephemeral(ctx, component, "You've already responded.").await;
        }
        Ok(Err(e)) => {
            warn!(error = %e, "consent_click_rejected");
            return ack_ephemeral(ctx, component, "Can't record that click right now.").await;
        }
        Err(e) => {
            warn!(error = %e, "consent_send_failed");
            return ack_ephemeral(ctx, component, "Session is no longer active.").await;
        }
    };

    match outcome {
        ConsentOutcome::Ack { embed } => {
            update_consent_embed(ctx, component, embed).await?;
            Ok(())
        }
        ConsentOutcome::QuorumMet { embed } => {
            update_consent_embed(ctx, component, embed).await?;

            // Kick off the startup pipeline asynchronously — the user has
            // their ack, and the pipeline takes up to ~30s to settle. Any
            // failure is surfaced via a message in the text channel.
            let handle_clone = handle.clone();
            let ctx_clone = ctx.clone();
            let component_clone = component.clone();
            let scope_copy = scope;
            tokio::spawn(async move {
                let result = request(&handle_clone, |reply| {
                    SessionCmd::StartRecording { reply }
                })
                .await;
                match result {
                    Ok(StartRecordingOutcome::Recording) => {
                        let _ = component_clone
                            .channel_id
                            .say(&ctx_clone.http, "Recording. Use `/stop` when done.")
                            .await;
                        if scope_copy == ConsentScope::Full {
                            send_license_followup(&ctx_clone, &handle_clone, &component_clone)
                                .await;
                        }
                    }
                    Ok(StartRecordingOutcome::DaveFailed) => {
                        let _ = component_clone
                            .channel_id
                            .say(
                                &ctx_clone.http,
                                "Unable to receive audio. Try `/record` again.",
                            )
                            .await;
                    }
                    Ok(StartRecordingOutcome::DaveRejoinFailed) => {
                        let _ = component_clone
                            .channel_id
                            .say(
                                &ctx_clone.http,
                                "Failed to reconnect to voice. Try `/record` again.",
                            )
                            .await;
                    }
                    Ok(StartRecordingOutcome::VoiceJoinFailed(e)) => {
                        warn!(error = %e, "voice_join_failed");
                        let _ = component_clone
                            .channel_id
                            .say(&ctx_clone.http, "Failed to join voice channel.")
                            .await;
                    }
                    Ok(StartRecordingOutcome::Preempted) => {
                        // Probably /stop'd externally — nothing to say.
                    }
                    Err(e) => {
                        warn!(error = %e, "start_recording_send_failed");
                    }
                }
            });
            Ok(())
        }
        ConsentOutcome::QuorumFailed { embed } => {
            update_consent_embed(ctx, component, embed).await?;
            metrics::counter!("chronicle_sessions_total", "outcome" => "cancelled").increment(1);
            component
                .channel_id
                .say(
                    &ctx.http,
                    "Recording cancelled — consent requirements not met.",
                )
                .await?;
            Ok(())
        }
    }
}

async fn ack_ephemeral(
    ctx: &Context,
    component: &ComponentInteraction,
    msg: &str,
) -> Result<(), serenity::Error> {
    component
        .create_response(
            &ctx.http,
            CreateInteractionResponse::Message(
                CreateInteractionResponseMessage::new()
                    .content(msg)
                    .ephemeral(true),
            ),
        )
        .await
}

async fn update_consent_embed(
    ctx: &Context,
    component: &ComponentInteraction,
    embed: CreateEmbed,
) -> Result<(), serenity::Error> {
    component
        .create_response(
            &ctx.http,
            CreateInteractionResponse::UpdateMessage(
                CreateInteractionResponseMessage::new()
                    .embed(embed)
                    .components(vec![consent_buttons()]),
            ),
        )
        .await
}

/// Send the ephemeral license-preference followup and register the cleanup
/// task with the actor so /stop can abort it.
async fn send_license_followup(
    ctx: &Context,
    handle: &SessionHandle,
    component: &ComponentInteraction,
) {
    let followup = CreateInteractionResponseFollowup::new()
        .content(
            "Your audio defaults to **public dataset + LLM training**. \
             Toggle restrictions below:",
        )
        .ephemeral(true)
        .components(vec![CreateActionRow::Buttons(vec![
            CreateButton::new("license_no_llm")
                .label("No LLM Training")
                .style(ButtonStyle::Secondary),
            CreateButton::new("license_no_public")
                .label("No Public Release")
                .style(ButtonStyle::Secondary),
        ])]);

    let msg = match component.create_followup(&ctx.http, followup).await {
        Ok(m) => m,
        Err(e) => {
            warn!(error = %e, "license_followup_failed");
            return;
        }
    };

    let http = ctx.http.clone();
    let interaction_token = component.token.clone();
    let msg_id = msg.id;
    let handle_inner = tokio::spawn(async move {
        tokio::time::sleep(std::time::Duration::from_secs(14 * 60)).await;
        let edit = EditInteractionResponse::new().components(vec![]);
        let _ = http
            .edit_followup_message(&interaction_token, msg_id, &edit, vec![])
            .await;
    });
    let _ = handle
        .send(SessionCmd::AddLicenseFollowup {
            token: component.token.clone(),
            message_id: msg.id,
            cleanup_task: handle_inner,
        })
        .await;
}

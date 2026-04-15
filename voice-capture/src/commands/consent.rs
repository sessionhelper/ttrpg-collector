//! Consent button handler.
//!
//! Routes `consent_accept` / `consent_decline` clicks to the per-guild actor.
//! License buttons (F4 removal) are no longer dispatched here — the Accept
//! click writes default license flags immediately and the portal owns any
//! later mutation UI.

use std::sync::Arc;

use serenity::all::*;
use tracing::warn;

use crate::commands::respond::{respond, InteractionReply};
use crate::session::actor::{request, ConsentOutcome, SessionCmd, SessionError};
use crate::session::{consent_buttons, ConsentScope};
use crate::state::AppState;

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
    respond(ctx, component, || consent_inner(ctx, component, state)).await
}

async fn consent_inner(
    _ctx: &Context,
    component: &ComponentInteraction,
    state: &Arc<AppState>,
) -> InteractionReply {
    let scope = match component.data.custom_id.as_str() {
        "consent_accept" => ConsentScope::Full,
        "consent_decline" => ConsentScope::Decline,
        // License buttons dropped in F4. Ignore legacy IDs silently so a
        // clicker on a stale message doesn't get a scary error.
        "license_no_llm" | "license_no_public" => return InteractionReply::Silent,
        _ => return InteractionReply::Silent,
    };

    let scope_label = match scope {
        ConsentScope::Full => "full",
        ConsentScope::Decline => "decline",
    };
    metrics::counter!("chronicle_consent_responses_total", "scope" => scope_label).increment(1);

    let Some(guild_id) = component.guild_id else {
        return InteractionReply::Silent;
    };
    let user_id = component.user.id;

    let handle = match state.sessions.get(&guild_id.get()) {
        Some(entry) if !entry.is_shutting_down() => entry.clone(),
        _ => {
            return InteractionReply::Followup {
                content: "There's no active recording to consent to.".into(),
                ephemeral: true,
                components: vec![],
            };
        }
    };

    let outcome = request(&handle, |reply| SessionCmd::RecordConsent {
        user: user_id,
        scope,
        reply,
    })
    .await;

    match outcome {
        Ok(Ok(ConsentOutcome::Ack { embed })) => InteractionReply::UpdateMessage {
            embed: Some(embed),
            content: None,
            components: vec![consent_buttons()],
        },
        Ok(Ok(ConsentOutcome::QuorumFailed { embed })) => {
            metrics::counter!("chronicle_sessions_total", "outcome" => "cancelled").increment(1);
            InteractionReply::UpdateMessage {
                embed: Some(embed),
                content: None,
                components: vec![],
            }
        }
        Ok(Err(SessionError::NotParticipant)) => InteractionReply::Followup {
            content: "You're not in the voice channel for this session.".into(),
            ephemeral: true,
            components: vec![],
        },
        Ok(Err(SessionError::AlreadyResponded)) => InteractionReply::Followup {
            content: "You've already responded.".into(),
            ephemeral: true,
            components: vec![],
        },
        Ok(Err(e)) => {
            warn!(error = %e, "consent_click_rejected");
            InteractionReply::Followup {
                content: "Can't record that click right now.".into(),
                ephemeral: true,
                components: vec![],
            }
        }
        Err(e) => {
            warn!(error = %e, "consent_send_failed");
            InteractionReply::Followup {
                content: "Session is no longer active.".into(),
                ephemeral: true,
                components: vec![],
            }
        }
    }
}

//! License preference button handler (`No LLM Training` / `No Public Release`).
//!
//! These buttons appear in an ephemeral followup once recording is active.
//! Each click toggles one license flag in the Data API and re-renders the
//! button row so the current state is visible through button style.

use serenity::all::*;
use tracing::error;

use crate::state::AppState;

/// Handle a click on one of the license preference buttons.
///
/// Acknowledges the component interaction immediately (type-6 deferred
/// update message, no visible "thinking" state), then does the Data API
/// round-trips and edits the original button row via `edit_response`. If
/// we tried to `create_response(UpdateMessage)` at the end instead, two
/// sequential Data API calls could overrun Discord's 3-second interaction
/// window and fail with "Unknown interaction".
#[tracing::instrument(
    skip_all,
    fields(
        guild_id = component.guild_id.map(|g| g.get()),
        user_id = %component.user.id,
    )
)]
pub async fn handle_license_button(
    ctx: &Context,
    component: &ComponentInteraction,
    state: &AppState,
) -> Result<(), serenity::Error> {
    component
        .create_response(&ctx.http, CreateInteractionResponse::Acknowledge)
        .await?;

    let guild_id = component.guild_id.unwrap().get();
    let user_id = component.user.id;

    let field = match component.data.custom_id.as_str() {
        "license_no_llm" => "no_llm_training",
        "license_no_public" => "no_public_release",
        _ => return Ok(()),
    };

    // Snapshot session id, cached participant UUID, and current cached
    // license flags under one lock. All three come from the local Session
    // — no HTTP roundtrip for a read-back.
    let (session_id_str, cached_pid, cached_flags) = {
        let sessions = state.sessions.lock().await;
        match sessions.get(guild_id) {
            Some(s) => (
                Some(s.id.clone()),
                s.participant_uuid(user_id),
                s.license_flags(user_id),
            ),
            None => (None, None, (false, false)),
        }
    };

    // Fast path: cached UUID → single PATCH that returns the new flag
    // values in the response body. No find_participant, no read-back,
    // no extra GET. One HTTP round trip per click.
    //
    // Cold cache: fall back to the 3-hop toggle (upsert user + list
    // participants + PATCH). Same as the pre-cache behavior.
    let (no_llm, no_public) = if let Some(pid) = cached_pid {
        match state
            .api
            .toggle_license_flag_by_id(pid, field, cached_flags.0, cached_flags.1)
            .await
        {
            Ok(new_flags) => new_flags,
            Err(e) => {
                error!("API call failed (toggle_license_flag_by_id): {e}");
                cached_flags
            }
        }
    } else if let Some(sid_str) = &session_id_str {
        if let Ok(sid) = uuid::Uuid::parse_str(sid_str) {
            match state.api.toggle_license_flag(sid, user_id.get(), field).await {
                Ok(new_flags) => new_flags,
                Err(e) => {
                    error!("API call failed (toggle_license_flag): {e}");
                    (false, false)
                }
            }
        } else {
            (false, false)
        }
    } else {
        (false, false)
    };

    // Update the local cache so the next click on this button computes
    // the correct toggle without another round trip.
    {
        let mut sessions = state.sessions.lock().await;
        if let Some(s) = sessions.get_mut(guild_id) {
            s.set_license_flags(user_id, no_llm, no_public);
        }
    }

    // Re-render button row — state expressed through button style alone.
    let llm_style = if no_llm { ButtonStyle::Danger } else { ButtonStyle::Secondary };
    let public_style = if no_public { ButtonStyle::Danger } else { ButtonStyle::Secondary };
    let llm_label = if no_llm { "No LLM Training ✓" } else { "No LLM Training" };
    let public_label = if no_public { "No Public Release ✓" } else { "No Public Release" };

    component
        .edit_response(
            &ctx.http,
            EditInteractionResponse::new()
                .content("Toggle restrictions on your audio:")
                .components(vec![CreateActionRow::Buttons(vec![
                    CreateButton::new("license_no_llm")
                        .label(llm_label)
                        .style(llm_style),
                    CreateButton::new("license_no_public")
                        .label(public_label)
                        .style(public_style),
                ])]),
        )
        .await?;

    Ok(())
}
